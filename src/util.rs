//! Misc utility functionality.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use shell::{
    DirList, clone_root, commands, commands::Frontend, find_bookmark, path_from_args, resolve_cd,
    resolve_root, ssh,
};

use crate::{
    OutType, out_type_for, path_color,
    remote::Remote,
    state::State,
    util,
    wsl::{self, Here},
};

/// Read a file into a `String`. Used by the `cat` builtin to populate the
/// output pane.
pub fn read_file(path: &Path) -> io::Result<String> {
    fs::read_to_string(path)
}

/// Note: In `shell`, this is in the `commands` module. To prevent possible confusion, we don't
/// have an additoinal `commands` module in this program.
pub fn handle_cmd(state: &mut State, cmd: &str, input: &str, args: &str, ctx: &egui::Context) {
    // While the active tab is connecting, there's nowhere to run anything yet.
    if let Some(Remote::Connecting(c)) = state.remote_here() {
        match cmd {
            "disconnect" | "quit" | "exit" => state.disconnect_remote(),
            _ => {
                let msg = format!(
                    "ssh: still connecting to {}; `disconnect` cancels",
                    c.label()
                );
                state.push_out(msg, OutType::StdErr);
            }
        }
        return;
    }

    // While the active tab is connected, commands run in the remote shell,
    // which keeps its state (cwd, env) between them. Only the
    // session-management keywords are intercepted; everything else, `cd` and
    // `exit` included, goes to the remote. (Input for a command already
    // running there is sent straight from the input row; see `ui::term_in`.)
    if state.live_here().is_some() {
        match cmd {
            "disconnect" | "quit" => state.disconnect_remote(),
            // Our own help, not the remote's — answer locally.
            "shelp" => state.push_out(commands::shelp_text(Frontend::Gui), OutType::StdOut),
            "--version" | "-v" => state.push_out(commands::version_line(), OutType::StdOut),
            // The pane is ours: the remote's `clear` would only send it
            // escape codes (or complain that TERM isn't set).
            "clear" | "cls" | "Clear-Host" => state.clear_out(),
            // The shell's Rust built-ins, translated to the equivalent remote
            // command: the remote shell has never heard of them.
            "sync" | "logs" => {
                let mut sink = |kind, msg| state.push_out(msg, out_type_for(kind));
                // A snapshot, as the GUI's local `logs` shows.
                let remote_cmd = if cmd == "sync" {
                    ssh::remote_sync_command(args, &mut sink)
                } else {
                    ssh::remote_logs_command(args, false, &mut sink)
                };
                if let Some(c) = remote_cmd {
                    state.run_remote(&c);
                }
            }
            "rm_targets" | "open" | "view" | "edit" | "root" | "rust" => state.push_out(
                format!("{cmd}: only runs locally; disconnect first"),
                OutType::StdErr,
            ),
            // Everything else runs on the remote, with shorthands (`pull`,
            // `run`, bare-name clones) expanded first.
            _ => {
                let expanded = commands::expand_input(
                    input,
                    state.clone_roots.last().map(String::as_str),
                    None,
                );
                if expanded.echo {
                    state.push_out(format!("> {}", expanded.line), OutType::Prompt);
                }
                state.run_remote(&expanded.line);
            }
        }
        return;
    }

    let here = state.here();
    let wsl = here.is_some();

    match cmd {
        // As over SSH, `exit` leaves.
        "exit" | "quit" if wsl => state.leave_wsl(),

        "exit" | "quit" => {
            state.push_out("(use the window close button to exit)", OutType::StdErr);
        }

        // The shell's Rust built-ins, as Linux commands, as over SSH.
        "sync" | "logs" if wsl => {
            let mut sink = |kind, msg| state.push_out(msg, out_type_for(kind));
            let line = if cmd == "sync" {
                ssh::remote_sync_command(args, &mut sink)
            } else {
                ssh::remote_logs_command(args, false, &mut sink)
            };
            if let Some(line) = line {
                let cwd = state.cwd().to_path_buf();
                state.start_running(&line, &cwd, None, ctx);
            }
        }

        "rust" if wsl => state.push_out("rust: not available in WSL tabs", OutType::StdErr),

        "shelp" => state.push_out(commands::shelp_text(Frontend::Gui), OutType::StdOut),

        "--version" | "-v" => state.push_out(commands::version_line(), OutType::StdOut),

        // pwsh's `Clear-Host` needs a console, and its stdio here are pipes.
        "clear" | "cls" | "Clear-Host" => state.clear_out(),

        "remote" => state.cmd_remote(args),

        "open" => {
            let cwd = state.cwd().to_path_buf();
            commands::open_dir(&cwd, &mut |kind, msg| {
                state.push_out(msg, out_type_for(kind));
            });
        }

        // The CLI version asks for confirmation on stdin, which the GUI
        // doesn't route; say so rather than handing `rm_targets` to pwsh.
        "rm_targets" => state.push_out(
            "rm_targets: only available in the CLI (it asks for confirmation before deleting)",
            OutType::StdErr,
        ),

        // `ssh <index>` connects the active tab to a saved remote (same as the
        // per-remote buttons in the tab strip); `ssh [user@]host[:port]`
        // connects to a freeform target using a keyring-stored password.
        "ssh" => state.cmd_ssh(args),

        "sync" => {
            // Delegate to the shared implementation. Clone `cwd` so the
            // sink closure can borrow `state` mutably for `push_out` without
            // overlapping with a `&state.cwd` borrow.
            let cwd = state.cwd().to_path_buf();
            commands::sync(args, &cwd, &mut |kind, msg| {
                state.push_out(msg, out_type_for(kind));
            });
        }

        "logs" => {
            // GUI uses `follow = false`: the live `-f` tail would block the
            // UI thread indefinitely, so the shared impl falls back to a
            // bounded `-n 200` snapshot in this mode. On non-Linux the
            // shared impl emits a friendly "only supported on Linux" error.
            commands::logs(args, false, &mut |kind, msg| {
                state.push_out(msg, out_type_for(kind));
            });
        }

        // `rust new`: `cargo new`, then our own .gitignore, rustfmt.toml and README.
        "rust" => {
            let cwd = state.cwd().to_path_buf();
            commands::rust(args, &cwd, &mut |kind, msg| {
                state.push_out(msg, out_type_for(kind));
            });
        }

        // Linux's own, in WSL.
        "cat" if !wsl => {
            let target = path_from_args(
                state.home.as_deref(),
                state.cwd(),
                &state.dir_bookmarks,
                args,
            );
            match util::read_file(&target) {
                Ok(text) => state.push_out(text, OutType::StdOut),
                Err(e) => {
                    state.push_out(format!("cat: {}: {e}", target.display()), OutType::StdErr)
                }
            }
        }

        // A file in a viewer window; `edit` opens it in edit mode.
        "view" | "edit" => {
            if args.is_empty() {
                state.push_out(format!("{cmd}: usage: {cmd} <file>"), OutType::StdErr);
                return;
            }
            let (home, arg) = path_arg(state, args);
            let target = path_from_args(home.as_deref(), state.cwd(), &state.dir_bookmarks, &arg);
            if !target.is_file() {
                state.push_out(
                    format!("{cmd}: {}: no such file", target.display()),
                    OutType::StdErr,
                );
            } else if cmd == "edit" {
                state.file_viewers.edit(target);
            } else {
                state.file_viewers.open(target);
            }
            ctx.request_repaint();
        }

        "del" => {
            let (sub, rest) = match args.find(char::is_whitespace) {
                Some(i) => (&args[..i], args[i..].trim()),
                None => (args, ""),
            };
            match sub {
                // Numbered as the Bookmarks panel shows them, in its sort order.
                "bm" => match rest.parse::<usize>() {
                    Ok(idx) => {
                        if let Some(i) = state.bookmark_at(idx) {
                            let path = state.dir_bookmarks.remove(i);
                            state.refresh_bookmarks_filter();
                            state.push_out(
                                format!("Deleted bookmark: {}", path.display()),
                                OutType::StdOut,
                            );
                            if let Err(e) = state.save() {
                                state.push_out(
                                    format!("del bm: failed to save bookmarks: {e}"),
                                    OutType::StdErr,
                                );
                            }
                        } else {
                            let len = state.dir_bookmarks.len();
                            state.push_out(
                                format!("del bm: no bookmark at index {idx} (have {len})"),
                                OutType::StdErr,
                            );
                        }
                    }
                    Err(_) => state.push_out(
                        "del bm: expected a number, e.g. `del bm 4`",
                        OutType::StdErr,
                    ),
                },
                "" => state.push_out("del: usage: del bm <number>", OutType::StdErr),
                other => state.push_out(
                    format!("del: unknown target `{other}` (expected `bm`)"),
                    OutType::StdErr,
                ),
            }
        }

        "cd" => {
            // `cd <number>` jumps to a recent dir by index; anything else is
            // a path, bookmark, recent-dir or standard-dir name, or the start
            // of a directory Tab would complete.
            let target = if let Ok(idx) = args.parse::<usize>() {
                let resolved = state.recent_dirs.get(idx).map(|r| r.path.clone());
                if resolved.is_none() {
                    state.push_out(
                        format!("cd: no recent directory at index {idx}"),
                        OutType::StdErr,
                    );
                }
                resolved
            } else if let Some(target) =
                wsl::resolve(here.as_ref(), args).filter(|t| wsl::fs_path(t).is_dir())
            {
                // A Linux path, as Linux takes it.
                Some(target)
            } else {
                cd_target(state, here.as_ref(), args)
            };

            if let Some(target) = target {
                let before = state.cwd().to_path_buf();
                state.change_saved_dir(target, DirList::RecentDirs);
                note_navigation(state, &before);
            }
        }

        // The Files panel's Project root button.
        "root" => match resolve_root(state.cwd()) {
            Ok(root) => {
                let before = state.cwd().to_path_buf();
                state.change_dir(root);
                note_navigation(state, &before);
            }
            Err(e) => state.push_out(format!("root: {e}"), OutType::StdErr),
        },

        "bm" => {
            // As for `del bm`, a number is the one the Bookmarks panel shows.
            let resolved = if let Ok(idx) = args.parse::<usize>() {
                state
                    .bookmark_at(idx)
                    .map(|i| state.dir_bookmarks[i].clone())
            } else if !args.is_empty() {
                find_bookmark(&state.dir_bookmarks, args)
            } else {
                None
            };
            match resolved {
                Some(target) => state.change_saved_dir(target, DirList::Bookmarks),
                None => state.push_out(
                    format!("bm: no matching bookmark for `{args}`"),
                    OutType::StdErr,
                ),
            }
        }

        // Everything else goes to the system shell, after the shell library's
        // shorthand rewrites (`pull` -> `git pull`, `run` -> `cargo run`,
        // bare-name clones, venv python/pip) — the same ones the CLI applies.
        _ => {
            let cwd = state.cwd().to_path_buf();
            // A Windows venv's python is no use in WSL.
            let expanded = commands::expand_input(
                input,
                state.clone_roots.last().map(String::as_str),
                (!wsl).then_some(cwd.as_path()),
            );
            if expanded.echo {
                state.push_out(format!("> {}", expanded.line), OutType::Prompt);
            }

            // A clone from a full URL saves the URL's root once it succeeds.
            let root = clone_root(&expanded.line);
            state.start_running(&expanded.line, &cwd, root, ctx);
        }
    }
}

/// Where `cd <args>` goes by name, from the bookmarks, recent and standard
/// dirs (see [resolve_cd]). In WSL `here`, only to a dir in the distro,
/// unless a Windows path was typed.
fn cd_target(state: &mut State, here: Option<&Here>, args: &str) -> Option<PathBuf> {
    let home = state.tab_home();
    let saved = here.map(|h| state.saved_dirs_in(h));
    let (bookmarks, recent) = match &saved {
        Some((b, r)) => (b.as_slice(), r.as_slice()),
        None => (state.dir_bookmarks.as_slice(), state.recent_dirs.as_slice()),
    };

    let target = match resolve_cd(home.as_deref(), state.cwd(), bookmarks, recent, args) {
        Ok(target) => target,
        Err(e) => {
            state.push_out(format!("cd: {e}"), OutType::StdErr);
            return None;
        }
    };
    let in_distro = |h: &Here| {
        wsl::split(&target).is_some_and(|(d, _)| d.eq_ignore_ascii_case(&h.distro))
            || target.starts_with(state.cwd())
            || wsl::windows_absolute(args)
    };
    if here.is_some_and(|h| !in_distro(h)) {
        state.push_out(format!("cd: {args}: no such directory"), OutType::StdErr);
        return None;
    }

    Some(target)
}

/// Where `~` is for a path argument in the active tab, and the argument,
/// respelled for Windows when it's a Linux path (see [wsl::resolve]).
fn path_arg(state: &State, args: &str) -> (Option<PathBuf>, String) {
    let arg = match wsl::resolve(state.here().as_ref(), args) {
        Some(target) => wsl::fs_path(&target).to_string_lossy().into_owned(),
        None => args.to_owned(),
    };

    (state.tab_home(), arg)
}

/// After `cd` or `root`: say where to, if it left `before`.
fn note_navigation(state: &mut State, before: &Path) {
    if state.cwd() == before {
        return;
    }

    let shown = state.cwd_shown();
    let parts = path_color::parts(&shown, Some("~"), true);
    state.push_out(
        format!("Navigated to {}", path_color::to_ansi(&parts)),
        OutType::StdOut,
    );
}
