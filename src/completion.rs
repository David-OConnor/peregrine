//! Input-row autocomplete, matching the CLI's rustyline setup: Tab completion
//! (see [complete]) and the greyed history autosuggestion (see [history_hint]).

use std::{fs, path::Path};

use shell::{
    CompletionCandidate, CompletionResult, HistoryItem, RecentDir, complete_cd_path,
    complete_clone_url, complete_command_path,
};

/// Tab completion for `line` with the caret at `pos`, trying the same sources
/// in the same order as the CLI's `ShellHelper`: `cd` paths, `clone` repo
/// URLs, explicit paths (`./script.sh`), then files and folders.
pub fn complete(
    line: &str,
    pos: usize,
    cwd: &Path,
    home: Option<&Path>,
    bookmarks: &[std::path::PathBuf],
    recent_dirs: &[RecentDir],
    clone_roots: &[String],
) -> Option<CompletionResult> {
    let found = |r: &CompletionResult| !r.candidates.is_empty();
    complete_cd_path(line, pos, cwd, home, bookmarks, recent_dirs)
        .filter(found)
        .or_else(|| complete_clone_url(line, pos, clone_roots).filter(found))
        .or_else(|| complete_command_path(line, pos, cwd, home).filter(found))
        .or_else(|| complete_filename(line, pos, cwd, home))
}

/// The rest of the newest history entry that starts with `live`, as the CLI's
/// rustyline `HistoryHinter` proposes: case-sensitive, and nothing when the
/// input is empty or that entry is exactly the input.
pub fn history_hint(history: &[HistoryItem], live: &str) -> Option<String> {
    if live.is_empty() {
        return None;
    }
    let entry = history.iter().rev().find(|h| h.text.starts_with(live))?;
    (entry.text != live).then(|| entry.text[live.len()..].to_string())
}

/// Fallback Tab completion, like the CLI's rustyline `FilenameCompleter`: the
/// word before the caret completes against files and folders in the
/// directory it points into (the cwd for a bare name). Folders get a trailing
/// `/` so completion continues into them. A name with a space is opened with
/// a `"`, and inside an open quote the word runs back to that quote, so
/// `python "White W` completes too. Files close the quote; folders leave it
/// open for the next Tab.
fn complete_filename(
    line: &str,
    pos: usize,
    cwd: &Path,
    home: Option<&Path>,
) -> Option<CompletionResult> {
    let before = line.get(..pos)?;
    let quote = open_quote(before);
    let start = match quote {
        Some((i, _)) => i + 1,
        None => before.rfind(char::is_whitespace).map_or(0, |i| i + 1),
    };

    let word = &before[start..];
    let (dir_part, leaf) = match word.rfind(['/', '\\']) {
        Some(i) => (&word[..=i], &word[i + 1..]),
        None => ("", word),
    };

    let dir = match dir_part
        .strip_prefix("~/")
        .or_else(|| dir_part.strip_prefix("~\\"))
    {
        Some(rest) => home?.join(rest),
        None => cwd.join(dir_part),
    };

    let leaf = fold_case(leaf);

    let mut candidates: Vec<CompletionCandidate> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if !fold_case(&name).starts_with(&leaf) {
                return None;
            }
            let is_dir = fs::metadata(e.path()).ok()?.is_dir();
            let mut replacement = format!("{dir_part}{name}");
            if is_dir {
                replacement.push('/');
            }
            match quote {
                Some((_, q)) if !is_dir => replacement.push(q),
                None if replacement.contains(' ') => {
                    replacement.insert(0, '"');
                    if !is_dir {
                        replacement.push('"');
                    }
                }
                _ => (),
            }
            let display = if is_dir { format!("{name}/") } else { name };
            Some(CompletionCandidate {
                display,
                replacement,
            })
        })
        .collect();
    candidates.sort_by(|a, b| a.display.cmp(&b.display));
    Some(CompletionResult { start, candidates })
}

/// Byte index and char of a quote (`"` or `'`) that `s` leaves unclosed.
fn open_quote(s: &str) -> Option<(usize, char)> {
    let mut open = None;
    for (i, c) in s.char_indices() {
        match open {
            Some((_, q)) if c == q => open = None,
            None if c == '"' || c == '\'' => open = Some((i, c)),
            _ => (),
        }
    }
    open
}

/// Filenames match case-insensitively where the filesystem is, as in
/// rustyline's completer.
fn fold_case(s: &str) -> String {
    if cfg!(any(windows, target_os = "macos")) {
        s.to_lowercase()
    } else {
        s.to_string()
    }
}

/// The candidates of an ambiguous completion, for listing on a second Tab.
pub fn candidate_list(result: &CompletionResult) -> String {
    let names: Vec<&str> = result
        .candidates
        .iter()
        .map(|c| c.display.as_str())
        .collect();
    names.join("   ")
}
