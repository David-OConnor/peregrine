//! The Files panel's path bar, above its listing: the current dir as
//! breadcrumbs, as in a file manager's address bar. Each dir from the root
//! (or `~`) down is a button that goes there, and the `›` after it a menu of
//! the folders in it. A click on the bar beside them, or Ctrl+L / Alt+D as
//! in Explorer, swaps them for an edit box holding the whole path, to change
//! or replace and go to with Enter.

use std::{
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
};

use eframe::epaint::text::cursor::CCursor;
use egui::{
    Align, Button, Color32, Context, CursorIcon, EventFilter, FontId, Frame, Id, Key, Margin,
    Modifiers, Response, RichText, ScrollArea, Sense, TextEdit, TextStyle, Ui, UiBuilder,
    containers::menu::MenuButton, text::CCursorRange,
};
use shell::{apply_completion, complete_cd_path, read_browser_files};

use crate::{
    path_color, path_env, render_with_tilde,
    state::{PathEdit, State, listing_shows},
    ui::{
        self, ListAction,
        dirs::{BOOKMARKED_TEXT, bookmarked_fill},
        file_browser::{RowAction, apply_row_action, drop_target},
    },
};

/// Draw the path bar for the active tab's dir, in the width left in `ui`:
/// its breadcrumbs, on the bookmark green if `bookmarked`, or while a path
/// is typed in, the edit box (see [PathEdit]). `start_edit` (Ctrl+L / Alt+D)
/// opens that. Returns whether a file's path was entered, and its row
/// selected, to be scrolled into view.
pub(super) fn draw(state: &mut State, ui: &mut Ui, bookmarked: bool, start_edit: bool) -> bool {
    if state
        .ui
        .path_edit
        .as_ref()
        .is_some_and(|e| e.dir != state.cwd())
    {
        state.ui.path_edit = None;
    }
    if start_edit {
        begin_edit(state, ui.ctx());
    }

    let Some(mut edit) = state.ui.path_edit.take() else {
        let mut action = None;
        if breadcrumbs(state, ui, bookmarked, &mut action) {
            begin_edit(state, ui.ctx());
        }
        if action.is_some() {
            ui.ctx().request_repaint();
        }
        apply_row_action(state, ui, action);
        return false;
    };

    match edit_box(state, ui, &mut edit) {
        None => state.ui.path_edit = Some(edit),
        Some(true) => return submit(state, ui, edit),
        Some(false) => ui.ctx().request_repaint(),
    }
    false
}

/// Swap the breadcrumbs for the edit box, holding the whole path.
fn begin_edit(state: &mut State, ctx: &Context) {
    let dir = state.cwd().to_path_buf();
    state.ui.path_edit = Some(PathEdit {
        text: dir.display().to_string(),
        dir,
        focused: false,
        error: None,
        matches: Vec::new(),
    });
    ctx.request_repaint();
}

/// One breadcrumb: a dir in the path, under its name.
struct Crumb {
    name: String,
    dir: PathBuf,
}

/// The breadcrumbs for `cwd`, from its root down, or from `~` when it's in
/// `home`; and in that case, the dirs above `home` the `~` stands in for,
/// root first.
fn crumbs(cwd: &Path, home: Option<&Path>) -> (Vec<Crumb>, Vec<PathBuf>) {
    let mut crumbs = Vec::new();
    let mut above_home = Vec::new();

    for dir in cwd.ancestors() {
        if Some(dir) == home {
            crumbs.push(Crumb {
                name: "~".into(),
                dir: dir.to_path_buf(),
            });
            above_home = dir.ancestors().skip(1).map(Path::to_path_buf).collect();
            above_home.reverse();
            break;
        }
        crumbs.push(Crumb {
            name: dir_name(dir),
            dir: dir.to_path_buf(),
        });
    }

    crumbs.reverse();
    (crumbs, above_home)
}

/// A dir's own name, or a root's (`C:`, `/`, `\\server\share`), without the
/// separator after it unless that's all there is.
fn dir_name(dir: &Path) -> String {
    if let Some(name) = dir.file_name() {
        return name.to_string_lossy().into_owned();
    }
    let root = dir.to_string_lossy();
    let trimmed = root.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        root.into_owned()
    } else {
        trimmed.to_owned()
    }
}

/// The breadcrumbs, in a box filling the width `ui` has left. Puts what a
/// click, middle-click or drop on one asks for in `action`. Returns whether
/// the box was clicked beside them, to type a path.
fn breadcrumbs(
    state: &State,
    ui: &mut Ui,
    bookmarked: bool,
    action: &mut Option<RowAction>,
) -> bool {
    let home = state.home.as_deref();
    let (crumbs, above_home) = crumbs(state.cwd(), home);
    let dark = ui.visuals().dark_mode;
    let listing = (state.ui.show_hidden_attr, state.ui.show_dotfiles);
    // On the bookmark green, the light theme draws text dark.
    let plain = (bookmarked && !dark).then_some(BOOKMARKED_TEXT);

    let visuals = ui.visuals();
    let mut frame = Frame::new()
        .stroke(visuals.widgets.noninteractive.bg_stroke)
        .corner_radius(visuals.widgets.inactive.corner_radius)
        .inner_margin(Margin::symmetric(2, 1));
    if bookmarked {
        frame = frame.fill(bookmarked_fill(dark));
    }

    let mut menu_open = false;

    // Its click sense is beneath the breadcrumbs', so it only gets the
    // clicks that miss them.
    let bar = ui.scope_builder(UiBuilder::new().sense(Sense::click()), |ui| {
        frame.show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                let font = TextStyle::Body.resolve(ui.style());
                let bold = FontId::new(font.size, ui::bold_family());

                if !above_home.is_empty() {
                    let (resp, _) = MenuButton::from_button(chevron("«", plain)).ui(ui, |ui| {
                        for dir in &above_home {
                            let name = dir.display().to_string();
                            dir_item(ui, dir, &name, false, home, action);
                        }
                    });
                    resp.on_hover_text("The folders ~ is in");
                }

                for (i, crumb) in crumbs.iter().enumerate() {
                    // Bold for the dir itself, as wherever else it's shown.
                    let font = if i + 1 == crumbs.len() {
                        bold.clone()
                    } else {
                        font.clone()
                    };
                    let mut text = RichText::new(&crumb.name).font(font);
                    let shown = render_with_tilde(&crumb.dir, home);
                    if let Some(color) = path_color::last_color(&shown, Some("~"), dark) {
                        text = text.color(color);
                    }

                    let resp = ui
                        .add(Button::new(text).frame_when_inactive(false))
                        .on_hover_text(crumb.dir.display().to_string());
                    if let Some(a) = dir_action(ui, &resp, &crumb.dir) {
                        *action = Some(a);
                    }

                    let next = crumbs.get(i + 1).map(|c| c.dir.as_path());
                    menu_open |= subdir_menu(ui, &crumb.dir, next, listing, home, plain, action);
                }
            });
        });
    });

    if !menu_open {
        forget_subdirs(ui.ctx());
    }

    bar.response
        .on_hover_cursor(CursorIcon::Text)
        .on_hover_text("Click to type a path (Ctrl+L)")
        .clicked()
}

/// A `›` (or `«`) that opens a menu of dirs, framed only while it's
/// hovered, as the breadcrumbs are. In the `plain` color, if one's given.
fn chevron(glyph: &str, plain: Option<Color32>) -> Button<'_> {
    let mut text = RichText::new(glyph);
    if let Some(color) = plain {
        text = text.color(color);
    }
    Button::new(text).frame_when_inactive(false)
}

/// What a click (go there), middle-click (open it in a new tab) or Files
/// rows dropped (move them there, see [drop_target]) on `resp`, a button for
/// `dir`, asks for.
fn dir_action(ui: &Ui, resp: &Response, dir: &Path) -> Option<RowAction> {
    if let Some(drop) = drop_target(ui, resp, dir) {
        return Some(RowAction::Drop(drop));
    }
    let dir = dir.to_path_buf();
    if resp.middle_clicked() {
        Some(RowAction::List(ListAction::NewTabDir(dir)))
    } else if resp.clicked() {
        Some(RowAction::List(ListAction::ChangeDir(dir)))
    } else {
        None
    }
}

/// The `›` after the breadcrumb for `dir`: a menu of the folders in it that
/// the listing would show, by its `listing` filters (see [listing_shows]),
/// with `next`, the one the path goes on into, highlighted. Puts what a
/// click on one asks for in `action`. Returns whether it's open.
fn subdir_menu(
    ui: &mut Ui,
    dir: &Path,
    next: Option<&Path>,
    listing: (bool, bool),
    home: Option<&Path>,
    plain: Option<Color32>,
    action: &mut Option<RowAction>,
) -> bool {
    let (_, menu) = MenuButton::from_button(chevron("›", plain)).ui(ui, |ui| {
        let (dirs, opened) = subdirs(ui.ctx(), dir, listing);
        if dirs.1.is_empty() {
            ui.label(RichText::new("No folders").italics().weak());
            return;
        }

        let max_height = 0.6 * ui.ctx().content_rect().height();
        ScrollArea::vertical()
            .max_height(max_height)
            .show(ui, |ui| {
                for sub in &dirs.1 {
                    let is_next = Some(sub.as_path()) == next;
                    let resp = dir_item(ui, sub, &dir_name(sub), is_next, home, action);
                    if is_next && opened {
                        resp.scroll_to_me(Some(Align::Center));
                    }
                }
            });
    });
    menu.is_some()
}

/// A row in a breadcrumb menu for `dir`, shown as `name` in the color it has
/// in paths everywhere. Puts what a click on it asks for in `action`.
fn dir_item(
    ui: &mut Ui,
    dir: &Path,
    name: &str,
    selected: bool,
    home: Option<&Path>,
    action: &mut Option<RowAction>,
) -> Response {
    let mut text = RichText::new(format!("📁 {name}"));
    let shown = render_with_tilde(dir, home);
    if let Some(color) = path_color::last_color(&shown, Some("~"), ui.visuals().dark_mode) {
        text = text.color(color);
    }

    let resp = ui.add(Button::selectable(selected, text));
    if let Some(a) = dir_action(ui, &resp, dir) {
        *action = Some(a);
    }
    resp
}

/// A dir, and the folders in it, as a `›` menu lists them.
type SubDirs = Arc<(PathBuf, Vec<PathBuf>)>;

fn subdirs_id() -> Id {
    Id::new("path_bar_subdirs")
}

/// The folders in `dir` the listing would show (see [subdir_menu]), and
/// whether they were read just now, as its menu opened. They're kept while
/// it's open, rather than read again every frame.
fn subdirs(ctx: &Context, dir: &Path, (attr, dot): (bool, bool)) -> (SubDirs, bool) {
    let kept = ctx.data(|d| d.get_temp::<SubDirs>(subdirs_id()));
    if let Some(kept) = kept.filter(|k| k.0 == dir) {
        return (kept, false);
    }

    let dirs = read_browser_files(dir)
        .into_iter()
        .filter(|f| f.is_folder && listing_shows(f, attr, dot))
        .map(|f| f.path)
        .collect();
    let read = Arc::new((dir.to_path_buf(), dirs));
    ctx.data_mut(|d| d.insert_temp(subdirs_id(), Arc::clone(&read)));
    (read, true)
}

/// Once no `›` menu is open, so the next to open reads its folders afresh.
fn forget_subdirs(ctx: &Context) {
    ctx.data_mut(|d| d.remove_temp::<SubDirs>(subdirs_id()));
}

fn edit_id() -> Id {
    Id::new("files_path_edit")
}

/// The edit box the path bar turns into to type a path, with why the last
/// one entered went nowhere under it, or the folders Tab could complete it
/// to. Returns `Some(true)` once Enter is pressed in it, and `Some(false)`
/// once it's given up with Escape or a click elsewhere.
fn edit_box(state: &State, ui: &mut Ui, edit: &mut PathEdit) -> Option<bool> {
    let id = edit_id();
    // Taken before the box sees it, as completion.
    let tab = ui.memory(|m| m.has_focus(id))
        && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Tab));
    if tab {
        complete(state, edit);
    }

    ui.vertical(|ui| {
        // Tab and Up / Down stay with the box, rather than move the focus to
        // another widget and so give it up.
        let filter = EventFilter {
            tab: true,
            horizontal_arrows: true,
            vertical_arrows: true,
            escape: false,
        };
        let mut out = TextEdit::singleline(&mut edit.text)
            .id(id)
            .event_filter(filter)
            .hint_text("The path of a folder to go to, or of a file to select")
            .desired_width(f32::INFINITY)
            .show(ui);

        if out.response.changed() {
            edit.error = None;
            edit.matches.clear();
        }

        // All selected at first, as in Explorer, so typing replaces it; after
        // a completion, the caret at its end.
        let len = edit.text.chars().count();
        let caret = if !edit.focused {
            edit.focused = true;
            out.response.request_focus();
            Some(CCursorRange::two(CCursor::new(0), CCursor::new(len)))
        } else if tab {
            Some(CCursorRange::one(CCursor::new(len)))
        } else {
            None
        };
        if let Some(range) = caret {
            out.state.cursor.set_char_range(Some(range));
            out.state.store(ui.ctx(), id);
        }

        if let Some(error) = &edit.error {
            ui.label(RichText::new(error).color(ui.visuals().error_fg_color));
        } else if !edit.matches.is_empty() {
            ui.label(RichText::new(match_list(&edit.matches)).weak());
        }

        out.response
            .lost_focus()
            .then(|| ui.input(|i| i.key_pressed(Key::Enter)))
    })
    .inner
}

/// Tab in the edit box: complete the folder name the path ends in, as far as
/// the folders it could be agree, as `cd`'s Tab does in the terminal (see
/// [complete_cd_path]); or, when that adds nothing, list them.
fn complete(state: &State, edit: &mut PathEdit) {
    const CD: &str = "cd ";
    let line = format!("{CD}{}", edit.text);
    let Some(result) = complete_cd_path(
        &line,
        line.len(),
        state.cwd(),
        state.home.as_deref(),
        &state.dir_bookmarks,
        &state.recent_dirs,
    ) else {
        return;
    };

    // As in the terminal, only take the shared prefix when it adds to the
    // path: fuzzy candidates can share less than was typed.
    let applied = apply_completion(&line, line.len(), &result)
        .filter(|text| result.candidates.len() == 1 || text.len() > line.len())
        .and_then(|text| text.strip_prefix(CD).map(str::to_owned));

    if let Some(text) = applied {
        edit.text = text;
        edit.matches.clear();
        edit.error = None;
    } else if result.candidates.len() > 1 {
        edit.matches = result.candidates.into_iter().map(|c| c.display).collect();
    }
}

/// The folders a Tab could complete to, for under the edit box: as many as
/// fit a few lines.
fn match_list(matches: &[String]) -> String {
    const MAX: usize = 30;
    let list = matches[..matches.len().min(MAX)].join("   ");
    match matches.len().checked_sub(MAX) {
        Some(more) if more > 0 => format!("{list}   and {more} more"),
        _ => list,
    }
}

/// Enter in the edit box: go to the dir typed, or for a file, to its dir with
/// it selected. Otherwise the box stays as it's typed, with why under it.
/// Returns as [draw] does.
fn submit(state: &mut State, ui: &Ui, mut edit: PathEdit) -> bool {
    ui.ctx().request_repaint();

    let gone = resolve(&edit.text, state.cwd(), state.home.as_deref()).and_then(|target| {
        let shown = target.dir.display().to_string();
        state
            .try_change_dir(target.dir)
            .map(|()| target.file)
            .map_err(|e| format!("Can't open {shown}: {e}"))
    });

    match gone {
        Ok(None) => false,
        Ok(Some(name)) => {
            // As the listing has it, which may be spelled differently from
            // what was typed (`..`, `/` for `\`).
            let path = state.cwd().join(name);
            state.select_file(&path);
            state.ui.rename_pending = None;
            true
        }
        Err(error) => {
            edit.error = Some(error);
            edit.matches.clear();
            state.ui.path_edit = Some(edit);
            // Enter took the focus from the box; give it back, with the
            // caret where it was.
            ui.memory_mut(|m| m.request_focus(edit_id()));
            false
        }
    }
}

/// Where a path typed into the path bar goes.
#[derive(Debug, PartialEq)]
struct Target {
    dir: PathBuf,
    /// The file in `dir` to select, when it's a file's path.
    file: Option<OsString>,
}

/// Where `text`, typed into the path bar, goes: a path from `cwd`, or from
/// `home` for one that starts with `~`, with environment variables filled
/// in as in a PATH entry (see [path_env::expand]). The quotes around a path
/// copied from elsewhere are dropped. `Err` says why it goes nowhere.
fn resolve(text: &str, cwd: &Path, home: Option<&Path>) -> Result<Target, String> {
    let text = text.trim();
    let text = text
        .strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .unwrap_or(text)
        .trim();
    if text.is_empty() {
        return Err("Type the path of a folder to go to".into());
    }

    let expanded = path_env::expand(text);
    let path = match expanded.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with(['/', '\\']) => {
            let home = home.ok_or("There's no home folder for ~ to stand for")?;
            let rest = rest.trim_start_matches(['/', '\\']);
            if rest.is_empty() {
                home.to_path_buf()
            } else {
                home.join(rest)
            }
        }
        _ => cwd.join(drive_root(&expanded)),
    };

    match fs::metadata(&path) {
        Ok(md) if md.is_dir() => Ok(Target {
            dir: path,
            file: None,
        }),
        Ok(_) => match (path.parent(), path.file_name()) {
            (Some(dir), Some(name)) => Ok(Target {
                dir: dir.to_path_buf(),
                file: Some(name.to_os_string()),
            }),
            _ => Err(format!("Can't find the folder {text} is in")),
        },
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            Err(missing(text, &path))
        }
        Err(e) => Err(format!("Can't open {text}: {e}")),
    }
}

/// On Windows, a bare drive (`D:`) as its root (`D:\`). On its own it stands
/// for that drive's current dir, which on the current drive is the cwd.
fn drive_root(path: &str) -> String {
    let b = path.as_bytes();
    if cfg!(windows) && b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        format!("{path}\\")
    } else {
        path.to_owned()
    }
}

/// Why `path`, typed as `text`, isn't there: the first part of it that's
/// missing, and from where.
fn missing(text: &str, path: &Path) -> String {
    let Some(base) = path.ancestors().skip(1).find(|a| a.exists()) else {
        return format!("Can't find {text}");
    };
    if !base.is_dir() {
        return format!("{} is a file, not a folder", base.display());
    }
    match path
        .strip_prefix(base)
        .ok()
        .and_then(|r| r.components().next())
    {
        Some(part) => format!(
            "There's no \"{}\" in {}",
            part.as_os_str().to_string_lossy(),
            base.display()
        ),
        None => format!("Can't find {text}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A new, empty dir for one test, with `sub/` and `sub/notes.txt` in it.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("peregrine_path_bar_{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub").join("notes.txt"), "").unwrap();
        dir
    }

    fn dir(path: PathBuf) -> Result<Target, String> {
        Ok(Target {
            dir: path,
            file: None,
        })
    }

    #[test]
    fn dirs_resolve_absolute_relative_quoted_and_from_home() {
        let root = scratch("dirs");
        let sub = root.join("sub");
        let abs = sub.display().to_string();

        assert_eq!(resolve(&abs, Path::new("/"), None), dir(sub.clone()));
        assert_eq!(resolve("sub", &root, None), dir(root.join("sub")));
        assert_eq!(
            resolve(&format!(" \"{abs}\" "), &root, None),
            dir(sub.clone())
        );
        assert_eq!(resolve("~", &root, Some(&sub)), dir(sub.clone()));
        assert_eq!(resolve("~/sub", &root, Some(&root)), dir(root.join("sub")));
        assert_eq!(resolve("~\\sub", &root, Some(&root)), dir(root.join("sub")));
    }

    #[test]
    fn a_file_resolves_to_its_dir_and_name() {
        let root = scratch("file");
        let target = resolve("sub/notes.txt", &root, None).unwrap();
        assert_eq!(target.dir, root.join("sub"));
        assert_eq!(target.file, Some(OsString::from("notes.txt")));
    }

    #[test]
    fn a_missing_path_names_the_part_missing() {
        let root = scratch("missing");
        let err = resolve("sub/nope/deeper", &root, None).unwrap_err();
        assert!(err.contains("\"nope\""), "{err}");
        assert!(
            err.contains(&root.join("sub").display().to_string()),
            "{err}"
        );

        let err = resolve("sub/notes.txt/inside", &root, None).unwrap_err();
        assert!(err.contains("is a file"), "{err}");

        assert!(resolve("  ", &root, None).is_err());
        assert!(resolve("~", &root, None).is_err());
    }

    #[test]
    fn crumbs_run_from_the_root_or_home() {
        let home = std::env::temp_dir().join("home");
        let cwd = home.join("code").join("app");
        let (trail, above) = crumbs(&cwd, Some(&home));
        let names: Vec<&str> = trail.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["~", "code", "app"]);
        assert_eq!(trail[0].dir, home);
        assert_eq!(trail[2].dir, cwd);
        assert_eq!(above.last(), home.parent().map(Path::to_path_buf).as_ref());
        assert!(above[0].parent().is_none());

        let (trail, above) = crumbs(&cwd, None);
        assert!(above.is_empty());
        assert!(trail[0].dir.parent().is_none());
        assert_eq!(trail.last().unwrap().name, "app");
    }

    #[test]
    fn roots_are_named_without_their_trailing_separator() {
        assert_eq!(dir_name(Path::new("/")), "/");
        if cfg!(windows) {
            assert_eq!(dir_name(Path::new("C:\\")), "C:");
            assert_eq!(dir_name(Path::new(r"\\server\share\")), r"\\server\share");
            assert_eq!(drive_root("d:"), "d:\\");
        }
    }
}
