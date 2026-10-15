//! Source highlighting with the syntect engine already used by Markdown.
//! One lazy syntax/theme set serves all source windows. Each source or
//! plain-text window retains its laid-out text, so repainting neither
//! re-parses nor copies the file.

use std::{
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
    Color32, FontId, Galley, TextStyle, Ui,
    text::{ByteIndex, LayoutJob, LayoutSection, TextFormat},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, ThemeSet},
    parsing::{SyntaxDefinition, SyntaxSet},
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
#[derive(Clone, PartialEq)]
struct Style {
    dark: bool,
    font: FontId,
    color: Color32,
    pixels_per_point: f32,
    /// Where lines wrap: infinite for source, which scrolls sideways instead.
    wrap_width: f32,
}

#[derive(Default)]
pub(super) struct Source {
    style: Option<Style>,
    galley: Option<Arc<Galley>>,
    task: Option<Task>,
    /// Also true after a failed attempt, so a difficult file doesn't retry
    /// on every repaint. A reload or style change allows a new attempt.
    started: bool,
}

struct Task {
    rx: Receiver<Option<LayoutJob>>,
    cancelled: Arc<AtomicBool>,
}

impl Drop for Task {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

impl Source {
    pub(super) fn clear(&mut self) {
        self.style = None;
        self.galley = None;
        self.started = false;
        if let Some(task) = &self.task {
            task.cancelled.store(true, Ordering::Relaxed);
        }
    }

    /// `text` laid out in the monospace font, wrapped at `wrap_width`.
    /// Highlighted as `language`, if given, once the background task is done.
    pub(super) fn galley(
        &mut self,
        ui: &Ui,
        text: &Arc<str>,
        language: Option<&'static str>,
        wrap_width: f32,
    ) -> Arc<Galley> {
        let style = Style {
            dark: ui.visuals().dark_mode,
            font: TextStyle::Monospace.resolve(ui.style()),
            color: ui.visuals().text_color(),
            pixels_per_point: ui.ctx().pixels_per_point(),
            wrap_width,
        };
        
        if self.style.as_ref() != Some(&style) {
            self.clear();
            self.style = Some(style.clone());
        }

        if let Some(job) = self.receive() {
            self.galley = Some(ui.fonts_mut(|fonts| fonts.layout_job(job)));
        }
        if let Some(language) = language
            && !self.started
            && self.task.is_none()
        {
            self.started = true;
            
            let (tx, rx) = mpsc::channel();
            let cancelled = Arc::new(AtomicBool::new(false));
            let cancel = Arc::clone(&cancelled);
            let text = Arc::clone(text);
            let style = style.clone();
            let ctx = ui.ctx().clone();
            let viewport = ctx.viewport_id();
            
            // At most one task per window, including after rapid reloads.
            // An obsolete task must finish before the newest one starts.
            if thread::Builder::new()
                .name("source-highlighting".into())
                .spawn(move || {
                    let job = highlight(&text, language, &style, &cancel);
                    if tx.send(job).is_ok() {
                        ctx.request_repaint_of(viewport);
                    }
                })
                .is_ok()
            {
                self.task = Some(Task { rx, cancelled });
            }
        }

        // Plain text is all a plain-text file gets. Source shows it while
        // the worker runs, and keeps it after a regex error or a budget limit.
        let galley = self.galley.get_or_insert_with(|| {
            ui.fonts_mut(|fonts| {
                fonts.layout_job(LayoutJob::simple(
                    text.to_string(),
                    style.font,
                    style.color,
                    style.wrap_width,
                ))
            })
        });
        Arc::clone(galley)
    }

    fn receive(&mut self) -> Option<LayoutJob> {
        let task = self.task.as_ref()?;
        let result = match task.rx.try_recv() {
            Ok(job) => job,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => None,
        };
        
        let current = !task.cancelled.load(Ordering::Relaxed);
        self.task = None;
        result.filter(|_| current)
    }
}

fn highlight(text: &str, language: &str, style: &Style, cancel: &AtomicBool) -> Option<LayoutJob> {
    if cancel.load(Ordering::Relaxed) || text.lines().any(|line| line.len() > MAX_LINE_BYTES) {
        return None;
    }
    
    let syntax = SYNTAXES.find_syntax_by_extension(language)?;
    let theme = &THEMES.themes[if style.dark {
        "base16-ocean.dark"
    } else {
        "InspiredGitHub"
    }];
    
    let mut highlighter = HighlightLines::new(syntax, theme);
    let start = Instant::now();
    let mut job = LayoutJob {
        text: text.into(),
        ..Default::default()
    };
    job.wrap.max_width = style.wrap_width;
    let mut offset = 0;
    
    for line in LinesWithEndings::from(text) {
        if cancel.load(Ordering::Relaxed) || start.elapsed() > BUDGET {
            return None;
        }
        
        for (token, part) in highlighter.highlight_line(line, &SYNTAXES).ok()? {
            if job.sections.len() >= MAX_SECTIONS {
                return None;
            }
            
            let fg = token.foreground;
            let color = Color32::from_rgb(fg.r, fg.g, fg.b);
            job.sections.push(LayoutSection {
                leading_space: 0.0,
                byte_range: ByteIndex(offset)..ByteIndex(offset + part.len()),
                format: TextFormat {
                    font_id: style.font.clone(),
                    color,
                    italics: token.font_style.contains(FontStyle::ITALIC),
                    underline: if token.font_style.contains(FontStyle::UNDERLINE) {
                        egui::Stroke::new(1.0, color)
                    } else {
                        egui::Stroke::NONE
                    },
                    ..Default::default()
                },
            });
            offset += part.len();
        }
    }
    
    Some(job)
}
