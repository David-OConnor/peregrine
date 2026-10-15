//! Program information and the location of the state shared with Shell.

use std::path::Path;

use egui::{Context, RichText, Window};

use crate::{VERSION, files::open_with_default_app, render_with_tilde, state::State};

const REPO_URL: &str = "https://github.com/David-OConnor/peregrine";

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
