//! Colour-coding for directory paths, so how deep a directory sits reads at
//! a glance wherever it appears: the Bookmarks, Recent dirs and Files panels,
//! the input row's prompt, and prompts echoed into the output pane.
//!
//! Each directory is coloured by its depth, along the Viridis scale: a root
//! (`/`, `C:`, a UNC path's `\\`) is purple, and each level below it steps
//! through blue and green towards yellow, which every directory
//! [depth_levels] or more levels down gets. The home directory (`~`, or its
//! full form like `C:\Users\me`) counts as a root, so `~/code` is one level
//! down. Separators (`/`, `\`) between directories keep the surrounding
//! text's colour.

use std::fmt::Write;
use std::sync::atomic::{AtomicU8, Ordering};

use egui::{Color32, FontId, TextFormat, text::LayoutJob};

/// Default for [depth_levels].
pub const DEFAULT_DEPTH_LEVELS: u8 = 5;
/// Range for [depth_levels].
pub const MIN_DEPTH_LEVELS: u8 = 1;
pub const MAX_DEPTH_LEVELS: u8 = 32;

/// See [depth_levels]. A global rather than a parameter, like the theme, as
/// it applies to every path drawn, from all over the UI.
static DEPTH_LEVELS: AtomicU8 = AtomicU8::new(DEFAULT_DEPTH_LEVELS);

/// How many levels below a root (or `~`) a directory is when its colour
/// reaches the yellow end of the scale. Set by the tab strip's "Dir depth"
/// control (see `State::adjust_dir_color_depth`) and persisted with the theme.
pub fn depth_levels() -> u8 {
    DEPTH_LEVELS.load(Ordering::Relaxed)
}

/// Set [depth_levels], clamped to [MIN_DEPTH_LEVELS]..=[MAX_DEPTH_LEVELS].
pub fn set_depth_levels(n: u8) {
    DEPTH_LEVELS.store(n.clamp(MIN_DEPTH_LEVELS, MAX_DEPTH_LEVELS), Ordering::Relaxed);
}

/// Viridis at 11 even stops (matplotlib's `#440154` to `#FDE725`), with its
/// OKLab lightness remapped linearly so both ends read on the background, and
/// hue and chroma kept. For dark backgrounds (the terminal panes, and the
/// panels in dark mode), lightness runs 0.60 to 0.93: at least 4:1 contrast
/// on the panels.
const VIRIDIS_DARK: [Color32; 11] = [
    Color32::from_rgb(0xA2, 0x63, 0xAF), // root: purple
    Color32::from_rgb(0x98, 0x77, 0xC8),
    Color32::from_rgb(0x86, 0x8E, 0xD1),
    Color32::from_rgb(0x76, 0xA2, 0xCF),
    Color32::from_rgb(0x69, 0xB4, 0xC8),
    Color32::from_rgb(0x60, 0xC5, 0xBE),
    Color32::from_rgb(0x5C, 0xD5, 0xB0),
    Color32::from_rgb(0x6B, 0xE3, 0x98),
    Color32::from_rgb(0x93, 0xEB, 0x7C),
    Color32::from_rgb(0xCB, 0xEE, 0x62),
    Color32::from_rgb(0xFF, 0xEB, 0x5D), // deepest: yellow
];

/// [VIRIDIS_DARK] for the light-mode panels: lightness 0.30 to 0.56, so the
/// yellow end darkens to ochre (at least 4:1 contrast).
const VIRIDIS_LIGHT: [Color32; 11] = [
    Color32::from_rgb(0x48, 0x06, 0x54),
    Color32::from_rgb(0x41, 0x1C, 0x68),
    Color32::from_rgb(0x31, 0x32, 0x6E),
    Color32::from_rgb(0x19, 0x43, 0x6B),
    Color32::from_rgb(0x00, 0x52, 0x64),
    Color32::from_rgb(0x00, 0x5F, 0x5A),
    Color32::from_rgb(0x00, 0x6A, 0x4C),
    Color32::from_rgb(0x00, 0x74, 0x32),
    Color32::from_rgb(0x21, 0x79, 0x00),
    Color32::from_rgb(0x5F, 0x79, 0x00),
    Color32::from_rgb(0x8C, 0x74, 0x00),
];

/// The colour of a directory `depth` levels below a root (or `~`), for a
/// `dark` or light background: interpolated along the Viridis stops, and
/// yellow from [depth_levels] down.
pub fn depth_color(depth: usize, dark: bool) -> Color32 {
    let stops = if dark { &VIRIDIS_DARK } else { &VIRIDIS_LIGHT };
    let levels = depth_levels() as usize;
    let pos = depth.min(levels) as f32 / levels as f32 * (stops.len() - 1) as f32;
    let i = (pos as usize).min(stops.len() - 2);
    let t = pos - i as f32;
    let (a, b) = (stops[i], stops[i + 1]);
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}

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

/// Split `display` (a path as shown to the user) into coloured runs.
/// `home` is how the home directory is spelled in `display` — `"~"` for a
/// [crate::render_with_tilde] string, or the full home path for an absolute
/// one — and is coloured as a single run, as a root. `dark` picks the
/// colours for the background it will be drawn on.
pub fn parts(display: &str, home: Option<&str>, dark: bool) -> Vec<Part> {
    let mut out = Vec::new();
    // Depth of the next directory.
    let mut depth = 0;
    let mut rest = display;

    if let Some(h) = home.filter(|h| !h.is_empty())
        && let Some(after) = display.strip_prefix(h)
        && (after.is_empty() || after.starts_with(is_sep))
    {
        out.push(Part {
            text: h.to_string(),
            color: Some(depth_color(0, dark)),
        });
        depth = 1;
        rest = after;
    } else {
        // A leading separator is the root: `/`, or a UNC path's `\\`.
        let root = rest.len() - rest.trim_start_matches(is_sep).len();
        if root > 0 {
            out.push(Part {
                text: rest[..root].to_string(),
                color: Some(depth_color(0, dark)),
            });
            depth = 1;
            rest = &rest[root..];
        }
    }

    let mut component = |name: &str, out: &mut Vec<Part>| {
        if name.is_empty() {
            return;
        }
        out.push(Part {
            text: name.to_string(),
            color: Some(depth_color(depth, dark)),
        });
        depth += 1;
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
/// (which follows hover / selection). If `last_font` is given, the last
/// coloured run (the path's final directory) is drawn in it, e.g. to bold it.
pub fn layout_job(
    parts: &[Part],
    font: FontId,
    last_font: Option<FontId>,
    default: Color32,
) -> LayoutJob {
    let last = parts.iter().rposition(|p| p.color.is_some());
    let mut job = LayoutJob::default();

    for (i, p) in parts.iter().enumerate() {
        let font_id = match &last_font {
            Some(f) if Some(i) == last => f.clone(),
            _ => font.clone(),
        };

        let format = TextFormat {
            font_id,
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

    fn colors(display: &str, home: Option<&str>) -> Vec<(String, Option<Color32>)> {
        parts(display, home, true)
            .into_iter()
            .map(|p| (p.text, p.color))
            .collect()
    }

    fn c(depth: usize) -> Option<Color32> {
        Some(depth_color(depth, true))
    }

    #[test]
    fn root_and_home_are_purple() {
        assert_eq!(depth_color(0, true), VIRIDIS_DARK[0]);
        assert_eq!(colors("/", None), [("/".into(), c(0))]);
        assert_eq!(colors("~", Some("~")), [("~".into(), c(0))]);
        assert_eq!(
            colors(r"C:\Users\me", Some(r"C:\Users\me")),
            [(r"C:\Users\me".into(), c(0))]
        );
    }

    #[test]
    fn depth_counts_from_root_or_home() {
        assert_eq!(
            colors("/usr/lib", None),
            [
                ("/".into(), c(0)),
                ("usr".into(), c(1)),
                ("/".into(), None),
                ("lib".into(), c(2)),
            ]
        );
        assert_eq!(
            colors(r"C:\Users", None),
            [("C:".into(), c(0)), (r"\".into(), None), ("Users".into(), c(1))]
        );
        assert_eq!(
            colors(r"C:\Users\me\strike", Some(r"C:\Users\me")),
            [
                (r"C:\Users\me".into(), c(0)),
                (r"\".into(), None),
                ("strike".into(), c(1)),
            ]
        );
    }

    #[test]
    fn saturates_at_depth_levels() {
        let levels = depth_levels() as usize;
        assert_eq!(depth_color(levels, true), VIRIDIS_DARK[10]);
        assert_eq!(depth_color(levels + 5, false), VIRIDIS_LIGHT[10]);
    }
}
