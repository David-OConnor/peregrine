// Disables the terminal window. Use this for releases, and disable when debugging.
#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod ansi;
mod clipboard;
mod cmd_color;
mod completion;
mod deep_search;
mod drag_out;
mod files;
mod markdown;
mod path_color;
mod path_env;
mod placement;
mod prefs;
mod project;
mod pty;
mod remote;
mod running;
mod state;
mod term;
mod terminal;
mod ui;
mod util;
mod viewer;

use std::{
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};

use chrono::Utc;
use shell::{HistoryItem, NavState, commands::OutKind, save_data};
use state::State;

use crate::{
    ui::{MIN_CENTRAL_WIDTH, WINDOW_HEIGHT, WINDOW_WIDTH},
    util::handle_cmd,
};

/// The application version, taken from Cargo.toml's `[package] version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Copy, PartialEq)]
pub enum OutType {
    Prompt,
    StdOut,
    StdErr,
    /// Stderr from a process that exited successfully (warnings, progress);
    /// shown amber rather than red. See [OutKind::StderrOk].
    StdErrOk,
}

/// Map the shared `commands` module's stream kind onto the GUI's output
/// row type. Used by the `sync` / `logs` sinks.
fn out_type_for(kind: OutKind) -> OutType {
    match kind {
        OutKind::Stdout => OutType::StdOut,
        OutKind::Stderr => OutType::StdErr,
        OutKind::StderrOk => OutType::StdErrOk,
    }
}

/// The app icon, for the title bar and taskbar button of every window (the
/// main one, the file viewers, the PATH editor). Decoded once and shared, so
/// the child viewports' builders, rebuilt every frame, compare equal by
/// pointer. The `.exe`'s own icon, which Explorer, shortcuts and Start show,
/// is `resources/icon.ico`, embedded by `build.rs`.
fn app_icon() -> Arc<egui::IconData> {
    static ICON: LazyLock<Arc<egui::IconData>> = LazyLock::new(|| {
        let png = include_bytes!("../resources/icon.png");
        Arc::new(eframe::icon_data::from_png_bytes(png).expect("resources/icon.png is a valid PNG"))
    });

    Arc::clone(&ICON)
}

/// Render a path as `~/relative` when it lives under the home directory;
/// otherwise use the absolute form. Uses forward slashes after the tilde for
/// consistency with the rest of the shell.
pub fn render_with_tilde(p: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home {
        if let Ok(rest) = p.strip_prefix(home) {
            let rest_str = rest.to_string_lossy().replace('\\', "/");
            if rest_str.is_empty() {
                return "~".to_string();
            }
            return format!("~/{}", rest_str);
        }
    }
    p.display().to_string()
}

/// Echo a command line into the output pane and queue it to run at the start
/// of the next frame (see `ui::draw`). Some built-ins (`sync`, an `ssh`
/// connect) block the UI thread while they run, so deferring by a frame lets
/// the echo (and the cleared input row) paint before the wait, rather than
/// appearing only once the command is done. Shell commands run in the
/// background (see [running]).
pub fn submit_command(state: &mut State, input: &str) {
    let input = input.trim();
    if input.is_empty() {
        return;
    }

    // Echo the prompt + command into the output pane so the user can see
    // what they ran (mirrors how a normal terminal scrolls). Capture the
    // recall state first so the echo preserves any `his N` / `cd N`
    // indicator, then submitting commits us back to the live input — drop
    // recall state so the next arrow starts walking from the newest entry.
    let (his_cursor, cd_cursor) = state.active_nav_cursors();
    let nav_snapshot = NavState {
        his_cursor,
        cd_cursor,
        ..NavState::new()
    };
    let prompt = state.prompt(&nav_snapshot);
    state.reset_history_nav();
    state.push_out(format!("{prompt}> {input}"), OutType::Prompt);

    state.ui.pending_cmd = Some(input.to_string());
}

/// Runs one command line, already echoed by [submit_command]. Output is
/// pushed into `state.ui.out` rather than printed to stdout/stderr, so the
/// GUI's terminal pane can show it.
pub fn run_command(state: &mut State, input: &str, ctx: &egui::Context) {
    let (cmd, args) = match input.find(char::is_whitespace) {
        Some(i) => (&input[..i], input[i..].trim()),
        None => (input, ""),
    };

    // History recall and page listing use the same absolute indices as the
    // visible history panels. Do not store the recall invocation itself.
    if cmd == "his" || cmd == "hist" || cmd == "this" {
        let cwd = (cmd == "this").then_some(state.cwd());
        if args.is_empty()
            || args
                .strip_prefix('p')
                .and_then(|n| n.parse::<usize>().ok())
                .is_some()
        {
            let page = args
                .strip_prefix('p')
                .and_then(|n| n.parse::<usize>().ok())
                .unwrap_or(1)
                .saturating_sub(1);
            let listing = if cmd == "this" {
                shell::render_history_in_dir(&state.history, state.cwd(), page)
            } else {
                shell::render_history(&state.history, page)
            };
            state.push_out(listing, OutType::StdOut);
            return;
        }
        let resolved = shell::find_history_index(&state.history, cwd, args)
            .and_then(|idx| state.history.get(idx).map(|item| item.text.clone()));
        match resolved {
            Some(text) => submit_command(state, &text),
            None => state.push_out(
                format!("{cmd}: no matching history item for `{args}`"),
                OutType::StdErr,
            ),
        }
        return;
    }

    // `hisd <n>` re-runs a previous history item in its original working
    // directory, without changing the GUI's CWD. Bypasses the built-in
    // dispatcher and shells the command out directly, since the point is to
    // run it elsewhere on the filesystem.
    if cmd == "hisd" {
        match args.parse::<usize>() {
            Ok(idx) => match state
                .history
                .get(idx)
                .map(|item| (item.text.clone(), item.dir.clone()))
            {
                Some((text, dir)) => {
                    // Expand shorthands (`run`, `pull`, …) the same way a
                    // typed command would be; pwsh has never heard of them.
                    let line = shell::commands::expand_input(
                        &text,
                        state.clone_roots.last().map(String::as_str),
                        Some(&dir),
                    )
                    .line;
                    state.start_running(&line, &dir, None, ctx);
                    return;
                }
                None => state.push_out(
                    format!("hisd: no history item at index {idx}"),
                    OutType::StdErr,
                ),
            },
            Err(_) => state.push_out("hisd: usage: hisd <number>", OutType::StdErr),
        }
        return;
    }

    state.history.push(HistoryItem {
        text: input.to_string(),
        dir: state.cwd().to_path_buf(),
        dt: Utc::now(),
    });

    state.refresh_history_filter();
    state.refresh_cwd_history_filter();
    state.refresh_cmd_colors();

    // Track directories we've run real commands from (everything except `cd`).
    // Persist regardless so the new history entry hits disk.
    if cmd != "cd" {
        let cwd = state.cwd().to_path_buf();
        state.record_recent_dir(&cwd);
    }

    if let Err(e) = state.save() {
        state.push_out(
            format!("warning: failed to save state: {e}"),
            OutType::StdErr,
        );
    }

    handle_cmd(state, cmd, input, args, ctx);

    // After every command, re-detect the branch: cwd may have moved (cd),
    // or the command may have switched branches (`git checkout`). Refresh
    // the file-browser listing too — a passthrough command may have
    // created or removed files in the cwd.
    state.refresh_branch();
    state.refresh_browser_files();
}

fn main() {
    // Started elevated by the PATH panel just to write the System PATH:
    // do that and exit, without opening a window.
    if let Some(code) = path_env::run_elevated_helper() {
        std::process::exit(code);
    }

    let state_path =
        save_data::default_path().unwrap_or_else(|| PathBuf::from(save_data::FILENAME));

    let mut state = State::load(state_path).unwrap_or_default();

    let options = eframe::NativeOptions {
        // eframe reopens the window where the user left it (`persist_window`,
        // on by default), so this size is only for the first run. Floor the
        // window width at the central column's minimum so the side panels can
        // always honor [MIN_CENTRAL_WIDTH]; below this the central panel
        // would have nowhere left to keep its width.
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([WINDOW_WIDTH, WINDOW_HEIGHT])
            .with_min_inner_size([MIN_CENTRAL_WIDTH, 200.])
            .with_icon(app_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "Peregrine",
        options,
        Box::new(|cc| {
            ui::install_fonts(&cc.egui_ctx);
            // eframe has already restored egui's memory, theme included:
            // without a saved choice, egui follows the OS theme.
            if let Some(storage) = cc.storage {
                prefs::load(&mut state, storage);
            }
            Ok(Box::new(state))
        }),
    )
    .unwrap();
}
