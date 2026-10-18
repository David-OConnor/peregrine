use std::{
    collections::HashSet,
    env, io,
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::Utc;
use shell::{
    Appearance, BrowserFile, CmdHistoryPrefs, DirLabelLens, DirList, FilesPanelPrefs, FontSizes,
    HistoryItem, NavState, OpenTabs, PaneSplits, PanelVis, PanelWidths, RecentDir,
    RemoteTerminal, WindowSize, apply_completion, current_branch, dir_missing, get_home,
    read_browser_files, remove_missing_recent_dirs, remove_saved_dir, save_data,
    ssh::{self, ConnectError, ConnectRequest, secrets},
    stale_dir_list, truncate_branch,
};

use crate::{
    OutType,
    clipboard::{self, Paste},
    cmd_color::CmdColors,
    completion,
    deep_search::{self, DeepSearch},
    files::{Delete, FileDetails, FileSort, SelectionSummary, sort_browser_files},
    path_color,
    project::Project,
    remote::{LiveRemote, Remote},
    running::RunningCmd,
    terminal::{self, Output},
    ui::{self, path_panel::PathPanel},
    viewer::{FileViewers, ImagePreview},
};

/// State specific to the GUI: the current input buffer, the captured terminal
/// output, and a "please focus the input next frame" flag we use after
/// running a command so the user can keep typing.
pub struct StateUi {
    pub clipboard: ui::clipboard::ClipboardPane,
    /// The About window and its last folder-opening error. Not persisted.
    pub about: bool,
    pub about_folder_error: Option<String>,
    /// Checking for, downloading, and installing a newer release, from the
    /// About window. Not persisted.
    pub updater: software_manager::Updater,
    /// By tab
    pub cli_input: Vec<String>,
    /// Bounded terminal scrollback per tab.
    pub out: Vec<Output>,
    /// Which tab in `State::cwd` / `cli_input` / `out` is currently shown.
    /// Always a valid index into those parallel vectors; we maintain the
    /// invariant that there is at least one tab.
    pub active_tab: usize,
    /// Set when something programmatic should pull keyboard focus to the
    /// input box on the next frame (e.g. just after running a command).
    pub focus_input: bool,
    /// A submitted command that's been echoed but not yet run. Set by
    /// `submit_command` and taken at the start of the next frame, so the echo
    /// paints before a built-in that blocks (`sync`, say) starts.
    pub pending_cmd: Option<String>,
    /// The input at a Tab whose completion was ambiguous and added nothing.
    /// A second Tab on the same input lists the candidates, as the CLI does.
    pub tab_list_pending: Option<String>,

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
    /// Each side panel's width as drawn last frame (`None` until it's first
    /// shown, when nothing was saved). Persisted so the panels reopen at the
    /// widths the user dragged them to; the saved value seeds the panel's
    /// `default_size`, after which egui owns the live width.
    pub panel_widths: PanelWidths,
    /// Set while a panel edge drag has changed `panel_widths` without a save
    /// yet; `ui::draw` saves once the pointer is released.
    pub panel_widths_unsaved: bool,

    /// Per-tab browser-style back/forward navigation stacks (parallel to
    /// `State::cwd`). `nav_back` holds dirs we can return to (most recent on
    /// top); `nav_forward` holds dirs we can re-visit after going Back.
    /// Populated whenever the cwd changes via [State::change_dir]; not
    /// persisted (a fresh session starts with empty stacks).
    pub nav_back: Vec<Vec<PathBuf>>,
    pub nav_forward: Vec<Vec<PathBuf>>,

    /// A Recent dirs / Bookmarks entry found to no longer exist when a cd to
    /// it failed, and the list it's in. While set, a popup asks whether to
    /// remove it (see [State::change_saved_dir]).
    pub stale_dir: Option<(PathBuf, DirList)>,

    /// Files-panel listing filters, toggled independently by the buttons
    /// above the listing; not persisted. `show_hidden_attr` covers entries
    /// with the Windows Hidden attribute ([BrowserFile::hidden_attr]);
    /// `show_dotfiles` covers names starting with `.`.
    pub show_hidden_attr: bool,
    pub show_dotfiles: bool,
    /// The Files-panel listing's order, picked with the Name / Modified /
    /// Type / Size buttons. Not persisted.
    pub files_sort: FileSort,
    /// Files-panel text filter: case-insensitive substring match against
    /// entry names. Empty = show everything. Cleared when the listing moves
    /// to a different directory (see [State::refresh_browser_files]).
    pub files_search: String,
    /// Indices into `State::browser_files` matching `files_search`, kept
    /// current by [State::refresh_browser_files_filter].
    pub browser_files_filter: Vec<usize>,
    /// The Files-panel row being renamed in place, if any.
    pub rename: Option<FileRename>,
    /// A click on the already-selected Files row, and the time (egui's
    /// `InputState::time`) it was released. It becomes a rename once a
    /// double-click can no longer follow, so double-clicking the selected
    /// row still opens it. Dropped by any other press in the meantime.
    pub rename_pending: Option<(PathBuf, f64)>,
    /// The new Files-panel folder or file being named, if any.
    pub create: Option<FileCreate>,
    /// Snapshot of the selected paths awaiting the delete confirmation.
    pub delete_pending: Option<Vec<PathBuf>>,
    /// Set while the last click was in the Files panel, so Ctrl+C / X / V
    /// act on its files rather than on whatever else has the keyboard.
    pub files_active: bool,
    /// Set by a press that made the Files panel active, until the next
    /// press. A click that only activates the panel, as on a window in a
    /// file manager, doesn't rename the selected row.
    pub files_activating_press: bool,
    /// A Ctrl+C (`false`) or Ctrl+X (`true`) in the Files panel last frame,
    /// which copies or cuts its selected rows unless egui copied text for it
    /// instead (see `ui::clipboard::capture`).
    pub files_copy_pending: Option<bool>,
    /// How many columns the Files listing was last laid out in, for the
    /// arrow keys to move across.
    pub files_columns: usize,
    /// The fraction of the Files panel's height its details pane takes, and
    /// likewise for the PATH window's; see [ui::Divider]. Persisted.
    pub files_details_split: f32,
    pub path_details_split: f32,
    /// Files-panel Deep search text, looked for in the contents (or names)
    /// of every file under the listed dir (see [State::start_deep_search]).
    /// Empty = off, and the panel lists the dir as usual. Cleared on cd,
    /// like `files_search`.
    pub deep_search: String,
    /// When `deep_search` was last edited (egui's `InputState::time`), until
    /// the search it calls for starts once typing pauses.
    pub deep_search_edited: Option<f64>,
    /// Regex or not, names or contents: the toggles beside the box.
    pub deep_search_opts: deep_search::Options,
    /// The branch name typed beside the Files panel's Git Branch button.
    pub git_branch_input: String,

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

    /// Recently closed tabs, most recent last, for
    /// [State::reopen_closed_tab]. Capped at [CLOSED_TABS_MAX]; not
    /// persisted.
    pub closed_tabs: Vec<DetachedTab>,

    /// The output pane's size in characters (columns, rows) as last drawn,
    /// which remote terminals are kept at.
    pub term_size: (u16, u16),
}

/// Most closed tabs kept for [State::reopen_closed_tab].
const CLOSED_TABS_MAX: usize = 20;

/// A tab's own state while it's out of the tab strip: one just closed, kept
/// so it can be reopened, or a new one about to go in. A running command or
/// SSH session ends with its tab, so neither is kept.
pub struct DetachedTab {
    /// Where it sits (or sat) in the strip.
    index: usize,
    cwd: PathBuf,
    cli_input: String,
    out: Output,
    nav_back: Vec<PathBuf>,
    nav_forward: Vec<PathBuf>,
}

impl DetachedTab {
    /// A fresh tab at `cwd`, to go in at `index`.
    fn new(index: usize, cwd: PathBuf) -> Self {
        Self {
            index,
            cwd,
            cli_input: String::new(),
            out: Output::default(),
            nav_back: Vec::new(),
            nav_forward: Vec::new(),
        }
    }
}

fn trim_closed_output(tabs: &mut [DetachedTab]) {
    let mut held: usize = tabs.iter().map(|t| t.out.retained_bytes()).sum();
    for tab in tabs {
        if held <= terminal::CLOSED_BYTES {
            break;
        }
        let before = tab.out.retained_bytes();
        tab.out.clear();
        held -= before - tab.out.retained_bytes();
    }
}

/// Where the tab at `i` ends up once the tab at `from` moves to `to`.
fn moved_tab_index(i: usize, from: usize, to: usize) -> usize {
    if i == from {
        to
    } else if from < i && i <= to {
        i - 1
    } else if to <= i && i < from {
        i + 1
    } else {
        i
    }
}

impl Default for StateUi {
    fn default() -> Self {
        Self {
            clipboard: ui::clipboard::ClipboardPane::default(),
            about: false,
            about_folder_error: None,
            updater: Default::default(),
            cli_input: vec![String::new()],
            out: vec![Output::default()],
            active_tab: 0,
            focus_input: true,
            pending_cmd: None,
            tab_list_pending: None,
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
            panel_widths: PanelWidths::default(),
            panel_widths_unsaved: false,
            nav_back: vec![Vec::new()],
            nav_forward: vec![Vec::new()],
            stale_dir: None,
            show_hidden_attr: true,
            show_dotfiles: true,
            files_sort: FileSort::default(),
            files_search: String::new(),
            browser_files_filter: Vec::new(),
            rename: None,
            rename_pending: None,
            create: None,
            delete_pending: None,
            files_active: false,
            files_activating_press: false,
            files_copy_pending: None,
            files_columns: 1,
            files_details_split: ui::DEFAULT_DETAILS_SPLIT,
            path_details_split: ui::DEFAULT_DETAILS_SPLIT,
            deep_search: String::new(),
            deep_search_edited: None,
            deep_search_opts: deep_search::Options::default(),
            git_branch_input: String::new(),
            remote_form_open: false,
            remote_host: String::new(),
            remote_port: String::new(),
            remote_user: String::new(),
            remote_password: String::new(),
            remote_edit_idx: None,
            closed_tabs: Vec::new(),
            term_size: (80, 24),
        }
    }
}

/// The rows selected in the Files panel: one by a click, more with Ctrl and
/// Shift, as in a file manager. Changed only by [State::select_file] and
/// co., which keep [State::selected_file] in step.
#[derive(Default)]
pub struct FileSelection {
    pub paths: HashSet<PathBuf>,
    /// Where a Shift+click or Shift+arrow selects from: the row last clicked
    /// or moved to without Shift.
    pub anchor: Option<PathBuf>,
    /// Totals for the details pane while more than one row is selected.
    pub summary: Option<SelectionSummary>,
}

/// A Files-panel row being renamed in place (see [State::start_rename]).
pub struct FileRename {
    pub path: PathBuf,
    /// The name as edited so far.
    pub name: String,
    /// Set once the edit box has taken focus with the name selected, which
    /// happens the first frame it's drawn.
    pub focused: bool,
}

/// A new folder or file being named in the Files panel, before it's made
/// (see [State::start_create]).
pub struct FileCreate {
    /// A folder, rather than an empty file.
    pub folder: bool,
    /// The name as typed so far.
    pub name: String,
    /// Set once the edit box has taken focus, the first frame it's drawn.
    pub focused: bool,
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
    /// SSH connection per tab (parallel to `cwd`): being made, or live.
    /// While live, that tab's typed commands run on the remote. Not
    /// persisted (a connection can't outlive the process).
    pub remote: Vec<Option<Remote>>,
    /// The shell command running in each tab, if any (parallel to `cwd`).
    /// While set, that tab's typed lines go to its stdin. See [RunningCmd].
    pub running: Vec<Option<RunningCmd>>,
    /// In the current dir. Note persistent, unlike some of our other lists.
    pub browser_files: Vec<BrowserFile>,
    /// The dir `browser_files` was last read from, so a refresh can tell a
    /// move to a new dir (which clears the Files filter) from a re-read.
    pub browser_dir: PathBuf,
    /// The selected Files row last clicked or moved to, which Enter, the
    /// arrow keys and renaming act on, with the details its lower pane shows.
    /// Always one of `selection`'s rows. Re-read whenever `browser_files` is,
    /// and cleared once the path is no longer in the listing (e.g. after a
    /// cd or tab switch).
    pub selected_file: Option<FileDetails>,
    /// Every selected Files row (see [State::select_file] and co.).
    pub selection: FileSelection,
    /// What kind of project `browser_dir` is, for the Files panel's project
    /// rows. Re-detected along with `browser_files`.
    pub project: Project,
    /// The picture under `selected_file`'s details, when it's an image. Not
    /// persisted.
    pub file_preview: ImagePreview,
    /// The Files panel's Deep search for `ui.deep_search`, running or done.
    /// While set, the panel lists its results instead of `browser_files`.
    pub deep_search: Option<DeepSearch>,
    /// A Files-panel paste in progress (see [State::paste_files]).
    pub paste: Option<Paste>,
    /// Confirmed Files-panel deletion running in the background.
    pub deleting: Option<Delete>,
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
    /// Max displayed path length (in characters) in the Bookmarks and Recent
    /// dirs panels, set via the controls at the top of each and persisted
    /// alongside the font sizes. Longer paths are truncated from the front.
    pub bookmark_label_len: usize,
    pub recent_dir_label_len: usize,
    /// Whether the Bookmarks and Recent dirs panels show just each dir's
    /// final name ("Names") rather than its path ("Paths"). Persisted with the
    /// path lengths.
    pub bookmark_names: bool,
    pub recent_dir_names: bool,
    /// Whether each Cmd history row shows the "In <dir>" button that re-runs
    /// it where it was first run. Off by default; persisted.
    pub cmd_history_show_dir: bool,
    /// Whether the window is dark, set by the theme button on the tab strip.
    /// `None` until first toggled, meaning follow the OS theme. Persisted.
    pub dark_mode: Option<bool>,
    /// Open Markdown and text viewer windows (double-clicked in the Files
    /// panel).
    /// Not persisted.
    pub file_viewers: FileViewers,
    /// The PATH panel's lists and their unsaved edits. Not persisted.
    pub path_panel: PathPanel,
    /// For background work (an SSH connection) to wake the UI with. Set by
    /// the first frame.
    pub egui_ctx: Option<egui::Context>,
}

/// A saved Bookmarks / Recent dirs path length, or the default when none was
/// saved, clamped to the range the panels' controls allow.
fn dir_label_len(saved: Option<usize>) -> usize {
    saved
        .unwrap_or(ui::dirs::DEFAULT_DIR_LABEL_LEN)
        .clamp(ui::dirs::MIN_DIR_LABEL_LEN, ui::dirs::MAX_DIR_LABEL_LEN)
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
            remote: vec![None],
            running: vec![None],
            browser_files: Vec::new(),
            browser_dir: PathBuf::new(),
            selected_file: None,
            selection: FileSelection::default(),
            project: Project::default(),
            file_preview: ImagePreview::default(),
            deep_search: None,
            paste: None,
            deleting: None,
            state_path: save_data::default_path()
                .unwrap_or_else(|| PathBuf::from(save_data::FILENAME)),
            branch: vec![branch],
            window_size: None,
            font_size: ui::DEFAULT_FONT_SIZE,
            gui_font_size: ui::DEFAULT_GUI_FONT_SIZE,
            bookmark_label_len: ui::dirs::DEFAULT_DIR_LABEL_LEN,
            recent_dir_label_len: ui::dirs::DEFAULT_DIR_LABEL_LEN,
            bookmark_names: true,
            recent_dir_names: true,
            cmd_history_show_dir: false,
            dark_mode: None,
            file_viewers: FileViewers::default(),
            path_panel: PathPanel::default(),
            egui_ctx: None,
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
        match self.remote_here() {
            Some(Remote::Connecting(c)) => return format!("{} (connecting…)", c.label()),
            Some(Remote::Live(l)) => {
                return format!("{}:{}", l.session.label(), l.session.display_cwd());
            }
            None => {}
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

    /// Whether the active tab has an SSH connection, live or being made
    /// (used by the input row to suppress the local git-branch label while
    /// remote).
    pub fn is_remote(&self) -> bool {
        self.remote_here().is_some()
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
                .map(|b| format!(" \x1b[95mbr: {}\x1b[0m", truncate_branch(b, 10)))
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
    /// points as [refresh_branch]. Entries the `show_hidden_attr` /
    /// `show_dotfiles` filters exclude are dropped, which also deselects
    /// now-filtered rows. An entry that is both Hidden and a dotfile (e.g.
    /// `.git`) shows if either filter is on. Moving to a different dir
    /// clears the text filter and Deep search, which only make sense for the
    /// dir they were typed in.
    pub fn refresh_browser_files(&mut self) {
        let cwd = self.cwd().to_path_buf();
        if cwd != self.browser_dir {
            self.ui.files_search.clear();
            self.ui.rename = None;
            self.ui.rename_pending = None;
            self.ui.create = None;
            self.ui.deep_search.clear();
            self.ui.deep_search_edited = None;
            self.deep_search = None;
        }
        self.browser_files = read_browser_files(&cwd);
        self.project = Project::detect(&cwd);
        self.browser_dir = cwd;
        let (attr, dot) = (self.ui.show_hidden_attr, self.ui.show_dotfiles);
        self.browser_files.retain(|f| {
            let is_dot = f.disp_name.starts_with('.');
            (!f.hidden_attr && !is_dot) || (attr && f.hidden_attr) || (dot && is_dot)
        });
        sort_browser_files(&mut self.browser_files, self.ui.files_sort);
        self.refresh_browser_files_filter();
    }

    /// Reorder the Files-panel listing by `sort`, as its sort buttons do.
    pub fn set_files_sort(&mut self, sort: FileSort) {
        self.ui.files_sort = sort;
        sort_browser_files(&mut self.browser_files, sort);
        self.refresh_browser_files_filter();
    }

    /// Case-insensitive substring match of the Files-panel text filter
    /// against each listed entry's name. Empty search matches everything.
    /// Like the hidden/dotfile filters, hiding a selected row deselects it
    /// (and cancels renaming it), unless Deep search results are shown
    /// instead and it's one of those (see [Self::refresh_selection]).
    pub fn refresh_browser_files_filter(&mut self) {
        let needle = self.ui.files_search.to_lowercase();

        self.ui.browser_files_filter = self
            .browser_files
            .iter()
            .enumerate()
            .filter(|(_, f)| needle.is_empty() || f.disp_name.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();

        self.refresh_selection();
        // Likewise an in-place rename of a row that's gone (deleted by a
        // command, say) is abandoned.
        if let Some(r) = &self.ui.rename {
            let shown = self
                .ui
                .browser_files_filter
                .iter()
                .any(|&i| self.browser_files[i].path == r.path);
            if !shown {
                self.ui.rename = None;
            }
        }
    }

    /// The rows the Files panel shows, in order: the listing's, or while
    /// Deep search results stand in for it, the files and folders found
    /// that the Search box doesn't hide (as [ui::deep_search_results] shows
    /// them).
    pub fn shown_files(&self) -> Vec<&Path> {
        match &self.deep_search {
            Some(search) => {
                let needle = self.ui.files_search.to_lowercase();
                search
                    .hits
                    .iter()
                    .filter(|hit| {
                        needle.is_empty()
                            || hit
                                .path
                                .strip_prefix(&search.root)
                                .unwrap_or(&hit.path)
                                .to_string_lossy()
                                .to_lowercase()
                                .contains(&needle)
                    })
                    .map(|hit| hit.path.as_path())
                    .collect()
            }
            None => self
                .ui
                .browser_files_filter
                .iter()
                .map(|&i| self.browser_files[i].path.as_path())
                .collect(),
        }
    }

    /// The selected Files rows, in the order they're shown.
    pub fn selected_paths(&self) -> Vec<PathBuf> {
        self.shown_files()
            .into_iter()
            .filter(|p| self.selection.paths.contains(*p))
            .map(Path::to_path_buf)
            .collect()
    }

    /// Select just the Files-panel row for `path` and read its details, as
    /// a plain click does. Leaves nothing selected if `path` isn't shown
    /// (see [Self::shown_files]).
    pub fn select_file(&mut self, path: &Path) {
        self.select_files(&[path.to_path_buf()]);
    }

    /// Select just the Files rows for those of `paths` that are shown, the
    /// first of them leading (see [Self::selected_file]).
    pub fn select_files(&mut self, paths: &[PathBuf]) {
        let paths: Vec<PathBuf> = {
            let shown: HashSet<&Path> = self.shown_files().into_iter().collect();
            paths
                .iter()
                .filter(|p| shown.contains(p.as_path()))
                .cloned()
                .collect()
        };
        self.selection.anchor = paths.first().cloned();
        self.selection.paths = paths.iter().cloned().collect();
        self.set_lead(paths.first().map(PathBuf::as_path));
    }

    /// Add the Files row for `path` to the selection, or take it out if it's
    /// in, as Ctrl+click does. Shift+clicks then select from it.
    pub fn toggle_file(&mut self, path: &Path) {
        self.selection.anchor = Some(path.to_path_buf());
        if self.selection.paths.remove(path) {
            // The lead must be selected, so the first row left takes over.
            let was_lead = self.selected_file.as_ref().is_some_and(|d| d.path == path);
            if was_lead {
                let next = self.selected_paths().into_iter().next();
                self.set_lead(next.as_deref());
            } else {
                self.refresh_summary();
            }
        } else if self.shown_files().contains(&path) {
            self.selection.paths.insert(path.to_path_buf());
            self.set_lead(Some(path));
        }
    }

    /// Select the Files rows from the anchor (see [FileSelection::anchor])
    /// to `path`, leading with `path`, as Shift+click does. With `add`
    /// (Ctrl+Shift+click), they're added to what's selected already;
    /// otherwise they replace it.
    pub fn select_range(&mut self, path: &Path, add: bool) {
        let range: Vec<PathBuf> = {
            let shown = self.shown_files();
            let Some(to) = shown.iter().position(|p| *p == path) else {
                return;
            };
            // With no anchor shown, this row becomes it.
            let from = self
                .selection
                .anchor
                .as_deref()
                .and_then(|a| shown.iter().position(|p| *p == a));
            let from = from.unwrap_or(to);
            shown[from.min(to)..=from.max(to)]
                .iter()
                .map(|p| p.to_path_buf())
                .collect()
        };
        if range.len() == 1 {
            self.selection.anchor = Some(path.to_path_buf());
        }
        if !add {
            self.selection.paths.clear();
        }
        self.selection.paths.extend(range);
        self.set_lead(Some(path));
    }

    /// Select every Files row shown, as Ctrl+A does. The lead stays where
    /// it was.
    pub fn select_all_files(&mut self) {
        let shown: Vec<PathBuf> = self
            .shown_files()
            .into_iter()
            .map(Path::to_path_buf)
            .collect();
        let lead = self
            .selected_file
            .as_ref()
            .map(|d| d.path.clone())
            .or_else(|| shown.first().cloned());
        if self.selection.anchor.is_none() {
            self.selection.anchor = lead.clone();
        }
        self.selection.paths = shown.into_iter().collect();
        self.set_lead(lead.as_deref());
    }

    /// Select no Files rows, as a click on the listing's background does.
    pub fn clear_file_selection(&mut self) {
        self.selection = FileSelection::default();
        self.selected_file = None;
    }

    /// Deselect Files rows that are no longer shown, and re-read the details
    /// of the lead (or if it's gone, of the first row left) and the totals.
    fn refresh_selection(&mut self) {
        let mut paths = std::mem::take(&mut self.selection.paths);
        if !paths.is_empty() {
            let shown: HashSet<&Path> = self.shown_files().into_iter().collect();
            paths.retain(|p| shown.contains(p.as_path()));
        }
        self.selection.paths = paths;
        let lead = self
            .selected_file
            .as_ref()
            .map(|d| d.path.clone())
            .filter(|p| self.selection.paths.contains(p))
            .or_else(|| self.selected_paths().into_iter().next());
        self.set_lead(lead.as_deref());
    }

    /// Make `path`, one of the selected rows, the lead (see
    /// [Self::selected_file]), reading its details, then total up the
    /// selection. A row that's vanished since it was listed is deselected.
    fn set_lead(&mut self, path: Option<&Path>) {
        self.selected_file = path.and_then(|path| match &self.deep_search {
            Some(_) => FileDetails::read_file(path),
            None => self
                .browser_files
                .iter()
                .find(|f| f.path == path)
                .and_then(FileDetails::read),
        });
        if let Some(path) = path
            && self.selected_file.is_none()
        {
            self.selection.paths.remove(path);
        }
        self.refresh_summary();
    }

    /// Re-read [FileSelection::summary], which the details pane shows while
    /// more than one row is selected.
    fn refresh_summary(&mut self) {
        self.selection.summary = (self.selection.paths.len() > 1)
            .then(|| SelectionSummary::read(&self.selected_paths()));
    }

    /// Start a Deep search of the active tab's cwd for `ui.deep_search`,
    /// replacing (and stopping) any earlier one; an empty search just ends
    /// it, returning the panel to the dir listing. `ctx` is repainted as
    /// results come in. The selection is kept only if it's still shown. A
    /// search moves the dir to the top of Recent dirs.
    pub fn start_deep_search(&mut self, ctx: &egui::Context) {
        if !self.ui.deep_search.is_empty() {
            let cwd = self.cwd().to_path_buf();
            self.touch_recent_dir(&cwd);
        }
        let query = &self.ui.deep_search;
        self.deep_search = (!query.is_empty()).then(|| {
            DeepSearch::start(
                self.cwd().to_path_buf(),
                query,
                self.ui.deep_search_opts,
                ctx.clone(),
            )
        });
        self.ui.deep_search_edited = None;
        self.ui.rename = None;
        self.ui.rename_pending = None;
        self.refresh_selection();
    }

    /// Swap the Files-panel row for `path` to an edit box holding its name.
    pub fn start_rename(&mut self, path: &Path) {
        self.ui.rename = self
            .browser_files
            .iter()
            .find(|f| f.path == path)
            .map(|f| FileRename {
                path: f.path.clone(),
                name: f.disp_name.clone(),
                focused: false,
            });
    }

    /// Finish an in-place rename, applying the edited name. A blank or
    /// unchanged name leaves the entry as it was; a failure is reported in
    /// the output pane. The renamed entry stays selected if it was, and its
    /// dir moves to the top of Recent dirs.
    pub fn commit_rename(&mut self) {
        let Some(r) = self.ui.rename.take() else {
            return;
        };
        if r.name.trim().is_empty() {
            return;
        }
        match crate::files::rename_entry(&r.path, &r.name) {
            Ok(new_path) if new_path != r.path => {
                let selected = self
                    .selected_file
                    .as_ref()
                    .is_some_and(|d| d.path == r.path);
                self.refresh_browser_files();
                if selected {
                    self.select_file(&new_path);
                }
                if let Some(dir) = new_path.parent() {
                    self.touch_recent_dir(dir);
                }
            }
            Ok(_) => {}
            Err(e) => self.push_out(
                format!("rename: can't rename {}: {e}", r.path.display()),
                OutType::StdErr,
            ),
        }
    }

    /// Start naming a new folder (`folder`) or empty file for the Files
    /// panel's dir, in an edit box above the listing (see
    /// `ui::create_box`). A rename in progress is applied first.
    pub fn start_create(&mut self, folder: bool) {
        self.commit_rename();
        self.ui.rename_pending = None;
        self.ui.create = Some(FileCreate {
            folder,
            name: String::new(),
            focused: false,
        });
    }

    /// Finish naming a new folder or file, making it in the Files panel's
    /// dir as `mkdir` or `touch` would (see [crate::files::create_entry]). A
    /// blank name makes nothing; a failure is reported in the output pane.
    /// The new entry is selected, and its dir moves to the top of Recent
    /// dirs.
    pub fn commit_create(&mut self) {
        let Some(c) = self.ui.create.take() else {
            return;
        };
        if c.name.trim().is_empty() {
            return;
        }
        let dir = self.cwd().to_path_buf();
        match crate::files::create_entry(&dir, &c.name, c.folder) {
            Ok(path) => {
                self.refresh_browser_files();
                self.select_file(&path);
                self.touch_recent_dir(&dir);
            }
            Err(e) => {
                let cmd = if c.folder { "mkdir" } else { "touch" };
                self.push_out(
                    format!(
                        "{cmd}: can't create {}: {e}",
                        dir.join(c.name.trim()).display()
                    ),
                    OutType::StdErr,
                );
            }
        }
    }

    /// Put the selected Files rows on the OS clipboard, to be copied (or
    /// with `cut`, moved) wherever they're pasted: here, or in the OS file
    /// manager. Their dir moves to the top of Recent dirs.
    pub fn copy_selected(&mut self, cut: bool) {
        let paths = self.selected_paths();
        let Some(first) = paths.first() else {
            return;
        };
        if let Some(dir) = first.parent() {
            self.touch_recent_dir(dir);
        }
        let content = clipboard::Content::Files {
            paths: paths.clone(),
            cut,
        };
        self.ui.clipboard.sys.set(content.clone());
        self.ui.clipboard.history.record(content);
        if let Err(e) = clipboard::set_files(&paths, cut) {
            let cmd = if cut { "cut" } else { "copy" };
            self.push_out(format!("{cmd}: {e}"), OutType::StdErr);
        }
    }

    /// Copy the files on the OS clipboard into the active tab's cwd, or move
    /// them if they were cut. Runs in the background; [Self::pump_paste]
    /// picks up the outcome. Ignored while an earlier paste is still going.
    /// The cwd moves to the top of Recent dirs.
    pub fn paste_files(&mut self, ctx: &egui::Context) {
        if self.paste.is_some() || self.deleting.is_some() {
            return;
        }
        match clipboard::get_files() {
            Ok(Some((srcs, cut))) => {
                // Files the watcher already knows keep their entry's time.
                let content = clipboard::Content::Files {
                    paths: srcs.clone(),
                    cut,
                };
                if !self.ui.clipboard.sys.holds(&content) {
                    self.ui.clipboard.sys.set(content.clone());
                    self.ui.clipboard.history.record(content);
                }
                let dest = self.cwd().to_path_buf();
                self.touch_recent_dir(&dest);
                self.paste = Some(Paste::start(srcs, dest, cut, ctx.clone()));
            }
            Ok(None) => self.push_out(
                "paste: there are no files on the clipboard",
                OutType::StdErr,
            ),
            Err(e) => self.push_out(format!("paste: {e}"), OutType::StdErr),
        }
    }

    /// Move (or with `copy`, copy) `srcs` into the folder `dest`, as dropping
    /// Files rows on a folder does. Runs in the background like a paste
    /// ([Self::pump_paste] picks up the outcome too), so it's refused while
    /// one is going. Dropping a folder on itself does nothing. The cwd moves
    /// to the top of Recent dirs.
    pub fn drop_files(&mut self, srcs: &[PathBuf], dest: &Path, copy: bool, ctx: &egui::Context) {
        if self.paste.is_some() || self.deleting.is_some() {
            let what = if copy { "copy" } else { "move" };
            self.push_out(
                format!("{what}: wait for the file operation in progress to finish"),
                OutType::StdErr,
            );
            return;
        }
        let srcs: Vec<PathBuf> = srcs.iter().filter(|p| *p != dest).cloned().collect();
        if srcs.is_empty() {
            return;
        }
        let cwd = self.cwd().to_path_buf();
        self.touch_recent_dir(&cwd);
        self.paste = Some(Paste::start(srcs, dest.to_path_buf(), !copy, ctx.clone()));
    }

    /// Once a paste (or drop) has finished: report what failed, re-read the
    /// listing, and select what was pasted, if it's here. Pasted cut files
    /// are taken off the clipboard, since they've moved. Called every frame.
    pub fn pump_paste(&mut self) {
        let Some(results) = self.paste.as_mut().and_then(Paste::try_finish) else {
            return;
        };
        let paste = self.paste.take().unwrap();
        let mut pasted = Vec::new();
        let mut moved = Vec::new();
        for (src, r) in paste.srcs.iter().zip(results) {
            match r {
                Ok(path) => {
                    if paste.moving && *src != path {
                        moved.push(src.clone());
                    }
                    pasted.push(path);
                }
                Err(e) => self.push_out(format!("paste: {e}"), OutType::StdErr),
            }
        }
        self.ui.clipboard.history.forget_paths(&moved);
        // Even a partial move makes the old OS file list unsafe to reuse.
        if !moved.is_empty() {
            clipboard::clear_if_holds(&paste.srcs);
        }
        self.refresh_browser_files();
        // What's dropped in a subfolder isn't here to select, so the
        // selection is left as it is.
        let here: Vec<PathBuf> = {
            let shown: HashSet<&Path> = self.shown_files().into_iter().collect();
            pasted
                .into_iter()
                .filter(|p| shown.contains(p.as_path()))
                .collect()
        };
        if !here.is_empty() {
            self.select_files(&here);
        }
    }

    /// Capture the selection before showing the confirmation dialog.
    pub fn request_delete_selected(&mut self) {
        if self.paste.is_some() || self.deleting.is_some() || self.ui.delete_pending.is_some() {
            return;
        }
        self.commit_rename();
        let paths = self.selected_paths();
        if !paths.is_empty() {
            self.ui.rename_pending = None;
            self.ui.delete_pending = Some(paths);
        }
    }

    /// Move only the paths captured by the confirmation to the system trash.
    pub fn confirm_delete(&mut self, ctx: &egui::Context) {
        if self.paste.is_some() || self.deleting.is_some() {
            return;
        }
        if let Some(paths) = self.ui.delete_pending.take() {
            self.deleting = Some(Delete::start(paths, ctx.clone()));
        }
    }

    /// Refresh the listing and clipboard after trashing, keeping failed rows
    /// selected so they can be retried. Called every frame, even if hidden.
    pub fn pump_delete(&mut self, ctx: &egui::Context) {
        let Some(operation) = &mut self.deleting else {
            return;
        };
        let Some(results) = operation.try_finish() else {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
            return;
        };
        let operation = self.deleting.take().unwrap();
        let mut deleted = Vec::new();
        for (path, result) in operation.paths.into_iter().zip(results) {
            match result {
                Ok(()) => deleted.push(path),
                Err(e) => {
                    self.push_out(format!("delete: {}: {e}", path.display()), OutType::StdErr)
                }
            }
        }
        self.ui.clipboard.history.forget_paths(&deleted);
        if let Ok(Some((held, _))) = clipboard::get_files()
            && held
                .iter()
                .any(|p| deleted.iter().any(|d| p.starts_with(d)))
        {
            clipboard::clear_if_holds(&held);
        }
        // Search results can still contain a deleted path; explicitly remove
        // those selections before refreshing or restarting the search.
        self.selection
            .paths
            .retain(|p| !deleted.iter().any(|d| p.starts_with(d)));
        if self.deep_search.is_some() {
            self.start_deep_search(ctx);
        }
        self.refresh_browser_files();
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
            DirLabelLens {
                bookmarks: Some(self.bookmark_label_len),
                recent_dirs: Some(self.recent_dir_label_len),
                bookmark_names: Some(self.bookmark_names),
                recent_dir_names: Some(self.recent_dir_names),
            },
            self.ui.panel_widths,
            FilesPanelPrefs {
                show_hidden: Some(self.ui.show_hidden_attr),
                show_dotfiles: Some(self.ui.show_dotfiles),
                deep_search_names: Some(self.ui.deep_search_opts.file_names),
            },
            PaneSplits {
                files: Some(self.ui.files_details_split),
                path: Some(self.ui.path_details_split),
            },
            CmdHistoryPrefs {
                show_dir: Some(self.cmd_history_show_dir),
            },
            Appearance {
                dark_mode: self.dark_mode,
                dir_color_depth: Some(path_color::depth_levels()),
            },
            &self.ui.clipboard.history.entries,
            &self.state_path,
        )
    }

    /// Adjust the terminal-pane font size by `delta` points, clamped to a
    /// sane range, and persist the change. Wired to the "CLI font size"
    /// drag control on the tab strip.
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

    /// Step how many directory levels the path colours take to go from
    /// purple to yellow (see [path_color::depth_levels]) by `delta`, and
    /// persist the change. Wired to the "Dir depth" +/- buttons on the tab
    /// strip.
    pub fn adjust_dir_color_depth(&mut self, delta: i32) {
        let cur = path_color::depth_levels();
        let next = (cur as i32 + delta).clamp(
            path_color::MIN_DEPTH_LEVELS as i32,
            path_color::MAX_DEPTH_LEVELS as i32,
        ) as u8;
        if next != cur {
            path_color::set_depth_levels(next);
            let _ = self.save();
        }
    }

    /// Switch the window between dark and light, and persist the choice.
    /// Wired to the theme button on the tab strip.
    pub fn set_dark_mode(&mut self, ctx: &egui::Context, dark: bool) {
        self.dark_mode = Some(dark);
        ctx.set_theme(theme(dark));
        let _ = self.save();
    }

    pub fn load(path: PathBuf) -> io::Result<Self> {
        let loaded = save_data::load_state(&path)?;
        path_color::set_depth_levels(
            loaded
                .appearance
                .dir_color_depth
                .unwrap_or(path_color::DEFAULT_DEPTH_LEVELS),
        );

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
        ui.panel_widths = loaded.panel_widths;
        let prefs = loaded.files_panel;
        ui.show_hidden_attr = prefs.show_hidden.unwrap_or(ui.show_hidden_attr);
        ui.show_dotfiles = prefs.show_dotfiles.unwrap_or(ui.show_dotfiles);
        ui.deep_search_opts.file_names = prefs
            .deep_search_names
            .unwrap_or(ui.deep_search_opts.file_names);
        let splits = loaded.pane_splits;
        ui.files_details_split = splits.files.unwrap_or(ui.files_details_split);
        ui.path_details_split = splits.path.unwrap_or(ui.path_details_split);
        ui.active_tab = active_tab;
        ui.cli_input = vec![String::new(); n];
        ui.out = (0..n).map(|_| Output::default()).collect();
        ui.history_nav = (0..n).map(|_| NavState::new()).collect();
        ui.nav_back = (0..n).map(|_| Vec::new()).collect();
        ui.nav_forward = (0..n).map(|_| Vec::new()).collect();
        ui.clipboard.history = clipboard::History::from_saved(loaded.clipboard);
        // The OS clipboard most likely still holds the newest entry. Saying
        // so means the watcher's first look doesn't record it again with a
        // new time; if it holds something else, the watcher corrects this.
        if let Some(newest) = ui.clipboard.history.entries.first() {
            ui.clipboard.sys.set(newest.content.clone());
        }

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
            remote: (0..n).map(|_| None).collect(),
            running: (0..n).map(|_| None).collect(),
            browser_files: Vec::new(),
            browser_dir: PathBuf::new(),
            selected_file: None,
            selection: FileSelection::default(),
            project: Project::default(),
            file_preview: ImagePreview::default(),
            deep_search: None,
            paste: None,
            deleting: None,
            state_path: path,
            branch,
            window_size: loaded.window_size,
            font_size: loaded.font_size.cli.unwrap_or(ui::DEFAULT_FONT_SIZE),
            gui_font_size: loaded.font_size.gui.unwrap_or(ui::DEFAULT_GUI_FONT_SIZE),
            bookmark_label_len: dir_label_len(loaded.dir_label_lens.bookmarks),
            recent_dir_label_len: dir_label_len(loaded.dir_label_lens.recent_dirs),
            bookmark_names: loaded.dir_label_lens.bookmark_names.unwrap_or(true),
            recent_dir_names: loaded.dir_label_lens.recent_dir_names.unwrap_or(true),
            cmd_history_show_dir: loaded.cmd_history.show_dir.unwrap_or(false),
            dark_mode: loaded.appearance.dark_mode,
            file_viewers: FileViewers::default(),
            path_panel: PathPanel::default(),
            egui_ctx: None,
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
        self.push_out_to(self.ui.active_tab, text, type_);
    }

    /// Empty the active tab's output pane, as `clear` does in a terminal.
    pub fn clear_out(&mut self) {
        let i = self.ui.active_tab;
        self.ui.out[i].clear();
        if let Some(cmd) = self.running[i].as_mut() {
            cmd.out_cleared();
        }
        if let Some(live) = self.remote[i].as_mut().and_then(Remote::live_mut) {
            live.out_cleared();
        }
    }

    /// [Self::push_out] to tab `i`, which needn't be the active one.
    pub fn push_out_to(&mut self, i: usize, text: impl Into<String>, type_: OutType) {
        self.ui.out[i].push(&text.into(), type_);
    }

    /// Open a new tab inheriting the active tab's cwd. Switches focus to it.
    pub fn add_tab(&mut self) {
        let tab = DetachedTab::new(self.cwd.len(), self.cwd().to_path_buf());
        // New tab inherits the active tab's branch (same cwd → same repo).
        let inherited_branch = self.branch[self.ui.active_tab].clone();
        self.insert_tab(tab, inherited_branch);

        self.refresh_cwd_history_filter();
        self.refresh_browser_files();
    }

    /// Put `tab` into the strip at its index (clamped to the end) and make it
    /// the active tab. The caller refreshes whatever follows the cwd.
    fn insert_tab(&mut self, tab: DetachedTab, branch: Option<String>) {
        let i = tab.index.min(self.cwd.len());

        self.cwd.insert(i, tab.cwd);
        self.ui.cli_input.insert(i, tab.cli_input);
        self.ui.out.insert(i, tab.out);
        self.ui.history_nav.insert(i, NavState::new());
        self.ui.nav_back.insert(i, tab.nav_back);
        self.ui.nav_forward.insert(i, tab.nav_forward);
        self.branch.insert(i, branch);
        // Tabs go in local and idle; a connection isn't inherited.
        self.remote.insert(i, None);
        self.running.insert(i, None);

        self.ui.active_tab = i;
        self.ui.focus_input = true;
    }

    /// Bring back the most recently closed tab, where it was, with its
    /// output, input, and Back/Forward history. Skips any whose dir has gone
    /// since. No-op when there's none.
    pub fn reopen_closed_tab(&mut self) {
        while let Some(tab) = self.ui.closed_tabs.pop() {
            if !tab.cwd.is_dir() {
                continue;
            }
            let _ = env::set_current_dir(&tab.cwd);
            let branch = current_branch(&tab.cwd);
            self.insert_tab(tab, branch);

            self.refresh_cwd_history_filter();
            self.refresh_browser_files();
            return;
        }
    }

    /// Move the tab at `from` to `to`, shifting the tabs between over by one.
    /// The active tab stays active wherever it lands.
    pub fn move_tab(&mut self, from: usize, to: usize) {
        let n = self.cwd.len();
        if from >= n || to >= n || from == to {
            return;
        }

        fn shift<T>(v: &mut Vec<T>, from: usize, to: usize) {
            let item = v.remove(from);
            v.insert(to, item);
        }
        shift(&mut self.cwd, from, to);
        shift(&mut self.ui.cli_input, from, to);
        shift(&mut self.ui.out, from, to);
        shift(&mut self.ui.history_nav, from, to);
        shift(&mut self.ui.nav_back, from, to);
        shift(&mut self.ui.nav_forward, from, to);
        shift(&mut self.branch, from, to);
        shift(&mut self.remote, from, to);
        shift(&mut self.running, from, to);

        self.ui.active_tab = moved_tab_index(self.ui.active_tab, from, to);
    }

    /// Close a tab, keeping it for [Self::reopen_closed_tab]. No-op if it's
    /// the last remaining one (we keep at least one tab open so the
    /// invariant holds).
    pub fn close_tab(&mut self, idx: usize) {
        if self.cwd.len() <= 1 || idx >= self.cwd.len() {
            return;
        }
        let closed = DetachedTab {
            index: idx,
            cwd: self.cwd.remove(idx),
            cli_input: self.ui.cli_input.remove(idx),
            out: self.ui.out.remove(idx),
            nav_back: self.ui.nav_back.remove(idx),
            nav_forward: self.ui.nav_forward.remove(idx),
        };
        self.ui.history_nav.remove(idx);
        self.branch.remove(idx);
        // Tear down any SSH session attached to the closed tab.
        if let Some(Remote::Live(live)) = self.remote.remove(idx) {
            live.session.disconnect();
        }
        // Dropping a command still running there stops it.
        self.running.remove(idx);

        let closed_tabs = &mut self.ui.closed_tabs;
        closed_tabs.push(closed);
        if closed_tabs.len() > CLOSED_TABS_MAX {
            closed_tabs.remove(0);
        }
        // Preserve reopen/navigation history, but release older snapshots'
        // scrollback once their combined allocation exceeds the byte budget.
        trim_closed_output(closed_tabs);

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

    /// Move `dir` to the top of Recent dirs, adding it if it isn't there yet.
    /// Done for the dir a command runs in (see `run_command`), and by
    /// [Self::touch_recent_dir]. Not saved.
    pub fn record_recent_dir(&mut self, dir: &Path) {
        self.recent_dirs.retain(|r| r.path != dir);
        self.recent_dirs.push(RecentDir {
            path: dir.to_path_buf(),
            dt: Utc::now(),
        });
        self.refresh_recent_dirs_filter();
    }

    /// [Self::record_recent_dir] and save, for a dir something was done in
    /// from the Files panel: a file opened, copied, renamed or pasted there,
    /// a Deep search of it, or it bookmarked or shown in the OS file manager.
    /// Just passing through a dir doesn't count, so Recent dirs isn't
    /// cluttered with every dir browsed on the way.
    pub fn touch_recent_dir(&mut self, dir: &Path) {
        self.record_recent_dir(dir);
        let _ = self.save();
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
        self.toggle_bookmark(self.cwd().to_path_buf())
    }

    /// Toggle `dir` in the bookmark list, as the Files and Recent dirs ★
    /// buttons do. Returns the message, as [Self::toggle_bookmark_cwd] does.
    pub fn toggle_bookmark(&mut self, dir: PathBuf) -> String {
        let msg = if let Some(pos) = self.dir_bookmarks.iter().position(|p| p == &dir) {
            self.dir_bookmarks.remove(pos);
            format!("Removed bookmark: {}", dir.display())
        } else {
            self.dir_bookmarks.push(dir.clone());
            format!("Added a bookmark: {}", dir.display())
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

    /// Complete the active input row, as the CLI's Tab does (see
    /// [completion::complete]): extend it to the candidates' shared prefix,
    /// or, when that adds nothing, list the candidates on a second Tab.
    pub fn autocomplete_input(&mut self) {
        let i = self.ui.active_tab;
        let live = self.ui.cli_input[i].clone();
        let Some(result) = completion::complete(
            &live,
            live.len(),
            self.cwd(),
            self.home.as_deref(),
            &self.dir_bookmarks,
            &self.recent_dirs,
            &self.clone_roots,
        ) else {
            return;
        };
        // As in rustyline, only take the shared prefix when it adds to the
        // input: case-insensitive and fuzzy candidates can share less than
        // was typed.
        let applied = apply_completion(&live, live.len(), &result)
            .filter(|text| result.candidates.len() == 1 || text.len() > live.len());
        if let Some(text) = applied {
            self.ui.cli_input[i] = text;
            self.ui.tab_list_pending = None;
        } else if result.candidates.len() > 1 {
            if self.ui.tab_list_pending.as_deref() == Some(live.as_str()) {
                let prompt = self.prompt(&self.ui.history_nav[i]);
                self.push_out(format!("{prompt}> {live}"), OutType::Prompt);
                self.push_out(completion::candidate_list(&result), OutType::StdOut);
                self.ui.tab_list_pending = None;
            } else {
                self.ui.tab_list_pending = Some(live);
            }
        }
    }

    /// The greyed history suggestion for the active input row (see
    /// [completion::history_hint]); → accepts it.
    pub fn input_hint(&self) -> Option<String> {
        completion::history_hint(&self.history, &self.ui.cli_input[self.ui.active_tab])
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

    /// [Self::change_dir] to a dir picked from `from`: a Recent dirs or
    /// Bookmarks row, or a typed `cd <n>` / `bm <n>`. If the cd fails because
    /// the dir no longer exists, sets `stale_dir` so a popup offers to remove
    /// it from its list.
    pub fn change_saved_dir(&mut self, target: PathBuf, from: DirList) {
        self.change_dir(target.clone());
        if let Some(list) = stale_dir_list(&target, from, &self.dir_bookmarks, &self.recent_dirs) {
            self.ui.stale_dir = Some((target, list));
        }
    }

    /// Open a new tab in `target` and switch to it, as middle-clicking a
    /// folder does. Unlike [Self::change_dir], the new tab starts with no
    /// Back history. If `target` can't be entered, reports why and opens
    /// nothing.
    pub fn open_dir_in_new_tab(&mut self, target: PathBuf) {
        if let Err(e) = env::set_current_dir(&target) {
            self.push_out(format!("cd: {e}"), OutType::StdErr);
            return;
        }
        self.add_tab();
        self.navigate_to(target);
    }

    /// [Self::open_dir_in_new_tab] for a dir picked from `from` (a Recent dirs
    /// or Bookmarks row). If it no longer exists, offers to remove it from its
    /// list, as [Self::change_saved_dir] does.
    pub fn open_saved_dir_in_new_tab(&mut self, target: PathBuf, from: DirList) {
        self.open_dir_in_new_tab(target.clone());
        if let Some(list) = stale_dir_list(&target, from, &self.dir_bookmarks, &self.recent_dirs) {
            self.ui.stale_dir = Some((target, list));
        }
    }

    /// The stale-dir popup's Remove button: drop `stale_dir` from its list.
    pub fn remove_stale_dir(&mut self) {
        if let Some((path, list)) = self.ui.stale_dir.take() {
            self.remove_from_dir_list(&path, list);
        }
    }

    /// Remove `path` from `list` (the dir itself is untouched), report it in
    /// the output pane, and save.
    pub fn remove_from_dir_list(&mut self, path: &Path, list: DirList) {
        if !remove_saved_dir(list, path, &mut self.dir_bookmarks, &mut self.recent_dirs) {
            return;
        }
        self.refresh_bookmarks_filter();
        self.refresh_recent_dirs_filter();
        let shown = crate::render_with_tilde(path, self.home.as_deref());
        self.push_out(
            format!("Removed {shown} from {}", list.name()),
            OutType::StdOut,
        );
        self.save_or_warn();
    }

    /// The Bookmarks / Recent dirs "Cleanup" button: drop every dir in
    /// `list` that's gone from disk (see [dir_missing]), listing them in the
    /// output pane.
    pub fn cleanup_missing_dirs(&mut self, list: DirList) {
        let removed = match list {
            DirList::RecentDirs => remove_missing_recent_dirs(&mut self.recent_dirs),
            DirList::Bookmarks => self
                .dir_bookmarks
                .extract_if(.., |p| dir_missing(p))
                .collect(),
        };
        if removed.is_empty() {
            self.push_out(
                format!("No missing directories in {}", list.name()),
                OutType::StdOut,
            );
            return;
        }
        self.refresh_bookmarks_filter();
        self.refresh_recent_dirs_filter();

        let noun = if removed.len() == 1 { "dir" } else { "dirs" };
        let mut msg = format!(
            "Removed {} missing {noun} from {}:",
            removed.len(),
            list.name()
        );
        for path in &removed {
            msg.push_str("\n  ");
            msg.push_str(&crate::render_with_tilde(path, self.home.as_deref()));
        }
        self.push_out(msg, OutType::StdOut);
        self.save_or_warn();
    }

    /// [Self::save], reporting a failure in the output pane.
    fn save_or_warn(&mut self) {
        if let Err(e) = self.save() {
            self.push_out(
                format!("warning: failed to save state: {e}"),
                OutType::StdErr,
            );
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
    /// The command running in the active tab, if any.
    pub fn running_here(&self) -> Option<&RunningCmd> {
        self.running[self.ui.active_tab].as_ref()
    }

    /// Run `line` in the system shell in `dir`, in the background, with its
    /// output streaming into the active tab's pane. `clone_root` is saved to
    /// `clone_roots` if it succeeds.
    pub fn start_running(
        &mut self,
        line: &str,
        dir: &Path,
        clone_root: Option<String>,
        ctx: &egui::Context,
    ) {
        let i = self.ui.active_tab;
        if self.running[i].is_some() {
            self.push_out(
                "a command is already running in this tab: stop it (Ctrl+C), or use another tab",
                OutType::StdErr,
            );
            return;
        }
        match RunningCmd::start(line, dir, &self.ui.out[i], ctx) {
            Ok(mut cmd) => {
                cmd.clone_root = clone_root;
                self.running[i] = Some(cmd);
            }
            Err(e) => self.push_out(format!("shell: {e}"), OutType::StdErr),
        }
    }

    /// Stop the command running in the active tab.
    pub fn stop_running(&mut self) {
        if let Some(cmd) = self.running[self.ui.active_tab].as_mut() {
            cmd.stop();
        }
    }

    /// Send a line typed into the active tab to its running command's stdin,
    /// echoing it into the pane as a terminal would.
    pub fn send_running_line(&mut self, line: &str) {
        let i = self.ui.active_tab;
        if let Some(cmd) = self.running[i].as_mut() {
            cmd.send_line(line);
            self.push_out(line, OutType::Prompt);
        }
    }

    /// Bring each tab's running command's new output into its pane, and wrap
    /// up any that have finished. Called once per frame.
    pub fn pump_running(&mut self) {
        for i in 0..self.running.len() {
            let Some(success) = self.running[i]
                .as_mut()
                .and_then(|cmd| cmd.poll(&mut self.ui.out[i]))
            else {
                continue;
            };
            let Some(cmd) = self.running[i].take() else {
                continue;
            };

            // A successful clone from a full URL saves the URL's root, so later
            // clones from the same place need only the repo name. Only on
            // success, so a mistyped URL isn't saved.
            if success && let Some(root) = cmd.clone_root.clone() {
                self.clone_roots.retain(|r| *r != root);
                self.clone_roots.push(root);
                if let Err(e) = self.save() {
                    self.push_out_to(
                        i,
                        format!("warning: failed to save state: {e}"),
                        OutType::StdErr,
                    );
                }
            }

            // The command may have switched branches or changed files. A
            // background tab gets both refreshed when it's selected.
            if i == self.ui.active_tab {
                self.refresh_branch();
                self.refresh_browser_files();
            }
        }
    }

    /// The active tab's SSH connection, if any.
    pub fn remote_here(&self) -> Option<&Remote> {
        self.remote[self.ui.active_tab].as_ref()
    }

    /// The active tab's SSH session, once connected.
    pub fn live_here(&self) -> Option<&LiveRemote> {
        self.remote_here().and_then(Remote::live)
    }

    pub fn live_here_mut(&mut self) -> Option<&mut LiveRemote> {
        self.remote[self.ui.active_tab]
            .as_mut()
            .and_then(Remote::live_mut)
    }

    /// Connect the active tab to the saved remote at `idx`.
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

    /// Start connecting the active tab, in the background: the window stays
    /// live, and [Self::pump_remotes] takes over once the connection is up
    /// (or has failed). SSH keys are tried, and the keyring's saved password
    /// when there is one. Replaces any connection the tab already has.
    pub fn connect_with(&mut self, host: String, port: u16, user: String) {
        let i = self.ui.active_tab;
        if self.running[i].is_some() {
            self.push_out(
                "ssh: a command is running in this tab: stop it (Ctrl+C), or use another tab",
                OutType::StdErr,
            );
            return;
        }
        let password = secrets::get_password(&host, port, &user);
        let ctx = self.egui_ctx.clone();
        let waker: ssh::Waker = Arc::new(move || {
            if let Some(ctx) = &ctx {
                ctx.request_repaint();
            }
        });
        let (cols, rows) = self.ui.term_size;
        self.push_out(
            format!("Connecting to {user}@{host}:{port} …"),
            OutType::Prompt,
        );
        let connecting = ssh::connect(
            ConnectRequest {
                host,
                port,
                user,
                password,
                cols,
                rows,
            },
            waker,
        );
        if let Some(Remote::Live(old)) = self.remote[i].replace(Remote::Connecting(connecting)) {
            old.session.disconnect();
        }
    }

    /// Disconnect the active tab, or stop it connecting.
    pub fn disconnect_remote(&mut self) {
        let i = self.ui.active_tab;
        match self.remote[i].take() {
            Some(Remote::Live(live)) => {
                live.session.disconnect();
                self.push_out("ssh: disconnected", OutType::StdOut);
            }
            Some(Remote::Connecting(_)) => self.push_out("ssh: cancelled", OutType::StdOut),
            None => {}
        }
    }

    /// Run a command line in the active tab's remote shell.
    pub fn run_remote(&mut self, line: &str) {
        if let Some(live) = self.live_here_mut() {
            live.run(line);
        }
    }

    /// Type a line into the program running on the active tab's remote.
    pub fn send_remote_line(&mut self, line: &str) {
        if let Some(live) = self.live_here_mut() {
            live.send_line(line);
        }
    }

    /// Ctrl+C for the program running on the active tab's remote.
    pub fn interrupt_remote(&mut self) {
        if let Some(live) = self.live_here() {
            live.session.interrupt();
        }
    }

    /// Bring in each tab's SSH news: a finished connection attempt, remote
    /// output, or the connection closing. Called once per frame. Returns
    /// whether more output is waiting, for another frame soon.
    pub fn pump_remotes(&mut self) -> bool {
        let mut more = false;
        let (cols, rows) = self.ui.term_size;
        for i in 0..self.remote.len() {
            match self.remote[i].as_mut() {
                None => {}
                Some(Remote::Connecting(c)) => {
                    let (host, port, user) = (c.host().to_string(), c.port(), c.user().to_string());
                    match c.poll() {
                        None => {}
                        Some(Ok(session)) => {
                            let label = session.label();
                            self.remote[i] = Some(Remote::Live(Box::new(LiveRemote::new(session))));
                            self.push_out_to(
                                i,
                                format!("Connected to {label}. Commands run there now; `exit` logs out."),
                                OutType::StdOut,
                            );
                            self.record_remote(&host, port, &user);
                        }
                        Some(Err(e)) => {
                            self.remote[i] = None;
                            self.push_out_to(i, format!("ssh: {e}"), OutType::StdErr);
                            if let ConnectError::Auth { password_tried } = e {
                                self.ask_for_password(i, host, port, user, password_tried);
                            }
                        }
                    }
                }
                Some(Remote::Live(live)) => {
                    live.resize(cols, rows);
                    let pumped = live.pump(&mut self.ui.out[i]);
                    more |= pumped.more;
                    if pumped.closed {
                        let label = live.session.label();
                        self.remote[i] = None;
                        self.push_out_to(
                            i,
                            format!("ssh: connection to {label} closed"),
                            OutType::StdOut,
                        );
                    }
                }
            }
        }
        more
    }

    /// After a login was turned down, open the Remote panel's form on that
    /// remote, for its password.
    fn ask_for_password(
        &mut self,
        tab: usize,
        host: String,
        port: u16,
        user: String,
        password_tried: bool,
    ) {
        let saved = self
            .remote_terminals
            .iter()
            .position(|r| r.host == host && r.port == port && r.username == user);
        self.ui.remote_host = host;
        self.ui.remote_port = port.to_string();
        self.ui.remote_user = user;
        self.ui.remote_password.clear();
        self.ui.remote_edit_idx = saved;
        self.ui.remote_form_open = true;
        self.ui.panel_vis.remote_terminals = true;
        let msg = if password_tried {
            "ssh: correct the password in the Remote panel, then Save & connect"
        } else {
            "ssh: enter the password in the Remote panel, then Save & connect"
        };
        self.push_out_to(tab, msg, OutType::StdOut);
    }

    /// Add a remote we've logged in to the saved list, as the CLI does.
    fn record_remote(&mut self, host: &str, port: u16, user: &str) {
        if self
            .remote_terminals
            .iter()
            .any(|r| r.host == host && r.port == port && r.username == user)
        {
            return;
        }
        self.remote_terminals.push(RemoteTerminal {
            host: host.to_string(),
            port,
            username: user.to_string(),
        });
        let _ = self.save();
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
    pub fn save_remote_from_form(&mut self) -> Result<(String, u16, String), String> {
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

        let entry = RemoteTerminal {
            host: host.clone(),
            port,
            username: user.clone(),
        };
        match self.ui.remote_edit_idx {
            Some(i) if i < self.remote_terminals.len() => self.remote_terminals[i] = entry,
            _ => {
                self.remote_terminals
                    .retain(|r| !(r.host == host && r.port == port && r.username == user));
                self.remote_terminals.push(entry);
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
        Ok((host, port, user))
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

/// The egui theme for a dark or light window.
pub fn theme(dark: bool) -> egui::Theme {
    if dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    }
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
        // Commands still running would otherwise carry on with nothing to
        // show their output or stop them.
        for cmd in &mut self.running {
            cmd.take();
        }
    }
}

#[cfg(test)]
mod memory_tests {
    use super::*;

    #[test]
    fn closed_tabs_keep_navigation_while_old_output_obeys_a_shared_budget() {
        let mut tabs: Vec<_> = (0..4).map(|i| DetachedTab::new(i, PathBuf::from("."))).collect();
        let text = "x".repeat(terminal::MAX_LINE_BYTES);
        for tab in &mut tabs {
            tab.nav_back.push(PathBuf::from("previous"));
            for _ in 0..260 { tab.out.push(&text, OutType::StdOut); }
        }
        assert!(tabs.iter().map(|t| t.out.retained_bytes()).sum::<usize>() > terminal::CLOSED_BYTES);
        trim_closed_output(&mut tabs);
        assert!(tabs.iter().map(|t| t.out.retained_bytes()).sum::<usize>() <= terminal::CLOSED_BYTES);
        assert!(tabs[0].out.lines.is_empty());
        assert!(!tabs.last().unwrap().out.lines.is_empty());
        assert!(tabs.iter().all(|t| t.nav_back == [PathBuf::from("previous")]));
    }
}
