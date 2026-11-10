//! Color-coding for commands in the Cmd history panels,
//! so runs of the same program are easy to spot down the list.
//!
//! Only the base command (the first word, split the way `run_command` does)
//! is colored; its arguments keep the default color. Every `cd` on screen is
//! one color, every `cargo` another, and so on.

use std::collections::HashMap;

use egui::Color32;

use crate::path_color::Part;

/// Hues spread around the color wheel so any two read as different at a
/// glance. Kept short on purpose: more entries means near-identical pairs.
type Palette = [Color32; PALETTE_LEN];
const PALETTE_LEN: usize = 8;

/// For dark backgrounds: the terminal panes, and the panels in dark mode.
const PALETTE_DARK: Palette = [
    Color32::from_rgb(255, 165, 50),  // amber
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

/// The palette for a dark or light background.
fn palette(dark: bool) -> &'static Palette {
    if dark { &PALETTE_DARK } else { &PALETTE_LIGHT }
}

/// 64-bit FNV-1a. Hand-rolled rather than `DefaultHasher` so a command keeps
/// its color across Rust versions.
fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Palette index a base command prefers. FNV-1a's low bits are weak: bit `k`
/// of the hash depends only on bits `0..=k` of each input byte, so taking it
/// mod 8 (the palette size) would go by just the low 3 bits of each letter,
/// and e.g. `cd`, `cl`, `sd` and `kt` would always match. Mixing first
/// (MurmurHash3's 64-bit finaliser) spreads every input bit into the low
/// ones.
fn preferred(key: &str) -> usize {
    let mut h = fnv1a(key);
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^= h >> 33;
    (h % PALETTE_LEN as u64) as usize
}

/// `text` split into its leading whitespace, base command, and the rest.
fn split(text: &str) -> (&str, &str, &str) {
    let trimmed = text.trim_start();
    let lead = &text[..text.len() - trimmed.len()];
    let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    (lead, &trimmed[..end], &trimmed[end..])
}

/// The key a base command's color is looked up by. Program names are
/// case-insensitive on Windows, so `Cargo` and `cargo` match there.
fn key(cmd: &str) -> String {
    if cfg!(windows) {
        cmd.to_lowercase()
    } else {
        cmd.to_string()
    }
}

/// Palette index for each of the [PALETTE_LEN] most recently used base
/// commands, all different. Rebuilt whenever the history changes (see
/// `State::refresh_cmd_colors`), so a command's color can shift when another
/// one is run; keeping each command on its [preferred] color where possible
/// keeps that rare.
#[derive(Default)]
pub struct CmdColors {
    recent: HashMap<String, usize>,
}

impl CmdColors {
    /// Walk the history newest first. Each new command takes its preferred
    /// color if no more recent command has it, or else the next free one,
    /// until every color is taken.
    pub fn new<'a>(newest_first: impl IntoIterator<Item = &'a str>) -> Self {
        let mut taken = [false; PALETTE_LEN];
        let mut recent = HashMap::new();

        for text in newest_first {
            if recent.len() == PALETTE_LEN {
                break;
            }

            let (_, cmd, _) = split(text);
            let k = key(cmd);
            if cmd.is_empty() || recent.contains_key(&k) {
                continue;
            }

            let mut i = preferred(&k);
            while taken[i] {
                i = (i + 1) % PALETTE_LEN;
            }

            taken[i] = true;
            recent.insert(k, i);
        }

        Self { recent }
    }

    /// Split the command line `text` into colored runs: any leading
    /// whitespace, then the base command in its color, then the rest
    /// uncolored. `dark` picks the palette for the background it will be
    /// drawn on.
    pub fn parts(&self, text: &str, dark: bool) -> Vec<Part> {
        let (lead, cmd, rest) = split(text);
        let mut out = Vec::new();

        if !lead.is_empty() {
            out.push(Part::plain(lead));
        }

        if !cmd.is_empty() {
            let k = key(cmd);
            let i = self
                .recent
                .get(&k)
                .copied()
                .unwrap_or_else(|| preferred(&k));
            out.push(Part {
                text: cmd.to_string(),
                color: Some(palette(dark)[i]),
            });
        }

        if !rest.is_empty() {
            out.push(Part::plain(rest));
        }

        out
    }
}
