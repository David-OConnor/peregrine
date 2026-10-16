use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use egui::{text::{CCursor, CCursorRange, LayoutJob, TextWrapping}, Align, Button, CentralPanel, Color32, DragValue, FontData, FontDefinitions, FontFamily, FontId, FontSelection, Frame, Key, Layout, Panel, Rect, RichText, ScrollArea, TextEdit, TextFormat, TextStyle, TextWrapMode, Ui, WidgetText};
use shell::{DirList, SshMode};

mod about;
pub mod clipboard;
pub mod dirs;
mod file_browser;
pub mod path_panel;
mod tab_keys;
mod terminal;

use crate::{
    OutType,
    cmd_color::CmdColors,
    deep_search::{DeepSearch, MAX_MATCHES, Row},
    files::{FileDetails, SelectionSummary, format_size, open_with_default_app},
    path_color::{self, Part},
    render_with_tilde, run_command,
    state::{FileRename, State},
    submit_command, viewer,
};
use crate::ui::dirs::{cleanup_missing_button, MIN_DIR_LABEL_LEN, MAX_DIR_LABEL_LEN};

/// Renders a single-line "Search" box that fills the available width and
/// triggers `on_changed` when the buffer is edited. Pulled out so all three
/// sidebars look identical.
fn search_box(ui: &mut Ui, buf: &mut String, id_salt: &str) -> bool {
    ui.add(
        TextEdit::singleline(buf)
            .hint_text("Search")
            .desired_width(f32::INFINITY)
            .id_salt(id_salt),
    )
    .changed()
}

/// A side panel's title row: the heading on the left and a small `x` on the
/// right that hides the panel — the same as unticking it in the "Panels:" row
/// (see [panel_toggles]). `visible` is that panel's [shell::PanelVis] flag.
fn panel_header(ui: &mut Ui, title: &str, visible: &mut bool) {
    ui.horizontal(|ui| {
        ui.heading(title);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.small_button("x").on_hover_text("Close panel").clicked() {
                *visible = false;
                ui.ctx().request_repaint();
            }
        });
    });
}



pub const WINDOW_WIDTH: f32 = 1400.;
pub const WINDOW_HEIGHT: f32 = 900.;

/// Default font size (points) for the terminal input/output panes, used when
/// the state file has no saved `FONT_SIZE`. Matches egui's default monospace
/// size so a fresh install looks unchanged.
pub const DEFAULT_FONT_SIZE: f32 = 13.;
/// Default body-text size (points) for the side panels, used when the state
/// file has no saved GUI font size. A little smaller than the terminal panes,
/// as the panels were before their size was adjustable.
pub const DEFAULT_GUI_FONT_SIZE: f32 = 11.;
/// Bounds the +/- font-size buttons (CLI and GUI) clamp to, so text can't be
/// shrunk to an unreadable size or blown up past the window.
pub const MIN_FONT_SIZE: f32 = 8.;
pub const MAX_FONT_SIZE: f32 = 28.;

/// Name of the bold font family registered by [install_fonts]; see
/// [bold_family].
const BOLD_FAMILY: &str = "bold";

/// Bold system fonts to try, in order. egui's built-in fonts have no bold
/// weight, so [install_fonts] borrows the first of these that exists. Ubuntu
/// Bold comes first on Linux as it matches egui's own Ubuntu Light.
fn bold_font_candidates() -> Vec<PathBuf> {
    if cfg!(windows) {
        let windir = std::env::var_os("WINDIR").unwrap_or_else(|| r"C:\Windows".into());
        let fonts = PathBuf::from(windir).join("Fonts");
        vec![fonts.join("segoeuib.ttf"), fonts.join("arialbd.ttf")]
    } else if cfg!(target_os = "macos") {
        vec![
            "/System/Library/Fonts/Supplemental/Arial Bold.ttf".into(),
            "/Library/Fonts/Arial Bold.ttf".into(),
        ]
    } else {
        [
            "/usr/share/fonts/truetype/ubuntu/Ubuntu-B.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSans-Bold.ttf",
        ]
        .into_iter()
        .map(PathBuf::from)
        .collect()
    }
}

/// Register the [bold_family] font family: the first bold system font found
/// (see [bold_font_candidates]), falling back to the normal proportional
/// fonts for glyphs it lacks. If none is found the family is just the
/// proportional fonts, so text drawn in it is still readable, only not bold.
pub fn install_fonts(ctx: &egui::Context) {
    let mut defs = FontDefinitions::default();
    let mut family = defs
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    if let Some(bytes) = bold_font_candidates()
        .into_iter()
        .find_map(|p| std::fs::read(p).ok())
    {
        defs.font_data.insert(
            BOLD_FAMILY.to_owned(),
            Arc::new(FontData::from_owned(bytes)),
        );
        family.insert(0, BOLD_FAMILY.to_owned());
    }
    defs.families
        .insert(FontFamily::Name(BOLD_FAMILY.into()), family);
    ctx.set_fonts(defs);
}

/// The bold font family registered by [install_fonts].
fn bold_family() -> FontFamily {
    FontFamily::Name(BOLD_FAMILY.into())
}

const RIGHT_COL_WIDTH: f32 = 180.;
const CWD_HIST_COL_WIDTH: f32 = 220.;
const BROWSER_COL_WIDTH: f32 = 200.;
/// The Files list splits into as many columns as fit at least this many
/// monospace characters each (so the threshold follows the GUI font size).
/// See [file_browser] and [list_columns].
const FILES_COL_MIN_CHARS: f32 = 26.;
/// Likewise for the Cmd history and Cmds this dir lists, in button-text
/// characters. Long commands wrap within their column.
const CMD_COL_MIN_CHARS: f32 = 30.;
const REMOTE_COL_WIDTH: f32 = 180.;

/// The main terminal output/input column must always keep at least this many
/// points of width, no matter how wide the user drags the side panels (or how
/// narrow they make the window). See [central_reserve].
pub const MIN_CENTRAL_WIDTH: f32 = 400.;

/// Points shaved off every text style in the top tab strip, so it reads
/// slightly smaller than the main terminal column. See [shrink_panel_fonts].
/// (The side panels follow the "GUI font size" instead; see
/// [scale_panel_fonts].)
const PANEL_FONT_REDUCTION: f32 = 2.;

/// Width of every tab in the top strip, its `x` included. Longer labels are
/// cut off at the edge (no ellipsis). See [tab_button].
const TAB_WIDTH: f32 = 100.;

/// Gap either side of the separator between tabs in the top strip, and the
/// separator's own width. See [tabs_bar].
const TAB_GAP: f32 = 2.;

/// The active tab's fill and label colour: purple, so it can't be mistaken
/// for the blue "on" state of the panel toggles below. See [tab_button].
const TAB_ACTIVE_FILL: Color32 = Color32::from_rgb(128, 64, 208);
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
const TAB_DROP_MARKER_WIDTH: f32 = 2.;

const COLOR_PROMPT: Color32 = Color32::from_rgb(220, 220, 90);
const COLOR_STDOUT: Color32 = Color32::from_rgb(210, 210, 210);
const COLOR_STDERR: Color32 = Color32::from_rgb(240, 110, 110);
// Amber: stderr from a command that succeeded (warnings, progress), so red
// is reserved for failures.
const COLOR_STDERR_OK: Color32 = Color32::from_rgb(240, 180, 70);
// Light green to match the CLI's `his N` highlight.
const COLOR_HIS: Color32 = Color32::from_rgb(120, 240, 120);
// Magenta to match the CLI's branch-indicator highlight (`\x1b[95m`).
const COLOR_BRANCH: Color32 = Color32::from_rgb(255, 130, 255);
// Executable-file colour in the file-browser column.
const COLOR_EXECUTABLE: Color32 = Color32::from_rgb(120, 220, 120);

/// Action a list-panel button asked us to take. Collected during rendering
/// (where we only hold `&State`'s fields immutably via iteration) and
/// applied afterwards, so the borrow checker is happy.
enum ListAction {
    ChangeDir(PathBuf),
    /// cd to a Recent dirs or Bookmarks row, offering to remove it from that
    /// list if it no longer exists. See [State::change_saved_dir].
    ChangeSavedDir(PathBuf, DirList),
    /// Open a dir in a new tab, as middle-clicking a Files folder does.
    NewTabDir(PathBuf),
    /// [ListAction::NewTabDir] for a Recent dirs or Bookmarks row. See
    /// [State::open_saved_dir_in_new_tab].
    NewTabSavedDir(PathBuf, DirList),
    /// Add or remove a dir from Bookmarks (the Recent dirs ★).
    ToggleBookmark(PathBuf),
    /// Open a file with the OS's default associated program, the way a
    /// double-click does in Explorer / a Linux file manager.
    OpenFile(PathBuf),
    RemoveBookmark(usize),
    /// Drop a path from Recent dirs (the dir itself is untouched).
    RemoveRecentDir(PathBuf),
    FillInput(String),
    /// Run a command in a new tab at the active tab's cwd, as middle-clicking
    /// a Cmd history or Cmds this dir row does.
    RunInNewTab(String),
    /// Re-run the history item at this index in its original working dir,
    /// without changing the GUI's CWD. Delegates to the `hisd` built-in.
    RunHistoryInDir(usize),
    /// Connect the active tab to the saved remote at this index.
    ConnectRemote(usize),
    /// Load the saved remote at this index into the add/edit form.
    EditRemote(usize),
    /// Delete the saved remote at this index (and its keyring password).
    RemoveRemote(usize),
}

/// `path` as `~/...`, colour-coded by directory (see [path_color]) with its
/// final part bold, in the body font. Uncoloured runs take the label's own
/// text colour.
fn path_job(ui: &Ui, path: &Path, home: Option<&Path>) -> LayoutJob {
    path_job_on(ui, path, home, ui.visuals().dark_mode, Color32::PLACEHOLDER)
}

/// [path_job] in the colours for a `dark` or light background (see
/// [path_color::depth_color]), with uncoloured runs in `text_color`.
fn path_job_on(
    ui: &Ui,
    path: &Path,
    home: Option<&Path>,
    dark: bool,
    text_color: Color32,
) -> LayoutJob {
    let parts = path_color::parts(&render_with_tilde(path, home), Some("~"), dark);
    let font = TextStyle::Body.resolve(ui.style());
    let bold = FontId::new(font.size, bold_family());
    path_color::layout_job(&parts, font, Some(bold), text_color)
}

/// A Cmd history / In this dir button label: `"<i>: "`, then the command
/// `text` with its base command bold and colour-coded (see [CmdColors]).
/// Uncoloured runs take the button's own text colour.
fn cmd_label(ui: &Ui, colors: &CmdColors, i: usize, text: &str) -> LayoutJob {
    let font = TextStyle::Button.resolve(ui.style());
    let bold = FontId::new(font.size, bold_family());
    let mut job = LayoutJob::default();
    let mut append = |text: &str, font_id: FontId, color: Color32| {
        let format = TextFormat {
            font_id,
            color,
            ..Default::default()
        };
        job.append(text, 0.0, format);
    };
    append(&format!("{i}: "), font.clone(), Color32::PLACEHOLDER);
    // The only coloured run is the base command.
    for p in colors.parts(text, ui.visuals().dark_mode) {
        match p.color {
            Some(c) => append(&p.text, bold.clone(), c),
            None => append(&p.text, font.clone(), Color32::PLACEHOLDER),
        }
    }
    job
}

/// Rough width of `n` characters of `style` text, taking each to be as wide as
/// a `0`.
fn chars_width(ui: &Ui, style: TextStyle, n: f32) -> f32 {
    let font = style.resolve(ui.style());
    n * ui.ctx().fonts_mut(|f| f.glyph_width(&font, '0'))
}

/// Lay a list panel's `items` out in as many columns as fit across `ui`, each
/// at least `min_width` wide (or just one if even that doesn't fit), drawing
/// each with `row`. Filled down the first column, then the next, like `ls`,
/// so the list still reads top to bottom: [column_len] items to a column.
/// Returns how many columns fit.
fn list_columns<T: Copy>(
    ui: &mut Ui,
    items: &[T],
    min_width: f32,
    mut row: impl FnMut(&mut Ui, T),
) -> usize {
    let gap = ui.spacing().item_spacing.x;
    let n = ((ui.available_width() + gap) / (min_width + gap)).max(1.0) as usize;
    if n == 1 {
        for &item in items {
            row(ui, item);
        }
        return n;
    }
    let per_col = column_len(items.len(), n);
    ui.columns(n, |cols| {
        for (col, chunk) in cols.iter_mut().zip(items.chunks(per_col)) {
            for &item in chunk {
                row(col, item);
            }
        }
    });
    n
}

/// How many of `len` items [list_columns] puts in each column, when
/// `columns` fit. The last may have fewer.
fn column_len(len: usize, columns: usize) -> usize {
    len.div_ceil(columns.max(1)).max(1)
}

/// The least height a [Divider] leaves the details pane below it, and by
/// default the list above.
const MIN_SPLIT_HEIGHT: f32 = 40.;

/// The fraction of its panel a details pane under a [Divider] takes, until
/// the divider is dragged.
pub const DEFAULT_DETAILS_SPLIT: f32 = 1. / 3.;

/// A divider across a panel, between a list above and a details pane below
/// (a `Panel::bottom`), that the user drags up or down to share the panel's
/// height between them. Its position is kept as the fraction of the panel
/// the details take (`split`), so it holds as the window is resized.
///
/// Make one with [Divider::begin] before laying anything out, show the
/// details pane at [Divider::height], then call [Divider::end] once the
/// list above is drawn too, so the divider is on top of both for the
/// pointer.
pub struct Divider {
    id: egui::Id,
    /// The panel's height and bottom edge, measured by [Divider::begin].
    total: f32,
    bottom: f32,
}

impl Divider {
    /// Measure the panel `ui` is laying out, and follow a drag of the
    /// divider in progress, setting `split`.
    pub fn begin(ui: &Ui, id_salt: &str, split: &mut f32) -> Self {
        let id = ui.id().with(id_salt);
        let space = ui.available_rect_before_wrap();
        let divider = Self {
            id,
            total: space.height(),
            bottom: space.bottom(),
        };
        // Read before the pane is drawn, so it follows the pointer without
        // lagging a frame behind.
        if let Some(r) = ui.ctx().read_response(id)
            && r.dragged()
            && let Some(p) = r.interact_pointer_pos()
        {
            *split = divider.fraction(divider.bottom - p.y);
        }
        divider
    }

    /// The details pane's height for `split`, leaving at least `keep_above`
    /// of what's left of `ui` for the rest of the panel above it.
    pub fn height(&self, ui: &Ui, split: f32, keep_above: f32) -> f32 {
        let max = (ui.available_height() - keep_above).max(MIN_SPLIT_HEIGHT);
        (split * self.total).clamp(MIN_SPLIT_HEIGHT, max)
    }

    /// Make the top edge of `pane`, the details pane as drawn, draggable:
    /// highlighted while hovered or dragged, as an egui panel's edge is.
    /// Once a drag ends, sets `split` to where the pane was left (within
    /// the limits [Self::height] set) and returns `true`, for the caller to
    /// save it.
    pub fn end(self, ui: &Ui, pane: Rect, split: &mut f32) -> bool {
        let grab = ui.style().interaction.resize_grab_radius_side;
        let y = pane.top();
        let rect = Rect::from_x_y_ranges(pane.x_range(), egui::Rangef::new(y - grab, y + grab));
        let resp = ui.interact(rect, self.id, egui::Sense::drag());
        if resp.hovered() || resp.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
            let widgets = &ui.visuals().widgets;
            let stroke = if resp.dragged() {
                widgets.active.fg_stroke
            } else {
                widgets.hovered.fg_stroke
            };
            // Over the pane's own separator line.
            ui.painter()
                .hline(pane.x_range(), y + stroke.width / 2., stroke);
        }
        if resp.drag_stopped() {
            *split = self.fraction(pane.height());
            return true;
        }
        false
    }

    /// `height` as a fraction of the panel's.
    fn fraction(&self, height: f32) -> f32 {
        (height / self.total.max(1.)).clamp(0.01, 0.99)
    }
}

/// Display commands previously run in the active tab's cwd. Newest first, in
/// columns when the panel is wide enough. Same click behavior as the global
/// history panel (see [cmd_click]).
fn cwd_cmd_history(state: &mut State, ui: &mut Ui) {
    panel_header(
        ui,
        "Cmds this dir",
        &mut state.ui.panel_vis.recent_cmds_in_dir,
    );

    if search_box(ui, &mut state.ui.cwd_history_search, "cwd_history_search") {
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
            let min_width = chars_width(ui, TextStyle::Button, CMD_COL_MIN_CHARS);
            list_columns(ui, &rows, min_width, |ui, i| {
                let Some(item) = state.history.get(i) else {
                    return;
                };

                let label = cmd_label(ui, &state.cmd_colors, i, &item.text);
                let btn = Button::new(label)
                    .wrap()
                    .min_size(egui::vec2(ui.available_width(), 0.0));
                if let Some(a) = cmd_click(ui.add(btn), &item.text) {
                    action = Some(a);
                }
            });
        });

    apply_action(state, action);
}

/// What a click on a Cmd history / Cmds this dir row does: a left click
/// copies `text` into the input box, a middle click runs it in a new tab.
fn cmd_click(resp: egui::Response, text: &str) -> Option<ListAction> {
    if resp.middle_clicked() {
        Some(ListAction::RunInNewTab(text.to_owned()))
    } else if resp.clicked() {
        Some(ListAction::FillInput(text.to_owned()))
    } else {
        None
    }
}

/// Display recent commands entered, newest first, in columns when the panel
/// is wide enough. Clicking the label copies
/// the command into the input box so the user can edit or run it, and
/// middle-clicking runs it in a new tab (see [cmd_click]); the "In dir"
/// button, shown while "Show dir" is on, re-runs it in the directory it was
/// originally run from.
fn cmd_history(state: &mut State, ui: &mut Ui) {
    panel_header(ui, "Cmd history", &mut state.ui.panel_vis.recent_cmds);

    let show_dir = state.cmd_history_show_dir;
    let hover = if show_dir {
        "Showing the dir each command was run in. Click to hide it."
    } else {
        "Hiding the dir each command was run in. Click to show it."
    };
    if ui
        .add(Button::selectable(show_dir, "Show dir"))
        .on_hover_text(hover)
        .clicked()
    {
        state.cmd_history_show_dir = !show_dir;
        let _ = state.save();
    }

    if search_box(ui, &mut state.ui.history_search, "history_search") {
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
            let min_width = chars_width(ui, TextStyle::Button, CMD_COL_MIN_CHARS);
            list_columns(ui, &rows, min_width, |ui, i| {
                let Some(item) = state.history.get(i) else {
                    return;
                };
                let label = cmd_label(ui, &state.cmd_colors, i, &item.text);
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

    apply_action(state, action);
}

fn apply_action(state: &mut State, action: Option<ListAction>) {
    match action {
        Some(ListAction::ChangeDir(p)) => state.change_dir(p),
        Some(ListAction::ChangeSavedDir(p, list)) => state.change_saved_dir(p, list),
        Some(ListAction::NewTabDir(p)) => state.open_dir_in_new_tab(p),
        Some(ListAction::NewTabSavedDir(p, list)) => state.open_saved_dir_in_new_tab(p, list),
        Some(ListAction::ToggleBookmark(p)) => {
            let msg = state.toggle_bookmark(p);
            state.push_out(msg, OutType::StdOut);
        }
        // Either way, the file's dir moves to the top of Recent dirs.
        Some(ListAction::OpenFile(p)) => {
            if let Some(dir) = p.parent() {
                state.touch_recent_dir(dir);
            }
            if viewer::can_view(&p) {
                state.file_viewers.open(p);
            } else if let Err(e) = open_with_default_app(&p) {
                state.push_out(
                    format!("open: failed to open {}: {e}", p.display()),
                    OutType::StdErr,
                );
            }
        }
        Some(ListAction::RemoveBookmark(i)) => {
            if i < state.dir_bookmarks.len() {
                let path = state.dir_bookmarks.remove(i);
                state.refresh_bookmarks_filter();
                state.push_out(
                    format!("Deleted bookmark: {}", path.display()),
                    OutType::StdOut,
                );
                let _ = state.save();
            }
        }
        Some(ListAction::RemoveRecentDir(p)) => {
            state.remove_from_dir_list(&p, DirList::RecentDirs);
        }
        Some(ListAction::FillInput(s)) => {
            let i = state.ui.active_tab;
            state.ui.cli_input[i] = s;
            state.ui.focus_input = true;
        }
        Some(ListAction::RunInNewTab(s)) => {
            state.add_tab();
            submit_command(state, &s);
        }
        Some(ListAction::RunHistoryInDir(i)) => {
            // Reuse the `hisd` built-in so the spawn / output-capture path
            // stays in one place (run_command in main.rs).
            submit_command(state, &format!("hisd {i}"));
        }
        Some(ListAction::ConnectRemote(i)) => state.connect_remote(i),
        Some(ListAction::EditRemote(i)) => {
            if let Some(r) = state.remote_terminals.get(i) {
                state.ui.remote_host = r.host.clone();
                state.ui.remote_port = r.port.to_string();
                state.ui.remote_user = r.username.clone();
                state.ui.remote_password.clear();
                state.ui.remote_edit_idx = Some(i);
                state.ui.remote_form_open = true;
            }
        }
        Some(ListAction::RemoveRemote(i)) => state.remove_remote(i),
        None => {}
    }
}

/// Override the `Monospace` text style's size on `ui` so the terminal panes
/// render at the user-chosen font size. Both panes resolve `Monospace` (the
/// output pane for its layout jobs, the input row for its prompt labels and
/// `TextEdit`), so setting it here is enough to scale all the terminal text.
fn set_mono_size(ui: &mut Ui, size: f32) {
    if let Some(font) = ui
        .style_mut()
        .text_styles
        .get_mut(&egui::TextStyle::Monospace)
    {
        font.size = size;
    }
}

/// The "terminal" output pane: scrolls, sticks to the bottom so the latest
/// line stays in view, and colours rows by stream kind.
fn term_out(state: &mut State, ui: &mut Ui) {
    set_mono_size(ui, state.font_size);
    terminal::show(&mut state.ui.out[state.ui.active_tab], ui);
}

/// The input row: prompt line on top, with an edit beneath it that wraps long
/// input onto extra rows. Enter submits. When the user is recalling a history entry via Up/Down or
/// a recent dir via Left/Right, a green ` his N` / ` cd N` label is
/// inserted between the cwd and the `$`.
fn term_in(state: &mut State, ui: &mut Ui) {
    // Scale the prompt labels and input box with the chosen font size. Both
    // resolve `Monospace`, so overriding it on this panel's `ui` is enough.
    set_mono_size(ui, state.font_size);
    let active = state.ui.active_tab;
    let (his_cursor, cd_cursor) = state.active_nav_cursors();
    // Locally, the cwd's directories are colour-coded (see [path_color]), with
    // the `*` and separators left in the prompt colour.
    let cwd_parts = if state.is_remote() {
        vec![Part::plain(state.cwd_display())]
    } else {
        let mut parts = vec![Part::plain(state.cwd_star())];
        parts.extend(state.cwd_color_parts());
        parts
    };
    let branch = state.branch[active].clone();
    // While remote, the cwd label already shows `user@host`; don't also show
    // the (stale) local git branch.
    let show_branch = !state.is_remote();
    let running = state.running_here().map(|cmd| cmd.line.clone());
    let mut stop = false;

    // Wrapped, so a long cwd or running command flows onto extra rows (and
    // the Stop button drops below) instead of running off the right edge.
    ui.horizontal_wrapped(|ui| {
        let mono = TextStyle::Monospace.resolve(ui.style());
        ui.label(path_color::layout_job(&cwd_parts, mono, None, COLOR_PROMPT));

        if let Some(b) = branch {
            if show_branch {
                ui.label(
                    RichText::new(format!("br: {}", shell::truncate_branch(&b, 10)))
                        .color(COLOR_BRANCH)
                        .monospace(),
                );
            }
        }

        if let Some(i) = his_cursor {
            ui.label(
                RichText::new(format!("his {i}"))
                    .color(COLOR_HIS)
                    .monospace(),
            );
        }

        if let Some(i) = cd_cursor {
            ui.label(
                RichText::new(format!("cd {i}"))
                    .color(COLOR_HIS)
                    .monospace(),
            );
        }

        // While a command runs, the input goes to its stdin instead.
        match &running {
            Some(line) => {
                ui.label(
                    RichText::new(format!("running: {line}"))
                        .color(COLOR_STDERR_OK)
                        .monospace(),
                );
                stop = ui
                    .button("Stop")
                    .on_hover_text("End the command and everything it started (Ctrl+C)")
                    .clicked();
            }
            None => {
                ui.label(RichText::new("$").color(COLOR_PROMPT).monospace());
            }
        }
    });

    // The keys we repurpose are consumed *before* the edit is drawn, while it
    // has focus, so it never sees them: Enter submits, Tab autocompletes,
    // ↑/↓ walk the global history list, and ←/→ walk recent dirs. A
    // multi-line edit would otherwise use these for newlines, tab-indents and
    // moving between wrapped rows. Left/Right only steal the keystroke when
    // the buffer is empty or a cd recall is already active — otherwise they
    // stay as normal caret movement.
    let input_id = ui.make_persistent_id("cli_input");
    let focused = ui.memory(|m| m.has_focus(input_id));
    let consume =
        |ui: &mut Ui, key| focused && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, key));

    let submitted = consume(ui, Key::Enter);
    let tab = consume(ui, Key::Tab);
    let up = !tab && consume(ui, Key::ArrowUp);
    let down = !tab && consume(ui, Key::ArrowDown);
    let cd_active = cd_cursor.is_some() || state.ui.cli_input[active].is_empty();
    let left = !tab && !up && !down && cd_active && consume(ui, Key::ArrowLeft);
    let right = !tab && !up && !down && !left && cd_active && consume(ui, Key::ArrowRight);

    // → (or End) with the caret at the end accepts the greyed history
    // suggestion, as in the CLI.
    let caret_at_end = TextEdit::load_state(ui.ctx(), input_id)
        .and_then(|s| s.cursor.char_range())
        .is_none_or(|r| {
            r.is_empty()
                && r.primary.index
                    == egui::text::CharIndex(state.ui.cli_input[active].chars().count())
        });
    let hint = if focused && caret_at_end && running.is_none() {
        state.input_hint()
    } else {
        None
    };

    let accept_hint = hint.is_some()
        && !tab
        && !up
        && !down
        && !left
        && !right
        && (consume(ui, Key::ArrowRight) || consume(ui, Key::End));

    // Ctrl+C stops a running command, as in a terminal, unless there's text
    // selected for it to copy (egui sends it as a Copy event, not a key).
    if running.is_some()
        && focused
        && ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)))
    {
        let input_selected = TextEdit::load_state(ui.ctx(), input_id)
            .and_then(|s| s.cursor.char_range())
            .is_some_and(|r| !r.is_empty());
        let label_selected = ui
            .ctx()
            .with_plugin(|s: &mut egui::text_selection::LabelSelectionState| s.has_selection())
            .unwrap_or(false);
        stop |= !input_selected && !label_selected;
    }
    if stop {
        state.stop_running();
        state.ui.focus_input = true;
    }

    // Input box sits on its own row, directly beneath the prompt line above.
    // It's multi-line so a long command wraps onto extra rows (and the panel
    // grows to fit) instead of scrolling off to the right, but it's still one
    // logical line: Enter never inserts a newline (`return_key(None)`), and
    // pasted newlines are flattened to spaces below. `lock_focus` stops egui
    // from using Tab for widget-to-widget focus navigation, since we use it
    // for path autocompletion.
    if let Some(text) = state.ui.clipboard.pending_text.take() {
        ui.ctx().memory_mut(|m| m.request_focus(input_id));
        ui.ctx()
            .input_mut(|i| i.events.push(egui::Event::Paste(text)));
    }
    let output = TextEdit::multiline(&mut state.ui.cli_input[active])
        .id(input_id)
        .desired_rows(1)
        .desired_width(f32::INFINITY)
        .return_key(None)
        .lock_focus(true)
        .font(egui::TextStyle::Monospace)
        .show(ui);
    let response = output.response;

    // Grey ghost text for the history suggestion, painted just after the
    // end of the typed text. Clipped to the whole box: the edit's own text
    // clip rect ends where the typed text does.
    if let Some(hint) = hint.as_ref().filter(|_| !accept_hint) {
        let end = egui::text::CCursor::new(state.ui.cli_input[active].chars().count());
        let pos = output.galley_pos + output.galley.pos_from_cursor(end).min.to_vec2();
        ui.painter().with_clip_rect(response.rect).text(
            pos,
            egui::Align2::LEFT_TOP,
            hint,
            TextStyle::Monospace.resolve(ui.style()),
            ui.visuals().weak_text_color(),
        );
    }

    // Same flattening a single-line edit applies to pastes. It's
    // char-for-char, so the edit's caret position stays valid.
    let buf = &mut state.ui.cli_input[active];
    if buf.contains(['\r', '\n']) {
        *buf = buf.replace(['\r', '\n'], " ");
    }

    if state.ui.focus_input {
        response.request_focus();
        state.ui.focus_input = false;
    }

    if tab || accept_hint {
        if tab {
            state.autocomplete_input();
        } else if let Some(hint) = &hint {
            state.ui.cli_input[active].push_str(hint);
        }
        // egui's TextEdit keeps its own caret position, which isn't
        // updated when we replace the buffer behind its back. Move the
        // caret to the end so it follows the completed text.
        if let Some(mut edit_state) = TextEdit::load_state(ui.ctx(), response.id) {
            let end = CCursor::new(state.ui.cli_input[active].chars().count());
            edit_state
                .cursor
                .set_char_range(Some(CCursorRange::one(end)));
            edit_state.store(ui.ctx(), response.id);
        }
    } else if up {
        state.history_nav(true);
    } else if down {
        state.history_nav(false);
    } else if left {
        state.recent_dir_nav(true);
    } else if right {
        state.recent_dir_nav(false);
    }

    if submitted {
        let input = std::mem::take(&mut state.ui.cli_input[active]);
        // In interactive (PTY) mode the line goes straight to the remote shell,
        // which echoes it back — so we don't run it as a command or echo it
        // locally. Exec mode (and all local tabs) go through `submit_command`.
        let pty = state
            .active_remote()
            .map(|s| s.mode() == SshMode::Pty)
            .unwrap_or(false);

        if pty {
            state.send_remote_line(&input);
        } else if running.is_some() {
            state.send_running_line(&input);
        } else {
            submit_command(state, &input);
        }
        state.ui.focus_input = true;
    }
}

/// The edit box that stands in for a Files row while it's being renamed.
/// Returns `Some(true)` once editing ends with Enter or a click elsewhere,
/// and `Some(false)` if Escape cancelled it.
fn rename_box(ui: &mut Ui, r: &mut FileRename, is_folder: bool) -> Option<bool> {
    // Not derived from `ui`'s id, so it survives the row moving between the
    // list's columns (e.g. when a refresh changes the row count).
    let id = egui::Id::new("file_rename");
    ui.horizontal(|ui| {
        if is_folder {
            ui.label(RichText::new("📁").monospace());
        }
        let mut out = TextEdit::singleline(&mut r.name)
            .id(id)
            .font(TextStyle::Monospace)
            .desired_width(f32::INFINITY)
            .show(ui);

        if !r.focused {
            // Like Explorer, select the name but not its extension, so typing
            // replaces just the name; a folder's whole name is selected.
            r.focused = true;
            out.response.request_focus();
            let end = if is_folder {
                r.name.chars().count()
            } else {
                Path::new(&r.name)
                    .file_stem()
                    .map_or(0, |s| s.to_string_lossy().chars().count())
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

/// How long Deep search waits after the last keystroke before searching, so
/// it doesn't walk the whole tree once per letter typed.
const DEEP_SEARCH_DELAY: f64 = 0.3;

/// The Deep search box's, for the Files panel to tell it has the keyboard.
fn deep_search_id() -> egui::Id {
    egui::Id::new("files_deep_search")
}

/// The Files panel's Deep search box, followed on its row by the switch
/// between searching file names and contents and the regex toggle (see
/// `crate::deep_search`). A search starts once typing pauses for
/// [DEEP_SEARCH_DELAY], or at once on Enter (which also re-runs an
/// unchanged one) or a toggle; clearing the box ends it. Also takes in the
/// running search's latest results.
fn deep_search_bar(state: &mut State, ui: &mut Ui) {
    let now = ui.input(|i| i.time);
    let mut run_now = false;
    // Filenames / Contents was switched; unlike the regex toggle, it's saved.
    let mut mode_changed = false;
    let opts = &mut state.ui.deep_search_opts;
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui
                .add(Button::selectable(opts.regex, ".*"))
                .on_hover_text("Treat the Deep search as a regular expression, not plain text")
                .clicked()
            {
                opts.regex = !opts.regex;
                run_now = true;
            }
            // Right to left, so Contents comes before Filenames.
            let before = opts.file_names;
            ui.selectable_value(&mut opts.file_names, false, "Contents")
                .on_hover_text("Match lines inside files, like `rg <search>`");
            ui.selectable_value(&mut opts.file_names, true, "Filenames")
                .on_hover_text(
                    "Match file and folder names, like `rg --files` filtered by the search",
                );
            mode_changed = opts.file_names != before;
            run_now |= mode_changed;

            let what = if opts.file_names {
                "names of every file and folder"
            } else {
                "contents of every file"
            };

            let resp = ui
                .add(
                    TextEdit::singleline(&mut state.ui.deep_search)
                        .hint_text("Deep search")
                        .desired_width(f32::INFINITY)
                        .id(deep_search_id()),
                )
                .on_hover_text(format!(
                    "Search the {what} in this folder and its subfolders, \
                     as ripgrep does: hidden and .gitignored files are skipped. \
                     Case-insensitive unless the search has a capital letter."
                ));

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
        });
    });

    if mode_changed {
        let _ = state.save();
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

/// `n` and the noun for it, e.g. `1 file`, `3 files`.
fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
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
fn deep_search_results(
    search: &DeepSearch,
    name_filter: &str,
    selected: &HashSet<PathBuf>,
    bookmarks: &[PathBuf],
    home: Option<&Path>,
    ui: &mut Ui,
) -> Option<file_browser::RowAction> {
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
    let files = plural(search.hits.len() - n_folders, "file", "files");
    let (counts, unit) = if search.opts.file_names {
        let folders = plural(n_folders, "folder", "folders");
        let counts = match (search.hits.len() - n_folders, n_folders) {
            (_, 0) => files,
            (0, _) => folders,
            _ => format!("{files}, {folders}"),
        };
        (counts, "results")
    } else {
        let matches = plural(search.n_matches, "match", "matches");
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
                plural(incomplete, "file", "files")
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
                                base.color = dirs::BOOKMARKED_TEXT;
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

/// One "Label  value" line of the Files details pane, wrapping when the
/// value is long.
fn detail_field(ui: &mut Ui, label: &str, value: WidgetText) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(label).weak());
        ui.label(value);
    });
}

/// A time as the Files details pane shows it, e.g. `2025-10-15 14:05`.
fn detail_time(t: SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(t)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

/// Most names [file_details] lists while several rows are selected.
const MAX_SELECTED_NAMES: usize = 200;

/// The Files panel's lower pane: type, size, dates and attributes of the
/// selected row, read when it was selected (see [FileDetails::read]). Under
/// them, an image's `preview` fills the rest of the pane. While more than
/// one row is selected, their `summary` instead: how many files and
/// folders, the files' total size, when they were last modified, and their
/// names.
fn file_details(
    details: Option<&FileDetails>,
    summary: Option<&SelectionSummary>,
    home: Option<&Path>,
    preview: &viewer::ImagePreview,
    ui: &mut Ui,
) {
    if let Some(s) = summary {
        selection_details(s, home, ui);
        return;
    }
    let Some(d) = details else {
        ui.label(
            RichText::new("Select a file to see its details.")
                .italics()
                .weak(),
        );
        return;
    };

    // Only as tall as the text, so the preview gets the rest; it scrolls
    // when the pane is too short for it all.
    ScrollArea::vertical()
        .id_salt("file_details_scroll")
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.style_mut().wrap_mode = Some(TextWrapMode::Wrap);

            ui.label(RichText::new(&d.name).strong());
            detail_field(ui, "Type", d.type_name.clone().into());
            if let Some(size) = d.size {
                detail_field(ui, "Size", format_size(size).into());
            }
            if let Some([w, h]) = preview.size() {
                detail_field(ui, "Dimensions", format!("{w} × {h}").into());
            }
            if let Some(n) = d.item_count {
                let s = if n == 1 { "" } else { "s" };
                detail_field(ui, "Contains", format!("{n} item{s}").into());
            }
            if let Some(t) = d.modified {
                detail_field(ui, "Modified", detail_time(t).into());
            }
            if let Some(t) = d.created {
                detail_field(ui, "Created", detail_time(t).into());
            }
            let attrs: Vec<&str> = [
                (d.readonly, "Read-only"),
                (d.hidden, "Hidden"),
                (d.executable && !d.is_folder, "Executable"),
            ]
            .into_iter()
            .filter_map(|(on, name)| on.then_some(name))
            .collect();
            if !attrs.is_empty() {
                detail_field(ui, "Attributes", attrs.join(", ").into());
            }
            if let Some(target) = &d.link_target {
                detail_field(ui, "Link to", path_job(ui, target, home).into());
            }
            detail_field(ui, "Path", path_job(ui, &d.path, home).into());
        });
    ui.add_space(4.);
    preview.show(ui);
}

/// [file_details] while several rows are selected.
fn selection_details(s: &SelectionSummary, home: Option<&Path>, ui: &mut Ui) {
    ScrollArea::vertical()
        .id_salt("file_details_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.style_mut().wrap_mode = Some(TextWrapMode::Wrap);

            ui.label(RichText::new(format!("{} items selected", s.items.len())).strong());
            let files = plural(s.files, "file", "files");
            let folders = plural(s.folders, "folder", "folders");
            let counts = match (s.files, s.folders) {
                (_, 0) => files,
                (0, _) => folders,
                _ => format!("{files}, {folders}"),
            };
            detail_field(ui, "Selected", counts.into());
            if s.files > 0 {
                let mut size = format_size(s.size);
                if s.folders > 0 {
                    size.push_str(", not counting the folders");
                }
                detail_field(ui, "Size", size.into());
            }
            if let Some((oldest, newest)) = s.modified {
                let (oldest, newest) = (detail_time(oldest), detail_time(newest));
                let when = if oldest == newest {
                    oldest
                } else {
                    format!("{oldest} to {newest}")
                };
                detail_field(ui, "Modified", when.into());
            }
            if let Some(dir) = &s.dir {
                detail_field(ui, "In", path_job(ui, dir, home).into());
            }

            ui.add_space(4.);
            ui.spacing_mut().item_spacing.y = 0.;
            for (name, is_folder) in s.items.iter().take(MAX_SELECTED_NAMES) {
                let text = if *is_folder {
                    format!("📁 {name}")
                } else {
                    name.clone()
                };
                ui.label(RichText::new(text).monospace());
            }
            if s.items.len() > MAX_SELECTED_NAMES {
                let more = s.items.len() - MAX_SELECTED_NAMES;
                ui.label(RichText::new(format!("and {more} more")).italics().weak());
            }
        });
}

/// Saved SSH remotes. Lists each `user@host:port` with Connect / Edit / delete
/// controls, an add/edit form, and — when the active tab is connected — a
/// status line with Disconnect and an Exec⇄PTY mode toggle. Passwords are kept
/// in the OS keyring (see `shell::secrets`), never in the panel or state file.
fn remote_terminals(state: &mut State, ui: &mut Ui) {
    panel_header(ui, "Remote", &mut state.ui.panel_vis.remote_terminals);

    // Connection status for the active tab.
    if let Some(remote) = state.active_remote() {
        let label = remote.label();
        let mode = match remote.mode() {
            SshMode::Exec => "exec",
            SshMode::Pty => "interactive",
        };
        ui.label(
            RichText::new(format!("● {label}  [{mode}]"))
                .color(COLOR_EXECUTABLE)
                .strong(),
        );
        ui.horizontal(|ui| {
            if ui.button("Disconnect").clicked() {
                state.disconnect_remote();
            }

            let mut label = "Exec mode";
            if let Some(s) = state.active_remote() {
                if let SshMode::Pty = s.mode() {
                    label = "PTY mode";
                }
            }

            if ui
                .button(label)
                .on_hover_text("Toggle between per-command and interactive shell mode")
                .clicked()
            {
                state.toggle_remote_mode();
            }
        });
        ui.separator();
    }

    // Add / edit form toggle.
    ui.horizontal(|ui| {
        let label = if state.ui.remote_form_open {
            "Close form"
        } else {
            "+ Add remote"
        };
        if ui.button(label).clicked() {
            state.ui.remote_form_open = !state.ui.remote_form_open;
            if !state.ui.remote_form_open {
                state.ui.remote_edit_idx = None;
            }
        }
    });

    if state.ui.remote_form_open {
        remote_form(state, ui);
    }

    ui.separator();

    let mut action: Option<ListAction> = None;

    ScrollArea::vertical()
        .id_salt("remote_terminals_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.style_mut().wrap_mode = Some(TextWrapMode::Wrap);
            if state.remote_terminals.is_empty() {
                ui.label(RichText::new("(no saved remotes)").italics());
                return;
            }
            for (i, rt) in state.remote_terminals.iter().enumerate() {
                ui.label(format!("{i}: {}@{}:{}", rt.username, rt.host, rt.port));
                ui.horizontal(|ui| {
                    if ui.button("Connect").clicked() {
                        action = Some(ListAction::ConnectRemote(i));
                    }
                    if ui.small_button("Edit").clicked() {
                        action = Some(ListAction::EditRemote(i));
                    }
                    if ui
                        .small_button("x")
                        .on_hover_text("Delete remote")
                        .clicked()
                    {
                        action = Some(ListAction::RemoveRemote(i));
                    }
                });
                ui.separator();
            }
        });

    apply_action(state, action);
}

/// The add/edit form for a saved remote. Writes through
/// [State::save_remote_from_form] on Save (storing the password in the keyring
/// when one was entered).
fn remote_form(state: &mut State, ui: &mut Ui) {
    egui::Grid::new("remote_form_grid")
        .num_columns(2)
        .spacing([6.0, 4.0])
        .show(ui, |ui| {
            ui.label("Host");
            ui.add(
                TextEdit::singleline(&mut state.ui.remote_host)
                    .hint_text("example.com")
                    .desired_width(f32::INFINITY),
            );
            ui.end_row();

            ui.label("Port");
            ui.add(
                TextEdit::singleline(&mut state.ui.remote_port)
                    .hint_text("22")
                    .desired_width(f32::INFINITY),
            );
            ui.end_row();

            ui.label("User");
            ui.add(
                TextEdit::singleline(&mut state.ui.remote_user)
                    .hint_text("root")
                    .desired_width(f32::INFINITY),
            );
            ui.end_row();

            ui.label("Password");
            ui.add(
                TextEdit::singleline(&mut state.ui.remote_password)
                    .password(true)
                    .hint_text("(stored in OS keyring)")
                    .desired_width(f32::INFINITY),
            );
            ui.end_row();
        });

    ui.horizontal(|ui| {
        if ui.button("Save").clicked() {
            if let Err(e) = state.save_remote_from_form() {
                state.push_out(format!("remote: {e}"), OutType::StdErr);
            }
        }
        if ui.button("Cancel").clicked() {
            state.ui.remote_form_open = false;
            state.ui.remote_edit_idx = None;
            state.ui.remote_password.clear();
        }
    });
}

/// Row of show/hide toggles for the optional side panels. Lives in the
/// top tabs panel (rendered next to the tab strip) so that even when
/// every side panel is hidden, the user still has a way to bring one
/// back. Each toggle reads from / writes back to [PanelVis] on the
/// active StateUi.
fn panel_toggles(state: &mut State, ui: &mut Ui) {
    ui.horizontal(|ui| {
        let v = &mut state.ui.panel_vis;
        ui.toggle_value(&mut v.bookmarks, "Bosynokmarks");
        ui.toggle_value(&mut v.recent_dirs, "Recent dirs");
        ui.toggle_value(&mut v.file_browser, "Files");
        ui.toggle_value(&mut v.remote_terminals, "Remote");
        ui.toggle_value(&mut v.recent_cmds_in_dir, "Cmds this dir");
        ui.toggle_value(&mut v.recent_cmds, "Cmd history");
        // Opens a window of its own rather than a side panel.
        ui.toggle_value(&mut v.path_editor, "PATH");
        ui.toggle_value(&mut v.clipboard, "Clipboard")
            .on_hover_text("Toggle clipboard history (Ctrl+Shift+V)");
    });
}

/// Field-by-field equality so a toggle change triggers a save without
/// adding PartialEq derives on the upstream struct.
fn panel_vis_eq(a: &shell::PanelVis, b: &shell::PanelVis) -> bool {
    a.bookmarks == b.bookmarks
        && a.recent_dirs == b.recent_dirs
        && a.recent_cmds == b.recent_cmds
        && a.recent_cmds_in_dir == b.recent_cmds_in_dir
        && a.remote_terminals == b.remote_terminals
        && a.file_browser == b.file_browser
        && a.path_editor == b.path_editor
        && a.clipboard == b.clipboard
}

/// Returns the requested change in font size, or zero when unchanged.
fn font_size_control(ui: &mut Ui, size: f32) -> f32 {
    let mut val = size.round() as i32;

    let response = ui
        .add(
            DragValue::new(&mut val)
                .range(MIN_FONT_SIZE..=MAX_FONT_SIZE)
                .speed(0.25)
                .update_while_editing(false),
        )
        .on_hover_text(
            "Increase or decrease font size. Drag, or click to type."
        );

    if response.changed() {
        val as f32 - size
    } else {
        0.
    }
}

fn step_control(ui: &mut Ui, label: &str, value: i32, less: &str, more: &str) -> i32 {
    ui.label(label);
    ui.label(format!("{value}"));
    let mut delta = 0;

    if ui.small_button("-").on_hover_text(less).clicked() {
        delta = -1;
    }

    if ui.small_button("+").on_hover_text(more).clicked() {
        delta = 1;
    }

    delta
}

/// One [TAB_WIDTH]-wide tab of the top strip. The label is laid out by hand
/// so it can be cut off at the edge with no ellipsis (egui's own truncation
/// always adds one); the full label is in the hover text when cut. With
/// `closable`, an `x` sits over the tab's right end. Returns the tab's
/// response and whether the `x` was clicked.
fn tab_button(ui: &mut Ui, text: &str, selected: bool, closable: bool) -> (egui::Response, bool) {
    let close_width = if closable {
        ui.spacing().interact_size.y
    } else {
        0.
    };

    let label_width = TAB_WIDTH - 2. * ui.spacing().button_padding.x - close_width;
    // The font a plain-text button label would get. PLACEHOLDER color lets
    // the button apply its own (selected/hovered).
    let mut job = LayoutJob::simple_singleline(
        text.to_owned(),
        FontSelection::Default.resolve_with_fallback(ui.style(), TextStyle::Button.into()),
        Color32::PLACEHOLDER,
    );

    job.wrap = TextWrapping {
        overflow_character: None,
        ..TextWrapping::truncate_at_width(label_width.max(0.))
    };

    let galley = ui.ctx().fonts_mut(|f| f.layout_job(job));
    let cut = galley.elided;

    let button =
        Button::selectable(selected, WidgetText::from(galley)).min_size(egui::vec2(TAB_WIDTH, 0.));

    // Left-align the label; horizontal layouts centre button text. A selected
    // button takes its fill and text colour from the selection visuals, so
    // swap in the tab colours for just this button.
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
        tab = tab.on_hover_text(text);
    }

    let close_clicked = closable && {
        let rect = tab.rect;
        let close_rect =
            Rect::from_min_max(egui::pos2(rect.right() - close_width, rect.top()), rect.max);

        // Only the active tab is closable, so match its label colour.
        let x = RichText::new("x").color(TAB_ACTIVE_TEXT);
        let shortcut = ui.ctx().format_shortcut(&tab_keys::CLOSE_TAB);
        ui.put(close_rect, Button::new(x).frame_when_inactive(false))
            .on_hover_text(format!("Close tab ({shortcut})"))
            .clicked()
    };

    (tab, close_clicked)
}

/// Top-of-window tab strip. Each tab labels itself with its cwd's leaf
/// folder; clicking selects, dragging moves it along the strip, the active
/// tab's `x` (or middle-clicking any tab) closes it (unavailable when only
/// one tab is left), and `+` appends a fresh tab inheriting the active tab's
/// cwd. The keyboard shortcuts are in [tab_keys].
fn tabs_bar(state: &mut State, ui: &mut Ui) {
    const TAB_SECTION_PADDING: f32 = 20.;

    // Snapshot the data we need so the inner closure doesn't fight the
    // borrow checker with the mutable `state` we mutate at the end.
    let active = state.ui.active_tab;
    let n = state.cwd.len();
    let home = state.home.clone();

    let labels: Vec<String> = state
        .cwd
        .iter()
        .map(|p| {
            p.file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| render_with_tilde(p, home.as_deref()))
        })
        .collect();

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
    let mut dir_depth_delta = 0;
    let mut set_dark = None;

    ui.horizontal(|ui| {
        // The theme toggle comes first, showing the theme a click switches to.
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
        // terminal panes, one for the side panels; then the path colours'
        // depth.

        font_delta = font_size_control(ui, font_size);
        ui.separator();
        gui_font_delta = font_size_control(ui, gui_font_size);

        ui.separator();
        // How deep a directory is when its colour reaches yellow (see
        // [path_color::depth_levels]).
        dir_depth_delta = step_control(
            ui,
            "Dir color depth: ",
            path_color::depth_levels() as i32,
            "Fewer directory levels from purple (root or ~) to yellow",
            "More directory levels from purple (root or ~) to yellow",
        );
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

                        for (i, leaf) in labels.iter().enumerate() {
                            // Numbered from 1, to match Ctrl/Cmd + 1-8.
                            let text = format!("{}: {leaf}", i + 1);
                            // Only the active tab shows its `x`, Firefox-style.
                            let is_active = i == active;
                            let (tab, close_clicked) =
                                tab_button(ui, &text, is_active, n > 1 && is_active);
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
                    if ui
                        .button("+ tab")
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
    });

    if let Some(dark) = set_dark {
        state.set_dark_mode(ui.ctx(), dark);
    }

    if font_delta != 0.0 {
        state.adjust_font_size(font_delta);
    }

    if gui_font_delta != 0.0 {
        state.adjust_gui_font_size(gui_font_delta);
    }

    if dir_depth_delta != 0 {
        state.adjust_dir_color_depth(dir_depth_delta);
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

/// Cap a resizable side panel so the central output/input column keeps at
/// least [MIN_CENTRAL_WIDTH] points. `ui` is queried for the width still
/// available at the point the panel is about to be added (egui lays panels out
/// sequentially, so this already excludes any earlier panels). Because we
/// reserve the floor at every panel, the space left over for the central panel
/// can never fall below the floor. `existing_max` lets a panel keep its own
/// tighter cap; pass `f32::INFINITY` if it has none.
fn central_reserve(ui: &Ui, existing_max: f32) -> f32 {
    let reserve = (ui.available_width() - MIN_CENTRAL_WIDTH).max(0.0);
    existing_max.min(reserve)
}

/// Shrink every text style in `ui` by [PANEL_FONT_REDUCTION] points. Called at
/// the top of the tab strip's content closure so it only affects that panel's
/// child `ui`, leaving the main terminal column at the default size.
fn shrink_panel_fonts(ui: &mut Ui) {
    for font_id in ui.style_mut().text_styles.values_mut() {
        font_id.size = (font_id.size - PANEL_FONT_REDUCTION).max(1.0);
    }
}

/// Scale every text style in `ui` so body text is `size` points (the "GUI font
/// size"), keeping their proportions: headings stay larger, small text
/// smaller. Called at the top of each side panel's content closure so it only
/// affects that panel's child `ui`.
fn scale_panel_fonts(ui: &mut Ui, size: f32) {
    let styles = &mut ui.style_mut().text_styles;
    let Some(body) = styles.get(&TextStyle::Body).map(|f| f.size) else {
        return;
    };
    let factor = size / body;
    for font_id in styles.values_mut() {
        font_id.size = (font_id.size * factor).max(1.0);
    }
}

pub fn draw(state: &mut State, ui: &mut Ui) {
    // Snapshot panel visibility so a change from the "Panels:" toggles, a
    // panel's own close button, or the clipboard shortcut (handled by
    // `capture`) is persisted at the end of the frame.
    let vis_before = state.ui.panel_vis;
    // Take in what other apps copy, whether or not the panel is open.
    state.ui.clipboard.sys.watch(ui.ctx());
    clipboard::capture(state, ui.ctx());

    // Run the command submitted last frame. That frame already painted its
    // echo, so the user sees what they ran while a blocking built-in runs.
    if let Some(input) = state.ui.pending_cmd.take() {
        let ctx = ui.ctx().clone();
        run_command(state, &input, &ctx);
    }

    // After that command, which runs in the tab it was typed in, and before
    // any widget, so the input box never sees these keys.
    tab_keys::handle(state, ui.ctx());

    // Bring in output from shell commands running in the background, and
    // wrap up any that have finished. Their reader threads request a repaint
    // whenever there's news.
    state.pump_running();
    // Likewise a Files-panel paste.
    state.pump_paste();
    state.pump_delete(ui.ctx());

    // Pump any live PTY output into the active tab's pane. While a PTY session
    // is active, keep requesting repaints so streamed output (e.g. `top`) keeps
    // flowing even when the user isn't interacting.
    if state.pump_pty() {
        ui.ctx().request_repaint();
    }

    Panel::top("tabs_panel").resizable(false).show(ui, |ui| {
        shrink_panel_fonts(ui);
        tabs_bar(state, ui);
        panel_toggles(state, ui);
    });

    // The side panels' text follows the "GUI font size" control.
    let gui_font_size = state.gui_font_size;

    // Each side panel opens at its saved width (falling back to its default)
    // and records the width it was drawn at, so `save` persists drags.
    let prev = state.ui.panel_widths;

    if state.ui.panel_vis.bookmarks {
        let max = central_reserve(ui, f32::INFINITY);
        let r = Panel::left("bookmarks_panel")
            .resizable(true)
            .default_size(prev.bookmarks.unwrap_or(dirs::BOOKMARK_COL_WIDTH))
            .max_size(max)
            .show(ui, |ui| {
                scale_panel_fonts(ui, gui_font_size);
                dirs::dir_bookmarks(state, ui);
            });
        state.ui.panel_widths.bookmarks = Some(r.response.rect.width());
    }

    if state.ui.panel_vis.recent_dirs {
        let max = central_reserve(ui, f32::INFINITY);
        let r = Panel::left("recent_dirs_panel")
            .resizable(true)
            .default_size(prev.recent_dirs.unwrap_or(dirs::RECENT_DIRS_COL_WIDTH))
            .max_size(max)
            .show(ui, |ui| {
                scale_panel_fonts(ui, gui_font_size);
                dirs::dir_history(state, ui);
            });
        state.ui.panel_widths.recent_dirs = Some(r.response.rect.width());
    }

    if state.ui.panel_vis.file_browser {
        let max = central_reserve(ui, f32::INFINITY);
        let r = Panel::left("file_browser_panel")
            .resizable(true)
            .default_size(prev.file_browser.unwrap_or(BROWSER_COL_WIDTH))
            .max_size(max)
            .show(ui, |ui| {
                scale_panel_fonts(ui, gui_font_size);
                file_browser::file_browser(state, ui);
            });
        state.ui.panel_widths.file_browser = Some(r.response.rect.width());
    }

    if state.ui.panel_vis.clipboard {
        let max = central_reserve(ui, f32::INFINITY);
        Panel::right("clipboard_panel")
            .resizable(true)
            .default_size(260.0)
            .max_size(max)
            .show(ui, |ui| {
                scale_panel_fonts(ui, gui_font_size);
                clipboard::draw(state, ui);
            });
    }

    if state.ui.panel_vis.remote_terminals {
        let max = central_reserve(ui, f32::INFINITY);
        let r = Panel::left("remote_terminals_panel")
            .resizable(true)
            .default_size(prev.remote_terminals.unwrap_or(REMOTE_COL_WIDTH))
            .max_size(max)
            .show(ui, |ui| {
                scale_panel_fonts(ui, gui_font_size);
                remote_terminals(state, ui);
            });
        state.ui.panel_widths.remote_terminals = Some(r.response.rect.width());
    }

    if state.ui.panel_vis.recent_cmds {
        // Long commands wrap inside the panel (see [cmd_history]) rather
        // than widening it, so it stays at whatever width it's dragged to.
        let max = central_reserve(ui, f32::INFINITY);
        let r = Panel::right("cmd_history_panel")
            .resizable(true)
            .default_size(prev.recent_cmds.unwrap_or(RIGHT_COL_WIDTH))
            .max_size(max)
            .show(ui, |ui| {
                scale_panel_fonts(ui, gui_font_size);
                cmd_history(state, ui);
            });
        state.ui.panel_widths.recent_cmds = Some(r.response.rect.width());
    }

    if state.ui.panel_vis.recent_cmds_in_dir {
        let max = central_reserve(ui, f32::INFINITY);
        let r = Panel::right("cwd_cmd_history_panel")
            .resizable(true)
            .default_size(prev.recent_cmds_in_dir.unwrap_or(CWD_HIST_COL_WIDTH))
            .max_size(max)
            .show(ui, |ui| {
                scale_panel_fonts(ui, gui_font_size);
                cwd_cmd_history(state, ui);
            });
        state.ui.panel_widths.recent_cmds_in_dir = Some(r.response.rect.width());
    }

    // A width that changed while the pointer is held is a drag on a panel
    // edge. Saved once released (below) rather than every frame of the drag.
    // Changes without a drag (a panel clamped as the window narrows) ride
    // along with the next save, like the window size.
    let pointer_down = ui.input(|i| i.pointer.any_down());
    if pointer_down && state.ui.panel_widths != prev {
        state.ui.panel_widths_unsaved = true;
    }

    // Black backgrounds on the terminal panes so they read as a terminal,
    // not as a regular widget area.
    let term_frame = Frame::default()
        .fill(Color32::BLACK)
        .inner_margin(egui::Margin::same(6));

    // Pin the input row to the bottom of the central area, with the
    // scrolling output pane filling the remaining space above it.
    Panel::bottom("term_in_panel")
        .resizable(false)
        .frame(term_frame)
        .show(ui, |ui| {
            term_in(state, ui);
        });

    CentralPanel::default().frame(term_frame).show(ui, |ui| {
        term_out(state, ui);
    });

    // File viewer windows are child viewports: they only stay open while
    // the main window's pass keeps showing them.
    state.file_viewers.show(ui.ctx());

    // Likewise the PATH editor's window.
    path_panel::path_window(state, ui.ctx());

    about::show(state, ui.ctx());
    dirs::stale_dir_popup(state, ui.ctx());
    file_browser::delete_popup(state, ui.ctx());

    // A command was submitted this frame: paint its echo, then come straight
    // back to run it.
    if state.ui.pending_cmd.is_some() {
        ui.ctx().request_repaint();
    }

    // Persist on any change so the layout and clipboard history survive
    // restart.
    let drag_done = state.ui.panel_widths_unsaved && !pointer_down;
    let clipboard_changed = std::mem::take(&mut state.ui.clipboard.history.unsaved);
    if drag_done || clipboard_changed || !panel_vis_eq(&vis_before, &state.ui.panel_vis) {
        state.ui.panel_widths_unsaved = false;
        let _ = state.save();
    }
}
