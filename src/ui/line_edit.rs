//! Shell's line-editing keys (its line editor's Emacs-style bindings) for the
//! CLI input: moving to the start or end, deleting words and to either end
//! into a kill ring, pasting back, swapping, and changing case. The keys are
//! taken out of the frame's input before the input box is drawn, so egui's
//! own uses of some of them (Ctrl+A selects all, Ctrl+Y redoes) don't also
//! happen.
//!
//! On Windows and Linux, Ctrl+W, Ctrl+T and Ctrl+N stay tab shortcuts (see
//! [tab_keys]), which take them first. On Mac those use Cmd, so Ctrl + them
//! reach here.
//!
//! Positions are in chars, as egui's text cursor counts them.
//!
//! [tab_keys]: super::tab_keys

use egui::{
    Context, Event, Id, Key, Modifiers, TextEdit, text::CCursor, text_selection::CCursorRange,
};

/// A change to the input line and its caret.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Edit {
    /// Ctrl+A, Home.
    Start,
    /// Ctrl+E, End.
    End,
    /// Ctrl+W: back to the start of the whitespace-separated word.
    KillWordBack,
    /// Alt+D: to the end of the word.
    KillWordForward,
    /// Ctrl+K.
    KillToEnd,
    /// Ctrl+U.
    KillToStart,
    /// Ctrl+Y: paste the last deleted text.
    Yank,
    /// Ctrl+T.
    TransposeChars,
    /// Alt+T.
    TransposeWords,
    /// Alt+C, Alt+U, Alt+L: the word at or after the caret.
    Capitalize,
    Upcase,
    Downcase,
    /// Ctrl+D, on a line with text.
    DeleteChar,
}

/// A line-editing key pressed in the CLI input.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LineKey {
    Edit(Edit),
    /// Ctrl+P / Ctrl+N: the previous / next history entry, as Up / Down.
    History {
        up: bool,
    },
    /// Ctrl+L.
    ClearScreen,
    /// Ctrl+D: delete the character under the caret or, on an empty line,
    /// end a running program's input, or `exit`.
    Eof,
}

/// The line-editing key `key` + `modifiers` stands for, if any. Modifiers
/// must match exactly, so an AltGr character (Ctrl+Alt on Windows) still
/// types. `alt` is the pattern standing for Alt: Cmd+Option on Mac, where
/// Option + a letter types a symbol.
fn line_key(key: Key, modifiers: Modifiers, alt: Modifiers) -> Option<LineKey> {
    let is = |pattern: Modifiers| modifiers.matches_exact(pattern);

    let edit = if is(Modifiers::CTRL) {
        match key {
            Key::A => Edit::Start,
            Key::E => Edit::End,
            Key::W => Edit::KillWordBack,
            Key::K => Edit::KillToEnd,
            Key::U => Edit::KillToStart,
            Key::Y => Edit::Yank,
            Key::T => Edit::TransposeChars,
            Key::P => return Some(LineKey::History { up: true }),
            Key::N => return Some(LineKey::History { up: false }),
            Key::L => return Some(LineKey::ClearScreen),
            Key::D => return Some(LineKey::Eof),
            _ => return None,
        }
    } else if is(alt) {
        match key {
            Key::D => Edit::KillWordForward,
            Key::T => Edit::TransposeWords,
            Key::C => Edit::Capitalize,
            Key::U => Edit::Upcase,
            Key::L => Edit::Downcase,
            _ => return None,
        }
    } else if is(Modifiers::NONE) {
        match key {
            Key::Home => Edit::Start,
            Key::End => Edit::End,
            _ => return None,
        }
    } else {
        return None;
    };

    Some(LineKey::Edit(edit))
}

/// Remove the line-editing key presses in `events`, returning them in order.
/// Ctrl+_ becomes egui's own undo key (Ctrl+Z), left in place for the input
/// box. Holding Ctrl+L or Ctrl+D down acts just once.
pub fn take_keys(events: &mut Vec<Event>, mac: bool) -> Vec<LineKey> {
    let alt = if mac {
        Modifiers::MAC_CMD | Modifiers::ALT
    } else {
        Modifiers::ALT
    };
    let mut keys = Vec::new();
    // Alt + a letter also types it, in a text event just after the key's;
    // drop that. (With Cmd held, as on Mac, nothing's typed.)
    let mut drop_text = false;

    events.retain_mut(|event| {
        let Event::Key {
            key,
            pressed: true,
            repeat,
            modifiers,
            ..
        } = event
        else {
            if matches!(event, Event::Text(_)) && std::mem::take(&mut drop_text) {
                return false;
            }
            return true;
        };
        drop_text = false;

        if *key == Key::Minus && modifiers.matches_exact(Modifiers::CTRL | Modifiers::SHIFT) {
            *key = Key::Z;
            *modifiers = Modifiers::COMMAND;
            return true;
        }
        let Some(line_key) = line_key(*key, *modifiers, alt) else {
            return true;
        };

        if !*repeat || !matches!(line_key, LineKey::ClearScreen | LineKey::Eof) {
            keys.push(line_key);
        }
        drop_text = modifiers.alt && !modifiers.command;
        false
    });

    keys
}

/// The last deleted text, for [Edit::Yank].
#[derive(Default)]
pub struct KillRing {
    text: String,
    /// The line and caret just after the last kill. A kill made from them
    /// adds to that one, so Ctrl+W twice then Ctrl+Y brings back both words.
    after: Option<(String, usize)>,
}

/// Word chars for Alt+D, Alt+T, and the case changes.
fn is_word(c: char) -> bool {
    c.is_alphanumeric()
}

/// Word chars for Ctrl+W.
fn is_big_word(c: char) -> bool {
    !c.is_whitespace()
}

/// The start of the word before `pos`, skipping what isn't word first.
fn word_start(chars: &[char], pos: usize, word: fn(char) -> bool) -> usize {
    let mut i = pos;
    while i > 0 && !word(chars[i - 1]) {
        i -= 1;
    }
    while i > 0 && word(chars[i - 1]) {
        i -= 1;
    }
    i
}

/// The end of the word after `pos`, skipping what isn't word first.
fn word_end(chars: &[char], pos: usize, word: fn(char) -> bool) -> usize {
    let mut i = pos;
    while i < chars.len() && !word(chars[i]) {
        i += 1;
    }
    while i < chars.len() && word(chars[i]) {
        i += 1;
    }
    i
}

/// Swap the word at or after `pos` with the one before it, returning the
/// caret after them. With no word after `pos`, the last two before it.
/// `None` if there aren't two words.
fn transpose_words(chars: &mut Vec<char>, pos: usize) -> Option<usize> {
    let w2_end = if chars[pos..].iter().any(|&c| is_word(c)) {
        word_end(chars, pos, is_word)
    } else {
        word_end(chars, word_start(chars, pos, is_word), is_word)
    };
    let w2_start = word_start(chars, w2_end, is_word);
    let w1_start = word_start(chars, w2_start, is_word);
    let w1_end = word_end(chars, w1_start, is_word);
    if w1_start == w2_start || w1_end > w2_start {
        return None;
    }

    let swapped: Vec<char> = [
        &chars[w2_start..w2_end],
        &chars[w1_end..w2_start],
        &chars[w1_start..w1_end],
    ]
    .concat();
    chars.splice(w1_start..w2_end, swapped);
    Some(w2_end)
}

/// Apply `edit` to `line`, with the caret at `pos`, returning the caret's new
/// position. `line` may hold several lines, as the Scratchpad's does: the
/// moves and kills to either end, and Ctrl+T, then keep to the caret's line,
/// and Ctrl+K at a line's end deletes its newline, joining the next on. The
/// word edits and Ctrl+D cross lines, as in Emacs.
pub fn apply(edit: Edit, line: &mut String, pos: usize, ring: &mut KillRing) -> usize {
    let mut chars: Vec<char> = line.chars().collect();
    let len = chars.len();
    let pos = pos.min(len);
    let line_start = chars[..pos]
        .iter()
        .rposition(|&c| c == '\n')
        .map_or(0, |i| i + 1);
    let line_end = chars[pos..]
        .iter()
        .position(|&c| c == '\n')
        .map_or(len, |i| pos + i);

    // A deletion into the kill ring, of `from..to`.
    let kill = |chars: &mut Vec<char>, from: usize, to: usize, ring: &mut KillRing| {
        if from == to {
            return pos;
        }
        let killed: String = chars.drain(from..to).collect();
        let line_before: String = line.clone();
        if ring.after.as_ref() == Some(&(line_before, pos)) {
            // Text killed backward goes in front of what was killed before.
            if to == pos {
                ring.text.insert_str(0, &killed);
            } else {
                ring.text.push_str(&killed);
            }
        } else {
            ring.text = killed;
        }
        ring.after = Some((chars.iter().collect(), from));
        from
    };

    let new_pos = match edit {
        Edit::Start => line_start,
        Edit::End => line_end,
        Edit::KillWordBack => {
            let start = word_start(&chars, pos, is_big_word);
            kill(&mut chars, start, pos, ring)
        }
        Edit::KillWordForward => {
            let end = word_end(&chars, pos, is_word);
            kill(&mut chars, pos, end, ring)
        }
        Edit::KillToEnd if pos == line_end && pos < len => kill(&mut chars, pos, pos + 1, ring),
        Edit::KillToEnd => kill(&mut chars, pos, line_end, ring),
        Edit::KillToStart => kill(&mut chars, line_start, pos, ring),
        Edit::Yank => {
            ring.after = None;
            chars.splice(pos..pos, ring.text.chars());
            pos + ring.text.chars().count()
        }
        Edit::TransposeChars => {
            if pos == line_start || line_end - line_start < 2 {
                return pos;
            }
            // At the end of the line, the two chars before the caret.
            let at = if pos == line_end { pos - 1 } else { pos };
            chars.swap(at - 1, at);
            at + 1
        }
        Edit::TransposeWords => match transpose_words(&mut chars, pos) {
            Some(pos) => pos,
            None => return pos,
        },
        Edit::Capitalize | Edit::Upcase | Edit::Downcase => {
            let Some(start) = (pos..len).find(|&i| is_word(chars[i])) else {
                return pos;
            };
            let end = word_end(&chars, start, is_word);
            let word: String = chars[start..end].iter().collect();
            let changed = match edit {
                Edit::Upcase => word.to_uppercase(),
                Edit::Downcase => word.to_lowercase(),
                _ => {
                    let mut rest = word.chars();
                    let first = rest.next().into_iter().flat_map(char::to_uppercase);
                    first.chain(rest.as_str().to_lowercase().chars()).collect()
                }
            };
            let changed_len = changed.chars().count();
            chars.splice(start..end, changed.chars());
            start + changed_len
        }
        Edit::DeleteChar => {
            if pos < len {
                chars.remove(pos);
            }
            pos
        }
    };

    *line = chars.into_iter().collect();
    new_pos
}

/// [Apply](apply) `edit` to `text`, the text of the text box `id`, at the
/// box's caret, moving the caret to match.
pub fn apply_in_box(ctx: &Context, id: Id, text: &mut String, edit: Edit, ring: &mut KillRing) {
    let caret = TextEdit::load_state(ctx, id)
        .and_then(|s| s.cursor.char_range())
        .map_or(text.chars().count(), |r| r.primary.index.0);
    let caret = apply(edit, text, caret, ring);
    set_caret(ctx, id, caret);
}

/// Put the text box `id`'s caret at char `caret`, with nothing selected.
pub fn set_caret(ctx: &Context, id: Id, caret: usize) {
    let mut edit_state = TextEdit::load_state(ctx, id).unwrap_or_default();
    edit_state
        .cursor
        .set_char_range(Some(CCursorRange::one(CCursor::new(caret))));
    edit_state.store(ctx, id);
}

/// Select all of `text`, the text of the text box `id`.
pub fn select_all(ctx: &Context, id: Id, text: &str) {
    let mut edit_state = TextEdit::load_state(ctx, id).unwrap_or_default();
    edit_state.cursor.set_char_range(Some(CCursorRange::two(
        CCursor::new(0),
        CCursor::new(text.chars().count()),
    )));
    edit_state.store(ctx, id);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Apply `edit` to `line` with the caret at the `|`, returning the result
    /// with its caret marked the same way.
    fn run(edit: Edit, line: &str, ring: &mut KillRing) -> String {
        let pos = line.chars().position(|c| c == '|').unwrap();
        let mut text = line.replacen('|', "", 1);
        let pos = apply(edit, &mut text, pos, ring);
        let mut chars: Vec<char> = text.chars().collect();
        chars.insert(pos, '|');
        chars.into_iter().collect()
    }

    fn edit(edit: Edit, line: &str) -> String {
        run(edit, line, &mut KillRing::default())
    }

    #[test]
    fn moves_to_either_end() {
        assert_eq!(edit(Edit::Start, "git st|atus"), "|git status");
        assert_eq!(edit(Edit::End, "git st|atus"), "git status|");
    }

    #[test]
    fn kills_whitespace_words_back_and_alphanumeric_words_forward() {
        assert_eq!(edit(Edit::KillWordBack, "cd ~/code/peregrine|"), "cd |");
        assert_eq!(edit(Edit::KillWordBack, "cd foo  |"), "cd |");
        assert_eq!(edit(Edit::KillWordBack, "|cd"), "|cd");
        assert_eq!(edit(Edit::KillWordForward, "cd |~/code/x"), "cd |/x");
        assert_eq!(edit(Edit::KillWordForward, "git| status -s"), "git| -s");
    }

    #[test]
    fn kills_to_either_end() {
        assert_eq!(edit(Edit::KillToEnd, "git |status"), "git |");
        assert_eq!(edit(Edit::KillToStart, "git |status"), "|status");
    }

    #[test]
    fn yanks_the_last_kill_and_joins_consecutive_ones() {
        let mut ring = KillRing::default();
        let line = run(Edit::KillWordBack, "echo one two three|", &mut ring);
        let line = run(Edit::KillWordBack, &line, &mut ring);
        assert_eq!(line, "echo one |");
        assert_eq!(run(Edit::Yank, &line, &mut ring), "echo one two three|");

        // Typing (here, a move) in between starts a fresh kill.
        let mut ring = KillRing::default();
        let line = run(Edit::KillToEnd, "a |b c", &mut ring);
        let line = run(Edit::Start, &line, &mut ring);
        let line = run(Edit::KillToEnd, &line, &mut ring);
        assert_eq!(run(Edit::Yank, &line, &mut ring), "a |");

        // Forward kills add to the end.
        let mut ring = KillRing::default();
        let line = run(Edit::KillWordForward, "|one two", &mut ring);
        let line = run(Edit::KillWordForward, &line, &mut ring);
        assert_eq!(run(Edit::Yank, &line, &mut ring), "one two|");
    }

    #[test]
    fn transposes_chars() {
        assert_eq!(edit(Edit::TransposeChars, "gti|"), "git|");
        assert_eq!(edit(Edit::TransposeChars, "g|it"), "ig|t");
        assert_eq!(edit(Edit::TransposeChars, "sl|"), "ls|");
        assert_eq!(edit(Edit::TransposeChars, "|ab"), "|ab");
        assert_eq!(edit(Edit::TransposeChars, "a|"), "a|");
    }

    #[test]
    fn transposes_words() {
        assert_eq!(edit(Edit::TransposeWords, "one two|"), "two one|");
        assert_eq!(
            edit(Edit::TransposeWords, "one tw|o three"),
            "two one| three"
        );
        assert_eq!(edit(Edit::TransposeWords, "a b c  |"), "a c b|  ");
        assert_eq!(edit(Edit::TransposeWords, "one|"), "one|");
    }

    #[test]
    fn changes_case_of_the_next_word() {
        assert_eq!(edit(Edit::Capitalize, "|hELLO world"), "Hello| world");
        assert_eq!(edit(Edit::Upcase, "echo| hi there"), "echo HI| there");
        assert_eq!(edit(Edit::Downcase, "HE|LLO"), "HEllo|");
        assert_eq!(edit(Edit::Upcase, "done  |"), "done  |");
        assert_eq!(edit(Edit::Upcase, "|straße"), "STRASSE|");
    }

    #[test]
    fn deletes_the_char_under_the_caret() {
        assert_eq!(edit(Edit::DeleteChar, "l|xs"), "l|s");
        assert_eq!(edit(Edit::DeleteChar, "ls|"), "ls|");
    }

    #[test]
    fn keeps_line_edits_to_the_caret_line() {
        assert_eq!(edit(Edit::Start, "one\ntw|o\nthree"), "one\n|two\nthree");
        assert_eq!(edit(Edit::End, "one\nt|wo\nthree"), "one\ntwo|\nthree");
        assert_eq!(edit(Edit::KillToEnd, "one\nt|wo\nthree"), "one\nt|\nthree");
        assert_eq!(
            edit(Edit::KillToStart, "one\ntw|o\nthree"),
            "one\n|o\nthree"
        );
        assert_eq!(edit(Edit::TransposeChars, "one\nab|\nc"), "one\nba|\nc");
        assert_eq!(edit(Edit::TransposeChars, "one\n|ab"), "one\n|ab");

        // At a line's end, Ctrl+K takes the newline; Ctrl+Y brings it back.
        let mut ring = KillRing::default();
        let line = run(Edit::KillToEnd, "one|\ntwo", &mut ring);
        assert_eq!(line, "one|two");
        assert_eq!(run(Edit::Yank, &line, &mut ring), "one\n|two");
        assert_eq!(edit(Edit::KillToEnd, "one\ntwo|"), "one\ntwo|");

        // Word edits and Ctrl+D cross lines.
        assert_eq!(edit(Edit::KillWordBack, "one\n|two"), "|two");
        assert_eq!(edit(Edit::DeleteChar, "one|\ntwo"), "one|two");
    }

    fn press(key: Key, modifiers: Modifiers) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    #[test]
    fn takes_bound_keys_and_the_text_alt_types() {
        let mut events = vec![
            press(Key::A, Modifiers::CTRL),
            press(Key::D, Modifiers::ALT),
            Event::Text("d".into()),
            Event::Text("x".into()),
            press(Key::Minus, Modifiers::CTRL | Modifiers::SHIFT),
            // AltGr, and an unbound key, still type.
            press(Key::D, Modifiers::CTRL | Modifiers::ALT),
            press(Key::B, Modifiers::CTRL),
        ];
        let keys = take_keys(&mut events, false);

        assert_eq!(
            keys,
            [
                LineKey::Edit(Edit::Start),
                LineKey::Edit(Edit::KillWordForward)
            ]
        );
        assert_eq!(
            events,
            [
                Event::Text("x".into()),
                press(Key::Z, Modifiers::COMMAND),
                press(Key::D, Modifiers::CTRL | Modifiers::ALT),
                press(Key::B, Modifiers::CTRL),
            ]
        );
    }
}
