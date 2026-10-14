//! File viewer windows. Double-clicking a Markdown or plain-text file in the
//! Files panel opens it in its own native window (an egui deferred viewport):
//! Markdown rendered by `egui_commonmark` (see `crate::markdown`), plain text
//! as is. Each window re-reads its file when it changes on disk, so it can sit
//! beside an editor as a live preview.

use std::{
    ffi::OsStr,
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant, SystemTime},
};

use egui::{
    CentralPanel, Context, Frame, Id, Key, Label, Panel, RichText, ScrollArea, Ui, ViewportBuilder,
    ViewportCommand, ViewportId,
};
use egui_commonmark::CommonMarkCache;

use crate::{
    files::{format_size, open_with_default_app},
    markdown::{self, Block, is_markdown},
};

/// Extensions (lowercase) of plain-text files.
const TEXT_EXTENSIONS: [&str; 1] = ["txt"];
/// Names (lowercase) of plain-text files that have no extension as far as
/// `Path::extension` is concerned: a leading dot starts the name.
const TEXT_FILE_NAMES: [&str; 1] = [".gitignore"];

/// How often an open window checks its file's modification time.
const RELOAD_POLL: Duration = Duration::from_secs(1);

/// How much of a file a window reads. Every window draws on the UI thread,
/// so laying out a huge log would stall the whole app; the rest is left to
/// "Open externally".
const MAX_BYTES: u64 = 1 << 20;

const WINDOW_SIZE: [f32; 2] = [820., 900.];
const MIN_WINDOW_SIZE: [f32; 2] = [300., 200.];

/// Whether `path` is a file type the viewer shows, so a double-click views it
/// here rather than handing it to the OS's default program.
pub fn can_view(path: &Path) -> bool {
    is_markdown(path) || is_text(path)
}

fn is_text(path: &Path) -> bool {
    let lower = |s: &OsStr| s.to_string_lossy().to_lowercase();
    path.extension()
        .map(lower)
        .is_some_and(|e| TEXT_EXTENSIONS.contains(&e.as_str()))
        || path
            .file_name()
            .map(lower)
            .is_some_and(|n| TEXT_FILE_NAMES.contains(&n.as_str()))
}

/// How a window renders its file.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Markdown,
    /// Plain text, in the monospace font.
    Text,
}

impl Kind {
    fn of(path: &Path) -> Self {
        if is_markdown(path) {
            Self::Markdown
        } else {
            Self::Text
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Markdown => "Markdown",
            Self::Text => "Text",
        }
    }
}

/// The open viewer windows. Shared with each window's render callback, which
/// egui calls outside the main window's pass, hence the `Arc<Mutex>`.
/// Everything runs on the UI thread, so the lock is never contended.
#[derive(Default)]
pub struct FileViewers {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    docs: Vec<Doc>,
    /// Files to open (or focus and reload, if already open) at the end of the
    /// main window's next pass. Queued rather than opened directly so the
    /// Files panel and links clicked in a viewer window share one path.
    to_open: Vec<PathBuf>,
    /// One Markdown cache for every window: building it loads syntect's
    /// syntax and theme sets, so it's created on the first Markdown open, not
    /// at startup.
    cache: Option<CommonMarkCache>,
    /// Source of unique viewport ids.
    next_id: u64,
}

/// One viewer window.
struct Doc {
    id: ViewportId,
    path: PathBuf,
    kind: Kind,
    /// The file's contents (at most [MAX_BYTES] of them), or why it couldn't
    /// be read.
    text: Result<String, String>,
    /// The file's size, when `text` holds only the start of it.
    truncated: Option<u64>,
    /// Markdown `text` split at its `<details>` blocks; see
    /// [markdown::split_details]. Empty for plain text.
    blocks: Vec<Block>,
    /// Modification time of the contents in `text`, to notice edits.
    modified: Option<SystemTime>,
    last_poll: Instant,
    /// Cleared when the user closes the window; the main pass then drops it.
    open: bool,
    /// A one-off message in the toolbar, e.g. a link whose target is missing.
    notice: Option<String>,
}

impl Doc {
    fn new(id: ViewportId, path: PathBuf, kind: Kind) -> Self {
        let mut doc = Self {
            id,
            path,
            kind,
            text: Ok(String::new()),
            truncated: None,
            blocks: Vec::new(),
            modified: None,
            last_poll: Instant::now(),
            open: true,
            notice: None,
        };
        doc.reload();
        doc
    }

    fn reload(&mut self) {
        self.modified = modified_time(&self.path);
        self.last_poll = Instant::now();
        let read = read_text(&self.path);
        self.truncated = read.as_ref().ok().and_then(|(_, size)| *size);
        self.text = read
            .map(|(text, _)| text)
            .map_err(|e| format!("Couldn't read {}: {e}", self.path.display()));
        self.blocks = match (&self.text, self.kind) {
            (Ok(text), Kind::Markdown) => markdown::split_details(text),
            _ => Vec::new(),
        };
    }

    /// Re-read the file if its modification time changed since it was read.
    /// Polled at most once per [RELOAD_POLL].
    fn reload_if_changed(&mut self) {
        if self.last_poll.elapsed() < RELOAD_POLL {
            return;
        }
        self.last_poll = Instant::now();
        if modified_time(&self.path) != self.modified {
            self.reload();
        }
    }

    fn title(&self) -> String {
        let name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string());
        format!("{name} - {}", self.kind.name())
    }
}

fn modified_time(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Read the text file at `path`. A file bigger than [MAX_BYTES] is cut at
/// the last line break within that, and its size returned alongside.
fn read_text(path: &Path) -> io::Result<(String, Option<u64>)> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 <= MAX_BYTES {
        return Ok((decode(&bytes), None));
    }
    bytes.truncate(MAX_BYTES as usize);
    let mut text = decode(&bytes);
    // Drop the partial last line, and with it any character cut in two.
    if let Some(end) = text.rfind('\n') {
        text.truncate(end + 1);
    }
    let size = fs::metadata(path).map_or(MAX_BYTES, |m| m.len());
    Ok((text, Some(size)))
}

/// A text file's contents: UTF-16 if it starts with a UTF-16 byte order mark
/// (as Windows PowerShell's `>` writes), otherwise UTF-8. Lossy, so a stray
/// bad byte doesn't hide the whole file.
fn decode(bytes: &[u8]) -> String {
    let utf16 = |rest: &[u8], from_bytes: fn([u8; 2]) -> u16| {
        let units = rest.as_chunks::<2>().0.iter().map(|&c| from_bytes(c));
        char::decode_utf16(units)
            .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect()
    };
    match bytes {
        [0xFF, 0xFE, rest @ ..] => utf16(rest, u16::from_le_bytes),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, u16::from_be_bytes),
        _ => {
            let text = String::from_utf8_lossy(bytes);
            text.strip_prefix('\u{feff}').unwrap_or(&text).to_string()
        }
    }
}

impl FileViewers {
    /// Open `path` in a viewer window, or focus and reload its window if it's
    /// already open. Takes effect in [Self::show], later this pass.
    pub fn open(&self, path: PathBuf) {
        self.lock().to_open.push(path);
    }

    /// Keep each open window alive. Call once per pass of the main window:
    /// egui closes any viewport that isn't shown in its parent's pass.
    pub fn show(&self, ctx: &Context) {
        let windows: Vec<(ViewportId, String)> = {
            let mut inner = self.lock();
            for path in std::mem::take(&mut inner.to_open) {
                inner.open_now(ctx, path);
            }
            inner.docs.retain(|d| d.open);
            inner.docs.iter().map(|d| (d.id, d.title())).collect()
        };

        // The lock is released first: without native multi-window support
        // egui runs the callback right here, embedded in the main window.
        for (id, title) in windows {
            let builder = ViewportBuilder::default()
                .with_title(title)
                .with_inner_size(WINDOW_SIZE)
                .with_min_inner_size(MIN_WINDOW_SIZE)
                .with_icon(crate::app_icon());
            let inner = Arc::clone(&self.inner);
            ctx.show_viewport_deferred(id, builder, move |ui, _class| {
                lock(&inner).show_window(ui, id);
            });
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        lock(&self.inner)
    }
}

/// A panic mid-render can't leave [Inner] half-updated in a way that matters,
/// so recover from poisoning rather than taking every window down with it.
fn lock(inner: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    inner.lock().unwrap_or_else(|e| e.into_inner())
}

impl Inner {
    fn open_now(&mut self, ctx: &Context, path: PathBuf) {
        let kind = Kind::of(&path);
        // Compare canonical paths so `./README.md` and `docs/../README.md`
        // find the same window.
        let path = fs::canonicalize(&path).map(strip_verbatim).unwrap_or(path);
        if let Some(doc) = self.docs.iter_mut().find(|d| d.path == path && d.open) {
            doc.reload();
            ctx.send_viewport_cmd_to(doc.id, ViewportCommand::Focus);
            return;
        }
        let id = ViewportId::from_hash_of(("file_viewer", self.next_id));
        self.next_id += 1;
        self.docs.push(Doc::new(id, path, kind));
    }

    /// Render the window for viewport `id`. Runs in that viewport's own pass.
    fn show_window(&mut self, ui: &mut Ui, id: ViewportId) {
        let Self {
            docs,
            to_open,
            cache,
            ..
        } = self;
        let Some(doc) = docs.iter_mut().find(|d| d.id == id) else {
            return;
        };
        let ctx = ui.ctx().clone();

        if ctx.input(|i| i.viewport().close_requested()) {
            // eframe repaints the main window too, whose `show` drops us.
            doc.open = false;
            return;
        }

        if ctx.input(|i| i.key_pressed(Key::F5)) {
            doc.reload();
        } else {
            doc.reload_if_changed();
        }
        // Keep polling for edits while the window is idle.
        ctx.request_repaint_after(RELOAD_POLL);

        // Panel and scroll state live in context-wide memory, so salt their
        // ids with the viewport's: each window keeps its own scroll position.
        Panel::top(Id::new(("viewer_toolbar", id)))
            .resizable(false)
            .show(ui, |ui| toolbar(doc, ui));

        CentralPanel::default()
            .frame(Frame::central_panel(ui.style()).inner_margin(egui::Margin::symmetric(16, 8)))
            .show(ui, |ui| {
                ScrollArea::vertical()
                    .id_salt(("viewer_scroll", id))
                    .auto_shrink([false, false])
                    .show(ui, |ui| match (&doc.text, doc.kind) {
                        (Ok(text), Kind::Markdown) => {
                            let cache = cache.get_or_insert_with(CommonMarkCache::default);
                            markdown::show(ui, text, &doc.blocks, &doc.path, cache, id);
                        }
                        (Ok(text), Kind::Text) => {
                            ui.add(Label::new(RichText::new(text).monospace()).selectable(true));
                        }
                        (Err(e), _) => {
                            ui.label(RichText::new(e).color(ui.visuals().error_fg_color));
                        }
                    });
            });

        if doc.kind == Kind::Markdown {
            let base_dir = doc.path.parent().unwrap_or(Path::new(""));
            for path in markdown::take_local_links(&ctx, base_dir) {
                open_link(doc, path, to_open, &ctx);
            }
        }
    }
}

/// Path, notices, and the Reload / Open externally buttons.
fn toolbar(doc: &mut Doc, ui: &mut Ui) {
    ui.horizontal(|ui| {
        if ui
            .button("⟳ Reload")
            .on_hover_text("Re-read the file (F5)")
            .clicked()
        {
            doc.reload();
            doc.notice = None;
        }
        if ui
            .button("Open externally")
            .on_hover_text("Open with the default program for this file type")
            .clicked()
        {
            doc.notice = open_with_default_app(&doc.path)
                .err()
                .map(|e| format!("Couldn't open: {e}"));
        }
        ui.label(RichText::new(doc.path.display().to_string()).weak());
    });
    if let Some(size) = doc.truncated {
        let msg = format!(
            "Showing the first {} MB of {}. Open externally to see it all.",
            MAX_BYTES >> 20,
            format_size(size)
        );
        ui.label(RichText::new(msg).color(ui.visuals().warn_fg_color));
    }
    if let Some(notice) = &doc.notice {
        ui.label(RichText::new(notice).color(ui.visuals().error_fg_color));
    }
}

/// Open `path`, a local file linked from `doc`: in another viewer window if
/// it's a type the viewer shows, otherwise (a folder, a source file) with its
/// default program.
fn open_link(doc: &mut Doc, path: PathBuf, to_open: &mut Vec<PathBuf>, ctx: &Context) {
    if !path.exists() {
        doc.notice = Some(format!("Link target not found: {}", path.display()));
    } else if can_view(&path) {
        to_open.push(path);
        // The main window's pass is what opens it.
        ctx.request_repaint_of(ViewportId::ROOT);
    } else if let Err(e) = open_with_default_app(&path) {
        doc.notice = Some(format!("Couldn't open {}: {e}", path.display()));
    }
}

/// `fs::canonicalize` on Windows returns `\\?\C:\...` paths, which display
/// badly and which some programs refuse. Drop the prefix when it's a plain
/// drive path.
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_viewable_files() {
        assert!(can_view(Path::new("a/README.md")));
        assert!(can_view(Path::new("notes.txt")));
        assert!(can_view(Path::new("NOTES.TXT")));
        assert!(can_view(Path::new("repo/.gitignore")));
        assert!(!can_view(Path::new("a/main.rs")));
        assert!(!can_view(Path::new("txt")));
        assert!(!can_view(Path::new("a/.gitignore.bak")));
        assert_eq!(Kind::of(Path::new("README.md")), Kind::Markdown);
        assert_eq!(Kind::of(Path::new(".gitignore")), Kind::Text);
    }

    #[test]
    fn decodes_text() {
        assert_eq!(decode(b"\xEF\xBB\xBFhi\n"), "hi\n");
        assert_eq!(decode(b"a\xFFb"), "a\u{FFFD}b");
        assert_eq!(decode(b"\xFF\xFEh\0i\0"), "hi");
        assert_eq!(decode(b"\xFE\xFF\0h\0i"), "hi");
    }

    #[test]
    fn truncates_big_files_at_a_line_break() {
        let path =
            std::env::temp_dir().join(format!("peregrine_viewer_{}.txt", std::process::id()));
        let line = "0123456789abcdef\n"; // 17 bytes, so MAX_BYTES lands mid-line.
        let len = MAX_BYTES as usize / line.len() + 10;
        fs::write(&path, line.repeat(len)).unwrap();
        let read = read_text(&path);
        fs::remove_file(&path).unwrap();

        let (text, size) = read.unwrap();
        assert_eq!(size, Some((line.len() * len) as u64));
        assert!(text.len() as u64 <= MAX_BYTES);
        assert!(text.ends_with('\n'));
        assert_eq!(text.len() % line.len(), 0);
    }
}
