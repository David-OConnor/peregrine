//! The project bar a group of buttons
//! for each kind of project the active tab's dir is (see [project::Project]).

use egui::{
    Align, Button, Color32, EventFilter, Frame, Id, IntoAtoms, Key, KeyboardShortcut, Layout,
    Modifiers, OpenUrl, Popup, RichText, TextEdit, TextStyle, Ui,
    text::{CCursor, CCursorRange},
};

use crate::{OutType, project, state::State, submit_command, ui, ui::terminal};

/// Clicks the Run button (see [run_button]) from anywhere in the window, as
/// in JetBrains IDEs. Cargo's, if Python's is shown too.
pub const RUN: KeyboardShortcut = KeyboardShortcut::new(Modifiers::SHIFT, Key::F10);

/// The Cargo group's buttons after Run (see [run_button]), and the line each
/// runs.
const CARGO_BUTTONS: [(&str, &str); 6] = [
    ("Run rel", "cargo run --release"),
    ("Build", "cargo build"),
    ("Build rel", "cargo build --release"),
    ("Test", "cargo test"),
    ("Fmt", "cargo +nightly fmt"),
    ("Clippy", "cargo clippy"),
];

pub(super) const BUSY: &str = "A command is already running in this tab: stop it first (Ctrl+C)";

/// The green of Run's "play" triangle and the red of Stop's square (see
/// [run_button]), in light and dark mode, as IDEs color them.
const PLAY_LIGHT: Color32 = Color32::from_rgb(40, 140, 50);
const PLAY_DARK: Color32 = Color32::from_rgb(110, 200, 110);
const STOP_LIGHT: Color32 = Color32::from_rgb(205, 50, 50);
const STOP_DARK: Color32 = Color32::from_rgb(235, 95, 85);

/// A project group's background in light and dark mode (see [tinted_group]).
struct Tint {
    light: Color32,
    dark: Color32,
}

// Each keeps its brand color's hue, with the saturation and brightness cut
// so text reads on it as well as on the bare panel: a pale tint in light
// mode, and in dark mode a muted one, darker than the theme's buttons so they
// still stand out.

/// Git's logo orange, #F05032. In light mode about as light as the logo,
/// with its chroma cut a little, as any paler tint of it reads as salmon or
/// peach, not orange; the group labels are drawn strong (black) so they
/// still read on it. In dark mode with more chroma than the other groups.
const GIT_TINT: Tint = Tint {
    light: Color32::from_rgb(233, 121, 91),
    dark: Color32::from_rgb(69, 33, 25),
};
/// The Rust Foundation's Rust Orange, #D34516: desaturated more than Git's
/// (its hue is close), and turned a little toward yellow, as GitHub's tan
/// for Rust (#DEA584) is, so it reads as brown.
const CARGO_TINT: Tint = Tint {
    light: Color32::from_rgb(236, 220, 210),
    dark: Color32::from_rgb(54, 41, 32),
};
/// The Python logo's yellow, #FFD43B.
const PYTHON_TINT: Tint = Tint {
    light: Color32::from_rgb(248, 234, 186),
    dark: Color32::from_rgb(52, 46, 24),
};

/// Where the width the bar took last frame is kept (see [last_width]).
fn width_id() -> Id {
    Id::new("project_bar_width")
}

/// The width the bar took when last drawn, for the tab strip to tell whether
/// it fits beside the tabs; 0 before it's been drawn.
pub(super) fn last_width(ui: &Ui) -> f32 {
    ui.data(|d| d.get_temp(width_id())).unwrap_or(0.)
}

/// Lay `add_contents` out as a project group on a band of `tint`. In light
/// mode its buttons take the panel's near-white, since the theme's light gray
/// ones would all but vanish into a pale tint.
fn tinted_group(ui: &mut Ui, tint: Tint, add_contents: impl FnOnce(&mut Ui)) {
    let dark = ui.visuals().dark_mode;
    Frame::new()
        .fill(if dark { tint.dark } else { tint.light })
        .corner_radius(ui.visuals().widgets.inactive.corner_radius)
        .inner_margin(egui::Margin::symmetric(4, 2))
        .show(ui, |ui| {
            if !dark {
                let panel = ui.visuals().panel_fill;
                let button = &mut ui.visuals_mut().widgets.inactive;
                button.weak_bg_fill = panel;
                button.bg_fill = panel;
            }
            add_contents(ui);
        });
}

/// Right-aligned in the rest of `ui`'s row: Git while the dir's in a repo (as
/// the prompt's branch shows), with a button for each branch active lately
/// (see [project::Branches::recent]) and one opening the repo's web page (see
/// [web_button]), Cargo, Python, each on a band of its brand's
/// color (see [tinted_group]). Each button runs its line as if typed, so
/// it's echoed, kept in history, and runs in the background like any other,
/// with `python` the venv's. Not while connected over SSH, where typed lines
/// go to the remote, and disabled while a command's already running in this
/// tab.
pub(super) fn project_bar(state: &mut State, ui: &mut Ui) {
    let width = ui
        .with_layout(Layout::right_to_left(Align::Center), |ui| groups(state, ui))
        .response
        .rect
        .width();

    // Lay the frame out again at once when the width changes (the dir's
    // project kind did), so the tab strip doesn't show it in the wrong place.
    if (width - last_width(ui)).abs() > 0.5 {
        ui.data_mut(|d| d.insert_temp(width_id(), width));
        ui.ctx().request_discard("project bar width changed");
    }
}

/// [project_bar]'s groups. They're laid out right to left, so each group and
/// each button in it is added in reverse.
fn groups(state: &mut State, ui: &mut Ui) {
    let branch = state.branch[state.ui.active_tab].clone();
    let proj = state.project;
    if state.is_remote() || (branch.is_none() && !proj.cargo && !proj.python) {
        return;
    }

    let idle = state.running_here().is_none();
    let dir = state.cwd().to_path_buf();
    // The line to run, or why there's none.
    let mut line: Option<Result<String, String>> = None;
    let mut stop = false;

    if proj.python {
        tinted_group(ui, PYTHON_TINT, |ui| {
            let black_hover = "Format every Python file here with black";
            if cmd_button(ui, idle, "Black", black_hover) {
                line = Some(Ok(project::python_black(&dir).to_owned()));
            }
            let run_hover =
                "Run this Python application. Uses `uv` and/or the included venv, if available.";
            if run_button(ui, idle, run_hover, !proj.cargo, &mut stop) {
                line = Some(project::python_run(&dir));
            }
            ui.strong("Python:");
        });
    }

    if proj.cargo {
        tinted_group(ui, CARGO_TINT, |ui| {
            for &(label, cmd) in CARGO_BUTTONS.iter().rev() {
                if cmd_button(ui, idle, label, cmd) {
                    line = Some(Ok(cmd.to_owned()));
                }
            }

            if run_button(ui, idle, "cargo run", true, &mut stop) {
                line = Some(Ok("cargo run".to_owned()));
            }

            ui.strong("Cargo:");
        });
    }

    if let Some(branch) = &branch {
        state.load_git_branches();
        let branches = state.git_branches.as_ref();
        let names = branches.map_or(&[][..], |b| &b.names);
        let recent = branches.map_or(&[][..], |b| &b.recent);
        let web_url = branches.and_then(|b| b.web_url.as_deref());
        let root = branches.and_then(|b| b.root.clone());
        let projects_open = root.is_some() && state.ui.projects.repo() == root.as_deref();
        let mut reload_branches = false;
        let mut toggle_projects = false;

        tinted_group(ui, GIT_TINT, |ui| {
            // The latest first, from the left.
            let shown = &recent[..recent.len().min(project::MAX_RECENT_BRANCHES)];
            for name in shown.iter().rev() {
                if name == branch {
                    ui.add(Button::selectable(true, name.as_str()))
                        .on_hover_text("The current branch");
                } else if cmd_button(ui, idle, name.as_str(), &format!("git switch {name}")) {
                    line = Some(project::git_branch(&dir, name));
                }
            }
            if !shown.is_empty() {
                ui.separator();
            }

            let input = &mut state.ui.git_branch_input;
            let (entered, gained_focus) = branch_box(ui, input, branch, names);
            // Read afresh, for any made outside since.
            reload_branches = gained_focus;

            let typed = !input.trim().is_empty();
            let clicked = ui
                .add_enabled(idle && typed, Button::new("Branch"))
                .on_hover_text(
                    "Switch to the branch named in the box (git switch), \
                     or create it if there's none by that name (git switch -c)",
                )
                .on_disabled_hover_text(if idle {
                    "Type a branch name in the box"
                } else {
                    BUSY
                })
                .clicked();

            if idle && typed && (clicked || entered) {
                let result = project::git_branch(&dir, input);
                if result.is_ok() {
                    input.clear();
                }
                line = Some(result);
            }

            for (label, cmd) in [("Push", "git push"), ("Pull", "git pull")] {
                if cmd_button(ui, idle, label, cmd) {
                    line = Some(Ok(cmd.to_owned()));
                }
            }

            if root.is_some() {
                ui.separator();
                toggle_projects = ui
                    .add(Button::selectable(projects_open, "Projects"))
                    .on_hover_text(
                        "List the repos in the workspace this one was cloned from, \
                         to clone beside it or pull",
                    )
                    .clicked();
            }
            web_button(ui, web_url);
            ui.strong("Git:");
        });

        if reload_branches {
            state.git_branches = None;
        }
        if toggle_projects && let Some(root) = root {
            state.ui.projects.toggle(root, ui.ctx());
        }
    }

    if stop {
        state.stop_running();
    }
    match line {
        Some(Ok(line)) => submit_command(state, &line),
        Some(Err(e)) => state.push_out(e, OutType::StdErr),
        None => {}
    }
}

/// The most branches [branch_box] lists under itself.
const MAX_BRANCH_MATCHES: usize = 10;

/// The Git group's Branch box, holding `input`, with the `current` branch as
/// its hint. While it's typed in, the branches of `names` that match it (see
/// [branch_matches]) are listed under it: ↑ / ↓ pick one, and Tab or a click
/// puts it in the box; Enter with one picked switches to that one. Returns
/// whether Enter was pressed, to switch to what's then in the box, and
/// whether the box just got the keyboard.
fn branch_box(ui: &mut Ui, input: &mut String, current: &str, names: &[String]) -> (bool, bool) {
    let id = Id::new("git_branch_input");
    let pick_id = id.with("pick");
    let mut matches = branch_matches(names, input);
    let mut pick = ui
        .data(|d| d.get_temp::<usize>(pick_id))
        .filter(|&i| i < matches.len());

    // Taken before the box sees them.
    let focused = ui.memory(|m| m.has_focus(id));
    let (mut up, mut down, mut tab) = (false, false, false);
    if focused && !matches.is_empty() {
        ui.input_mut(|i| {
            up = i.consume_key(Modifiers::NONE, Key::ArrowUp);
            down = i.consume_key(Modifiers::NONE, Key::ArrowDown);
            tab = i.consume_key(Modifiers::NONE, Key::Tab);
        });
    }
    let n = matches.len();
    if down {
        pick = Some(pick.map_or(0, |i| (i + 1) % n));
    }
    if up {
        pick = Some(pick.map_or(n - 1, |i| (i + n - 1) % n));
    }
    let mut fill = tab.then(|| matches[pick.unwrap_or(0)].clone());

    // Tab and Up / Down stay with the box, rather than move the focus to
    // another widget.
    let filter = EventFilter {
        tab: true,
        horizontal_arrows: true,
        vertical_arrows: true,
        escape: false,
    };
    let resp = ui
        .add(
            TextEdit::singleline(input)
                .id(id)
                .event_filter(filter)
                .hint_text(current)
                .desired_width(ui::chars_width(ui, TextStyle::Body, 14.0)),
        )
        .on_hover_text(
            "A branch to switch to, or create. The branches matching what's typed are \
             listed under it: ↑ / ↓ to pick one, Tab to fill it in",
        );
    let entered = resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));

    if entered && let Some(i) = pick {
        *input = matches[i].clone();
    }
    if resp.changed() {
        pick = None;
        matches = branch_matches(names, input);
    }

    // While it has the keyboard, or has just lost it to a click on the list.
    let listed = (resp.has_focus() || resp.lost_focus()) && !entered && !matches.is_empty();
    if listed {
        Popup::from_response(&resp)
            .open(true)
            .width(resp.rect.width())
            .show(|ui| {
                for (i, name) in matches.iter().enumerate() {
                    let row = ui.add(Button::selectable(pick == Some(i), name.as_str()));
                    if row.clicked() {
                        fill = Some(name.clone());
                    }
                    if pick == Some(i) && (up || down) {
                        row.scroll_to_me(None);
                    }
                }
            });
    }

    if let Some(name) = fill {
        *input = name;
        pick = None;
        // Back in the box, with the caret at the end.
        ui.memory_mut(|m| m.request_focus(id));
        if let Some(mut state) = TextEdit::load_state(ui.ctx(), id) {
            let end = CCursor::new(input.chars().count());
            state.cursor.set_char_range(Some(CCursorRange::one(end)));
            state.store(ui.ctx(), id);
        }
    }

    ui.data_mut(|d| match pick {
        Some(i) => {
            d.insert_temp(pick_id, i);
        }
        None => d.remove::<usize>(pick_id),
    });
    (entered, resp.gained_focus())
}

/// The branches of `names` (sorted) that `typed` matches, ignoring case: those
/// it starts, then those it's in elsewhere. Not one it already names
/// exactly. None until something's typed. At most [MAX_BRANCH_MATCHES].
fn branch_matches(names: &[String], typed: &str) -> Vec<String> {
    let typed = typed.trim();
    if typed.is_empty() {
        return Vec::new();
    }
    let needle = typed.to_lowercase();
    let (starts, within): (Vec<&String>, Vec<&String>) = names
        .iter()
        .filter(|name| *name != typed && name.to_lowercase().contains(&needle))
        .partition(|name| name.to_lowercase().starts_with(&needle));

    starts
        .into_iter()
        .chain(within)
        .take(MAX_BRANCH_MATCHES)
        .cloned()
        .collect()
}

/// While a
/// command's running in this tab, a Stop button with their red square
/// instead, ending it as the Stop by the input line does: that sets `stop`.
/// Returns whether Run was clicked, or, if `shortcut`, [RUN] pressed. [RUN]
/// is left alone while a command's running, for the program to get.
fn run_button(ui: &mut Ui, idle: bool, hover: &str, shortcut: bool, stop: &mut bool) -> bool {
    let dark = ui.visuals().dark_mode;
    if idle {
        let play = RichText::new("⏵").color(if dark { PLAY_DARK } else { PLAY_LIGHT });
        let pressed = shortcut && terminal::take_shortcut(ui.ctx(), RUN);
        let hover = if shortcut {
            format!("{hover} ({})", ui.ctx().format_shortcut(&RUN))
        } else {
            hover.to_owned()
        };
        cmd_button(ui, idle, (play, "Run"), &hover) || pressed
    } else {
        let square = RichText::new("⏹").color(if dark { STOP_DARK } else { STOP_LIGHT });
        *stop |= ui
            .button((square, "Stop"))
            .on_hover_text("End the command and everything it started (Ctrl+C)")
            .clicked();
        false
    }
}

/// The Git group's 🌐 button, opening the repo's page on its host (`url`;
/// see [project::web_url]) in the browser. It runs no command, so it's left
/// enabled while one's running.
fn web_button(ui: &mut Ui, url: Option<&str>) {
    let clicked = ui
        .add_enabled(url.is_some(), Button::new("🌐"))
        .on_hover_text(format!(
            "Open this repo's page in the browser: {}",
            url.unwrap_or_default()
        ))
        .on_disabled_hover_text("This repo has no remote with a web page")
        .clicked();
    if clicked && let Some(url) = url {
        ui.ctx().open_url(OpenUrl::new_tab(url));
    }
}

/// A project-bar button, saying on hover what it runs.
fn cmd_button<'a>(ui: &mut Ui, idle: bool, label: impl IntoAtoms<'a>, hover: &str) -> bool {
    ui.add_enabled(idle, Button::new(label))
        .on_hover_text(hover)
        .on_disabled_hover_text(BUSY)
        .clicked()
}
