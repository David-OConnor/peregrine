//! Program information, updates, and the location of the state shared with
//! Shell.

use std::{path::Path, time::Duration};

use egui::{
    CollapsingHeader, Context, Grid, ProgressBar, RichText, ScrollArea, Ui, ViewportCommand,
    Window, os::OperatingSystem,
};
use software_manager::{Application, UpdateStatus};

use crate::{VERSION, files::open_with_default_app, render_with_tilde, state::State};

const REPO_URL: &str = "https://github.com/David-OConnor/peregrine";

/// Check GitHub for a newer release, download it, and install it. Installing
/// waits for Peregrine to close, since its files can't be replaced while it
/// runs; the release's own setup script then installs it, and Peregrine
/// reopens.
fn update_section(state: &mut State, ui: &mut Ui) {
    let updater = &mut state.ui.updater;
    updater.poll();

    if updater.busy() {
        // So the download progress moves.
        ui.ctx().request_repaint_after(Duration::from_millis(100));
    }

    let mut check = false;
    let mut download = false;
    let mut install = false;

    ui.horizontal_wrapped(|ui| match updater.status() {
        UpdateStatus::Idle => {
            check = ui.button("Check for updates").clicked();
        }
        UpdateStatus::Checking(_) => {
            ui.spinner();
            ui.label("Checking for updates…");
        }
        UpdateStatus::UpToDate => {
            ui.label("This is the latest version.");
            check = ui.button("Check again").clicked();
        }
        UpdateStatus::Available(release) => {
            ui.label(format!("Version {} is available.", release.version));
            ui.hyperlink_to("Release notes", &release.page_url);

            download = ui
                .button("Download and install")
                .on_hover_text(format!(
                    "Download the update ({:.0} MB). Installing it is the next step.",
                    release.size as f64 / 1_000_000.
                ))
                .clicked();
        }
        UpdateStatus::Downloading {
            release, progress, ..
        } => {
            ui.add(
                ProgressBar::new(progress.fraction())
                    .text(format!("Downloading {}…", release.version))
                    .desired_width(300.),
            );
        }
        UpdateStatus::Ready(staged) => {
            ui.label(format!("Version {} is downloaded.", staged.release.version));

            install = ui
                .button("Restart and install")
                .on_hover_text(
                    "Close Peregrine, install the update, then reopen it. Running \
                    commands are stopped, as when closing it.",
                )
                .clicked();
        }
        UpdateStatus::Installing => {
            ui.label("Installing…");
        }
        UpdateStatus::Failed(e) => {
            ui.label(
                RichText::new(format!("Unable to update: {e}"))
                    .color(ui.visuals().error_fg_color)
                    .small(),
            );
            check = ui.button("Try again").clicked();
        }
    });

    let ctx = ui.ctx().clone();
    let repaint = {
        let ctx = ctx.clone();
        move || ctx.request_repaint()
    };

    if check {
        updater.check(Application::Peregrine, VERSION, repaint);
    } else if download {
        updater.download(repaint);
    } else if install && updater.install_on_exit() {
        // The installer is waiting for this process to end. Closing saves
        // state, in `on_exit`.
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }
}

pub(super) fn show(state: &mut State, ctx: &Context) {
    if !state.ui.about {
        return;
    }

    // Keep the close button's flag separate while the contents borrow state.
    let mut open = true;
    Window::new("About")
        .open(&mut open)
        // Clamped to the screen, and shrunk to the contents if they're
        // shorter. Its width follows the shortcut columns.
        .default_height(900.)
        .resizable(true)
        .show(ctx, |ui| {
            super::scale_panel_fonts(ui, state.gui_font_size);

            ui.horizontal(|ui| {
                ui.heading("Peregrine");
                ui.label(RichText::new(format!("v{VERSION}")).weak());
            });

            ui.add_space(8.);
            ui.label("An integrated terminal and file browser.");

            ui.add_space(8.);
            update_section(state, ui);

            ui.add_space(8.);
            ui.horizontal_wrapped(|ui| {
                ui.label("Source code:");
                ui.hyperlink_to(REPO_URL, REPO_URL);
            });

            ui.add_space(8.);
            ui.label("Preferences, bookmarks, command history, and clipboard history are saved in the state file shared with Shell:");
            ui.label(render_with_tilde(&state.state_path, state.home.as_deref()))
                .on_hover_text(state.state_path.to_string_lossy());

            if ui.button("Open containing folder").clicked() {
                let folder = state
                    .state_path
                    .parent()
                    .filter(|path| !path.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
                state.ui.about_folder_error = open_with_default_app(folder)
                    .err()
                    .map(|error| error.to_string());
            }

            if let Some(error) = &state.ui.about_folder_error {
                ui.label(
                    RichText::new(format!("Could not open the state file's folder: {error}"))
                        .color(ui.visuals().error_fg_color)
                        .small(),
                );
            }

            ui.add_space(8.);
            ui.separator();
            shortcuts_section(ui);
        });
    state.ui.about = open;
}

/// A list of keys and what they do.
type Shortcuts = Vec<(String, &'static str)>;

/// Every keyboard shortcut, grouped by where it works, with this OS's keys.
fn shortcut_groups(mac: bool) -> Vec<(&'static str, Shortcuts)> {
    let cmd = if mac { "Cmd" } else { "Ctrl" };
    // Option + a digit or letter types a symbol on Mac.
    let alt = if mac { "Cmd + Option" } else { "Alt" };

    let anywhere = vec![
        ("Tab".into(), "Focus the CLI input, and clear it"),
        (
            format!("{cmd} + B"),
            "Bookmark the current directory, or remove its bookmark",
        ),
        (
            format!("{alt} + 0 to 9"),
            "Go to that bookmark, by its number in the Bookmarks panel",
        ),
        (
            format!("{cmd} + C / X / V"),
            "Copy, cut, and paste, with clipboard history",
        ),
        ("Ctrl + Shift + V".into(), "Show or hide the clipboard pane"),
        (
            if mac { "Option + F12" } else { "Alt + F12" }.into(),
            "Show or hide the terminal pane",
        ),
    ];

    // Ctrl + N, T and W are tab shortcuts, except on Mac, where those use Cmd.
    let history_keys = if mac {
        "Up / Down or Ctrl + P / N"
    } else {
        "Up / Down or Ctrl + P"
    };
    let cli = vec![
        ("Enter".into(), "Run the input"),
        ("Tab".into(), "Autocomplete the input"),
        (history_keys.into(), "Walk command history"),
        (
            "Left / Right".into(),
            "Walk recent directories, while the input is empty",
        ),
        (
            "Right / End / Ctrl + E".into(),
            "Accept the dimmed suggestion, with the caret at the end",
        ),
        (
            format!("{cmd} + C"),
            "Interrupt the running command, or else clear the input, when no text is selected",
        ),
        (
            "Ctrl + D".into(),
            "Delete the character under the caret. On an empty line, end a running \
            program's input, or `exit`",
        ),
        ("Ctrl + L".into(), "Clear the output"),
    ];

    let mut line_editing = vec![
        ("Ctrl + A / Home".into(), "Move to the start of the line"),
        ("Ctrl + E / End".into(), "Move to the end of the line"),
        ("Ctrl + Left / Right".into(), "Move back / forward a word"),
        (
            if mac { "Ctrl + W" } else { "Ctrl + Backspace" }.into(),
            "Delete the word before the caret",
        ),
        (format!("{alt} + D"), "Delete the word after the caret"),
        ("Ctrl + K".into(), "Delete to the end of the line"),
        ("Ctrl + U".into(), "Delete to the start of the line"),
        ("Ctrl + Y".into(), "Paste the last deleted text"),
        (format!("{alt} + T"), "Swap the words around the caret"),
        (
            format!("{alt} + C / U / L"),
            "Capitalize / uppercase / lowercase the word at the caret",
        ),
        (format!("Ctrl + _ or {cmd} + Z"), "Undo"),
    ];
    if mac {
        line_editing.insert(
            8,
            ("Ctrl + T".into(), "Swap the characters around the caret"),
        );
    }

    let mut tabs = vec![
        (
            format!("{cmd} + T or {cmd} + N"),
            "New tab, in the current tab's directory",
        ),
        (format!("{cmd} + W"), "Close the current tab"),
        (format!("{cmd} + Shift + T"), "Reopen the last closed tab"),
        ("Ctrl + Tab or Ctrl + PageDown".into(), "Next tab"),
        ("Ctrl + Shift + Tab or Ctrl + PageUp".into(), "Previous tab"),
        (
            format!("{cmd} + 1 to 8"),
            "Jump to that tab, counting from the left",
        ),
        (format!("{cmd} + 9"), "Jump to the last tab"),
    ];
    if mac {
        tabs.insert(
            5,
            ("Cmd + Option + Right / Left".into(), "Next / previous tab"),
        );
    }

    let files = vec![
        ("Enter".into(), "Open the selected file"),
        ("Arrow keys".into(), "Move the selection"),
        ("Shift + arrow keys".into(), "Extend the selection"),
        (format!("{cmd} + A"), "Select everything shown"),
        ("Escape".into(), "Clear the selection"),
        (
            "Delete".into(),
            if cfg!(windows) {
                "Move the selection to the Recycle Bin, after confirming"
            } else {
                "Move the selection to the Trash, after confirming"
            },
        ),
        (format!("{cmd} + Shift + N"), "New folder"),
        (
            format!("{cmd} + C / X / V"),
            "Copy, cut, and paste the selected files",
        ),
    ];

    let viewer = vec![
        (format!("{cmd} + F"), "Find"),
        (
            "Enter / Shift + Enter".into(),
            "Next / previous match, in the find box",
        ),
        ("F3 / Shift + F3".into(), "Next / previous match"),
        (
            format!("{cmd} + G / {cmd} + Shift + G"),
            "Next / previous match",
        ),
        (format!("{alt} + C"), "Toggle match case"),
        ("Escape".into(), "Clear the search, in the find box"),
        ("F5".into(), "Reload the file"),
        (
            "Left / Right".into(),
            "Previous / next image in the directory",
        ),
    ];

    vec![
        ("Anywhere", anywhere),
        ("CLI input", cli),
        ("Line editing", line_editing),
        ("Tabs", tabs),
        ("Files panel", files),
        ("File viewer", viewer),
    ]
}

/// Where to split [shortcut_groups] into two columns, so the taller column is
/// as short as it can be. Each group's header counts as a row.
fn column_split(groups: &[(&'static str, Shortcuts)]) -> usize {
    let height = |groups: &[(&'static str, Shortcuts)]| {
        groups.iter().map(|(_, rows)| rows.len() + 1).sum::<usize>()
    };
    (0..=groups.len())
        .min_by_key(|&i| height(&groups[..i]).max(height(&groups[i..])))
        .unwrap_or(0)
}

/// One group of shortcuts, under a header that folds it away.
fn shortcut_group(ui: &mut Ui, title: &'static str, rows: Shortcuts) {
    CollapsingHeader::new(title)
        .default_open(true)
        .show(ui, |ui| {
            Grid::new(("about_shortcuts", title))
                .num_columns(2)
                .spacing([12., 4.])
                .striped(true)
                .show(ui, |ui| {
                    for (keys, what) in rows {
                        ui.label(RichText::new(keys).monospace());
                        ui.label(what);
                        ui.end_row();
                    }
                });
        });
}

/// [shortcut_groups], in two columns. Scrolls when the window is shorter than
/// them.
fn shortcuts_section(ui: &mut Ui) {
    ui.strong("Keyboard shortcuts");

    let mac = ui.ctx().os() == OperatingSystem::Mac;
    let mut left = shortcut_groups(mac);
    let right = left.split_off(column_split(&left));

    ScrollArea::vertical()
        .id_salt("about_shortcuts")
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.columns_const(|[left_col, right_col]| {
                for (title, rows) in left {
                    shortcut_group(left_col, title, rows);
                }
                for (title, rows) in right {
                    shortcut_group(right_col, title, rows);
                }
            });
        });
}
