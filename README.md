# Shell
Making the terminal application I want to use.

[![Docs](https://docs.rs/dynamics/badge.svg)](https://www.athanorlab.com/docs)

A GUI version of [https://github.com/david-oconnor/shell](Shell).

For details, see [https://github.com/David-OConnor/shell/blob/main/README.md](Shell's Readme).

It uses the same save file (bookmarks, recent dirs etc) as the non-GUI, but includes additional fields which store
gui-specific state such as window size, panel visibility etc.

Most commands and terminal functionality from *Shell* work here as well, but this version includes more functionality
enabled by the GUI, including displayed panels of directories, bookmarks, recent commands, etc. It also includes GUI
controls to supplement commands.

Type `shelp` to list every command and key shortcut, one per line.
The **In this dir** panel lists commands entered in the active tab's current directory.
You can also use `this <number>` or `this <letters>` to re-run one, and
`this p<page>` to show a page of that directory's history in the output pane.

## Remote (SSH)
The **Remote** panel manages SSH connections (shared in-process `russh` transport with the CLI; passwords are kept in
the OS keyring, not the state file). Add a remote (host / port / user / password), then click **Connect** to attach the
active tab to it — typed commands then run on the remote. Use the **Exec ⇄ PTY** toggle to switch between per-command
output and an interactive shell, and **Disconnect** to return to local. Each tab has its own connection, so you can have
local and remote tabs side by side.

Note: in PTY mode the GUI streams output but is not a full terminal emulator — line-oriented interaction (prompts,
`sudo`, REPLs) works well; cursor-addressed full-screen apps render best in the CLI's PTY mode.
