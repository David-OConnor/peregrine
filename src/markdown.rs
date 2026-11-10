//! Markdown rendering for the file viewer (see `crate::viewer`), by
//! `egui_commonmark`, plus what it doesn't handle itself: `<details>`
//! sections, and links to local files.

use std::{
    ops::Range,
    path::{MAIN_SEPARATOR, Path, PathBuf},
};

use egui::{
    CollapsingHeader, Context, Id, OutputCommand, RichText, Ui, collapsing_header::CollapsingState,
};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use pulldown_cmark::{Event, Options, Parser, Tag};

/// Extensions (lowercase) that [is_markdown] treats as Markdown.
const MARKDOWN_EXTENSIONS: [&str; 5] = ["md", "markdown", "mdown", "mkd", "mkdn"];

/// Whether `path` has a Markdown extension, so the viewer renders it rather
/// than showing it as plain text.
pub fn is_markdown(path: &Path) -> bool {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .is_some_and(|e| MARKDOWN_EXTENSIONS.contains(&e.as_str()))
}

/// How [show] draws `<details>` sections.
pub enum Details<'a> {
    /// For the user, who opens and closes them. Whether each is open is
    /// recorded here, by its index.
    Shown(&'a mut Vec<bool>),
    /// For the viewer's find (see `crate::viewer::search`): open or closed as
    /// recorded by a [Details::Shown] pass, at once rather than animated.
    Measured(&'a [bool]),
}

/// Render the Markdown document `text`, split into `blocks` by
/// [split_details], in the viewer tab with id `doc`. `path` is the file it was
/// read from: relative image paths load from its folder.
pub fn show(
    ui: &mut Ui,
    text: &str,
    blocks: &[Block],
    path: &Path,
    cache: &mut CommonMarkCache,
    doc: Id,
    details: &mut Details,
) {
    ui.style_mut().url_in_tooltip = true;
    show_blocks(ui, blocks, text, cache, &image_base(path), doc, details);
}

/// The prefix `egui_commonmark` puts on image paths without a scheme, so
/// `![](img/logo.png)` loads relative to the document's folder rather than
/// the process's cwd. Native separators throughout: egui_extras' file loader
/// strips `file://` and hands the rest to the filesystem as is, except that on
/// Windows it reads `file://host\share` as the UNC path `\\host\share`, and
/// needs the empty-host form `file:///C:\...` for a drive path.
fn image_base(path: &Path) -> String {
    let Some(dir) = path.parent() else {
        return "file://".to_string();
    };
    // A root (`C:\`, `\\server\share\`) already ends in a separator.
    let dir = dir.display().to_string();
    let dir = dir.trim_end_matches(['/', '\\']);
    let dir = if !cfg!(windows) {
        dir.to_string()
    } else if let Some(unc) = dir.strip_prefix(r"\\") {
        unc.to_string()
    } else {
        format!("/{dir}")
    };
    format!("file://{dir}{MAIN_SEPARATOR}")
}

/// A piece of a document. egui_commonmark prints HTML as raw text, so
/// `<details>` / `<summary>` blocks are cut out before rendering and shown as
/// collapsing headers, with the Markdown between them rendered as usual.
#[derive(Debug, PartialEq)]
pub enum Block {
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
    DetailsOpen {
        open: bool,
    },
    DetailsClose,
    /// A whole `<summary>…</summary>` element, as plain text.
    Summary(String),
}

/// Split `text` at the `<details>`, `<summary>` and `</details>` tags in its
/// top-level HTML blocks. A document without any is one [Block::Markdown].
/// An unclosed `<details>` runs to the end of the document.
pub fn split_details(text: &str) -> Vec<Block> {
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
                    stack
                        .last_mut()
                        .map_or(&mut root, |d| &mut d.body)
                        .push(block);
                }
            }
        }
    }

    let body = stack.last_mut().map_or(&mut root, |d| &mut d.body);
    push_markdown(body, text, pos..text.len());
    while let Some(d) = stack.pop() {
        let block = d.finish();
        stack
            .last_mut()
            .map_or(&mut root, |d| &mut d.body)
            .push(block);
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
                    let len = rest[close..]
                        .find('>')
                        .map_or(rest.len(), |g| close + g + 1);
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

/// Render `blocks`, pieces of the document `text`, in the viewer tab with id `doc`.
fn show_blocks(
    ui: &mut Ui,
    blocks: &[Block],
    text: &str,
    cache: &mut CommonMarkCache,
    image_base: &str,
    doc: Id,
    details: &mut Details,
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
                let salt = ("md_details", doc, *index);
                let id = ui.make_persistent_id(salt);
                let mut header = CollapsingHeader::new(RichText::new(summary).strong())
                    .id_salt(salt)
                    .default_open(*open);
                if let Details::Measured(states) = details {
                    let open = states.get(*index).copied().unwrap_or(*open);
                    // An animation time of zero jumps straight to the end.
                    ui.ctx().animate_bool_with_time(id, open, 0.);
                    header = header.open(Some(open));
                }
                header.show(ui, |ui| {
                    show_blocks(ui, body, text, cache, image_base, doc, details)
                });
                if let Details::Shown(states) = details {
                    if states.len() <= *index {
                        states.resize(*index + 1, false);
                    }
                    states[*index] =
                        CollapsingState::load(ui.ctx(), id).map_or(*open, |s| s.is_open());
                }
            }
        }
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

/// egui turns a clicked link into an `OpenUrl` command for the OS. Take back
/// the ones pointing at local files and return them for the viewer to open,
/// resolved against `base_dir` (the document's folder) rather than the
/// process's cwd. Web links are left for egui to open in the browser.
pub fn take_local_links(ctx: &Context, base_dir: &Path) -> Vec<PathBuf> {
    let mut targets = Vec::new();

    ctx.output_mut(|o| {
        o.commands.retain(|cmd| {
            let OutputCommand::OpenUrl(open) = cmd else {
                return true;
            };
            match link_target(&open.url, base_dir) {
                LinkTarget::Web => true,
                LinkTarget::Anchor => false,
                LinkTarget::Local(path) => {
                    targets.push(path);
                    false
                }
            }
        })
    });

    targets
}
