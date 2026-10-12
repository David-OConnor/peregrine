//! Colour-coding for commands in the Command history and In this dir panels,
//! so runs of the same program are easy to spot down the list.
//!
//! Only the base command (the first word, split the way `run_command` does)
//! is coloured; its arguments keep the default colour. The colour comes from
//! hashing that word, so every `cd` is one colour, every `cargo` another, and
//! so on. Uses the same palette and hash as [crate::path_color].

use crate::path_color::{Part, fnv1a, palette};

/// Split the command line `text` into coloured runs: any leading whitespace,
/// then the base command in its colour, then the rest uncoloured. `dark`
/// picks the palette for the background it will be drawn on.
pub fn parts(text: &str, dark: bool) -> Vec<Part> {
    let trimmed = text.trim_start();
    let lead = &text[..text.len() - trimmed.len()];
    let (cmd, rest) = trimmed.split_at(trimmed.find(char::is_whitespace).unwrap_or(trimmed.len()));

    let mut out = Vec::new();
    if !lead.is_empty() {
        out.push(Part::plain(lead));
    }
    if !cmd.is_empty() {
        // Program names are case-insensitive on Windows, so `Cargo` and
        // `cargo` match there, as directory names do in `path_color`.
        let key = if cfg!(windows) {
            cmd.to_lowercase()
        } else {
            cmd.to_string()
        };
        let palette = palette(dark);
        out.push(Part {
            text: cmd.to_string(),
            color: Some(palette[(fnv1a(&key) % palette.len() as u64) as usize]),
        });
    }
    if !rest.is_empty() {
        out.push(Part::plain(rest));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(parts: &[Part]) -> Vec<&str> {
        parts.iter().map(|p| p.text.as_str()).collect()
    }

    #[test]
    fn only_base_command_is_coloured() {
        let p = parts("cargo build --release", true);
        assert_eq!(texts(&p), ["cargo", " build --release"]);
        assert!(p[0].color.is_some());
        assert_eq!(p[1].color, None);
    }

    #[test]
    fn same_command_same_colour() {
        let a = parts("cd ~/code", true);
        let b = parts("cd ..", true);
        let c = parts("cd", true);
        assert_eq!(a[0].color, b[0].color);
        assert_eq!(a[0].color, c[0].color);
    }

    #[test]
    fn leading_whitespace_stays_plain() {
        let p = parts("  ls -la", true);
        assert_eq!(texts(&p), ["  ", "ls", " -la"]);
        assert_eq!(p[0].color, None);
        assert_eq!(p[1].color, parts("ls", true)[0].color);
    }

    #[test]
    fn empty_and_blank_lines() {
        assert!(parts("", true).is_empty());
        let p = parts("   ", true);
        assert_eq!(texts(&p), ["   "]);
        assert_eq!(p[0].color, None);
    }
}
