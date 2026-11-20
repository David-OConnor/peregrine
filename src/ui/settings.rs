//! The Settings window, opened from the gear button on the tab strip: path
//! color depth, how many characters of each Bookmarks / Recent dirs path
//! and Files name to show, and the editor's language servers.

use egui::{CollapsingHeader, Context, DragValue, Grid, Id, Rect, TextEdit, Ui, Window};

use crate::{
    path_color,
    state::State,
    ui::{
        MAX_FILE_NAME_LEN, MAX_FONT_SIZE, MIN_FILE_NAME_LEN, MIN_FONT_SIZE,
        dirs::{MAX_DIR_LABEL_LEN, MIN_DIR_LABEL_LEN},
    },
    viewer::{self, LanguageServer, LspSettings},
};

/// A [DragValue] for [path_color::depth_levels]. Returns the change to apply.
fn dir_depth_control(ui: &mut Ui) -> i32 {
    let levels = path_color::depth_levels() as i32;
    let mut val = levels;

    let response = ui
        .add(
            DragValue::new(&mut val)
                .range(path_color::MIN_DEPTH_LEVELS..=path_color::MAX_DEPTH_LEVELS)
                .speed(0.1)
                .update_while_editing(false),
        )
        .on_hover_text(
            "Directory levels from purple (root or ~) to yellow. Drag, or click to type.",
        );

    if response.changed() { val - levels } else { 0 }
}

/// Returns the requested change in font size, or zero when unchanged, and
/// where the control was drawn.
fn font_size_control(ui: &mut Ui, size: f32) -> (f32, Rect) {
    let mut val = size.round() as i32;

    let response = ui
        .add(
            DragValue::new(&mut val)
                .range(MIN_FONT_SIZE..=MAX_FONT_SIZE)
                .speed(0.25)
                .update_while_editing(false),
        )
        .on_hover_text("Increase or decrease font size. Drag, click to type, or Ctrl+scroll.");

    let delta = if response.changed() {
        val as f32 - size
    } else {
        0.
    };

    (delta, response.rect)
}

/// A [DragValue] for a Bookmarks / Recent dirs path length. Updates `len`
/// live, clamped to [MIN_DIR_LABEL_LEN]..=[MAX_DIR_LABEL_LEN]. `what` names
/// the panel's paths, for the tooltip.
fn dir_label_len_control(ui: &mut Ui, len: &mut usize, what: &str) {
    ui.add(
        DragValue::new(len)
            .range(MIN_DIR_LABEL_LEN..=MAX_DIR_LABEL_LEN)
            .speed(0.25)
            .update_while_editing(false),
    )
    .on_hover_text(format!(
        "Max characters of {what} paths to show; longer ones are cut from the \
         front. Drag, or click to type."
    ));
}

/// A [DragValue] for the Files panel's name length. Updates `len` live,
/// clamped to [MIN_FILE_NAME_LEN]..=[MAX_FILE_NAME_LEN].
fn file_name_len_control(ui: &mut Ui, len: &mut usize) {
    ui.add(
        DragValue::new(len)
            .range(MIN_FILE_NAME_LEN..=MAX_FILE_NAME_LEN)
            .speed(0.25)
            .update_while_editing(false),
    )
    .on_hover_text(
        "Max characters of file names to show; longer ones are cut in the \
         middle. Drag, or click to type.",
    );
}

/// The file viewer's language servers: on or off, and the command run for
/// each language's.
fn lsp_settings(ui: &mut Ui, state: &State) {
    let mut settings = state.file_viewers.lsp_settings();
    let before = settings.clone();
    ui.checkbox(&mut settings.enabled, "Language servers in edit mode")
        .on_hover_text(
            "Editing source, start its language's server, if one is installed: it \
             underlines problems (F8 steps through them), shows docs on hover or \
             Ctrl+Q, completes (Ctrl+Space), goes to definitions (F12 or \
             Ctrl+click) and formats (Shift+Alt+F).",
        );
    if settings.enabled {
        CollapsingHeader::new("Server commands")
            .id_salt("lsp_commands")
            .show(ui, |ui| {
                Grid::new("lsp_commands_grid")
                    .num_columns(2)
                    .spacing([12., 4.])
                    .show(ui, |ui| {
                        for language in viewer::language_servers() {
                            ui.label(format!("{}:", language.name));
                            command_box(ui, &mut settings, &language);
                            ui.end_row();
                        }
                    });
            });
    }
    if settings != before {
        state.file_viewers.set_lsp_settings(settings);
    }
}

/// The command line `language`'s server runs, applied once the box loses
/// focus: each change restarts the server. Empty runs the first of its
/// defaults found on PATH.
fn command_box(ui: &mut Ui, settings: &mut LspSettings, language: &LanguageServer) {
    let id = Id::new(("lsp_command", language.name));
    let draft = id.with("draft");
    let saved = settings
        .commands
        .get(language.name)
        .cloned()
        .unwrap_or_default();
    let mut text = ui.data(|d| d.get_temp::<String>(draft)).unwrap_or(saved);
    let response = ui
        .add(
            TextEdit::singleline(&mut text)
                .id(id)
                .hint_text(language.commands[0])
                .desired_width(280.),
        )
        .on_hover_text(format!(
            "Leave empty to run the first of these on PATH:\n{}\n\nTo install one: {}",
            language.commands.join("\n"),
            language.install
        ));
    if response.has_focus() {
        ui.data_mut(|d| d.insert_temp(draft, text));
        return;
    }
    ui.data_mut(|d| d.remove::<String>(draft));
    if response.lost_focus() {
        match text.trim() {
            "" => settings.commands.remove(language.name),
            line => settings
                .commands
                .insert(language.name.to_owned(), line.to_owned()),
        };
    }
}

pub(super) fn show(state: &mut State, ctx: &Context) {
    if !state.ui.settings {
        return;
    }

    let mut dir_depth_delta = 0;

    // Keep the close button's flag separate while the contents borrow state.
    let mut open = true;
    Window::new("Settings")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            super::scale_panel_fonts(ui, state.gui_font_size);

            Grid::new("settings_grid")
                .num_columns(2)
                .spacing([12., 6.])
                .show(ui, |ui| {
                    let label = ui.label("CLI font:");
                    let (delta, rect) = font_size_control(ui, state.font_size);
                    let cli_font_delta = delta;
                    // Ctrl+scroll over it steps the terminal's font, not the GUI's.
                    state.ui.cli_font_rect = label.rect.union(rect);

                    ui.label("GUI font:");
                    let gui_font_delta = font_size_control(ui, state.gui_font_size).0;

                    if cli_font_delta != 0.0 {
                        state.adjust_font_size(cli_font_delta);
                    }

                    if gui_font_delta != 0.0 {
                        state.adjust_gui_font_size(gui_font_delta);
                    }

                    // How deep a directory is when its color reaches yellow
                    // (see [path_color::depth_levels]).
                    ui.label("Dir color depth:");
                    dir_depth_delta = dir_depth_control(ui);
                    ui.end_row();

                    // Applied every frame so the panels follow a drag live.
                    ui.label("Bookmarks chars:");
                    dir_label_len_control(ui, &mut state.bookmark_label_len, "bookmark");

                    ui.label("Recent dirs chars:");
                    dir_label_len_control(ui, &mut state.recent_dir_label_len, "recent dir");
                    ui.end_row();

                    ui.label("File name chars:");
                    file_name_len_control(ui, &mut state.file_name_len);
                    ui.end_row();
                });

            ui.separator();
            lsp_settings(ui, state);
        });
    state.ui.settings = open;

    if dir_depth_delta != 0 {
        state.adjust_dir_color_depth(dir_depth_delta);
    }
}
