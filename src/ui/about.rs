//! Program information, updates, and the location of the state shared with
//! Shell.

use std::{path::Path, time::Duration};

use egui::{Context, ProgressBar, RichText, Ui, ViewportCommand, Window};
use software_manager::{Application, UpdateStatus};

use crate::{VERSION, files::open_with_default_app, render_with_tilde, state::State};

const REPO_URL: &str = "https://github.com/David-OConnor/peregrine";

/// Check GitHub for a newer release, download it, and install it. Installing
/// waits for Peregrine to close, since its files can't be replaced while it
/// runs; the release's own setup script then installs it, and Peregrine
/// reopens.
fn update_section(state: &mut State, ui: &mut Ui) {
    let updater = &mut state.ui.updater;
    updater.poll();

    if updater.busy() {
        // So the download progress moves.
        ui.ctx().request_repaint_after(Duration::from_millis(100));
    }

    let mut check = false;
    let mut download = false;
    let mut install = false;

    ui.horizontal_wrapped(|ui| match updater.status() {
        UpdateStatus::Idle => {
            check = ui.button("Check for updates").clicked();
        }
        UpdateStatus::Checking(_) => {
            ui.spinner();
            ui.label("Checking for updates…");
        }
        UpdateStatus::UpToDate => {
            ui.label("This is the latest version.");
            check = ui.button("Check again").clicked();
        }
        UpdateStatus::Available(release) => {
            ui.label(format!("Version {} is available.", release.version));
            ui.hyperlink_to("Release notes", &release.page_url);

            download = ui
                .button("Download and install")
                .on_hover_text(format!(
                    "Download the update ({:.0} MB). Installing it is the next step.",
                    release.size as f64 / 1_000_000.
                ))
                .clicked();
        }
        UpdateStatus::Downloading {
            release, progress, ..
        } => {
            ui.add(
                ProgressBar::new(progress.fraction())
                    .text(format!("Downloading {}…", release.version))
                    .desired_width(300.),
            );
        }
        UpdateStatus::Ready(staged) => {
            ui.label(format!("Version {} is downloaded.", staged.release.version));

            install = ui
                .button("Restart and install")
                .on_hover_text(
                    "Close Peregrine, install the update, then reopen it. Running \
                    commands are stopped, as when closing it.",
                )
                .clicked();
        }
        UpdateStatus::Installing => {
            ui.label("Installing…");
        }
        UpdateStatus::Failed(e) => {
            ui.label(
                RichText::new(format!("Unable to update: {e}"))
                    .color(ui.visuals().error_fg_color)
                    .small(),
            );
            check = ui.button("Try again").clicked();
        }
    });

    let ctx = ui.ctx().clone();
    let repaint = {
        let ctx = ctx.clone();
        move || ctx.request_repaint()
    };

    if check {
        updater.check(Application::Peregrine, VERSION, repaint);
    } else if download {
        updater.download(repaint);
    } else if install && updater.install_on_exit() {
        // The installer is waiting for this process to end. Closing saves
        // state, in `on_exit`.
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }
}

pub(super) fn show(state: &mut State, ctx: &Context) {
    if !state.ui.about {
        return;
    }

    // Keep the close button's flag separate while the contents borrow state.
    let mut open = true;
    Window::new("About")
        .open(&mut open)
        .default_width(480.)
        .resizable(true)
        .show(ctx, |ui| {
            super::scale_panel_fonts(ui, state.gui_font_size);

            ui.horizontal(|ui| {
                ui.heading("Peregrine");
                ui.label(RichText::new(format!("v{VERSION}")).weak());
            });

            ui.add_space(8.);
            ui.label("An integrated terminal and file browser.");

            ui.add_space(8.);
            update_section(state, ui);

            ui.add_space(8.);
            ui.horizontal_wrapped(|ui| {
                ui.label("Source code:");
                ui.hyperlink_to(REPO_URL, REPO_URL);
            });

            ui.add_space(8.);
            ui.label("Preferences, bookmarks, command history, and clipboard history are saved in the state file shared with Shell:");
            ui.label(render_with_tilde(&state.state_path, state.home.as_deref()))
                .on_hover_text(state.state_path.to_string_lossy());

            if ui.button("Open containing folder").clicked() {
                let folder = state
                    .state_path
                    .parent()
                    .filter(|path| !path.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
                state.ui.about_folder_error = open_with_default_app(folder)
                    .err()
                    .map(|error| error.to_string());
            }

            if let Some(error) = &state.ui.about_folder_error {
                ui.label(
                    RichText::new(format!("Could not open the state file's folder: {error}"))
                        .color(ui.visuals().error_fg_color)
                        .small(),
                );
            }
        });
    state.ui.about = open;
}
