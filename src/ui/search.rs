//! For the general filter-style searches in each pane, and the RipGrep file search.

use crate::deep_search::{DeepSearch, MAX_MATCHES, Row};
use crate::ui::dirs::BOOKMARKED_TEXT;
use crate::ui::file_browser::RowAction;
use crate::ui::{dirs, file_browser};
use crate::{path_color, render_with_tilde, ui};
use eframe::epaint::Color32;
use eframe::epaint::text::{LayoutJob, TextFormat};
use egui::{Button, RichText, ScrollArea, TextEdit, TextStyle, Ui, WidgetText};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The Files list while a Deep search is set: a status line, then each file
/// (or in file-name mode, folder) found, by its path relative to the
/// searched dir. In file-name mode the matches in its name are highlighted,
/// and folders are marked and coloured as in the listing, on pale green when
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
                                // Same colour this folder has in any path
                                // shown elsewhere.
                                job.append("📁 ", 0.0, base.clone());
                                let tilde = render_with_tilde(&hit.path, home);
                                if let Some(c) = path_color::last_color(&tilde, Some("~"), dark) {
                                    base.color = c;
                                }
                            }
                            ui::append_highlighted(&mut job, &path, &ranges, &base, &hl);
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
                            ui::append_highlighted(&mut job, &m.text, &m.ranges, &plain, &hl);
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
                    if let Some(a) = file_browser::row_action(ui, &resp, &hit.path, hit.is_folder, false) {
                        action = Some(a);
                    }
                }
            });
    });
    action
}

/// Renders a single-line "Search" box that fills the available width and
/// triggers `on_changed` when the buffer is edited. Pulled out so all three
/// sidebars look identical.
pub fn search_box(ui: &mut Ui, buf: &mut String, id_salt: &str) -> bool {
    ui.add(
        TextEdit::singleline(buf)
            .hint_text("Search")
            .desired_width(f32::INFINITY)
            .id_salt(id_salt),
    )
    .changed()
}