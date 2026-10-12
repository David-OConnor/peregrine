//! Colour-coding for commands in the Command history and In this dir panels,
//! so runs of the same program are easy to spot down the list.
//!
//! Only the base command (the first word, split the way `run_command` does)
//! is coloured; its arguments keep the default colour. Every `cd` on screen is
//! one colour, every `cargo` another, and so on, from the same palette as
//! [crate::path_color].
//!
//! Hashing alone can't keep the commands on screen apart: with 8 colours, 6
//! commands all differ only ~8% of the time. So [CmdColors] hands colours out
//! to the [PALETTE_LEN] most recently used commands, which never share one,
//! and only commands older than those fall back to their hashed colour.

use std::collections::HashMap;

use crate::path_color::{PALETTE_LEN, Part, fnv1a, palette};

/// Palette index a base command prefers. FNV-1a's low bits are weak: bit `k`
/// of the hash depends only on bits `0..=k` of each input byte, so taking it
/// mod 8 (the palette size) would go by just the low 3 bits of each letter,
/// and e.g. `cd`, `cl`, `sd` and `kt` would always match. Mixing first
/// (MurmurHash3's 64-bit finaliser) spreads every input bit into the low
/// ones. `path_color` gets away without this as it reduces mod 7.
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

/// The key a base command's colour is looked up by. Program names are
/// case-insensitive on Windows, so `Cargo` and `cargo` match there, as
/// directory names do in `path_color`.
fn key(cmd: &str) -> String {
    if cfg!(windows) {
        cmd.to_lowercase()
    } else {
        cmd.to_string()
    }
}

/// Palette index for each of the [PALETTE_LEN] most recently used base
/// commands, all different. Rebuilt whenever the history changes (see
/// `State::refresh_cmd_colors`), so a command's colour can shift when another
/// one is run; keeping each command on its [preferred] colour where possible
/// keeps that rare.
#[derive(Default)]
pub struct CmdColors {
    recent: HashMap<String, usize>,
}

impl CmdColors {
    /// Walk the history newest first. Each new command takes its preferred
    /// colour if no more recent command has it, or else the next free one,
    /// until every colour is taken.
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

    /// Split the command line `text` into coloured runs: any leading
    /// whitespace, then the base command in its colour, then the rest
    /// uncoloured. `dark` picks the palette for the background it will be
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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn texts(parts: &[Part]) -> Vec<&str> {
        parts.iter().map(|p| p.text.as_str()).collect()
    }

    fn colour(colors: &CmdColors, text: &str) -> Option<egui::Color32> {
        colors.parts(text, true).into_iter().find_map(|p| p.color)
    }

    #[test]
    fn only_base_command_is_coloured() {
        let p = CmdColors::default().parts("cargo build --release", true);
        assert_eq!(texts(&p), ["cargo", " build --release"]);
        assert!(p[0].color.is_some());
        assert_eq!(p[1].color, None);
    }

    #[test]
    fn same_command_same_colour() {
        let c = CmdColors::new(["cd ..", "ls", "cd ~/code"]);
        assert_eq!(colour(&c, "cd ~/code"), colour(&c, "cd .."));
        assert_eq!(colour(&c, "cd ~/code"), colour(&c, "cd"));
    }

    #[test]
    fn leading_whitespace_stays_plain() {
        let c = CmdColors::default();
        let p = c.parts("  ls -la", true);
        assert_eq!(texts(&p), ["  ", "ls", " -la"]);
        assert_eq!(p[0].color, None);
        assert_eq!(p[1].color, colour(&c, "ls"));
    }

    #[test]
    fn preferred_depends_on_more_than_low_bits() {
        // These letters share their low 3 bits (`c`/`k`/`s` = 3, `d`/`l`/`t`
        // = 4), so unmixed FNV-1a mod 8 gives them all one colour.
        let prefs: HashSet<_> = ["cd", "cl", "ct", "kd", "kl", "kt", "sd", "sl", "st"]
            .into_iter()
            .map(preferred)
            .collect();
        assert!(prefs.len() > 1);
    }

    #[test]
    fn most_recent_commands_never_share() {
        let history = ["cd", "sync", "del", "fmt", "pwd", "ls", "git", "cargo"];
        let c = CmdColors::new(history);
        let colours: HashSet<_> = history.iter().map(|t| colour(&c, t)).collect();
        assert_eq!(colours.len(), PALETTE_LEN);
    }

    #[test]
    fn newest_command_gets_its_preferred_colour() {
        let c = CmdColors::new(["fmt", "cd", "sync"]);
        assert_eq!(colour(&c, "fmt"), Some(palette(true)[preferred("fmt")]));
    }

    #[test]
    fn older_commands_fall_back_to_preferred() {
        let history = ["a", "b", "c", "d", "e", "f", "g", "h", "old"];
        let c = CmdColors::new(history);
        assert_eq!(colour(&c, "old"), Some(palette(true)[preferred("old")]));
    }

    #[test]
    fn empty_and_blank_lines() {
        let c = CmdColors::new(["", "   "]);
        assert!(c.parts("", true).is_empty());
        let p = c.parts("   ", true);
        assert_eq!(texts(&p), ["   "]);
        assert_eq!(p[0].color, None);
    }
}
