//! The terminal a local command runs in (see [crate::pty]), emulated. What
//! the command writes is drawn onto a screen ([vt100]), as a terminal window
//! would draw it, and the tab's scrollback shows that screen's text: rows
//! that scroll off its top join the scrollback for good, and the lines still
//! on it are rewritten as they change. So a program redrawing what's already
//! on the screen (a progress display moving the cursor up, a REPL redrawing
//! its input line, Windows' pseudo console repainting the screen after a
//! full-screen program exits) changes those lines rather than adding copies.
//! While a full-screen program has the terminal, the pane shows the screen
//! itself instead (see `ui::grid`).

use std::ops::Range;

use egui::Color32;

use crate::{OutType, ansi, terminal::Output, ui::terminal::COLOR_STDOUT};

/// Rows the emulator holds once they've scrolled off its screen, until
/// they're taken into the scrollback. Output is fed to it at most
/// [FEED_BYTES] at a time, each byte scrolling at most a row, and the rows
/// taken are dropped once there are [KEEP_ROWS] of them, so this many are
/// never reached (but by escape sequences scrolling many rows at once).
const SCROLLBACK_ROWS: usize = KEEP_ROWS + FEED_BYTES + 1024;
const FEED_BYTES: usize = 2048;
/// Rows taken into the scrollback that the emulator may go on holding: it
/// can't drop them, so past this many it's rebuilt without them.
const KEEP_ROWS: usize = 1024;

/// Answers a program's questions about the terminal, as a terminal would:
/// where the cursor is, and what kind of terminal this is. Some programs
/// wait for the answer.
#[derive(Default)]
pub struct Replies(Vec<u8>);

impl Replies {
    /// The answers not yet sent.
    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.0)
    }
}

impl vt100::Callbacks for Replies {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        _i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let first = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        match (i1, c, first) {
            // Cursor position report.
            (None, 'n', 6) => {
                let (row, col) = screen.cursor_position();
                self.0
                    .extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
            }
            // Device status: OK.
            (None, 'n', 5) => self.0.extend_from_slice(b"\x1b[0n"),
            // Primary and secondary device attributes, as a VT220.
            (None, 'c', 0) => self.0.extend_from_slice(b"\x1b[?62;22c"),
            (Some(b'>'), 'c', 0) => self.0.extend_from_slice(b"\x1b[>1;10;0c"),
            _ => {}
        }
    }
}

/// A line of styled text, built up cell by cell.
#[derive(Default)]
struct Styled {
    text: String,
    styles: Vec<(Range<usize>, ansi::Style)>,
}

impl Styled {
    fn push(&mut self, s: &str, style: ansi::Style) {
        let start = self.text.len();
        self.text.push_str(s);
        match self.styles.last_mut() {
            Some((range, last)) if *last == style => range.end = self.text.len(),
            _ => self.styles.push((start..self.text.len(), style)),
        }
    }

    /// Drop the blank cells at the end, where nothing was written.
    fn trim_end(&mut self) {
        while let Some((range, style)) = self.styles.last_mut() {
            if style.has_background() {
                break;
            }
            let end = range.start + self.text[range.clone()].trim_end_matches(' ').len();
            self.text.truncate(end);
            range.end = end;
            if range.end > range.start {
                break;
            }
            self.styles.pop();
        }
    }

    /// Add the line to `out`, leaving this one empty.
    fn push_to(&mut self, out: &mut Output) -> u64 {
        self.trim_end();
        let id = out.push_styled(&self.text, &self.styles, OutType::StdOut);
        *self = Self::default();
        id
    }
}

pub struct Term {
    /// The screen, and the rows that have scrolled off its top.
    parser: vt100::Parser<Replies>,
    /// How many of the parser's scrolled-off rows are in the scrollback.
    taken: usize,
    /// The start of a line that has scrolled off the top, while its end,
    /// wrapped onto the next row, is still on the screen.
    carry: Styled,
    /// The first screen row the scrollback's live lines show. The rows above
    /// it are in the scrollback for good (see [Self::freeze]).
    live_from: u16,
    /// The first of the scrollback's lines that show the screen's rows, at
    /// its end, rewritten as they change; `None` while there are none.
    live: Option<u64>,
    /// The scrollback's revision when this last changed it, to tell when
    /// something else has.
    revision: u64,
    /// Set when the screen has changed since the live lines were written.
    dirty: bool,
    /// Set as a full-screen program takes over, for the UI to give the
    /// screen keyboard focus; cleared by the UI.
    pub grid_started: bool,
}

impl Term {
    pub fn new(cols: u16, rows: u16, out: &Output) -> Self {
        Self {
            parser: vt100::Parser::new_with_callbacks(
                rows.max(1),
                cols.max(1),
                SCROLLBACK_ROWS,
                Replies::default(),
            ),
            taken: 0,
            carry: Styled::default(),
            live_from: 0,
            live: None,
            revision: out.revision(),
            dirty: false,
            grid_started: false,
        }
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// Whether a full-screen program has the terminal: it switched to the
    /// alternate screen (vim, nano, edit), or to application cursor keys (as
    /// `less` does even when it stays on the main screen, as git's pager
    /// does). The pane then shows the screen, and sends keys straight to it.
    pub fn full_screen(&self) -> bool {
        let screen = self.parser.screen();
        screen.alternate_screen() || screen.application_cursor()
    }

    /// Take in what the program wrote. Rows it scrolls off the screen go
    /// into `out` right away; [Self::render] updates the rest. Returns the
    /// terminal's answers to the program's questions, to send back to it.
    pub fn feed(&mut self, bytes: &[u8], out: &mut Output) -> Vec<u8> {
        self.check(out);
        let was_full = self.full_screen();
        for part in split_before_alternate_screen(bytes) {
            // The main screen can't be read while the alternate one is up.
            if !self.parser.screen().alternate_screen() && sets_alternate_screen(part) {
                self.render(out);
                self.freeze(out);
            }
            for chunk in part.chunks(FEED_BYTES) {
                self.parser.process(chunk);
                self.dirty = true;
                self.take_scrolled(out);
            }
        }
        if !was_full && self.full_screen() {
            self.grid_started = true;
            // As for the alternate screen, before the screen is resized.
            self.render(out);
            self.freeze(out);
        }
        self.parser.callbacks_mut().take()
    }

    /// Change the screen's size. Its rows aren't rewrapped, so this cuts off
    /// the ends of lines it narrows: only for full-screen programs, which
    /// redraw to fit, and only once [Self::freeze] has kept the lines
    /// before them.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 || self.parser.screen().size() == (rows, cols) {
            return;
        }
        self.parser.screen_mut().set_size(rows, cols);
        self.live_from = self.live_from.min(rows - 1);
    }

    /// Bring the live lines in `out` up to date with the screen.
    pub fn render(&mut self, out: &mut Output) {
        self.check(out);
        let screen = self.parser.screen();
        if !self.dirty || screen.alternate_screen() {
            return;
        }
        self.dirty = false;

        let (rows, _) = screen.size();
        let (cursor_row, cursor_col) = screen.cursor_position();
        // To the last row with anything on it, or the cursor's, if anything's
        // been written before it on its row. Not the blank rows below.
        let mut last = (self.live_from..rows)
            .rev()
            .find(|&row| row_has_content(screen, row));
        if cursor_col > 0 && cursor_row >= self.live_from {
            last = last.max(Some(cursor_row));
        }

        if let Some(id) = self.live.take() {
            out.truncate_from(id);
        }
        let mut line = Styled {
            text: self.carry.text.clone(),
            styles: self.carry.styles.clone(),
        };
        if let Some(last) = last {
            for row in self.live_from..=last {
                push_row(screen, row, &mut line);
                if row == last || !screen.row_wrapped(row) {
                    let id = line.push_to(out);
                    self.live.get_or_insert(id);
                }
            }
        } else if !line.text.is_empty() {
            self.live = Some(line.push_to(out));
        }
        self.revision = out.revision();
    }

    /// The command has ended: show the screen's final lines. A program that
    /// ended without handing the screen back (killed in full screen) leaves
    /// its modes set, so they're reset first.
    pub fn finish(&mut self, out: &mut Output) {
        if self.full_screen() {
            self.parser.process(b"\x1b[?1049l\x1b[?1l\x1b>");
            self.dirty = true;
        }
        self.render(out);
    }

    /// When something other than this has changed `out` (emptied it, or
    /// added a line after the live lines), the live lines are no longer
    /// this one's to rewrite. The rows above the cursor's line stay out of
    /// the scrollback from then on, as they would stay above the cursor in a
    /// terminal: after a `clear`, they're gone.
    fn check(&mut self, out: &Output) {
        if out.revision() == self.revision {
            return;
        }
        self.revision = out.revision();
        self.live = None;
        self.carry = Styled::default();
        self.dirty = true;
        let screen = self.parser.screen();
        if !screen.alternate_screen() {
            self.live_from = self
                .live_from
                .max(cursor_line_start(screen, self.live_from));
        }
    }

    /// Keep the screen's rows above the cursor's line in the scrollback for
    /// good: a full-screen program is taking over, and when it hands the
    /// screen back they may not be as they were (resizing cuts them off).
    /// Expects the live lines to be up to date.
    fn freeze(&mut self, out: &mut Output) {
        let screen = self.parser.screen();
        if screen.alternate_screen() {
            return;
        }
        let start = cursor_line_start(screen, self.live_from);
        if start <= self.live_from {
            return;
        }
        // The live lines up to the cursor's become the scrollback's own.
        let lines = (self.live_from..start)
            .filter(|&row| !screen.row_wrapped(row))
            .count() as u64;
        self.live = self
            .live
            .map(|id| id + lines)
            .filter(|&id| out.lines.back().is_some_and(|line| line.id >= id));
        self.live_from = start;
        self.carry = Styled::default();
    }

    /// Move the rows scrolled off the top of the screen into `out`, before
    /// the live lines (which [Self::render] rewrites after them).
    fn take_scrolled(&mut self, out: &mut Output) {
        let screen = self.parser.screen_mut();
        screen.set_scrollback(usize::MAX);
        let len = screen.scrollback();
        let new = len.saturating_sub(self.taken);
        // Rows from above `live_from` are in the scrollback already.
        let frozen = new.min(usize::from(self.live_from));
        self.live_from -= frozen as u16;

        if new > frozen {
            if let Some(id) = self.live.take() {
                out.truncate_from(id);
            }
            let (rows, _) = screen.size();
            let mut next = self.taken + frozen;
            while next < len {
                // Scrolled back so `next` is the top row.
                screen.set_scrollback(len - next);
                let n = (len - next).min(usize::from(rows));
                for row in 0..n as u16 {
                    push_row(screen, row, &mut self.carry);
                    if !screen.row_wrapped(row) {
                        self.carry.push_to(out);
                    }
                }
                next += n;
            }
            self.revision = out.revision();
            self.dirty = true;
        }
        screen.set_scrollback(0);
        self.taken = len;

        if self.taken > KEEP_ROWS && !screen.alternate_screen() {
            self.rebuild();
        }
    }

    /// Replace the emulator with one in the same state, but without the
    /// scrolled-off rows (which [vt100] can't drop). Loses what the screen's
    /// contents and modes don't capture: a cursor position the program
    /// saved, a scrolling region; neither likely while output scrolls.
    fn rebuild(&mut self) {
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        let mut state = screen.state_formatted();
        state.extend(screen.attributes_formatted());
        let replies = self.parser.callbacks_mut().take();
        let mut parser =
            vt100::Parser::new_with_callbacks(rows, cols, SCROLLBACK_ROWS, Replies(replies));
        parser.process(&state);
        let screen = parser.screen_mut();
        screen.set_scrollback(usize::MAX);
        self.taken = screen.scrollback();
        screen.set_scrollback(0);
        self.parser = parser;
    }
}

/// The first row of the line the cursor is on, which may have wrapped onto
/// it from rows above; not above `top`.
fn cursor_line_start(screen: &vt100::Screen, top: u16) -> u16 {
    let (mut row, _) = screen.cursor_position();
    while row > top && screen.row_wrapped(row - 1) {
        row -= 1;
    }
    row
}

fn row_has_content(screen: &vt100::Screen, row: u16) -> bool {
    let (_, cols) = screen.size();
    (0..cols).any(|col| {
        screen.cell(row, col).is_some_and(|cell| {
            !cell.contents().trim().is_empty() || cell.bgcolor() != vt100::Color::Default
        })
    })
}

/// Add `row`'s cells to `line`.
fn push_row(screen: &vt100::Screen, row: u16, line: &mut Styled) {
    let (_, cols) = screen.size();
    for col in 0..cols {
        let Some(cell) = screen.cell(row, col) else {
            break;
        };
        if cell.is_wide_continuation() {
            continue;
        }
        let text = if cell.has_contents() {
            cell.contents()
        } else {
            " "
        };
        line.push(text, style_of(cell));
    }
}

/// How the scrollback shows a cell, as the screen does (see `ui::grid`).
fn style_of(cell: &vt100::Cell) -> ansi::Style {
    let color = |c: vt100::Color| match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(n) => Some(ansi::xterm256(n)),
        vt100::Color::Rgb(r, g, b) => Some(Color32::from_rgb(r, g, b)),
    };
    let (mut fg, mut bg) = (color(cell.fgcolor()), color(cell.bgcolor()));
    if cell.dim() {
        fg = Some(fg.unwrap_or(COLOR_STDOUT).gamma_multiply(0.6));
    }
    if cell.inverse() {
        (fg, bg) = (
            Some(bg.unwrap_or(Color32::BLACK)),
            Some(fg.unwrap_or(COLOR_STDOUT)),
        );
    }
    ansi::Style::with(fg, bg, cell.bold(), cell.italic(), cell.underline())
}

/// Split `bytes` before each sequence that switches to the alternate screen
/// (`ESC [ ? 1049 h`, or 1047 or 47), so the main screen can be read just
/// before it's put away.
fn split_before_alternate_screen(bytes: &[u8]) -> Vec<&[u8]> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while let Some(off) = bytes[i..].windows(3).position(|w| w == b"\x1b[?") {
        let at = i + off;
        i = at + 3;
        if sets_alternate_screen(&bytes[at..]) && at > start {
            parts.push(&bytes[start..at]);
            start = at;
        }
    }
    parts.push(&bytes[start..]);
    parts
}

/// Whether `bytes` starts with a sequence switching to the alternate screen.
fn sets_alternate_screen(bytes: &[u8]) -> bool {
    let Some(params) = bytes.strip_prefix(b"\x1b[?") else {
        return false;
    };
    let Some(len) = params
        .iter()
        .position(|b| !(b.is_ascii_digit() || *b == b';'))
    else {
        return false;
    };
    params[len] == b'h'
        && params[..len]
            .split(|b| *b == b';')
            .any(|p| matches!(p, b"1049" | b"1047" | b"47"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(out: &Output) -> Vec<String> {
        out.lines.iter().map(|l| l.text.clone()).collect()
    }

    /// Feed `chunks` one at a time, rendering after each, as polling does.
    fn run(cols: u16, rows: u16, chunks: &[&[u8]]) -> Vec<String> {
        let mut out = Output::default();
        out.push("$ cmd", OutType::Prompt);
        let mut term = Term::new(cols, rows, &out);
        for chunk in chunks {
            term.feed(chunk, &mut out);
            term.render(&mut out);
        }
        term.finish(&mut out);
        lines(&out)
    }

    #[test]
    fn lines_and_scrolling() {
        let text: String = (1..=12).map(|i| format!("line {i}\r\n")).collect();
        let expected: Vec<String> = std::iter::once("$ cmd".to_string())
            .chain((1..=12).map(|i| format!("line {i}")))
            .collect();
        assert_eq!(run(20, 5, &[text.as_bytes()]), expected);
        // The same, a byte at a time.
        let bytes: Vec<&[u8]> = text.as_bytes().chunks(1).collect();
        assert_eq!(run(20, 5, &bytes), expected);
    }

    #[test]
    fn a_long_line_wrapped_on_screen_is_one_line() {
        let long = "x".repeat(25);
        let text = format!("{long}\r\nshort\r\n");
        assert_eq!(run(10, 3, &[text.as_bytes()]), ["$ cmd", &long, "short"]);
        assert_eq!(run(10, 10, &[text.as_bytes()]), ["$ cmd", &long, "short"]);
    }

    #[test]
    fn a_repaint_rewrites_lines_rather_than_adding_them() {
        assert_eq!(
            run(
                20,
                5,
                &[
                    b"one\r\ntwo\r\n",
                    // Windows' pseudo console repainting after a resize.
                    b"\x1b[?25l\x1b[Hone\x1b[K\r\ntwo\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[3;1H\x1b[?25h",
                    b"three\r\n",
                ]
            ),
            ["$ cmd", "one", "two", "three"]
        );
    }

    #[test]
    fn progress_redrawn_in_place() {
        assert_eq!(
            run(
                20,
                5,
                &[
                    b"10%\r",
                    b"50%\r",
                    b"done\r\n",
                    b"a\r\nb\r\n\x1b[2A\x1b[2K",
                    b"A\r\n"
                ]
            ),
            ["$ cmd", "done", "A", "b"]
        );
    }

    #[test]
    fn a_full_screen_program_leaves_the_lines_before_it() {
        let out = run(
            20,
            5,
            &[
                b"before\r\n",
                b"\x1b[?1049h\x1b[H\x1b[2Jvim screen\x1b[?1h",
                b"\x1b[?1l\x1b[?1049l",
                // The pseudo console repainting the main screen.
                b"\x1b[Hbefore\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[2;1H",
                b"after\r\n",
            ],
        );
        assert_eq!(out, ["$ cmd", "before", "after"]);

        // The lines before it and its taking over in one read.
        let out = run(
            20,
            5,
            &[
                b"before\r\n\x1b[?1049h\x1b[H\x1b[2Jvim screen",
                b"\x1b[?1049l",
                b"after\r\n",
            ],
        );
        assert_eq!(out, ["$ cmd", "before", "after"]);
    }

    #[test]
    fn many_lines_survive_the_emulator_being_rebuilt() {
        let text: String = (0..5000).map(|i| format!("{i}\r\n")).collect();
        let out = run(20, 5, &[text.as_bytes()]);
        assert_eq!(out.len(), 5001);
        assert!(
            out[1..]
                .iter()
                .enumerate()
                .all(|(i, l)| *l == i.to_string())
        );
    }

    #[test]
    fn colors_carry_over() {
        let mut out = Output::default();
        let mut term = Term::new(20, 5, &out);
        term.feed(b"\x1b[31mred\x1b[m plain\r\n", &mut out);
        term.render(&mut out);
        assert_eq!(lines(&out), ["red plain"]);
        let job = out.lines[0].job(&egui::FontId::monospace(12.), COLOR_STDOUT, 100.);
        assert_eq!(job.sections.len(), 2);
        assert_ne!(job.sections[0].format.color, job.sections[1].format.color);
    }

    #[test]
    fn clearing_the_scrollback_drops_what_was_on_the_screen() {
        let mut out = Output::default();
        let mut term = Term::new(20, 5, &out);
        term.feed(b"old\r\n", &mut out);
        term.render(&mut out);
        out.clear();
        term.feed(b"new\r\n", &mut out);
        term.render(&mut out);
        assert_eq!(lines(&out), ["new"]);
    }

    #[test]
    fn alternate_screen_sequences() {
        assert!(sets_alternate_screen(b"\x1b[?1049h"));
        assert!(sets_alternate_screen(b"\x1b[?1;1049hrest"));
        assert!(!sets_alternate_screen(b"\x1b[?1049l"));
        assert!(!sets_alternate_screen(b"\x1b[?25h"));
        assert_eq!(
            split_before_alternate_screen(b"ab\x1b[?25lcd\x1b[?1049hef"),
            [&b"ab\x1b[?25lcd"[..], b"\x1b[?1049hef"]
        );
    }
}
