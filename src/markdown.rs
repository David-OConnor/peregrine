//! Markdown viewer windows. Double-clicking a Markdown file in the Files panel
//! opens it in its own native window (an egui deferred viewport), rendered by
//! `egui_commonmark`. Each window re-reads its file when it changes on disk,
//! so it can sit beside an editor as a live preview.

use std::{
    fs,
    ops::Range,
    path::{MAIN_SEPARATOR, Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant, SystemTime},
};

use egui::{
    CentralPanel, CollapsingHeader, Context, Frame, Id, Key, OutputCommand, Panel, RichText,
    ScrollArea, Ui, ViewportBuilder, ViewportCommand, ViewportId,
};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use pulldown_cmark::{Event, Options, Parser, Tag};

use crate::files::open_with_default_app;

/// Extensions (lowercase) that [is_markdown] treats as Markdown.
const MARKDOWN_EXTENSIONS: [&str; 5] = ["md", "markdown", "mdown", "mkd", "mkdn"];

/// How often an open window checks its file's modification time.
const RELOAD_POLL: Duration = Duration::from_secs(1);

const WINDOW_SIZE: [f32; 2] = [820., 900.];
const MIN_WINDOW_SIZE: [f32; 2] = [300., 200.];

/// Whether `path` has a Markdown extension, so a double-click views it here
/// rather than handing it to the OS's default program.
pub fn is_markdown(path: &Path) -> bool {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .is_some_and(|e| MARKDOWN_EXTENSIONS.contains(&e.as_str()))
}

/// The open Markdown windows. Shared with each window's render callback,
/// which egui calls outside the main window's pass, hence the `Arc<Mutex>`.
/// Everything runs on the UI thread, so the lock is never contended.
#[derive(Default)]
pub struct MarkdownViewers {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    docs: Vec<Doc>,
    /// Files to open (or focus and reload, if already open) at the end of the
    /// main window's next pass. Queued rather than opened directly so the
    /// Files panel and links clicked in a viewer window share one path.
    to_open: Vec<PathBuf>,
    /// One cache for every window: building it loads syntect's syntax and
    /// theme sets, so it's created on the first open, not at startup.
    cache: Option<CommonMarkCache>,
    /// Source of unique viewport ids.
    next_id: u64,
}

/// One viewer window.
struct Doc {
    id: ViewportId,
    path: PathBuf,
    /// The file's contents, or why it couldn't be read.
    text: Result<String, String>,
    /// `text` split at its `<details>` blocks; see [split_details].
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
    fn new(id: ViewportId, path: PathBuf) -> Self {
        let mut doc = Self {
            id,
            path,
            text: Ok(String::new()),
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
        // Lossy so a stray non-UTF-8 byte doesn't hide the whole document.
        self.text = fs::read(&self.path)
            .map(|bytes| {
                let text = String::from_utf8_lossy(&bytes);
                text.strip_prefix('\u{feff}').unwrap_or(&text).to_string()
            })
            .map_err(|e| format!("Couldn't read {}: {e}", self.path.display()));
        self.blocks = match &self.text {
            Ok(text) => split_details(text),
            Err(_) => Vec::new(),
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
        format!("{name} - Markdown")
    }

    /// The prefix `egui_commonmark` puts on image paths without a scheme, so
    /// `![](img/logo.png)` loads relative to this file's folder rather than
    /// the process's cwd. Native separators throughout: egui_extras' file
    /// loader strips `file://` and hands the rest to the filesystem as is.
    fn image_base(&self) -> String {
        match self.path.parent() {
            Some(dir) => format!("file://{}{MAIN_SEPARATOR}", dir.display()),
            None => "file://".to_string(),
        }
    }
}

fn modified_time(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl MarkdownViewers {
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
                .with_min_inner_size(MIN_WINDOW_SIZE);
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
        // Compare canonical paths so `./README.md` and `docs/../README.md`
        // find the same window.
        let path = fs::canonicalize(&path).map(strip_verbatim).unwrap_or(path);
        if let Some(doc) = self.docs.iter_mut().find(|d| d.path == path && d.open) {
            doc.reload();
            ctx.send_viewport_cmd_to(doc.id, ViewportCommand::Focus);
            return;
        }
        let id = ViewportId::from_hash_of(("markdown_viewer", self.next_id));
        self.next_id += 1;
        self.docs.push(Doc::new(id, path));
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
        Panel::top(Id::new(("md_toolbar", id)))
            .resizable(false)
            .show_inside(ui, |ui| toolbar(doc, ui));

        CentralPanel::default()
            .frame(Frame::central_panel(ui.style()).inner_margin(egui::Margin::symmetric(16, 8)))
            .show_inside(ui, |ui| {
                ScrollArea::vertical()
                    .id_salt(("md_scroll", id))
                    .auto_shrink([false, false])
                    .show(ui, |ui| match &doc.text {
                        Ok(text) => {
                            ui.style_mut().url_in_tooltip = true;
                            let cache = cache.get_or_insert_with(CommonMarkCache::default);
                            let base = doc.image_base();
                            show_blocks(ui, &doc.blocks, text, cache, &base, id);
                        }
                        Err(e) => {
                            ui.label(RichText::new(e).color(ui.visuals().error_fg_color));
                        }
                    });
            });

        handle_links(doc, to_open, &ctx);
    }
}

/// A piece of a document. egui_commonmark prints HTML as raw text, so
/// `<details>` / `<summary>` blocks are cut out before rendering and shown as
/// collapsing headers, with the Markdown between them rendered as usual.
#[derive(Debug, PartialEq)]
enum Block {
    /// Markdown to render: a byte range of the document.
    Markdown(Range<usize>),
    Details {
        summary: String,
        /// The `open` attribute: expanded until the user collapses it.
        open: bool,
        /// Ordinal of this `<details>` in the document, for its egui id: it
        /// stays put however other sections are expanded or collapsed.
        index: usize,
        body: Vec<Block>,
    },
}

/// A `<details>` that's been opened but not yet closed, in [split_details].
struct OpenDetails {
    summary: Option<String>,
    open: bool,
    index: usize,
    body: Vec<Block>,
}

impl OpenDetails {
    fn finish(self) -> Block {
        Block::Details {
            // GitHub's label for a `<details>` without a `<summary>`.
            summary: self.summary.unwrap_or_else(|| "Details".to_string()),
            open: self.open,
            index: self.index,
            body: self.body,
        }
    }
}

#[derive(Debug, PartialEq)]
enum HtmlTag {
    DetailsOpen { open: bool },
    DetailsClose,
    /// A whole `<summary>…</summary>` element, as plain text.
    Summary(String),
}

/// Split `text` at the `<details>`, `<summary>` and `</details>` tags in its
/// top-level HTML blocks. A document without any is one [Block::Markdown].
/// An unclosed `<details>` runs to the end of the document.
fn split_details(text: &str) -> Vec<Block> {
    let mut root = Vec::new();
    let mut stack: Vec<OpenDetails> = Vec::new();
    let mut pos = 0;
    let mut count = 0;

    for (range, tag) in details_tags(text) {
        let body = stack.last_mut().map_or(&mut root, |d| &mut d.body);
        push_markdown(body, text, pos..range.start);
        pos = range.end;
        match tag {
            HtmlTag::DetailsOpen { open } => {
                stack.push(OpenDetails {
                    summary: None,
                    open,
                    index: count,
                    body: Vec::new(),
                });
                count += 1;
            }
            HtmlTag::Summary(summary) => {
                // Only the first `<summary>` labels a `<details>`; a stray
                // one is dropped.
                if let Some(d) = stack.last_mut().filter(|d| d.summary.is_none()) {
                    d.summary = Some(summary);
                }
            }
            HtmlTag::DetailsClose => {
                if let Some(d) = stack.pop() {
                    let block = d.finish();
                    stack.last_mut().map_or(&mut root, |d| &mut d.body).push(block);
                }
            }
        }
    }

    let body = stack.last_mut().map_or(&mut root, |d| &mut d.body);
    push_markdown(body, text, pos..text.len());
    while let Some(d) = stack.pop() {
        let block = d.finish();
        stack.last_mut().map_or(&mut root, |d| &mut d.body).push(block);
    }
    root
}

fn push_markdown(blocks: &mut Vec<Block>, text: &str, range: Range<usize>) {
    if !text[range.clone()].trim().is_empty() {
        blocks.push(Block::Markdown(range));
    }
}

/// The details-related tags in `text`'s HTML blocks, with their byte ranges,
/// in order. Found with the same parser (and options) egui_commonmark uses,
/// so a `<details>` in a code block or inline in a paragraph is left alone.
/// Only top-level blocks count: splitting inside a list or quote would break
/// it apart.
fn details_tags(text: &str) -> Vec<(Range<usize>, HtmlTag)> {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_DEFINITION_LIST
        | Options::ENABLE_MATH;
    let mut tags = Vec::new();
    let mut depth = 0usize;
    for (event, range) in Parser::new_ext(text, options).into_offset_iter() {
        match event {
            Event::Start(tag) => {
                if depth == 0 && tag == Tag::HtmlBlock {
                    scan_html(&text[range.clone()], range.start, &mut tags);
                }
                depth += 1;
            }
            Event::End(_) => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    tags
}

/// Find details-related tags in the HTML `block`, which starts at byte
/// `offset` of the document.
fn scan_html(block: &str, offset: usize, tags: &mut Vec<(Range<usize>, HtmlTag)>) {
    // ASCII lowercasing keeps byte offsets, so indices into `lower` are valid
    // in `block` too.
    let lower = block.to_ascii_lowercase();
    let mut i = 0;
    while let Some(found) = lower[i..].find('<') {
        let at = i + found;
        let rest = &lower[at..];
        let (len, tag) = if let Some(end) = tag_end(rest, "<details") {
            let attrs = &rest["<details".len()..end];
            let open = attrs
                .split(|c: char| c.is_ascii_whitespace() || c == '/')
                .any(|a| a == "open" || a.starts_with("open="));
            (end + 1, HtmlTag::DetailsOpen { open })
        } else if let Some(end) = tag_end(rest, "</details") {
            (end + 1, HtmlTag::DetailsClose)
        } else if let Some(end) = tag_end(rest, "<summary") {
            let inner = end + 1;
            let (inner_end, len) = match rest[inner..].find("</summary") {
                Some(close) => {
                    let close = inner + close;
                    let len = rest[close..].find('>').map_or(rest.len(), |g| close + g + 1);
                    (close, len)
                }
                None => (rest.len(), rest.len()),
            };
            let summary = html_to_text(&block[at + inner..at + inner_end]);
            (len, HtmlTag::Summary(summary))
        } else {
            i = at + 1;
            continue;
        };
        tags.push((offset + at..offset + at + len, tag));
        i = at + len;
    }
}

/// If `s` starts with the tag `name` (lowercase, with its `<`), the index of
/// the tag's closing `>`.
fn tag_end(s: &str, name: &str) -> Option<usize> {
    let after = s.strip_prefix(name)?;
    if !after.starts_with(|c: char| c == '>' || c == '/' || c.is_ascii_whitespace()) {
        return None; // e.g. `<detailsfoo>`
    }
    s.find('>')
}

/// The text of an HTML fragment, for a `<summary>` label: tags dropped,
/// common entities decoded, and whitespace collapsed.
fn html_to_text(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => text.push(c),
            _ => {}
        }
    }
    let text = text
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Render `blocks`, pieces of the document `text`, in viewer window `window`.
fn show_blocks(
    ui: &mut Ui,
    blocks: &[Block],
    text: &str,
    cache: &mut CommonMarkCache,
    image_base: &str,
    window: ViewportId,
) {
    for (i, block) in blocks.iter().enumerate() {
        match block {
            Block::Markdown(range) => {
                // egui_commonmark ids its tables from `ui.id()` and a counter
                // that restarts with each `show`, so give each piece its own.
                ui.push_id(i, |ui| {
                    CommonMarkViewer::new()
                        .default_implicit_uri_scheme(image_base)
                        .show(ui, cache, &text[range.clone()]);
                });
            }
            Block::Details {
                summary,
                open,
                index,
                body,
            } => {
                CollapsingHeader::new(RichText::new(summary).strong())
                    .id_salt(("md_details", window, *index))
                    .default_open(*open)
                    .show(ui, |ui| {
                        show_blocks(ui, body, text, cache, image_base, window)
                    });
            }
        }
    }
}

/// Path, notice, and the Reload / Open externally buttons.
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
    if let Some(notice) = &doc.notice {
        ui.label(RichText::new(notice).color(ui.visuals().error_fg_color));
    }
}

/// Where a clicked link points.
#[derive(Debug, PartialEq)]
enum LinkTarget {
    /// A web (or other non-file) URL: left for egui to open in the browser.
    Web,
    /// A same-document `#heading` anchor. Dropped: the viewer can't scroll to
    /// headings.
    Anchor,
    Local(PathBuf),
}

fn link_target(url: &str, base_dir: &Path) -> LinkTarget {
    let local = if let Some(rest) = url.strip_prefix("file://") {
        // `file:///C:/x` -> `C:/x` on Windows; keep the root slash elsewhere.
        if cfg!(windows) {
            rest.trim_start_matches('/')
        } else {
            rest
        }
    } else if has_scheme(url) {
        return LinkTarget::Web;
    } else {
        url
    };
    let path = local.split(['#', '?']).next().unwrap_or("");
    if path.is_empty() {
        return LinkTarget::Anchor;
    }
    LinkTarget::Local(base_dir.join(percent_decode(path)))
}

/// Whether `url` starts with a URL scheme like `https:` or `mailto:`. A
/// single letter before the colon is a Windows drive (`C:\docs`), not a
/// scheme.
fn has_scheme(url: &str) -> bool {
    let Some((scheme, _)) = url.split_once(':') else {
        return false;
    };
    scheme.len() > 1
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Decode `%XX` escapes, e.g. `My%20Notes.md` -> `My Notes.md`. Malformed
/// escapes are kept as written.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(b)) => {
                out.push(b);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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

/// egui turns a clicked link into an `OpenUrl` command for the OS. Take back
/// the ones pointing at local files: Markdown opens in another viewer window,
/// anything else (a folder, a source file) goes to its default program,
/// resolved against this document's folder rather than the process's cwd.
fn handle_links(doc: &mut Doc, to_open: &mut Vec<PathBuf>, ctx: &Context) {
    let base_dir = doc.path.parent().unwrap_or(Path::new("")).to_path_buf();
    let mut targets = Vec::new();
    ctx.output_mut(|o| {
        o.commands.retain(|cmd| {
            let OutputCommand::OpenUrl(open) = cmd else {
                return true;
            };
            match link_target(&open.url, &base_dir) {
                LinkTarget::Web => true,
                LinkTarget::Anchor => false,
                LinkTarget::Local(path) => {
                    targets.push(path);
                    false
                }
            }
        })
    });

    for path in targets {
        if !path.exists() {
            doc.notice = Some(format!("Link target not found: {}", path.display()));
        } else if is_markdown(&path) {
            to_open.push(path);
            // The main window's pass is what opens it.
            ctx.request_repaint_of(ViewportId::ROOT);
        } else if let Err(e) = open_with_default_app(&path) {
            doc.notice = Some(format!("Couldn't open {}: {e}", path.display()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md<'a>(text: &'a str, block: &Block) -> &'a str {
        match block {
            Block::Markdown(r) => text[r.clone()].trim(),
            _ => panic!("expected Markdown, got {block:?}"),
        }
    }

    #[test]
    fn no_details_is_one_block() {
        let text = "# Hi\n\nSome `<details>` text.\n\n```\n<details>\n```\n";
        assert_eq!(split_details(text), vec![Block::Markdown(0..text.len())]);
    }

    #[test]
    fn splits_details() {
        let text = "Intro\n\n<details open>\n<summary>Notes &amp; <b>sources</b></summary>\n\n\
                    - a\n- b\n\n</details>\n\nOutro\n";
        let blocks = split_details(text);
        assert_eq!(blocks.len(), 3);
        assert_eq!(md(text, &blocks[0]), "Intro");
        let Block::Details {
            summary,
            open,
            index,
            body,
        } = &blocks[1]
        else {
            panic!("{:?}", blocks[1]);
        };
        assert_eq!(summary, "Notes & sources");
        assert!(*open);
        assert_eq!(*index, 0);
        assert_eq!(body.len(), 1);
        assert_eq!(md(text, &body[0]), "- a\n- b");
        assert_eq!(md(text, &blocks[2]), "Outro");
    }

    #[test]
    fn nested_and_unclosed_details() {
        let text =
            "<details>\n\nouter\n\n<details><summary>In</summary>\n\ninner\n\n</details>\n\ntail\n";
        let blocks = split_details(text);
        assert_eq!(blocks.len(), 1);
        let Block::Details {
            summary,
            open,
            body,
            ..
        } = &blocks[0]
        else {
            panic!("{:?}", blocks[0]);
        };
        assert_eq!(summary, "Details");
        assert!(!*open);
        assert_eq!(body.len(), 3);
        assert_eq!(md(text, &body[0]), "outer");
        assert!(matches!(&body[1], Block::Details { summary, index: 1, .. } if summary == "In"));
        assert_eq!(md(text, &body[2]), "tail");
    }

    #[test]
    fn details_in_list_is_left_alone() {
        let text = "- item\n\n  <details>\n\n  x\n\n  </details>\n";
        assert_eq!(split_details(text), vec![Block::Markdown(0..text.len())]);
    }

    #[test]
    fn detects_markdown() {
        assert!(is_markdown(Path::new("a/README.md")));
        assert!(is_markdown(Path::new("NOTES.Markdown")));
        assert!(!is_markdown(Path::new("a/main.rs")));
        assert!(!is_markdown(Path::new("md")));
    }

    #[test]
    fn classifies_links() {
        let base = Path::new("docs");
        assert_eq!(
            link_target("https://example.com/x.md", base),
            LinkTarget::Web
        );
        assert_eq!(link_target("mailto:a@b.c", base), LinkTarget::Web);
        assert_eq!(link_target("#usage", base), LinkTarget::Anchor);
        assert_eq!(
            link_target("guide/My%20Notes.md#setup", base),
            LinkTarget::Local(base.join("guide/My Notes.md"))
        );
        assert_eq!(
            link_target("../LICENSE", base),
            LinkTarget::Local(base.join("../LICENSE"))
        );
    }

    #[test]
    fn drive_letter_is_not_a_scheme() {
        assert!(!has_scheme(r"C:\docs\a.md"));
        assert!(has_scheme("https://x"));
        assert!(has_scheme("vscode-insiders://x"));
    }

    #[test]
    fn decodes_percent_escapes() {
        assert_eq!(percent_decode("a%20b%2Fc"), "a b/c");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }
}
