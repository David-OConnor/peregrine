use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use egui::{
    Align, Color32, DragValue, FontData, FontDefinitions, FontFamily, FontId, Layout, Panel, Rect,
    RichText, ScrollArea, TextFormat, TextStyle, TextWrapMode, Ui, WidgetText, text::LayoutJob,
};
use shell::DirList;

mod about;
pub mod clipboard;
mod cmd_history;
pub mod dirs;
mod file_browser;
mod grid;
pub mod line_edit;
pub mod panes;
mod path_bar;
pub mod path_panel;
mod project_bar;
mod remote;
pub mod scratchpad;
mod search;
mod settings;
mod tab_keys;
mod tabs;
pub mod terminal;

use crate::{
    OutType,
    cmd_color::CmdColors,
    files::{FileDetails, SelectionSummary, format_size, open_with_default_app},
    path_color::{self},
    render_with_tilde, run_command,
    state::State,
    submit_command, viewer,
};

pub const WINDOW_WIDTH: f32 = 1400.;
pub const WINDOW_HEIGHT: f32 = 900.;

const RIGHT_COL_WIDTH: f32 = 180.;
const CWD_HIST_COL_WIDTH: f32 = 220.;
const BROWSER_COL_WIDTH: f32 = 200.;
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

/// Space between the panes' show/hide toggles and the PATH window's, which
/// sits apart from them. See [panel_toggles].
const PATH_TOGGLE_GAP: f32 = 60.;

// Light green to match the CLI's `his N` highlight.
const COLOR_HIS: Color32 = Color32::from_rgb(120, 240, 120);
// Magenta to match the CLI's branch-indicator highlight (`\x1b[95m`).
const COLOR_BRANCH: Color32 = Color32::from_rgb(255, 130, 255);

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
pub const MAX_FONT_SIZE: f32 = 22.;

/// Name of the bold font family registered by [install_fonts]; see
/// [bold_family].
const BOLD_FAMILY: &str = "bold";

/// A side panel's title row: the heading on the left and a small `x` on the
/// right that hides the panel — the same as unticking it in the "Panels:" row
/// (see [panel_toggles]). `visible` is that panel's [panes::PanelVis] flag.
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

/// System fonts with wide symbol coverage (arrows, math, astrological signs,
/// dingbats, box drawing...), which egui's built-in fonts mostly lack. Every
/// one of these that exists is added by [install_fonts] as a last fallback,
/// so typed or pasted text like "☿→☉" draws instead of showing boxes.
fn symbol_font_candidates() -> Vec<PathBuf> {
    if cfg!(windows) {
        let windir = std::env::var_os("WINDIR").unwrap_or_else(|| r"C:\Windows".into());
        let fonts = PathBuf::from(windir).join("Fonts");
        vec![fonts.join("seguisym.ttf")]
    } else if cfg!(target_os = "macos") {
        vec!["/System/Library/Fonts/Apple Symbols.ttf".into()]
    } else {
        [
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
            "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/noto/NotoSansSymbols-Regular.ttf",
            "/usr/share/fonts/truetype/noto/NotoSansSymbols2-Regular.ttf",
            "/usr/share/fonts/noto/NotoSansSymbols-Regular.ttf",
            "/usr/share/fonts/noto/NotoSansSymbols2-Regular.ttf",
        ]
        .into_iter()
        .map(PathBuf::from)
        .collect()
    }
}

/// Add the system symbol fonts (see [symbol_font_candidates]) to the end of
/// the proportional and monospace families, then register the [bold_family]
/// font family: the first bold system font found (see
/// [bold_font_candidates]), falling back to the normal proportional fonts for
/// glyphs it lacks. If none is found the family is just the proportional
/// fonts, so text drawn in it is still readable, only not bold.
pub fn install_fonts(ctx: &egui::Context) {
    let mut defs = FontDefinitions::default();
    for (i, bytes) in symbol_font_candidates()
        .into_iter()
        .filter_map(|p| std::fs::read(p).ok())
        .enumerate()
    {
        let name = format!("symbols-{i}");
        defs.font_data
            .insert(name.clone(), Arc::new(FontData::from_owned(bytes)));
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            defs.families.entry(family).or_default().push(name.clone());
        }
    }
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
    /// Move the bookmark the Bookmarks panel shows as the first number to
    /// the second, as dragging it there does. See [State::move_bookmark].
    MoveBookmark(usize, usize),
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

/// `path` as `~/...`, color-coded by directory (see [path_color]) with its
/// final part bold, in the body font. Uncolored runs take the label's own
/// text color.
fn path_job(ui: &Ui, path: &Path, home: Option<&Path>) -> LayoutJob {
    path_job_on(ui, path, home, ui.visuals().dark_mode, Color32::PLACEHOLDER)
}

/// [path_job] in the colors for a `dark` or light background (see
/// [path_color::depth_color]), with uncolored runs in `text_color`.
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
/// `text` with its base command bold and color-coded (see [CmdColors]).
/// Uncolored runs take the button's own text color.
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
    // The only colored run is the base command.
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
    /// the limits [Self::height] set).
    pub fn end(self, ui: &Ui, pane: Rect, split: &mut f32) {
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
        }
    }

    /// `height` as a fraction of the panel's.
    fn fraction(&self, height: f32) -> f32 {
        (height / self.total.max(1.)).clamp(0.01, 0.99)
    }
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
        Some(ListAction::MoveBookmark(from, to)) => state.move_bookmark(from, to),
        Some(ListAction::RemoveRecentDir(p)) => {
            state.remove_from_dir_list(&p, DirList::RecentDirs);
        }
        Some(ListAction::FillInput(s)) => {
            let i = state.ui.active_tab;
            state.ui.cli_input[i] = s;
            state.ui.focus_input = true;
            // Into the input box, so it's no use hidden.
            state.ui.panel_vis.terminal = true;
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

/// `n` and the noun for it, e.g. `1 file`, `3 files`.
fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
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

/// Row of show/hide toggles for the panes (see [panes]), in their order, so
/// each sits where its pane does, then the PATH window's. Lives in the
/// top tabs panel (rendered next to the tab strip) so that even when
/// every pane is hidden, the user still has a way to bring one
/// back. Each toggle reads from / writes back to [panes::PanelVis] on the
/// active StateUi.
fn panel_toggles(state: &mut State, ui: &mut Ui) {
    ui.horizontal(|ui| {
        let v = &mut state.ui.panel_vis;
        for pane in state.ui.pane_order {
            let r = ui.toggle_value(pane.visible_mut(v), pane.title());
            match pane {
                panes::Pane::Clipboard => {
                    r.on_hover_text("Toggle clipboard history (Ctrl+Shift+V)");
                }
                panes::Pane::Terminal => {
                    r.on_hover_text("Toggle the terminal (Alt+F12)");
                }
                _ => {}
            }
        }
        // Opens a window of its own rather than a pane, so it's set apart.
        ui.add_space(PATH_TOGGLE_GAP);
        ui.toggle_value(&mut v.path_editor, "PATH");
    });
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
        .on_hover_text("Increase or decrease font size. Drag, or click to type.");

    if response.changed() {
        val as f32 - size
    } else {
        0.
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
    if state.egui_ctx.is_none() {
        state.egui_ctx = Some(ui.ctx().clone());
    }
    // Take in what other apps copy, whether or not the panel is open.
    state.ui.clipboard.sys.watch(ui.ctx());
    clipboard::capture(state, ui.ctx());

    // Run the command submitted last frame. That frame already painted its
    // echo, so the user sees what they ran while a blocking built-in runs.
    if let Some(input) = state.ui.pending_cmd.take() {
        let ctx = ui.ctx().clone();
        run_command(state, &input, &ctx);
    }

    // Even while a full-screen program has the keyboard, so it can be hidden
    // from there too.
    terminal::toggle_terminal_key(state, ui.ctx());

    // After that command, which runs in the tab it was typed in, and before
    // any widget, so the input box never sees these keys. Not while a
    // program has the keyboard: Ctrl+W is vim's, and nano's, Ctrl+B is
    // tmux's and Claude Code's, and Alt + a digit is irssi's and weechat's.
    if !terminal::program_has_keys(state, ui.ctx()) {
        tab_keys::handle(state, ui.ctx());
        terminal::toggle_bookmark_key(state, ui.ctx());
        terminal::folder_keys(state, ui.ctx());
        terminal::focus_input_key(state, ui.ctx());
    }
    // Pressed in a file viewer window (see `viewer::FileViewers`).
    if state.file_viewers.take_bookmark_key() {
        let msg = state.toggle_bookmark_cwd();
        state.push_out(msg, OutType::StdOut);
    }

    // Bring in output from shell commands running in the background, and
    // wrap up any that have finished. Their reader threads request a repaint
    // whenever there's news.
    state.pump_running();
    // Likewise a Files-panel paste.
    state.pump_paste();
    state.pump_delete(ui.ctx());
    state.pump_zip_job();

    // SSH: finished connection attempts, and remote output. The connection's
    // own tasks request a repaint when there's news; this asks for another
    // frame only while output is still queued.
    if state.pump_remotes() {
        ui.ctx().request_repaint();
    }

    Panel::top("tabs_panel").resizable(false).show(ui, |ui| {
        shrink_panel_fonts(ui);
        tabs::tabs_bar(state, ui);
        panel_toggles(state, ui);
    });

    panes::draw(state, ui);

    // File viewer windows are child viewports: they only stay open while
    // the main window's pass keeps showing them.
    state.file_viewers.show(ui.ctx());

    // Likewise the PATH editor's window.
    path_panel::path_window(state, ui.ctx());

    about::show(state, ui.ctx());
    settings::show(state, ui.ctx());
    dirs::stale_dir_popup(state, ui.ctx());
    file_browser::delete_popup(state, ui.ctx());
    file_browser::paste_conflict_popup(state, ui.ctx());
    scratchpad::popups(state, ui.ctx());

    // A command was submitted this frame: paint its echo, then come straight
    // back to run it.
    if state.ui.pending_cmd.is_some() {
        ui.ctx().request_repaint();
    }

    // Persist on any change so the clipboard history and scratchpad survive
    // restart. (The layout and settings are saved by eframe; see
    // [crate::prefs].)
    let clipboard_changed = std::mem::take(&mut state.ui.clipboard.history.unsaved);
    let scratchpad_due = state.ui.scratchpad.save_due(ui.ctx());
    if clipboard_changed || scratchpad_due {
        let _ = state.save();
    }
}
