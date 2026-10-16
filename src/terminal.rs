//! Bounded scrollback shared by built-ins, local commands, and SSH output.
//! Lines have stable IDs so eviction cannot invalidate a running stream or
//! a text selection. Only text and styles are retained, never laid-out glyphs.

use std::{
    collections::VecDeque,
    mem::size_of,
    ops::Range,
    sync::atomic::{AtomicU64, Ordering},
};

use egui::{
    Color32, FontId,
    text::{ByteIndex, LayoutJob, LayoutSection},
};

use crate::{OutType, ansi};

pub(crate) const MAX_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_LINES: usize = 20_000;
pub(crate) const MAX_LINE_BYTES: usize = 16 * 1024;
pub(crate) const CLOSED_BYTES: usize = 8 * 1024 * 1024;
const PARSE_CHUNK_BYTES: usize = 4096;

#[derive(Default)]
pub(crate) struct Stream {
    parser: ansi::Parser,
    open: Option<u64>,
}

impl Stream {
    pub(crate) fn break_line(&mut self) {
        self.open = None;
    }
}

pub(crate) struct Span {
    range: Range<usize>,
    style: ansi::Style,
}

pub(crate) struct Line {
    pub(crate) id: u64,
    pub(crate) text: String,
    pub(crate) kind: OutType,
    spans: Vec<Span>,
    pub(crate) height: Option<f32>,
}

impl Line {
    fn bytes(&self) -> usize {
        size_of::<Self>() + self.text.capacity() + self.spans.capacity() * size_of::<Span>()
    }

    pub(crate) fn job(&self, font: &FontId, color: Color32, width: f32) -> LayoutJob {
        let mut job = LayoutJob::simple(self.text.clone(), font.clone(), color, width);
        if !self.spans.is_empty() {
            job.sections = self
                .spans
                .iter()
                .map(|span| LayoutSection {
                    leading_space: 0.,
                    byte_range: ByteIndex(span.range.start)..ByteIndex(span.range.end),
                    format: span.style.format(color, font.clone()),
                })
                .collect();
        }
        job
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Cursor {
    pub(crate) line: u64,
    pub(crate) character: usize,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Selection {
    pub(crate) anchor: Cursor,
    pub(crate) head: Cursor,
}

impl Selection {
    pub(crate) fn sorted(self) -> (Cursor, Cursor) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

#[derive(PartialEq)]
pub(crate) struct Metrics {
    pub(crate) font: FontId,
    pub(crate) width: f32,
    pub(crate) pixels_per_point: f32,
    pub(crate) spacing: f32,
}

pub struct Output {
    pub(crate) lines: VecDeque<Line>,
    pub(crate) offsets: Vec<f32>,
    pub(crate) metrics: Option<Metrics>,
    pub(crate) first_measured: Option<u64>,
    pub(crate) selection: Option<Selection>,
    pub(crate) dragging: bool,
    pub(crate) truncated: bool,
    pub(crate) id: u64,
    bytes: usize,
    revision: u64,
    next_id: u64,
}

impl Default for Output {
    fn default() -> Self {
        static NEXT_OUTPUT: AtomicU64 = AtomicU64::new(0);
        Self {
            lines: VecDeque::new(),
            offsets: Vec::new(),
            metrics: None,
            first_measured: None,
            selection: None,
            dragging: false,
            truncated: false,
            id: NEXT_OUTPUT.fetch_add(1, Ordering::Relaxed),
            bytes: 0,
            revision: 0,
            next_id: 0,
        }
    }
}

impl Output {
    pub(crate) fn next_id(&self) -> u64 {
        self.next_id
    }
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        self.bytes
            + (self.lines.capacity() - self.lines.len()) * size_of::<Line>()
            + self.offsets.capacity() * size_of::<f32>()
    }

    pub(crate) fn clear(&mut self) {
        self.lines = VecDeque::new();
        self.offsets = Vec::new();
        self.bytes = 0;
        self.metrics = None;
        self.first_measured = None;
        self.selection = None;
        self.dragging = false;
        self.truncated = false;
        self.revision += 1;
    }

    /// Each built-in/echo is a separate block, even without a final newline.
    pub(crate) fn push(&mut self, text: &str, kind: OutType) {
        let mut stream = Stream::default();
        if text.is_empty() {
            self.new_line(kind);
            self.trim();
            self.revision += 1;
        } else {
            self.write(text, kind, &mut stream);
        }
    }

    /// Continue an unfinished line in this stream, including when the other
    /// stream has written in between. An echo/clear ends the continuation.
    pub(crate) fn write(&mut self, mut text: &str, kind: OutType, stream: &mut Stream) {
        while !text.is_empty() {
            let end = text.floor_char_boundary(PARSE_CHUNK_BYTES.min(text.len()));
            let (chunk, rest) = text.split_at(end);
            text = rest;
            for segment in stream.parser.feed(chunk) {
                let style = segment.style();
                for piece in segment.text.split_inclusive('\n') {
                    let id = match stream.open.filter(|&id| self.index(id).is_some()) {
                        Some(id) => id,
                        None => {
                            let id = self.new_line(kind);
                            stream.open = Some(id);
                            id
                        }
                    };
                    let complete = piece.ends_with('\n');
                    let plain = piece.strip_suffix('\n').unwrap_or(piece);
                    self.append(id, plain, style);
                    if complete {
                        // PowerShell writes CRLF, which may straddle reads.
                        // Store the newline as a row boundary, not a stray CR.
                        let i = self.index(id).unwrap();
                        let line = &mut self.lines[i];
                        if line.text.ends_with('\r') {
                            line.text.pop();
                            if let Some(span) = line.spans.last_mut() {
                                span.range.end = line.text.len();
                            }
                            if line.spans.last().is_some_and(|s| s.range.is_empty()) {
                                line.spans.pop();
                            }
                        }
                        stream.open = None;
                    }
                    self.trim();
                }
            }
        }
        self.revision += 1;
    }

    pub(crate) fn fail_since(&mut self, first: u64) {
        for line in &mut self.lines {
            if line.id >= first && line.kind == OutType::StdErrOk {
                line.kind = OutType::StdErr;
            }
        }
    }

    fn index(&self, id: u64) -> Option<usize> {
        let i = usize::try_from(id.checked_sub(self.lines.front()?.id)?).ok()?;
        (i < self.lines.len()).then_some(i)
    }

    fn new_line(&mut self, kind: OutType) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let line = Line {
            id,
            text: String::new(),
            kind,
            spans: Vec::new(),
            height: None,
        };
        self.bytes += line.bytes();
        self.lines.push_back(line);
        id
    }

    fn append(&mut self, id: u64, text: &str, style: ansi::Style) {
        let i = self
            .index(id)
            .expect("line created or checked before appending");
        let line = &mut self.lines[i];
        self.bytes -= line.bytes();
        let start = line.text.len();
        line.text.push_str(text);
        if !text.is_empty() {
            if let Some(span) = line.spans.last_mut().filter(|s| s.style == style) {
                span.range.end = line.text.len();
            } else {
                line.spans.push(Span {
                    range: start..line.text.len(),
                    style,
                });
            }
        }
        // A never-ending line must not turn into a giant layout or evade
        // the line-count cap. Keep its newest text on a UTF-8 boundary.
        if line.text.len() > MAX_LINE_BYTES {
            let cut = line
                .text
                .ceil_char_boundary(line.text.len() - MAX_LINE_BYTES);
            line.text.drain(..cut);
            line.text.shrink_to_fit();
            line.spans.retain_mut(|s| {
                let keep = s.range.end > cut;
                s.range = s.range.start.saturating_sub(cut)..s.range.end.saturating_sub(cut);
                keep
            });
            if line.spans.capacity() > line.spans.len() * 2 {
                line.spans.shrink_to_fit();
            }
            self.truncated = true;
            self.selection = None;
        }
        line.height = None;
        self.bytes += line.bytes();
    }

    fn trim(&mut self) {
        while self.bytes > MAX_BYTES || self.lines.len() > MAX_LINES {
            let line = self
                .lines
                .pop_front()
                .expect("nonempty buffer exceeds limit");
            self.bytes -= line.bytes();
            self.truncated = true;
            if self
                .selection
                .is_some_and(|s| s.anchor.line <= line.id || s.head.line <= line.id)
            {
                self.selection = None;
                self.dragging = false;
            }
        }
    }

    pub(crate) fn has_selection(&self) -> bool {
        self.selection.is_some_and(|s| s.anchor != s.head)
    }

    /// Copy from retained plain text, including selected lines outside the
    /// viewport. Soft wraps do not insert newlines into the clipboard.
    pub(crate) fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection?.sorted();
        if start == end {
            return None;
        }
        let first = self.index(start.line)?;
        let last = self.index(end.line)?;
        let mut text = String::new();
        for i in first..=last {
            let line = &self.lines[i];
            let from = if i == first {
                char_byte(&line.text, start.character)
            } else {
                0
            };
            let to = if i == last {
                char_byte(&line.text, end.character)
            } else {
                line.text.len()
            };
            if i != first {
                text.push('\n');
            }
            text.push_str(&line.text[from..to]);
        }
        Some(text)
    }
}

fn char_byte(text: &str, character: usize) -> usize {
    text.char_indices()
        .nth(character)
        .map_or(text.len(), |(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streamed_lines_preserve_styles_and_other_streams() {
        let mut out = Output::default();
        let mut stdout = Stream::default();
        let mut stderr = Stream::default();
        out.write("\x1b[3", OutType::StdOut, &mut stdout);
        out.write("1mhel", OutType::StdOut, &mut stdout);
        out.write("warning", OutType::StdErrOk, &mut stderr);
        out.write("lo\nnext\n\nlast", OutType::StdOut, &mut stdout);
        let texts: Vec<_> = out.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["hello", "warning", "next", "", "last"]);
        assert_eq!(out.lines[0].spans[0].style, out.lines[2].spans[0].style);
        assert_eq!(out.lines[0].spans.len(), 1);
        assert!(
            out.lines[0].spans[0]
                .style
                .format(Color32::WHITE, FontId::monospace(13.))
                .color
                != Color32::WHITE
        );
    }

    #[test]
    fn crlf_can_straddle_chunks_and_echoes_can_end_a_partial_line() {
        let mut out = Output::default();
        let mut stream = Stream::default();
        out.write("first\r", OutType::StdOut, &mut stream);
        out.write("\nsecond", OutType::StdOut, &mut stream);
        out.push("echo", OutType::Prompt);
        stream.break_line();
        out.write("third\r\n", OutType::StdOut, &mut stream);
        assert_eq!(
            out.lines
                .iter()
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>(),
            ["first", "second", "echo", "third"]
        );
        assert!(
            out.lines
                .iter()
                .all(|l| l.spans.iter().all(|s| s.range.end <= l.text.len()))
        );
    }

    #[test]
    fn byte_and_line_limits_keep_the_newest_output() {
        let mut out = Output::default();
        for i in 0..25_000 {
            out.push(&format!("{i}: {}", "x".repeat(100)), OutType::StdOut);
        }
        assert!(out.bytes <= MAX_BYTES);
        assert!(out.lines.len() <= MAX_LINES);
        assert!(out.truncated);
        assert!(out.lines.back().unwrap().text.starts_with("24999:"));
        let mut empty = Output::default();
        for _ in 0..MAX_LINES + 100 {
            empty.push("", OutType::StdOut);
        }
        assert_eq!(empty.lines.len(), MAX_LINES);
    }

    #[test]
    fn an_unfinished_unicode_line_is_bounded_and_can_continue_after_clear() {
        let mut out = Output::default();
        let mut stream = Stream::default();
        for _ in 0..100 {
            out.write(&"😀".repeat(4096), OutType::StdOut, &mut stream);
        }
        assert!(out.lines[0].text.len() <= MAX_LINE_BYTES);
        assert!(out.truncated);
        assert!(
            out.lines[0]
                .spans
                .iter()
                .all(|s| s.range.end <= out.lines[0].text.len())
        );
        let next_id = out.next_id();
        out.clear();
        assert_eq!(out.retained_bytes(), 0);
        out.write("after clear", OutType::StdOut, &mut stream);
        assert_eq!(out.lines[0].id, next_id);
        assert_eq!(out.lines[0].text, "after clear");
    }

    #[test]
    fn copies_unicode_and_offscreen_lines_without_ansi_or_soft_wraps() {
        let mut out = Output::default();
        out.push(
            "before\n\x1b[32m😀é first\x1b[0m\nsecond\nlast",
            OutType::StdOut,
        );
        out.selection = Some(Selection {
            anchor: Cursor {
                line: 1,
                character: 1,
            },
            head: Cursor {
                line: 3,
                character: 2,
            },
        });
        assert_eq!(out.selected_text().as_deref(), Some("é first\nsecond\nla"));
        let s = out.selection.unwrap();
        out.selection = Some(Selection {
            anchor: s.head,
            head: s.anchor,
        });
        assert_eq!(out.selected_text().as_deref(), Some("é first\nsecond\nla"));
    }

    #[test]
    fn stderr_recoloring_uses_stable_ids_after_eviction() {
        let mut out = Output::default();
        out.push("previous warning", OutType::StdErrOk);
        let command_start = out.next_id();
        out.push("new warning", OutType::StdErrOk);
        out.fail_since(command_start);
        assert!(out.lines[0].kind == OutType::StdErrOk);
        assert!(out.lines[1].kind == OutType::StdErr);
        for _ in 0..MAX_LINES {
            out.push("", OutType::StdOut);
        }
        out.push("retained warning", OutType::StdErrOk);
        out.fail_since(command_start);
        assert!(out.lines.back().unwrap().kind == OutType::StdErr);
    }
}
