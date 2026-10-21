//! Misc utility functionality.

use std::{fs, io, path::Path};

use shell::{
    DirList, clone_root, commands, commands::Frontend, find_bookmark, find_cd_fallback,
    path_from_args, ssh,
};

use crate::{OutType, out_type_for, remote::Remote, state::State, util};

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
            "rm_targets" | "open" => state.push_out(
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

    match cmd {
        "exit" | "quit" => {
            state.push_out("(use the window close button to exit)", OutType::StdErr);
        }

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

        "cat" => {
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
            // a path, bookmark, recent-dir or standard-dir name.
            let target = if let Ok(idx) = args.parse::<usize>() {
                let resolved = state.recent_dirs.get(idx).map(|r| r.path.clone());
                if resolved.is_none() {
                    state.push_out(
                        format!("cd: no recent directory at index {idx}"),
                        OutType::StdErr,
                    );
                }
                resolved
            } else {
                let mut target = path_from_args(
                    state.home.as_deref(),
                    state.cwd(),
                    &state.dir_bookmarks,
                    args,
                );
                if !target.is_dir() && !args.is_empty() && !args.starts_with('~') {
                    if let Some(found) =
                        find_cd_fallback(&state.recent_dirs, state.home.as_deref(), args)
                    {
                        target = found;
                    }
                }
                Some(target)
            };

            if let Some(target) = target {
                state.change_saved_dir(target, DirList::RecentDirs);
            }
        }

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
            let expanded = commands::expand_input(
                input,
                state.clone_roots.last().map(String::as_str),
                Some(&cwd),
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
