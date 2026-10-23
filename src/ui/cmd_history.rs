//! For all recent commands, and ones in the current dir.

use eframe::epaint::text::TextWrapMode;
use egui::{Button, ScrollArea, TextStyle, Ui};

use crate::{
    state::State,
    ui,
    ui::{CMD_COL_MIN_CHARS, ListAction, search},
};

/// Display recent commands entered, newest first, in columns when the panel
/// is wide enough. Clicking the label copies
/// the command into the input box so the user can edit or run it, and
/// middle-clicking runs it in a new tab (see [cmd_click]); the "In dir"
/// button, shown while "Show dir" is on, re-runs it in the directory it was
/// originally run from.
pub fn cmd_history(state: &mut State, ui: &mut Ui) {
    ui::panel_header(ui, "Recent cmds", &mut state.ui.panel_vis.recent_cmds);

    let show_dir = state.cmd_history_show_dir;
    let hover = if show_dir {
        "Showing the dir each command was run in. Click to hide it."
    } else {
        "Hiding the dir each command was run in. Click to show it."
    };
    // The search box shares the toggle's row, as much of it as that leaves.
    let (toggled, searched) = ui
        .horizontal_wrapped(|ui| {
            let toggled = ui
                .add(Button::selectable(show_dir, "Show dir"))
                .on_hover_text(hover)
                .clicked();
            let searched =
                search::inline_search_box(ui, &mut state.ui.history_search, "history_search");
            (toggled, searched)
        })
        .inner;

    if toggled {
        state.cmd_history_show_dir = !show_dir;
        let _ = state.save();
    }
    if searched {
        state.refresh_history_filter();
    }
    ui.separator();

    let mut action: Option<ListAction> = None;

    ScrollArea::vertical()
        .id_salt("cmd_history_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // Force every widget in this column to wrap rather than
            // expand horizontally, so long command lines break onto extra
            // rows instead of stretching the panel.
            ui.style_mut().wrap_mode = Some(TextWrapMode::Wrap);

            // Newest first.
            let rows: Vec<usize> = state.ui.history_filter.iter().rev().copied().collect();
            let min_width = ui::chars_width(ui, TextStyle::Button, CMD_COL_MIN_CHARS);
            ui::list_columns(ui, &rows, min_width, |ui, i| {
                let Some(item) = state.history.get(i) else {
                    return;
                };

                let label = ui::cmd_label(ui, &state.cmd_colors, i, &item.text);
                let btn = Button::new(label)
                    .wrap()
                    .min_size(egui::vec2(ui.available_width(), 0.0));
                if let Some(a) = cmd_click(ui.add(btn), &item.text) {
                    action = Some(a);
                }
                if !show_dir {
                    return;
                }

                // "In <leaf>" sits on its own row underneath so it
                // doesn't fight the wrapping command-text button for
                // horizontal space.
                let leaf = item
                    .dir
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| item.dir.display().to_string());
                let leaf = if leaf.chars().count() > 20 {
                    leaf.chars().take(20).collect::<String>()
                } else {
                    leaf
                };

                if ui.small_button(format!("In {leaf}")).clicked() {
                    action = Some(ListAction::RunHistoryInDir(i));
                }
                ui.separator();
            });
        });

    ui::apply_action(state, action);
}

/// What a click on a Cmd history / Cmds this dir row does: a left click
/// copies `text` into the input box, a middle click runs it in a new tab.
pub fn cmd_click(resp: egui::Response, text: &str) -> Option<ListAction> {
    if resp.middle_clicked() {
        Some(ListAction::RunInNewTab(text.to_owned()))
    } else if resp.clicked() {
        Some(ListAction::FillInput(text.to_owned()))
    } else {
        None
    }
}

/// Display commands previously run in the active tab's cwd. Newest first, in
/// columns when the panel is wide enough. Same click behavior as the global
/// history panel (see [cmd_click]).
pub fn cwd_cmd_history(state: &mut State, ui: &mut Ui) {
    ui::panel_header(
        ui,
        "Cmds this dir",
        &mut state.ui.panel_vis.recent_cmds_in_dir,
    );

    if search::search_box(ui, &mut state.ui.cwd_history_search, "cwd_history_search") {
        state.refresh_cwd_history_filter();
    }
    ui.separator();

    let mut action: Option<ListAction> = None;

    ScrollArea::vertical()
        .id_salt("cwd_cmd_history_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // Wrap long commands onto extra rows instead of letting the
            // widest one stretch the panel across the window. The panel
            // stays user-resizable via its drag handle.
            ui.style_mut().wrap_mode = Some(TextWrapMode::Wrap);

            let rows: Vec<usize> = state.ui.cwd_history_filter.iter().rev().copied().collect();
            let min_width = ui::chars_width(ui, TextStyle::Button, CMD_COL_MIN_CHARS);
            ui::list_columns(ui, &rows, min_width, |ui, i| {
                let Some(item) = state.history.get(i) else {
                    return;
                };

                let label = ui::cmd_label(ui, &state.cmd_colors, i, &item.text);
                let btn = Button::new(label)
                    .wrap()
                    .min_size(egui::vec2(ui.available_width(), 0.0));
                if let Some(a) = cmd_click(ui.add(btn), &item.text) {
                    action = Some(a);
                }
            });
        });

    ui::apply_action(state, action);
}
