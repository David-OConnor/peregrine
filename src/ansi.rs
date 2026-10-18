//! An ANSI escape-sequence parser for line-oriented output.
//!
//! Shells like PowerShell emit sequences such as `ESC[32;1m` to colour their
//! output. We parse those into styled segments so the GUI can render. Output
//! from a remote terminal also moves the cursor within a line to redraw it
//! (progress bars, spinners); those few controls come out as [Ctl]s for the
//! scrollback to apply. Anything else (cursor addressing, modes, titles) is
//! dropped: full-screen programs get a real terminal screen instead (see
//! `remote`).

use egui::{Color32, FontId, Stroke, TextFormat};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Style {
    fg: Option<Color32>,
    bg: Option<Color32>,
    bold: bool,
    italic: bool,
    underline: bool,
}

impl Style {
    const fn new() -> Self {
        Self {
            fg: None,
            bg: None,
            bold: false,
            italic: false,
            underline: false,
        }
    }

    pub(crate) fn format(self, default_fg: Color32, font: FontId) -> TextFormat {
        let color = self.fg.unwrap_or(default_fg);
        let color = if self.bold { brighten(color) } else { color };
        TextFormat {
            font_id: font,
            color,
            background: self.bg.unwrap_or(Color32::TRANSPARENT),
            italics: self.italic,
            underline: if self.underline {
                Stroke::new(1.0, color)
            } else {
                Stroke::NONE
            },
            ..Default::default()
        }
    }
}

impl Default for Style {
    fn default() -> Self {
        Self::new()
    }
}

/// A piece of parsed output: styled text, or a cursor control.
pub(crate) enum Piece {
    Text(Segment),
    Ctl(Ctl),
}

/// The in-line cursor controls the scrollback applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ctl {
    /// Carriage return: back to the start of the line.
    Cr,
    /// One column left.
    Backspace,
    /// `ESC [ n K`: erase to the end of the line (0), from its start to the
    /// cursor (1), or all of it (2).
    EraseLine(u8),
    /// `ESC [ n G`: to this column, from 0.
    Column(usize),
    /// `ESC [ n C` / `ESC [ n D`: this many columns right / left.
    Forward(usize),
    Back(usize),
}

pub struct Segment {
    pub text: String,
    pub fg: Option<Color32>,
    pub bg: Option<Color32>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

impl Segment {
    pub(crate) fn style(&self) -> Style {
        Style {
            fg: self.fg,
            bg: self.bg,
            bold: self.bold,
            italic: self.italic,
            underline: self.underline,
        }
    }
}

/// Standard 16-colour ANSI palette, tuned to read well on a black background.
pub(crate) fn ansi_color(code: u32, bright: bool) -> Option<Color32> {
    let (r, g, b) = match (code, bright) {
        (0, false) => (40, 40, 40),
        (1, false) => (200, 70, 70),
        (2, false) => (90, 200, 90),
        (3, false) => (200, 180, 70),
        (4, false) => (90, 130, 230),
        (5, false) => (200, 90, 200),
        (6, false) => (70, 200, 200),
        (7, false) => (200, 200, 200),
        (0, true) => (110, 110, 110),
        (1, true) => (255, 110, 110),
        (2, true) => (130, 255, 130),
        (3, true) => (255, 230, 110),
        (4, true) => (130, 170, 255),
        (5, true) => (255, 130, 255),
        (6, true) => (130, 255, 255),
        (7, true) => (255, 255, 255),
        _ => return None,
    };
    Some(Color32::from_rgb(r, g, b))
}

/// xterm 256-colour palette lookup.
pub(crate) fn xterm256(n: u8) -> Color32 {
    if n < 16 {
        let bright = n >= 8;
        let code = (n % 8) as u32;
        return ansi_color(code, bright).unwrap_or(Color32::WHITE);
    }
    if n >= 232 {
        let v = 8 + (n - 232).saturating_mul(10);
        return Color32::from_rgb(v, v, v);
    }
    let c = n - 16;
    let levels = [0u8, 95, 135, 175, 215, 255];
    let r = (c / 36) as usize;
    let g = ((c % 36) / 6) as usize;
    let b = (c % 6) as usize;
    Color32::from_rgb(levels[r], levels[g], levels[b])
}

fn apply_sgr(style: &mut Style, params: &[u32]) {
    let mut i = 0;
    while i < params.len() {
        let p = params[i];
        match p {
            0 => *style = Style::new(),
            1 => style.bold = true,
            3 => style.italic = true,
            4 => style.underline = true,
            22 => style.bold = false,
            23 => style.italic = false,
            24 => style.underline = false,
            30..=37 => style.fg = ansi_color(p - 30, false),
            90..=97 => style.fg = ansi_color(p - 90, true),
            40..=47 => style.bg = ansi_color(p - 40, false),
            100..=107 => style.bg = ansi_color(p - 100, true),
            39 => style.fg = None,
            49 => style.bg = None,
            38 => {
                if i + 2 < params.len() && params[i + 1] == 5 {
                    style.fg = Some(xterm256(params[i + 2] as u8));
                    i += 2;
                } else if i + 4 < params.len() && params[i + 1] == 2 {
                    style.fg = Some(Color32::from_rgb(
                        params[i + 2] as u8,
                        params[i + 3] as u8,
                        params[i + 4] as u8,
                    ));
                    i += 4;
                }
            }
            48 => {
                if i + 2 < params.len() && params[i + 1] == 5 {
                    style.bg = Some(xterm256(params[i + 2] as u8));
                    i += 2;
                } else if i + 4 < params.len() && params[i + 1] == 2 {
                    style.bg = Some(Color32::from_rgb(
                        params[i + 2] as u8,
                        params[i + 3] as u8,
                        params[i + 4] as u8,
                    ));
                    i += 4;
                }
            }
            _ => {}
        }
        i += 1;
    }
}

#[derive(Default)]
enum Escape {
    #[default]
    Text,
    Start,
    /// After `ESC` and an intermediate byte, as in `ESC ( B` (select a
    /// character set): the next byte ends it.
    Intermediate,
    Csi,
    /// A string up to BEL or `ESC \`: OSC (`ESC ]`), and likewise DCS, SOS,
    /// PM and APC.
    Osc,
    OscEnd,
}

/// Incremental parser: a read can end halfway through an escape sequence.
/// ANSI is consumed once on arrival, rather than on every output repaint.
#[derive(Default)]
pub(crate) struct Parser {
    style: Style,
    escape: Escape,
    params: String,
    params_overflow: bool,
}

impl Parser {
    pub(crate) fn feed(&mut self, input: &str) -> Vec<Piece> {
        let mut pieces = Vec::new();
        let mut text = String::new();
        let flush = |pieces: &mut Vec<Piece>, text: &mut String, style: &Style| {
            if !text.is_empty() {
                pieces.push(Piece::Text(make_segment(std::mem::take(text), style)));
            }
        };
        for c in input.chars() {
            match self.escape {
                Escape::Text => match c {
                    '\x1b' => self.escape = Escape::Start,
                    '\r' => {
                        flush(&mut pieces, &mut text, &self.style);
                        pieces.push(Piece::Ctl(Ctl::Cr));
                    }
                    '\x08' => {
                        flush(&mut pieces, &mut text, &self.style);
                        pieces.push(Piece::Ctl(Ctl::Backspace));
                    }
                    // Other C0/C1 controls (BEL, shift-in/out, …) don't print.
                    '\t' | '\n' => text.push(c),
                    c if c.is_control() => {}
                    _ => text.push(c),
                },
                Escape::Start => match c {
                    '[' => {
                        self.params.clear();
                        self.params_overflow = false;
                        self.escape = Escape::Csi;
                    }
                    ']' | 'P' | 'X' | '^' | '_' => self.escape = Escape::Osc,
                    ' '..='/' => self.escape = Escape::Intermediate,
                    _ => self.escape = Escape::Text,
                },
                Escape::Intermediate => {
                    if !(' '..='/').contains(&c) {
                        self.escape = Escape::Text;
                    }
                }
                Escape::Csi => {
                    if ('@'..='~').contains(&c) {
                        // Private sequences (`ESC [ ? 25 l`) set modes.
                        let plain = !self.params_overflow
                            && self.params.bytes().all(|b| b.is_ascii_digit() || b == b';');
                        let n = || -> Option<usize> { self.params.split(";").next()?.parse().ok() };
                        let ctl = match c {
                            'm' if plain => {
                                flush(&mut pieces, &mut text, &self.style);
                                let params: Vec<u32> = self
                                    .params
                                    .split(';')
                                    .map(|p| p.parse().unwrap_or(0))
                                    .collect();
                                apply_sgr(&mut self.style, &params);
                                None
                            }
                            'K' if plain => Some(Ctl::EraseLine(n().unwrap_or(0).min(3) as u8)),
                            'G' if plain => Some(Ctl::Column(n().unwrap_or(1).max(1) - 1)),
                            'C' if plain => Some(Ctl::Forward(n().unwrap_or(1).max(1))),
                            'D' if plain => Some(Ctl::Back(n().unwrap_or(1).max(1))),
                            _ => None,
                        };
                        if let Some(ctl) = ctl {
                            flush(&mut pieces, &mut text, &self.style);
                            pieces.push(Piece::Ctl(ctl));
                        }
                        self.escape = Escape::Text;
                    } else if self.params.len() < 128 {
                        self.params.push(c);
                    } else {
                        // Malformed controls must not become an unbounded buffer.
                        self.params_overflow = true;
                    }
                }
                Escape::Osc => match c {
                    '\x07' => self.escape = Escape::Text,
                    '\x1b' => self.escape = Escape::OscEnd,
                    _ => {}
                },
                Escape::OscEnd => {
                    self.escape = if c == '\\' || c == '\x07' {
                        Escape::Text
                    } else {
                        Escape::Osc
                    };
                }
            }
        }
        flush(&mut pieces, &mut text, &self.style);
        pieces
    }
}

fn brighten(c: Color32) -> Color32 {
    let lift = |v: u8| ((v as u16 + 255) / 2).min(255) as u8;
    Color32::from_rgb(lift(c.r()), lift(c.g()), lift(c.b()))
}

fn make_segment(text: String, style: &Style) -> Segment {
    Segment {
        text,
        fg: style.fg,
        bg: style.bg,
        bold: style.bold,
        italic: style.italic,
        underline: style.underline,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_sequences_survive_every_read_boundary() {
        let text = "plain\x1b[31;1mred é\x1b[0m normal\x1b]title\x1b\\ end";
        let expand = |pieces: Vec<Piece>| -> Vec<(char, Style)> {
            pieces
                .into_iter()
                .flat_map(|p| match p {
                    Piece::Text(s) => {
                        let style = s.style();
                        s.text.chars().map(|c| (c, style)).collect::<Vec<_>>()
                    }
                    Piece::Ctl(_) => Vec::new(),
                })
                .collect()
        };
        let expected = expand(Parser::default().feed(text));
        for (i, _) in text.char_indices() {
            let mut parser = Parser::default();
            let mut actual = expand(parser.feed(&text[..i]));
            actual.extend(expand(parser.feed(&text[i..])));
            assert_eq!(actual, expected, "split at byte {i}");
        }
        assert_eq!(
            expected.iter().map(|(c, _)| c).collect::<String>(),
            "plainred é normal end"
        );
    }

    #[test]
    fn malformed_controls_do_not_accumulate_unbounded_text() {
        let mut parser = Parser::default();
        assert!(
            parser
                .feed(&format!("\x1b[{}", "1".repeat(100_000)))
                .is_empty()
        );
        assert!(parser.params.len() <= 128);
        assert_eq!(text(parser.feed("mvisible")), "visible");
        assert!(
            parser
                .feed(&format!("\x1b]{}", "x".repeat(100_000)))
                .is_empty()
        );
        assert_eq!(text(parser.feed("\x07visible")), "visible");
    }

    fn text(pieces: Vec<Piece>) -> String {
        pieces
            .into_iter()
            .filter_map(|p| match p {
                Piece::Text(s) => Some(s.text),
                Piece::Ctl(_) => None,
            })
            .collect()
    }

    #[test]
    fn line_controls_come_out_and_other_sequences_are_dropped() {
        let pieces = Parser::default()
            .feed("a\rb\x08\x1b[2K\x1b[5G\x1b(Bc\x1b[?25l\x07\x1bP1$r\x1b\\d");
        let got: Vec<String> = pieces
            .into_iter()
            .map(|p| match p {
                Piece::Text(s) => s.text,
                Piece::Ctl(c) => format!("{c:?}"),
            })
            .collect();
        assert_eq!(
            got,
            ["a", "Cr", "b", "Backspace", "EraseLine(2)", "Column(4)", "cd"]
        );
    }
}
