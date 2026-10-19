//! The Settings window, opened from the gear button on the tab strip: path
//! colour depth, and how many characters of each Bookmarks / Recent dirs path
//! to show.

use egui::{Context, DragValue, Grid, Ui, Window};

use crate::{
    path_color,
    state::State,
    ui::dirs::{MAX_DIR_LABEL_LEN, MIN_DIR_LABEL_LEN},
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

/// A [DragValue] for a Bookmarks / Recent dirs path length. Updates `len`
/// live, clamped to [MIN_DIR_LABEL_LEN]..=[MAX_DIR_LABEL_LEN]. Returns true
/// once a change is complete (a finished drag, or a committed edit), so the
/// caller can persist it without saving on every frame of a drag. `what`
/// names the panel's paths, for the tooltip.
fn dir_label_len_control(ui: &mut Ui, len: &mut usize, what: &str) -> bool {
    let resp = ui
        .add(
            DragValue::new(len)
                .range(MIN_DIR_LABEL_LEN..=MAX_DIR_LABEL_LEN)
                .speed(0.25)
                .update_while_editing(false),
        )
        .on_hover_text(format!(
            "Max characters of {what} paths to show; longer ones are cut from the \
             front. Drag, or click to type."
        ));

    (resp.changed() && !resp.dragged()) || resp.drag_stopped()
}

pub(super) fn show(state: &mut State, ctx: &Context) {
    if !state.ui.settings {
        return;
    }

    let mut dir_depth_delta = 0;
    let mut save = false;

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
                    // How deep a directory is when its colour reaches yellow
                    // (see [path_color::depth_levels]).
                    ui.label("Dir color depth:");
                    dir_depth_delta = dir_depth_control(ui);
                    ui.end_row();

                    // Applied every frame so the panels follow a drag live;
                    // saved once done.
                    ui.label("Bookmarks chars:");
                    save |= dir_label_len_control(ui, &mut state.bookmark_label_len, "bookmark");
                    ui.end_row();

                    ui.label("Recent dirs chars:");
                    save |=
                        dir_label_len_control(ui, &mut state.recent_dir_label_len, "recent dir");
                    ui.end_row();
                });
        });
    state.ui.settings = open;

    if dir_depth_delta != 0 {
        state.adjust_dir_color_depth(dir_depth_delta);
    }

    if save {
        let _ = state.save();
    }
}
