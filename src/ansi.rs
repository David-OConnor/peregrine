//! An ANSI SGR escape-sequence parser.
//!
//! Shells like PowerShell emit sequences such as `ESC[32;1m` to colour their
//! output. We parse those into styled segments so the GUI can render.

use egui::{Color32, FontId, Stroke, TextFormat, text::LayoutJob};

#[derive(Clone, Copy)]
struct Style {
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
}

pub struct Segment {
    pub text: String,
    pub fg: Option<Color32>,
    pub bg: Option<Color32>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
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

/// Parse a string with ANSI escape sequences into styled text segments. SGR
/// sequences (`ESC[...m`) update the running style; other CSI/OSC sequences
/// are silently discarded so they don't leak into the rendered text.
pub fn parse(input: &str) -> Vec<Segment> {
    let mut segments: Vec<Segment> = Vec::new();
    let mut style = Style::new();
    let mut buf = String::new();
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '\x1b' {
            buf.push(c);
            continue;
        }

        let Some(&next) = chars.peek() else { break };

        if next == '[' {
            chars.next();
            let mut params_str = String::new();
            let mut final_char = None;
            while let Some(pc) = chars.next() {
                if pc.is_ascii_alphabetic() {
                    final_char = Some(pc);
                    break;
                }
                params_str.push(pc);
            }
            if final_char == Some('m') {
                if !buf.is_empty() {
                    segments.push(make_segment(std::mem::take(&mut buf), &style));
                }
                let params: Vec<u32> = if params_str.is_empty() {
                    vec![0]
                } else {
                    params_str
                        .split(';')
                        .map(|s| s.parse().unwrap_or(0))
                        .collect()
                };
                apply_sgr(&mut style, &params);
            }
        } else if next == ']' {
            // OSC: consume until BEL or ESC \
            chars.next();
            while let Some(pc) = chars.next() {
                if pc == '\x07' {
                    break;
                }
                if pc == '\x1b' {
                    let _ = chars.next();
                    break;
                }
            }
        } else {
            // Unknown two-byte escape; drop the next char.
            chars.next();
        }
    }

    if !buf.is_empty() {
        segments.push(make_segment(buf, &style));
    }
    segments
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

/// Build a `LayoutJob` from parsed segments. `default_fg` is used for
/// segments without an explicit foreground colour.
pub fn to_layout_job(segments: &[Segment], default_fg: Color32, font: FontId) -> LayoutJob {
    let mut job = LayoutJob::default();
    for seg in segments {
        let mut color = seg.fg.unwrap_or(default_fg);
        if seg.bold {
            // egui doesn't switch font weight without a bold font family,
            // so approximate bold by brightening the foreground — most
            // terminals do the same for the dim/bright palette swap.
            color = brighten(color);
        }
        let fmt = TextFormat {
            font_id: font.clone(),
            color,
            background: seg.bg.unwrap_or(Color32::TRANSPARENT),
            italics: seg.italic,
            underline: if seg.underline {
                Stroke::new(1.0_f32, color)
            } else {
                Stroke::NONE
            },
            ..Default::default()
        };
        job.append(&seg.text, 0.0, fmt);
    }
    job
}
