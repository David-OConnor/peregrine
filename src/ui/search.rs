//! For the general filter-style searches in each pane, and the RipGrep file search.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use eframe::{
    emath::Align,
    epaint::{
        Color32,
        text::{LayoutJob, TextFormat},
    },
};
use egui::{
    Button, Id, Key, Layout, Response, RichText, ScrollArea, TextEdit, TextStyle, Ui, WidgetText,
};

use crate::{
    deep_search,
    deep_search::{DeepSearch, MAX_MATCHES, Row},
    path_color, render_with_tilde,
    state::State,
    ui,
    ui::{dirs, dirs::BOOKMARKED_TEXT, file_browser, file_browser::RowAction},
};

pub const QUICK_SEARCH_WIDTH: f32 = 140.;

/// The narrowest an [inline_search_box] gets beside the widgets before it on
/// its row; any narrower and it goes on a row of its own.
const INLINE_SEARCH_MIN_WIDTH: f32 = 60.;

/// The narrowest the Deep search box gets with its toggles beside it; any
/// narrower and they go on a row of their own under it. No narrower than the
/// Search box.
const DEEP_SEARCH_MIN_WIDTH: f32 = QUICK_SEARCH_WIDTH;

/// How long Deep search waits after the last keystroke before searching, so
/// it doesn't walk the whole tree once per letter typed.
const DEEP_SEARCH_DELAY: f64 = 0.3;

/// The Files list while a Deep search is set: a status line, then each file
/// (or in file-name mode, folder) found, by its path relative to the
/// searched dir. In file-name mode the matches in its name are highlighted,
/// and folders are marked and colored as in the listing, on pale green when
/// they're among `bookmarks` (see [dirs::bookmarked_fill]); otherwise it
/// shows how many lines matched, and those lines follow it, numbered, with
/// the matches highlighted. `name_filter` (the panel's Search box) narrows
/// the results by that path. Clicking a row selects its file or folder (with
/// Ctrl and Shift, several of them); double-clicking opens the file or goes
/// into the folder, and middle-clicking a folder opens it in a new tab;
/// rows drag onto folders: all as in the listing (see
/// [file_browser::row_action]). The rows in `selected` are highlighted.
pub fn deep_search_results(
    search: &DeepSearch,
    name_filter: &str,
    selected: &HashSet<PathBuf>,
    bookmarks: &[PathBuf],
    home: Option<&Path>,
    ui: &mut Ui,
) -> Option<RowAction> {
    if let Some(e) = &search.error {
        ui.label(RichText::new(format!("Invalid search: {e}")).color(ui.visuals().error_fg_color));
        return None;
    }
    let rel = |p: &Path| {
        p.strip_prefix(&search.root)
            .unwrap_or(p)
            .display()
            .to_string()
    };

    let rows: Vec<Row> = if name_filter.is_empty() {
        search.rows.clone()
    } else {
        let needle = name_filter.to_lowercase();
        let keep: Vec<bool> = search
            .hits
            .iter()
            .map(|h| rel(&h.path).to_lowercase().contains(&needle))
            .collect();
        search
            .rows
            .iter()
            .copied()
            .filter(|&(Row::File(i) | Row::Line(i, _))| keep[i])
            .collect()
    };

    let n_folders = search.hits.iter().filter(|h| h.is_folder).count();
    let files = ui::plural(search.hits.len() - n_folders, "file", "files");

    let (counts, unit) = if search.opts.file_names {
        let folders = ui::plural(n_folders, "folder", "folders");
        let counts = match (search.hits.len() - n_folders, n_folders) {
            (_, 0) => files,
            (0, _) => folders,
            _ => format!("{files}, {folders}"),
        };
        (counts, "results")
    } else {
        let matches = ui::plural(search.n_matches, "match", "matches");
        (format!("{matches} in {files}"), "matches")
    };

    ui.horizontal_wrapped(|ui| {
        let status = if !search.done() {
            ui.spinner();
            format!("Searching... {counts}")
        } else if search.hits.is_empty() {
            "No matches.".to_owned()
        } else if search.n_matches >= MAX_MATCHES {
            format!("{counts}. Stopped at {MAX_MATCHES} {unit}.")
        } else {
            counts
        };
        ui.label(RichText::new(status).italics().weak());
    });

    let incomplete = search.incomplete_files();
    if incomplete != 0 {
        ui.label(
            RichText::new(format!(
                "{} couldn't be fully searched: unreadable, or a line exceeded the 8 MiB limit.",
                ui::plural(incomplete, "file", "files")
            ))
            .color(ui.visuals().warn_fg_color),
        );
    }
    if rows.is_empty() && !search.hits.is_empty() {
        ui.label(
            RichText::new("None of them match the Search box.")
                .italics()
                .weak(),
        );
    }

    // Every row is one line of monospace text, so only the visible ones need
    // laying out, however many matches there are.
    let row_height = ui
        .spacing()
        .interact_size
        .y
        .max(ui.text_style_height(&TextStyle::Monospace) + 2.0 * ui.spacing().button_padding.y);

    let visuals = ui.visuals();

    let plain = TextFormat {
        font_id: TextStyle::Monospace.resolve(ui.style()),
        color: Color32::PLACEHOLDER,
        ..Default::default()
    };

    let weak = TextFormat {
        color: visuals.weak_text_color(),
        ..plain.clone()
    };

    let hl = TextFormat {
        color: visuals.selection.stroke.color,
        background: visuals.selection.bg_fill,
        ..plain.clone()
    };

    let dark = visuals.dark_mode;
    let mut action = None;

    ui.scope(|ui| {
        // Rows butt up against each other, like the listing's.
        ui.spacing_mut().item_spacing.y = 0.0;
        ScrollArea::vertical()
            .id_salt("deep_search_scroll")
            .auto_shrink([false, false])
            .show_rows(ui, row_height, rows.len(), |ui, range| {
                // Under the rows, so it only gets clicks that miss them.
                if let Some(a) = file_browser::background_action(ui) {
                    action = Some(a);
                }
                file_browser::drag_autoscroll(ui);

                for &row in &rows[range] {
                    let (Row::File(i) | Row::Line(i, _)) = row;
                    let hit = &search.hits[i];
                    let is_selected = matches!(row, Row::File(_)) && selected.contains(&hit.path);
                    // As in the listing: green unless selected.
                    let bookmarked = matches!(row, Row::File(_))
                        && hit.is_folder
                        && !is_selected
                        && bookmarks.contains(&hit.path);

                    let text: WidgetText = match row {
                        Row::File(_) if search.opts.file_names => {
                            // The name ends the path; its matches are
                            // highlighted there.
                            let path = rel(&hit.path);
                            let name = hit.path.file_name().map(|n| n.to_string_lossy());
                            let ranges: Vec<_> = name
                                .filter(|n| path.ends_with(n.as_ref()))
                                .map(|n| path.len() - n.len())
                                .map_or_else(Vec::new, |at| {
                                    let shift =
                                        |r: &std::ops::Range<usize>| r.start + at..r.end + at;
                                    hit.name_ranges.iter().map(shift).collect()
                                });
                            let mut job = LayoutJob::default();
                            let mut base = plain.clone();

                            if bookmarked && !dark {
                                base.color = BOOKMARKED_TEXT;
                            }

                            if hit.is_folder {
                                // Same color this folder has in any path
                                // shown elsewhere.
                                job.append("📁 ", 0.0, base.clone());
                                let tilde = render_with_tilde(&hit.path, home);
                                if let Some(c) = path_color::last_color(&tilde, Some("~"), dark) {
                                    base.color = c;
                                }
                            }
                            append_highlighted(&mut job, &path, &ranges, &base, &hl);
                            job.into()
                        }
                        Row::File(_) => {
                            let text = format!("{} ({})", rel(&hit.path), hit.lines.len());
                            RichText::new(text).monospace().strong().into()
                        }
                        Row::Line(_, j) => {
                            let m = &hit.lines[j];
                            // Line numbers right-aligned within the file.
                            let width =
                                hit.lines.last().map_or(1, |l| l.line_num.to_string().len());
                            let mut job = LayoutJob::default();
                            job.append(&format!("  {:>width$}: ", m.line_num), 0.0, weak.clone());
                            append_highlighted(&mut job, &m.text, &m.ranges, &plain, &hl);
                            job.into()
                        }
                    };

                    let mut button = Button::selectable(is_selected, text)
                        .truncate()
                        .sense(egui::Sense::click_and_drag())
                        .min_size(egui::vec2(ui.available_width(), row_height));

                    if bookmarked {
                        button = button
                            .fill(dirs::bookmarked_fill(dark))
                            .frame_when_inactive(true);
                    }
                    let resp = ui.add(button);
                    // A matching line's row stands for its file. These can't
                    // be renamed in place, so none is the only one selected.
                    if let Some(a) =
                        file_browser::row_action(ui, &resp, &hit.path, hit.is_folder, false)
                    {
                        action = Some(a);
                    }
                }
            });
    });

    action
}

/// Renders a single-line "Search" box that triggers `on_changed` when the buffer is edited. This is used
/// in content-filter search on a number of panels.
pub fn search_box(ui: &mut Ui, buf: &mut String, id_salt: &str) -> bool {
    let id = ui.make_persistent_id(id_salt);
    search_box_with_id(ui, buf, id)
}

/// A [search_box] that follows other widgets on a `horizontal_wrapped` row,
/// taking the rest of it, up to [QUICK_SEARCH_WIDTH]. When that would leave
/// it narrower than [INLINE_SEARCH_MIN_WIDTH], it wraps onto a row of its own
/// instead.
pub fn inline_search_box(ui: &mut Ui, buf: &mut String, id_salt: &str) -> bool {
    let id = ui.make_persistent_id(id_salt);
    let rest = ui.available_size_before_wrap().x;
    let width = if rest >= INLINE_SEARCH_MIN_WIDTH {
        rest.min(QUICK_SEARCH_WIDTH)
    } else {
        QUICK_SEARCH_WIDTH
    };
    ui.add(
        TextEdit::singleline(buf)
            .hint_text("Search")
            .desired_width(width)
            .id(id),
    )
    .changed()
}

/// A [search_box] with the given `id`, for one whose focus is checked from
/// outside the `Ui` it's added to.
pub fn search_box_with_id(ui: &mut Ui, buf: &mut String, id: Id) -> bool {
    ui.add(
        TextEdit::singleline(buf)
            .hint_text("Search")
            .desired_width(QUICK_SEARCH_WIDTH)
            .id(id),
    )
    .changed()
}

/// The Files panel's Deep search box, followed on its row by the switch
/// between searching file names and contents and the regex toggle (see
/// `crate::deep_search`), or if that would squeeze the box below
/// [DEEP_SEARCH_MIN_WIDTH], with those on a row of their own under it. A
/// search starts once typing pauses for [DEEP_SEARCH_DELAY], or at once on
/// Enter (which also re-runs an unchanged one) or a toggle; clearing the box
/// ends it. Also takes in the running search's latest results.
pub fn deep_search_bar(state: &mut State, ui: &mut Ui) {
    let now = ui.input(|i| i.time);
    let opts = &mut state.ui.deep_search_opts;
    let buf = &mut state.ui.deep_search;

    // Give or take a point of rounding, so the Files panel's row, which
    // leaves exactly this much for it, can't tip it into two.
    let beside = ui.available_width() + 1.0 >= deep_search_min_width(ui);

    // Whether to re-run the search.
    let (mut run_now, resp) = if beside {
        right_to_left_row(ui, |ui| {
            let toggled = deep_search_toggles(ui, opts);
            (toggled, deep_search_box(ui, buf, opts.file_names))
        })
    } else {
        let resp = deep_search_box(ui, buf, opts.file_names);
        (
            right_to_left_row(ui, |ui| deep_search_toggles(ui, opts)),
            resp,
        )
    };

    if resp.changed() {
        if state.ui.deep_search.is_empty() {
            run_now = true;
        } else {
            state.ui.deep_search_edited = Some(now);
        }
    }

    if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
        run_now = true;
    }

    if run_now {
        state.start_deep_search(ui.ctx());
    } else if let Some(t) = state.ui.deep_search_edited {
        let wait = t + DEEP_SEARCH_DELAY - now;
        if wait > 0.0 {
            ui.ctx().request_repaint_after_secs(wait as f32);
        } else {
            state.start_deep_search(ui.ctx());
        }
    }
    if let Some(s) = &mut state.deep_search {
        s.poll();
    }
}

/// The Deep search box's, for the Files panel to tell it has the keyboard.
pub fn deep_search_id() -> Id {
    Id::new("files_deep_search")
}

/// How wide [deep_search_bar] needs to be to keep its toggles on the box's
/// row: the box's minimum, plus the toggles as last drawn.
pub fn deep_search_min_width(ui: &Ui) -> f32 {
    let toggles = ui
        .ctx()
        .data(|d| d.get_temp::<f32>(toggles_width_id()))
        .unwrap_or(0.0);
    DEEP_SEARCH_MIN_WIDTH + ui.spacing().item_spacing.x + toggles
}

/// Where [deep_search_toggles] keeps its width for [deep_search_min_width].
fn toggles_width_id() -> Id {
    deep_search_id().with("toggles_width")
}

/// The Deep search box itself, filling what's left of its row.
fn deep_search_box(ui: &mut Ui, buf: &mut String, file_names: bool) -> Response {
    let what = if file_names {
        "names of every file and folder"
    } else {
        "contents of every file"
    };

    ui.add(
        TextEdit::singleline(buf)
            .hint_text("Deep search")
            .desired_width(f32::INFINITY)
            .id(deep_search_id()),
    )
    .on_hover_text(format!(
        "Search the {what} in this folder and its subfolders, \
         as ripgrep does: hidden and .gitignored files are skipped. \
         Case-insensitive unless the search has a capital letter."
    ))
}

/// The Deep search toggles, added right to left: the regex one, then
/// Contents and Filenames. Returns whether the search should re-run.
/// Remembers how wide they were for [deep_search_min_width], and redoes the
/// frame if that changed (on the first, or with a new font size), as the
/// bar's layout depends on it.
fn deep_search_toggles(ui: &mut Ui, opts: &mut deep_search::Options) -> bool {
    let regex = ui
        .add(Button::selectable(opts.regex, ".*"))
        .on_hover_text("Treat the Deep search as a regular expression, not plain text");
    if regex.clicked() {
        opts.regex = !opts.regex;
    }

    // Right to left, so Contents comes before Filenames.
    let before = opts.file_names;
    let contents = ui
        .selectable_value(&mut opts.file_names, false, "Contents")
        .on_hover_text("Match lines inside files, like `rg <search>`");
    let names = ui
        .selectable_value(&mut opts.file_names, true, "Filenames")
        .on_hover_text("Match file and folder names, like `rg --files` filtered by the search");
    let mode_changed = opts.file_names != before;

    let width = regex.rect.width()
        + contents.rect.width()
        + names.rect.width()
        + 2.0 * ui.spacing().item_spacing.x;
    let id = toggles_width_id();
    let old = ui.ctx().data(|d| d.get_temp::<f32>(id));
    if old.is_none_or(|w| (w - width).abs() > 1.0) {
        ui.ctx().data_mut(|d| d.insert_temp(id, width));
        ui.ctx().request_discard("Deep search toggles resized");
    }

    regex.clicked() || mode_changed
}

/// A row as wide as `ui`, filled from the right.
fn right_to_left_row<R>(ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> R {
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), add_contents)
            .inner
    })
    .inner
}

/// Append `text` to `job` in the `plain` format, except for its byte
/// `ranges` (in order, not overlapping), which get `hl`.
fn append_highlighted(
    job: &mut LayoutJob,
    text: &str,
    ranges: &[std::ops::Range<usize>],
    plain: &TextFormat,
    hl: &TextFormat,
) {
    let mut at = 0;
    for r in ranges {
        job.append(&text[at..r.start], 0.0, plain.clone());
        job.append(&text[r.clone()], 0.0, hl.clone());
        at = r.end;
    }
    job.append(&text[at..], 0.0, plain.clone());
}
