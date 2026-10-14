use std::path::PathBuf;

use egui::{
    Align, Button, Event, Frame, Key, Layout, Modifiers, Panel, RichText, ScrollArea, TextStyle, Ui,
};

use crate::{
    OutType, files, path_color, render_with_tilde,
    state::State,
    ui,
    ui::{
        COLOR_EXECUTABLE, FILES_COL_MIN_CHARS, ListAction,
        dirs::{BOOKMARKED_FILL, BOOKMARKED_TEXT},
    },
    viewer,
};

/// Files in the active tab's cwd, as a selectable list. Folders are
/// sorted first and prefixed with 📁, on pale green when bookmarked (as in
/// Recent dirs); executable files (Unix +x, or
/// Windows `.exe`/`.msi`) are coloured green. The listing is refreshed
/// by [State::refresh_browser_files] on every cwd change and after every
/// command, so we just read what's already there. The search box under the
/// hidden/dotfile toggles narrows the listing by name and selects the first
/// match as it's edited. Enter opens the selected row, just like a double-click.
/// When the panel is wide enough (see [FILES_COL_MIN_CHARS]) the list is laid
/// out in as many columns as fit. The arrow keys move the selection as the
/// rows are laid out (see [arrow_target]); Up and Down work from the search
/// box too.
///
/// Clicking a row selects it, and the pane at the bottom of the panel shows
/// its details (see [ui::file_details]), with a picture of an image under
/// them. The divider above that pane drags up and down (see [ui::Divider]).
/// Double-clicking a folder row navigates into
/// it (same path as clicking a bookmark or recent dir), and middle-clicking
/// opens it in a new tab; double-clicking a
/// Markdown or plain-text (`.txt`, `.gitignore`) file opens it in a viewer
/// window (see `crate::viewer`); any other file opens with the OS's default
/// associated program (see
/// [open_with_default_app]). Clicking the already-selected row again renames
/// it in place (see [ui::rename_box]): Enter or clicking away applies the new
/// name, Escape cancels.
///
/// Cut / Copy (or Ctrl+X / C) put the selected row on the OS clipboard, and
/// Paste (Ctrl+V) copies or moves the files there into this dir, so files
/// can go to and from the OS file manager too (see `crate::clipboard`).
pub(crate) fn file_browser(state: &mut State, ui: &mut Ui) {
    // Measured before anything is laid out, so the details pane's share is
    // of the whole panel rather than of what's left under the header.
    let split = &mut state.ui.files_details_split;
    let divider = ui::Divider::begin(ui, "files_details_divider", split);
    // The whole pane, for the mouse Back/Forward buttons below.
    let pane_rect = ui.max_rect();

    // A click in this pane gives it Enter, the arrow keys and Ctrl+C / X / V;
    // a click anywhere else takes them away.
    if ui.input(|i| i.pointer.any_pressed()) {
        state.ui.files_active = ui.rect_contains_pointer(pane_rect);
    }

    // Consume Enter before drawing the widgets, so the quick filter keeps
    // focus and a selected row's button doesn't turn it into a rename click.
    // Other text boxes (including Deep search and rename) keep their Enter.
    let search_id = ui.make_persistent_id(egui::IdSalt::new("files_search"));
    let search_has_keys = state.ui.rename.is_none() && ui.memory(|m| m.has_focus(search_id));
    let list_has_keys =
        state.ui.rename.is_none() && state.ui.files_active && !ui.ctx().text_edit_focused();
    let open_selected = (search_has_keys || list_has_keys)
        && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
    if open_selected {
        state.ui.rename_pending = None;
    }

    // Likewise the arrow keys, which move the selection: Up / Down from the
    // quick filter too, but Left / Right only from the listing, so they still
    // move the filter's caret. Not while Deep search results stand in for
    // the listing.
    let mut arrows = Vec::new();
    if state.deep_search.is_none() {
        use Key::{ArrowDown, ArrowLeft, ArrowRight, ArrowUp};
        ui.input_mut(|i| {
            for key in [ArrowUp, ArrowDown, ArrowLeft, ArrowRight] {
                let vertical = matches!(key, ArrowUp | ArrowDown);
                if list_has_keys || (search_has_keys && vertical) {
                    let n = i.count_and_consume_key(Modifiers::NONE, key);
                    arrows.extend(std::iter::repeat_n(key, n));
                }
            }
        });
    }
    if !arrows.is_empty() {
        // egui would otherwise also move keyboard focus off the quick filter.
        ui.memory_mut(|m| m.move_focus(egui::FocusDirection::None));
    }

    ui::panel_header(ui, "Files", &mut state.ui.panel_vis.file_browser);

    // The directory being listed, colour-coded like the other panels' paths,
    // and on pale green when it's bookmarked, as its rows are elsewhere.
    // Wraps rather than widening the panel when it's long. The buttons to its
    // right bookmark it (or remove the bookmark), and do what the `open`
    // command does: show this dir in the OS's file manager.
    let bookmarked = state.dir_bookmarks.iter().any(|p| p == state.cwd());
    let home = state.home.clone();
    let cwd_job = if bookmarked {
        ui::path_job_on(ui, state.cwd(), home.as_deref(), false, BOOKMARKED_TEXT)
    } else {
        ui::path_job(ui, state.cwd(), home.as_deref())
    };
    let mut open_clicked = false;
    let mut star_clicked = false;
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::TOP), |ui| {
            if ui
                .button("📂")
                .on_hover_text("Open this folder in the system file manager")
                .clicked()
            {
                open_clicked = true;
            }
            star_clicked = ui::dirs::bookmark_star(ui, bookmarked, false).clicked();
            // Left-aligned in whatever width the buttons leave.
            ui.with_layout(Layout::left_to_right(Align::TOP), |ui| {
                let label = egui::Label::new(cwd_job).wrap();
                if bookmarked {
                    Frame::new()
                        .fill(BOOKMARKED_FILL)
                        .corner_radius(ui.visuals().widgets.inactive.corner_radius)
                        .inner_margin(egui::Margin::symmetric(4, 1))
                        .show(ui, |ui| ui.add(label));
                } else {
                    ui.add(label);
                }
            });
        });
    });
    if star_clicked {
        let msg = state.toggle_bookmark_cwd();
        state.push_out(msg, OutType::StdOut);
    }
    if open_clicked {
        let cwd = state.cwd().to_path_buf();
        shell::commands::open_dir(&cwd, &mut |kind, msg| {
            state.push_out(msg, crate::out_type_for(kind));
        });
    }

    // Browser-style navigation row. Up goes to the parent dir; Back/Forward
    // walk the per-tab history maintained in `State::change_dir`. Buttons are
    // disabled when there's nowhere to go in that direction.
    let i = state.ui.active_tab;
    let can_up = state.cwd().parent().is_some();
    let can_back = !state.ui.nav_back[i].is_empty();
    let can_forward = !state.ui.nav_forward[i].is_empty();
    // Common-destination shortcuts (see [files::shortcuts]): 🏠 Home,
    // ⚙ app data, 🖥 Desktop, 📥 Downloads, 📦 programs, 🖴 root, 🗑 trash.
    // Each is only enabled when the target actually exists, so we never `cd`
    // into a missing dir and push a diagnostic. On Windows, 🖴 is a menu of
    // drives and 🗑 opens the Recycle Bin in Explorer.

    let shortcuts = files::shortcuts(home.as_deref());
    let mut go_up = false;
    let mut go_back = false;
    let mut go_forward = false;
    let mut go_to: Option<PathBuf> = None;
    #[cfg(windows)]
    let mut open_bin = false;

    // Wraps so the shortcuts drop to a second row in a narrow panel.
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(can_up, Button::new("⬆"))
            .on_hover_text("Parent directory")
            .clicked()
        {
            go_up = true;
        }

        if ui
            .add_enabled(can_back, Button::new("⬅"))
            .on_hover_text("Previous directory")
            .clicked()
        {
            go_back = true;
        }

        if ui
            .add_enabled(can_forward, Button::new("➡"))
            .on_hover_text("Next directory")
            .clicked()
        {
            go_forward = true;
        }

        for s in shortcuts {
            let ok = s.dir.as_ref().is_some_and(|d| d.is_dir());
            if ui
                .add_enabled(ok, Button::new(s.glyph))
                .on_hover_text(s.hover)
                .clicked()
            {
                go_to = s.dir;
            }
        }

        #[cfg(windows)]
        {
            // Read afresh each frame the menu is open, so a USB stick
            // plugged in meanwhile shows up. A drive with no media is still
            // listed, as in Explorer; picking it reports the `cd` error.
            ui.menu_button("🖴", |ui| {
                for d in files::drives() {
                    if ui.button(format!("{} {}", d.glyph, d.name)).clicked() {
                        go_to = Some(d.root);
                    }
                }
            })
            .response
            .on_hover_text("This PC: devices and drives");
            if ui
                .button("🗑")
                .on_hover_text("Recycle Bin (opens in Explorer)")
                .clicked()
            {
                open_bin = true;
            }
        }
    });

    #[cfg(windows)]
    if open_bin && let Err(e) = files::open_recycle_bin() {
        state.push_out(
            format!("open: failed to open the Recycle Bin: {e}"),
            OutType::StdErr,
        );
    }

    // Mouse buttons 4/5 (Back/Forward) act like ⬅/➡ while the pointer is
    // anywhere over this pane.
    if ui.rect_contains_pointer(pane_rect) {
        ui.input(|i| {
            go_back |= i.pointer.button_pressed(egui::PointerButton::Extra1);
            go_forward |= i.pointer.button_pressed(egui::PointerButton::Extra2);
        });
    }

    if go_up {
        state.nav_up();
    } else if go_back {
        state.nav_back();
    } else if go_forward {
        state.nav_forward();
    } else if let Some(target) = go_to {
        state.change_dir(target);
    }

    // Listing filters. Each button is highlighted while the entries it
    // covers are shown. The Hidden attribute is Windows-only, so elsewhere
    // only the dotfile filter appears. Then Cut / Copy / Paste. Wraps so the
    // latter drop to a second row in a narrow panel.
    let has_sel = state.selected_file.is_some();
    let pasting = state.paste.is_some();
    let mut filters_changed = false;
    let (mut cut, mut copy, mut paste) = (false, false, false);
    ui.horizontal_wrapped(|ui| {
        let mut toggle = |ui: &mut Ui, on: &mut bool, label: &str, what: &str| {
            let hover = if *on {
                format!("Showing {what}. Click to hide them.")
            } else {
                format!("Hiding {what}. Click to show them.")
            };
            if ui
                .add(Button::selectable(*on, label))
                .on_hover_text(hover)
                .clicked()
            {
                *on = !*on;
                filters_changed = true;
            }
        };
        #[cfg(windows)]
        toggle(
            ui,
            &mut state.ui.show_hidden_attr,
            "👁 Hidden",
            "files and folders with the Hidden attribute",
        );
        toggle(
            ui,
            &mut state.ui.show_dotfiles,
            "👁 .dot",
            "files and folders whose names start with \".\"",
        );

        ui.separator();
        cut = ui
            .add_enabled(has_sel, Button::new("✂ Cut"))
            .on_hover_text("Cut the selected item, to move it where it's pasted (Ctrl+X)")
            .clicked();
        copy = ui
            .add_enabled(has_sel, Button::new("🗐 Copy"))
            .on_hover_text("Copy the selected item to the clipboard (Ctrl+C)")
            .clicked();
        paste = ui
            .add_enabled(!pasting, Button::new("📋 Paste"))
            .on_hover_text(
                "Paste files from the clipboard into this folder (Ctrl+V).                  Works with files copied or cut in the system file manager too.",
            )
            .clicked();
        if pasting {
            ui.spinner();
        }
    });
    if filters_changed {
        state.refresh_browser_files();
        let _ = state.save();
    }

    // The same from the keyboard, while this pane has it: it was clicked
    // last, and none of its text boxes (search, rename) has focus. Text
    // selected in the details pane is left for egui to copy.
    if state.ui.files_active && !ui.ctx().text_edit_focused() {
        let text_selected = ui
            .ctx()
            .with_plugin(|s: &mut egui::text_selection::LabelSelectionState| s.has_selection())
            .unwrap_or(false);
        ui.input(|i| {
            for e in &i.events {
                match e {
                    Event::Cut => cut |= !text_selected,
                    Event::Copy => copy |= !text_selected,
                    // egui-winit swallows Ctrl+V's key press, and only sends
                    // a Paste event when there's text on the clipboard, which
                    // there isn't for copied files. So paste on its release.
                    Event::Key {
                        key: Key::V,
                        pressed: false,
                        modifiers,
                        ..
                    } if modifiers.command => paste = true,
                    _ => {}
                }
            }
        });
    }
    if cut || copy {
        state.copy_selected(cut);
    }
    if paste {
        state.paste_files(ui.ctx());
    }

    // Text filter on names in this dir; cleared on cd (see
    // [State::refresh_browser_files]).
    let search_changed = ui::search_box(ui, &mut state.ui.files_search, "files_search");
    if search_changed {
        state.refresh_browser_files_filter();
    }
    ui::deep_search_bar(state, ui);

    if search_changed {
        state.ui.rename_pending = None;
        let first_match = match &state.deep_search {
            Some(search) => {
                let needle = state.ui.files_search.to_lowercase();
                search
                    .hits
                    .iter()
                    .find(|hit| {
                        hit.path
                            .strip_prefix(&search.root)
                            .unwrap_or(&hit.path)
                            .to_string_lossy()
                            .to_lowercase()
                            .contains(&needle)
                    })
                    .map(|hit| hit.path.clone())
            }
            None => state
                .ui
                .browser_files_filter
                .first()
                .map(|&i| state.browser_files[i].path.clone()),
        };
        if let Some(path) = first_match {
            state.select_file(&path);
        } else {
            state.selected_file = None;
        }
    }

    let arrow_moved = move_selection(state, &arrows);

    if open_selected {
        let action = state.selected_file.as_ref().map(|file| {
            if file.is_folder {
                ListAction::ChangeDir(file.path.clone())
            } else {
                ListAction::OpenFile(file.path.clone())
            }
        });
        ui::apply_action(state, action);
    }

    ui.separator();

    let image = state
        .selected_file
        .as_ref()
        .filter(|d| !d.is_folder && viewer::is_image(&d.path))
        .map(|d| (d.path.as_path(), d.modified));
    state.file_preview.set(ui.ctx(), image);

    // Added before the list so it claims the bottom of the panel; the list
    // then fills the space left above it.
    let details_height = divider.height(ui, state.ui.files_details_split, ui::MIN_SPLIT_HEIGHT);
    let details = Panel::bottom("file_details_panel")
        .resizable(false)
        .exact_size(details_height)
        .frame(Frame::new().inner_margin(egui::Margin::symmetric(0, 4)))
        .show(ui, |ui| {
            let home = state.home.as_deref();
            ui::file_details(state.selected_file.as_ref(), home, &state.file_preview, ui);
        });

    // While a Deep search is set, its results stand in for the listing.
    if let Some(search) = &state.deep_search {
        let selected = state.selected_file.as_ref().map(|d| d.path.as_path());
        let home = state.home.as_deref();
        let action = ui::deep_search_results(
            search,
            &state.ui.files_search,
            selected,
            &state.dir_bookmarks,
            home,
            ui,
        );
        ui::apply_action(state, action);
    } else {
        file_list(state, ui, search_changed, arrow_moved);
    }

    // After the list, so the divider is on top of it for the pointer.
    if divider.end(ui, details.response.rect, &mut state.ui.files_details_split) {
        let _ = state.save();
    }
}

/// Move the Files selection by `arrows`, each as [arrow_target] says, in the
/// listing as last laid out. Returns whether it moved.
fn move_selection(state: &mut State, arrows: &[Key]) -> bool {
    if arrows.is_empty() {
        return false;
    }
    let rows = &state.ui.browser_files_filter;
    let path_at = |k: usize| &state.browser_files[rows[k]].path;
    let from = state
        .selected_file
        .as_ref()
        .and_then(|d| (0..rows.len()).find(|&k| *path_at(k) == d.path));
    let mut at = from;
    for &key in arrows {
        at = arrow_target(key, at, rows.len(), state.ui.files_columns).or(at);
    }
    let Some(to) = at.filter(|&to| Some(to) != from) else {
        return false;
    };
    let path = path_at(to).clone();
    state.select_file(&path);
    state.ui.rename_pending = None;
    true
}

/// Where an arrow `key` moves the selection in a list of `len` items laid
/// out in `columns` columns by [ui::list_columns], from item `from` (or from
/// nothing selected, the first item, or for Up the last). Up and Down step
/// through the list, wrapping around at its ends: down a column, then on to
/// the top of the next. Left and Right move to the same row of the column
/// beside, or to the last item when that column is shorter. `None` when
/// there's nowhere to go.
fn arrow_target(key: Key, from: Option<usize>, len: usize, columns: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let Some(i) = from else {
        return Some(if key == Key::ArrowUp { len - 1 } else { 0 });
    };
    let per_col = ui::column_len(len, columns);
    match key {
        Key::ArrowUp => Some((i + len - 1) % len),
        Key::ArrowDown => Some((i + 1) % len),
        Key::ArrowLeft => i.checked_sub(per_col),
        Key::ArrowRight => ((i / per_col + 1) * per_col < len).then(|| (i + per_col).min(len - 1)),
        _ => None,
    }
}

/// The Files panel's listing of its dir: one row per entry, in columns when
/// they fit (see [file_browser]). `search_changed` and `arrow_moved` say
/// the selection just moved, to be scrolled into view.
fn file_list(state: &mut State, ui: &mut Ui, search_changed: bool, arrow_moved: bool) {
    let home = state.home.clone();

    // Explorer-style rename: a click on the already-selected row starts one,
    // once it's clear the click isn't the first half of a double-click. Any
    // other press in the meantime calls it off.
    let (now, pressed) = ui.input(|i| (i.time, i.pointer.any_pressed()));
    let dbl_click_delay = ui.ctx().options(|o| o.input_options.max_double_click_delay);

    if pressed {
        state.ui.rename_pending = None;
    }

    if let Some((path, t)) = state.ui.rename_pending.clone() {
        let wait = t + dbl_click_delay - now;
        if wait > 0.0 {
            ui.ctx().request_repaint_after_secs(wait as f32);
        } else {
            state.ui.rename_pending = None;
            if state.selected_file.as_ref().is_some_and(|d| d.path == path) {
                state.start_rename(&path);
            }
        }
    }

    let selected = state.selected_file.as_ref().map(|d| d.path.as_path());
    let dark = ui.visuals().dark_mode;
    let mut action: Option<ListAction> = None;
    // A click on the selected row, which may become a rename (see above).
    let mut clicked_selected: Option<PathBuf> = None;
    // Set when the rename box loses focus: `true` to apply the new name,
    // `false` if Escape cancelled it. Kept apart from `action` so a click on
    // another row in the same frame can't drop the rename.
    let mut rename_done: Option<bool> = None;

    let columns = ScrollArea::vertical()
        .id_salt("file_browser_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // Rows butt up against each other, like a file manager's list.
            ui.spacing_mut().item_spacing.y = 0.0;

            if state.ui.browser_files_filter.is_empty() && !state.ui.files_search.is_empty() {
                ui.label(RichText::new("No matches.").italics().weak());
            }
            let row = |ui: &mut Ui, i: usize| {
                let Some(f) = state.browser_files.get(i) else {
                    return;
                };

                if let Some(r) = state.ui.rename.as_mut().filter(|r| r.path == f.path) {
                    if let Some(apply) = ui::rename_box(ui, r, f.is_folder) {
                        rename_done = Some(apply);
                    }
                    return;
                }

                let text = if f.is_folder {
                    format!("📁 {}", f.disp_name)
                } else {
                    f.disp_name.clone()
                };

                let is_selected = selected == Some(f.path.as_path());
                // A bookmarked folder is drawn on the same pale green as in
                // Recent dirs, unless it's selected so the selection shows.
                let bookmarked =
                    f.is_folder && !is_selected && state.dir_bookmarks.contains(&f.path);

                let mut rich = RichText::new(text).monospace();
                if f.is_folder {
                    if bookmarked {
                        rich = rich.color(BOOKMARKED_TEXT);
                    }
                    // Same colour this folder has in any path shown elsewhere,
                    // from the light-background palette on the green.
                    let path = render_with_tilde(&f.path, home.as_deref());
                    if let Some(c) = path_color::last_color(&path, Some("~"), dark && !bookmarked) {
                        rich = rich.color(c);
                    }
                } else if f.is_executable {
                    rich = rich.color(COLOR_EXECUTABLE);
                }

                // Full width so the whole row is clickable and the selection
                // highlight spans the column. Long names are truncated; the
                // details pane shows the full name.
                let mut row = Button::selectable(is_selected, rich)
                    .truncate()
                    .min_size(egui::vec2(ui.available_width(), 0.0));
                if bookmarked {
                    row = row.fill(BOOKMARKED_FILL).frame_when_inactive(true);
                }
                let resp = ui.add(row);
                if is_selected && search_changed {
                    resp.scroll_to_me(Some(Align::TOP));
                } else if is_selected && arrow_moved {
                    resp.scroll_to_me(None);
                }

                // A double-click's first click has already selected the row.
                // Then a folder navigates into it; a file opens with its
                // associated program, just like a desktop file manager. A
                // middle click opens a folder in a new tab.
                if f.is_folder && resp.middle_clicked() {
                    action = Some(ListAction::NewTabDir(f.path.clone()));
                } else if resp.double_clicked() {
                    action = Some(if f.is_folder {
                        ListAction::ChangeDir(f.path.clone())
                    } else {
                        ListAction::OpenFile(f.path.clone())
                    });
                } else if resp.clicked() {
                    if is_selected {
                        clicked_selected = Some(f.path.clone());
                    } else {
                        action = Some(ListAction::SelectFile(f.path.clone()));
                    }
                }
            };

            // As many columns as fit FILES_COL_MIN_CHARS characters each,
            // filled down like `ls`, so the folders stay together at the top
            // left.
            let min_width = ui::chars_width(ui, TextStyle::Monospace, FILES_COL_MIN_CHARS);
            ui::list_columns(ui, &state.ui.browser_files_filter, min_width, row)
        })
        .inner;
    state.ui.files_columns = columns;

    match rename_done {
        Some(true) => state.commit_rename(),
        Some(false) => state.ui.rename = None,
        None => {}
    }
    if let Some(path) = clicked_selected {
        state.ui.rename_pending = Some((path, now));
        ui.ctx().request_repaint_after_secs(dbl_click_delay as f32);
    }

    ui::apply_action(state, action);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrows_follow_the_layout() {
        use Key::{ArrowDown as D, ArrowLeft as L, ArrowRight as R, ArrowUp as U};
        // 8 items in 3 columns, filled down: 0 3 6 / 1 4 7 / 2 5.
        let go = |key, from| arrow_target(key, Some(from), 8, 3);
        assert_eq!(go(D, 1), Some(2));
        assert_eq!(go(D, 2), Some(3), "on to the top of the next column");
        assert_eq!(go(D, 7), Some(0), "wraps at the end");
        assert_eq!(go(U, 0), Some(7), "and at the start");
        assert_eq!(go(R, 1), Some(4));
        assert_eq!(go(L, 4), Some(1));
        assert_eq!(go(L, 1), None, "no column to the left");
        assert_eq!(go(R, 7), None, "nor to the right");
        assert_eq!(go(R, 5), Some(7), "the last item of a shorter column");

        // One column: Left and Right go nowhere.
        assert_eq!(arrow_target(R, Some(0), 8, 1), None);
        assert_eq!(arrow_target(L, Some(5), 8, 1), None);
        assert_eq!(arrow_target(D, Some(5), 8, 1), Some(6));

        // Nothing selected yet; nothing to select.
        assert_eq!(arrow_target(D, None, 8, 3), Some(0));
        assert_eq!(arrow_target(R, None, 8, 3), Some(0));
        assert_eq!(arrow_target(U, None, 8, 3), Some(7));
        assert_eq!(arrow_target(D, None, 0, 3), None);
    }
}
