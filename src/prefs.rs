//! The GUI's settings and layout, saved by eframe (see [eframe::App::save]),
//! on exit and every 30s, rather than in the `.ss` file shared with the CLI.
//! eframe also saves the main window's position and size, and egui's memory:
//! the theme, side panel widths, and egui windows' positions. The child
//! windows' places are here (see [crate::placement]).
//!
//! Each setting has its own key, so one that fails to load (e.g. after its
//! type changes) keeps its default without losing the rest.

use eframe::{Storage, get_value, set_value};
use serde::de::DeserializeOwned;

use crate::{path_color, state::State, ui::panes};

/// Save the settings, for [load] on the next run.
pub fn save(state: &State, storage: &mut dyn Storage) {
    let ui = &state.ui;

    set_value(storage, "panel_vis", &ui.panel_vis);
    set_value(storage, "pane_order", &panes::order_keys(&ui.pane_order));
    set_value(storage, "files_details_split", &ui.files_details_split);
    set_value(storage, "path_details_split", &ui.path_details_split);
    set_value(storage, "show_hidden_attr", &ui.show_hidden_attr);
    set_value(storage, "show_dotfiles", &ui.show_dotfiles);
    set_value(
        storage,
        "deep_search_names",
        &ui.deep_search_opts.file_names,
    );
    set_value(storage, "scratchpad_tab", &ui.scratchpad.tab);
    set_value(storage, "path_window", &ui.path_window.placement);
    set_value(storage, "viewer_window", &state.file_viewers.placement());

    set_value(storage, "font_size", &state.font_size);
    set_value(storage, "gui_font_size", &state.gui_font_size);
    set_value(storage, "bookmark_label_len", &state.bookmark_label_len);
    set_value(storage, "recent_dir_label_len", &state.recent_dir_label_len);
    set_value(storage, "bookmark_names", &state.bookmark_names);
    set_value(storage, "recent_dir_names", &state.recent_dir_names);
    set_value(storage, "bookmark_sort", &state.bookmark_sort);
    set_value(storage, "cmd_history_show_dir", &state.cmd_history_show_dir);
    set_value(storage, "dir_color_depth", &path_color::depth_levels());
}

/// Apply what [save] saved. Settings it lacks keep their defaults.
pub fn load(state: &mut State, storage: &dyn Storage) {
    let ui = &mut state.ui;

    load_into(storage, "panel_vis", &mut ui.panel_vis);
    if let Some(keys) = get_value::<Vec<String>>(storage, "pane_order") {
        ui.pane_order = panes::order_from_keys(&keys);
    }
    load_into(storage, "files_details_split", &mut ui.files_details_split);
    load_into(storage, "path_details_split", &mut ui.path_details_split);
    load_into(storage, "show_hidden_attr", &mut ui.show_hidden_attr);
    load_into(storage, "show_dotfiles", &mut ui.show_dotfiles);
    load_into(
        storage,
        "deep_search_names",
        &mut ui.deep_search_opts.file_names,
    );
    load_into(storage, "scratchpad_tab", &mut ui.scratchpad.tab);
    load_into(storage, "path_window", &mut ui.path_window.placement);
    // Fitted to the monitors when a window opens there (see
    // [crate::placement::Placement::on_screen]), not here.
    state
        .file_viewers
        .set_placement(get_value(storage, "viewer_window").flatten());

    load_into(storage, "font_size", &mut state.font_size);
    load_into(storage, "gui_font_size", &mut state.gui_font_size);
    load_into(storage, "bookmark_label_len", &mut state.bookmark_label_len);
    load_into(
        storage,
        "recent_dir_label_len",
        &mut state.recent_dir_label_len,
    );
    load_into(storage, "bookmark_names", &mut state.bookmark_names);
    load_into(storage, "recent_dir_names", &mut state.recent_dir_names);
    load_into(storage, "bookmark_sort", &mut state.bookmark_sort);
    load_into(
        storage,
        "cmd_history_show_dir",
        &mut state.cmd_history_show_dir,
    );
    if let Some(n) = get_value(storage, "dir_color_depth") {
        path_color::set_depth_levels(n);
    }

    // Both depend on settings just loaded.
    state.refresh_bookmarks_filter();
    state.refresh_browser_files();
}

/// Set `into` to the value saved under `key`, if there is one.
fn load_into<T: DeserializeOwned>(storage: &dyn Storage, key: &str, into: &mut T) {
    if let Some(v) = get_value(storage, key) {
        *into = v;
    }
}
