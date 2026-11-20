//! A language server's process, and the threads that talk to it. Messages
//! are JSON, each after a `Content-Length` header, over the server's stdin
//! and stdout. One thread writes them. Another reads them, answers the
//! server's own requests, and hands the rest to the UI thread as [Event]s,
//! already parsed, so a long completion list never stalls a frame. A third
//! keeps the end of the server's stderr, to say why it stopped.

use std::{
    collections::{HashMap, VecDeque},
    io::{self, BufRead, BufReader, BufWriter, Write},
    path::Path,
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Stdio},
    sync::{
        Arc, Mutex, MutexGuard,
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use egui::{Context, ViewportId};
use serde_json::{Value, json};

use super::protocol::{self, Capabilities, CompletionItem, Location, PublishDiagnostics, TextEdit};

/// The largest message read: far above any real one, so a garbled header
/// can't allocate gigabytes.
const MAX_MESSAGE: usize = 64 << 20;
/// How long a server has to exit after being asked to, before it's ended.
const GRACE: Duration = Duration::from_secs(3);
/// How many lines of stderr are kept.
const TAIL_LINES: usize = 6;

/// What a request asked for, so the reader can parse its reply.
#[derive(Clone, Copy)]
pub(super) enum Kind {
    Initialize,
    Hover,
    Definition,
    Completion,
    Formatting,
    Shutdown,
}

/// The requests waiting for a reply, by id. Shared with the reader, which
/// drops replies whose request is no longer here (cancelled).
pub(super) type Kinds = Arc<Mutex<HashMap<i64, Kind>>>;

/// What the reader hands the UI thread.
pub(super) enum Event {
    Initialized(Capabilities),
    InitFailed(String),
    Diagnostics(PublishDiagnostics),
    Reply(i64, Reply),
    /// Work in progress, such as indexing, by its token; `None` once done.
    Progress(String, Option<String>),
    /// An error or warning the server wants shown.
    Message(String),
    /// The server's stdout closed, with the end of its stderr.
    Exited(String),
}

pub(super) enum Reply {
    Hover(Option<String>),
    Locations(Vec<Location>),
    Completion {
        items: Vec<CompletionItem>,
        incomplete: bool,
    },
    Edits(Vec<TextEdit>),
    Error(String),
}

/// What the reader answers the server's own questions with.
pub(super) struct Answers {
    pub root_uri: String,
    pub root_name: String,
    /// The project's venv interpreter, for Python servers' `pythonPath`.
    pub python: Option<String>,
}

/// A running server: where to send messages, and where its events arrive.
pub(super) struct Link {
    pub tx: Sender<Vec<u8>>,
    pub rx: Receiver<Event>,
    pub kinds: Kinds,
    /// Last, so the channels close before it's ended.
    _process: Process,
}

pub(super) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Start `program` with `args` in `root`, with its threads. Its events
/// repaint the window `viewport`.
pub(super) fn spawn(
    program: &Path,
    args: &[String],
    root: &Path,
    answers: Answers,
    ctx: &Context,
    viewport: ViewportId,
) -> io::Result<Link> {
    let mut command = shell::quiet_command(program);
    command
        .args(args)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own group, so ending it ends what it started too.
        command.process_group(0);
    }
    let mut child = command.spawn()?;
    let group = os::Group::new(&child);
    let pipes = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    let process = Process {
        child: Some(child),
        group,
    };
    let (Some(stdin), Some(stdout), Some(stderr)) = pipes else {
        return Err(io::Error::other("no pipes to the server"));
    };

    let (tx, outgoing) = mpsc::channel();
    thread::Builder::new()
        .name("lsp-write".into())
        .spawn(move || write_loop(stdin, outgoing))?;
    let tail = Arc::new(Mutex::new(VecDeque::new()));
    let tail_writer = Arc::clone(&tail);
    thread::Builder::new()
        .name("lsp-stderr".into())
        .spawn(move || stderr_loop(stderr, &tail_writer))?;
    let (events, rx) = mpsc::channel();
    let kinds = Kinds::default();
    let reader = Reader {
        out: tx.clone(),
        events,
        kinds: Arc::clone(&kinds),
        answers,
        tail,
        titles: HashMap::new(),
        ctx: ctx.clone(),
        viewport,
    };
    thread::Builder::new()
        .name("lsp-read".into())
        .spawn(move || reader.run(stdout))?;
    Ok(Link {
        tx,
        rx,
        kinds,
        _process: process,
    })
}

/// The JSON of a notification.
pub(super) fn notification(method: &str, params: Value) -> Vec<u8> {
    encode(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
}

/// The JSON of a request.
pub(super) fn request(id: i64, method: &str, params: Value) -> Vec<u8> {
    encode(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
}

fn encode(message: &Value) -> Vec<u8> {
    serde_json::to_vec(message).unwrap_or_default()
}

fn write_loop(stdin: ChildStdin, outgoing: Receiver<Vec<u8>>) {
    let mut stdin = BufWriter::new(stdin);
    for body in outgoing {
        let written = write!(stdin, "Content-Length: {}\r\n\r\n", body.len())
            .and_then(|()| stdin.write_all(&body))
            .and_then(|()| stdin.flush());
        if written.is_err() {
            break;
        }
    }
}

/// Keep the last [TAIL_LINES] of stderr. Reading it all also keeps a chatty
/// server from blocking on a full pipe.
fn stderr_loop(stderr: ChildStderr, tail: &Mutex<VecDeque<String>>) {
    let mut stderr = BufReader::new(stderr);
    let mut line = Vec::new();
    while stderr.read_until(b'\n', &mut line).is_ok_and(|n| n > 0) {
        let text = String::from_utf8_lossy(&line).trim_end().to_owned();
        line.clear();
        if text.is_empty() {
            continue;
        }
        let mut tail = lock(tail);
        if tail.len() == TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(text);
    }
}

/// One message's body, or `None` at the end of the stream. Header lines
/// other than `Content-Length` are skipped, as is anything before them.
fn read_message(input: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut length = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line)? == 0 {
            return Ok(None);
        }
        let header = String::from_utf8_lossy(&line);
        let header = header.trim();
        if header.is_empty() {
            if length.is_some() {
                break;
            }
            continue;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let length = length.unwrap_or_default();
    if length > MAX_MESSAGE {
        return Err(io::Error::other("message too large"));
    }
    let mut body = vec![0; length];
    input.read_exact(&mut body)?;
    Ok(Some(body))
}

struct Reader {
    /// To the writer, for answers.
    out: Sender<Vec<u8>>,
    events: Sender<Event>,
    kinds: Kinds,
    answers: Answers,
    tail: Arc<Mutex<VecDeque<String>>>,
    /// Each piece of work in progress's title, by token: later reports of
    /// it have only a message.
    titles: HashMap<String, String>,
    ctx: Context,
    viewport: ViewportId,
}

impl Reader {
    fn run(mut self, stdout: ChildStdout) {
        let mut input = BufReader::new(stdout);
        while let Ok(Some(body)) = read_message(&mut input) {
            if let Ok(message) = serde_json::from_slice(&body) {
                self.handle(message);
            }
        }
        // Its last words, once stderr has caught up.
        thread::sleep(Duration::from_millis(200));
        let tail = Vec::from(lock(&self.tail).clone()).join("\n");
        self.send(Event::Exited(tail));
    }

    fn send(&self, event: Event) {
        // Fails once the server's dropped: it's being shut down.
        if self.events.send(event).is_ok() {
            self.ctx.request_repaint_of(self.viewport);
        }
    }

    fn write(&self, message: &Value) {
        let _ = self.out.send(encode(message));
    }

    fn handle(&mut self, mut message: Value) {
        let method = message["method"].as_str().map(str::to_owned);
        let id = message.get("id").cloned();
        let params = message
            .get_mut("params")
            .map(Value::take)
            .unwrap_or_default();
        match (method, id) {
            (Some(method), Some(id)) => {
                let reply = match self.answer(&method, &params) {
                    Some(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    None => json!({"jsonrpc": "2.0", "id": id, "error": {
                        "code": -32601,
                        "message": format!("{method} isn't supported"),
                    }}),
                };
                self.write(&reply);
            }
            (Some(method), None) => self.notification(&method, params),
            (None, Some(id)) => self.response(&id, message),
            (None, None) => {}
        }
    }

    /// The answer to a request from the server, or `None` for one this
    /// client doesn't take. Every request must be answered: some servers
    /// wait for these before doing anything else.
    fn answer(&self, method: &str, params: &Value) -> Option<Value> {
        Some(match method {
            "workspace/configuration" => {
                let items = params["items"].as_array().map_or(&[][..], Vec::as_slice);
                let section = |item: &Value| item["section"].as_str().map(str::to_owned);
                let settings = items.iter().map(|item| {
                    match (section(item).as_deref(), &self.answers.python) {
                        (Some("python"), Some(python)) => json!({"pythonPath": python}),
                        _ => Value::Null,
                    }
                });
                Value::Array(settings.collect())
            }
            "workspace/workspaceFolders" => {
                json!([{"uri": self.answers.root_uri, "name": self.answers.root_name}])
            }
            "workspace/applyEdit" => json!({"applied": false}),
            "window/showDocument" => json!({"success": false}),
            "window/workDoneProgress/create"
            | "window/showMessageRequest"
            | "client/registerCapability"
            | "client/unregisterCapability" => Value::Null,
            // Semantic tokens, inlay hints, code lenses: none are shown.
            method if method.starts_with("workspace/") && method.ends_with("/refresh") => {
                Value::Null
            }
            _ => return None,
        })
    }

    fn notification(&mut self, method: &str, params: Value) {
        match method {
            "textDocument/publishDiagnostics" => {
                if let Ok(published) = serde_json::from_value(params) {
                    self.send(Event::Diagnostics(published));
                }
            }
            "$/progress" => self.progress(&params),
            "window/showMessage" if matches!(params["type"].as_u64(), Some(1 | 2)) => {
                if let Some(message) = params["message"].as_str() {
                    self.send(Event::Message(message.to_owned()));
                }
            }
            _ => {}
        }
    }

    fn progress(&mut self, params: &Value) {
        let token = match &params["token"] {
            Value::String(token) => token.clone(),
            token => token.to_string(),
        };
        let value = &params["value"];
        match value["kind"].as_str() {
            Some("end") => {
                self.titles.remove(&token);
                self.send(Event::Progress(token, None));
                return;
            }
            Some("begin") => {
                let title = value["title"].as_str().unwrap_or_default().to_owned();
                self.titles.insert(token.clone(), title);
            }
            Some("report") => {}
            // Partial results, not work done.
            _ => return,
        }
        let mut text = self.titles.get(&token).cloned().unwrap_or_default();
        if let Some(message) = value["message"].as_str() {
            if !text.is_empty() {
                text.push_str(": ");
            }
            text.push_str(message);
        }
        if let Some(percentage) = value["percentage"].as_u64() {
            text.push_str(&format!(" ({percentage}%)"));
        }
        self.send(Event::Progress(token, Some(text)));
    }

    fn response(&mut self, id: &Value, mut message: Value) {
        let Some(id) = id.as_i64() else {
            return;
        };
        // Not there if it was cancelled: nobody's waiting for it.
        let Some(kind) = lock(&self.kinds).remove(&id) else {
            return;
        };
        if let Some(error) = message.get("error") {
            let text = error["message"]
                .as_str()
                .unwrap_or("unknown error")
                .to_owned();
            match kind {
                Kind::Initialize => self.send(Event::InitFailed(text)),
                Kind::Shutdown => self.write(&json!({"jsonrpc": "2.0", "method": "exit"})),
                _ => self.send(Event::Reply(id, Reply::Error(text))),
            }
            return;
        }
        let result = message
            .get_mut("result")
            .map(Value::take)
            .unwrap_or_default();
        let reply = match kind {
            Kind::Initialize => {
                // First, before anything the UI thread queued meanwhile.
                self.write(&json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}));
                self.send(Event::Initialized(Capabilities::parse(&result)));
                return;
            }
            Kind::Shutdown => {
                self.write(&json!({"jsonrpc": "2.0", "method": "exit"}));
                return;
            }
            Kind::Hover => Reply::Hover(protocol::hover_markdown(&result)),
            Kind::Definition => Reply::Locations(protocol::locations(result)),
            Kind::Completion => {
                let (items, incomplete) = protocol::completions(result);
                Reply::Completion { items, incomplete }
            }
            Kind::Formatting => Reply::Edits(protocol::edits(result)),
        };
        self.send(Event::Reply(id, reply));
    }
}

/// The server's process. Dropping it gives the server [GRACE] to exit, on
/// a thread of its own, then ends it and everything it started (such as
/// `cargo check`).
struct Process {
    child: Option<Child>,
    group: os::Group,
}

impl Drop for Process {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let group = std::mem::take(&mut self.group);
        let _ = thread::Builder::new()
            .name("lsp-exit".into())
            .spawn(move || {
                let deadline = Instant::now() + GRACE;
                while Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) {
                    thread::sleep(Duration::from_millis(50));
                }
                group.end(&mut child);
                let _ = child.wait();
            });
    }
}

#[cfg(windows)]
mod os {
    use std::{
        mem::{size_of, zeroed},
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::Child,
        ptr::null,
    };

    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };

    /// A job object holding the server and whatever it starts. It ends
    /// them all when closed, so they don't outlive Peregrine even if it
    /// crashes.
    #[derive(Default)]
    pub(super) struct Group(Option<OwnedHandle>);

    impl Group {
        pub(super) fn new(child: &Child) -> Self {
            // SAFETY: plain calls on handles we own, with a zeroed struct
            // of the size given.
            unsafe {
                let job = CreateJobObjectW(null(), null());
                if job.is_null() {
                    return Self(None);
                }
                let job = OwnedHandle::from_raw_handle(job);
                let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    job.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    (&raw const limits).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                let assigned =
                    AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) != 0;
                Self(assigned.then_some(job))
            }
        }

        pub(super) fn end(self, child: &mut Child) {
            match self.0 {
                // SAFETY: a job handle we own.
                Some(job) => unsafe {
                    TerminateJobObject(job.as_raw_handle(), 1);
                },
                None => {
                    let _ = child.kill();
                }
            }
        }
    }
}

#[cfg(unix)]
mod os {
    use std::process::Child;

    /// The server's process group: its id is the server's.
    #[derive(Default)]
    pub(super) struct Group(Option<i32>);

    impl Group {
        pub(super) fn new(child: &Child) -> Self {
            Self(i32::try_from(child.id()).ok())
        }

        pub(super) fn end(self, child: &mut Child) {
            if let Some(group) = self.0 {
                // SAFETY: a plain call; the group is the server's own.
                unsafe { libc::kill(-group, libc::SIGKILL) };
            }
            let _ = child.kill();
        }
    }
}

#[cfg(not(any(windows, unix)))]
mod os {
    use std::process::Child;

    #[derive(Default)]
    pub(super) struct Group;

    impl Group {
        pub(super) fn new(_: &Child) -> Self {
            Self
        }

        pub(super) fn end(self, child: &mut Child) {
            let _ = child.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_framed() {
        let stream = b"Content-Length: 2\r\n\r\n{}\
            junk before\r\ncontent-type: x\r\nCONTENT-LENGTH: 7\r\n\r\n{\"a\":1}";
        let mut input = &stream[..];
        assert_eq!(read_message(&mut input).unwrap().unwrap(), b"{}");
        assert_eq!(read_message(&mut input).unwrap().unwrap(), b"{\"a\":1}");
        assert!(read_message(&mut input).unwrap().is_none());
    }
}
