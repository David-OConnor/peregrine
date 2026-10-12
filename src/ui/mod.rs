use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

use egui::{
    Align, CentralPanel, Color32, Frame, Key, Layout, Panel, RichText, ScrollArea, TextEdit,
    TextWrapMode, Ui,
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
    files::{FileDetails, format_size, open_with_default_app},
    markdown::is_markdown,
    render_with_tilde, run_command,
    state::State,
    submit_command,
};

pub const WINDOW_WIDTH: f32 = 1400.;
pub const WINDOW_HEIGHT: f32 = 900.;

/// Default font size (points) for the terminal input/output panes, used when
/// the state file has no saved `FONT_SIZE`. Matches egui's default monospace
/// size so a fresh install looks unchanged.
pub const DEFAULT_FONT_SIZE: f32 = 13.;
/// Bounds the +/- font-size buttons clamp to, so the panes can't be shrunk
/// to an unreadable size or blown up past the window.
pub const MIN_FONT_SIZE: f32 = 6.;
pub const MAX_FONT_SIZE: f32 = 48.;

/// Bookmarks column. Narrower than the other side columns: bookmark paths are
/// truncated to [BOOKMARK_MAX_LEN] chars, so this only needs to fit the
/// `"<i>: "` prefix plus that path and the trailing `x` button.
const BOOKMARK_COL_WIDTH: f32 = 170.;
/// Recent-dirs column. Paths are truncated to [BOOKMARK_MAX_LEN] chars like
/// bookmarks; slightly wider to fit the extra `*` (bookmarked) marker.
const RECENT_DIRS_COL_WIDTH: f32 = 175.;
const RIGHT_COL_WIDTH: f32 = 180.;
const CWD_HIST_COL_WIDTH: f32 = 220.;
const BROWSER_COL_WIDTH: f32 = 200.;
const REMOTE_COL_WIDTH: f32 = 180.;

/// The main terminal output/input column must always keep at least this many
/// points of width, no matter how wide the user drags the side panels (or how
/// narrow they make the window). See [central_reserve].
pub const MIN_CENTRAL_WIDTH: f32 = 400.;

/// Points shaved off every text style in the side panels (and the top tab
/// strip), so they read slightly smaller than the main terminal column.
/// See [shrink_panel_fonts].
const PANEL_FONT_REDUCTION: f32 = 2.;

const COLOR_PROMPT: Color32 = Color32::from_rgb(220, 220, 90);
const COLOR_STDOUT: Color32 = Color32::from_rgb(210, 210, 210);
const COLOR_STDERR: Color32 = Color32::from_rgb(240, 110, 110);
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

/// Max displayed length (in characters) of a bookmark's path. Longer paths
/// are truncated from the front by [truncate_start] so the column stays narrow.
const BOOKMARK_MAX_LEN: usize = 22;

/// Truncate `s` to at most `max` characters for display in a narrow column,
/// keeping the tail (the most specific part of a path) and prefixing `...`
/// when shortened, e.g. `.../code/Bio/dynamics`. Counts by `char` so the
/// result is never longer than `max` even with multi-byte characters.
fn truncate_start(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    // Reserve 3 chars for the leading ellipsis.
    let keep = max.saturating_sub(3);
    let tail: String = s.chars().skip(count - keep).collect();
    format!("...{tail}")
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
                    let path = render_with_tilde(bm, home.as_deref());
                    let label = format!("{i}: {}", truncate_start(&path, BOOKMARK_MAX_LEN));
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
                let path = render_with_tilde(&r.path, home.as_deref());
                let label = format!("{i}: {star}{}", truncate_start(&path, BOOKMARK_MAX_LEN));
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
        "In this dir",
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

                let label = format!("{i}: {}", item.text);
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
            // expand horizontally — the panel itself is capped at 200px
            // wide, so long command lines need to break onto extra rows.
            ui.style_mut().wrap_mode = Some(TextWrapMode::Wrap);

            // Newest first.
            for &i in state.ui.history_filter.iter().rev() {
                let Some(item) = state.history.get(i) else {
                    continue;
                };
                let label = format!("{i}: {}", item.text);
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

/// The input row: prompt label on the left, single-line edit on the right.
/// Enter submits. When the user is recalling a history entry via Up/Down or
/// a recent dir via Left/Right, a green ` his N` / ` cd N` label is
/// inserted between the cwd and the `$`.
fn term_in(state: &mut State, ui: &mut Ui) {
    // Scale the prompt labels and input box with the chosen font size. Both
    // resolve `Monospace`, so overriding it on this panel's `ui` is enough.
    set_mono_size(ui, state.font_size);
    let active = state.ui.active_tab;
    let (his_cursor, cd_cursor) = state.active_nav_cursors();
    let cwd_part = state.cwd_display();
    let branch = state.branch[active].clone();
    // While remote, the cwd label already shows `user@host`; don't also show
    // the (stale) local git branch.
    let show_branch = !state.is_remote();
    ui.horizontal(|ui| {
        ui.label(RichText::new(cwd_part).color(COLOR_PROMPT).monospace());
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

    // Input box sits on its own row, directly beneath the prompt line above.
    let response = ui.add(
        TextEdit::singleline(&mut state.ui.cli_input[active])
            .desired_width(f32::INFINITY)
            .font(egui::TextStyle::Monospace),
    );

    if state.ui.focus_input {
        response.request_focus();
        state.ui.focus_input = false;
    }

    // Stop egui from stealing Tab for widget-to-widget focus navigation:
    // we use Tab ourselves for path autocompletion below. Locking the
    // filter tells egui this widget consumes Tab, so focus stays in the
    // input instead of jumping to the next button.
    if response.has_focus() {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                response.id,
                egui::EventFilter {
                    tab: true,
                    ..Default::default()
                },
            );
        });
    }

    // Arrow-key recall: ↑/↓ walks the global history list, ←/→ walks
    // recent dirs. Up/Down are safe to consume unconditionally (the
    // single-line TextEdit doesn't use them), but Left/Right only
    // steal the keystroke when the buffer is empty or a cd recall is
    // already active — otherwise they stay as normal caret movement.
    if response.has_focus() || response.lost_focus() {
        let tab = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Tab));
        let up = !tab && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::ArrowUp));
        let down = !tab && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::ArrowDown));
        if tab {
            state.autocomplete_input();
            response.request_focus();
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
        } else {
            let cd_active = cd_cursor.is_some() || state.ui.cli_input[active].is_empty();
            if cd_active {
                let left = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::ArrowLeft));
                let right = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::ArrowRight));
                if left {
                    state.recent_dir_nav(true);
                } else if right {
                    state.recent_dir_nav(false);
                }
            }
        }
    }

    let submitted = response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
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
/// command, so we just read what's already there.
///
/// Clicking a row selects it, and the bottom third of the panel shows its
/// details (see [file_details]). Double-clicking a folder row navigates into
/// it (same path as clicking a bookmark or recent dir); double-clicking a
/// Markdown file opens it in a viewer window (see `crate::markdown`); any
/// other file opens with the OS's default associated program (see
/// [open_with_default_app]).
fn file_browser(state: &mut State, ui: &mut Ui) {
    // Measured before anything is laid out, so the details pane is a third
    // of the whole panel rather than of what's left under the header.
    let details_height = ui.available_height() / 3.0;

    panel_header(ui, "Files", &mut state.ui.panel_vis.file_browser);

    // Browser-style navigation row. Up goes to the parent dir; Back/Forward
    // walk the per-tab history maintained in `State::change_dir`. Buttons are
    // disabled when there's nowhere to go in that direction.
    let i = state.ui.active_tab;
    let can_up = state.cwd().parent().is_some();
    let can_back = !state.ui.nav_back[i].is_empty();
    let can_forward = !state.ui.nav_forward[i].is_empty();
    // Common-destination shortcuts. Home comes straight from `state.home`;
    // Desktop/Downloads are the conventional sub-dirs that exist on both
    // Windows and Linux. Each is only enabled when the target actually
    // exists, so we never `cd` into a missing dir and push a diagnostic.
    let home = state.home.clone();
    let desktop = home.as_ref().map(|h| h.join("Desktop"));
    let downloads = home.as_ref().map(|h| h.join("Downloads"));
    let mut go_up = false;
    let mut go_back = false;
    let mut go_forward = false;
    let mut go_to: Option<PathBuf> = None;
    ui.horizontal(|ui| {
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
        // Glyph buttons: 🏠 Home, 🖥 Desktop, 📥 Downloads.
        let home_ok = home.as_ref().is_some_and(|h| h.is_dir());
        if ui
            .add_enabled(home_ok, egui::Button::new("🏠"))
            .on_hover_text("Home directory")
            .clicked()
        {
            go_to = home.clone();
        }
        let desktop_ok = desktop.as_ref().is_some_and(|d| d.is_dir());
        if ui
            .add_enabled(desktop_ok, egui::Button::new("🖥"))
            .on_hover_text("Desktop")
            .clicked()
        {
            go_to = desktop.clone();
        }
        let downloads_ok = downloads.as_ref().is_some_and(|d| d.is_dir());
        if ui
            .add_enabled(downloads_ok, egui::Button::new("📥"))
            .on_hover_text("Downloads")
            .clicked()
        {
            go_to = downloads.clone();
        }
    });
    if go_up {
        state.nav_up();
    } else if go_back {
        state.nav_back();
    } else if go_forward {
        state.nav_forward();
    } else if let Some(target) = go_to {
        state.change_dir(target);
    }

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

    let selected = state.selected_file.as_ref().map(|d| d.path.as_path());
    let mut action: Option<ListAction> = None;

    ScrollArea::vertical()
        .id_salt("file_browser_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // Rows butt up against each other, like a file manager's list.
            ui.spacing_mut().item_spacing.y = 0.0;

            for f in &state.browser_files {
                let text = if f.is_folder {
                    format!("📁 {}", f.disp_name)
                } else {
                    f.disp_name.clone()
                };
                let mut rich = RichText::new(text).monospace();
                if !f.is_folder && f.is_executable {
                    rich = rich.color(COLOR_EXECUTABLE);
                }
                // Full width so the whole row is clickable and the selection
                // highlight spans the panel. Long names are truncated; the
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
                    action = Some(ListAction::SelectFile(f.path.clone()));
                }
            }
        });

    apply_action(state, action);
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
    let field = |ui: &mut Ui, label: &str, value: String| {
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
            field(ui, "Type", d.type_name.clone());
            if let Some(size) = d.size {
                field(ui, "Size", format_size(size));
            }
            if let Some(n) = d.item_count {
                let s = if n == 1 { "" } else { "s" };
                field(ui, "Contains", format!("{n} item{s}"));
            }
            if let Some(t) = d.modified {
                field(ui, "Modified", time(t));
            }
            if let Some(t) = d.created {
                field(ui, "Created", time(t));
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
                field(ui, "Attributes", attrs.join(", "));
            }
            if let Some(target) = &d.link_target {
                field(ui, "Link to", render_with_tilde(target, home));
            }
            field(ui, "Path", render_with_tilde(&d.path, home));
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
        ui.label("Panels:");
        let v = &mut state.ui.panel_vis;
        ui.toggle_value(&mut v.bookmarks, "Bookmarks");
        ui.toggle_value(&mut v.recent_dirs, "Recent dirs");
        ui.toggle_value(&mut v.file_browser, "Files");
        ui.toggle_value(&mut v.remote_terminals, "Remote");
        ui.toggle_value(&mut v.recent_cmds, "Cmd history");
        ui.toggle_value(&mut v.recent_cmds_in_dir, "In this dir");
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

    let mut to_select: Option<usize> = None;
    let mut to_close: Option<usize> = None;
    let mut add_new = false;
    let mut connect_remote: Option<usize> = None;
    let mut font_delta = 0.0_f32;

    ui.horizontal(|ui| {
        // Font-size controls sit to the left of the tab strip: a label, the
        // current size, then +/- buttons that scale the terminal panes.
        ui.label("Font size: ");
        ui.label(format!("{}", font_size.round() as i32));
        if ui
            .small_button("+")
            .on_hover_text("Increase terminal font size")
            .clicked()
        {
            font_delta = 1.0;
        }
        if ui
            .small_button("-")
            .on_hover_text("Decrease terminal font size")
            .clicked()
        {
            font_delta = -1.0;
        }
        ui.separator();

        for (i, leaf) in labels.iter().enumerate() {
            let text = format!("{i}: {leaf}");
            if ui.selectable_label(i == active, text).clicked() {
                to_select = Some(i);
            }
            if n > 1 && ui.small_button("x").on_hover_text("Close tab").clicked() {
                to_close = Some(i);
            }
            ui.separator();
        }
        if ui.button("+ tab").on_hover_text("New tab").clicked() {
            add_new = true;
        }
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
/// the top of each side panel's content closure so it only affects that
/// panel's child `ui`, leaving the main terminal column at the default size.
fn shrink_panel_fonts(ui: &mut Ui) {
    for font_id in ui.style_mut().text_styles.values_mut() {
        font_id.size = (font_id.size - PANEL_FONT_REDUCTION).max(1.0);
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

    if state.ui.panel_vis.bookmarks {
        let max = central_reserve(ui, f32::INFINITY);
        Panel::left("bookmarks_panel")
            .resizable(true)
            .default_size(BOOKMARK_COL_WIDTH)
            .max_size(max)
            .show_inside(ui, |ui| {
                shrink_panel_fonts(ui);
                dir_bookmarks(state, ui);
            });
    }

    if state.ui.panel_vis.recent_dirs {
        let max = central_reserve(ui, f32::INFINITY);
        Panel::left("recent_dirs_panel")
            .resizable(true)
            .default_size(RECENT_DIRS_COL_WIDTH)
            .max_size(max)
            .show_inside(ui, |ui| {
                shrink_panel_fonts(ui);
                dir_history(state, ui);
            });
    }

    if state.ui.panel_vis.file_browser {
        let max = central_reserve(ui, f32::INFINITY);
        Panel::left("file_browser_panel")
            .resizable(true)
            .default_size(BROWSER_COL_WIDTH)
            .max_size(max)
            .show_inside(ui, |ui| {
                shrink_panel_fonts(ui);
                file_browser(state, ui);
            });
    }

    if state.ui.panel_vis.remote_terminals {
        let max = central_reserve(ui, f32::INFINITY);
        Panel::left("remote_terminals_panel")
            .resizable(true)
            .default_size(REMOTE_COL_WIDTH)
            .max_size(max)
            .show_inside(ui, |ui| {
                shrink_panel_fonts(ui);
                remote_terminals(state, ui);
            });
    }

    if state.ui.panel_vis.recent_cmds {
        // Cap the cmd-history panel at 200px wide so long commands don't
        // blow out its column; rows wrap inside it instead. `max_size`
        // still allows shrinking via the drag handle, and the central
        // reserve keeps it from crowding out the terminal column.
        let max = central_reserve(ui, RIGHT_COL_WIDTH);
        Panel::right("cmd_history_panel")
            .resizable(true)
            .default_size(RIGHT_COL_WIDTH)
            .max_size(max)
            .show_inside(ui, |ui| {
                shrink_panel_fonts(ui);
                cmd_history(state, ui);
            });
    }

    if state.ui.panel_vis.recent_cmds_in_dir {
        let max = central_reserve(ui, f32::INFINITY);
        Panel::right("cwd_cmd_history_panel")
            .resizable(true)
            .default_size(CWD_HIST_COL_WIDTH)
            .max_size(max)
            .show_inside(ui, |ui| {
                shrink_panel_fonts(ui);
                cwd_cmd_history(state, ui);
            });
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
    if !panel_vis_eq(&vis_before, &state.ui.panel_vis) {
        let _ = state.save();
    }
}
