//! The PATH panel: edits the PATH each [Scope] keeps (see `crate::path_env`)
//! one directory per row, like Windows' "Edit environment variable" dialog,
//! though PATH itself is a single string.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use egui::{
    Align, Button, CollapsingHeader, ComboBox, Frame, Key, Panel, RichText, ScrollArea, TextEdit,
    TextStyle, TextWrapMode, Ui, WidgetText,
    text::{CCursor, CCursorRange},
};

use super::{panel_header, plural};
use crate::{
    OutType,
    path_env::{self, DirPrograms, Scope},
    state::State,
};

/// What the list marks an entry with.
struct EntryStatus {
    /// The entry with its variables filled in, when that differs from it.
    expanded: Option<String>,
    exists: bool,
    /// The index of an earlier entry naming the same dir.
    dup_of: Option<usize>,
    /// A character the entry can't hold here (see [path_env::bad_char]).
    bad: Option<char>,
}

/// A row being edited in place.
struct RowEdit {
    idx: usize,
    text: String,
    /// Set once the edit box has taken focus, the first frame it's drawn.
    focused: bool,
}

/// One scope's list, as being edited.
pub struct ScopeEdit {
    scope: Scope,
    entries: Vec<String>,
    /// The entries as last read or saved. Any difference is unsaved.
    saved: Vec<String>,
    /// Parallel to `entries`; see [Self::changed].
    status: Vec<EntryStatus>,
    /// Why this scope can't be edited here, if it can't.
    read_only: Option<String>,
    /// Other things changing this scope's PATH; see [path_env::Loaded].
    others: Vec<String>,
    others_title: String,
    selected: Option<usize>,
    editing: Option<RowEdit>,
    /// While "Edit as text" is on, the entries as text, one per line. The
    /// entries are re-split from it as it's edited.
    text: Option<String>,
    /// A row to scroll into view next frame, e.g. one just added.
    scroll_to: Option<usize>,
}

impl ScopeEdit {
    fn load(scope: Scope, rc_file: &str) -> Self {
        let loaded = path_env::load(scope, rc_file).unwrap_or_else(|e| path_env::Loaded {
            entries: Vec::new(),
            read_only: Some(format!(
                "Couldn't read {}: {e}",
                path_env::location(scope, rc_file)
            )),
            others: Vec::new(),
            others_title: String::new(),
        });
        let mut s = Self {
            scope,
            saved: loaded.entries.clone(),
            entries: loaded.entries,
            status: Vec::new(),
            read_only: loaded.read_only,
            others: loaded.others,
            others_title: loaded.others_title,
            selected: None,
            editing: None,
            text: None,
            scroll_to: None,
        };
        s.changed();
        s
    }

    fn dirty(&self) -> bool {
        self.entries != self.saved
    }

    /// Re-derive `status` after `entries` changed, and keep the selection
    /// in range.
    fn changed(&mut self) {
        let keys: Vec<String> = self.entries.iter().map(|e| path_env::key(e)).collect();
        self.status = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let expanded = path_env::expand(e);
                EntryStatus {
                    exists: Path::new(&expanded).is_dir(),
                    expanded: (expanded != *e).then_some(expanded),
                    dup_of: keys[..i].iter().position(|k| *k == keys[i]),
                    bad: path_env::bad_char(self.scope, e),
                }
            })
            .collect();
        if self.selected.is_some_and(|i| i >= self.entries.len()) {
            self.selected = self.entries.len().checked_sub(1);
        }
    }

    /// Add `entry` at the end and select it, then edit it in place if
    /// `edit`. An entry that's already listed is just selected.
    fn add(&mut self, entry: String, edit: bool) {
        let key = path_env::key(&entry);
        let idx = match self
            .entries
            .iter()
            .position(|e| !entry.is_empty() && path_env::key(e) == key)
        {
            Some(i) => i,
            None => {
                self.entries.push(entry);
                self.changed();
                self.entries.len() - 1
            }
        };
        self.selected = Some(idx);
        self.scroll_to = Some(idx);
        if edit {
            self.start_edit(idx);
        }
    }

    fn start_edit(&mut self, idx: usize) {
        self.finish_edit(true);
        if let Some(e) = self.entries.get(idx) {
            self.selected = Some(idx);
            self.editing = Some(RowEdit {
                idx,
                text: e.clone(),
                focused: false,
            });
        }
    }

    /// End an in-place edit, keeping the new text if `apply`. Like Windows'
    /// dialog, an entry left empty is removed, as is a new one cancelled.
    fn finish_edit(&mut self, apply: bool) {
        let Some(r) = self.editing.take() else {
            return;
        };
        let text = r.text.trim();
        if apply && !text.is_empty() {
            self.entries[r.idx] = text.to_owned();
        } else if apply || self.entries[r.idx].is_empty() {
            self.entries.remove(r.idx);
        }
        self.changed();
    }

    fn delete_selected(&mut self) {
        if let Some(i) = self.selected.filter(|&i| i < self.entries.len()) {
            self.entries.remove(i);
            self.changed();
        }
    }

    /// Move the selected entry `by` rows (-1 up, 1 down).
    fn move_selected(&mut self, by: isize) {
        let Some(i) = self.selected else {
            return;
        };
        let Some(j) = i.checked_add_signed(by).filter(|&j| j < self.entries.len()) else {
            return;
        };
        self.entries.swap(i, j);
        self.selected = Some(j);
        self.scroll_to = Some(j);
        self.changed();
    }

    /// The entries were edited as text: re-split them from it.
    fn text_changed(&mut self) {
        self.entries = path_env::split(self.text.as_deref().unwrap_or_default());
        self.changed();
    }
}

/// What the details pane shows for the selected entry.
struct Details {
    scope: Scope,
    /// The entries searched before the selected one, then it: what this was
    /// built from, to tell when it's out of date.
    order: Vec<String>,
    entry: String,
    expanded: String,
    /// Why there are no executables to list, if there aren't: the dir is
    /// missing, say.
    problem: Option<String>,
    /// The executables in the selected dir. Each has the file that runs
    /// instead when its name is typed, if one does: one of the same name in
    /// a dir searched earlier.
    programs: Vec<(String, Option<PathBuf>)>,
}

impl Details {
    /// The details for the last of `order`, the entries searched up to and
    /// including the selected one. Dirs are read through `cache`, keyed by
    /// [path_env::key].
    fn build(
        scope: Scope,
        order: Vec<String>,
        cache: &mut HashMap<String, Result<DirPrograms, String>>,
    ) -> Self {
        for e in &order {
            cache.entry(path_env::key(e)).or_insert_with(|| {
                DirPrograms::read(Path::new(&path_env::expand(e))).map_err(|err| err.to_string())
            });
        }
        let entry = order.last().cloned().unwrap_or_default();
        let key = path_env::key(&entry);
        let expanded = path_env::expand(&entry);
        let dir = PathBuf::from(&expanded);
        let this = match &cache[&key] {
            Ok(p) => p,
            Err(e) => {
                let problem = if !dir.exists() {
                    "No such directory.".to_owned()
                } else if !dir.is_dir() {
                    "Not a directory.".to_owned()
                } else {
                    format!("Couldn't read it: {e}")
                };
                return Self {
                    scope,
                    order,
                    entry,
                    expanded,
                    problem: Some(problem),
                    programs: Vec::new(),
                };
            }
        };

        // The dirs searched first, leaving out copies of this one: the list
        // marks a duplicate entry as such.
        let earlier: Vec<(PathBuf, &DirPrograms)> = order[..order.len() - 1]
            .iter()
            .filter(|e| path_env::key(e) != key)
            .filter_map(|e| {
                let programs = cache[&path_env::key(e)].as_ref().ok()?;
                Some((PathBuf::from(path_env::expand(e)), programs))
            })
            .collect();
        let programs = this
            .names
            .iter()
            .map(|name| {
                let cmd = path_env::command_name(name);
                // An earlier dir's program of this name, or on Windows, one
                // here whose extension PATHEXT tries first.
                let instead = earlier
                    .iter()
                    .find_map(|(d, p)| p.runs(&cmd).map(|f| d.join(f)))
                    .or_else(|| {
                        this.runs(&cmd)
                            .filter(|f| *f != name.as_str())
                            .map(|f| dir.join(f))
                    });
                (name.clone(), instead)
            })
            .collect();
        Self {
            scope,
            order,
            entry,
            expanded,
            problem: None,
            programs,
        }
    }
}

/// The PATH panel's state: which scope is shown, and each one's list.
pub struct PathPanel {
    scope: Scope,
    /// The startup file User is kept in on Linux/macOS (one of
    /// [path_env::RC_FILES]); Windows ignores it.
    rc_file: &'static str,
    /// Each scope's list, by `Scope as usize`, read the first time it's
    /// shown.
    edits: [Option<ScopeEdit>; 3],
    /// The details pane's, for the selected entry. See [Self::update_details].
    details: Option<Details>,
    /// Each dir's executables as read for the details pane, by
    /// [path_env::key]; dropped on Reload.
    programs: HashMap<String, Result<DirPrograms, String>>,
    /// The details pane's filter on executable names.
    program_filter: String,
}

impl Default for PathPanel {
    fn default() -> Self {
        Self {
            scope: Scope::User,
            rc_file: path_env::default_rc(),
            edits: [None, None, None],
            details: None,
            programs: HashMap::new(),
            program_filter: String::new(),
        }
    }
}

impl PathPanel {
    /// `scope`'s list, read now if it hasn't been yet.
    fn edit(&mut self, scope: Scope) -> &mut ScopeEdit {
        let rc_file = self.rc_file;
        self.edits[scope as usize].get_or_insert_with(|| ScopeEdit::load(scope, rc_file))
    }

    /// Bring `details` up to date with the shown scope's selection,
    /// rebuilding it when the selected entry, or one searched before it,
    /// changed. Returns whether it changed.
    fn update_details(&mut self) -> bool {
        let scope = self.scope;
        let mut order = match path_env::searched_before(scope) {
            Some(s) => self.edit(s).entries.clone(),
            None => Vec::new(),
        };
        let edit = self.edit(scope);
        match edit.selected.filter(|&i| i < edit.entries.len()) {
            Some(i) => order.extend_from_slice(&edit.entries[..=i]),
            None => return self.details.take().is_some(),
        }
        if self
            .details
            .as_ref()
            .is_some_and(|d| d.scope == scope && d.order == order)
        {
            return false;
        }
        self.details = Some(Details::build(scope, order, &mut self.programs));
        true
    }

    fn is_dirty(&self, scope: Scope) -> bool {
        self.edits[scope as usize]
            .as_ref()
            .is_some_and(ScopeEdit::dirty)
    }

    /// Save the shown scope's list, and make the same change to this app's
    /// PATH. Returns the message for the output pane.
    fn save(&mut self) -> Result<String, String> {
        let (scope, rc_file) = (self.scope, self.rc_file);
        let location = path_env::location(scope, rc_file);
        let edit = self.edit(scope);
        edit.finish_edit(true);
        if let Some((e, c)) = edit
            .entries
            .iter()
            .find_map(|e| path_env::bad_char(scope, e).map(|c| (e, c)))
        {
            return Err(format!("PATH: not saved; `{e}` can't contain `{c}`"));
        }
        if scope != Scope::User {
            path_env::check_system(&edit.entries).map_err(|e| format!("PATH: not saved. {e}"))?;
        }
        let new = edit.entries.clone();
        path_env::save(scope, rc_file, &new)
            .map_err(|e| format!("PATH: couldn't save to {location}: {e}"))?;
        let old = std::mem::replace(&mut edit.saved, new.clone());

        let n = match new.len() {
            1 => "1 entry".to_owned(),
            n => format!("{n} entries"),
        };
        if scope == Scope::Session {
            return Ok(format!(
                "PATH: this app now uses {n}; commands run from any tab get it."
            ));
        }
        let (added, removed) = path_env::apply_to_session(scope, rc_file, &old, &new);
        // Re-read This app's list when next shown, unless it has edits of
        // its own.
        if !self.is_dirty(Scope::Session) {
            self.edits[Scope::Session as usize] = None;
        }
        let session = if added + removed > 0 {
            format!(" This app's PATH was changed to match (+{added}, -{removed}).")
        } else {
            String::new()
        };
        Ok(format!(
            "PATH: saved {n} to {location}. {}.{session}",
            path_env::takes_effect(scope)
        ))
    }
}

/// The entry "+ Current dir" adds for `dir`. Under the home dir, a
/// Linux/macOS User entry is written `$HOME/...`, so the startup file
/// doesn't hard-code it.
fn dir_entry(scope: Scope, dir: &Path, home: Option<&Path>) -> String {
    if !cfg!(windows)
        && scope == Scope::User
        && let Some(rest) = home.and_then(|h| dir.strip_prefix(h).ok())
    {
        return if rest.as_os_str().is_empty() {
            "$HOME".to_owned()
        } else {
            format!("$HOME/{}", rest.display())
        };
    }
    dir.display().to_string()
}

/// What the PATH panel asked for, done once it's drawn.
enum Request {
    Save,
    Reload,
    /// cd the active tab to this dir.
    ChangeDir(PathBuf),
    /// Show this dir in the OS's file manager.
    OpenDir(PathBuf),
}

/// The PATH panel. Pick a scope at the top; its entries are listed in PATH
/// order, the first searched first. Click a row to select it, double-click
/// to edit it in place (Enter or clicking away keeps the change, Escape
/// drops it). Missing dirs and duplicates are marked. Changes are kept
/// apart per scope until saved.
pub fn path_panel(state: &mut State, ui: &mut Ui) {
    // Measured before anything is laid out, so the details pane is a third
    // of the whole panel, as in the Files panel.
    let details_height = ui.available_height() / 3.0;
    panel_header(ui, "PATH", &mut state.ui.panel_vis.path_editor);

    let cwd = state.cwd().to_path_buf();
    let home = state.home.clone();
    let request = draw(
        &mut state.path_panel,
        &cwd,
        home.as_deref(),
        details_height,
        ui,
    );

    match request {
        Some(Request::Save) => match state.path_panel.save() {
            Ok(msg) => state.push_out(msg, OutType::StdOut),
            Err(msg) => state.push_out(msg, OutType::StdErr),
        },
        Some(Request::Reload) => {
            let p = &mut state.path_panel;
            p.edits[p.scope as usize] = Some(ScopeEdit::load(p.scope, p.rc_file));
            // Re-read the dirs too, in case programs were installed.
            p.programs.clear();
            p.details = None;
        }
        Some(Request::ChangeDir(dir)) => state.change_dir(dir),
        Some(Request::OpenDir(dir)) => shell::commands::open_dir(&dir, &mut |kind, msg| {
            state.push_out(msg, crate::out_type_for(kind));
        }),
        None => {}
    }

    // A click in the list changes the selection after the details pane was
    // drawn this frame, so show its details on the next.
    if state.path_panel.update_details() {
        ui.ctx().request_repaint();
    }
}

fn draw(
    panel: &mut PathPanel,
    cwd: &Path,
    home: Option<&Path>,
    details_height: f32,
    ui: &mut Ui,
) -> Option<Request> {
    // Scope picker. `*` marks one with unsaved changes.
    ui.horizontal(|ui| {
        for scope in Scope::ALL {
            let label = if panel.is_dirty(scope) {
                format!("{}*", scope.label())
            } else {
                scope.label().to_owned()
            };
            let hover = path_env::location(scope, panel.rc_file);
            if ui
                .add(Button::selectable(panel.scope == scope, label))
                .on_hover_text(hover)
                .clicked()
            {
                if let Some(e) = panel.edits[panel.scope as usize].as_mut() {
                    e.finish_edit(true);
                }
                panel.scope = scope;
            }
        }
    });

    let scope = panel.scope;
    if cfg!(windows) || scope != Scope::User {
        let location = path_env::location(scope, panel.rc_file);
        ui.add(egui::Label::new(RichText::new(location).monospace()).wrap());
    } else {
        // Which startup file User is kept in. Switching re-reads the list,
        // so not while it has changes.
        let dirty = panel.is_dirty(scope);
        let mut rc_file = panel.rc_file;
        ui.horizontal(|ui| {
            ui.label("Kept in");
            let combo = ui.add_enabled_ui(!dirty, |ui| {
                ComboBox::from_id_salt("path_rc_file")
                    .selected_text(format!("~/{rc_file}"))
                    .show_ui(ui, |ui| {
                        for f in path_env::RC_FILES {
                            ui.selectable_value(&mut rc_file, f, format!("~/{f}"));
                        }
                    });
            });
            if dirty {
                combo
                    .response
                    .on_hover_text("Save or revert this list's changes first");
            }
        });
        if rc_file != panel.rc_file {
            panel.rc_file = rc_file;
            panel.edits[Scope::User as usize] = None;
        }
    }
    let note = path_env::note(scope, panel.rc_file);
    ui.add(egui::Label::new(RichText::new(note).italics()).wrap());

    let read_only = panel.edit(scope).read_only.clone();
    if let Some(why) = &read_only {
        ui.add(egui::Label::new(RichText::new(why).color(ui.visuals().warn_fg_color)).wrap());
    }
    ui.separator();

    // The details claim the bottom of the panel, then the buttons the space
    // above them; the list fills the rest.
    panel.update_details();
    let mut request = details_pane(panel, details_height, ui);

    let edit = panel.edit(scope);
    let editable = read_only.is_none();
    Panel::bottom("path_panel_controls")
        .resizable(false)
        .frame(Frame::new().inner_margin(egui::Margin::symmetric(0, 4)))
        .show_inside(ui, |ui| {
            if editable {
                // Row buttons act on the list, so are off while it's being
                // edited as text.
                let rows = edit.text.is_none();
                let sel = edit.selected.filter(|_| rows);
                let n = edit.entries.len();
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add_enabled(rows, Button::new("+ New"))
                        .on_hover_text("Add an entry at the end, and type it in")
                        .clicked()
                    {
                        edit.add(String::new(), true);
                    }
                    if ui
                        .add_enabled(rows, Button::new("+ Current dir"))
                        .on_hover_text(format!("Add the active tab's directory: {}", cwd.display()))
                        .clicked()
                    {
                        edit.add(dir_entry(scope, cwd, home), false);
                    }
                    if ui
                        .add_enabled(sel.is_some(), Button::new("Edit"))
                        .on_hover_text("Edit the selected entry (or double-click it)")
                        .clicked()
                        && let Some(i) = sel
                    {
                        edit.start_edit(i);
                    }
                    if ui
                        .add_enabled(sel.is_some(), Button::new("Delete"))
                        .on_hover_text("Remove the selected entry")
                        .clicked()
                    {
                        edit.delete_selected();
                    }
                    if ui
                        .add_enabled(sel.is_some_and(|i| i > 0), Button::new("⬆"))
                        .on_hover_text("Move up. Earlier entries are searched first.")
                        .clicked()
                    {
                        edit.move_selected(-1);
                    }
                    if ui
                        .add_enabled(sel.is_some_and(|i| i + 1 < n), Button::new("⬇"))
                        .on_hover_text("Move down")
                        .clicked()
                    {
                        edit.move_selected(1);
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add(Button::selectable(edit.text.is_some(), "Edit as text"))
                        .on_hover_text(
                            "Edit the list as text, one entry per line. A pasted \
                             PATH string splits into entries too.",
                        )
                        .clicked()
                    {
                        edit.finish_edit(true);
                        edit.text = match edit.text {
                            Some(_) => None,
                            None => Some(edit.entries.join("\n")),
                        };
                    }
                    let (save, save_hover) = match scope {
                        Scope::Session => ("Apply", "Use this list as this app's PATH".to_owned()),
                        _ => ("Save", format!("{}.", path_env::takes_effect(scope))),
                    };
                    let dirty = edit.dirty();
                    if ui
                        .add_enabled(dirty, Button::new(RichText::new(save).strong()))
                        .on_hover_text(save_hover)
                        .clicked()
                    {
                        request = Some(Request::Save);
                    }
                    let (reload, reload_hover) = if dirty {
                        ("Revert", "Drop the changes, re-reading the saved list")
                    } else {
                        (
                            "Reload",
                            "Re-read the list, in case something else changed it",
                        )
                    };
                    if ui.button(reload).on_hover_text(reload_hover).clicked() {
                        request = Some(Request::Reload);
                    }
                });
            } else if ui
                .button("Reload")
                .on_hover_text("Re-read the list")
                .clicked()
            {
                request = Some(Request::Reload);
            }

            if !edit.others.is_empty() {
                CollapsingHeader::new(format!("{} ({})", edit.others_title, edit.others.len()))
                    .id_salt(("path_others", scope as usize))
                    .show(ui, |ui| {
                        ScrollArea::vertical()
                            .id_salt(("path_others_scroll", scope as usize))
                            .max_height(120.0)
                            .show(ui, |ui| {
                                ui.style_mut().wrap_mode = Some(TextWrapMode::Wrap);
                                for o in &edit.others {
                                    ui.label(RichText::new(o).monospace().weak());
                                }
                            });
                    });
            }
        });

    if let Some(text) = edit.text.as_mut() {
        let mut changed = false;
        ScrollArea::vertical()
            .id_salt(("path_text_scroll", edit.scope as usize))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                changed = ui
                    .add(
                        TextEdit::multiline(text)
                            .font(TextStyle::Monospace)
                            .desired_width(f32::INFINITY)
                            .desired_rows(12),
                    )
                    .changed();
            });
        if changed {
            edit.text_changed();
        }
        return request;
    }

    entry_list(edit, editable, ui);
    request
}

/// The scope's entries, one selectable row each, or an edit box for the row
/// being edited.
fn entry_list(edit: &mut ScopeEdit, editable: bool, ui: &mut Ui) {
    let mut select = None;
    let mut start_edit = None;
    // Set when the edit box loses focus: `true` to keep the new text, `false`
    // if Escape dropped it.
    let mut edit_done = None;
    let scroll_to = edit.scroll_to.take();

    ScrollArea::vertical()
        .id_salt(("path_list_scroll", edit.scope as usize))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // Rows butt up against each other, like the Files listing's.
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.style_mut().wrap_mode = Some(TextWrapMode::Wrap);

            if edit.entries.is_empty() {
                ui.label(RichText::new("No entries.").italics().weak());
            }
            for (i, entry) in edit.entries.iter().enumerate() {
                if let Some(r) = edit.editing.as_mut().filter(|r| r.idx == i) {
                    if let Some(apply) = row_edit_box(ui, r) {
                        edit_done = Some(apply);
                    }
                    if scroll_to == Some(i) {
                        ui.scroll_to_cursor(Some(Align::Center));
                    }
                    continue;
                }
                let st = &edit.status[i];
                let visuals = ui.visuals();
                let mut text = RichText::new(entry).monospace();
                if st.bad.is_some() {
                    text = RichText::new(format!("⚠{entry}"))
                        .monospace()
                        .color(visuals.error_fg_color);
                } else if !st.exists {
                    text = RichText::new(format!("⚠{entry}"))
                        .monospace()
                        .color(visuals.warn_fg_color);
                } else if st.dup_of.is_some() {
                    text = text.strikethrough();
                }

                let row = Button::selectable(edit.selected == Some(i), text)
                    .wrap()
                    .min_size(egui::vec2(ui.available_width(), 0.0));
                let mut resp = ui.add(row);
                if let Some(hover) = row_hover(st, &edit.entries) {
                    resp = resp.on_hover_text(hover);
                }
                if scroll_to == Some(i) {
                    resp.scroll_to_me(Some(Align::Center));
                }
                if resp.double_clicked() && editable {
                    start_edit = Some(i);
                } else if resp.clicked() {
                    select = Some(i);
                }
            }
        });

    if let Some(apply) = edit_done {
        edit.finish_edit(apply);
    }
    if let Some(i) = select {
        edit.selected = Some(i);
    }
    if let Some(i) = start_edit {
        edit.start_edit(i);
    }
}

/// The panel's lower pane: details of the selected entry (see [Details]),
/// chiefly the executables in it. One a dir searched earlier hides (its
/// name runs that one instead) is struck through, saying which file runs.
fn details_pane(panel: &mut PathPanel, height: f32, ui: &mut Ui) -> Option<Request> {
    let mut request = None;
    Panel::bottom("path_details_panel")
        .resizable(false)
        .exact_size(height)
        .frame(Frame::new().inner_margin(egui::Margin::symmetric(0, 4)))
        .show_inside(ui, |ui| {
            let Some(d) = &panel.details else {
                ui.label(
                    RichText::new("Select an entry to see its details.")
                        .italics()
                        .weak(),
                );
                return;
            };
            ui.style_mut().wrap_mode = Some(TextWrapMode::Wrap);

            // One "Label  value" line, wrapping when the value is long, as
            // in the Files panel's details.
            let field = |ui: &mut Ui, label: &str, value: WidgetText| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(label).weak());
                    ui.label(value);
                });
            };
            ui.label(RichText::new(&d.entry).strong());
            if d.expanded != d.entry {
                field(
                    ui,
                    "Expands to",
                    RichText::new(&d.expanded).monospace().into(),
                );
            }
            if let Some(p) = &d.problem {
                ui.label(RichText::new(p).color(ui.visuals().warn_fg_color));
                return;
            }

            let hidden = d.programs.iter().filter(|(_, i)| i.is_some()).count();
            let mut count = plural(d.programs.len(), "executable", "executables");
            if hidden > 0 {
                count += &format!(", {hidden} hidden by ones searched earlier (struck through)");
            }
            field(ui, "Contains", count.into());
            ui.horizontal(|ui| {
                let dir = PathBuf::from(&d.expanded);
                if ui
                    .button("cd here")
                    .on_hover_text("Go to this folder in the active tab")
                    .clicked()
                {
                    request = Some(Request::ChangeDir(dir.clone()));
                }
                if ui
                    .button("📂 Open")
                    .on_hover_text("Open this folder in the system file manager")
                    .clicked()
                {
                    request = Some(Request::OpenDir(dir));
                }
            });
            if d.programs.is_empty() {
                return;
            }
            ui.add(
                TextEdit::singleline(&mut panel.program_filter)
                    .hint_text("Filter executables...")
                    .desired_width(f32::INFINITY)
                    .id_salt("path_program_filter"),
            );

            let needle = panel.program_filter.to_lowercase();
            let rows: Vec<&(String, Option<PathBuf>)> = d
                .programs
                .iter()
                .filter(|(name, _)| needle.is_empty() || name.to_lowercase().contains(&needle))
                .collect();
            if rows.is_empty() {
                ui.label(RichText::new("No matches.").italics().weak());
            }
            // One line of monospace text each, so only the visible ones
            // need laying out, even for a dir like system32.
            let row_height = ui.text_style_height(&TextStyle::Monospace);
            ScrollArea::vertical()
                .id_salt("path_programs_scroll")
                .auto_shrink([false, false])
                .show_rows(ui, row_height, rows.len(), |ui, range| {
                    for (name, instead) in &rows[range] {
                        let mut text = RichText::new(name).monospace();
                        if instead.is_some() {
                            text = text.strikethrough();
                        }
                        let resp = ui.add(egui::Label::new(text).truncate());
                        if let Some(f) = instead {
                            resp.on_hover_text(format!(
                                "Hidden: `{}` runs {} instead",
                                path_env::command_name(name),
                                f.display()
                            ));
                        }
                    }
                });
        });
    request
}

/// A row's tooltip: what's notable about its entry, if anything.
fn row_hover(st: &EntryStatus, entries: &[String]) -> Option<String> {
    let mut lines = Vec::new();
    if let Some(x) = &st.expanded {
        lines.push(format!("= {x}"));
    }
    if let Some(c) = st.bad {
        lines.push(format!("Can't contain `{c}` here"));
    } else if !st.exists {
        lines.push("No such directory".to_owned());
    }
    if let Some(j) = st.dup_of {
        lines.push(format!(
            "Duplicate: {} is listed above, and is searched first",
            entries[j]
        ));
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// The edit box standing in for a row while it's edited. Returns
/// `Some(true)` once editing ends with Enter or a click elsewhere, and
/// `Some(false)` if Escape dropped it.
fn row_edit_box(ui: &mut Ui, r: &mut RowEdit) -> Option<bool> {
    let id = egui::Id::new("path_row_edit");
    let mut out = TextEdit::singleline(&mut r.text)
        .id(id)
        .font(TextStyle::Monospace)
        .hint_text("Directory")
        .desired_width(f32::INFINITY)
        .show(ui);
    if !r.focused {
        // Caret at the end, ready to tweak the path.
        r.focused = true;
        out.response.request_focus();
        let end = CCursor::new(r.text.chars().count());
        out.state
            .cursor
            .set_char_range(Some(CCursorRange::one(end)));
        out.state.store(ui.ctx(), id);
    }
    out.response
        .lost_focus()
        .then(|| !ui.input(|i| i.key_pressed(Key::Escape)))
}
