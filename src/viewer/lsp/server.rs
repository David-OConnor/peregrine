//! One language server, as the UI thread sees it: its state, what it can
//! do, the problems it found in each file, and the replies waiting to be
//! taken. Shared by every tab editing a file of its project.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use egui::{Context, ViewportId};
use serde_json::{Value, json};

use super::{
    Launch,
    process::{self, Answers, Event, Kind, Link, Reply, lock},
    protocol::{self, Capabilities, Diagnostic},
};

enum State {
    /// Waiting for its reply to `initialize`.
    Starting,
    Ready,
    /// It couldn't be started, or refused to.
    Failed(String),
    Exited(String),
}

/// The problems a server last published for a file.
pub(super) struct Published {
    /// Unique to this publication, to notice a new one.
    pub generation: u64,
    /// The document version they're for, if the server said.
    pub version: Option<i32>,
    pub items: Arc<[Diagnostic]>,
}

/// What the toolbar says about a server.
pub(super) enum Health<'a> {
    Starting,
    Ready,
    /// Working on something, such as indexing, described.
    Busy(&'a str),
    /// Why it isn't running.
    Stopped(&'a str),
}

pub(super) struct Server {
    /// The [super::Config] it serves.
    pub config: &'static str,
    /// Its program's name, such as `rust-analyzer`.
    pub name: String,
    pub root: PathBuf,
    state: State,
    caps: Capabilities,
    link: Option<Link>,
    next_id: i64,
    /// Messages sent before it was ready, sent once it is.
    queue: Vec<Vec<u8>>,
    replies: HashMap<i64, Reply>,
    /// By [protocol::path_key].
    diagnostics: HashMap<String, Published>,
    generation: u64,
    /// Work in progress, oldest first, by token.
    progress: Vec<(String, String)>,
    /// The last error or warning it asked to show.
    message: Option<String>,
    /// Its command changed in Settings, or the user restarted it: no new
    /// tabs use it, and those using it start another.
    pub retired: bool,
    /// When the last tab using it stopped.
    pub idle_since: Option<Instant>,
}

impl Server {
    /// Start the server `launch` says, or one that only says why it
    /// couldn't. `python` is the project's venv interpreter, if any.
    pub fn start(
        config: &'static str,
        launch: Result<Launch, String>,
        root: PathBuf,
        python: Option<PathBuf>,
        ctx: &Context,
        viewport: ViewportId,
    ) -> Self {
        let mut server = Self {
            config,
            name: config.to_owned(),
            root,
            state: State::Starting,
            caps: Capabilities::default(),
            link: None,
            next_id: 1,
            queue: Vec::new(),
            replies: HashMap::new(),
            diagnostics: HashMap::new(),
            generation: 0,
            progress: Vec::new(),
            message: None,
            retired: false,
            idle_since: None,
        };
        let launch = match launch {
            Ok(launch) => launch,
            Err(e) => {
                server.state = State::Failed(e);
                return server;
            }
        };
        server.name.clone_from(&launch.name);
        let root_uri = protocol::path_to_uri(&server.root);
        let root_name = server.root.file_name().map_or_else(
            || server.root.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let answers = Answers {
            root_uri: root_uri.clone(),
            root_name: root_name.clone(),
            python: python.map(|p| p.to_string_lossy().into_owned()),
        };
        match process::spawn(
            &launch.program,
            &launch.args,
            &server.root,
            answers,
            ctx,
            viewport,
        ) {
            Ok(link) => {
                lock(&link.kinds).insert(0, Kind::Initialize);
                let params = initialize_params(&server.root, &root_uri, &root_name);
                let _ = link.tx.send(process::request(0, "initialize", params));
                server.link = Some(link);
            }
            Err(e) => {
                server.state =
                    State::Failed(format!("Couldn't start {}: {e}", launch.program.display()));
            }
        }
        server
    }

    /// Starting or running.
    pub fn alive(&self) -> bool {
        matches!(self.state, State::Starting | State::Ready)
    }

    pub fn ready(&self) -> bool {
        matches!(self.state, State::Ready)
    }

    pub fn caps(&self) -> &Capabilities {
        &self.caps
    }

    pub fn health(&self) -> Health<'_> {
        match &self.state {
            State::Starting => Health::Starting,
            State::Ready => match self.progress.last() {
                Some((_, work)) => Health::Busy(work),
                None => Health::Ready,
            },
            State::Failed(why) | State::Exited(why) => Health::Stopped(why),
        }
    }

    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    pub fn diagnostics(&self, key: &str) -> Option<&Published> {
        self.diagnostics.get(key)
    }

    /// Send a notification; queued while it starts.
    pub fn notify(&mut self, method: &str, params: Value) {
        let body = process::notification(method, params);
        match (&self.state, &self.link) {
            (State::Ready, Some(link)) => {
                let _ = link.tx.send(body);
            }
            (State::Starting, Some(_)) => self.queue.push(body),
            _ => {}
        }
    }

    /// Send a request, if the server's ready, returning its id: its reply
    /// comes to [Self::take_reply].
    pub fn request(&mut self, method: &str, params: Value, kind: Kind) -> Option<i64> {
        let link = self.link.as_ref().filter(|_| self.ready())?;
        let id = self.next_id;
        self.next_id += 1;
        lock(&link.kinds).insert(id, kind);
        let _ = link.tx.send(process::request(id, method, params));
        Some(id)
    }

    /// Forget request `id`: its reply, or the request itself, which the
    /// server is told it may drop.
    pub fn cancel(&mut self, id: i64) {
        self.replies.remove(&id);
        let waiting = self
            .link
            .as_ref()
            .is_some_and(|link| lock(&link.kinds).remove(&id).is_some());
        if waiting {
            self.notify("$/cancelRequest", json!({"id": id}));
        }
    }

    pub fn take_reply(&mut self, id: i64) -> Option<Reply> {
        self.replies.remove(&id)
    }

    /// Take in what the server sent since last time.
    pub fn poll(&mut self) {
        let Some(link) = &self.link else {
            return;
        };
        let events: Vec<Event> = link.rx.try_iter().collect();
        for event in events {
            match event {
                Event::Initialized(caps) => {
                    self.caps = caps;
                    self.state = State::Ready;
                    if let Some(link) = &self.link {
                        for body in self.queue.drain(..) {
                            let _ = link.tx.send(body);
                        }
                    }
                }
                Event::InitFailed(why) => {
                    self.state = State::Failed(format!("{} refused to start: {why}", self.name));
                    self.queue.clear();
                }
                Event::Diagnostics(published) => {
                    if let Some(path) = protocol::uri_to_path(&published.uri) {
                        self.generation += 1;
                        let published = Published {
                            generation: self.generation,
                            version: published.version,
                            items: published.diagnostics.into(),
                        };
                        self.diagnostics
                            .insert(protocol::path_key(&path), published);
                    }
                }
                Event::Reply(id, reply) => {
                    self.replies.insert(id, reply);
                }
                Event::Progress(token, work) => {
                    self.progress.retain(|(t, _)| *t != token);
                    if let Some(work) = work {
                        self.progress.push((token, work));
                    }
                }
                Event::Message(message) => self.message = Some(message),
                Event::Exited(tail) => {
                    self.progress.clear();
                    self.replies.clear();
                    if !matches!(self.state, State::Failed(_)) {
                        let why = match tail.lines().last() {
                            Some(line) => format!("{} stopped: {line}", self.name),
                            None => format!("{} stopped", self.name),
                        };
                        self.state = State::Exited(why);
                    }
                    // Its stderr, in full, for the toolbar's tooltip.
                    if !tail.is_empty() {
                        self.message = Some(tail);
                    }
                }
            }
        }
    }
}

impl Drop for Server {
    /// Ask the server to shut down, and to exit once it has. The process
    /// is ended if it hasn't after a while (see [process]).
    fn drop(&mut self) {
        let Some(link) = self.link.take() else {
            return;
        };
        match self.state {
            State::Ready => {
                lock(&link.kinds).insert(self.next_id, Kind::Shutdown);
                let _ = link
                    .tx
                    .send(process::request(self.next_id, "shutdown", Value::Null));
            }
            State::Starting => {
                let _ = link.tx.send(process::notification("exit", Value::Null));
            }
            State::Failed(_) | State::Exited(_) => {}
        }
    }
}

/// What the client can do, told to the server at start-up.
fn initialize_params(root: &Path, root_uri: &str, root_name: &str) -> Value {
    json!({
        "processId": std::process::id(),
        "clientInfo": {"name": "Peregrine", "version": env!("CARGO_PKG_VERSION")},
        "rootUri": root_uri,
        "rootPath": root.to_string_lossy(),
        "workspaceFolders": [{"uri": root_uri, "name": root_name}],
        "capabilities": {
            // UTF-32 counts chars, as egui does.
            "general": {"positionEncodings": ["utf-32", "utf-8", "utf-16"]},
            "workspace": {"configuration": true, "workspaceFolders": true},
            "window": {"workDoneProgress": true},
            "textDocument": {
                "synchronization": {"didSave": true},
                "publishDiagnostics": {"versionSupport": true},
                "hover": {"contentFormat": ["markdown", "plaintext"]},
                "definition": {"linkSupport": true},
                "completion": {
                    "contextSupport": true,
                    "completionItem": {
                        // As plain text, the first placeholder selected.
                        "snippetSupport": true,
                        "labelDetailsSupport": true,
                        "documentationFormat": ["markdown", "plaintext"],
                    },
                },
                "formatting": {},
            },
        },
    })
}
