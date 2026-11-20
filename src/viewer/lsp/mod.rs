//! Language servers for the editor: problems underlined as you type, docs
//! on hover, completion, go to definition and formatting, from whichever
//! server for the file's language is installed (on PATH). None is bundled;
//! Settings lists the commands tried, and can change them.
//!
//! One server runs per language and project (the folder holding the
//! nearest `Cargo.toml`, `pyproject.toml`, `go.mod`...), shared by every
//! tab editing a file of it, and is stopped a while after the last such
//! tab leaves edit mode, or at once when the viewer window closes. Each
//! edit sends the whole text, after a pause in typing: the editor only
//! takes files up to 1 MB.

use std::{
    collections::{BTreeMap, VecDeque},
    env,
    ops::Range,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use egui::Context;
use serde::{Deserialize, Serialize};
use serde_json::json;

use self::{
    process::{Kind, Reply, lock},
    protocol::{Encoding, Lines, Position},
    server::Server,
};

mod process;
mod protocol;
mod server;
mod view;

pub(super) use view::severity_color;

/// A change the language server made to the editor's text before the box
/// is drawn (a completion taken, the file formatted), or a place it sent
/// the caret to.
pub(super) struct Edit {
    /// Where the caret goes, in chars.
    pub caret: usize,
    /// How many chars after it to select, such as a snippet's placeholder.
    pub select: usize,
    /// Scroll to it: it may be off screen.
    pub reveal: bool,
    /// Back to typing: it came from a click elsewhere.
    pub focus: bool,
}

/// How long after the last keystroke the text is sent to the server.
const DEBOUNCE: Duration = Duration::from_millis(250);
/// How long a server no tab uses is kept, in case one opens again.
const IDLE: Duration = Duration::from_secs(180);
/// How many sent versions of a text are kept, to place replies about an
/// older one.
const KEPT_VERSIONS: usize = 16;

/// A language's servers, and where its projects start.
struct Config {
    /// As Settings shows it, and its key there.
    name: &'static str,
    /// The `syntax::language`s of its files.
    languages: &'static [&'static str],
    /// The command lines tried, in order: the first whose program is on
    /// PATH runs.
    commands: &'static [&'static str],
    /// Files or folders whose folder is a project's root, nearest first.
    markers: &'static [&'static str],
    /// How to install one, for the toolbar when none is found.
    install: &'static str,
}

const CONFIGS: &[Config] = &[
    Config {
        name: "Rust",
        languages: &["rs"],
        commands: &["rust-analyzer"],
        markers: &["Cargo.toml"],
        install: "rustup component add rust-analyzer",
    },
    Config {
        name: "Python",
        languages: &["py"],
        commands: &[
            "basedpyright-langserver --stdio",
            "pyright-langserver --stdio",
            "pylsp",
            "jedi-language-server",
        ],
        markers: &[
            "pyproject.toml",
            "setup.py",
            "setup.cfg",
            "requirements.txt",
            ".git",
        ],
        install: "npm install -g pyright, or pip install python-lsp-server",
    },
    Config {
        name: "C/C++",
        languages: &["c", "cpp"],
        commands: &["clangd"],
        markers: &[
            "compile_commands.json",
            "compile_flags.txt",
            ".clangd",
            "CMakeLists.txt",
            ".git",
        ],
        install: "install clangd, which comes with LLVM",
    },
    Config {
        name: "Go",
        languages: &["go"],
        commands: &["gopls"],
        markers: &["go.work", "go.mod", ".git"],
        install: "go install golang.org/x/tools/gopls@latest",
    },
    Config {
        name: "JavaScript",
        languages: &["js"],
        commands: &["typescript-language-server --stdio"],
        markers: &["package.json", "jsconfig.json", "tsconfig.json", ".git"],
        install: "npm install -g typescript-language-server typescript",
    },
    Config {
        name: "Shell",
        languages: &["sh"],
        commands: &["bash-language-server start"],
        markers: &[".git"],
        install: "npm install -g bash-language-server",
    },
    Config {
        name: "TOML",
        languages: &["toml"],
        commands: &["taplo lsp stdio"],
        markers: &[".git"],
        install: "cargo install taplo-cli --features lsp",
    },
    Config {
        name: "JSON",
        languages: &["json"],
        commands: &["vscode-json-language-server --stdio"],
        markers: &[".git"],
        install: "npm install -g vscode-langservers-extracted",
    },
    Config {
        name: "HTML",
        languages: &["html"],
        commands: &["vscode-html-language-server --stdio"],
        markers: &[".git"],
        install: "npm install -g vscode-langservers-extracted",
    },
];

/// LSP's name for the language of files `syntax::language` calls `language`.
fn language_id(language: &str) -> &'static str {
    match language {
        "rs" => "rust",
        "py" => "python",
        "c" => "c",
        "cpp" => "cpp",
        "go" => "go",
        "js" => "javascript",
        "sh" => "shellscript",
        "toml" => "toml",
        "json" => "json",
        "html" => "html",
        _ => "plaintext",
    }
}

/// A language the Settings window lists, with the commands tried for it.
pub struct LanguageServer {
    pub name: &'static str,
    pub commands: &'static [&'static str],
    pub install: &'static str,
}

/// The languages servers can be set for, in Settings' order.
pub fn language_servers() -> impl Iterator<Item = LanguageServer> {
    CONFIGS.iter().map(|c| LanguageServer {
        name: c.name,
        commands: c.commands,
        install: c.install,
    })
}

/// The language server settings, saved with the GUI's.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LspSettings {
    /// Whether edit mode starts servers at all.
    pub enabled: bool,
    /// Command lines run in place of the ones tried, by language name.
    pub commands: BTreeMap<String, String>,
}

impl Default for LspSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            commands: BTreeMap::new(),
        }
    }
}

impl LspSettings {
    fn command(&self, config: &str) -> Option<&str> {
        self.commands
            .get(config)
            .map(|c| c.trim())
            .filter(|c| !c.is_empty())
    }
}

/// What starting a server runs.
pub(super) struct Launch {
    /// The program's name, such as `rust-analyzer`.
    name: String,
    program: PathBuf,
    args: Vec<String>,
}

/// The servers running, for every tab of the viewer window.
#[derive(Default)]
pub(super) struct Servers {
    list: Vec<Arc<Mutex<Server>>>,
    settings: LspSettings,
    /// Bumped when servers are turned off or on: tabs drop theirs.
    generation: u64,
}

impl Servers {
    pub(super) fn settings(&self) -> &LspSettings {
        &self.settings
    }

    /// Apply `settings`. Servers whose command changed are replaced, as
    /// tabs using them notice.
    pub(super) fn set_settings(&mut self, settings: LspSettings) {
        if settings == self.settings {
            return;
        }
        if settings.enabled != self.settings.enabled {
            self.generation += 1;
            self.list.clear();
        } else {
            for server in &self.list {
                let mut server = lock(server);
                if settings.command(server.config) != self.settings.command(server.config) {
                    server.retired = true;
                }
            }
        }
        self.settings = settings;
    }

    /// Open the file at `path`, of `language`, with `text`, in its project's
    /// server, starting one if need be. `None` if servers are off, or none
    /// serves the language. Call in the viewer window's pass: the server's
    /// replies repaint it.
    pub(super) fn attach(
        &mut self,
        ctx: &Context,
        path: &Path,
        language: &str,
        text: &Arc<str>,
    ) -> Option<DocLsp> {
        if !self.settings.enabled {
            return None;
        }
        let config = CONFIGS.iter().find(|c| c.languages.contains(&language))?;
        let root = project_root(path, config);
        let running = self.list.iter().find(|server| {
            let server = lock(server);
            server.config == config.name && server.root == root && !server.retired
        });
        let server = match running {
            Some(server) => Arc::clone(server),
            None => {
                let launch = launch(config, self.settings.command(config.name));
                let python = (config.name == "Python")
                    .then(|| shell::venv_python(&root))
                    .flatten();
                let server =
                    Server::start(config.name, launch, root, python, ctx, ctx.viewport_id());
                let server = Arc::new(Mutex::new(server));
                self.list.push(Arc::clone(&server));
                server
            }
        };
        Some(DocLsp::open(
            server,
            self.generation,
            path,
            language_id(language),
            text,
        ))
    }

    /// Take in what the servers sent, and stop the ones no tab has used for
    /// a while, or that stopped by themselves.
    pub(super) fn poll(&mut self) {
        let now = Instant::now();
        self.list.retain(|server| {
            let used = Arc::strong_count(server) > 1;
            let mut server = lock(server);
            server.poll();
            if server.retired {
                return false;
            }
            if used {
                server.idle_since = None;
                return true;
            }
            server.alive() && now.duration_since(*server.idle_since.get_or_insert(now)) < IDLE
        });
    }

    pub(super) fn stop_all(&mut self) {
        self.list.clear();
    }
}

/// Which server for `config`, from `custom` (Settings' command line) or
/// else its first command whose program is on PATH.
fn launch(config: &Config, custom: Option<&str>) -> Result<Launch, String> {
    let lines = match custom {
        Some(line) => vec![line],
        None => config.commands.to_vec(),
    };
    for line in lines {
        let words = split_command(line);
        let Some((program, args)) = words.split_first() else {
            continue;
        };
        if let Some(found) = find_program(program) {
            let name = Path::new(program)
                .file_stem()
                .map_or_else(|| program.clone(), |s| s.to_string_lossy().into_owned());
            return Ok(Launch {
                name,
                program: found,
                args: args.to_vec(),
            });
        }
    }
    Err(match custom {
        Some(line) => format!("`{line}` (set in Settings) isn't on PATH"),
        None => format!(
            "No {} language server on PATH. To install one: {}",
            config.name, config.install
        ),
    })
}

/// `line` split into words at spaces, keeping "quoted words" whole.
fn split_command(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let (mut quoted, mut started) = (false, false);
    for c in line.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    words.push(std::mem::take(&mut word));
                }
                started = false;
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(word);
    }
    words
}

/// Where `program` is: itself, if it's a path, else the first match on
/// PATH. On Windows, with each PATHEXT extension in turn, since npm
/// installs servers as `.cmd` files.
fn find_program(program: &str) -> Option<PathBuf> {
    let candidates = |base: PathBuf| -> Vec<PathBuf> {
        if cfg!(windows) {
            let exts = env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
            let mut found: Vec<PathBuf> = exts
                .split(';')
                .filter(|e| !e.is_empty())
                .map(|e| {
                    let mut name = base.clone().into_os_string();
                    name.push(e);
                    PathBuf::from(name)
                })
                .collect();
            // Without an extension, only as given: npm puts a shell
            // script there for Git Bash, which Windows can't run.
            if base.extension().is_some() {
                found.insert(0, base);
            }
            found
        } else {
            vec![base]
        }
    };
    let path = Path::new(program);
    if path.components().count() > 1 {
        return candidates(path.to_path_buf())
            .into_iter()
            .find(|p| p.is_file());
    }
    env::split_paths(&env::var_os("PATH")?)
        .flat_map(|dir| candidates(dir.join(program)))
        .find(|p| p.is_file())
}

/// The root of the project `path` is in, for `config`'s servers: the
/// nearest folder up from it holding one of its markers, else its own.
/// For Rust, the Cargo workspace's, if a manifest above names one, so one
/// rust-analyzer serves all its crates.
fn project_root(path: &Path, config: &Config) -> PathBuf {
    let dir = path.parent().unwrap_or(path);
    if config.name == "Rust" {
        let manifests: Vec<&Path> = dir
            .ancestors()
            .filter(|d| d.join("Cargo.toml").is_file())
            .collect();
        let workspace = manifests.iter().find(|d| {
            std::fs::read_to_string(d.join("Cargo.toml")).is_ok_and(|t| t.contains("[workspace]"))
        });
        return workspace
            .or(manifests.first())
            .unwrap_or(&dir)
            .to_path_buf();
    }
    dir.ancestors()
        .find(|d| config.markers.iter().any(|m| d.join(m).exists()))
        .unwrap_or(dir)
        .to_path_buf()
}

/// A problem in the text, placed.
pub(super) struct Diag {
    /// Chars of the text.
    pub range: Range<usize>,
    /// 1 error, 2 warning, 3 information, 4 hint.
    pub severity: u8,
    pub message: String,
}

/// Where go to definition went, in another file.
pub(super) struct Jump {
    pub path: PathBuf,
    at: Position,
    encoding: Encoding,
    /// In the project: open it to edit, rather than view (e.g. a library's
    /// source).
    pub edit: bool,
}

impl Jump {
    /// Where it is in `text`, the file's.
    pub(super) fn char_index(&self, text: &str) -> usize {
        Lines::new(text).char_index(text, self.at, self.encoding)
    }
}

/// The requests waiting for a reply.
#[derive(Default)]
struct Requests {
    definition: Option<i64>,
    /// With the version formatted: edits to any other are dropped.
    format: Option<(i64, i32)>,
}

/// One tab's file, open in a server. Dropping it closes it there.
pub(super) struct DocLsp {
    server: Arc<Mutex<Server>>,
    generation: u64,
    key: String,
    uri: String,
    version: i32,
    /// The texts sent lately, newest last, by version.
    sent: VecDeque<(i32, Arc<str>)>,
    /// The editor's text, as of its last change: what [Self::diagnostics]
    /// are placed in.
    text: Arc<str>,
    lines: Option<Lines>,
    /// When `text` changed without being sent yet.
    changed_at: Option<Instant>,
    /// The publication `diagnostics` are from.
    published: u64,
    diagnostics: Vec<Diag>,
    /// Each line's worst problem's severity, or 0; built when drawn.
    marks: Option<Vec<u8>>,
    requests: Requests,
    hover: view::Hover,
    completion: Option<view::Completion>,
    /// F8 or a click on the problem count: go to the next problem, or
    /// with `false`, the one before.
    step: Option<bool>,
    jump: Option<Jump>,
    /// Shown in the toolbar, e.g. that there's no definition to go to.
    notice: Option<String>,
}

impl DocLsp {
    fn open(
        server: Arc<Mutex<Server>>,
        generation: u64,
        path: &Path,
        language_id: &str,
        text: &Arc<str>,
    ) -> Self {
        let uri = protocol::path_to_uri(path);
        lock(&server).notify(
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": uri,
                "languageId": language_id,
                "version": 1,
                "text": &**text,
            }}),
        );
        Self {
            server,
            generation,
            key: protocol::path_key(path),
            uri,
            version: 1,
            sent: VecDeque::from([(1, Arc::clone(text))]),
            text: Arc::clone(text),
            lines: None,
            changed_at: None,
            published: 0,
            diagnostics: Vec::new(),
            marks: None,
            requests: Requests::default(),
            hover: view::Hover::default(),
            completion: None,
            step: None,
            jump: None,
            notice: None,
        }
    }

    /// Whether this should be dropped for a new one: servers were turned
    /// off or on, or its server's command changed, or it was restarted.
    pub(super) fn stale(&self, servers: &Servers) -> bool {
        self.generation != servers.generation || lock(&self.server).retired
    }

    /// Replace the server, which the toolbar's Restart does.
    pub(super) fn restart(&self) {
        lock(&self.server).retired = true;
    }

    /// The file was saved: servers like rust-analyzer check it then.
    pub(super) fn saved(&mut self) {
        self.flush();
        lock(&self.server).notify(
            "textDocument/didSave",
            json!({"textDocument": {"uri": self.uri}}),
        );
    }

    /// Where go to definition went, in another file, once.
    pub(super) fn take_jump(&mut self) -> Option<Jump> {
        self.jump.take()
    }

    fn encoding(&self) -> Encoding {
        lock(&self.server).caps().encoding
    }

    fn lines(&mut self) -> &Lines {
        let text = &self.text;
        self.lines.get_or_insert_with(|| Lines::new(text))
    }

    /// The position of char `at` of the text.
    fn position(&mut self, at: usize) -> Position {
        let encoding = self.encoding();
        let text = Arc::clone(&self.text);
        self.lines().position(&text, at, encoding)
    }

    /// Follow the editor's text becoming `text`: problems after the change
    /// move with the text, and the change is sent after a pause.
    fn track(&mut self, text: &Arc<str>) {
        if Arc::ptr_eq(&self.text, text) {
            return;
        }
        if !self.diagnostics.is_empty() {
            shift(&mut self.diagnostics, &self.text, text);
        }
        self.text = Arc::clone(text);
        self.lines = None;
        self.marks = None;
        self.changed_at = Some(Instant::now());
        self.notice = None;
    }

    /// Send the text if it changed since last sent.
    fn flush(&mut self) {
        if self.changed_at.take().is_none() {
            return;
        }
        self.version += 1;
        lock(&self.server).notify(
            "textDocument/didChange",
            json!({
                "textDocument": {"uri": self.uri, "version": self.version},
                "contentChanges": [{"text": &*self.text}],
            }),
        );
        self.sent.push_back((self.version, Arc::clone(&self.text)));
        if self.sent.len() > KEPT_VERSIONS {
            self.sent.pop_front();
        }
    }

    /// Send the text once typing pauses.
    fn tick(&mut self, ctx: &Context) {
        if let Some(at) = self.changed_at {
            let wait = DEBOUNCE.saturating_sub(at.elapsed());
            if wait.is_zero() {
                self.flush();
            } else {
                ctx.request_repaint_after(wait);
            }
        }
    }

    /// Take the server's latest problems for this file, placed in the text.
    fn refresh_diagnostics(&mut self) {
        let server = Arc::clone(&self.server);
        let server = lock(&server);
        let Some(published) = server.diagnostics(&self.key) else {
            return;
        };
        if published.generation == self.published {
            return;
        }
        self.published = published.generation;
        let encoding = server.caps().encoding;
        // The text they were found in: the version they name, else the
        // one last sent.
        let base = published
            .version
            .and_then(|v| self.sent.iter().find(|(sent, _)| *sent == v))
            .or(self.sent.back())
            .map_or_else(|| Arc::clone(&self.text), |(_, text)| Arc::clone(text));
        let lines = Lines::new(&base);
        let mut diagnostics: Vec<Diag> = published
            .items
            .iter()
            .map(|d| {
                let start = lines.char_index(&base, d.range.start, encoding);
                let end = lines.char_index(&base, d.range.end, encoding).max(start);
                Diag {
                    range: start..end,
                    severity: d.severity.unwrap_or(1).clamp(1, 4),
                    message: d.text(),
                }
            })
            .collect();
        drop(server);
        if !Arc::ptr_eq(&base, &self.text) {
            shift(&mut diagnostics, &base, &self.text);
        }
        diagnostics.sort_by_key(|d| (d.range.start, d.severity));
        self.diagnostics = diagnostics;
        self.marks = None;
    }

    /// Each line's worst problem's severity, or 0.
    fn marks(&mut self) -> &[u8] {
        if self.marks.is_none() {
            let mut marks = vec![0; self.lines().count()];
            let lines = self.lines.as_ref().expect("built above");
            for d in &self.diagnostics {
                let mark = &mut marks[lines.line_of(d.range.start)];
                if *mark == 0 || d.severity < *mark {
                    *mark = d.severity;
                }
            }
            self.marks = Some(marks);
        }
        self.marks.as_deref().unwrap_or_default()
    }

    /// The text up to date in the server, then `request` with the text
    /// document and `at`'s position. `None` if it isn't ready.
    fn request_at(&mut self, method: &str, at: usize, kind: Kind) -> Option<i64> {
        self.flush();
        let position = self.position(at);
        lock(&self.server).request(
            method,
            json!({"textDocument": {"uri": self.uri}, "position": position}),
            kind,
        )
    }

    fn request_definition(&mut self, at: usize) {
        if !lock(&self.server).caps().definition {
            return;
        }
        if let Some(old) = self.requests.definition.take() {
            lock(&self.server).cancel(old);
        }
        self.requests.definition = self.request_at("textDocument/definition", at, Kind::Definition);
    }

    /// Format the file with indents of `tab` (tabs if `None`).
    fn request_format(&mut self, spaces: Option<usize>) {
        self.flush();
        let mut server = lock(&self.server);
        if !server.caps().formatting {
            drop(server);
            self.notice = Some("The language server doesn't format files".into());
            return;
        }
        let options = json!({
            "tabSize": spaces.unwrap_or(4),
            "insertSpaces": spaces.is_some(),
            "trimTrailingWhitespace": true,
        });
        let params = json!({"textDocument": {"uri": self.uri}, "options": options});
        let id = server.request("textDocument/formatting", params, Kind::Formatting);
        drop(server);
        self.requests.format = id.map(|id| (id, self.version));
    }

    /// Take the replies to go to definition and format, if they're in.
    /// Returns an edit made to `text`, the editor's, with its caret at
    /// `caret`.
    fn take_replies(&mut self, text: &mut String, caret: usize) -> Option<Edit> {
        let mut edit = None;
        // Each reply taken in a statement of its own: the lock mustn't be
        // held while handling it, which locks again.
        let reply = self
            .requests
            .definition
            .and_then(|id| lock(&self.server).take_reply(id));
        if let Some(reply) = reply {
            self.requests.definition = None;
            match reply {
                Reply::Locations(locations) => match locations.into_iter().next() {
                    Some(location) if protocol::path_key(&location.path) == self.key => {
                        let encoding = self.encoding();
                        let at = Lines::new(text).char_index(text, location.at, encoding);
                        edit = Some(Edit {
                            caret: at,
                            select: 0,
                            reveal: true,
                            focus: false,
                        });
                    }
                    Some(location) => {
                        let server = lock(&self.server);
                        let jump = Jump {
                            edit: location.path.starts_with(&server.root),
                            path: location.path,
                            at: location.at,
                            encoding: server.caps().encoding,
                        };
                        drop(server);
                        self.jump = Some(jump);
                    }
                    None => self.notice = Some("No definition found".into()),
                },
                Reply::Error(e) => self.notice = Some(e),
                _ => {}
            }
        }
        let reply = self
            .requests
            .format
            .and_then(|(id, _)| lock(&self.server).take_reply(id));
        if let Some(reply) = reply
            && let Some((_, version)) = self.requests.format.take()
        {
            match reply {
                // Unless typing went on meanwhile.
                Reply::Edits(edits) if version == self.version && self.changed_at.is_none() => {
                    if let Some(caret) = self.apply_edits(text, &edits, caret) {
                        edit = Some(Edit {
                            caret,
                            select: 0,
                            reveal: false,
                            focus: false,
                        });
                    }
                }
                Reply::Edits(_) => self.notice = Some("Not formatted: the text changed".into()),
                Reply::Error(e) => self.notice = Some(e),
                _ => {}
            }
        }
        edit
    }

    /// Make the server's `edits` to `text`, the editor's. Returns where the
    /// caret at `caret` goes: the same line and column, as near as there
    /// is. `None` if there were none.
    fn apply_edits(
        &mut self,
        text: &mut String,
        edits: &[protocol::TextEdit],
        caret: usize,
    ) -> Option<usize> {
        if edits.is_empty() {
            return None;
        }
        let encoding = self.encoding();
        let lines = Lines::new(text);
        let place = lines.position(text, caret, Encoding::Utf32);
        let ranges: Vec<(Range<usize>, String)> = edits
            .iter()
            .map(|e| {
                let start = lines.char_index(text, e.range.start, encoding);
                let end = lines.char_index(text, e.range.end, encoding).max(start);
                (start..end, e.new_text.clone())
            })
            .collect();
        apply(text, ranges);
        Some(Lines::new(text).char_index(text, place, Encoding::Utf32))
    }

    fn cancel_all(&mut self) {
        let mut server = lock(&self.server);
        let ids = [
            self.requests.definition,
            self.requests.format.map(|(id, _)| id),
        ];
        let ids = ids.into_iter().chain([
            self.hover.request(),
            self.completion.as_ref().and_then(|c| c.request()),
        ]);
        for id in ids.flatten() {
            server.cancel(id);
        }
    }
}

impl Drop for DocLsp {
    fn drop(&mut self) {
        self.cancel_all();
        lock(&self.server).notify(
            "textDocument/didClose",
            json!({"textDocument": {"uri": self.uri}}),
        );
    }
}

/// Move `diagnostics`, char ranges of `old`, to where the same text is in
/// `new`: those after the change by its length; those across it, to its
/// edges.
fn shift(diagnostics: &mut [Diag], old: &str, new: &str) {
    let (old_bytes, new_bytes) = (old.as_bytes(), new.as_bytes());
    let mut prefix = super::syntax::common_prefix(old_bytes, new_bytes);
    while !old.is_char_boundary(prefix) {
        prefix -= 1;
    }
    let mut suffix = super::syntax::common_suffix(&old_bytes[prefix..], &new_bytes[prefix..]);
    while !old.is_char_boundary(old.len() - suffix) || !new.is_char_boundary(new.len() - suffix) {
        suffix -= 1;
    }
    let start = old[..prefix].chars().count();
    let old_end = start + old[prefix..old.len() - suffix].chars().count();
    let new_end = start + new[prefix..new.len() - suffix].chars().count();
    for d in diagnostics {
        for at in [&mut d.range.start, &mut d.range.end] {
            if *at >= old_end {
                *at = *at - old_end + new_end;
            } else if *at > start {
                *at = (*at).min(new_end);
            }
        }
    }
}

/// Replace each of `edits`' char ranges of `text` with its text. Ranges
/// mustn't overlap; one that does is skipped. Insertions at the same place
/// go in in the order given, as LSP says.
fn apply(text: &mut String, edits: Vec<(Range<usize>, String)>) {
    let mut edits: Vec<(usize, (Range<usize>, String))> = edits.into_iter().enumerate().collect();
    // Last first, so the earlier ranges stay put.
    edits.sort_by_key(|(i, (range, _))| std::cmp::Reverse((range.start, range.end, *i)));
    let edits: Vec<(Range<usize>, String)> = edits.into_iter().map(|(_, edit)| edit).collect();
    // The byte offset of every char index needed, in one pass.
    let mut wanted: Vec<usize> = edits.iter().flat_map(|(r, _)| [r.start, r.end]).collect();
    wanted.sort_unstable();
    wanted.dedup();
    let mut bytes = Vec::with_capacity(wanted.len());
    let mut boundaries = text.char_indices().map(|(at, _)| at).chain([text.len()]);
    let mut seen = 0;
    for &at in &wanted {
        let found = boundaries.nth(at - seen);
        seen = at + 1;
        bytes.push((at, found.unwrap_or(text.len())));
    }
    let byte = |at: usize| bytes[bytes.partition_point(|&(c, _)| c < at)].1;
    let mut limit = usize::MAX;
    for (range, new) in edits {
        if range.end > limit {
            continue;
        }
        let (start, end) = (byte(range.start), byte(range.end));
        text.replace_range(start..end, &new);
        limit = range.start;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diag(range: Range<usize>) -> Diag {
        Diag {
            range,
            severity: 1,
            message: String::new(),
        }
    }

    #[test]
    fn problems_move_with_the_text() {
        let mut d = vec![diag(0..1), diag(4..7), diag(9..10)];
        // "ab" typed at 2.
        shift(&mut d, "x = foo + 1", "x ab= foo + 1");
        assert_eq!(
            d.iter().map(|d| d.range.clone()).collect::<Vec<_>>(),
            [0..1, 6..9, 11..12]
        );
        // "foo" deleted, across a problem.
        let mut d = vec![diag(4..7), diag(5..6), diag(9..10)];
        shift(&mut d, "x = foo + 1", "x =  + 1");
        assert_eq!(
            d.iter().map(|d| d.range.clone()).collect::<Vec<_>>(),
            [4..4, 4..4, 6..7]
        );
    }

    #[test]
    fn edits_apply_from_the_end() {
        let mut text = "fn  main(){\n1}".to_owned();
        apply(
            &mut text,
            vec![
                (2..4, " ".into()),
                (10..10, " ".into()),
                (12..12, "    ".into()),
                (13..13, "\n".into()),
            ],
        );
        assert_eq!(text, "fn main() {\n    1\n}");
        let mut text = "é𝄞x".to_owned();
        apply(&mut text, vec![(1..2, "-".into()), (3..3, "!".into())]);
        assert_eq!(text, "é-x!");
        // An overlapping edit is skipped.
        let mut text = "abcdef".to_owned();
        apply(&mut text, vec![(1..4, "X".into()), (3..5, "Y".into())]);
        assert_eq!(text, "abcYf");
    }

    #[test]
    fn commands_split() {
        assert_eq!(
            split_command("pyright-langserver  --stdio"),
            ["pyright-langserver", "--stdio"]
        );
        assert_eq!(
            split_command(r#""C:\Program Files\x.exe" -a "" b"#),
            [r"C:\Program Files\x.exe", "-a", "", "b"]
        );
    }

    #[test]
    fn rust_roots_are_workspaces() {
        let dir = std::env::temp_dir().join(format!("peregrine-lsp-root-{}", std::process::id()));
        let member = dir.join("crates").join("a");
        std::fs::create_dir_all(member.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/a\"]\n",
        )
        .unwrap();
        std::fs::write(member.join("Cargo.toml"), "[package]\nname = \"a\"\n").unwrap();
        let rust = CONFIGS.iter().find(|c| c.name == "Rust").unwrap();
        assert_eq!(project_root(&member.join("src").join("lib.rs"), rust), dir);
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"b\"\n").unwrap();
        assert_eq!(
            project_root(&member.join("src").join("lib.rs"), rust),
            member
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
