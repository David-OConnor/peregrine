// Disables the terminal window. Use this for releases, and disable when debugging.
// #![cfg_attr(
//     all(not(debug_assertions), target_os = "windows"),
//     windows_subsystem = "windows"
// )]

mod ansi;
mod cmd_color;
mod files;
mod markdown;
mod path_color;
mod state;
mod ui;
mod util;

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use shell::{HistoryItem, NavState, RecentDir, commands::OutKind, save_data};
use state::State;

use crate::{
    ui::{MIN_CENTRAL_WIDTH, WINDOW_HEIGHT, WINDOW_WIDTH},
    util::handle_cmd,
};

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

/// One line / chunk of terminal output. Lines from the same command are kept
/// as separate items so they can be coloured by stream.
pub struct OutItem {
    pub text: String,
    pub type_: OutType,
    pub dt: DateTime<Utc>,
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

/// Record `cwd` in the recent-dirs list. If the path is already present we
/// remove the old entry and push a fresh one to the end so the list stays
/// deduped and the newest entry sits at the top of the display.
fn record_recent_dir(recent: &mut Vec<RecentDir>, cwd: &Path) {
    recent.retain(|r| r.path != cwd);
    recent.push(RecentDir {
        path: cwd.to_path_buf(),
        dt: Utc::now(),
    });
}

/// Echo a command line into the output pane and queue it to run at the start
/// of the next frame (see `ui::draw`). Commands block the UI thread while they
/// run, so deferring by a frame lets the echo (and the cleared input row)
/// paint before the wait, rather than appearing only once the command is done.
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
pub fn run_command(state: &mut State, input: &str) {
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
                    util::run_passthrough(state, &line, &dir);
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
        record_recent_dir(&mut state.recent_dirs, &cwd);
        state.refresh_recent_dirs_filter();
    }
    if let Err(e) = state.save() {
        state.push_out(
            format!("warning: failed to save state: {e}"),
            OutType::StdErr,
        );
    }

    handle_cmd(state, cmd, input, args);

    // After every command, re-detect the branch: cwd may have moved (cd),
    // or the command may have switched branches (`git checkout`). Refresh
    // the file-browser listing too — a passthrough command may have
    // created or removed files in the cwd.
    state.refresh_branch();
    state.refresh_browser_files();
}

fn main() {
    let state_path =
        save_data::default_path().unwrap_or_else(|| PathBuf::from(save_data::FILENAME));

    let state = State::load(state_path).unwrap_or_default();

    // Reopen at the size the user left the window, falling back to the
    // built-in default when there's no saved size yet.
    let inner_size = state
        .window_size
        .map(|ws| [ws.x, ws.y])
        .unwrap_or([WINDOW_WIDTH, WINDOW_HEIGHT]);

    let options = eframe::NativeOptions {
        // Floor the window width at the central column's minimum so the side
        // panels can always honor [MIN_CENTRAL_WIDTH]; below this the central
        // panel would have nowhere left to keep its width.
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(inner_size)
            .with_min_inner_size([MIN_CENTRAL_WIDTH, 200.]),
        ..Default::default()
    };

    eframe::run_native(
        "Shell",
        options,
        Box::new(|cc| {
            ui::install_fonts(&cc.egui_ctx);
            Ok(Box::new(state))
        }),
    )
    .unwrap();
}
