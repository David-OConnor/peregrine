//! This module contains code related to the "Bookmarks" and "Recent dirs" panels. It includes
//! code used by one or the other, and shared between them.

use std::path::{Path, PathBuf};

use eframe::epaint::{Color32, FontId, text::LayoutJob};
use egui::{
    Button, CursorIcon, Modal, PointerButton, Pos2, Rangef, Rect, RichText, ScrollArea, Sense,
    Stroke, TextFormat, TextStyle, Ui,
};
use shell::{BookmarkSort, DirList};

use crate::{
    OutType, path_color,
    path_color::Part,
    render_with_tilde,
    state::State,
    ui,
    ui::{ListAction, search},
};

/// Default max displayed length (in characters) of a Bookmarks / Recent dirs
/// path, used when the state file has no saved `DIR_LABEL_LEN`. Longer paths
/// are truncated from the front (see [path_color::truncate_start]), e.g.
/// `.../code/Bio/dynamics`.
pub const DEFAULT_DIR_LABEL_LEN: usize = 22;
/// Bounds the path-length controls clamp to. The minimum leaves room for a
/// few characters after the `...`.
pub const MIN_DIR_LABEL_LEN: usize = 8;
pub const MAX_DIR_LABEL_LEN: usize = 200;

/// How the Bookmarks panel orders its rows until the user picks a mode at
/// its top, i.e. when the state file has no saved `BM_SORT`.
pub const DEFAULT_BOOKMARK_SORT: BookmarkSort = BookmarkSort::Proximity;

/// Bookmarks column. Narrower than the other side columns: bookmark paths are
/// truncated (to [DEFAULT_DIR_LABEL_LEN] chars unless the user changes it), so
/// this only needs to fit the `"<i>: "` prefix plus that path and the trailing
/// `x` button.
pub(super) const BOOKMARK_COL_WIDTH: f32 = 170.;
/// Recent-dirs column. Paths are truncated like bookmarks, with the same
/// `"<i>: "` prefix and trailing `x`.
pub(super) const RECENT_DIRS_COL_WIDTH: f32 = 170.;

/// Background of a Recent dirs or Files row whose dir is also bookmarked,
/// per panel theme: pale green in light mode, a muted dark green in dark mode
/// so it doesn't stand out from the rest of the window.
const BOOKMARKED_FILL_LIGHT: Color32 = Color32::from_rgb(220, 240, 220);
const BOOKMARKED_FILL_DARK: Color32 = Color32::from_rgb(40, 90, 50);
/// Uncoloured text (the index, separators) on a bookmarked row in light mode.
/// In dark mode the row keeps the theme's own text colour.
pub(super) const BOOKMARKED_TEXT: Color32 = Color32::from_gray(30);

/// The background of a bookmarked row (see [BOOKMARKED_FILL_LIGHT]).
pub(super) fn bookmarked_fill(dark: bool) -> Color32 {
    if dark {
        BOOKMARKED_FILL_DARK
    } else {
        BOOKMARKED_FILL_LIGHT
    }
}
/// The filled ★ of a bookmarked dir (see [bookmark_star]).
const STAR_COLOR: Color32 = Color32::from_rgb(225, 165, 0);
const STAR_ON: char = '★';
const STAR_OFF: char = '☆';

/// How a Bookmarks / Recent dirs panel labels its rows' dirs.
#[derive(Clone, Copy)]
struct DirLabelOpts<'a> {
    home: Option<&'a Path>,
    /// Max characters of each path or name; see [dir_label].
    max_len: usize,
    /// Show just each dir's final name ("Names"), not its path ("Paths").
    names: bool,
    /// With `names`, colour each name as its parent dir rather than as
    /// itself, so dirs that share a parent read as a group.
    parent_color: bool,
}

/// What a Bookmarks / Recent dirs row shows of `path`: all of it as `~/...`,
/// colour-coded by directory (see [path_color]), or with `opts.names`, just
/// its final directory, e.g. `molchanica` for `~/code/Bio/molchanica`, in
/// its own colour or its parent's (see [DirLabelOpts::parent_color]). `dark`
/// picks the colours for the background it will be drawn on.
fn dir_parts(path: &Path, opts: DirLabelOpts, dark: bool) -> Vec<Part> {
    let mut parts = path_color::parts(&render_with_tilde(path, opts.home), Some("~"), dark);
    if opts.names
        && let Some(last) = parts.iter().rposition(|p| p.color.is_some())
    {
        let parent = parts[..last].iter().rev().find_map(|p| p.color);
        parts.drain(..last);
        // A dir with no parent (e.g. `~`) keeps its own colour.
        if opts.parent_color && parent.is_some() {
            parts[0].color = parent;
        }
    }
    parts
}

/// Total characters in `parts`.
fn parts_len(parts: &[Part]) -> usize {
    parts.iter().map(|p| p.text.chars().count()).sum()
}

/// A Bookmarks / Recent dirs button label: `prefix` (the index), then `parts`
/// (see [dir_parts]) with the final directory bold, and truncated from the
/// front to `max_len` chars so the column stays narrow. Uncoloured runs are
/// `text_color`.
fn dir_label(
    ui: &Ui,
    prefix: String,
    parts: Vec<Part>,
    max_len: usize,
    text_color: Color32,
) -> LayoutJob {
    let mut label = vec![Part::plain(prefix)];
    label.extend(path_color::truncate_start(parts, max_len));
    let font = TextStyle::Button.resolve(ui.style());
    let bold = FontId::new(font.size, ui::bold_family());
    path_color::layout_job(&label, font, Some(bold), text_color)
}

/// A Bookmarks / Recent dirs row's button, labelled as [dir_label]. With
/// `highlight` (a Recent dirs row that's also bookmarked), it's drawn on
/// [bookmarked_fill]. When the label doesn't show the whole path (it's just
/// the name, or truncated), the hover text does.
fn dir_button(
    ui: &mut Ui,
    prefix: String,
    path: &Path,
    opts: DirLabelOpts,
    highlight: bool,
) -> egui::Response {
    let dark = ui.visuals().dark_mode;
    let text_color = if highlight && !dark {
        BOOKMARKED_TEXT
    } else {
        Color32::PLACEHOLDER
    };
    let parts = dir_parts(path, opts, dark);
    let whole = !opts.names && parts_len(&parts) <= opts.max_len;
    let mut button = Button::new(dir_label(ui, prefix, parts, opts.max_len, text_color));

    if highlight {
        button = button.fill(bookmarked_fill(dark));
    }

    let resp = ui.add(button);
    if whole {
        resp
    } else {
        resp.on_hover_text(ui::path_job(ui, path, opts.home))
    }
}

/// What a click on a Bookmarks / Recent dirs row (`list`) for `path` does: a
/// left click cd's there, a middle click opens it in a new tab.
fn dir_click(resp: &egui::Response, path: &Path, list: DirList) -> Option<ListAction> {
    if resp.middle_clicked() {
        Some(ListAction::NewTabSavedDir(path.to_path_buf(), list))
    } else if resp.clicked() {
        Some(ListAction::ChangeSavedDir(path.to_path_buf(), list))
    } else {
        None
    }
}

/// The Files and Recent dirs button that adds a dir to Bookmarks, or removes
/// it: a gold ★ while it's bookmarked, otherwise ☆. `small` as for
/// [Ui::small_button].
pub(super) fn bookmark_star(ui: &mut Ui, bookmarked: bool, small: bool) -> egui::Response {
    let (text, hover) = if bookmarked {
        (
            RichText::new(STAR_ON.to_string()).color(STAR_COLOR),
            "Remove this dir from bookmarks",
        )
    } else {
        (RichText::new(STAR_OFF.to_string()), "Bookmark this dir")
    };

    let mut button = Button::new(text);
    if small {
        button = button.small();
    }
    ui.add(button).on_hover_text(hover)
}

/// The Bookmarks panel button that toggles the current dir's bookmark, named
/// by its final directory: "☆ Bookmark synthesis" for `~/code/bio_web/synthesis`,
/// or "x Rem synthesis" once it's bookmarked. The hover text has the full path.
fn cwd_bookmark_button(ui: &mut Ui, state: &State) -> egui::Response {
    let cwd = state.cwd();
    let bookmarked = state.dir_bookmarks.iter().any(|p| p == cwd);
    // A root (e.g. `C:\`) has no final name; show it whole.
    let name = cwd.file_name().map_or_else(
        || cwd.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );

    let (text, hover) = if bookmarked {
        (
            format!("x Rem {name}"),
            "Remove the current directory from bookmarks",
        )
    } else {
        (
            format!("{STAR_OFF} {name}"),
            "Make the current directory a bookmark",
        )
    };

    let path = ui::path_job(ui, cwd, state.home.as_deref());
    ui.button(text).on_hover_ui(|ui| {
        ui.label(hover);
        ui.label(path);
    })
}

/// The most characters any of `paths` takes in its [dir_label]:
/// `opts.max_len`, unless they're all shorter, as names usually are. Taken
/// over the whole list, not just the rows the search box leaves, so the
/// columns don't shift while typing.
fn dir_label_chars<'a>(
    ui: &Ui,
    paths: impl Iterator<Item = &'a PathBuf>,
    opts: DirLabelOpts,
) -> usize {
    let dark = ui.visuals().dark_mode;
    paths
        .map(|p| parts_len(&dir_parts(p, opts, dark)))
        .max()
        .unwrap_or(0)
        .min(opts.max_len)
}

/// The `Paths | Names` selector at the top of the Bookmarks / Recent dirs
/// panels: whether each row shows the dir's path or just its final name. How
/// many characters of it to show is in the Settings window. `what` names the
/// panel's dirs, for the tooltips. Returns true if it changed.
fn dir_label_mode_control(ui: &mut Ui, names: &mut bool, what: &str) -> bool {
    let before = *names;

    ui.horizontal(|ui| {
        ui.selectable_value(names, false, "Paths")
            .on_hover_text(format!(
                "Show each {what}'s path, e.g. ~/code/Bio/molchanica"
            ));

        ui.selectable_value(names, true, "Names")
            .on_hover_text(format!(
                "Show just each {what}'s final name, e.g. molchanica"
            ));
    });

    *names != before
}

/// Width of a (non-small) button labelled `text`, padding included.
pub(super) fn button_width(ui: &Ui, text: &str) -> f32 {
    let font = TextStyle::Button.resolve(ui.style());
    let text_width = ui.ctx().fonts_mut(|f| {
        f.layout_no_wrap(text.to_owned(), font, Color32::PLACEHOLDER)
            .size()
            .x
    });
    text_width + 2.0 * ui.spacing().button_padding.x
}

/// The Bookmarks / Recent dirs "Cleanup" button, for `list`. Returns
/// true when clicked.
pub(super) fn cleanup_missing_button(ui: &mut Ui) -> bool {
    ui.button("Cleanup")
        .on_hover_text(
            "Remove bookmarks and recent directory history that no longer exist on disk. Only the list changes; \
             nothing on disk is touched.",
        )
        .clicked()
}

/// Width a Bookmarks / Recent dirs column needs so none of its rows spill into
/// the next: a `prefix_chars`-long index, a `label_chars`-long path or name
/// (see [dir_label_chars]), then a small button for each of `buttons` (e.g.
/// the `x`).
fn dir_col_width(ui: &Ui, prefix_chars: usize, label_chars: usize, buttons: &[char]) -> f32 {
    let s = ui.spacing();
    let font = TextStyle::Button.resolve(ui.style());
    let glyphs: f32 = ui
        .ctx()
        .fonts_mut(|f| buttons.iter().map(|&c| f.glyph_width(&font, c)).sum());

    // Every button's padding, and the gaps between them.
    let n = buttons.len() as f32;

    let extra = 2.0 * (n + 1.0) * s.button_padding.x + n * s.item_spacing.x;
    ui::chars_width(ui, TextStyle::Button, (prefix_chars + label_chars) as f32) + glyphs + extra
}

/// The order the Bookmarks panel lists `bookmarks` in for `sort`, as indices
/// into it. `custom` is the order for [BookmarkSort::Custom]; bookmarks it
/// doesn't name (added since) follow it, oldest first.
pub fn sort_bookmarks(bookmarks: &[PathBuf], sort: BookmarkSort, custom: &[PathBuf]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..bookmarks.len()).collect();

    // These sorts are stable, so ties stay oldest first.
    match sort {
        BookmarkSort::Added => {}
        // The same name in two places goes by path.
        BookmarkSort::Alpha => {
            order.sort_by_cached_key(|&i| (dir_name_key(&bookmarks[i]), tree_key(&bookmarks[i])))
        }
        BookmarkSort::Proximity => order.sort_by_cached_key(|&i| tree_key(&bookmarks[i])),
        BookmarkSort::Custom => order.sort_by_cached_key(|&i| {
            custom
                .iter()
                .position(|p| *p == bookmarks[i])
                .unwrap_or(usize::MAX)
        }),
    }

    order
}

/// `path`'s final name, lowercased, to sort by. A root (e.g. `C:\`), which
/// has none, sorts as itself.
fn dir_name_key(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .to_lowercase()
}

/// `path`'s components, lowercased, to sort by: each dir then comes right
/// after its parent, among its siblings, so dirs near each other in the tree
/// sit together. Sorting the path as text wouldn't quite: `a b` would split
/// `a` from `a/x`, since a space sorts before the separator.
fn tree_key(path: &Path) -> Vec<String> {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect()
}

/// The Bookmarks panel's sort modes, with each button's label and hover text.
const BOOKMARK_SORTS: [(BookmarkSort, &str, &str); 4] = [
    (
        BookmarkSort::Alpha,
        "A-Z",
        "Alphabetically, by each dir's final name",
    ),
    (
        BookmarkSort::Added,
        "Date",
        "In the order they were bookmarked, oldest first",
    ),
    (
        BookmarkSort::Proximity,
        "Prox",
        "By path, so dirs in the same part of the file system sit together",
    ),
    (
        BookmarkSort::Custom,
        "Custom",
        "In the order you drag them into. Drag a row to move it",
    ),
];

/// Width of the line marking where a dragged bookmark will land.
const BOOKMARK_DROP_MARKER_WIDTH: f32 = 2.;

/// The `Sort: A-Z | Added | Proximity | Custom` selector at the top of the
/// Bookmarks panel (see [BOOKMARK_SORTS]), wrapping when the panel is narrow.
/// Returns the mode picked, if it changed.
fn bookmark_sort_control(ui: &mut Ui, current: BookmarkSort) -> Option<BookmarkSort> {
    let mut sort = current;

    ui.horizontal_wrapped(|ui| {
        ui.label("Sort:");
        for (mode, label, hover) in BOOKMARK_SORTS {
            ui.selectable_value(&mut sort, mode, label)
                .on_hover_text(hover);
        }
    });

    (sort != current).then_some(sort)
}

/// Where a bookmark dragged to `pos` would land: the gap before or after the
/// row nearest it, as a place in the Bookmarks panel's order, with the `y`
/// and x range to mark that gap at. `rows` are the rows drawn, by number,
/// each as wide as its column. `gap` is the space between rows.
fn bookmark_drop_slot(rows: &[(usize, Rect)], pos: Pos2, gap: f32) -> Option<(usize, f32, Rangef)> {
    let &(n, rect) = rows.iter().min_by(|a, b| {
        a.1.distance_sq_to_pos(pos)
            .total_cmp(&b.1.distance_sq_to_pos(pos))
    })?;

    let (slot, y) = if pos.y < rect.center().y {
        (n, rect.top() - gap / 2.)
    } else {
        (n + 1, rect.bottom() + gap / 2.)
    };

    Some((slot, y, rect.x_range()))
}

/// Display directory bookmarks as a clickable list, in columns when the panel
/// is wide enough, in the order picked at the top (see [sort_bookmarks]).
/// Clicking a row cd's to that bookmark, and middle-clicking opens it in a new
/// tab; in Custom order, dragging it moves it. The `x` removes it from the
/// list, and "Cleanup missing" removes every bookmark whose dir is gone. The
/// search box filters by case-insensitive substring against the full path.
pub(super) fn dir_bookmarks(state: &mut State, ui: &mut Ui) {
    ui::panel_header(ui, "Bookmarks", &mut state.ui.panel_vis.bookmarks);

    if dir_label_mode_control(ui, &mut state.bookmark_names, "bookmark") {
        let _ = state.save();
    }

    if let Some(sort) = bookmark_sort_control(ui, state.bookmark_sort) {
        state.set_bookmark_sort(sort);
    }

    ui.horizontal_wrapped(|ui| {
        if cwd_bookmark_button(ui, state).clicked() {
            let msg = state.toggle_bookmark_cwd();
            state.push_out(msg, OutType::StdOut);
        }
    });

    if search::search_box(ui, &mut state.ui.bookmarks_search, "bookmarks_search") {
        state.refresh_bookmarks_filter();
    }
    ui.separator();

    let home = state.home.clone();
    let opts = DirLabelOpts {
        home: home.as_deref(),
        max_len: state.bookmark_label_len,
        names: state.bookmark_names,
        parent_color: false,
    };
    let mut action: Option<ListAction> = None;

    ScrollArea::vertical()
        .id_salt("bookmarks_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let sorted = &state.ui.bookmarks_sorted;
            // `"<n>: "`, sized for the largest number.
            let prefix_chars = sorted.len().to_string().len() + 2;
            let label_chars = dir_label_chars(ui, state.dir_bookmarks.iter(), opts);
            let min_width = dir_col_width(ui, prefix_chars, label_chars, &['x']);

            // Rows move by dragging only in Custom order.
            let draggable = state.bookmark_sort == BookmarkSort::Custom;
            // Each row drawn, by number, for placing a drop.
            let mut rows = Vec::new();
            // The row being dragged, or let go of this frame.
            let mut dragged = None;
            let mut dropped = None;

            // Rows are numbered by their place in the panel's order, which is
            // what `bm <n>` takes; `i` indexes `dir_bookmarks`.
            ui::list_columns(ui, &state.ui.bookmarks_filter, min_width, |ui, n| {
                let Some(&i) = sorted.get(n) else {
                    return;
                };
                let bm = &state.dir_bookmarks[i];

                let row = ui.horizontal(|ui| {
                    let mut resp = dir_button(ui, format!("{n}: "), bm, opts, false);
                    if draggable {
                        // A press that moves becomes a drag; one that doesn't
                        // is still a click.
                        resp = resp.interact(Sense::drag());
                        if resp.dragged_by(PointerButton::Primary) {
                            dragged = Some(n);
                        }
                        if resp.drag_stopped_by(PointerButton::Primary) {
                            dropped = Some(n);
                        }
                    }

                    if let Some(a) = dir_click(&resp, bm, DirList::Bookmarks) {
                        action = Some(a);
                    }

                    if ui.small_button("x").clicked() {
                        action = Some(ListAction::RemoveBookmark(i));
                    }
                });

                let column = ui.max_rect().x_range();
                rows.push((
                    n,
                    Rect::from_x_y_ranges(column, row.response.rect.y_range()),
                ));
            });

            let pointer = ui.input(|i| i.pointer.latest_pos());
            let gap = ui.spacing().item_spacing.y;
            if let (Some(from), Some(pos)) = (dragged.or(dropped), pointer)
                && let Some((slot, y, x_range)) = bookmark_drop_slot(&rows, pos, gap)
            {
                if dragged.is_some() {
                    ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
                    // Mark where the row would land, unless that's where it is.
                    if slot != from && slot != from + 1 {
                        let color = ui.visuals().selection.bg_fill;
                        let stroke = Stroke::new(BOOKMARK_DROP_MARKER_WIDTH, color);
                        ui.painter().hline(x_range, y, stroke);
                    }
                } else {
                    // Taking the row out first shifts the later places up.
                    let to = if slot > from { slot - 1 } else { slot };
                    if to != from {
                        action = Some(ListAction::MoveBookmark(from, to));
                    }
                }
            }
        });

    ui::apply_action(state, action);
}

/// Display recent directories we've executed a command from or acted on in
/// the Files panel (see [State::touch_recent_dir]), as a clickable list, in
/// columns when the panel is wide enough. Newest first. Dirs that are
/// also bookmarked have a green background (see [bookmarked_fill]). In
/// "Names" mode each name takes its parent dir's colour, so siblings group
/// visually. Clicking a row cd's there, and middle-clicking opens it in a new
/// tab. The ★ adds the row's dir to Bookmarks or removes it (see
/// [bookmark_star]), the `x` removes a row from the list, and "Cleanup
/// missing" removes every row whose dir is gone. The search box filters by
/// case-insensitive substring against the full path.
pub(super) fn dir_history(state: &mut State, ui: &mut Ui) {
    ui::panel_header(ui, "Recent dirs", &mut state.ui.panel_vis.recent_dirs);

    if dir_label_mode_control(ui, &mut state.recent_dir_names, "recent dir") {
        let _ = state.save();
    }

    if search::search_box(ui, &mut state.ui.recent_dirs_search, "recent_dirs_search") {
        state.refresh_recent_dirs_filter();
    }
    ui.separator();

    let home = state.home.clone();
    let opts = DirLabelOpts {
        home: home.as_deref(),
        max_len: state.recent_dir_label_len,
        names: state.recent_dir_names,
        parent_color: true,
    };
    let mut action: Option<ListAction> = None;

    ScrollArea::vertical()
        .id_salt("recent_dirs_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // Render newest-first so the most-recently-used dirs are at the top.
            let rows: Vec<usize> = state.ui.recent_dirs_filter.iter().rev().copied().collect();
            // `"<i>: "`, sized for the largest index.
            let prefix_chars = state.recent_dirs.len().to_string().len() + 2;
            let paths = state.recent_dirs.iter().map(|r| &r.path);
            let label_chars = dir_label_chars(ui, paths, opts);
            let min_width = dir_col_width(ui, prefix_chars, label_chars, &[STAR_OFF, 'x']);

            ui::list_columns(ui, &rows, min_width, |ui, i| {
                let Some(r) = state.recent_dirs.get(i) else {
                    return;
                };

                let bookmarked = state.dir_bookmarks.contains(&r.path);
                ui.horizontal(|ui| {
                    let resp = dir_button(ui, format!("{i}: "), &r.path, opts, bookmarked);
                    if let Some(a) = dir_click(&resp, &r.path, DirList::RecentDirs) {
                        action = Some(a);
                    }

                    if bookmark_star(ui, bookmarked, true).clicked() {
                        action = Some(ListAction::ToggleBookmark(r.path.clone()));
                    }

                    if ui
                        .small_button("x")
                        .on_hover_text("Remove from recent dirs")
                        .clicked()
                    {
                        action = Some(ListAction::RemoveRecentDir(r.path.clone()));
                    }
                });
            });
        });

    ui::apply_action(state, action);
}

/// Popup shown while `state.ui.stale_dir` is set: a Recent dirs / Bookmarks
/// entry no longer exists, so ask whether to remove it from its list. Esc or
/// a click outside the popup keeps it, as the Keep button does.
pub(super) fn stale_dir_popup(state: &mut State, ctx: &egui::Context) {
    let Some((path, list)) = state.ui.stale_dir.clone() else {
        return;
    };
    let home = state.home.clone();
    let shown = render_with_tilde(&path, home.as_deref());
    let gui_font_size = state.gui_font_size;

    // `Some(true)` to remove, `Some(false)` to keep.
    let mut remove = None;
    let modal = Modal::new(egui::Id::new("stale_dir_popup")).show(ctx, |ui| {
        ui::scale_panel_fonts(ui, gui_font_size);
        ui.heading("Directory not found");
        ui.add_space(4.);

        let mut msg = ui::path_job(ui, &path, home.as_deref());
        let body = TextFormat {
            font_id: TextStyle::Body.resolve(ui.style()),
            color: Color32::PLACEHOLDER,
            ..Default::default()
        };

        msg.append(
            &format!(" doesn't exist. Remove it from {}?", list.name()),
            0.,
            body,
        );

        ui.label(msg);
        ui.add_space(8.);

        ui.horizontal_wrapped(|ui| {
            let font_id = TextStyle::Button.resolve(ui.style());
            let mut label = LayoutJob::default();
            label.append(
                "🗙 ",
                0.,
                TextFormat {
                    font_id: font_id.clone(),
                    color: ui.visuals().error_fg_color,
                    ..Default::default()
                },
            );

            label.append(
                &format!("Remove {shown} from {}", list.name()),
                0.,
                TextFormat {
                    font_id,
                    color: Color32::PLACEHOLDER,
                    ..Default::default()
                },
            );

            let remove_btn = ui.button(label);
            let keep_btn = ui.button(format!("Keep this {}", list.entry_name()));

            // Hold keyboard focus inside the popup, starting on Keep: the
            // input row behind it would otherwise keep its focus and take
            // typing (and Enter) while the popup is up. Tab switches between
            // the two; Enter or Space presses the focused one.
            if !remove_btn.has_focus() && !keep_btn.has_focus() {
                keep_btn.request_focus();
            }
            if remove_btn.clicked() {
                remove = Some(true);
            }
            if keep_btn.clicked() {
                remove = Some(false);
            }
        });
    });

    if remove.is_none() && modal.should_close() {
        remove = Some(false);
    }

    match remove {
        Some(true) => state.remove_stale_dir(),
        Some(false) => state.ui.stale_dir = None,
        None => return,
    }
    state.ui.focus_input = true;
}
