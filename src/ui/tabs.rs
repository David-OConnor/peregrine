//! For the tabs bar, and each tab in it.

use std::path::{Path, PathBuf};

use eframe::{
    emath::{Align, Rect},
    epaint::{
        Color32,
        text::{LayoutJob, TextWrapping},
    },
};
use egui::{Button, FontId, FontSelection, Frame, RichText, TextStyle, Ui, WidgetText};
use shell::DirList;

use crate::{
    render_with_tilde,
    state::State,
    ui,
    ui::{dirs, dirs::cleanup_missing_button, project_bar, tab_keys},
};

/// A dir that says little on its own, so a tab on it is always labelled with
/// the folder above it too, e.g. `molchanica/src`. See [tab_labels].
const VAGUE_DIR: &str = "src";

/// What a tab is labelled with: its cwd's final folder, `leaf`, after
/// `parents`, the folders above it that tell it apart (see [tab_labels]),
/// e.g. `molchanica/` for `~/code/molchanica/src`. A connected tab is named
/// for its remote, with no `parents`.
struct TabLabel {
    parents: String,
    leaf: String,
}

/// Each tab's [TabLabel]: for one connected to a remote, its `remotes`
/// label; otherwise its cwd's final folder. A [VAGUE_DIR] gets the folder
/// above it too, and tabs on different dirs that would read the same, e.g.
/// two projects' `venv`s, get as many of the folders above as tell them
/// apart. Folders above stop at `~` for dirs under `home`.
fn tab_labels(cwds: &[PathBuf], remotes: &[Option<String>], home: Option<&Path>) -> Vec<TabLabel> {
    let names: Vec<Vec<String>> = cwds.iter().map(|p| folder_names(p, home)).collect();
    // How many of each tab's `names` its label shows.
    let mut depth: Vec<usize> = names
        .iter()
        .map(|n| {
            if n.len() > 1 && n[0] == VAGUE_DIR {
                2
            } else {
                1
            }
        })
        .collect();

    let local: Vec<usize> = (0..cwds.len()).filter(|&i| remotes[i].is_none()).collect();
    let shown = |i: usize, depth: &[usize]| &names[i][..depth[i].min(names[i].len())];

    // One more folder at a time for each tab that still reads the same as
    // one on another dir, until none do or there are no folders left.
    loop {
        let deeper: Vec<usize> = local
            .iter()
            .copied()
            .filter(|&i| {
                depth[i] < names[i].len()
                    && local
                        .iter()
                        .any(|&j| cwds[j] != cwds[i] && shown(j, &depth) == shown(i, &depth))
            })
            .collect();
        if deeper.is_empty() {
            break;
        }
        for i in deeper {
            depth[i] += 1;
        }
    }

    (0..cwds.len())
        .map(|i| match &remotes[i] {
            Some(label) => TabLabel {
                parents: String::new(),
                leaf: label.clone(),
            },
            None => {
                let shown = shown(i, &depth);
                let parents: String = shown[1..]
                    .iter()
                    .rev()
                    .map(|name| format!("{name}/"))
                    .collect();
                TabLabel {
                    parents,
                    leaf: shown[0].clone(),
                }
            }
        })
        .collect()
}

/// `dir`'s own name, then those of the folders it's in, innermost first: up
/// to `~` when it's in `home`, else to its root (as `C:`; `/` has none). A
/// root's own name is all of it, as `C:\`.
fn folder_names(dir: &Path, home: Option<&Path>) -> Vec<String> {
    let own = dir.file_name().map_or_else(
        || render_with_tilde(dir, home),
        |n| n.to_string_lossy().into_owned(),
    );
    let mut names = vec![own];

    for above in dir.ancestors().skip(1) {
        if Some(above) == home {
            names.push("~".to_owned());
            break;
        }
        match above.file_name() {
            Some(n) => names.push(n.to_string_lossy().into_owned()),
            None => {
                let root = above.to_string_lossy();
                let root = root.trim_end_matches(['/', '\\']);
                if !root.is_empty() {
                    names.push(root.to_owned());
                }
            }
        }
    }
    names
}

/// The text of tab `n`'s `label`, cut to fit `width` in `font`: from the
/// front of its `parents`, after a `…`, so the folder itself still shows.
/// Only when even that doesn't fit is its end cut off (see [tab_button]).
fn fit_label(ui: &Ui, n: usize, label: &TabLabel, font: &FontId, width: f32) -> String {
    let text_width = |text: &str| {
        ui.ctx().fonts_mut(|f| {
            f.layout_no_wrap(text.to_owned(), font.clone(), Color32::PLACEHOLDER)
                .size()
                .x
        })
    };
    let full = format!("{n}: {}{}", label.parents, label.leaf);
    if label.parents.is_empty() || text_width(&full) <= width {
        return full;
    }

    let chars: Vec<char> = label.parents.chars().collect();
    (1..chars.len())
        .map(|skip| {
            let rest: String = chars[skip..].iter().collect();
            format!("{n}: …{rest}{}", label.leaf)
        })
        .find(|text| text_width(text) <= width)
        .unwrap_or_else(|| format!("{n}: {}", label.leaf))
}

/// One [TAB_WIDTH]-wide tab of the top strip, for tab `n`'s `label` (see
/// [fit_label]). The label is laid out by hand so it can be cut off at the
/// edge with no ellipsis (egui's own truncation always adds one); the full
/// label is in the hover text when cut. With `closable`, an `x` sits over
/// the tab's right end. Returns the tab's response and whether the `x` was
/// clicked.
fn tab_button(
    ui: &mut Ui,
    n: usize,
    label: &TabLabel,
    selected: bool,
    closable: bool,
) -> (egui::Response, bool) {
    let close_width = if closable {
        ui.spacing().interact_size.y
    } else {
        0.
    };

    let label_width = TAB_WIDTH - 2. * ui.spacing().button_padding.x - close_width;
    // The font a plain-text button label would get. PLACEHOLDER color lets
    // the button apply its own (selected/hovered).
    let font = FontSelection::Default.resolve_with_fallback(ui.style(), TextStyle::Button.into());
    let full = format!("{n}: {}{}", label.parents, label.leaf);
    let text = fit_label(ui, n, label, &font, label_width.max(0.));
    let mut job = LayoutJob::simple_singleline(text.clone(), font, Color32::PLACEHOLDER);

    job.wrap = TextWrapping {
        overflow_character: None,
        ..TextWrapping::truncate_at_width(label_width.max(0.))
    };

    let galley = ui.ctx().fonts_mut(|f| f.layout_job(job));
    let cut = galley.elided || text != full;

    let button =
        Button::selectable(selected, WidgetText::from(galley)).min_size(egui::vec2(TAB_WIDTH, 0.));

    // Left-align the label; horizontal layouts centre button text. A selected
    // button takes its fill and text color from the selection visuals, so
    // swap in the tab colors for just this button.
    let layout = ui.layout().with_main_align(Align::Min);
    let mut tab = ui
        .with_layout(layout, |ui| {
            let selection = &mut ui.visuals_mut().selection;
            selection.bg_fill = TAB_ACTIVE_FILL;
            selection.stroke.color = TAB_ACTIVE_TEXT;
            ui.add(button)
        })
        .inner;
    if cut {
        tab = tab.on_hover_text(full);
    }

    let close_clicked = closable && {
        let rect = tab.rect;
        let close_rect =
            Rect::from_min_max(egui::pos2(rect.right() - close_width, rect.top()), rect.max);

        // Only the active tab is closable, so match its label color.
        let x = RichText::new("x").color(TAB_ACTIVE_TEXT);
        let shortcut = format!(
            "{} or middle click",
            ui.ctx().format_shortcut(&tab_keys::CLOSE_TAB)
        );
        ui.put(close_rect, Button::new(x).frame_when_inactive(false))
            .on_hover_text(format!("Close tab ({shortcut})"))
            .clicked()
    };

    (tab, close_clicked)
}

/// Top-of-window tab strip. Each tab labels itself with its cwd's leaf
/// folder, and the folders above it where that alone could be mistaken (see
/// [tab_labels]); clicking selects, dragging moves it along the strip, the active
/// tab's `x` (or middle-clicking any tab) closes it (unavailable when only
/// one tab is left), and `+` appends a fresh tab inheriting the active tab's
/// cwd. The keyboard shortcuts are in [tab_keys]. The project bar (see
/// [project_bar]) sits at the right end of the row, or on a row of its own
/// below when it doesn't fit.
pub fn tabs_bar(state: &mut State, ui: &mut Ui) {
    const TAB_SECTION_PADDING: f32 = 20.;

    // Snapshot the data we need so the inner closure doesn't fight the
    // borrow checker with the mutable `state` we mutate at the end.
    let active = state.ui.active_tab;
    let n = state.cwd.len();
    let home = state.home.clone();

    // A connected tab is named for its remote.
    let remotes: Vec<Option<String>> = state
        .remote
        .iter()
        .map(|r| r.as_ref().map(|r| r.label()))
        .collect();
    let labels = tab_labels(&state.cwd, &remotes, home.as_deref());

    // Saved remotes, for the one-click connect buttons to the right of `+`.
    let remotes: Vec<(usize, String)> = state
        .remote_terminals
        .iter()
        .enumerate()
        .map(|(i, r)| (i, r.host.clone()))
        .collect();

    let font_size = state.font_size;
    let gui_font_size = state.gui_font_size;

    let mut to_select: Option<usize> = None;
    let mut to_close: Option<usize> = None;
    // A tab dropped after dragging, and the index it lands at.
    let mut to_move: Option<(usize, usize)> = None;
    let mut add_new = false;
    let mut connect_remote: Option<usize> = None;
    let mut font_delta = 0.;
    let mut gui_font_delta = 0.;
    let mut set_dark = None;
    let mut project_bar_fits = true;

    ui.horizontal(|ui| {
        if ui.button("⚙").on_hover_text("Settings").clicked() {
            state.ui.settings = !state.ui.settings;
        }

        // Then the theme toggle, showing the theme a click switches to.
        let dark = ui.visuals().dark_mode;
        let (icon, other) = if dark {
            ("☀", "light")
        } else {
            ("🌙", "dark")
        };
        if ui
            .button(icon)
            .on_hover_text(format!("Switch to {other} mode"))
            .clicked()
        {
            set_dark = Some(!dark);
        }
        if ui
            .button("About")
            .on_hover_text("Show Peregrine's version, source code, and state file location")
            .clicked()
        {
            state.ui.about = !state.ui.about;
        }

        if cleanup_missing_button(ui) {
            state.cleanup_missing_dirs(DirList::Bookmarks);
            state.cleanup_missing_dirs(DirList::RecentDirs);
        }

        ui.separator();
        // Font-size controls sit to the left of the tab strip: one for the
        // terminal panes, one for the side panels.

        ui.label("CLI font:");
        font_delta = ui::font_size_control(ui, font_size);
        ui.separator();

        ui.label("GUI font:");
        gui_font_delta = ui::font_size_control(ui, gui_font_size);

        ui.separator();
        // Breathing room either side of the tab section.
        ui.add_space(TAB_SECTION_PADDING);

        // The tabs and `+ tab` sit on a background of their own, setting them
        // apart from the controls either side and the panel toggles below.
        let fill = if ui.visuals().dark_mode {
            TAB_BAR_FILL_DARK
        } else {
            TAB_BAR_FILL_LIGHT
        };
        Frame::new()
            .fill(fill)
            .inner_margin(TAB_SECTION_MARGIN)
            .corner_radius(TAB_SECTION_ROUNDING)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    // Tighter than the default spacing, so tabs and their separators sit
                    // close together. Scoped so the rest of the strip keeps the default.
                    ui.scope(|ui| {
                        ui.spacing_mut().item_spacing.x = TAB_GAP;

                        let mut tab_rects = Vec::with_capacity(n);
                        let mut dragged = None;
                        let mut dropped = None;

                        for (i, label) in labels.iter().enumerate() {
                            // Only the active tab shows its `x`, Firefox-style.
                            let is_active = i == active;
                            // Numbered from 1, to match Ctrl/Cmd + 1-8.
                            let (tab, close_clicked) =
                                tab_button(ui, i + 1, label, is_active, n > 1 && is_active);
                            // A press that moves becomes a drag; one that doesn't is
                            // still a click.
                            let tab = tab.interact(egui::Sense::drag());
                            if tab.clicked() {
                                to_select = Some(i);
                            }

                            // Middle-click closes, same as the `x`.
                            if close_clicked || (n > 1 && tab.middle_clicked()) {
                                to_close = Some(i);
                            }

                            if tab.dragged_by(egui::PointerButton::Primary) {
                                dragged = Some(i);
                            }
                            if tab.drag_stopped_by(egui::PointerButton::Primary) {
                                dropped = Some(i);
                            }
                            tab_rects.push(tab.rect);

                            ui.add(egui::Separator::default().spacing(TAB_GAP));
                        }

                        let pointer = ui.input(|i| i.pointer.latest_pos());
                        if let (Some(from), Some(pos)) = (dragged.or(dropped), pointer) {
                            // The gap the pointer is nearest: 0 is before the first tab,
                            // `n` after the last.
                            let slot = tab_rects.iter().filter(|r| r.center().x < pos.x).count();

                            if dragged.is_some() {
                                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                                // Mark where the tab would land, unless that's where it is.
                                if slot != from && slot != from + 1 {
                                    let x = match slot {
                                        0 => tab_rects[0].left() - 1.5 * TAB_GAP,
                                        s if s == n => tab_rects[n - 1].right() + 1.5 * TAB_GAP,
                                        s => (tab_rects[s - 1].right() + tab_rects[s].left()) / 2.,
                                    };
                                    let stroke =
                                        egui::Stroke::new(TAB_DROP_MARKER_WIDTH, TAB_ACTIVE_FILL);
                                    ui.painter().vline(x, tab_rects[0].y_range(), stroke);
                                }
                            } else {
                                // Taking the tab out first shifts the later slots left.
                                let to = if slot > from { slot - 1 } else { slot };
                                to_move = Some((from, to));
                            }
                        }
                    });

                    let shortcut = ui.ctx().format_shortcut(&tab_keys::NEW_TAB);
                    let plus_width = 1.5 * dirs::button_width(ui, "+");

                    if ui
                        .add(Button::new("+").min_size(egui::vec2(plus_width, 0.)))
                        .on_hover_text(format!("New tab ({shortcut})"))
                        .clicked()
                    {
                        add_new = true;
                    }
                });
            });

        ui.add_space(TAB_SECTION_PADDING);
        // One-click SSH connect per saved remote, e.g. `ssh 0 192.168.1.10`.
        // Connects the active tab — same as typing `ssh <index>`.
        for (i, host) in &remotes {
            ui.separator();
            if ui
                .button(format!("+ ssh {i} {host}"))
                .on_hover_text("Connect this tab to the saved remote")
                .clicked()
            {
                connect_remote = Some(*i);
            }
        }

        project_bar_fits =
            ui.available_width() >= project_bar::last_width(ui) + TAB_SECTION_PADDING;
        if project_bar_fits {
            project_bar::project_bar(state, ui);
        }
    });

    if !project_bar_fits {
        ui.horizontal(|ui| project_bar::project_bar(state, ui));
    }

    if let Some(dark) = set_dark {
        state.set_dark_mode(ui.ctx(), dark);
    }

    if font_delta != 0.0 {
        state.adjust_font_size(font_delta);
    }

    if gui_font_delta != 0.0 {
        state.adjust_gui_font_size(gui_font_delta);
    }

    if let Some(i) = to_select {
        state.select_tab(i);
    }

    if let Some(i) = to_close {
        state.close_tab(i);
    }

    // A dragged tab becomes the active one, as in a browser.
    if let Some((from, to)) = to_move {
        state.move_tab(from, to);
        state.select_tab(to);
    }

    if add_new {
        state.add_tab();
    }

    if let Some(i) = connect_remote {
        state.connect_remote(i);
    }
}

/// Width of every tab in the top strip, its `x` included. Longer labels are
/// cut off at the edge (no ellipsis). See [tab_button].
const TAB_WIDTH: f32 = 120.;
/// Gap either side of the separator between tabs in the top strip, and the
/// separator's own width. See [tabs_bar].
const TAB_GAP: f32 = 2.;
/// The active tab's fill and label color: purple, so it can't be mistaken
/// for the blue "on" state of the panel toggles below. See [tab_button].
pub(super) const TAB_ACTIVE_FILL: Color32 = Color32::from_rgb(128, 64, 208);
const TAB_ACTIVE_TEXT: Color32 = Color32::WHITE;
/// Background of the tab section of the top strip (the tabs and `+ tab`),
/// setting it apart from the controls beside it and the panel-toggle row
/// beneath. A deep violet or a light lavender, per theme. See [tabs_bar].
const TAB_BAR_FILL_DARK: Color32 = Color32::from_rgb(36, 28, 66);
const TAB_BAR_FILL_LIGHT: Color32 = Color32::from_rgb(226, 220, 248);
/// Padding between that background's edge and the tabs, and its rounding.
const TAB_SECTION_MARGIN: egui::Margin = egui::Margin::symmetric(6, 3);
const TAB_SECTION_ROUNDING: u8 = 4;
/// Width of the line marking where a dragged tab will land. See [tabs_bar].
pub(super) const TAB_DROP_MARKER_WIDTH: f32 = 2.;

#[cfg(test)]
mod tests {
    use super::*;

    /// Each local tab's label for `dirs`, as `parents` + `leaf`.
    fn labels(dirs: &[&str], home: Option<&str>) -> Vec<String> {
        let cwds: Vec<PathBuf> = dirs.iter().map(PathBuf::from).collect();
        let remotes = vec![None; cwds.len()];
        tab_labels(&cwds, &remotes, home.map(Path::new))
            .into_iter()
            .map(|l| format!("{}{}", l.parents, l.leaf))
            .collect()
    }

    #[test]
    fn src_gets_its_parent() {
        assert_eq!(
            labels(&["/code/molchanica/src", "/code/peregrine"], None),
            ["molchanica/src", "peregrine"]
        );
    }

    #[test]
    fn same_names_in_different_dirs_get_parents_until_they_differ() {
        assert_eq!(
            labels(&["/code/a/venv", "/code/b/venv", "/code/c"], None),
            ["a/venv", "b/venv", "c"]
        );
        assert_eq!(
            labels(&["/x/app/venv", "/y/app/venv"], None),
            ["x/app/venv", "y/app/venv"]
        );
        // The same dir twice is no clash.
        assert_eq!(labels(&["/code/a", "/code/a"], None), ["a", "a"]);
        // Two `src`s of projects of the same name.
        assert_eq!(
            labels(&["/x/app/src", "/y/app/src"], None),
            ["x/app/src", "y/app/src"]
        );
    }

    #[test]
    fn folders_above_stop_at_home() {
        assert_eq!(
            labels(&["/home/me/src", "/home/me/code/x"], Some("/home/me")),
            ["~/src", "x"]
        );
        assert_eq!(
            labels(&["/home/me/a", "/opt/a"], Some("/home/me")),
            ["~/a", "opt/a"]
        );
    }

    #[test]
    fn remote_tabs_keep_their_label() {
        let cwds = [PathBuf::from("/code/a"), PathBuf::from("/other/a")];
        let remotes = [Some("me@host".to_owned()), None];
        let labels = tab_labels(&cwds, &remotes, None);
        assert_eq!(
            (labels[0].parents.as_str(), labels[0].leaf.as_str()),
            ("", "me@host")
        );
        assert_eq!(
            (labels[1].parents.as_str(), labels[1].leaf.as_str()),
            ("", "a")
        );
    }
}
