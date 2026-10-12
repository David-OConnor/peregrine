use std::{
    env, io,
    path::{Path, PathBuf},
};

use chrono::Utc;
use shell::{
    BrowserFile, FontSizes, HistoryItem, NavState, OpenTabs, PanelVis, RecentDir, RemoteSession,
    RemoteTerminal, SshMode, WindowSize, apply_completion, complete_cd_path, current_branch,
    get_home, read_browser_files, save_data, ssh, ssh::secrets, truncate_branch,
};

use crate::{
    OutItem, OutType, cmd_color::CmdColors, files::FileDetails, markdown::MarkdownViewers,
    path_color, ui,
};

/// State specific to the GUI: the current input buffer, the captured terminal
/// output, and a "please focus the input next frame" flag we use after
/// running a command so the user can keep typing.
pub struct StateUi {
    /// By tab
    pub cli_input: Vec<String>,
    /// Outer: Per tab. Inner: Out items in that tab.
    pub out: Vec<Vec<OutItem>>,
    /// Which tab in `State::cwd` / `cli_input` / `out` is currently shown.
    /// Always a valid index into those parallel vectors; we maintain the
    /// invariant that there is at least one tab.
    pub active_tab: usize,
    /// Set when something programmatic should pull keyboard focus to the
    /// input box on the next frame (e.g. just after running a command).
    pub focus_input: bool,
    /// A submitted command that's been echoed but not yet run. Set by
    /// `submit_command` and taken at the start of the next frame, so the echo
    /// paints before the (blocking) command starts.
    pub pending_cmd: Option<String>,

    /// Search text for each sidebar panel. Empty = "show everything". Updated
    /// in-place by the panel's TextEdit.
    pub bookmarks_search: String,
    pub recent_dirs_search: String,
    pub history_search: String,
    pub cwd_history_search: String,

    /// Indices into the corresponding source list (`dir_bookmarks`,
    /// `recent_dirs`, `history`) that match the current search. Re-derived
    /// whenever the search text or the source list changes via the
    /// `refresh_*_filter` helpers, so the render loop can just iterate these
    /// indices without re-running the filter every frame.
    pub bookmarks_filter: Vec<usize>,
    pub recent_dirs_filter: Vec<usize>,
    pub history_filter: Vec<usize>,
    /// History indices restricted to the active tab's cwd. Refreshed on
    /// cwd changes, tab switches, and new history entries.
    pub cwd_history_filter: Vec<usize>,

    /// Per-tab arrow-key recall state: Up/Down walks `history`, Left/Right
    /// walks `recent_dirs`. See [NavState].
    pub history_nav: Vec<NavState>,
    pub panel_vis: PanelVis,

    /// Per-tab browser-style back/forward navigation stacks (parallel to
    /// `State::cwd`). `nav_back` holds dirs we can return to (most recent on
    /// top); `nav_forward` holds dirs we can re-visit after going Back.
    /// Populated whenever the cwd changes via [State::change_dir]; not
    /// persisted (a fresh session starts with empty stacks).
    pub nav_back: Vec<Vec<PathBuf>>,
    pub nav_forward: Vec<Vec<PathBuf>>,

    /// Remote-panel "add / edit remote" form state. `remote_form_open` shows
    /// the form; the text fields hold the in-progress entry; `remote_edit_idx`
    /// is `Some(i)` when editing an existing saved remote rather than adding a
    /// new one.
    pub remote_form_open: bool,
    pub remote_host: String,
    pub remote_port: String,
    pub remote_user: String,
    pub remote_password: String,
    pub remote_edit_idx: Option<usize>,
}

impl Default for StateUi {
    fn default() -> Self {
        Self {
            cli_input: vec![String::new()],
            out: vec![Vec::new()],
            active_tab: 0,
            focus_input: true,
            pending_cmd: None,
            bookmarks_search: String::new(),
            recent_dirs_search: String::new(),
            history_search: String::new(),
            cwd_history_search: String::new(),
            bookmarks_filter: Vec::new(),
            recent_dirs_filter: Vec::new(),
            history_filter: Vec::new(),
            cwd_history_filter: Vec::new(),
            history_nav: vec![NavState::new()],
            panel_vis: PanelVis::default(),
            nav_back: vec![Vec::new()],
            nav_forward: vec![Vec::new()],
            remote_form_open: false,
            remote_host: String::new(),
            remote_port: String::new(),
            remote_user: String::new(),
            remote_password: String::new(),
            remote_edit_idx: None,
        }
    }
}

pub struct State {
    pub ui: StateUi,
    /// Cached.
    pub home: Option<PathBuf>,
    pub history: Vec<HistoryItem>,
    /// Colours for the base commands in the history panels. Rebuilt from
    /// `history` by [Self::refresh_cmd_colors].
    pub cmd_colors: CmdColors,
    /// This initializes to env::current_dir, but is then managed from within
    /// this program. Per tab.
    pub cwd: Vec<PathBuf>,
    /// User-controlled list of directory bookmarks that can be easily
    /// navigated to.
    pub dir_bookmarks: Vec<PathBuf>,
    /// Paths we've execute commands from. Works in a similar way to bookmarks.
    pub recent_dirs: Vec<RecentDir>,
    pub remote_terminals: Vec<RemoteTerminal>,
    /// Roots of repos cloned by full URL (written by the CLI's `clone` alias).
    /// The GUI doesn't use these; it round-trips them so saving doesn't drop them.
    pub clone_roots: Vec<String>,
    /// Live SSH session per tab (parallel to `cwd`). `Some` once a tab connects
    /// via the Remote panel; while set, that tab's typed commands run on the
    /// remote. Not persisted (a connection can't outlive the process).
    pub active_remote: Vec<Option<RemoteSession>>,
    /// In the current dir. Note persistent, unlike some of our other lists.
    pub browser_files: Vec<BrowserFile>,
    /// The row selected in the Files panel, with the details its lower pane
    /// shows. Re-read whenever `browser_files` is, and cleared once the path
    /// is no longer in the listing (e.g. after a cd or tab switch).
    pub selected_file: Option<FileDetails>,
    /// Where bookmarks + recent dirs are persisted.
    pub state_path: PathBuf,
    /// Cached git branch for each tab's cwd (parallel to `cwd`). `None`
    /// when that tab's cwd isn't inside a repo. Refreshed by
    /// `refresh_branch` after every command, cd, tab switch, or new tab.
    pub branch: Vec<Option<String>>,
    /// Last observed window size, refreshed every frame from the viewport so
    /// `save` persists the size the user left the window at. `None` until the
    /// first frame reports a size (and on a fresh file with no saved value).
    pub window_size: Option<WindowSize>,
    /// Font size (in points) for the terminal input/output panes, adjusted via
    /// the "CLI font size" +/- buttons on the tab strip and persisted to the
    /// state file.
    pub font_size: f32,
    /// Body-text size (in points) for the side panels, adjusted via the "GUI
    /// font size" +/- buttons and persisted alongside `font_size`. The panels'
    /// other text styles scale with it (see `ui::scale_panel_fonts`).
    pub gui_font_size: f32,
    /// Open Markdown viewer windows (double-clicked in the Files panel).
    /// Not persisted.
    pub md_viewers: MarkdownViewers,
}

impl Default for State {
    fn default() -> Self {
        let cwd = env::current_dir().unwrap_or_default();
        let branch = current_branch(&cwd);
        let mut s = Self {
            ui: StateUi::default(),
            home: get_home(),
            history: Vec::new(),
            cmd_colors: CmdColors::default(),
            cwd: vec![cwd],
            dir_bookmarks: Vec::new(),
            recent_dirs: Vec::new(),
            remote_terminals: Vec::new(),
            clone_roots: Vec::new(),
            active_remote: vec![None],
            browser_files: Vec::new(),
            selected_file: None,
            state_path: save_data::default_path()
                .unwrap_or_else(|| PathBuf::from(save_data::FILENAME)),
            branch: vec![branch],
            window_size: None,
            font_size: ui::DEFAULT_FONT_SIZE,
            gui_font_size: ui::DEFAULT_GUI_FONT_SIZE,
            md_viewers: MarkdownViewers::default(),
        };
        s.refresh_all_filters();
        s.refresh_cmd_colors();
        s.refresh_browser_files();
        s
    }
}

impl State {
    /// Just the `{star}{cwd}` portion of the prompt, used by the live
    /// input row which renders the branch + nav indicators as separately
    /// coloured labels (see `ui::term_in`).
    pub fn cwd_display(&self) -> String {
        // When the active tab is connected, show the remote location instead of
        // the local cwd so the user knows commands are running remotely.
        if let Some(remote) = self.active_remote() {
            let cwd = if remote.cwd().is_empty() {
                "~"
            } else {
                remote.cwd()
            };
            let mode = match remote.mode() {
                SshMode::Exec => "",
                SshMode::Pty => " (pty)",
            };
            return format!("{}:{}{}", remote.label(), cwd, mode);
        }
        format!("{}{}", self.cwd_star(), self.cwd().display())
    }

    /// `*` when the active tab's cwd is bookmarked, else empty. Shown before
    /// the cwd in the prompt.
    pub fn cwd_star(&self) -> &'static str {
        let cwd = self.cwd();
        if self.dir_bookmarks.iter().any(|p| p.as_path() == cwd) {
            "*"
        } else {
            ""
        }
    }

    /// The active tab's local cwd, split into colour-coded runs (see
    /// [path_color]) for drawing on the black terminal panes.
    pub fn cwd_color_parts(&self) -> Vec<path_color::Part> {
        let home = self.home.as_ref().map(|h| h.display().to_string());
        path_color::parts(&self.cwd().display().to_string(), home.as_deref(), true)
    }

    /// Whether the active tab has a live SSH session (used by the input row to
    /// suppress the local git-branch label while remote).
    pub fn is_remote(&self) -> bool {
        self.active_remote().is_some()
    }

    /// Full prompt prefix used when echoing a command into the output
    /// pane. Embeds ANSI escapes around the cwd's directories and the branch
    /// slot so the output pane's ANSI parser colours them the same as the
    /// live input row does. `nav` adds a ` his N` / ` cd N` indicator
    /// after the branch when a recall is active (see [NavState]).
    pub fn prompt(&self, nav: &NavState) -> String {
        let i = self.ui.active_tab;
        // No local git branch while remote — `cwd_display` already shows the
        // remote location.
        let branch_part = if self.is_remote() {
            String::new()
        } else {
            self.branch[i]
                .as_deref()
                .map(|b| format!(" \x1b[95mbranch: {}\x1b[0m", truncate_branch(b, 10)))
                .unwrap_or_default()
        };
        // Locally, colour-code the cwd like the input row does; the output
        // pane's ANSI parser turns the escapes back into colours.
        let cwd_part = if self.is_remote() {
            self.cwd_display()
        } else {
            let parts = self.cwd_color_parts();
            format!("{}{}", self.cwd_star(), path_color::to_ansi(&parts))
        };
        format!(
            "{}{}{}{}",
            cwd_part,
            branch_part,
            nav.his_indicator(),
            nav.cd_indicator(),
        )
    }

    /// Re-detect the git branch for the active tab's cwd. Called after
    /// every command (a `git checkout` may have switched branches), after
    /// `cd`, tab creation, and tab selection.
    pub fn refresh_branch(&mut self) {
        let i = self.ui.active_tab;
        self.branch[i] = current_branch(&self.cwd[i]);
    }

    /// Re-read the active tab's cwd into `browser_files`. Called after
    /// every command (a passthrough command may have created/removed
    /// files), after `cd`, tab creation, and tab selection — same trigger
    /// points as [refresh_branch].
    pub fn refresh_browser_files(&mut self) {
        let cwd = self.cwd().to_path_buf();
        self.browser_files = read_browser_files(&cwd);
        if let Some(sel) = self.selected_file.take() {
            self.select_file(&sel.path);
        }
    }

    /// Select the Files-panel row for `path` and read its details. Leaves
    /// nothing selected if `path` isn't in the current listing.
    pub fn select_file(&mut self, path: &Path) {
        self.selected_file = self
            .browser_files
            .iter()
            .find(|f| f.path == path)
            .and_then(FileDetails::read);
    }

    /// Snapshot of the active tab's recall cursors. Returned by value so
    /// callers can build a prompt string without holding a borrow on
    /// `self.ui.history_nav`.
    pub fn active_nav_cursors(&self) -> (Option<usize>, Option<usize>) {
        let n = &self.ui.history_nav[self.ui.active_tab];
        (n.his_cursor, n.cd_cursor)
    }

    pub fn save(&self) -> io::Result<()> {
        // Persist the current tab layout (each tab's cwd, in order, plus the
        // active index) so reopening the GUI restores the same tabs.
        let open_tabs = OpenTabs {
            paths: self.cwd.clone(),
            active: self.ui.active_tab,
        };
        save_data::save_state(
            &self.dir_bookmarks,
            &self.recent_dirs,
            &self.history,
            &self.remote_terminals,
            &self.clone_roots,
            &self.ui.panel_vis,
            self.window_size,
            &open_tabs,
            FontSizes {
                cli: Some(self.font_size),
                gui: Some(self.gui_font_size),
            },
            &self.state_path,
        )
    }

    /// Adjust the terminal-pane font size by `delta` points, clamped to a
    /// sane range, and persist the change. Wired to the "CLI font size" +/-
    /// buttons on the tab strip.
    pub fn adjust_font_size(&mut self, delta: f32) {
        let next = (self.font_size + delta).clamp(ui::MIN_FONT_SIZE, ui::MAX_FONT_SIZE);
        if next != self.font_size {
            self.font_size = next;
            let _ = self.save();
        }
    }

    /// As [Self::adjust_font_size], for the side panels' "GUI font size".
    pub fn adjust_gui_font_size(&mut self, delta: f32) {
        let next = (self.gui_font_size + delta).clamp(ui::MIN_FONT_SIZE, ui::MAX_FONT_SIZE);
        if next != self.gui_font_size {
            self.gui_font_size = next;
            let _ = self.save();
        }
    }

    pub fn load(path: PathBuf) -> io::Result<Self> {
        let loaded = save_data::load_state(&path)?;

        // Restore the saved tabs. Drop any whose directory no longer exists
        // (it may have been deleted or renamed since the last run); if none
        // remain — or the file predates the feature — fall back to a single
        // tab at the current directory.
        let mut cwd: Vec<PathBuf> = loaded
            .open_tabs
            .paths
            .into_iter()
            .filter(|p| p.is_dir())
            .collect();
        if cwd.is_empty() {
            cwd.push(env::current_dir().unwrap_or_default());
        }

        // Clamp the saved active index to the (possibly shrunken) tab list and
        // point the process at that tab's cwd so the first command and the
        // initial file listing match the selected tab.
        let active_tab = loaded.open_tabs.active.min(cwd.len() - 1);
        let _ = env::set_current_dir(&cwd[active_tab]);

        let n = cwd.len();
        let branch = cwd.iter().map(|c| current_branch(c)).collect();

        let mut ui = StateUi::default();
        ui.panel_vis = loaded.panel_vis;
        ui.active_tab = active_tab;
        ui.cli_input = vec![String::new(); n];
        ui.out = (0..n).map(|_| Vec::new()).collect();
        ui.history_nav = (0..n).map(|_| NavState::new()).collect();
        ui.nav_back = (0..n).map(|_| Vec::new()).collect();
        ui.nav_forward = (0..n).map(|_| Vec::new()).collect();

        let mut s = Self {
            ui,
            home: get_home(),
            history: loaded.history,
            cmd_colors: CmdColors::default(),
            cwd,
            dir_bookmarks: loaded.bookmarks,
            recent_dirs: loaded.recent_dirs,
            remote_terminals: loaded.remote_terminals,
            clone_roots: loaded.clone_roots,
            active_remote: (0..n).map(|_| None).collect(),
            browser_files: Vec::new(),
            selected_file: None,
            state_path: path,
            branch,
            window_size: loaded.window_size,
            font_size: loaded.font_size.cli.unwrap_or(ui::DEFAULT_FONT_SIZE),
            gui_font_size: loaded.font_size.gui.unwrap_or(ui::DEFAULT_GUI_FONT_SIZE),
            md_viewers: MarkdownViewers::default(),
        };
        s.refresh_all_filters();
        s.refresh_cmd_colors();
        s.refresh_browser_files();
        Ok(s)
    }

    /// Active tab's cwd. Always valid: we maintain at least one tab.
    pub fn cwd(&self) -> &Path {
        &self.cwd[self.ui.active_tab]
    }

    pub fn push_out(&mut self, text: impl Into<String>, type_: OutType) {
        let i = self.ui.active_tab;
        self.ui.out[i].push(OutItem {
            text: text.into(),
            type_,
            dt: Utc::now(),
        });
    }

    /// Open a new tab inheriting the active tab's cwd. Switches focus to it.
    pub fn add_tab(&mut self) {
        let new_cwd = self.cwd().to_path_buf();
        // New tab inherits the active tab's branch (same cwd → same repo).
        let inherited_branch = self.branch[self.ui.active_tab].clone();

        self.cwd.push(new_cwd);
        self.ui.cli_input.push(String::new());
        self.ui.out.push(Vec::new());
        self.ui.history_nav.push(NavState::new());
        self.ui.nav_back.push(Vec::new());
        self.ui.nav_forward.push(Vec::new());
        self.branch.push(inherited_branch);
        // New tabs start local; a connection isn't inherited.
        self.active_remote.push(None);

        self.ui.active_tab = self.cwd.len() - 1;
        self.ui.focus_input = true;

        self.refresh_cwd_history_filter();
        self.refresh_browser_files();
    }

    /// Close a tab. No-op if it's the last remaining one (we keep at least
    /// one tab open so the invariant holds).
    pub fn close_tab(&mut self, idx: usize) {
        if self.cwd.len() <= 1 || idx >= self.cwd.len() {
            return;
        }
        self.cwd.remove(idx);
        self.ui.cli_input.remove(idx);
        self.ui.out.remove(idx);
        self.ui.history_nav.remove(idx);
        self.ui.nav_back.remove(idx);
        self.ui.nav_forward.remove(idx);
        self.branch.remove(idx);
        // Tear down any SSH session attached to the closed tab.
        if let Some(session) = self.active_remote.remove(idx) {
            session.disconnect();
        }

        if self.ui.active_tab >= self.cwd.len() {
            self.ui.active_tab = self.cwd.len() - 1;
        } else if idx < self.ui.active_tab {
            self.ui.active_tab -= 1;
        }

        let cwd = self.cwd().to_path_buf();
        let _ = env::set_current_dir(&cwd);

        self.refresh_cwd_history_filter();
        // Branch may have moved on if the closed tab ran a checkout while
        // hidden, or if the newly active tab is in a different repo.
        self.refresh_branch();
        self.refresh_browser_files();
    }

    pub fn select_tab(&mut self, idx: usize) {
        if idx >= self.cwd.len() || idx == self.ui.active_tab {
            return;
        }

        self.ui.active_tab = idx;
        let cwd = self.cwd().to_path_buf();
        let _ = env::set_current_dir(&cwd);

        self.refresh_cwd_history_filter();
        self.refresh_branch();
        self.refresh_browser_files();

        self.ui.focus_input = true;
    }

    /// Rebuild every cached filter. Used right after construction (so the
    /// first frame already has populated filter lists) and as a convenience
    /// when we don't know which list just changed.
    pub fn refresh_all_filters(&mut self) {
        self.refresh_bookmarks_filter();
        self.refresh_recent_dirs_filter();
        self.refresh_history_filter();
        self.refresh_cwd_history_filter();
    }

    /// History indices restricted to the active tab's cwd, then narrowed by
    /// the cwd-history search box.
    pub fn refresh_cwd_history_filter(&mut self) {
        let cwd = self.cwd().to_path_buf();
        let needle = self.ui.cwd_history_search.to_lowercase();

        self.ui.cwd_history_filter = self
            .history
            .iter()
            .enumerate()
            .filter(|(_, h)| h.dir == cwd)
            .filter(|(_, h)| needle.is_empty() || h.text.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
    }

    /// Case-insensitive substring match against each bookmark's full
    /// rendered path. Empty search matches everything.
    pub fn refresh_bookmarks_filter(&mut self) {
        let needle = self.ui.bookmarks_search.to_lowercase();

        self.ui.bookmarks_filter = self
            .dir_bookmarks
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                needle.is_empty() || p.display().to_string().to_lowercase().contains(&needle)
            })
            .map(|(i, _)| i)
            .collect();
    }

    pub fn refresh_recent_dirs_filter(&mut self) {
        let needle = self.ui.recent_dirs_search.to_lowercase();

        self.ui.recent_dirs_filter = self
            .recent_dirs
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                needle.is_empty()
                    || r.path
                        .display()
                        .to_string()
                        .to_lowercase()
                        .contains(&needle)
            })
            .map(|(i, _)| i)
            .collect();
    }

    /// Re-assign the history panels' command colours after `history`
    /// changes. See [CmdColors].
    pub fn refresh_cmd_colors(&mut self) {
        self.cmd_colors = CmdColors::new(self.history.iter().rev().map(|h| h.text.as_str()));
    }

    /// Searches just the command text (not the dir) — that's what the user
    /// typically remembers when re-running.
    pub fn refresh_history_filter(&mut self) {
        let needle = self.ui.history_search.to_lowercase();

        self.ui.history_filter = self
            .history
            .iter()
            .enumerate()
            .filter(|(_, h)| needle.is_empty() || h.text.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
    }

    /// Toggle the current dir in the bookmark list. Returns the message we
    /// surfaced to the user, so callers can echo it into the output pane.
    pub fn toggle_bookmark_cwd(&mut self) -> String {
        let cwd = self.cwd().to_path_buf();

        let msg = if let Some(pos) = self.dir_bookmarks.iter().position(|p| p == &cwd) {
            self.dir_bookmarks.remove(pos);
            format!("Removed bookmark: {}", cwd.display())
        } else {
            self.dir_bookmarks.push(cwd.clone());
            format!("Added a bookmark: {}", cwd.display())
        };

        let _ = self.save();
        self.refresh_bookmarks_filter();

        msg
    }

    /// Walk the global history (all directories) for the active tab's input
    /// row. `up = true` moves to an older entry; `up = false` to a newer one.
    /// Delegates to [NavState::step_his] for the shared logic.
    pub fn history_nav(&mut self, up: bool) {
        let i = self.ui.active_tab;
        let live = self.ui.cli_input[i].clone();
        if let Some(text) = self.ui.history_nav[i].step_his(&self.history, up, &live) {
            self.ui.cli_input[i] = text;
        }
    }

    /// Walk recent-dirs for the active tab's input row. `left = true` moves
    /// to an older entry; `left = false` to a newer one. Loads a
    /// `cd <tilde-path>` command into the buffer so Enter goes there.
    pub fn recent_dir_nav(&mut self, left: bool) {
        let i = self.ui.active_tab;
        let live = self.ui.cli_input[i].clone();
        let home = self.home.clone();

        let result = self.ui.history_nav[i].step_cd(&self.recent_dirs, left, &live, |path| {
            format!("cd {}", crate::render_with_tilde(path, home.as_deref()))
        });

        if let Some(text) = result {
            self.ui.cli_input[i] = text;
        }
    }

    /// Complete the active input row using the shared `cd` path completer.
    pub fn autocomplete_input(&mut self) {
        let i = self.ui.active_tab;
        let live = self.ui.cli_input[i].clone();
        let Some(result) = complete_cd_path(
            &live,
            live.len(),
            self.cwd(),
            self.home.as_deref(),
            &self.dir_bookmarks,
            &self.recent_dirs,
        ) else {
            return;
        };
        if let Some(text) = apply_completion(&live, live.len(), &result) {
            self.ui.cli_input[i] = text;
        }
    }

    /// Reset the active tab's recall state back to the live input. Called
    /// after a command is submitted.
    pub fn reset_history_nav(&mut self) {
        let i = self.ui.active_tab;
        self.ui.history_nav[i].reset();
    }

    /// Raw cwd change for the active tab, without touching the back/forward
    /// stacks. Returns `true` if the cwd actually changed. Pushes a
    /// diagnostic on failure.
    fn navigate_to(&mut self, target: PathBuf) -> bool {
        let before = self.cwd().to_path_buf();
        match env::set_current_dir(&target) {
            Ok(_) => {
                let new_cwd = env::current_dir().unwrap_or(target);
                let i = self.ui.active_tab;
                self.cwd[i] = new_cwd;
                self.refresh_cwd_history_filter();
                self.refresh_branch();
                self.refresh_browser_files();
                self.cwd() != before
            }
            Err(e) => {
                self.push_out(format!("cd: {e}"), OutType::StdErr);
                false
            }
        }
    }

    /// `cd` to `target` and update the active tab's cwd. This is the "normal"
    /// navigation path (typed `cd`, clicking a bookmark/recent dir/folder,
    /// the Up button): on success it records the previous dir on the Back
    /// stack and clears the Forward stack, just like a web/file browser.
    pub fn change_dir(&mut self, target: PathBuf) {
        let before = self.cwd().to_path_buf();
        if self.navigate_to(target) {
            let i = self.ui.active_tab;
            self.ui.nav_back[i].push(before);
            self.ui.nav_forward[i].clear();
        }
    }

    /// Navigate to the parent of the active tab's cwd. No-op at a filesystem
    /// root. Records history like any other navigation.
    pub fn nav_up(&mut self) {
        if let Some(parent) = self.cwd().parent() {
            let parent = parent.to_path_buf();
            self.change_dir(parent);
        }
    }

    /// Back button: return to the most recent dir on the Back stack, pushing
    /// the current dir onto the Forward stack so it can be re-visited.
    pub fn nav_back(&mut self) {
        let i = self.ui.active_tab;
        let Some(target) = self.ui.nav_back[i].pop() else {
            return;
        };
        let before = self.cwd().to_path_buf();
        if self.navigate_to(target) {
            self.ui.nav_forward[i].push(before);
        }
    }

    /// Forward button: re-visit the dir we last went Back from.
    pub fn nav_forward(&mut self) {
        let i = self.ui.active_tab;
        let Some(target) = self.ui.nav_forward[i].pop() else {
            return;
        };
        let before = self.cwd().to_path_buf();
        if self.navigate_to(target) {
            self.ui.nav_back[i].push(before);
        }
    }

    // --- SSH ------------------------------------------------------------

    /// The active tab's live SSH session, if any.
    pub fn active_remote(&self) -> Option<&RemoteSession> {
        self.active_remote[self.ui.active_tab].as_ref()
    }

    /// Connect the active tab to the saved remote at `idx`. The password comes
    /// from the OS keyring (saved when the remote was added).
    pub fn connect_remote(&mut self, idx: usize) {
        let Some(rt) = self.remote_terminals.get(idx) else {
            self.push_out(
                format!("ssh: no saved remote at index {idx}"),
                OutType::StdErr,
            );
            return;
        };
        let (host, port, user) = (rt.host.clone(), rt.port, rt.username.clone());
        self.connect_with(host, port, user);
    }

    /// `ssh <index>` (a saved remote) or `ssh [user@]host[:port] [port]` (a
    /// freeform target). Mirrors the CLI's `ssh` built-in so the typed command
    /// behaves the same in both frontends.
    pub fn cmd_ssh(&mut self, args: &str) {
        let args = args.trim();
        if args.is_empty() {
            self.push_out(
                "ssh: usage: ssh [user@]host [port]  |  ssh <saved-index>",
                OutType::StdErr,
            );
            return;
        }
        // A bare number selects a saved remote by its index.
        if let Ok(idx) = args.parse::<usize>() {
            self.connect_remote(idx);
            return;
        }
        let (host, port, user) = parse_remote_spec(args);
        self.connect_with(host, port, user);
    }

    /// Shared connect path. Pulls the password from the OS keyring (a missing
    /// one is reported rather than prompting — the GUI can't host a blocking
    /// prompt mid-frame, so the user saves it via the Remote panel form first).
    /// Blocks the UI thread for the duration of the connect, like a shelled-out
    /// command. Replaces any existing session on the active tab.
    fn connect_with(&mut self, host: String, port: u16, user: String) {
        let Some(password) = secrets::get_password(&host, port, &user) else {
            self.push_out(
                format!("ssh: no saved password for {user}@{host}; add it in the Remote panel"),
                OutType::StdErr,
            );
            return;
        };

        self.push_out(
            format!("Connecting to {user}@{host}:{port} …"),
            OutType::Prompt,
        );
        match ssh::connect(&host, port, &user, &password) {
            Ok(session) => {
                let i = self.ui.active_tab;
                // Drop any prior session on this tab before replacing it.
                if let Some(old) = self.active_remote[i].replace(session) {
                    old.disconnect();
                }
                self.push_out(format!("Connected to {user}@{host}"), OutType::StdOut);
            }
            Err(e) => self.push_out(format!("ssh: {e}"), OutType::StdErr),
        }
    }

    /// Disconnect the active tab's session, if any.
    pub fn disconnect_remote(&mut self) {
        let i = self.ui.active_tab;
        if let Some(session) = self.active_remote[i].take() {
            session.disconnect();
            self.push_out("ssh: disconnected", OutType::StdOut);
        }
    }

    /// Flip the active tab's session between exec and PTY mode.
    pub fn toggle_remote_mode(&mut self) {
        let i = self.ui.active_tab;
        let res = self.active_remote[i].as_mut().map(|s| {
            let next = match s.mode() {
                SshMode::Exec => SshMode::Pty,
                SshMode::Pty => SshMode::Exec,
            };
            (next, s.set_mode(next))
        });

        if let Some((next, r)) = res {
            match r {
                Ok(()) => self.push_out(
                    format!(
                        "ssh: switched to {} mode",
                        if next == SshMode::Pty {
                            "interactive"
                        } else {
                            "exec"
                        }
                    ),
                    OutType::StdOut,
                ),
                Err(e) => self.push_out(format!("ssh: {e}"), OutType::StdErr),
            }
        }
    }

    /// Run one command on the active tab's remote in exec mode, buffering the
    /// output and pushing it into the pane afterwards. We `take` the session
    /// out of `self` for the call so `RemoteSession::run` (which needs `&mut
    /// self`) doesn't clash with the `&mut self` borrow `push_out` needs.
    pub fn run_remote_exec(&mut self, input: &str) {
        let i = self.ui.active_tab;
        let Some(mut session) = self.active_remote[i].take() else {
            return;
        };
        let mut lines: Vec<(OutType, String)> = Vec::new();
        let result = session.run(input, &mut |kind, msg| {
            lines.push((crate::out_type_for(kind), msg));
        });
        self.active_remote[i] = Some(session);

        for (t, m) in lines {
            self.push_out(m, t);
        }
        if let Err(e) = result {
            self.push_out(format!("ssh: {e}"), OutType::StdErr);
        }
    }

    /// Send a line of input to the active tab's PTY session (the remote echoes
    /// it back, so we don't echo locally).
    pub fn send_remote_line(&mut self, line: &str) {
        if let Some(session) = self.active_remote() {
            let mut bytes = line.as_bytes().to_vec();
            bytes.push(b'\r');
            let _ = session.send(&bytes);
        }
    }

    /// Drain any live PTY output for the active tab into the output pane and,
    /// if the remote shell has closed, drop back to local. Returns `true` when
    /// a PTY session is active (so the caller keeps requesting repaints to keep
    /// output flowing). Called once per frame.
    pub fn pump_pty(&mut self) -> bool {
        let i = self.ui.active_tab;
        let (text, closed) = match self.active_remote[i].as_ref() {
            Some(s) if s.mode() == SshMode::Pty => {
                let bytes = s.drain_output();
                (String::from_utf8_lossy(&bytes).into_owned(), s.pty_closed())
            }
            _ => return false,
        };
        if !text.is_empty() {
            self.push_out(text, OutType::StdOut);
        }
        if closed {
            if let Some(session) = self.active_remote[i].take() {
                session.disconnect();
            }
            self.push_out("ssh: remote shell exited; disconnected", OutType::StdOut);
            return false;
        }
        true
    }

    /// `remote list | add <[user@]host[:port]> | del <index>`, mirroring the
    /// CLI built-in. `add` can't prompt for a password mid-frame, so it
    /// pre-fills and opens the Remote panel's form for the user to finish.
    pub fn cmd_remote(&mut self, args: &str) {
        let (sub, rest) = match args.find(char::is_whitespace) {
            Some(i) => (&args[..i], args[i..].trim()),
            None => (args, ""),
        };
        match sub {
            "" | "list" | "ls" => {
                let listing = ssh::render_remotes(&self.remote_terminals, 0);
                self.push_out(listing, OutType::StdOut);
            }
            "add" => {
                if rest.is_empty() {
                    self.push_out(
                        "remote: usage: remote add [user@]host[:port] [port]",
                        OutType::StdErr,
                    );
                    return;
                }
                let (host, port, user) = parse_remote_spec(rest);
                self.ui.remote_host = host;
                self.ui.remote_port = port.to_string();
                self.ui.remote_user = user;
                self.ui.remote_password.clear();
                self.ui.remote_edit_idx = None;
                self.ui.remote_form_open = true;
                self.ui.panel_vis.remote_terminals = true;
                self.push_out(
                    "remote: enter the password in the Remote panel's form, then save",
                    OutType::StdOut,
                );
            }
            "del" | "rm" | "remove" => match rest.parse::<usize>() {
                Ok(idx) if idx < self.remote_terminals.len() => self.remove_remote(idx),
                Ok(idx) => self.push_out(
                    format!("remote: no saved remote at index {idx}"),
                    OutType::StdErr,
                ),
                Err(_) => self.push_out("remote: usage: remote del <number>", OutType::StdErr),
            },
            other => self.push_out(
                format!("remote: unknown subcommand `{other}` (expected list, add, or del)"),
                OutType::StdErr,
            ),
        }
    }

    /// Delete a saved remote and its keyring password.
    pub fn remove_remote(&mut self, idx: usize) {
        if idx >= self.remote_terminals.len() {
            return;
        }
        let r = self.remote_terminals.remove(idx);
        let _ = secrets::delete_password(&r.host, r.port, &r.username);
        let _ = self.save();
        self.push_out(
            format!("Removed remote {}@{}:{}", r.username, r.host, r.port),
            OutType::StdOut,
        );
    }

    /// Save (or update) a remote from the Remote-panel form fields, storing its
    /// password in the keyring when one was entered. `edit_idx` updates an
    /// existing entry in place; `None` adds a new one (deduped on
    /// host/port/user). Returns an error string for the caller to surface.
    pub fn save_remote_from_form(&mut self) -> Result<(), String> {
        let host = self.ui.remote_host.trim().to_string();
        if host.is_empty() {
            return Err("host is required".to_string());
        }
        let user = {
            let u = self.ui.remote_user.trim();
            if u.is_empty() {
                "root".to_string()
            } else {
                u.to_string()
            }
        };
        let port: u16 = match self.ui.remote_port.trim() {
            "" => 22,
            p => p.parse().map_err(|_| format!("invalid port `{p}`"))?,
        };

        if !self.ui.remote_password.is_empty() {
            secrets::set_password(&host, port, &user, &self.ui.remote_password)
                .map_err(|e| format!("couldn't save password: {e}"))?;
        }

        match self.ui.remote_edit_idx {
            Some(i) if i < self.remote_terminals.len() => {
                self.remote_terminals[i] = RemoteTerminal {
                    host,
                    port,
                    username: user,
                };
            }
            _ => {
                self.remote_terminals
                    .retain(|r| !(r.host == host && r.port == port && r.username == user));
                self.remote_terminals.push(RemoteTerminal {
                    host,
                    port,
                    username: user,
                });
            }
        }

        let _ = self.save();

        // Reset the form.
        self.ui.remote_form_open = false;
        self.ui.remote_edit_idx = None;
        self.ui.remote_host.clear();
        self.ui.remote_port.clear();
        self.ui.remote_user.clear();
        self.ui.remote_password.clear();
        Ok(())
    }
}

/// The OS account name, used as the default SSH username when `ssh host` omits
/// `user@`. Mirrors the CLI's helper in `shell::commands`.
fn default_user() -> String {
    env::var("USERNAME")
        .or_else(|_| env::var("USER"))
        .unwrap_or_else(|_| "root".to_string())
}

/// Parse a `[user@]host[:port] [port]` spec into `(host, port, user)`. The
/// optional trailing port argument wins over a `:port` suffix; defaults are the
/// OS user and port 22. Mirrors the CLI's `parse_remote_spec`.
fn parse_remote_spec(spec: &str) -> (String, u16, String) {
    let mut parts = spec.split_whitespace();
    let target = parts.next().unwrap_or("");
    let port_arg = parts.next().and_then(|p| p.parse().ok());

    let (user, hostport) = match target.split_once('@') {
        Some((u, hp)) => (u.to_string(), hp),
        None => (default_user(), target),
    };
    let (host, port) = match hostport.split_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(22)),
        None => (hostport.to_string(), 22),
    };
    (host, port_arg.unwrap_or(port), user)
}

impl eframe::App for State {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Track the current window size each frame so `save` (which runs on
        // every command) and `on_exit` persist whatever size the user has
        // dragged the window to. `inner_rect` is the content area in logical
        // points — what we hand back to `with_inner_size` on the next launch.
        if let Some(rect) = ui.ctx().input(|i| i.viewport().inner_rect) {
            let size = rect.size();
            if size.x > 0.0 && size.y > 0.0 {
                self.window_size = Some(WindowSize {
                    x: size.x,
                    y: size.y,
                });
            }
        }
        ui::draw(self, ui);
    }

    /// Persist on close so a resize made without running any command (which
    /// would otherwise be the only thing that triggers a save) still sticks.
    /// Disambiguated to the inherent `save` — `eframe::App` also has a `save`.
    fn on_exit(&mut self) {
        let _ = State::save(self);
    }
}
