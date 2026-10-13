# Shell
Making the terminal application I want to use.

[![Docs](https://docs.rs/dynamics/badge.svg)](https://www.athanorlab.com/docs)

An integrated and powerful terminal and file browser.

This includes the CLI component from [Shell](https://github.com/david-oconnor/shell), and shares its relevant systems including directory and command history and bookmarks. For information on terminal commands see [its Readme](https://github.com/David-OConnor/shell/blob/main/README.md).

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

## PATH
The **PATH** window (off by default; open it with the **PATH** toggle, last in the panel toggles) edits PATH one
directory per row, like Windows' "Edit environment variable" dialog. Pick what to edit at the top:

- **User**: your account's PATH. On Windows, the `HKEY_CURRENT_USER\Environment` `Path` value. On Linux and macOS, a
  block the panel keeps in a shell startup file (`~/.bashrc` by default; `~/.zshrc` if your shell is zsh), which adds
  its entries to the front of PATH. Pick another file (e.g. `~/.profile`, read at login by desktop sessions) from the
  dropdown. The file is copied to `<file>.shell_gui.bak` before each save, and the panel lists the file's other lines
  that touch PATH, which it leaves alone.
- **System**: PATH for every account. On Windows, the `HKEY_LOCAL_MACHINE` `Path` value; saving it asks for admin
  permission (UAC). On Linux, the PATH line in `/etc/environment`, saved through `pkexec`. On macOS, `/etc/paths`,
  saved through the admin password prompt. A System PATH without `system32` (Windows) or `/usr/bin` is refused.
- **This app**: the PATH commands run from this app use, like `export PATH=...` in a terminal: it lasts until the app
  exits.

The search box above the list filters it as you type, ignoring case: an entry stays if its path matches, or if its
dir has an executable whose name matches. Those executables are listed under it, struck through where one in a dir
searched earlier runs instead, so searching a command name shows every copy on PATH and which one wins.

Double-click a row to edit it; missing directories are shown in amber and duplicates struck through. **+ Current dir** adds the
active tab's directory. **Edit as text** edits the list one entry per line, and splits a pasted PATH string. Saving
User or System also makes the same change to this app's PATH, so commands run from here see it at once; other open
terminals keep their old PATH until restarted.

The pane under the list shows the selected entry's details: what it expands to, and the executables in it (on Windows,
files with a `PATHEXT` extension; elsewhere, files with an execute bit), with a filter box. An executable that a dir
searched earlier hides is struck through; hover it to see which file runs instead. On Windows, System's entries count
as searched before User's; on Linux/macOS, the User block's before System's. **cd here** and **Open** go to the dir.
