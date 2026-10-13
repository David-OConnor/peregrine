use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use egui::{
    Align, CentralPanel, Color32, DragValue, FontData, FontDefinitions, FontFamily, FontId, Frame,
    Key, Layout, Panel, RichText, ScrollArea, TextEdit, TextFormat, TextStyle, TextWrapMode, Ui,
    WidgetText,
    text::{CCursor, CCursorRange, LayoutJob},
};
use shell::SshMode;

/// Renders a single-line "Search..." box that fills the available width and
/// triggers `on_changed` when the buffer is edited. Pulled out so all three
/// sidebars look identical.
fn search_box(ui: &mut Ui, buf: &mut String, id_salt: &str) -> bool {
    ui.add(
        TextEdit::singleline(buf)
            .hint_text("Search...")
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

use crate::{
    OutType, ansi,
    cmd_color::CmdColors,
    deep_search::{DeepSearch, MAX_MATCHES, Row},
    files::{self, FileDetails, format_size, open_with_default_app},
    markdown::is_markdown,
    path_color::{self, Part},
    render_with_tilde, run_command,
    state::{FileRename, State},
    submit_command,
};

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
pub const MIN_FONT_SIZE: f32 = 6.;
pub const MAX_FONT_SIZE: f32 = 48.;

/// Default max displayed length (in characters) of a Bookmarks / Recent dirs
/// path, used when the state file has no saved `DIR_LABEL_LEN`. Longer paths
/// are truncated from the front (see [path_color::truncate_start]), e.g.
/// `.../code/Bio/dynamics`.
pub const DEFAULT_DIR_LABEL_LEN: usize = 22;
/// Bounds the path-length controls clamp to. The minimum leaves room for a
/// few characters after the `...`.
pub const MIN_DIR_LABEL_LEN: usize = 8;
pub const MAX_DIR_LABEL_LEN: usize = 200;

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

/// Bookmarks column. Narrower than the other side columns: bookmark paths are
/// truncated (to [DEFAULT_DIR_LABEL_LEN] chars unless the user changes it), so
/// this only needs to fit the `"<i>: "` prefix plus that path and the trailing
/// `x` button.
const BOOKMARK_COL_WIDTH: f32 = 170.;
/// Recent-dirs column. Paths are truncated like bookmarks; slightly wider to
/// fit the extra `*` (bookmarked) marker.
const RECENT_DIRS_COL_WIDTH: f32 = 175.;
const RIGHT_COL_WIDTH: f32 = 180.;
const CWD_HIST_COL_WIDTH: f32 = 220.;
const BROWSER_COL_WIDTH: f32 = 200.;
/// The Files list splits into two columns once the panel is wide enough for
/// each to fit this many monospace characters (so the threshold follows the
/// GUI font size). See [file_browser].
const FILES_COL_MIN_CHARS: f32 = 26.;
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
    /// Open a file with the OS's default associated program, the way a
    /// double-click does in Explorer / a Linux file manager.
    OpenFile(PathBuf),
    /// Select a Files-panel row, showing its details in the pane below.
    SelectFile(PathBuf),
    RemoveBookmark(usize),
    FillInput(String),
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

/// A Bookmarks / Recent dirs button label: `prefix` (the index, plus any
/// marker), then `path` as `~/...`, colour-coded by directory (see
/// [path_color]) with its final directory bold, and truncated from the front
/// to `max_len` chars so the column stays narrow.
fn dir_label(
    ui: &Ui,
    prefix: String,
    path: &Path,
    home: Option<&Path>,
    max_len: usize,
) -> LayoutJob {
    let dark = ui.visuals().dark_mode;
    let parts = path_color::parts(&render_with_tilde(path, home), Some("~"), dark);
    let mut label = vec![Part::plain(prefix)];
    label.extend(path_color::truncate_start(parts, max_len));
    let font = TextStyle::Button.resolve(ui.style());
    let bold = FontId::new(font.size, bold_family());
    path_color::layout_job(&label, font, Some(bold), Color32::PLACEHOLDER)
}

/// `path` as `~/...`, colour-coded by directory (see [path_color]) with its
/// final part bold, in the body font. Uncoloured runs take the label's own
/// text colour.
fn path_job(ui: &Ui, path: &Path, home: Option<&Path>) -> LayoutJob {
    let dark = ui.visuals().dark_mode;
    let parts = path_color::parts(&render_with_tilde(path, home), Some("~"), dark);
    let font = TextStyle::Body.resolve(ui.style());
    let bold = FontId::new(font.size, bold_family());
    path_color::layout_job(&parts, font, Some(bold), Color32::PLACEHOLDER)
}

/// A Command history / In this dir button label: `"<i>: "`, then the command
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

/// Display directory bookmarks as a clickable column. Clicking a row cd's
/// to that bookmark; the `x` removes it from the list. The search box
/// filters by case-insensitive substring against the full path.
fn dir_bookmarks(state: &mut State, ui: &mut Ui) {
    panel_header(ui, "Bookmarks", &mut state.ui.panel_vis.bookmarks);

    if ui.button("+ bookmark current dir").clicked() {
        let msg = state.toggle_bookmark_cwd();
        state.push_out(msg, OutType::StdOut);
    }

    if search_box(ui, &mut state.ui.bookmarks_search, "bookmarks_search") {
        state.refresh_bookmarks_filter();
    }
    ui.separator();

    let home = state.home.clone();
    let max_len = state.bookmark_label_len;
    let mut action: Option<ListAction> = None;

    ScrollArea::vertical()
        .id_salt("bookmarks_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for &i in &state.ui.bookmarks_filter {
                let Some(bm) = state.dir_bookmarks.get(i) else {
                    continue;
                };
                ui.horizontal(|ui| {
                    let label = dir_label(ui, format!("{i}: "), bm, home.as_deref(), max_len);
                    if ui.button(label).clicked() {
                        action = Some(ListAction::ChangeDir(bm.clone()));
                    }
                    if ui.small_button("x").clicked() {
                        action = Some(ListAction::RemoveBookmark(i));
                    }
                });
            }
        });

    apply_action(state, action);
}

/// Display recent directories we've executed a command from, as a clickable
/// column. Newest first. Stars mark dirs that are also bookmarked. The
/// search box filters by case-insensitive substring against the full path.
fn dir_history(state: &mut State, ui: &mut Ui) {
    panel_header(ui, "Recent dirs", &mut state.ui.panel_vis.recent_dirs);

    if search_box(ui, &mut state.ui.recent_dirs_search, "recent_dirs_search") {
        state.refresh_recent_dirs_filter();
    }
    ui.separator();

    let home = state.home.clone();
    let max_len = state.recent_dir_label_len;
    let mut action: Option<ListAction> = None;

    ScrollArea::vertical()
        .id_salt("recent_dirs_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // Render newest-first so the most-recently-used dirs are at the top.
            for &i in state.ui.recent_dirs_filter.iter().rev() {
                let Some(r) = state.recent_dirs.get(i) else {
                    continue;
                };
                let star = if state.dir_bookmarks.contains(&r.path) {
                    "*"
                } else {
                    ""
                };
                let label = dir_label(
                    ui,
                    format!("{i}: {star}"),
                    &r.path,
                    home.as_deref(),
                    max_len,
                );
                if ui.button(label).clicked() {
                    action = Some(ListAction::ChangeDir(r.path.clone()));
                }
            }
        });

    apply_action(state, action);
}

/// Display commands previously run in the active tab's cwd. Newest first.
/// Same click behavior as the global history panel.
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

            for &i in state.ui.cwd_history_filter.iter().rev() {
                let Some(item) = state.history.get(i) else {
                    continue;
                };

                let label = cmd_label(ui, &state.cmd_colors, i, &item.text);
                let btn = egui::Button::new(label)
                    .wrap()
                    .min_size(egui::vec2(ui.available_width(), 0.0));
                if ui.add(btn).clicked() {
                    action = Some(ListAction::FillInput(item.text.clone()));
                }
            }
        });

    apply_action(state, action);
}

/// Display recent commands entered, newest first. Clicking the label copies
/// the command into the input box so the user can edit or run it; the
/// "In dir" button re-runs it in the directory it was originally run from.
fn cmd_history(state: &mut State, ui: &mut Ui) {
    panel_header(ui, "Command history", &mut state.ui.panel_vis.recent_cmds);

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
            for &i in state.ui.history_filter.iter().rev() {
                let Some(item) = state.history.get(i) else {
                    continue;
                };
                let label = cmd_label(ui, &state.cmd_colors, i, &item.text);
                let btn = egui::Button::new(label)
                    .wrap()
                    .min_size(egui::vec2(ui.available_width(), 0.0));
                if ui.add(btn).clicked() {
                    action = Some(ListAction::FillInput(item.text.clone()));
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
            }
        });

    apply_action(state, action);
}

fn apply_action(state: &mut State, action: Option<ListAction>) {
    match action {
        Some(ListAction::ChangeDir(p)) => state.change_dir(p),
        Some(ListAction::OpenFile(p)) if is_markdown(&p) => state.md_viewers.open(p),
        Some(ListAction::OpenFile(p)) => {
            if let Err(e) = open_with_default_app(&p) {
                state.push_out(
                    format!("open: failed to open {}: {e}", p.display()),
                    OutType::StdErr,
                );
            }
        }
        Some(ListAction::SelectFile(p)) => state.select_file(&p),
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
        Some(ListAction::FillInput(s)) => {
            let i = state.ui.active_tab;
            state.ui.cli_input[i] = s;
            state.ui.focus_input = true;
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
    ScrollArea::vertical()
        .id_salt("term_out_scroll")
        .auto_shrink([false, false])
        .stick_to_bottom(true)
        .show(ui, |ui| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
            set_mono_size(ui, state.font_size);
            let mono_font = egui::TextStyle::Monospace.resolve(ui.style());
            let active = state.ui.active_tab;
            for item in &state.ui.out[active] {
                let default_color = match item.type_ {
                    OutType::Prompt => COLOR_PROMPT,
                    OutType::StdOut => COLOR_STDOUT,
                    OutType::StdErr => COLOR_STDERR,
                    OutType::StdErrOk => COLOR_STDERR_OK,
                };
                // Trim a trailing newline so blank rows don't accumulate
                // (Command output usually ends with one).
                let text = item.text.trim_end_matches('\n');
                let segments = ansi::parse(text);
                let job = ansi::to_layout_job(&segments, default_color, mono_font.clone());
                ui.label(job);
            }
        });
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
    ui.horizontal(|ui| {
        let mono = TextStyle::Monospace.resolve(ui.style());
        ui.label(path_color::layout_job(&cwd_parts, mono, None, COLOR_PROMPT));
        if let Some(b) = branch {
            if show_branch {
                ui.label(
                    RichText::new(format!("branch: {}", shell::truncate_branch(&b, 10)))
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
        ui.label(RichText::new("$").color(COLOR_PROMPT).monospace());
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

    // Input box sits on its own row, directly beneath the prompt line above.
    // It's multi-line so a long command wraps onto extra rows (and the panel
    // grows to fit) instead of scrolling off to the right, but it's still one
    // logical line: Enter never inserts a newline (`return_key(None)`), and
    // pasted newlines are flattened to spaces below. `lock_focus` stops egui
    // from using Tab for widget-to-widget focus navigation, since we use it
    // for path autocompletion.
    let response = ui.add(
        TextEdit::multiline(&mut state.ui.cli_input[active])
            .id(input_id)
            .desired_rows(1)
            .desired_width(f32::INFINITY)
            .return_key(None)
            .lock_focus(true)
            .font(egui::TextStyle::Monospace),
    );

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

    if tab {
        state.autocomplete_input();
        // egui's TextEdit keeps its own caret position, which isn't
        // updated when we replace the buffer behind its back. Move the
        // caret to the end so it follows the completed path.
        if let Some(mut edit_state) = TextEdit::load_state(ui.ctx(), response.id) {
            let end = egui::text::CCursor::new(state.ui.cli_input[active].chars().count());
            edit_state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::one(end)));
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
        } else {
            submit_command(state, &input);
        }
        state.ui.focus_input = true;
    }
}

/// Files in the active tab's cwd, as a selectable list. Folders are
/// sorted first and prefixed with 📁; executable files (Unix +x, or
/// Windows `.exe`/`.msi`) are coloured green. The listing is refreshed
/// by [State::refresh_browser_files] on every cwd change and after every
/// command, so we just read what's already there. The search box under the
/// hidden/dotfile toggles narrows the listing by name. When the panel is wide
/// enough (see [FILES_COL_MIN_CHARS]) the list is laid out in two columns.
///
/// Clicking a row selects it, and the bottom third of the panel shows its
/// details (see [file_details]). Double-clicking a folder row navigates into
/// it (same path as clicking a bookmark or recent dir); double-clicking a
/// Markdown file opens it in a viewer window (see `crate::markdown`); any
/// other file opens with the OS's default associated program (see
/// [open_with_default_app]). Clicking the already-selected row again renames
/// it in place (see [rename_box]): Enter or clicking away applies the new
/// name, Escape cancels.
fn file_browser(state: &mut State, ui: &mut Ui) {
    // Measured before anything is laid out, so the details pane is a third
    // of the whole panel rather than of what's left under the header.
    let details_height = ui.available_height() / 3.0;
    // The whole pane, for the mouse Back/Forward buttons below.
    let pane_rect = ui.max_rect();

    panel_header(ui, "Files", &mut state.ui.panel_vis.file_browser);

    // The directory being listed, colour-coded like the other panels' paths.
    // Wraps rather than widening the panel when it's long.
    let cwd_job = path_job(ui, state.cwd(), state.home.as_deref());
    ui.add(egui::Label::new(cwd_job).wrap());

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
    let home = state.home.clone();
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
            .add_enabled(can_up, egui::Button::new("⬆"))
            .on_hover_text("Parent directory")
            .clicked()
        {
            go_up = true;
        }
        if ui
            .add_enabled(can_back, egui::Button::new("⬅"))
            .on_hover_text("Previous directory")
            .clicked()
        {
            go_back = true;
        }
        if ui
            .add_enabled(can_forward, egui::Button::new("➡"))
            .on_hover_text("Next directory")
            .clicked()
        {
            go_forward = true;
        }
        for s in shortcuts {
            let ok = s.dir.as_ref().is_some_and(|d| d.is_dir());
            if ui
                .add_enabled(ok, egui::Button::new(s.glyph))
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
    // only the dotfile filter appears. The Open button after them does what
    // the `open` command does: shows this dir in the OS's file manager.
    let mut filters_changed = false;
    let mut open_clicked = false;
    ui.horizontal(|ui| {
        let mut toggle = |ui: &mut Ui, on: &mut bool, label: &str, what: &str| {
            let hover = if *on {
                format!("Showing {what}. Click to hide them.")
            } else {
                format!("Hiding {what}. Click to show them.")
            };
            if ui
                .add(egui::Button::selectable(*on, label))
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
        if ui
            .button("📂 Native browser")
            .on_hover_text("Open this folder in the system file manager")
            .clicked()
        {
            open_clicked = true;
        }
    });
    if filters_changed {
        state.refresh_browser_files();
    }
    if open_clicked {
        let cwd = state.cwd().to_path_buf();
        shell::commands::open_dir(&cwd, &mut |kind, msg| {
            state.push_out(msg, crate::out_type_for(kind));
        });
    }

    // Text filter on names in this dir; cleared on cd (see
    // [State::refresh_browser_files]).
    if search_box(ui, &mut state.ui.files_search, "files_search") {
        state.refresh_browser_files_filter();
    }
    deep_search_bar(state, ui);

    ui.separator();

    // Added before the list so it claims the bottom of the panel; the list
    // then fills the space left above it.
    Panel::bottom("file_details_panel")
        .resizable(false)
        .exact_size(details_height)
        .frame(Frame::new().inner_margin(egui::Margin::symmetric(0, 4)))
        .show_inside(ui, |ui| {
            file_details(state.selected_file.as_ref(), state.home.as_deref(), ui);
        });

    // While a Deep search is set, its results stand in for the listing.
    if let Some(search) = &state.deep_search {
        let selected = state.selected_file.as_ref().map(|d| d.path.as_path());
        let action = deep_search_results(search, &state.ui.files_search, selected, ui);
        apply_action(state, action);
        return;
    }

    // Explorer-style rename: a click on the already-selected row starts one,
    // once it's clear the click isn't the first half of a double-click. Any
    // other press in the meantime calls it off.
    let (now, pressed) = ui.input(|i| (i.time, i.pointer.any_pressed()));
    let dbl_click_delay = ui
        .ctx()
        .options(|o| o.input_options.max_double_click_delay);
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

    ScrollArea::vertical()
        .id_salt("file_browser_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // Rows butt up against each other, like a file manager's list.
            ui.spacing_mut().item_spacing.y = 0.0;

            if state.ui.browser_files_filter.is_empty() && !state.ui.files_search.is_empty() {
                ui.label(RichText::new("No matches.").italics().weak());
            }
            let mut row = |ui: &mut Ui, i: usize| {
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
                let mut rich = RichText::new(text).monospace();
                if f.is_folder {
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
                let row = egui::Button::selectable(selected == Some(f.path.as_path()), rich)
                    .truncate()
                    .min_size(egui::vec2(ui.available_width(), 0.0));
                let resp = ui.add(row);
                // A double-click's first click has already selected the row.
                // Then a folder navigates into it; a file opens with its
                // associated program, just like a desktop file manager.
                if resp.double_clicked() {
                    action = Some(if f.is_folder {
                        ListAction::ChangeDir(f.path.clone())
                    } else {
                        ListAction::OpenFile(f.path.clone())
                    });
                } else if resp.clicked() {
                    if selected == Some(f.path.as_path()) {
                        clicked_selected = Some(f.path.clone());
                    } else {
                        action = Some(ListAction::SelectFile(f.path.clone()));
                    }
                }
            };

            // Two columns once each fits FILES_COL_MIN_CHARS characters.
            let font = TextStyle::Monospace.resolve(ui.style());
            let char_width = ui.ctx().fonts_mut(|f| f.glyph_width(&font, '0'));
            let col_gap = ui.spacing().item_spacing.x;
            let two_cols = ui.available_width() >= 2.0 * FILES_COL_MIN_CHARS * char_width + col_gap;
            let rows = &state.ui.browser_files_filter;
            if two_cols {
                // Filled down the left column first, then the right, like
                // `ls`, so the folders stay together at the top left.
                let (left, right) = rows.split_at(rows.len().div_ceil(2));
                ui.columns(2, |cols| {
                    for &i in left {
                        row(&mut cols[0], i);
                    }
                    for &i in right {
                        row(&mut cols[1], i);
                    }
                });
            } else {
                for &i in rows {
                    row(ui, i);
                }
            }
        });

    match rename_done {
        Some(true) => state.commit_rename(),
        Some(false) => state.ui.rename = None,
        None => {}
    }
    if let Some(path) = clicked_selected {
        state.ui.rename_pending = Some((path, now));
        ui.ctx().request_repaint_after_secs(dbl_click_delay as f32);
    }
    apply_action(state, action);
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

/// The Files panel's Deep search box and its regex toggle (see
/// `crate::deep_search`). A search starts once typing pauses for
/// [DEEP_SEARCH_DELAY], or at once on Enter, which also re-runs an unchanged
/// one; clearing the box ends it. Also takes in the running search's latest
/// results.
fn deep_search_bar(state: &mut State, ui: &mut Ui) {
    let now = ui.input(|i| i.time);
    let mut run_now = false;
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let regex = &mut state.ui.deep_search_regex;
            if ui
                .add(egui::Button::selectable(*regex, ".*"))
                .on_hover_text("Treat the Deep search as a regular expression, not plain text")
                .clicked()
            {
                *regex = !*regex;
                run_now = true;
            }
            let resp = ui
                .add(
                    TextEdit::singleline(&mut state.ui.deep_search)
                        .hint_text("Deep search...")
                        .desired_width(f32::INFINITY)
                        .id_salt("deep_search"),
                )
                .on_hover_text(
                    "Search the contents of every file in this folder and its subfolders, \
                     as ripgrep does: hidden and .gitignored files are skipped. \
                     Case-insensitive unless the search has a capital letter.",
                );
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

/// The Files list while a Deep search is set: a status line, then each file
/// with matches (its path relative to the searched dir, and how many) above
/// its matching lines, numbered, with the matches highlighted. `name_filter`
/// (the panel's Search box) narrows the files by that path. Clicking a row
/// selects its file; double-clicking opens it, as in the listing.
fn deep_search_results(
    search: &DeepSearch,
    name_filter: &str,
    selected: Option<&Path>,
    ui: &mut Ui,
) -> Option<ListAction> {
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

    let counts = format!(
        "{} in {}",
        plural(search.n_matches, "match", "matches"),
        plural(search.hits.len(), "file", "files")
    );
    ui.horizontal_wrapped(|ui| {
        let status = if !search.done() {
            ui.spinner();
            format!("Searching... {counts}")
        } else if search.hits.is_empty() {
            "No matches.".to_owned()
        } else if search.n_matches >= MAX_MATCHES {
            format!("{counts}. Stopped at {MAX_MATCHES} matches.")
        } else {
            counts
        };
        ui.label(RichText::new(status).italics().weak());
    });

    // Every row is one line of monospace text, so only the visible ones need
    // laying out, however many matches there are.
    let font = TextStyle::Monospace.resolve(ui.style());
    let row_height = ui
        .spacing()
        .interact_size
        .y
        .max(ui.text_style_height(&TextStyle::Monospace) + 2.0 * ui.spacing().button_padding.y);
    let visuals = ui.visuals();
    let (weak, hl_text, hl_back) = (
        visuals.weak_text_color(),
        visuals.selection.stroke.color,
        visuals.selection.bg_fill,
    );
    let mut action = None;

    ui.scope(|ui| {
        // Rows butt up against each other, like the listing's.
        ui.spacing_mut().item_spacing.y = 0.0;
        ScrollArea::vertical()
            .id_salt("deep_search_scroll")
            .auto_shrink([false, false])
            .show_rows(ui, row_height, rows.len(), |ui, range| {
                for &row in &rows[range] {
                    let (Row::File(i) | Row::Line(i, _)) = row;
                    let hit = &search.hits[i];
                    let text: WidgetText = match row {
                        Row::File(_) => {
                            let text = format!("{} ({})", rel(&hit.path), hit.lines.len());
                            RichText::new(text).monospace().strong().into()
                        }
                        Row::Line(_, j) => {
                            let m = &hit.lines[j];
                            // Line numbers right-aligned within the file.
                            let width =
                                hit.lines.last().map_or(1, |l| l.line_num.to_string().len());
                            let plain = TextFormat {
                                font_id: font.clone(),
                                color: Color32::PLACEHOLDER,
                                ..Default::default()
                            };
                            let mut job = LayoutJob::default();
                            job.append(
                                &format!("  {:>width$}: ", m.line_num),
                                0.0,
                                TextFormat {
                                    color: weak,
                                    ..plain.clone()
                                },
                            );
                            let mut at = 0;
                            for r in &m.ranges {
                                job.append(&m.text[at..r.start], 0.0, plain.clone());
                                let hl = TextFormat {
                                    color: hl_text,
                                    background: hl_back,
                                    ..plain.clone()
                                };
                                job.append(&m.text[r.clone()], 0.0, hl);
                                at = r.end;
                            }
                            job.append(&m.text[at..], 0.0, plain);
                            job.into()
                        }
                    };
                    let is_selected = matches!(row, Row::File(_)) && selected == Some(&hit.path);
                    let button = egui::Button::selectable(is_selected, text)
                        .truncate()
                        .min_size(egui::vec2(ui.available_width(), row_height));
                    let resp = ui.add(button);
                    if resp.double_clicked() {
                        action = Some(ListAction::OpenFile(hit.path.clone()));
                    } else if resp.clicked() {
                        action = Some(ListAction::SelectFile(hit.path.clone()));
                    }
                }
            });
    });
    action
}

/// The Files panel's lower pane: type, size, dates and attributes of the
/// selected row, read when it was selected (see [FileDetails::read]).
fn file_details(details: Option<&FileDetails>, home: Option<&Path>, ui: &mut Ui) {
    let Some(d) = details else {
        ui.label(
            RichText::new("Select a file to see its details.")
                .italics()
                .weak(),
        );
        return;
    };

    // One "Label  value" line per field, wrapping when the value is long.
    let field = |ui: &mut Ui, label: &str, value: WidgetText| {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(label).weak());
            ui.label(value);
        });
    };
    let time = |t: SystemTime| {
        chrono::DateTime::<chrono::Local>::from(t)
            .format("%Y-%m-%d %H:%M")
            .to_string()
    };

    ScrollArea::vertical()
        .id_salt("file_details_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.style_mut().wrap_mode = Some(TextWrapMode::Wrap);

            ui.label(RichText::new(&d.name).strong());
            field(ui, "Type", d.type_name.clone().into());
            if let Some(size) = d.size {
                field(ui, "Size", format_size(size).into());
            }
            if let Some(n) = d.item_count {
                let s = if n == 1 { "" } else { "s" };
                field(ui, "Contains", format!("{n} item{s}").into());
            }
            if let Some(t) = d.modified {
                field(ui, "Modified", time(t).into());
            }
            if let Some(t) = d.created {
                field(ui, "Created", time(t).into());
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
                field(ui, "Attributes", attrs.join(", ").into());
            }
            if let Some(target) = &d.link_target {
                field(ui, "Link to", path_job(ui, target, home).into());
            }
            field(ui, "Path", path_job(ui, &d.path, home).into());
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
        ui.toggle_value(&mut v.bookmarks, "Bookmarks");
        ui.toggle_value(&mut v.recent_dirs, "Recent dirs");
        ui.toggle_value(&mut v.file_browser, "Files");
        ui.toggle_value(&mut v.remote_terminals, "Remote");
        ui.toggle_value(&mut v.recent_cmds, "Cmd history");
        ui.toggle_value(&mut v.recent_cmds_in_dir, "Cmds this dir");
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
}

/// A `<label> N + -` font-size control for the tab strip: the label, the
/// current size, then buttons to step it. Returns the step asked for (+1, -1,
/// or 0); `what` names the text it scales, for the tooltips.
fn font_size_control(ui: &mut Ui, label: &str, size: f32, what: &str) -> f32 {
    ui.label(label);
    ui.label(format!("{}", size.round() as i32));
    let mut delta = 0.0;

    if ui
        .small_button("-")
        .on_hover_text(format!("Decrease {what} font size"))
        .clicked()
    {
        delta = -1.0;
    }

    if ui
        .small_button("+")
        .on_hover_text(format!("Increase {what} font size"))
        .clicked()
    {
        delta = 1.0;
    }

    delta
}

/// A `<label> N + -` control for a Bookmarks / Recent dirs path length, like
/// [font_size_control] except `N` can also be dragged or clicked to type a
/// value. Updates `len` live, clamped to [MIN_DIR_LABEL_LEN]..=
/// [MAX_DIR_LABEL_LEN]. Returns true once a change is complete (a button
/// click, a finished drag, or a committed edit), so the caller can persist it
/// without saving on every frame of a drag. `what` names the panel's paths,
/// for the tooltips.
fn dir_label_len_control(ui: &mut Ui, label: &str, len: &mut usize, what: &str) -> bool {
    ui.label(label);

    let resp = ui
        .add(
            DragValue::new(len)
                .range(MIN_DIR_LABEL_LEN..=MAX_DIR_LABEL_LEN)
                .speed(0.25)
                .update_while_editing(false),
        )
        .on_hover_text(format!(
            "Max characters of {what} paths to show; longer ones are cut from the \
             front. Drag, or click to type."
        ));

    let mut done = (resp.changed() && !resp.dragged()) || resp.drag_stopped();

    if ui
        .small_button("-")
        .on_hover_text(format!("Show less of {what} paths"))
        .clicked()
        && *len > MIN_DIR_LABEL_LEN
    {
        *len -= 1;
        done = true;
    }

    if ui
        .small_button("+")
        .on_hover_text(format!("Show more of {what} paths"))
        .clicked()
        && *len < MAX_DIR_LABEL_LEN
    {
        *len += 1;
        done = true;
    }

    done
}

/// Top-of-window tab strip. Each tab labels itself with its cwd's leaf
/// folder; clicking selects, `x` closes (disabled when only one tab is
/// left), and `+` appends a fresh tab inheriting the active tab's cwd.
fn tabs_bar(state: &mut State, ui: &mut Ui) {
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
    let mut bookmark_label_len = state.bookmark_label_len;
    let mut recent_dir_label_len = state.recent_dir_label_len;

    let mut to_select: Option<usize> = None;
    let mut to_close: Option<usize> = None;
    let mut add_new = false;
    let mut connect_remote: Option<usize> = None;
    let mut font_delta = 0.0_f32;
    let mut gui_font_delta = 0.0_f32;
    let mut label_len_done = false;

    ui.horizontal(|ui| {
        // Font-size controls sit to the left of the tab strip: one for the
        // terminal panes, one for the side panels. Then the path lengths
        // for the Bookmarks and Recent dirs panels, which resize separately.
        font_delta = font_size_control(ui, "CLI font size: ", font_size, "terminal");
        ui.separator();
        gui_font_delta = font_size_control(ui, "GUI font size: ", gui_font_size, "panel");
        ui.separator();
        label_len_done |=
            dir_label_len_control(ui, "Bookmark chars: ", &mut bookmark_label_len, "bookmark");
        ui.separator();
        label_len_done |= dir_label_len_control(
            ui,
            "Recent dir chars: ",
            &mut recent_dir_label_len,
            "recent dir",
        );
        ui.separator();
        // Breathing room either side of the tab section.
        ui.add_space(20.0);

        for (i, leaf) in labels.iter().enumerate() {
            let text = format!("{i}: {leaf}");
            // Tighter spacing within a tab so its `x` hugs the name.
            ui.scope(|ui| {
                ui.spacing_mut().item_spacing.x = 1.0;
                let tab = ui.selectable_label(i == active, text);
                if tab.clicked() {
                    to_select = Some(i);
                }
                // Middle-click closes, same as the `x`.
                if n > 1 && tab.middle_clicked() {
                    to_close = Some(i);
                }
                if n > 1 && ui.small_button("x").on_hover_text("Close tab").clicked() {
                    to_close = Some(i);
                }
            });
            ui.separator();
        }
        if ui.button("+ tab").on_hover_text("New tab").clicked() {
            add_new = true;
        }
        ui.add_space(20.0);
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

    if font_delta != 0.0 {
        state.adjust_font_size(font_delta);
    }
    if gui_font_delta != 0.0 {
        state.adjust_gui_font_size(gui_font_delta);
    }
    // Applied every frame so the panels follow a drag live; saved once done.
    state.bookmark_label_len = bookmark_label_len;
    state.recent_dir_label_len = recent_dir_label_len;
    if label_len_done {
        let _ = state.save();
    }
    if let Some(i) = to_select {
        state.select_tab(i);
    }
    if let Some(i) = to_close {
        state.close_tab(i);
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
    // Snapshot panel visibility so a change from the "Panels:" toggles or a
    // panel's own close button is persisted at the end of the frame.
    let vis_before = state.ui.panel_vis;

    // Run the command submitted last frame. That frame already painted its
    // echo, so the user sees what they ran while this blocks.
    if let Some(input) = state.ui.pending_cmd.take() {
        run_command(state, &input);
    }

    // Pump any live PTY output into the active tab's pane. While a PTY session
    // is active, keep requesting repaints so streamed output (e.g. `top`) keeps
    // flowing even when the user isn't interacting.
    if state.pump_pty() {
        ui.ctx().request_repaint();
    }

    Panel::top("tabs_panel")
        .resizable(false)
        .show_inside(ui, |ui| {
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
            .default_size(prev.bookmarks.unwrap_or(BOOKMARK_COL_WIDTH))
            .max_size(max)
            .show_inside(ui, |ui| {
                scale_panel_fonts(ui, gui_font_size);
                dir_bookmarks(state, ui);
            });
        state.ui.panel_widths.bookmarks = Some(r.response.rect.width());
    }

    if state.ui.panel_vis.recent_dirs {
        let max = central_reserve(ui, f32::INFINITY);
        let r = Panel::left("recent_dirs_panel")
            .resizable(true)
            .default_size(prev.recent_dirs.unwrap_or(RECENT_DIRS_COL_WIDTH))
            .max_size(max)
            .show_inside(ui, |ui| {
                scale_panel_fonts(ui, gui_font_size);
                dir_history(state, ui);
            });
        state.ui.panel_widths.recent_dirs = Some(r.response.rect.width());
    }

    if state.ui.panel_vis.file_browser {
        let max = central_reserve(ui, f32::INFINITY);
        let r = Panel::left("file_browser_panel")
            .resizable(true)
            .default_size(prev.file_browser.unwrap_or(BROWSER_COL_WIDTH))
            .max_size(max)
            .show_inside(ui, |ui| {
                scale_panel_fonts(ui, gui_font_size);
                file_browser(state, ui);
            });
        state.ui.panel_widths.file_browser = Some(r.response.rect.width());
    }

    if state.ui.panel_vis.remote_terminals {
        let max = central_reserve(ui, f32::INFINITY);
        let r = Panel::left("remote_terminals_panel")
            .resizable(true)
            .default_size(prev.remote_terminals.unwrap_or(REMOTE_COL_WIDTH))
            .max_size(max)
            .show_inside(ui, |ui| {
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
            .show_inside(ui, |ui| {
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
            .show_inside(ui, |ui| {
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
        .show_inside(ui, |ui| {
            term_in(state, ui);
        });

    CentralPanel::default()
        .frame(term_frame)
        .show_inside(ui, |ui| {
            term_out(state, ui);
        });

    // Markdown viewer windows are child viewports: they only stay open while
    // the main window's pass keeps showing them.
    state.md_viewers.show(ui.ctx());

    // A command was submitted this frame: paint its echo, then come straight
    // back to run it.
    if state.ui.pending_cmd.is_some() {
        ui.ctx().request_repaint();
    }

    // Persist on any change so the layout survives restart.
    let drag_done = state.ui.panel_widths_unsaved && !pointer_down;
    if drag_done || !panel_vis_eq(&vis_before, &state.ui.panel_vis) {
        state.ui.panel_widths_unsaved = false;
        let _ = state.save();
    }
}
