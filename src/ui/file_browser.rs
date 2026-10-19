use std::path::{Path, PathBuf};

use eframe::epaint::text::cursor::CCursor;
use egui::{
    Align, Area, Button, Color32, DragAndDrop, Event, Frame, Id, Key, Layout, Modal, Modifiers,
    Panel, Response, RichText, ScrollArea, Sense, Stroke, StrokeKind, TextEdit, TextStyle, Ui,
    text::CCursorRange,
};

use crate::{
    OutType, clipboard, drag_out,
    files::{self, FileSort, SortKey},
    path_color, render_with_tilde,
    state::{FileCreate, FileRename, State},
    ui,
    ui::{
        COLOR_EXECUTABLE, FILES_COL_MIN_CHARS, ListAction,
        dirs::{BOOKMARKED_TEXT, bookmarked_fill},
        search,
    },
    viewer,
};

/// Files in the active tab's cwd, as a selectable list. Folders are
/// sorted first (then by the Name / Modified / Type / Size buttons) and
/// prefixed with 📁, on green when bookmarked (as in
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
/// them. As in a file manager, Ctrl+click adds a row to the selection (or
/// takes it out), Shift+click or Shift+arrow selects the rows from the one
/// last clicked, Ctrl+A selects them all, and a click on the background,
/// Escape or the Unselect button (shown while anything's selected) selects
/// none; the pane then totals up what's selected. The divider above
/// that pane drags up and down (see [ui::Divider]).
/// Double-clicking a folder row navigates into
/// it (same path as clicking a bookmark or recent dir), and middle-clicking
/// opens it in a new tab; double-clicking a
/// Markdown or plain-text (`.txt`, `.gitignore`, `.gitattributes`, `LICENSE`)
/// file opens it in a viewer
/// window (see `crate::viewer`); any other file opens with the OS's default
/// associated program (see
/// [open_with_default_app]). Clicking the already-selected row again renames
/// it in place (see [rename_box]): Enter or clicking away applies the new
/// name, Escape cancels.
///
/// Cut / Copy (or Ctrl+X / C) put the selected rows on the OS clipboard, and
/// Paste (Ctrl+V) copies or moves the files there into this dir, so files
/// can go to and from the OS file manager too (see `crate::clipboard`).
/// Delete (or the red trash button) confirms before moving the selected
/// files and folders to the OS's Recycle Bin / Trash.
/// Dragging rows onto a folder (or onto ⬆ or a shortcut button) moves them
/// there, or copies them with Ctrl held (see [drop_target]). Dragged out of
/// the window, they can be dropped on other programs, as Explorer's can (see
/// [drag_out()]).
///
/// +📁 (or Ctrl+Shift+N) and +🗋 make a new folder or empty file in this
/// dir, as `mkdir` and `touch` do: it's named in an edit box above the
/// listing (see [create_box]), and Enter or clicking away makes it and
/// selects it, while Escape or a blank name makes nothing.
pub(crate) fn file_browser(state: &mut State, ui: &mut Ui) {
    // Measured before anything is laid out, so the details pane's share is
    // of the whole panel rather than of what's left under the header.
    let split = &mut state.ui.files_details_split;
    let divider = ui::Divider::begin(ui, "files_details_divider", split);
    // The whole pane, for the mouse Back/Forward buttons below.
    let pane_rect = ui.max_rect();

    // A click in this pane gives it Enter, the arrow keys and Ctrl+A / C /
    // X / V; a click anywhere else takes them away.
    let modal_open =
        state.ui.delete_pending.is_some() || ui.memory(|m| m.top_modal_layer().is_some());
    if !modal_open && ui.input(|i| i.pointer.any_pressed()) {
        let active = ui.rect_contains_pointer(pane_rect);
        state.ui.files_activating_press = active && !state.ui.files_active;
        state.ui.files_active = active;
    }

    // Consume Enter before drawing the widgets, so the quick filter keeps
    // focus and a selected row's button doesn't turn it into a rename click.
    // Other text boxes (including Deep search and rename) keep their Enter.
    let search_id = ui.make_persistent_id(egui::IdSalt::new("files_search"));

    let focused = ui.memory(|m| m.focused());
    let search_has_keys = !modal_open && state.ui.rename.is_none() && focused == Some(search_id);
    let list_has_keys = !modal_open
        && state.ui.rename.is_none()
        && state.ui.files_active
        && !ui.ctx().text_edit_focused();

    let open_selected = (search_has_keys || list_has_keys)
        && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));

    if open_selected {
        state.ui.rename_pending = None;
    }

    // Likewise the arrow keys, which move the selection, or with Shift,
    // extend it: Up / Down from the quick filter too, but Left / Right only
    // from the listing, so they still move the filter's caret. Not while
    // Deep search results stand in for the listing.
    let mut arrows = Vec::new();
    if state.deep_search.is_none() {
        use Key::{ArrowDown, ArrowLeft, ArrowRight, ArrowUp};
        ui.input_mut(|i| {
            i.events.retain(|e| {
                let Event::Key {
                    key: key @ (ArrowUp | ArrowDown | ArrowLeft | ArrowRight),
                    pressed: true,
                    modifiers,
                    ..
                } = e
                else {
                    return true;
                };

                let shift = modifiers.matches_exact(Modifiers::SHIFT);
                let vertical = matches!(key, ArrowUp | ArrowDown);
                let ours = list_has_keys || (search_has_keys && vertical);

                if ours && (shift || modifiers.is_none()) {
                    arrows.push((*key, shift));
                    return false;
                }

                true
            });
        });
    }

    if !arrows.is_empty() {
        // egui would otherwise also move keyboard focus off the quick filter.
        ui.memory_mut(|m| m.move_focus(egui::FocusDirection::None));
    }

    if list_has_keys && ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::A)) {
        state.select_all_files();
        state.ui.rename_pending = None;
    }

    // Escape selects nothing, as Unselect does. egui has already taken the
    // keyboard from a text box for it, so from the quick filter one press does
    // both; a rename box keeps it, to cancel the rename, and an open menu, to
    // close.
    if list_has_keys
        && !state.selection.paths.is_empty()
        && !egui::Popup::is_any_open(ui.ctx())
        && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
    {
        state.clear_file_selection();
        state.ui.rename_pending = None;
    }

    // No header here, to save vertical space; it's obvious this is the file browser.
    // ui::panel_header(ui, "Files", &mut state.ui.panel_vis.file_browser);

    let bookmarked = state.dir_bookmarks.iter().any(|p| p == state.cwd());
    let home = state.home.clone();
    let dark = ui.visuals().dark_mode;
    let cwd_job = if bookmarked && !dark {
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
                        .fill(bookmarked_fill(dark))
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
        if !bookmarked {
            let cwd = state.cwd().to_path_buf();
            state.touch_recent_dir(&cwd);
        }
    }

    if open_clicked {
        let cwd = state.cwd().to_path_buf();
        shell::commands::open_dir(&cwd, &mut |kind, msg| {
            state.push_out(msg, crate::out_type_for(kind));
        });
        state.touch_recent_dir(&cwd);
    }

    // Browser-style navigation row. Up goes to the parent dir; Back/Forward
    // walk the per-tab history maintained in `State::change_dir`. Buttons are
    // disabled when there's nowhere to go in that direction.
    let i = state.ui.active_tab;
    let parent = state.cwd().parent().map(Path::to_path_buf);
    let can_back = !state.ui.nav_back[i].is_empty();
    let can_forward = !state.ui.nav_forward[i].is_empty();

    // Common-destination shortcuts (see [files::shortcuts]): 🏠 Home,
    // ⚙ app data, 🖥 Desktop, 📥 Downloads, 📦 programs, 🖴 root, 🗑 trash.
    // Each is only enabled when the target actually exists, so we never `cd`
    // into a missing dir and push a diagnostic. On Windows, 🖴 is a menu of
    // drives and 🗑 opens the Recycle Bin in Explorer. Rows dragged onto ⬆
    // or a shortcut go to that dir.

    let shortcuts = files::shortcuts(home.as_deref());
    let mut go_up = false;
    let mut go_back = false;
    let mut go_forward = false;
    let mut go_to: Option<PathBuf> = None;
    let mut dropped: Option<FileDrop> = None;
    #[cfg(windows)]
    let mut open_bin = false;

    // Wraps so the shortcuts drop to a second row in a narrow panel.
    ui.horizontal_wrapped(|ui| {
        let up = ui
            .add_enabled(parent.is_some(), Button::new("⬆"))
            .on_hover_text("Parent directory");
        go_up = up.clicked();
        if let Some(parent) = &parent
            && let Some(d) = drop_target(ui, &up, parent)
        {
            dropped = Some(d);
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
            let dir = s.dir.filter(|d| d.is_dir());
            let resp = ui
                .add_enabled(dir.is_some(), Button::new(s.glyph))
                .on_hover_text(s.hover);
            if let Some(dir) = &dir
                && let Some(d) = drop_target(ui, &resp, dir)
            {
                dropped = Some(d);
            }
            if resp.clicked() {
                go_to = dir;
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

    if let Some(d) = dropped {
        state.drop_files(&d.srcs, &d.dest, d.copy, ui.ctx());
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
    // only the dotfile filter appears. Then Cut / Copy / Paste / Delete, the
    // sort order (see [files::sort_browser_files]), and while anything's
    // selected, Unselect. Wraps so the latter drop to a second row
    // in a narrow panel.
    let has_sel = !state.selection.paths.is_empty();
    let pasting = state.paste.is_some();
    let busy = pasting || state.deleting.is_some();
    let mut filters_changed = false;
    let mut new_sort: Option<FileSort> = None;

    let (mut cut, mut copy, mut paste, mut unselect) = (false, false, false, false);
    let mut new_file = false;

    // Explorer's shortcut for it.
    let mut new_folder = list_has_keys
        && ui.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::N));
    let mut delete = list_has_keys
        && has_sel
        && !busy
        && !egui::Popup::is_any_open(ui.ctx())
        && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Delete));

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

        new_folder |= ui
            .button("+📁")
            .on_hover_text("New folder, named in a box above the listing (Ctrl+Shift+N)")
            .clicked();

        new_file = ui
            .button("+🗋")
            .on_hover_text("New empty file, named (extension and all) in a box above the listing")
            .clicked();

        ui.separator();

        cut = ui
            .add_enabled(has_sel, Button::new("✂"))
            .on_hover_text("Cut the selected items, to move them where they're pasted (Ctrl+X)")
            .clicked();

        copy = ui
            .add_enabled(has_sel, Button::new("🗐"))
            .on_hover_text("Copy the selected items to the clipboard (Ctrl+C)")
            .clicked();

        paste = ui
            .add_enabled(!busy, Button::new("📋"))
            .on_hover_text(
                "Paste files from the clipboard into this folder (Ctrl+V).                  Works with files copied or cut in the system file manager too.",
            )
            .clicked();

        let spacing = ui.spacing().item_spacing.x;
        ui.spacing_mut().item_spacing.x = 12.0;
        delete |= ui
            .add_enabled(has_sel && !busy, delete_button("🗑"))
            .on_hover_text("Move the selected items to the Recycle Bin / Trash (Delete)")
            .clicked();
        ui.spacing_mut().item_spacing.x = spacing;

        // Its space is kept while it's hidden, so it never wraps the row
        // anew and shifts the listing under the pointer as a click selects.
        unselect = ui
            .add_visible(has_sel, Button::new("Unselect"))
            .on_hover_text("Unselect all items (Esc)")
            .clicked();

        if busy {
            ui.spinner();
        }
    });

    sort_buttons(&mut new_sort, state.ui.files_sort, ui);

    if filters_changed {
        state.refresh_browser_files();
        let _ = state.save();
    }

    if let Some(sort) = new_sort {
        state.set_files_sort(sort);
    }

    if new_folder || new_file {
        state.start_create(new_folder);
    }

    if cut || copy {
        state.copy_selected(cut);
    }

    if unselect {
        state.clear_file_selection();
        state.ui.rename_pending = None;
    }

    if delete {
        state.request_delete_selected();
    }

    // The same from the keyboard, while this pane has it: it was clicked
    // last, and no text box elsewhere has taken the keyboard since. Its own
    // search boxes leave the keys to the files, but text selected in them,
    // or in the details pane, is copied as usual instead. Since egui only
    // copies that at the end of the frame, the files wait for the next one
    // to see whether it did (see `ui::clipboard::capture`).
    let deep_search_id = search::deep_search_id();
    let own_box_focused = focused.is_some_and(|id| id == search_id || id == deep_search_id);
    if !modal_open
        && state.ui.files_active
        && state.ui.rename.is_none()
        && (own_box_focused || !ui.ctx().text_edit_focused())
    {
        let mut paste_key = false;
        ui.input(|i| {
            for e in &i.events {
                match e {
                    Event::Cut => state.ui.files_copy_pending = Some(true),
                    Event::Copy => state.ui.files_copy_pending = Some(false),
                    // egui-winit swallows Ctrl+V's key press, and only sends
                    // a Paste event when there's text on the clipboard, which
                    // there isn't for copied files. So paste on its release.
                    Event::Key {
                        key: Key::V,
                        pressed: false,
                        modifiers,
                        ..
                    } if modifiers.command => paste_key = true,
                    _ => {}
                }
            }
        });
        if state.ui.files_copy_pending.is_some() {
            ui.ctx().request_repaint();
        }

        // In a search box, text on the clipboard was pasted into it.
        paste |=
            paste_key && (!own_box_focused || clipboard::get_files().is_ok_and(|f| f.is_some()));
    }

    if paste {
        state.paste_files(ui.ctx());
    }

    ui.horizontal(|ui| {


        // Text filter on names in this dir; cleared on cd (see
        // [State::refresh_browser_files]).
        let search_changed = search::search_box(ui, &mut state.ui.files_search, "files_search");
        if search_changed {
            state.refresh_browser_files_filter();
        }
        ui.add_space(20.);
        search::deep_search_bar(state, ui);

        if search_changed {
            state.ui.rename_pending = None;
            match state.shown_files().first().map(|p| p.to_path_buf()) {
                Some(path) => state.select_file(&path),
                None => state.clear_file_selection(),
            }
        }
    });

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

    // A picture only while a single image is selected.
    let image = state
        .selected_file
        .as_ref()
        .filter(|d| state.selection.summary.is_none() && !d.is_folder && viewer::is_image(&d.path))
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
            ui::file_details(
                state.selected_file.as_ref(),
                state.selection.summary.as_ref(),
                home,
                &state.file_preview,
                ui,
            );
        });

    // A new folder or file being named. Outside the listing's scroll area,
    // so it stays in view.
    let mut created = false;
    if let Some(c) = &mut state.ui.create {
        match create_box(ui, c) {
            Some(true) => {
                state.commit_create();
                created = true;
            }
            Some(false) => state.ui.create = None,
            None => {}
        }
    }

    // While a Deep search is set, its results stand in for the listing.
    if let Some(search) = &state.deep_search {
        let home = state.home.as_deref();
        let action = search::deep_search_results(
            search,
            &state.ui.files_search,
            &state.selection.paths,
            &state.dir_bookmarks,
            home,
            ui,
        );
        apply_row_action(state, ui, action);
    } else {
        // A new entry is scrolled into view, as the arrow keys' are.
        file_list(state, ui, search_changed, arrow_moved || created);
    }

    // After the list, so the divider is on top of it for the pointer.
    if divider.end(ui, details.response.rect, &mut state.ui.files_details_split) {
        let _ = state.save();
    }

    drag_preview(ui);
    drag_out(state, ui);
}

fn delete_button(label: &str) -> Button<'_> {
    Button::new(RichText::new(label).color(Color32::WHITE)).fill(Color32::from_rgb(180, 45, 45))
}

/// Shown outside the pane so it remains available if the pane is hidden.
pub(super) fn delete_popup(state: &mut State, ctx: &egui::Context) {
    let Some(paths) = &state.ui.delete_pending else {
        return;
    };
    let destination = if cfg!(windows) {
        "Recycle Bin"
    } else {
        "Trash"
    };

    let mut confirm = false;
    let mut cancel = false;

    let modal = Modal::new(Id::new("delete_files_popup")).show(ctx, |ui| {
        ui::scale_panel_fonts(ui, state.gui_font_size);
        ui.set_width(360.0_f32.min((ctx.content_rect().width() - 48.0).max(100.0)));
        ui.heading("Delete selected items?");

        let items = ui::plural(paths.len(), "item", "items");
        ui.label(format!("Move {items} to the {destination}?"));
        ui.add_space(4.0);

        ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
            for path in paths {
                ui.label(render_with_tilde(path, state.home.as_deref()));
            }
        });

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let delete = ui.add(delete_button("Delete"));
            let keep = ui.button("Cancel");
            // Keep keyboard focus in the modal, starting on Delete, so Enter confirms.
            if !delete.has_focus() && !keep.has_focus() {
                delete.request_focus();
            }
            confirm = delete.clicked();
            cancel = keep.clicked();
        });
    });

    if confirm {
        state.confirm_delete(ctx);
    } else if cancel || modal.should_close() {
        state.ui.delete_pending = None;
    }

    if state.ui.delete_pending.is_none() {
        ctx.request_repaint();
    }
}

/// Move the Files selection by `arrows`, each as [arrow_target] says, in the
/// listing as last laid out; those with Shift (`true`) extend it instead,
/// as Shift+click does. Returns whether it moved.
fn move_selection(state: &mut State, arrows: &[(Key, bool)]) -> bool {
    let mut moved = false;
    for &(key, shift) in arrows {
        let rows = &state.ui.browser_files_filter;
        let path_at = |k: usize| &state.browser_files[rows[k]].path;
        let from = state
            .selected_file
            .as_ref()
            .and_then(|d| (0..rows.len()).find(|&k| *path_at(k) == d.path));

        let to = arrow_target(key, from, rows.len(), state.ui.files_columns);
        let Some(to) = to.filter(|&to| Some(to) != from) else {
            continue;
        };

        let path = path_at(to).clone();
        if shift {
            state.select_range(&path, false);
        } else {
            state.select_file(&path);
        }
        moved = true;
    }

    if moved {
        state.ui.rename_pending = None;
    }
    moved
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

    let selected = &state.selection.paths;
    let lead = state.selected_file.as_ref().map(|d| d.path.as_path());
    let dark = ui.visuals().dark_mode;
    let mut action: Option<RowAction> = None;

    let mut rename_done: Option<bool> = None;

    let columns = ScrollArea::vertical()
        .id_salt("file_browser_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // Under the rows, so it only gets clicks between and below them.
            if let Some(a) = background_action(ui) {
                action = Some(a);
            }
            drag_autoscroll(ui);

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
                    if let Some(apply) = rename_box(ui, r, f.is_folder) {
                        rename_done = Some(apply);
                    }
                    return;
                }

                let text = if f.is_folder {
                    format!("📁 {}", f.disp_name)
                } else {
                    f.disp_name.clone()
                };

                let is_selected = selected.contains(&f.path);
                // A bookmarked folder is drawn on the same green as in
                // Recent dirs, unless it's selected so the selection shows.
                let bookmarked =
                    f.is_folder && !is_selected && state.dir_bookmarks.contains(&f.path);

                let mut rich = RichText::new(text).monospace();
                if f.is_folder {
                    if bookmarked && !dark {
                        rich = rich.color(BOOKMARKED_TEXT);
                    }
                    // Same colour this folder has in any path shown elsewhere.
                    let path = render_with_tilde(&f.path, home.as_deref());
                    if let Some(c) = path_color::last_color(&path, Some("~"), dark) {
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
                    .sense(Sense::click_and_drag())
                    .min_size(egui::vec2(ui.available_width(), 0.0));
                if bookmarked {
                    row = row.fill(bookmarked_fill(dark)).frame_when_inactive(true);
                }

                let resp = ui.add(row);
                let is_lead = lead == Some(f.path.as_path());
                if is_lead && search_changed {
                    resp.scroll_to_me(Some(Align::TOP));
                } else if is_lead && arrow_moved {
                    resp.scroll_to_me(None);
                }

                let only_selected = is_selected && selected.len() == 1;
                if let Some(a) = row_action(ui, &resp, &f.path, f.is_folder, only_selected) {
                    action = Some(a);
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
    apply_row_action(state, ui, action);
}

/// What a click, drag or drop on a Files row asks for (in the listing, or
/// Deep search results), besides what the other lists' rows can.
pub(super) enum RowAction {
    /// Open the row, go into it, or open it in a new tab.
    List(ListAction),
    /// A plain click: select just this row.
    Select(PathBuf),
    /// A plain click on the only row selected, which renames it once it's
    /// clear the click isn't the first of a double-click.
    ClickSelected(PathBuf),
    /// Ctrl+click: add the row to the selection, or take it out.
    Toggle(PathBuf),
    /// Shift+click: select from the last row clicked to this one, adding to
    /// what's selected if Ctrl is held too (`true`).
    Range(PathBuf, bool),
    /// A click on the background: select nothing.
    Deselect,
    /// A drag of this row began, taking the rest of the selection with it
    /// if it's selected.
    Drag(PathBuf),
    Drop(FileDrop),
}

/// What `resp`, the Files row for `path`, was clicked, dragged or dropped
/// on for. `only_selected` says it's the one row selected. A folder takes
/// rows dropped on it (see [drop_target]).
pub(super) fn row_action(
    ui: &Ui,
    resp: &Response,
    path: &Path,
    is_folder: bool,
    only_selected: bool,
) -> Option<RowAction> {
    if is_folder && let Some(drop) = drop_target(ui, resp, path) {
        return Some(RowAction::Drop(drop));
    }
    let path = path.to_path_buf();
    if resp.drag_started_by(egui::PointerButton::Primary) {
        return Some(RowAction::Drag(path));
    }
    // A double-click's first click has already selected the row. Then a
    // folder navigates into it; a file opens with its associated program,
    // just like a desktop file manager. A middle click opens a folder in a
    // new tab.
    let action = if is_folder && resp.middle_clicked() {
        RowAction::List(ListAction::NewTabDir(path))
    } else if resp.double_clicked() {
        RowAction::List(if is_folder {
            ListAction::ChangeDir(path)
        } else {
            ListAction::OpenFile(path)
        })
    } else if resp.clicked() {
        let m = ui.input(|i| i.modifiers);
        if m.shift {
            RowAction::Range(path, m.command)
        } else if m.command {
            RowAction::Toggle(path)
        } else if only_selected {
            RowAction::ClickSelected(path)
        } else {
            RowAction::Select(path)
        }
    } else {
        return None;
    };
    Some(action)
}

/// Register the visible part of a Files list's `ui` (a [ScrollArea]'s
/// contents) for clicks before its rows, so it gets those that miss them:
/// a plain one selects nothing, as in a file manager.
pub(super) fn background_action(ui: &Ui) -> Option<RowAction> {
    let resp = ui.interact(
        ui.clip_rect(),
        ui.id().with("files_background"),
        Sense::click(),
    );
    let plain = ui.input(|i| !i.modifiers.command && !i.modifiers.shift);
    (resp.clicked() && plain).then_some(RowAction::Deselect)
}

/// Do what a click, drag or drop on a Files row asked for.
pub(super) fn apply_row_action(state: &mut State, ui: &Ui, action: Option<RowAction>) {
    let Some(action) = action else {
        return;
    };
    match action {
        RowAction::List(a) => ui::apply_action(state, Some(a)),
        RowAction::Select(p) => state.select_file(&p),
        RowAction::ClickSelected(p) => {
            // Not if the click only made the panel active, as in a file
            // manager, nor in Deep search results, which can't be renamed in
            // place.
            if state.deep_search.is_none() && !state.ui.files_activating_press {
                let now = ui.input(|i| i.time);
                let delay = ui.ctx().options(|o| o.input_options.max_double_click_delay);
                state.ui.rename_pending = Some((p, now));
                ui.ctx().request_repaint_after_secs(delay as f32);
            }
        }
        RowAction::Toggle(p) => state.toggle_file(&p),
        RowAction::Range(p, add) => state.select_range(&p, add),
        RowAction::Deselect => state.clear_file_selection(),
        RowAction::Drag(p) => {
            // An unselected row is selected first, as in a file manager: on
            // its own, or with Ctrl, along with the rest.
            if !state.selection.paths.contains(&p) {
                if ui.input(|i| i.modifiers.command) {
                    state.toggle_file(&p);
                } else {
                    state.select_file(&p);
                }
            }
            state.ui.rename_pending = None;
            let paths = state.selected_paths();
            if !paths.is_empty() {
                DragAndDrop::set_payload(ui.ctx(), FileDrag(paths));
            }
        }
        RowAction::Drop(d) => state.drop_files(&d.srcs, &d.dest, d.copy, ui.ctx()),
    }
}

/// The drag-and-drop payload while Files rows are dragged: their paths.
struct FileDrag(Vec<PathBuf>);

/// Files rows let go of over a folder (see [drop_target]).
pub(super) struct FileDrop {
    srcs: Vec<PathBuf>,
    dest: PathBuf,
    /// Copy them there, rather than move them.
    copy: bool,
}

/// What dropping the rows being dragged would do where the pointer is, as
/// [drop_target] found this frame, for [drag_preview].
#[derive(Clone, Default)]
struct DropHover {
    /// The folder's name.
    name: String,
    copy: bool,
    /// `false` when it's one of them, or inside one.
    ok: bool,
}

fn drop_hover_id() -> Id {
    Id::new("files_drop_hover")
}

/// Let `resp`, a folder row or a button that goes to `dest`, take Files rows
/// dropped on it: while they're held over it, it's outlined if they can go
/// there, and [drag_preview] says what letting go does. Returns the drop
/// once they're let go of. As in a file manager, they're moved there, or
/// copied when it's on another drive; Ctrl (Option on a Mac) copies anyway,
/// and Shift (Cmd) moves anyway.
pub(super) fn drop_target(ui: &Ui, resp: &Response, dest: &Path) -> Option<FileDrop> {
    let drag = resp.dnd_hover_payload::<FileDrag>()?;
    // Not into itself, or one of the folders being dragged.
    let ok = !drag.0.iter().any(|src| dest.starts_with(src));
    let m = ui.input(|i| i.modifiers);
    let mac = ui.ctx().os() == egui::os::OperatingSystem::Mac;
    let (copy_key, move_key) = if mac {
        (m.alt, m.mac_cmd)
    } else {
        (m.ctrl, m.shift)
    };
    let copy = copy_key
        || (!move_key
        && drag
        .0
        .first()
        .is_some_and(|src| !files::same_volume(src, dest)));
    let name = dest.file_name().map_or_else(
        || dest.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    ui.ctx()
        .data_mut(|d| d.insert_temp(drop_hover_id(), DropHover { name, copy, ok }));
    if !ok {
        return None;
    }
    let visuals = ui.visuals();
    let stroke = Stroke::new(2.0, visuals.widgets.active.fg_stroke.color);
    let radius = visuals.widgets.hovered.corner_radius;
    ui.painter()
        .rect_stroke(resp.rect, radius, stroke, StrokeKind::Inside);

    let drag = resp.dnd_release_payload::<FileDrag>()?;
    Some(FileDrop {
        srcs: drag.0.clone(),
        dest: dest.to_path_buf(),
        copy,
    })
}

/// While Files rows are dragged: a note by the pointer of what letting go
/// of them would do there, and a cursor to match.
fn drag_preview(ui: &Ui) {
    let ctx = ui.ctx();
    // Taken every frame, so it's only ever this frame's.
    let hover = ctx.data_mut(|d| d.remove_temp::<DropHover>(drop_hover_id()));
    let Some(drag) = DragAndDrop::payload::<FileDrag>(ctx) else {
        return;
    };

    // Out of the window, where [drag_out] hands them to the OS.
    let Some(pos) = ctx
        .pointer_latest_pos()
        .filter(|p| ctx.viewport_rect().contains(*p))
    else {
        return;
    };

    let what = match drag.0.as_slice() {
        [one] => one.file_name().map_or_else(
            || one.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        ),
        many => format!("{} items", many.len()),
    };

    let text = match hover {
        Some(h) if h.ok => {
            let verb = if h.copy { "Copy" } else { "Move" };
            let icon = if h.copy {
                egui::CursorIcon::Copy
            } else {
                egui::CursorIcon::Grabbing
            };

            ctx.set_cursor_icon(icon);
            format!("{verb} {what} to {}", h.name)
        }
        Some(h) => {
            ctx.set_cursor_icon(egui::CursorIcon::NoDrop);
            format!("Can't move {} into itself", h.name)
        }
        None => what,
    };

    Area::new(Id::new("files_drag_preview"))
        .order(egui::Order::Tooltip)
        .interactable(false)
        .fixed_pos(pos + egui::vec2(16.0, 16.0))
        .show(ctx, |ui| {
            Frame::popup(ui.style()).show(ui, |ui| {
                ui.label(text);
            });
        });
}

/// Hand Files rows dragged out of the window to the OS, so they can be
/// dropped on other programs (see `crate::drag_out`). That's the frame after
/// the pointer leaves with them, so the window, which doesn't repaint while
/// the OS has the drag on Windows, isn't left showing [drag_preview]'s note.
/// Should the pointer come back, the drag carries on here. On Linux the OS
/// has it on another thread, while this keeps drawing, and asks after it
/// each frame till it's over.
///
/// Once they're dropped elsewhere, the listing is read again then and a
/// second later: the program they went to may have moved them away, and
/// Explorer finishes doing that after the drop.
fn drag_out(state: &mut State, ui: &Ui) {
    let ctx = ui.ctx();
    let refresh_id = refresh_id();
    if let Some(at) = ctx.data(|d| d.get_temp::<f64>(refresh_id)) {
        let now = ctx.input(|i| i.time);
        if now >= at {
            ctx.data_mut(|d| d.remove_temp::<f64>(refresh_id));
            state.refresh_browser_files();
        } else {
            ctx.request_repaint_after_secs((at - now) as f32);
        }
    }

    let drag = DragAndDrop::payload::<FileDrag>(ctx);
    if let Some(going) = &mut state.ui.files_drag_out {
        // egui drops the rows for Escape, the button still held: so does
        // the OS. (Let go of, it's the OS's to say whether they dropped.)
        if drag.is_none() && ctx.input(|i| i.pointer.primary_down()) {
            going.cancel();
        }
        let Some(end) = going.poll() else {
            if let Some(icon) = going.cursor() {
                ctx.set_cursor_icon(icon);
            }
            return;
        };
        state.ui.files_drag_out = None;
        drag_ended(state, ctx, end);
        return;
    }

    let pending_id = Id::new("files_drag_out_pending");
    let out = ctx.input(|i| {
        i.pointer.primary_down()
            && i.pointer
            .latest_pos()
            .is_none_or(|p| !i.viewport_rect().contains(p))
    });
    let (Some(window), Some(drag), true) = (state.main_window, drag, out) else {
        ctx.data_mut(|d| d.remove_temp::<bool>(pending_id));
        return;
    };
    if !ctx
        .data_mut(|d| d.remove_temp::<bool>(pending_id))
        .unwrap_or(false)
    {
        ctx.data_mut(|d| d.insert_temp(pending_id, true));
        ctx.request_repaint();
        return;
    }

    let mut going = drag_out::start(window, &drag.0, ctx);
    match going.poll() {
        Some(end) => drag_ended(state, ctx, end),
        None => state.ui.files_drag_out = Some(going),
    }
    ctx.request_repaint();
}

/// When the listing's next read after rows were dropped on another
/// program (see [drag_out()]).
fn refresh_id() -> Id {
    Id::new("files_drag_out_refresh")
}

/// What's left to do once a drag handed to the OS is over (see
/// [drag_out()]).
fn drag_ended(state: &mut State, ctx: &egui::Context, end: Result<drag_out::Outcome, String>) {
    match end {
        Ok(drag_out::Outcome::Returned) => {}
        Ok(drag_out::Outcome::Ended { dropped }) => {
            DragAndDrop::clear_payload(ctx);
            if dropped {
                state.refresh_browser_files();
                let at = ctx.input(|i| i.time) + 1.0;
                ctx.data_mut(|d| d.insert_temp(refresh_id(), at));
            }
        }
        Err(e) => {
            DragAndDrop::clear_payload(ctx);
            state.push_out(e, OutType::StdErr);
        }
    }
    ctx.request_repaint();
}

/// Scroll a Files list's `ui` (a [ScrollArea]'s contents) while rows are
/// dragged near its top or bottom edge, the faster the closer, so they can
/// be dropped on folders out of view.
pub(super) fn drag_autoscroll(ui: &Ui) {
    if !DragAndDrop::has_payload_of_type::<FileDrag>(ui.ctx()) {
        return;
    }

    let Some(pos) = ui.ctx().pointer_latest_pos() else {
        return;
    };

    let view = ui.clip_rect();
    let edge = 1.5 * ui.text_style_height(&TextStyle::Monospace);
    if !view.x_range().contains(pos.x) {
        return;
    }

    // How far into the band along an edge (or past it) the pointer is.
    let depth = if (view.top() - edge..view.top() + edge).contains(&pos.y) {
        view.top() + edge - pos.y
    } else if (view.bottom() - edge..view.bottom() + edge).contains(&pos.y) {
        view.bottom() - edge - pos.y
    } else {
        return;
    };

    let dt = ui.input(|i| i.stable_dt).min(0.1);

    ui.scroll_with_delta(egui::vec2(0.0, 12.0 * depth * dt));
    ui.ctx().request_repaint();
}

fn sort_buttons(new_sort: &mut Option<FileSort>, files_sort: FileSort, ui: &mut Ui) {
    // The listing's order. Clicking the current one reverses it.
    let current = files_sort;

    ui.horizontal_wrapped(|ui| {
        for key in SortKey::ALL {
            let on = current.key == key;
            let label = match (on, current.descending) {
                (true, false) => format!("{} ⬆", key.label()),
                (true, true) => format!("{} ⬇", key.label()),
                (false, _) => key.label().to_owned(),
            };

            let hover = if on {
                format!("{}. Click to reverse the order.", key.hover())
            } else {
                key.hover().to_owned()
            };

            if ui
                .add(Button::selectable(on, label))
                .on_hover_text(hover)
                .clicked()
            {
                let descending = if on {
                    !current.descending
                } else {
                    key.default_descending()
                };

                *new_sort = Some(FileSort { key, descending });
            }
        }
    });
}

/// The edit box that stands in for a Files row while it's being renamed.
/// Returns `Some(true)` once editing ends with Enter or a click elsewhere,
/// and `Some(false)` if Escape cancelled it.
pub fn rename_box(ui: &mut Ui, r: &mut FileRename, is_folder: bool) -> Option<bool> {
    // Not derived from `ui`'s id, so it survives the row moving between the
    // list's columns (e.g. when a refresh changes the row count).
    let id = egui::Id::new("file_rename");
    let icon = is_folder.then_some("📁");
    name_box(ui, id, icon, &mut r.name, &mut r.focused, !is_folder, "")
}

/// The edit box above the Files listing where a new folder or file is
/// named (see [State::start_create]). Returns as [rename_box] does.
fn create_box(ui: &mut Ui, c: &mut FileCreate) -> Option<bool> {
    let (icon, hint) = if c.folder {
        ("📁", "New folder name")
    } else {
        ("🗋", "New file name, e.g. notes.txt")
    };
    let id = egui::Id::new("file_create");
    name_box(ui, id, Some(icon), &mut c.name, &mut c.focused, false, hint)
}

/// A one-line edit box for a Files entry's `name`, after its `icon`, that
/// takes focus the first frame it's drawn (as `focused` records) with the
/// name selected, or with `select_stem`, all but its extension. Returns as
/// [rename_box] does.
fn name_box(
    ui: &mut Ui,
    id: egui::Id,
    icon: Option<&str>,
    name: &mut String,
    focused: &mut bool,
    select_stem: bool,
    hint: &str,
) -> Option<bool> {
    ui.horizontal(|ui| {
        if let Some(icon) = icon {
            ui.label(RichText::new(icon).monospace());
        }
        let mut out = TextEdit::singleline(name)
            .id(id)
            .font(TextStyle::Monospace)
            .hint_text(hint)
            .desired_width(f32::INFINITY)
            .show(ui);

        if !*focused {
            // Like Explorer, select the name but not its extension, so typing
            // replaces just the name; a folder's whole name is selected.
            *focused = true;

            out.response.request_focus();

            let end = if select_stem {
                Path::new(name.as_str())
                    .file_stem()
                    .map_or(0, |s| s.to_string_lossy().chars().count())
            } else {
                name.chars().count()
            };

            out.state
                .cursor
                .set_char_range(Some(CCursorRange::two(CCursor::new(0), CCursor::new(end))));
            out.state.store(ui.ctx(), id);
        }
        out.response
            .lost_focus()
            .then(|| !ui.input(|i| i.key_pressed(Key::Escape)))
    })
        .inner
}
