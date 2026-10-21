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

use crate::{
    OutType,
    ansi::{self, Ctl, Piece},
};

const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_LINES: usize = 20_000;
pub const MAX_LINE_BYTES: usize = 16 * 1024;
pub const CLOSED_BYTES: usize = 8 * 1024 * 1024;
const PARSE_CHUNK_BYTES: usize = 4096;
/// The furthest right a cursor control can move the column, so a stray
/// `ESC [ 99999 C` can't pad a line out to megabytes.
const MAX_COLUMN: usize = 4096;

#[derive(Default)]
pub struct Stream {
    parser: ansi::Parser,
    open: Option<u64>,
    /// Where in the open line the next text goes, in chars, after a carriage
    /// return or other cursor control moved it (see [ansi::Ctl]); `None` at
    /// the end of the line, the usual case.
    col: Option<usize>,
}

impl Stream {
    pub fn break_line(&mut self) {
        self.open = None;
        self.col = None;
    }
}

struct Span {
    range: Range<usize>,
    style: ansi::Style,
}

pub struct Line {
    pub id: u64,
    pub text: String,
    pub kind: OutType,
    spans: Vec<Span>,
    pub height: Option<f32>,
}

impl Line {
    fn bytes(&self) -> usize {
        size_of::<Self>() + self.text.capacity() + self.spans.capacity() * size_of::<Span>()
    }

    pub fn job(&self, font: &FontId, color: Color32, width: f32) -> LayoutJob {
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
pub struct Cursor {
    pub line: u64,
    pub character: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct Selection {
    pub anchor: Cursor,
    pub head: Cursor,
}

impl Selection {
    pub fn sorted(self) -> (Cursor, Cursor) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

#[derive(PartialEq)]
pub struct Metrics {
    pub font: FontId,
    pub width: f32,
    pub pixels_per_point: f32,
    pub spacing: f32,
}

pub struct Output {
    pub lines: VecDeque<Line>,
    pub offsets: Vec<f32>,
    pub metrics: Option<Metrics>,
    pub first_measured: Option<u64>,
    pub selection: Option<Selection>,
    pub dragging: bool,
    pub truncated: bool,
    pub id: u64,
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
    pub fn next_id(&self) -> u64 {
        self.next_id
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn retained_bytes(&self) -> usize {
        self.bytes
            + (self.lines.capacity() - self.lines.len()) * size_of::<Line>()
            + self.offsets.capacity() * size_of::<f32>()
    }

    pub fn clear(&mut self) {
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
    pub fn push(&mut self, text: &str, kind: OutType) {
        let mut stream = Stream::default();
        if text.is_empty() {
            self.new_line(kind);
            self.trim();
            self.revision += 1;
        } else {
            self.write(text, kind, &mut stream);
        }
    }

    /// Add a finished line of `text`, styled over its byte ranges by
    /// `styles`, which cover all of it. Returns its ID.
    pub fn push_styled(
        &mut self,
        text: &str,
        styles: &[(Range<usize>, ansi::Style)],
        kind: OutType,
    ) -> u64 {
        let id = self.new_line(kind);
        for (range, style) in styles {
            self.append(id, &text[range.clone()], *style);
        }
        self.trim();
        self.revision += 1;
        id
    }

    /// Remove the lines from `id` on, at the end.
    pub fn truncate_from(&mut self, id: u64) {
        while let Some(line) = self.lines.pop_back_if(|line| line.id >= id) {
            self.bytes -= line.bytes();
            // Its ID goes to the next new line: finding a line by its ID
            // relies on them being consecutive.
            self.next_id = line.id;
        }
        if self
            .selection
            .is_some_and(|s| s.anchor.line >= id || s.head.line >= id)
        {
            self.selection = None;
            self.dragging = false;
        }
        self.revision += 1;
    }

    /// Continue an unfinished line in this stream, including when the other
    /// stream has written in between. An echo/clear ends the continuation.
    /// A carriage return (as in CRLF, or a progress bar redrawing itself)
    /// and the other in-line cursor controls move where the stream writes
    /// next within its line, as a terminal would.
    pub fn write(&mut self, mut text: &str, kind: OutType, stream: &mut Stream) {
        while !text.is_empty() {
            let end = text.floor_char_boundary(PARSE_CHUNK_BYTES.min(text.len()));
            let (chunk, rest) = text.split_at(end);
            text = rest;
            for piece in stream.parser.feed(chunk) {
                let segment = match piece {
                    Piece::Text(segment) => segment,
                    Piece::Ctl(ctl) => {
                        self.control(ctl, stream);
                        continue;
                    }
                };
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
                    self.put(id, plain, style, &mut stream.col);
                    if complete {
                        stream.open = None;
                        stream.col = None;
                    }
                    self.trim();
                }
            }
        }
        self.revision += 1;
    }

    /// Apply a cursor control to `stream`'s open line.
    fn control(&mut self, ctl: Ctl, stream: &mut Stream) {
        let open = stream.open.and_then(|id| self.index(id));
        let len = open.map_or(0, |i| self.lines[i].text.chars().count());
        let cur = stream.col.unwrap_or(len);
        stream.col = Some(match ctl {
            Ctl::Cr => 0,
            Ctl::Backspace => cur.saturating_sub(1),
            Ctl::Column(n) => n.min(MAX_COLUMN),
            Ctl::Forward(n) => (cur + n).min(MAX_COLUMN),
            Ctl::Back(n) => cur.saturating_sub(n),
            Ctl::EraseLine(mode) => {
                if let Some(i) = open {
                    self.edit(i, |cells| match mode {
                        0 => cells.truncate(cur),
                        1 => {
                            let end = (cur + 1).min(cells.len());
                            cells[..end].fill((' ', ansi::Style::default()));
                        }
                        _ => cells.clear(),
                    });
                }
                cur
            }
        });
    }

    /// Write `text` into line `id` at `col` (its end when `None`), over
    /// whatever's there, and move `col` past it.
    fn put(&mut self, id: u64, text: &str, style: ansi::Style, col: &mut Option<usize>) {
        if text.is_empty() {
            return;
        }
        let Some(c) = *col else {
            self.append(id, text, style);
            return;
        };
        let i = self
            .index(id)
            .expect("line created or checked before writing");
        let len = self.lines[i].text.chars().count();
        if c >= len {
            if c > len {
                self.append(id, &" ".repeat(c - len), ansi::Style::default());
            }
            self.append(id, text, style);
            *col = None;
            return;
        }
        let new: Vec<_> = text.chars().map(|ch| (ch, style)).collect();
        let n = new.len();
        self.edit(i, |cells| {
            let end = (c + n).min(cells.len());
            cells.splice(c..end, new);
        });
        *col = (c + n < self.lines[i].text.chars().count()).then_some(c + n);
    }

    /// Rewrite line `i` through its characters and their styles. Slower than
    /// [Self::append], so only for redraws within a line.
    fn edit(&mut self, i: usize, f: impl FnOnce(&mut Vec<(char, ansi::Style)>)) {
        let line = &mut self.lines[i];
        self.bytes -= line.bytes();
        let mut spans = line.spans.iter().peekable();
        let mut cells: Vec<_> = line
            .text
            .char_indices()
            .map(|(at, ch)| {
                while spans.peek().is_some_and(|s| s.range.end <= at) {
                    spans.next();
                }
                let style = spans
                    .peek()
                    .filter(|s| s.range.start <= at)
                    .map_or_else(ansi::Style::default, |s| s.style);
                (ch, style)
            })
            .collect();
        f(&mut cells);
        line.text.clear();
        line.spans.clear();
        for (ch, style) in cells {
            let start = line.text.len();
            line.text.push(ch);
            match line.spans.last_mut() {
                Some(span) if span.style == style => span.range.end = line.text.len(),
                _ => line.spans.push(Span {
                    range: start..line.text.len(),
                    style,
                }),
            }
        }
        line.height = None;
        self.bytes += line.bytes();
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

    pub fn has_selection(&self) -> bool {
        self.selection.is_some_and(|s| s.anchor != s.head)
    }

    /// Copy from retained plain text, including selected lines outside the
    /// viewport. Soft wraps do not insert newlines into the clipboard.
    pub fn selected_text(&self) -> Option<String> {
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
