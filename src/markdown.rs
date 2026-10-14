//! Markdown rendering for the file viewer windows (see `crate::viewer`), by
//! `egui_commonmark`, plus what it doesn't handle itself: `<details>`
//! sections, and links to local files.

use std::{
    ops::Range,
    path::{MAIN_SEPARATOR, Path, PathBuf},
};

use egui::{CollapsingHeader, Context, OutputCommand, RichText, Ui, ViewportId};
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

/// Render the Markdown document `text`, split into `blocks` by
/// [split_details], in viewer window `window`. `path` is the file it was read
/// from: relative image paths load from its folder.
pub fn show(
    ui: &mut Ui,
    text: &str,
    blocks: &[Block],
    path: &Path,
    cache: &mut CommonMarkCache,
    window: ViewportId,
) {
    ui.style_mut().url_in_tooltip = true;
    show_blocks(ui, blocks, text, cache, &image_base(path), window);
}

/// The prefix `egui_commonmark` puts on image paths without a scheme, so
/// `![](img/logo.png)` loads relative to the document's folder rather than
/// the process's cwd. Native separators throughout: egui_extras' file loader
/// strips `file://` and hands the rest to the filesystem as is.
fn image_base(path: &Path) -> String {
    match path.parent() {
        Some(dir) => format!("file://{}{MAIN_SEPARATOR}", dir.display()),
        None => "file://".to_string(),
    }
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
