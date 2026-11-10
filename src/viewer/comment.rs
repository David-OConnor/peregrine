//! Ctrl+/ in the editor: comment out lines of source, or uncomment them, with
//! the markers of the file's language (see `syntax::comment`).

use std::ops::Range;

use egui::text::{CCursor, CCursorRange};

/// How a language comments out a line.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Comment {
    /// A marker before the line, such as `// `.
    Line(&'static str),
    /// Markers around the line, such as `<!-- ` and ` -->`, for languages
    /// with no line comments.
    Block(&'static str, &'static str),
}

/// Text inserted, or removed, at a byte offset: where, how many bytes
/// removed, and what's inserted in their place.
type Edit = (usize, usize, &'static str);

/// Ctrl+/, as in VS Code and JetBrains IDEs: comment out the lines `range`
/// touches, or uncomment them if they all are already. A selection ending at
/// the start of a line leaves that line. Blank lines stay as they are, unless
/// there are only blank lines. Markers go at the lines' least indentation, so
/// they line up. Returns the selection over the same text as before, taking
/// in the markers at its ends.
pub(super) fn toggle(text: &mut String, range: CCursorRange, comment: Comment) -> CCursorRange {
    let byte = |c: CCursor| {
        text.char_indices()
            .nth(c.index.0)
            .map_or(text.len(), |(i, _)| i)
    };
    let (primary, secondary) = (byte(range.primary), byte(range.secondary));
    let (start, end) = (primary.min(secondary), primary.max(secondary));
    let from = text[..start].rfind('\n').map_or(0, |i| i + 1);
    let last = if end > start && text[..end].ends_with('\n') {
        end - 1
    } else {
        end
    };
    let to = text[last..].find('\n').map_or(text.len(), |i| last + i);

    // Each line, without its newline, and where it starts.
    let lines: Vec<(usize, &str)> = text[from..to]
        .split('\n')
        .scan(from, |offset, line| {
            let at = *offset;
            *offset += line.len() + 1;
            Some((at, line))
        })
        .collect();
    let mut targets: Vec<_> = lines
        .iter()
        .copied()
        .filter(|(_, line)| !line.trim().is_empty())
        .collect();
    if targets.is_empty() {
        targets = lines;
    }

    let uncomment: Option<Vec<[Edit; 2]>> = targets
        .iter()
        .map(|&(at, line)| {
            let [open, close] = markers(line, comment)?;
            Some([
                (at + open.start, open.len(), ""),
                (at + close.start, close.len(), ""),
            ])
        })
        .collect();
    let edits: Vec<Edit> = match uncomment {
        Some(edits) => edits.into_iter().flatten().collect(),
        None => {
            let column = targets
                .iter()
                .map(|(_, line)| indent(line))
                .min()
                .unwrap_or(0);
            let (open, close) = match comment {
                Comment::Line(marker) => (marker, ""),
                Comment::Block(open, close) => (open, close),
            };
            targets
                .iter()
                .flat_map(|&(at, line)| {
                    [
                        (at + column, 0, open),
                        (at + line.trim_end().len().max(column), 0, close),
                    ]
                })
                .collect()
        }
    };

    let mut edited =
        String::with_capacity(text.len() + edits.iter().map(|e| e.2.len()).sum::<usize>());
    let mut copied = 0;
    for &(at, removed, inserted) in &edits {
        edited.push_str(&text[copied..at]);
        edited.push_str(inserted);
        copied = at + removed;
    }
    edited.push_str(&text[copied..]);

    // Where byte `p` of the text is in the edited one. Text inserted at `p`
    // goes before it, except at the start of a selection, which takes it in.
    let cursor = |p: usize, start: bool| {
        let p = edits
            .iter()
            .take_while(|&&(at, ..)| at < p || (at == p && !start))
            .fold(p, |new, &(at, removed, inserted)| {
                new + inserted.len() - removed.min(p - at)
            });
        CCursor::new(edited[..p].chars().count())
    };
    let range = CCursorRange {
        primary: cursor(primary, primary < secondary),
        secondary: cursor(secondary, secondary < primary),
        h_pos: None,
    };
    *text = edited;
    range
}

/// How many bytes of spaces and tabs `line` starts with.
fn indent(line: &str) -> usize {
    line.len() - line.trim_start_matches([' ', '\t']).len()
}

/// Where `line`'s opening and closing markers are, if it's commented out,
/// each with a space inside it. The closing one is empty for a line comment.
fn markers(line: &str, comment: Comment) -> Option<[Range<usize>; 2]> {
    let (open, close) = match comment {
        Comment::Line(marker) => (marker.trim_end(), ""),
        Comment::Block(open, close) => (open.trim_end(), close.trim_start()),
    };
    let start = indent(line);
    // Case-insensitively, for words such as batch files' `rem`.
    if !line
        .get(start..start + open.len())
        .is_some_and(|marker| marker.eq_ignore_ascii_case(open))
    {
        return None;
    }
    let mut open_end = start + open.len();
    if open.ends_with(char::is_alphanumeric) && line[open_end..].starts_with(char::is_alphanumeric)
    {
        return None;
    }
    let (mut close_start, close_end) = match comment {
        Comment::Line(_) => (line.len(), line.len()),
        Comment::Block(..) => {
            let end = line.trim_end().len();
            if end < open_end + close.len() || !line[..end].ends_with(close) {
                return None;
            }
            (end - close.len(), end)
        }
    };
    // A space between the markers goes with the first.
    if open_end < close_start && line[open_end..].starts_with(' ') {
        open_end += 1;
    }
    if open_end < close_start && close_start < close_end && line[..close_start].ends_with(' ') {
        close_start -= 1;
    }
    Some([start..open_end, close_start..close_end])
}
