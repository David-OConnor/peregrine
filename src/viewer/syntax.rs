//! Source highlighting with the syntect engine already used by Markdown.
//! One lazy syntax/theme set serves all source windows. Each source or
//! plain-text window retains its laid-out text, so repainting neither
//! re-parses nor copies the file.
//!
//! Highlighting is incremental, as in Sublime Text and VS Code: the parser's
//! state at the start of every line is kept, so after an edit or a reload,
//! lines are highlighted again from the first changed one only until the
//! parser is back in a state it was in before. That's usually just the edited
//! line, done before the frame is drawn. Anything more (opening a block
//! comment near the top, say) continues on a background thread, while lines
//! it hasn't reached yet keep their old colors.

use std::{
    iter,
    path::Path,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};

use egui::{
    Color32, FontId, Galley, Stroke, TextStyle, Ui,
    text::{ByteIndex, LayoutJob, LayoutSection, TextFormat},
};
use syntect::{
    highlighting::{FontStyle, HighlightIterator, HighlightState, Highlighter, Theme, ThemeSet},
    parsing::{ParseState, ScopeStack, SyntaxDefinition, SyntaxSet},
    util::LinesWithEndings,
};

const TOML: &str = include_str!("../../resources/syntaxes/TOML.sublime-syntax");
const POWERSHELL: &str = include_str!("../../resources/syntaxes/PowerShell.sublime-syntax");

// Regex grammars can be expensive on minified/generated input. Such files
// stay readable as plain text. The budget is checked between complete lines,
// so multi-line comments/strings are never highlighted with a reset parser.
const MAX_LINE_BYTES: usize = 16 * 1024;
const MAX_SECTIONS: usize = 100_000;
const BUDGET: Duration = Duration::from_secs(2);
/// How long a frame may spend highlighting after a change: plenty for the
/// lines an edit changes, as a rule. The background task does the rest.
const FRAME_BUDGET: Duration = Duration::from_millis(4);

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(|| {
    let mut builder = SyntaxSet::load_defaults_newlines().into_builder();
    for source in [TOML, POWERSHELL] {
        builder.add(
            SyntaxDefinition::load_from_str(source, true, None)
                .expect("bundled syntax is valid with syntect's fancy-regex backend"),
        );
    }
    builder.build()
});
static THEMES: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

/// Deliberately limit routing to the supported file types, rather than every
/// extension syntect knows (which would take over unrelated file handlers).
pub(super) fn language(path: &Path) -> Option<&'static str> {
    Some(
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "html" | "htm" => "html",
            "svg" | "xml" => "xml",
            "rs" => "rs",
            "py" => "py",
            "js" | "mjs" | "cjs" => "js",
            "toml" => "toml",
            "json" => "json",
            "c" | "h" => "c",
            "cpp" | "cc" | "cxx" | "c++" | "hpp" | "hh" | "hxx" | "h++" | "cu" | "cuh" => "cpp",
            "go" => "go",
            "sh" | "bash" => "sh",
            "ps1" | "psm1" | "psd1" => "ps1",
            "bat" | "cmd" => "bat",
            "lock" => return lockfile_language(path),
            _ => return None,
        },
    )
}

/// `.lock` is no single format: it depends on the tool that wrote it. Known
/// TOML and JSON lockfiles are highlighted; the rest (`yarn.lock`,
/// `Gemfile.lock`, ...) are shown as plain text (see `TEXT_EXTENSIONS`).
fn lockfile_language(path: &Path) -> Option<&'static str> {
    Some(
        match path.file_name()?.to_str()?.to_ascii_lowercase().as_str() {
            "cargo.lock" | "poetry.lock" | "uv.lock" | "pdm.lock" | "gopkg.lock" => "toml",
            "pipfile.lock" | "composer.lock" | "flake.lock" | "deno.lock" => "json",
            _ => return None,
        },
    )
}

/// Everything, besides file contents, that changes the cached text layout.
/// Of these, only `dark` (the theme) changes the colors.
#[derive(Clone, PartialEq)]
struct Style {
    dark: bool,
    font: FontId,
    color: Color32,
    pixels_per_point: f32,
    /// Where lines wrap: infinite for source, which scrolls sideways instead.
    wrap_width: f32,
}

impl Style {
    fn of(ui: &Ui, wrap_width: f32) -> Self {
        Self {
            dark: ui.visuals().dark_mode,
            font: TextStyle::Monospace.resolve(ui.style()),
            color: ui.visuals().text_color(),
            pixels_per_point: ui.ctx().pixels_per_point(),
            wrap_width,
        }
    }
}

#[derive(Default)]
pub(super) struct Source {
    style: Option<Style>,
    language: Option<&'static str>,
    /// The text `galley` shows.
    text: Option<Arc<str>>,
    galley: Option<Arc<Galley>>,
    /// The colors of `text`, kept across its edits and reloads.
    highlights: Option<Highlights>,
    task: Option<Task>,
    /// Set after a failed attempt, so a difficult file doesn't retry on every
    /// edit. A theme change allows a new attempt.
    gave_up: bool,
}

/// Highlighting the rest of a text, on a background thread.
struct Task {
    /// The highlights when finished, or `None` if the task gave up.
    rx: Receiver<Option<Highlights>>,
    cancelled: Arc<AtomicBool>,
}

impl Drop for Task {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

impl Source {
    /// `text` laid out in the monospace font, wrapped at `wrap_width`.
    /// Highlighted as `language`, if given: at once where that's quick,
    /// otherwise once the background task is done. A changed `text` keeps
    /// the colors of the lines it didn't change meanwhile.
    pub(super) fn galley(
        &mut self,
        ui: &Ui,
        text: &Arc<str>,
        language: Option<&'static str>,
        wrap_width: f32,
    ) -> Arc<Galley> {
        let style = Style::of(ui, wrap_width);
        if self.style.as_ref().is_none_or(|old| old.dark != style.dark) || self.language != language
        {
            self.language = language;
            self.highlights = None;
            self.gave_up = false;
            self.cancel();
        }
        if self.style.as_ref() != Some(&style) {
            self.style = Some(style.clone());
            self.galley = None;
        }
        self.receive();

        let mut changed = false;
        if self.text.as_ref().is_none_or(|old| !Arc::ptr_eq(old, text)) {
            self.text = Some(Arc::clone(text));
            self.galley = None;
            self.cancel();
            if let Some(highlights) = &mut self.highlights {
                highlights.edit(text);
                changed = true;
            }
        }
        if let Some(language) = language
            && !self.gave_up
        {
            if self.highlights.is_none() {
                self.highlights = Highlights::new(language, style.dark, text);
                self.gave_up = self.highlights.is_none();
                changed = true;
            }
            self.highlight(ui, changed);
        }

        // Plain text is all a plain-text file gets. Source shows it until
        // highlighted, and keeps it after a regex error or a budget limit.
        let galley = self.galley.get_or_insert_with(|| {
            let job = match &self.highlights {
                Some(highlights) => highlights.job(&style),
                None => plain(text, &style),
            };
            ui.fonts_mut(|fonts| fonts.layout_job(job))
        });
        Arc::clone(galley)
    }

    /// Bring the highlights up to date: after a change, as far as this frame
    /// allows, and then the rest on a background thread.
    fn highlight(&mut self, ui: &Ui, changed: bool) {
        let Some(highlights) = &mut self.highlights else {
            return;
        };
        if changed {
            let deadline = Instant::now() + FRAME_BUDGET;
            if highlights.run(|| Instant::now() < deadline).is_err() {
                self.give_up();
                return;
            }
        }
        // At most one task per window, including after rapid edits. An
        // obsolete task must finish before the newest one starts.
        if highlights.done() || self.task.is_some() {
            return;
        }

        let mut background = highlights.clone();
        let (tx, rx) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel = Arc::clone(&cancelled);
        let ctx = ui.ctx().clone();
        let viewport = ctx.viewport_id();
        if thread::Builder::new()
            .name("source-highlighting".into())
            .spawn(move || {
                let deadline = Instant::now() + BUDGET;
                let done =
                    background.run(|| !cancel.load(Ordering::Relaxed) && Instant::now() < deadline);
                if !cancel.load(Ordering::Relaxed) {
                    let _ = tx.send((done == Ok(true)).then_some(background));
                }
                // First, so the repaint sees the task has ended.
                drop(tx);
                ctx.request_repaint_of(viewport);
            })
            .is_ok()
        {
            self.task = Some(Task { rx, cancelled });
        }
    }

    /// Take the background task's highlights, once it's done.
    fn receive(&mut self) {
        let Some(task) = &self.task else {
            return;
        };
        let result = match task.rx.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            // Cancelled, or crashed.
            Err(TryRecvError::Disconnected) => None,
        };
        // Edits cancel the task, so if it wasn't, its text is still current.
        let current = !task.cancelled.load(Ordering::Relaxed);
        self.task = None;
        if current {
            match result {
                Some(highlights) => {
                    self.highlights = Some(highlights);
                    self.galley = None;
                }
                None => self.give_up(),
            }
        }
    }

    /// Mark the background task's work obsolete. It's kept until it ends.
    fn cancel(&self) {
        if let Some(task) = &self.task {
            task.cancelled.store(true, Ordering::Relaxed);
        }
    }

    fn give_up(&mut self) {
        self.cancel();
        self.gave_up = true;
        self.highlights = None;
        self.galley = None;
    }
}

/// The parser's state between two lines: where it is in the grammar, and
/// the scopes (and so the colors) open there.
type State = (ParseState, HighlightState);

/// A text's colors, line by line, kept up to date as it's edited.
#[derive(Clone)]
struct Highlights {
    theme: &'static Theme,
    text: Arc<str>,
    /// One for each line of `text`.
    lines: Vec<Line>,
    /// The state at the start of each line, and then at the end of the text.
    /// `None` where it isn't known yet.
    states: Vec<Option<Arc<State>>>,
    /// The first line that may need highlighting. The lines before it, and
    /// its state, are up to date.
    frontier: usize,
    /// Where that line starts in `text`.
    offset: usize,
    /// How many tokens `lines` have, all together.
    tokens: usize,
    /// How many lines are longer than [MAX_LINE_BYTES]. While there are any,
    /// the text shows plain.
    long_lines: usize,
}

#[derive(Clone)]
struct Line {
    /// The line's colors. After the frontier, they may be from before an
    /// edit somewhere above, standing in until the line is highlighted again;
    /// or none, for a line not highlighted yet.
    tokens: Arc<[Token]>,
    /// Whether `tokens`, and the next line's state, were found from this
    /// line's state. Then they're up to date as soon as that state is.
    valid: bool,
}

/// A run of text in one color.
#[derive(Clone, Copy)]
struct Token {
    len: usize,
    color: Color32,
    font_style: FontStyle,
}

impl Highlights {
    /// `text`, to be highlighted as `language`.
    fn new(language: &str, dark: bool, text: &Arc<str>) -> Option<Self> {
        let syntax = SYNTAXES.find_syntax_by_extension(language)?;
        let theme = &THEMES.themes[if dark {
            "base16-ocean.dark"
        } else {
            "InspiredGitHub"
        }];
        let start = (
            ParseState::new(syntax),
            HighlightState::new(&Highlighter::new(theme), ScopeStack::new()),
        );
        let mut highlights = Self {
            theme,
            text: Arc::from(""),
            lines: Vec::new(),
            states: vec![Some(Arc::new(start))],
            frontier: 0,
            offset: 0,
            tokens: 0,
            long_lines: 0,
        };
        highlights.edit(text);
        Some(highlights)
    }

    fn done(&self) -> bool {
        self.long_lines > 0 || self.frontier == self.lines.len()
    }

    /// Follow `self.text` becoming `text`. Lines the change didn't touch
    /// keep their colors, for now.
    fn edit(&mut self, text: &Arc<str>) {
        let (old, new) = (self.text.as_bytes(), text.as_bytes());
        let prefix = common_prefix(old, new);
        if prefix == old.len() && prefix == new.len() {
            self.text = Arc::clone(text);
            return;
        }
        let suffix = common_suffix(&old[prefix..], &new[prefix..]);

        // The changed lines: from the start of the one the change starts in,
        // to the first line start after it that is one in both texts.
        let start = old[..prefix]
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |i| i + 1);
        let new_end = |end: usize| end + new.len() - old.len();
        let mut end = old.len() - suffix;
        if !(line_start(old, end) && line_start(new, new_end(end))) {
            end = old[end..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(old.len(), |i| end + i + 1);
        }
        let first = old[..start].iter().filter(|&&b| b == b'\n').count();
        let mut removed = 0;
        for line in LinesWithEndings::from(&self.text[start..end]) {
            removed += 1;
            self.long_lines -= usize::from(line.len() > MAX_LINE_BYTES);
        }
        let mut added = 0;
        for line in LinesWithEndings::from(&text[start..new_end(end)]) {
            added += 1;
            self.long_lines += usize::from(line.len() > MAX_LINE_BYTES);
        }
        let last = first + removed;

        let blank = Line {
            tokens: Arc::from([]),
            valid: false,
        };
        for line in self.lines.splice(first..last, iter::repeat_n(blank, added)) {
            self.tokens -= line.tokens.len();
        }
        // The states inside the changed lines are unknown now. The one after
        // them stays, to tell when highlighting is back in step after them.
        let after = self.states[last].clone();
        if added == 0 {
            self.states.drain(first + 1..=last);
            // The line after now starts where the changed lines did.
            if self.states[first] != after
                && let Some(line) = self.lines.get_mut(first)
            {
                line.valid = false;
            }
        } else {
            self.states.splice(
                first + 1..=last,
                iter::repeat_n(None, added - 1).chain([after]),
            );
        }
        if self.frontier > first {
            self.frontier = first;
            self.offset = start;
        }
        self.text = Arc::clone(text);
        debug_assert_eq!(self.states.len(), self.lines.len() + 1);
    }

    /// Highlight lines from the frontier on, while `more()`. `Ok(true)` once
    /// all are up to date; `Err` for a regex error or too many sections.
    fn run(&mut self, mut more: impl FnMut() -> bool) -> Result<bool, ()> {
        if self.long_lines > 0 {
            return Ok(true);
        }
        let highlighter = Highlighter::new(self.theme);
        while self.frontier < self.lines.len() {
            if self.lines[self.frontier].valid {
                // Found from the frontier's state, so up to date already.
                self.offset += line_len(&self.text[self.offset..]);
                self.frontier += 1;
                continue;
            }
            if !more() {
                return Ok(false);
            }
            let i = self.frontier;
            let line = &self.text[self.offset..self.offset + line_len(&self.text[self.offset..])];
            let state = self.states[i].clone().ok_or(())?;
            let (mut parse, mut highlight) = State::clone(&state);
            let ops = parse.parse_line(line, &SYNTAXES).map_err(drop)?;

            let mut tokens: Vec<Token> = Vec::new();
            for (style, part) in HighlightIterator::new(&mut highlight, &ops, line, &highlighter) {
                let fg = style.foreground;
                let color = Color32::from_rgb(fg.r, fg.g, fg.b);
                match tokens.last_mut() {
                    Some(last) if last.color == color && last.font_style == style.font_style => {
                        last.len += part.len();
                    }
                    _ => tokens.push(Token {
                        len: part.len(),
                        color,
                        font_style: style.font_style,
                    }),
                }
            }
            self.tokens = self.tokens + tokens.len() - self.lines[i].tokens.len();
            if self.tokens > MAX_SECTIONS {
                return Err(());
            }
            self.lines[i] = Line {
                tokens: tokens.into(),
                valid: true,
            };
            self.frontier += 1;
            self.offset += line.len();

            // The same state as before means highlighting is back in step:
            // the following lines found from it are up to date already.
            let end = (parse, highlight);
            if self.states[i + 1].as_deref() != Some(&end) {
                // Most lines end in the state they started in: share it.
                self.states[i + 1] = Some(if *state == end { state } else { Arc::new(end) });
                if let Some(next) = self.lines.get_mut(i + 1) {
                    next.valid = false;
                }
            }
        }
        Ok(true)
    }

    /// The text laid out in `style`, in the colors it has so far. Lines not
    /// highlighted yet show plain.
    fn job(&self, style: &Style) -> LayoutJob {
        if self.long_lines > 0 {
            return plain(&self.text, style);
        }
        let mut job = LayoutJob {
            text: self.text.to_string(),
            keep_trailing_whitespace: true,
            ..Default::default()
        };
        job.wrap.max_width = style.wrap_width;
        job.sections.reserve(self.tokens);
        let mut offset = 0;
        let mut look = None;
        for (text, line) in LinesWithEndings::from(&self.text).zip(&self.lines) {
            let plain = [Token {
                len: text.len(),
                color: style.color,
                font_style: FontStyle::empty(),
            }];
            let tokens = if line.tokens.iter().map(|token| token.len).sum::<usize>() == text.len() {
                &line.tokens[..]
            } else {
                &plain[..]
            };
            for token in tokens {
                let end = ByteIndex(offset + token.len);
                // Neighbors in the same color share a section.
                match job.sections.last_mut() {
                    Some(last) if look == Some((token.color, token.font_style)) => {
                        last.byte_range.end = end;
                    }
                    _ => {
                        look = Some((token.color, token.font_style));
                        job.sections.push(LayoutSection {
                            leading_space: 0.0,
                            byte_range: ByteIndex(offset)..end,
                            format: TextFormat {
                                font_id: style.font.clone(),
                                color: token.color,
                                italics: token.font_style.contains(FontStyle::ITALIC),
                                underline: if token.font_style.contains(FontStyle::UNDERLINE) {
                                    Stroke::new(1.0, token.color)
                                } else {
                                    Stroke::NONE
                                },
                                ..Default::default()
                            },
                        });
                    }
                }
                offset = end.0;
            }
        }
        job
    }
}

fn plain(text: &str, style: &Style) -> LayoutJob {
    let mut job = LayoutJob::simple(
        text.to_owned(),
        style.font.clone(),
        style.color,
        style.wrap_width,
    );
    job.keep_trailing_whitespace = true;
    job
}

/// The length of the line `text` starts with, with its newline.
fn line_len(text: &str) -> usize {
    text.find('\n').map_or(text.len(), |i| i + 1)
}

fn line_start(text: &[u8], at: usize) -> bool {
    at == 0 || text[at - 1] == b'\n'
}

/// Compared a chunk at a time first, which is as fast as `memcmp`.
const CHUNK: usize = 64;

fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    let len = a.len().min(b.len());
    let mut n = 0;
    while n + CHUNK <= len && a[n..n + CHUNK] == b[n..n + CHUNK] {
        n += CHUNK;
    }
    while n < len && a[n] == b[n] {
        n += 1;
    }
    n
}

fn common_suffix(a: &[u8], b: &[u8]) -> usize {
    let len = a.len().min(b.len());
    let (a_end, b_end) = (a.len(), b.len());
    let mut n = 0;
    while n + CHUNK <= len && a[a_end - n - CHUNK..a_end - n] == b[b_end - n - CHUNK..b_end - n] {
        n += CHUNK;
    }
    while n < len && a[a_end - n - 1] == b[b_end - n - 1] {
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use egui::{Context, RawInput};

    use super::*;

    fn style() -> Style {
        Style {
            dark: true,
            font: FontId::monospace(13.),
            color: Color32::WHITE,
            pixels_per_point: 1.,
            wrap_width: f32::INFINITY,
        }
    }

    fn highlighted(language: &str, text: &str) -> Highlights {
        let mut highlights = Highlights::new(language, true, &Arc::from(text)).unwrap();
        assert_eq!(highlights.run(|| true), Ok(true));
        highlights
    }

    /// Each byte's color.
    fn colors(job: &LayoutJob) -> Vec<Color32> {
        let mut colors = Vec::new();
        for section in &job.sections {
            assert_eq!(section.byte_range.start.0, colors.len());
            colors.resize(section.byte_range.end.0, section.format.color);
        }
        assert_eq!(colors.len(), job.text.len());
        colors
    }

    /// `run`'s `more` for highlighting at most `lines` lines.
    fn lines(mut lines: usize) -> impl FnMut() -> bool {
        move || {
            let more = lines > 0;
            lines = lines.saturating_sub(1);
            more
        }
    }

    #[test]
    fn editor_languages_keep_their_text_and_have_syntax_colors() {
        for (language, text) in [
            ("rs", "fn main() { let answer = 42; }\n"),
            ("py", "def example():\n    return 'hello'\n"),
            ("js", "const answer = 42; // comment\n"),
            ("html", "<html><body class=\"test\">Hello</body></html>\n"),
            ("md", "# Heading\n\nSome **bold** text.\n"),
        ] {
            let job = highlighted(language, text).job(&style());
            assert_eq!(job.text, text);
            let first = job.sections[0].format.color;
            assert!(
                job.sections
                    .iter()
                    .any(|section| section.format.color != first),
                "{language} should be highlighted"
            );
        }
    }

    #[test]
    fn edits_end_up_highlighted_as_if_from_scratch() {
        let base = "fn main() {\n    let s = \"text\";\n    // note\n}\n\nstruct A;\n";
        let mut highlights = highlighted("rs", "");
        for (text, partly) in [
            (base, 0),
            // Into and out of a block comment over the rest, typed faster
            // than it's highlighted.
            (
                "/* fn main() {\n    let s = \"text\";\n    // note\n}\n\nstruct A;\n",
                1,
            ),
            (
                "/* fn main() {\n    let s = \"text\";*/\n    // note\n}\n\nstruct A;\n",
                1,
            ),
            (
                "fn main() {\n    let s = \"text\";*/\n    // note\n}\n\nstruct A;\n",
                0,
            ),
            (
                "/* fn main() {\n    let s = \"text\";\n    // note\n}\n\nstruct A;\n",
                0,
            ),
            (base, 2),
            // A string split over lines, then joined again.
            (
                "fn main() {\n    let s = \"te\nxt\";\n    // note\n}\n\nstruct A;\n",
                0,
            ),
            (base, 0),
            // Lines added and removed, at the start, middle and end.
            (
                "use std;\nfn main() {\n    let s = \"text\";\n}\n\nstruct A;\n",
                0,
            ),
            (
                "use std;\nfn main() {\n    let s = \"雪 🦅\";\n}\n\nstruct A;\nstruct B;",
                0,
            ),
            (
                "use std;\nfn main() {\n    let s = \"雪 🦅\";\n}\n\nstruct A;\nstruct B;\n",
                1,
            ),
            ("use std;\nfn main() {\n}\n\nstruct A;\nstruct B;\n", 0),
            ("use std;\nfn main() {\n}\n\nstruct A;\n", 0),
            ("\n\nuse std;\nfn main() {\n}\n\nstruct A;\n", 0),
            ("fn main() {\n}\n\nstruct A;\n", 0),
            ("", 0),
            ("struct A;", 0),
        ] {
            highlights.edit(&Arc::from(text));
            assert_eq!(highlights.lines.len(), LinesWithEndings::from(text).count());
            // Stopped part way, and edited again, as in typing during the
            // background task.
            if partly > 0 {
                highlights.run(lines(partly)).unwrap();
                continue;
            }
            assert_eq!(highlights.run(|| true), Ok(true));
            assert_eq!(
                colors(&highlights.job(&style())),
                colors(&highlighted("rs", text).job(&style())),
                "{text:?}"
            );
        }
    }

    #[test]
    fn an_edit_highlights_only_the_lines_it_changes() {
        let text = "fn f() {\n    let x = 1;\n}\n".repeat(200);
        let mut highlights = highlighted("rs", &text);
        let mut edit = |text: &str| {
            highlights.edit(&Arc::from(text));
            highlights.run(lines(1))
        };
        let text = text.replacen("let x = 1", "let y = 2", 1);
        assert_eq!(edit(&text), Ok(true));
        let text = text.replacen("{\n    let", "{\n\n    let", 1);
        assert_eq!(edit(&text), Ok(true));
        let text = text.replacen("{\n\n    let", "{\n    let", 1);
        assert_eq!(edit(&text), Ok(true));
        // A new block comment changes every line after it.
        assert_eq!(edit(&format!("/*{text}")), Ok(false));
    }

    #[test]
    fn lines_not_highlighted_again_yet_keep_their_colors() {
        let text = "fn main() {\n    let answer = 42;\n}\n";
        let mut highlights = highlighted("rs", text);
        let before = colors(&highlights.job(&style()));
        // Opens a comment over the rest, which no line has been told yet.
        highlights.edit(&Arc::from(format!("/*{text}").as_str()));
        let after = colors(&highlights.job(&style()));
        assert!(
            after[..2 + "fn main() {\n".len()]
                .iter()
                .all(|&c| c == Color32::WHITE)
        );
        assert_eq!(
            after[2 + "fn main() {\n".len()..],
            before["fn main() {\n".len()..]
        );
    }

    #[test]
    fn long_lines_show_plain_until_removed() {
        let long = format!("let x = \"{}\";\n", "a".repeat(MAX_LINE_BYTES));
        let mut highlights = highlighted("rs", &format!("fn f() {{}}\n{long}"));
        let plain = |highlights: &Highlights| {
            let job = highlights.job(&style());
            job.sections
                .iter()
                .all(|s| s.format.color == Color32::WHITE)
        };
        assert!(plain(&highlights));
        highlights.edit(&Arc::from("fn f() {}\n"));
        assert!(!highlights.done());
        assert_eq!(highlights.run(|| true), Ok(true));
        assert!(!plain(&highlights));
    }

    #[test]
    fn editing_keeps_the_text_colored_in_every_frame() {
        let ctx = Context::default();
        let mut source = Source::default();
        // The frame's colors, and the text's colors highlighted from scratch.
        let frame = |source: &mut Source, text: &Arc<str>| {
            let mut result = None;
            ctx.run_ui(RawInput::default(), |ui| {
                let galley = source.galley(ui, text, Some("rs"), f32::INFINITY);
                let style = Style::of(ui, f32::INFINITY);
                let mut highlights = Highlights::new("rs", style.dark, text).unwrap();
                highlights.run(|| true).unwrap();
                result = Some((colors(&galley.job), colors(&highlights.job(&style))));
            })
            .drop_without_applying_deltas();
            result.unwrap()
        };
        let text = Arc::from("fn main() {\n    let answer = 42;\n}\n");
        frame(&mut source, &text);
        let start = Instant::now();
        while source.task.is_some() || !source.highlights.as_ref().unwrap().done() {
            assert!(start.elapsed() < Duration::from_secs(60));
            thread::sleep(Duration::from_millis(10));
            frame(&mut source, &text);
        }
        for edited in [
            "fn main() {\n    let answer = 420;\n}\n",
            "fn main() {\n    let answer = 420; // done\n}\n",
            "fn main() {\n}\n",
        ] {
            let (shown, expected) = frame(&mut source, &Arc::from(edited));
            assert_eq!(shown, expected, "{edited:?}");
        }
    }
}
