//! An ANSI SGR escape-sequence parser.
//!
//! Shells like PowerShell emit sequences such as `ESC[32;1m` to colour their
//! output. We parse those into styled segments so the GUI can render.

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
fn ansi_color(code: u32, bright: bool) -> Option<Color32> {
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
fn xterm256(n: u8) -> Color32 {
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
    Csi,
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
    pub(crate) fn feed(&mut self, input: &str) -> Vec<Segment> {
        let mut segments = Vec::new();
        let mut text = String::new();
        for c in input.chars() {
            match self.escape {
                Escape::Text => {
                    if c == '\x1b' {
                        self.escape = Escape::Start;
                    } else {
                        text.push(c);
                    }
                }
                Escape::Start => match c {
                    '[' => {
                        self.params.clear();
                        self.params_overflow = false;
                        self.escape = Escape::Csi;
                    }
                    ']' => self.escape = Escape::Osc,
                    _ => self.escape = Escape::Text,
                },
                Escape::Csi => {
                    if ('@'..='~').contains(&c) {
                        if c == 'm' && !self.params_overflow {
                            if !text.is_empty() {
                                segments.push(make_segment(std::mem::take(&mut text), &self.style));
                            }
                            let params: Vec<u32> = self
                                .params
                                .split(';')
                                .map(|p| p.parse().unwrap_or(0))
                                .collect();
                            apply_sgr(&mut self.style, &params);
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
        if !text.is_empty() {
            segments.push(make_segment(text, &self.style));
        }
        segments
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
        let expand = |segments: Vec<Segment>| -> Vec<(char, Style)> {
            segments
                .into_iter()
                .flat_map(|s| {
                    let style = s.style();
                    s.text.chars().map(|c| (c, style)).collect::<Vec<_>>()
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
        assert_eq!(parser.feed("mvisible")[0].text, "visible");
        assert!(
            parser
                .feed(&format!("\x1b]{}", "x".repeat(100_000)))
                .is_empty()
        );
        assert_eq!(parser.feed("\x07visible")[0].text, "visible");
    }
}
