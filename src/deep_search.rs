//! The Files panel's Deep search: the contents of every file under the
//! listed dir, searched the way `rg <query>` would — or with
//! [Options::file_names], the names of every file and folder, like
//! `rg --files` filtered by the query (but with folders too). Uses ripgrep's
//! own library crates — `ignore` for its parallel, .gitignore-aware
//! directory walk, `grep-searcher` and `grep-regex` for the search itself —
//! on background threads, so the panel keeps drawing while results come in.

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
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, sinks::Lossy};
use ignore::{DirEntry, WalkBuilder, WalkState};

/// A search stops once it has found this many matches (matching lines, or
/// files and folders in file-name mode), so searching a big tree for
/// something common can't swamp memory or the list.
pub const MAX_MATCHES: usize = 2_000;
/// Matching lines longer than this many chars (minified JS, say) are cut to
/// a window around their first match.
const MAX_LINE_CHARS: usize = 160;
/// How many chars of such a window come before its first match.
const LEAD_CHARS: usize = 24;

/// How a search matches, set by the toggles beside the Deep search box.
#[derive(Clone, Copy, Default)]
pub struct Options {
    /// The query is a regex rather than literal text.
    pub regex: bool,
    /// Match the query against file and folder names rather than file
    /// contents.
    pub file_names: bool,
}

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

/// A file with at least one match, or in file-name mode, a folder.
pub struct FileHit {
    pub path: PathBuf,
    /// Only in file-name mode.
    pub is_folder: bool,
    /// In line order. Empty in file-name mode.
    pub lines: Vec<MatchLine>,
    /// In file-name mode, byte ranges of the matches in the name (the last
    /// part of `path`, lossily converted), for highlighting.
    pub name_ranges: Vec<Range<usize>>,
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
    pub opts: Options,
    /// Files with matches, sorted by path.
    pub hits: Vec<FileHit>,
    /// `hits` flattened to the rows of the results list.
    pub rows: Vec<Row>,
    /// Matches across `hits`: matching lines, or in file-name mode, files.
    pub n_matches: usize,
    /// Why the search couldn't run, e.g. an invalid regex.
    pub error: Option<String>,
    cancel: Arc<AtomicBool>,
    /// `None` once the search threads have finished and everything they
    /// sent has been taken.
    rx: Option<Receiver<FileHit>>,
}

impl DeepSearch {
    /// Start searching the files under `root` for `query`, as `opts` says.
    /// Case-insensitive unless `query` has an uppercase letter (ripgrep's
    /// `--smart-case`). `ctx` is asked to repaint as results come in, and
    /// when the search finishes.
    pub fn start(root: PathBuf, query: &str, opts: Options, ctx: egui::Context) -> Self {
        let mut s = Self {
            root: root.clone(),
            opts,
            hits: Vec::new(),
            rows: Vec::new(),
            n_matches: 0,
            error: None,
            cancel: Arc::new(AtomicBool::new(false)),
            rx: None,
        };
        let matcher = RegexMatcherBuilder::new()
            .case_smart(true)
            .fixed_strings(!opts.regex)
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
            search(&root, &matcher, opts.file_names, &tx, &cancel, &ctx);
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
                    self.n_matches += hit.lines.len().max(1);
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
/// each file with matches (in its name, if `file_names`, else its contents)
/// to `tx`. Returns once the walk finishes, is cancelled, or has found
/// [MAX_MATCHES] matches.
fn search(
    root: &Path,
    matcher: &RegexMatcher,
    file_names: bool,
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
            // Folders only have names to match, and `root` itself (depth 0)
            // isn't a result.
            let (is_file, is_folder) = entry
                .file_type()
                .map_or((false, false), |t| (t.is_file(), t.is_dir()));
            let hit = if file_names && (is_file || (is_folder && entry.depth() > 0)) {
                name_hit(&matcher, entry, is_folder)
            } else if is_file && !file_names {
                contents_hit(&mut searcher, &matcher, entry, cancel)
            } else {
                None
            };
            let Some(mut hit) = hit else {
                return WalkState::Continue;
            };

            // Each matching line counts towards the limit, or in file-name
            // mode, each file or folder.
            let n = hit.lines.len().max(1);
            let before = found.fetch_add(n, Ordering::Relaxed);
            if before >= MAX_MATCHES {
                return WalkState::Quit;
            }
            hit.lines.truncate(MAX_MATCHES - before);
            let full = before + n >= MAX_MATCHES;
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

/// `entry` (a file, or a folder if `is_folder`) as a [FileHit] if `matcher`
/// matches its name.
fn name_hit(matcher: &RegexMatcher, entry: DirEntry, is_folder: bool) -> Option<FileHit> {
    let name = entry.file_name().to_string_lossy();
    let mut matched = false;
    let mut name_ranges = Vec::new();
    let _ = matcher.find_iter(name.as_bytes(), |m| {
        matched = true;
        let s = name.floor_char_boundary(m.start());
        let e = name.ceil_char_boundary(m.end());
        if s < e {
            name_ranges.push(s..e);
        }
        true
    });
    drop(name);
    matched.then(|| FileHit {
        path: entry.into_path(),
        is_folder,
        lines: Vec::new(),
        name_ranges,
    })
}

/// `entry` as a [FileHit] if `matcher` matches any of its lines. Reading it
/// stops early at [MAX_MATCHES] lines, or once `cancel` is set.
fn contents_hit(
    searcher: &mut Searcher,
    matcher: &RegexMatcher,
    entry: DirEntry,
    cancel: &AtomicBool,
) -> Option<FileHit> {
    let mut lines = Vec::new();
    let _ = searcher.search_path(
        matcher,
        entry.path(),
        Lossy(|line_num, line| {
            lines.push(match_line(matcher, line_num, line));
            Ok(lines.len() < MAX_MATCHES && !cancel.load(Ordering::Relaxed))
        }),
    );
    (!lines.is_empty()).then(|| FileHit {
        path: entry.into_path(),
        is_folder: false,
        lines,
        name_ranges: Vec::new(),
    })
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
