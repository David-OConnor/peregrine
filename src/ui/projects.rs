//! The Projects window, from the project bar's Git group: the repos in the
//! remote workspace the active repo was cloned from (see `workspace`), each
//! with a button to clone it beside that repo, or pull it where it's already
//! there.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    thread::{self, JoinHandle},
};

use egui::{Button, Context, Label, RichText, ScrollArea, TextStyle, Ui, Window, vec2};

use crate::{
    project, render_with_tilde,
    state::State,
    submit_command,
    workspace::{Listing, Page, RemoteRepo},
};

/// A page, and the listing it's from.
type Fetched = Result<(Arc<Listing>, Page), String>;

/// Open while `repo` is set.
#[derive(Default)]
pub struct ProjectsWindow {
    /// The repo whose workspace it lists. Its parent is where repos are
    /// cloned.
    repo: Option<PathBuf>,
    /// Found with the first page.
    listing: Option<Arc<Listing>>,
    page: Option<Page>,
    /// Which of `page`'s repos have a dir beside `repo`, to pull rather than
    /// clone.
    exists: Vec<bool>,
    error: Option<String>,
    fetch: Option<JoinHandle<Fetched>>,
    /// The active tab had no command running last pass.
    was_idle: bool,
}

impl ProjectsWindow {
    /// The repo it's open for.
    pub fn repo(&self) -> Option<&Path> {
        self.repo.as_deref()
    }

    /// Open it for the repo at `root`, or close it if it's open for that one.
    pub fn toggle(&mut self, root: PathBuf, ctx: &Context) {
        let reopen = self.repo.as_ref() != Some(&root);
        *self = Self::default();

        if reopen {
            self.repo = Some(root);
            self.fetch(1, ctx);
        }
    }

    /// Fetch page `n` on a worker thread, finding the listing first if need
    /// be.
    fn fetch(&mut self, n: u32, ctx: &Context) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let listing = self.listing.clone();
        let ctx = ctx.clone();

        self.error = None;
        self.fetch = Some(thread::spawn(move || {
            let result = (|| {
                let listing = match listing {
                    Some(l) => l,
                    None => Arc::new(Listing::find(&repo)?),
                };
                let page = listing.page(n)?;
                Ok((listing, page))
            })();
            ctx.request_repaint();
            result
        }));
    }

    /// Take in a finished fetch.
    fn pump(&mut self) {
        if !self.fetch.as_ref().is_some_and(JoinHandle::is_finished) {
            return;
        }

        let result = self.fetch.take().unwrap().join();
        match result.unwrap_or_else(|_| Err("Listing them crashed.".to_owned())) {
            Ok((listing, page)) => {
                self.listing = Some(listing);
                self.page = Some(page);
                self.recheck();
            }
            Err(e) => self.error = Some(e),
        }
    }

    fn recheck(&mut self) {
        let parent = self.repo.as_deref().and_then(Path::parent);
        self.exists = self
            .page
            .iter()
            .flat_map(|p| &p.repos)
            .map(|r| parent.is_some_and(|d| d.join(&r.name).exists()))
            .collect();
    }
}

pub(super) fn show(state: &mut State, ctx: &Context) {
    let idle = state.running_here().is_none();
    // Clone and Pull run there, as typed.
    let why_not = if state.is_remote() {
        Some("Not while this tab is connected over SSH")
    } else if !idle {
        Some(super::project_bar::BUSY)
    } else {
        None
    };
    let font_size = state.gui_font_size;
    let home = state.home.as_deref();

    let win = &mut state.ui.projects;
    let Some(repo) = win.repo.clone() else {
        return;
    };
    let Some(into) = repo.parent() else {
        return;
    };

    win.pump();
    // A clone or pull just finished.
    if idle && !win.was_idle {
        win.recheck();
    }
    win.was_idle = idle;

    let mut open = true;
    let mut goto = None;
    let mut line = None;

    Window::new("Projects")
        .open(&mut open)
        .default_size([600., 640.])
        .resizable(true)
        .show(ctx, |ui| {
            super::scale_panel_fonts(ui, font_size);

            ui.horizontal_wrapped(|ui| {
                ui.label("Workspace:");
                match &win.listing {
                    Some(l) => {
                        let ws = &l.workspace;
                        ui.hyperlink_to(RichText::new(ws.label()).strong(), ws.web_url());
                    }
                    None => {
                        ui.weak("…");
                    }
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.label("Clones to:");
                ui.label(RichText::new(render_with_tilde(into, home)).strong());
            });

            ui.add_space(4.);
            goto = pager(ui, win);

            if let Some(e) = &win.error {
                ui.label(RichText::new(e).color(ui.visuals().error_fg_color));
            }
            ui.separator();

            let Some(page) = &win.page else {
                return;
            };
            if page.repos.is_empty() {
                ui.weak("No repos.");
                return;
            }

            ScrollArea::vertical()
                // Each page from its top.
                .id_salt(("projects_list", page.number))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let button_width = super::chars_width(ui, TextStyle::Button, 6.);
                    for (i, r) in page.repos.iter().enumerate() {
                        let exists = win.exists.get(i).copied().unwrap_or(false);
                        let dest = into.join(&r.name);
                        if let Some(l) = repo_row(ui, r, exists, &dest, why_not, button_width) {
                            line = Some(l);
                        }
                        ui.separator();
                    }
                });
        });

    if !open {
        *win = ProjectsWindow::default();
    } else if let Some(n) = goto {
        win.fetch(n, ctx);
    }
    if let Some(line) = line {
        submit_command(state, &line);
    }
}

/// Prev / Next, and the page shown. Returns the page to fetch.
fn pager(ui: &mut Ui, win: &ProjectsWindow) -> Option<u32> {
    let fetching = win.fetch.is_some();
    let page = win.page.as_ref();
    let n = page.map_or(1, |p| p.number);
    let has_next = page.is_some_and(|p| p.has_next);
    let mut goto = None;

    ui.horizontal(|ui| {
        if ui
            .add_enabled(!fetching && n > 1, Button::new("⏴ Prev"))
            .clicked()
        {
            goto = Some(n - 1);
        }
        let of = match page.and_then(|p| p.last) {
            Some(last) => format!("Page {n} of {last}"),
            None => format!("Page {n}"),
        };
        ui.label(of);
        if ui
            .add_enabled(!fetching && has_next, Button::new("Next ⏵"))
            .clicked()
        {
            goto = Some(n + 1);
        }

        if fetching {
            ui.spinner();
        } else if ui.button("Refresh").clicked() {
            goto = Some(n);
        }
    });

    goto
}

/// A repo's Clone or Pull button, then its name, linking to its page, and
/// description. Returns the line to run, when clicked.
fn repo_row(
    ui: &mut Ui,
    r: &RemoteRepo,
    exists: bool,
    dest: &Path,
    why_not: Option<&str>,
    button_width: f32,
) -> Option<String> {
    let (label, line) = if exists {
        ("Pull", project::git_pull(dest))
    } else {
        ("Clone", project::git_clone(&r.clone_url, dest))
    };
    let mut clicked = false;

    ui.horizontal_top(|ui| {
        let button = Button::new(label).min_size(vec2(button_width, 0.));
        clicked = ui
            .add_enabled(why_not.is_none(), button)
            .on_hover_text(&line)
            .on_disabled_hover_text(why_not.unwrap_or_default())
            .clicked();

        ui.vertical(|ui| {
            ui.horizontal_wrapped(|ui| {
                ui.hyperlink_to(RichText::new(&r.name).strong(), &r.web_url);
                let tags = [
                    (!r.visibility.is_empty(), r.visibility.as_str()),
                    (r.fork, "fork"),
                    (r.archived, "archived"),
                ];
                for (_, tag) in tags.into_iter().filter(|(on, _)| *on) {
                    ui.label(RichText::new(tag).small().weak());
                }
            });
            if !r.description.is_empty() {
                ui.add(Label::new(RichText::new(&r.description).weak()).wrap());
            }
        });
    });

    clicked.then_some(line)
}
