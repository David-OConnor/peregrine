//! The Files panel's Deep search: the contents of every file under the
//! listed dir, searched the way `rg <query>` would. Uses ripgrep's own
//! library crates — `ignore` for its parallel, .gitignore-aware directory
//! walk, `grep-searcher` and `grep-regex` for the search itself — on
//! background threads, so the panel keeps drawing while results come in.

use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    thread,
};

use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, SearcherBuilder, sinks::Lossy};
use ignore::{WalkBuilder, WalkState};

/// A search stops once it has found this many matching lines, so searching
/// a big tree for something common can't swamp memory or the list.
pub const MAX_MATCHES: usize = 2_000;
/// Matching lines longer than this many chars (minified JS, say) are cut to
/// a window around their first match.
const MAX_LINE_CHARS: usize = 160;
/// How many chars of such a window come before its first match.
const LEAD_CHARS: usize = 24;

/// One matching line, ready to draw.
pub struct MatchLine {
    /// 1-based.
    pub line_num: u64,
    /// The line without its indentation or trailing space, cut to
    /// [MAX_LINE_CHARS] (with `…` marking the cuts) if it's long.
    pub text: String,
    /// Byte ranges in `text` of the matches, in order, for highlighting.
    pub ranges: Vec<Range<usize>>,
}

/// A file with at least one match.
pub struct FileHit {
    pub path: PathBuf,
    /// In line order.
    pub lines: Vec<MatchLine>,
}

/// A row of the results list: a file, or one of its matching lines (indices
/// into [DeepSearch::hits] and that hit's `lines`).
#[derive(Clone, Copy)]
pub enum Row {
    File(usize),
    Line(usize, usize),
}

/// A search started by [DeepSearch::start]. It runs on background threads
/// until [DeepSearch::done]; [DeepSearch::poll] collects what they've found
/// so far. Dropping it stops them.
pub struct DeepSearch {
    /// The dir searched.
    pub root: PathBuf,
    /// Files with matches, sorted by path.
    pub hits: Vec<FileHit>,
    /// `hits` flattened to the rows of the results list.
    pub rows: Vec<Row>,
    /// Matching lines across `hits`.
    pub n_matches: usize,
    /// Why the search couldn't run, e.g. an invalid regex.
    pub error: Option<String>,
    cancel: Arc<AtomicBool>,
    /// `None` once the search threads have finished and everything they
    /// sent has been taken.
    rx: Option<Receiver<FileHit>>,
}

impl DeepSearch {
    /// Start searching the files under `root` for `query`: literal text, or
    /// a regex if `regex`. Case-insensitive unless `query` has an uppercase
    /// letter (ripgrep's `--smart-case`). `ctx` is asked to repaint as
    /// results come in, and when the search finishes.
    pub fn start(root: PathBuf, query: &str, regex: bool, ctx: egui::Context) -> Self {
        let mut s = Self {
            root: root.clone(),
            hits: Vec::new(),
            rows: Vec::new(),
            n_matches: 0,
            error: None,
            cancel: Arc::new(AtomicBool::new(false)),
            rx: None,
        };
        let matcher = RegexMatcherBuilder::new()
            .case_smart(true)
            .fixed_strings(!regex)
            .line_terminator(Some(b'\n'))
            .build(query);
        let matcher = match matcher {
            Ok(m) => m,
            Err(e) => {
                s.error = Some(e.to_string());
                return s;
            }
        };

        let (tx, rx) = mpsc::channel();
        s.rx = Some(rx);
        let cancel = s.cancel.clone();
        thread::spawn(move || {
            search(&root, &matcher, &tx, &cancel, &ctx);
            // Dropping the last sender is what tells `poll` it's over.
            drop(tx);
            ctx.request_repaint();
        });
        s
    }

    /// Whether the search has finished (or never started, on an error).
    pub fn done(&self) -> bool {
        self.rx.is_none()
    }

    /// Take in whatever the search threads have found since the last call.
    pub fn poll(&mut self) {
        let Some(rx) = &self.rx else {
            return;
        };
        let mut changed = false;
        loop {
            match rx.try_recv() {
                Ok(hit) => {
                    // Kept sorted as they arrive, so rows don't jump around
                    // once the search finishes.
                    self.n_matches += hit.lines.len();
                    let i = self.hits.partition_point(|h| h.path < hit.path);
                    self.hits.insert(i, hit);
                    changed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.rx = None;
                    break;
                }
            }
        }
        if changed {
            self.rows.clear();
            for (i, hit) in self.hits.iter().enumerate() {
                self.rows.push(Row::File(i));
                self.rows
                    .extend((0..hit.lines.len()).map(|j| Row::Line(i, j)));
            }
        }
    }

    /// Whether `path` is one of the files found.
    pub fn has_file(&self, path: &Path) -> bool {
        self.hits.iter().any(|h| h.path == path)
    }
}

impl Drop for DeepSearch {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Walk `root` as ripgrep does — in parallel, skipping hidden entries and
/// whatever `.gitignore`, `.ignore` and `.rgignore` files exclude — and send
/// each file with matches to `tx`. Returns once the walk finishes, is
/// cancelled, or has found [MAX_MATCHES] lines.
fn search(
    root: &Path,
    matcher: &RegexMatcher,
    tx: &Sender<FileHit>,
    cancel: &AtomicBool,
    ctx: &egui::Context,
) {
    let found = AtomicUsize::new(0);
    let mut walk = WalkBuilder::new(root);
    walk.add_custom_ignore_filename(".rgignore");

    walk.build_parallel().run(|| {
        // One of each per walker thread.
        let (matcher, tx, found) = (matcher.clone(), tx.clone(), &found);
        let mut searcher = SearcherBuilder::new()
            .binary_detection(BinaryDetection::quit(b'\0'))
            .line_number(true)
            .build();

        Box::new(move |entry| {
            if cancel.load(Ordering::Relaxed) {
                return WalkState::Quit;
            }
            // Unreadable dirs and files are skipped, as ripgrep does (it
            // just reports them on stderr).
            let Ok(entry) = entry else {
                return WalkState::Continue;
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                return WalkState::Continue;
            }

            let mut lines = Vec::new();
            let _ = searcher.search_path(
                &matcher,
                entry.path(),
                Lossy(|line_num, line| {
                    lines.push(match_line(&matcher, line_num, line));
                    Ok(lines.len() < MAX_MATCHES && !cancel.load(Ordering::Relaxed))
                }),
            );
            if lines.is_empty() {
                return WalkState::Continue;
            }

            let before = found.fetch_add(lines.len(), Ordering::Relaxed);
            lines.truncate(MAX_MATCHES.saturating_sub(before));
            let full = before + lines.len() >= MAX_MATCHES;
            if lines.is_empty() {
                return WalkState::Quit;
            }
            let hit = FileHit {
                path: entry.into_path(),
                lines,
            };
            // A send fails once the search has been dropped.
            if tx.send(hit).is_err() {
                return WalkState::Quit;
            }
            ctx.request_repaint();
            if full {
                WalkState::Quit
            } else {
                WalkState::Continue
            }
        })
    });
}

/// `line` (number `line_num`) as a [MatchLine], with `matcher`'s matches in
/// it located for highlighting.
fn match_line(matcher: &RegexMatcher, line_num: u64, line: &str) -> MatchLine {
    let line = line.trim_end_matches(['\n', '\r']);
    let mut found = Vec::new();
    let _ = matcher.find_iter(line.as_bytes(), |m| {
        found.push(m.start()..m.end());
        true
    });

    // The part shown: no indentation or trailing space, and for a long line,
    // a window starting a little before its first match.
    let mut start = line.len() - line.trim_start().len();
    let mut end = line.trim_end().len().max(start);
    let trimmed = start..end;
    if line[start..end].chars().count() > MAX_LINE_CHARS {
        let first = found.first().map_or(start, |r| r.start.clamp(start, end));
        let first = line.floor_char_boundary(first);
        start = line[start..first]
            .char_indices()
            .rev()
            .take(LEAD_CHARS)
            .last()
            .map_or(first, |(i, _)| start + i);
        end = line[start..end]
            .char_indices()
            .nth(MAX_LINE_CHARS)
            .map_or(end, |(i, _)| start + i);
    }

    let prefix = if start > trimmed.start { "…" } else { "" };
    let suffix = if end < trimmed.end { "…" } else { "" };
    let text = format!("{prefix}{}{suffix}", &line[start..end]);
    // Each match's part within the window, shifted to `text`.
    let ranges = found
        .into_iter()
        .filter_map(|r| {
            let s = line.floor_char_boundary(r.start.clamp(start, end));
            let e = line.ceil_char_boundary(r.end.clamp(start, end));
            (s < e).then(|| s - start + prefix.len()..e - start + prefix.len())
        })
        .collect();

    MatchLine {
        line_num,
        text,
        ranges,
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, time::Duration};

    use super::*;

    fn matcher(query: &str) -> RegexMatcher {
        RegexMatcherBuilder::new()
            .case_smart(true)
            .fixed_strings(true)
            .build(query)
            .unwrap()
    }

    #[test]
    fn trims_and_highlights() {
        let m = match_line(&matcher("foo"), 3, "    let foo = Foo::new(); \r\n");
        assert_eq!(m.line_num, 3);
        assert_eq!(m.text, "let foo = Foo::new();");
        let hl: Vec<_> = m.ranges.iter().map(|r| &m.text[r.clone()]).collect();
        assert_eq!(hl, ["foo", "Foo"]);
    }

    #[test]
    fn cuts_long_lines_around_the_match() {
        let line = format!("{}needle{}", "a".repeat(500), "b".repeat(500));
        let m = match_line(&matcher("needle"), 1, &line);
        assert!(m.text.starts_with('…') && m.text.ends_with('…'));
        // The window plus the two ellipses.
        assert_eq!(m.text.chars().count(), MAX_LINE_CHARS + 2);
        assert_eq!(m.ranges.len(), 1);
        assert_eq!(&m.text[m.ranges[0].clone()], "needle");
        assert_eq!(m.text[..m.ranges[0].start].chars().count(), LEAD_CHARS + 1);
    }

    #[test]
    fn searches_a_tree() {
        let dir = std::env::temp_dir().join(format!("shell_gui_deep_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("a.txt"), "one\nNeedle two\nthree\n").unwrap();
        fs::write(dir.join("sub/b.rs"), "// needle\nfn main() {}\n").unwrap();
        fs::write(dir.join("c.txt"), "nothing here\n").unwrap();

        // Each file with matches, relative to `dir`, and its first match's
        // line number.
        let run = |query: &str| {
            let mut s = DeepSearch::start(dir.clone(), query, false, egui::Context::default());
            for _ in 0..500 {
                s.poll();
                if s.done() {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            assert!(s.done() && s.error.is_none());
            let found: Vec<_> = s
                .hits
                .iter()
                .map(|h| {
                    (
                        h.path.strip_prefix(&dir).unwrap().to_path_buf(),
                        h.lines[0].line_num,
                    )
                })
                .collect();
            (found, s.rows.len())
        };

        let (found, rows) = run("needle");
        assert_eq!(
            found,
            [
                (PathBuf::from("a.txt"), 2),
                (Path::new("sub").join("b.rs"), 1)
            ]
        );
        assert_eq!(rows, 4);
        // Smart case: an uppercase letter makes it case-sensitive.
        assert_eq!(run("Needle").0, [(PathBuf::from("a.txt"), 2)]);

        let s = DeepSearch::start(dir.clone(), "(unclosed", true, egui::Context::default());
        assert!(s.done() && s.error.is_some());

        fs::remove_dir_all(&dir).unwrap();
    }
}
