//! Colour-coding for directory paths, so a given directory is easy to spot
//! wherever it appears: the Bookmarks, Recent dirs and Files panels, the input
//! row's prompt, and prompts echoed into the output pane.
//!
//! The home directory (`~`, or its full form like `C:\Users\me`) is always
//! [Palette] colour 0. Every other directory gets one of the remaining colours,
//! picked by hashing its path up to and including itself (`~/code`), so it's
//! the same colour in every path that runs through it. A directory never takes
//! its parent's colour, so neighbouring names stay visually separate.
//! Separators (`/`, `\`) keep the surrounding text's colour.

use std::fmt::Write;

use egui::{Color32, FontId, TextFormat, text::LayoutJob};

/// Hues spread around the colour wheel so any two read as different at a
/// glance. Kept short on purpose: more entries means near-identical pairs.
type Palette = [Color32; 8];

/// For dark backgrounds: the terminal panes, and the panels in dark mode.
const PALETTE_DARK: Palette = [
    Color32::from_rgb(255, 165, 50),  // home: amber
    Color32::from_rgb(80, 160, 255),  // blue
    Color32::from_rgb(90, 210, 110),  // green
    Color32::from_rgb(245, 120, 190), // pink
    Color32::from_rgb(175, 135, 255), // violet
    Color32::from_rgb(70, 210, 220),  // cyan
    Color32::from_rgb(195, 225, 70),  // lime
    Color32::from_rgb(250, 95, 85),   // red
];

/// The same hues, darkened to read on the light-mode panels.
const PALETTE_LIGHT: Palette = [
    Color32::from_rgb(200, 110, 0),
    Color32::from_rgb(20, 100, 210),
    Color32::from_rgb(20, 140, 50),
    Color32::from_rgb(200, 40, 130),
    Color32::from_rgb(120, 70, 210),
    Color32::from_rgb(0, 135, 150),
    Color32::from_rgb(110, 140, 0),
    Color32::from_rgb(205, 35, 35),
];

/// One run of a displayed path. `color` is `None` for text that keeps the
/// default colour (separators, and any prefix the caller adds).
pub struct Part {
    pub text: String,
    pub color: Option<Color32>,
}

impl Part {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            color: None,
        }
    }
}

fn is_sep(c: char) -> bool {
    c == '/' || c == '\\'
}

/// 64-bit FNV-1a. Hand-rolled rather than `DefaultHasher` so a directory keeps
/// its colour across Rust versions.
fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Split `display` (a path as shown to the user) into coloured runs.
/// `home` is how the home directory is spelled in `display` — `"~"` for a
/// [crate::render_with_tilde] string, or the full home path for an absolute
/// one — and is coloured as a single run. `dark` picks the palette for the
/// background it will be drawn on.
pub fn parts(display: &str, home: Option<&str>, dark: bool) -> Vec<Part> {
    let palette = if dark { &PALETTE_DARK } else { &PALETTE_LIGHT };
    let mut out = Vec::new();
    // The path so far, normalised (`~` for home, `/` separators, and
    // lowercased on Windows), so `~/code` and `C:\Users\me\Code` match.
    let mut key = String::new();
    let mut prev_idx = None;
    let mut rest = display;

    if let Some(h) = home.filter(|h| !h.is_empty())
        && let Some(after) = display.strip_prefix(h)
        && (after.is_empty() || after.starts_with(is_sep))
    {
        out.push(Part {
            text: h.to_string(),
            color: Some(palette[0]),
        });
        key.push('~');
        prev_idx = Some(0);
        rest = after;
    }

    let mut component = |name: &str, out: &mut Vec<Part>| {
        if name.is_empty() {
            return;
        }
        if !key.is_empty() {
            key.push('/');
        }
        if cfg!(windows) {
            key.push_str(&name.to_lowercase());
        } else {
            key.push_str(name);
        }
        // Colour 0 is home's; hash into the rest, stepping on to the next
        // colour if we landed on the parent's.
        let n = palette.len() as u64 - 1;
        let mut idx = 1 + (fnv1a(&key) % n) as usize;
        if prev_idx == Some(idx) {
            idx = idx % n as usize + 1;
        }
        prev_idx = Some(idx);
        out.push(Part {
            text: name.to_string(),
            color: Some(palette[idx]),
        });
    };

    let mut start = 0;
    for (i, c) in rest.char_indices() {
        if is_sep(c) {
            component(&rest[start..i], &mut out);
            out.push(Part::plain(c));
            start = i + c.len_utf8();
        }
    }
    component(&rest[start..], &mut out);

    out
}

/// The colour [parts] gives the last directory in `display`, e.g. for a
/// folder's row in the Files list.
pub fn last_color(display: &str, home: Option<&str>, dark: bool) -> Option<Color32> {
    parts(display, home, dark)
        .into_iter()
        .rev()
        .find_map(|p| p.color)
}

/// Shorten `parts` to at most `max` characters for a narrow column, keeping
/// the tail (the most specific part of the path) behind a leading `...`.
/// Truncating after colouring means a cut-off directory keeps its colour.
pub fn truncate_start(parts: Vec<Part>, max: usize) -> Vec<Part> {
    let count: usize = parts.iter().map(|p| p.text.chars().count()).sum();
    if count <= max {
        return parts;
    }
    // Reserve 3 chars for the ellipsis.
    let mut skip = count - max.saturating_sub(3);
    let mut out = vec![Part::plain("...")];
    for p in parts {
        let n = p.text.chars().count();
        if skip >= n {
            skip -= n;
            continue;
        }
        out.push(Part {
            text: p.text.chars().skip(skip).collect(),
            color: p.color,
        });
        skip = 0;
    }
    out
}

/// Lay `parts` out for a label or button. `default` colours the uncoloured
/// runs; pass [Color32::PLACEHOLDER] to use the widget's own text colour
/// (which follows hover / selection).
pub fn layout_job(parts: &[Part], font: FontId, default: Color32) -> LayoutJob {
    let mut job = LayoutJob::default();
    for p in parts {
        let format = TextFormat {
            font_id: font.clone(),
            color: p.color.unwrap_or(default),
            ..Default::default()
        };
        job.append(&p.text, 0.0, format);
    }
    job
}

/// `parts` as text with ANSI truecolour escapes, for strings that go through
/// the output pane's ANSI parser (see `crate::ansi`). Uncoloured runs reset to
/// the line's default colour.
pub fn to_ansi(parts: &[Part]) -> String {
    let mut s = String::new();
    for p in parts {
        match p.color {
            Some(c) => {
                let (r, g, b) = (c.r(), c.g(), c.b());
                let _ = write!(s, "\x1b[38;2;{r};{g};{b}m{}\x1b[39m", p.text);
            }
            None => s.push_str(&p.text),
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(parts: &[Part]) -> Vec<&str> {
        parts.iter().map(|p| p.text.as_str()).collect()
    }

    #[test]
    fn home_is_one_run_in_colour_0() {
        let full = parts(r"C:\Users\me\code", Some(r"C:\Users\me"), true);
        assert_eq!(texts(&full), [r"C:\Users\me", r"\", "code"]);
        assert_eq!(full[0].color, Some(PALETTE_DARK[0]));
        assert_eq!(full[1].color, None);

        let tilde = parts("~/code", Some("~"), true);
        assert_eq!(texts(&tilde), ["~", "/", "code"]);
        assert_eq!(tilde[0].color, Some(PALETTE_DARK[0]));
    }

    #[test]
    fn same_dir_same_colour_in_either_spelling() {
        let full = parts(r"C:\Users\me\code\a", Some(r"C:\Users\me"), true);
        let tilde = parts("~/code/b", Some("~"), true);
        assert_eq!(full[2].color, tilde[2].color);
        assert_ne!(full[2].color, Some(PALETTE_DARK[0]));
    }

    #[test]
    fn home_prefix_must_end_at_a_separator() {
        let p = parts(r"C:\Users\mel", Some(r"C:\Users\me"), true);
        assert_eq!(texts(&p), ["C:", r"\", "Users", r"\", "mel"]);
    }

    #[test]
    fn never_matches_parent() {
        for name in ["a", "b", "code", "src", "target", "project_a", "x/y/z"] {
            let p = parts(&format!("~/code/{name}"), Some("~"), true);
            let colors: Vec<_> = p.iter().filter_map(|p| p.color).collect();
            for w in colors.windows(2) {
                assert_ne!(w[0], w[1], "{name}");
            }
        }
    }

    #[test]
    fn ansi_round_trips_through_output_parser() {
        let p = parts(r"C:\Users\me\code", Some(r"C:\Users\me"), true);
        let segs = crate::ansi::parse(&to_ansi(&p));
        let got: Vec<_> = segs.iter().map(|s| (s.text.as_str(), s.fg)).collect();
        let want: Vec<_> = p.iter().map(|p| (p.text.as_str(), p.color)).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn truncation_keeps_tail_and_colours() {
        let p = truncate_start(parts("~/code/project_a", Some("~"), true), 10);
        assert_eq!(texts(&p), ["...", "oject_a"]);
        assert_eq!(p[1].color, last_color("~/code/project_a", Some("~"), true));
        let total: usize = p.iter().map(|p| p.text.chars().count()).sum();
        assert_eq!(total, 10);
    }
}
