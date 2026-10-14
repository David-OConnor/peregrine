# Peregrine


An integrated and powerful terminal and file browser. For Linux, Windows, and MacOS. I build (And am actively tailoring it) towards my own file-system and CLI workflows. Wherever I have noticed a friction point, in my workflow, I have set up Peregrine to fix it. For example, when using a `cd` command to navigate in a terminal, I've made it trivial to access ones I've used before, or to access using just the final dir's name without remembering exactly where it is. Or moving files and folders between my software projects without managing a collection of file browser tabs, or repeatedly typing full paths.

This includes the CLI component from [Shell](https://github.com/david-oconnor/shell), and shares its relevant systems including directory and command history and bookmarks. For information on terminal commands,see [its Readme](https://github.com/David-OConnor/shell/blob/main/README.md).

I built this to facilitate file system and CLI operations on my computer. It's based on the abstraction that navigating a filesystem in a GUI, and executing commands in a terminal are complementary operations that should interact. 

## Install
- [Download the latest release for your OS](https://github.com/David-OConnor/molchanica/releases)
- Launch the application directly, or run the *setup* script to add a shortcut to your OS's application launcher.


## Who this is for, and its purpose
Those who execute CLI applications commands regularly, and finds their behavior too naive. For example, if you regularly execute commands like `git`, `python`, `cargo`, `gmx`, `orca`, `cd`, `ls`, `grep`/`rg` in a terminal. It makes accessing you usual workflows easier by remembering directories and commands. It has intuitive bookmark, history, and autocomplete systems.

If you are navigating to a regularly-used directory via CLI and are doing too much typing, this may help.


## Who this is not for
If you use a fully CLI or keyboard-driven workflow, or if your terminal must be available when you are using an arbitrary computer or via SSH, this is not what you're looking for.

If you do all your file operations in a terminal, or all your file operations in GUI - this application is for workflows that use both.


## Everything is searchable
You can search in every panel: bookmarks, recent directories, recent commands etc. The file browser has a quick filter, and full Ripgrep search for file names or contents.



## Directory bookmarks



## Recent directories


## File browser
You can rename a file by selecting it, then clicking its name again. Double-clicking a file opens it in its default file handler.

Note: Opening a Markdown (.md) or text (.txt) file in the browser will open an integrated viewer; you can then open in your OS's default handler from there. 

## CLI / terminal

Most CLI commands from *Shell* work here, but this version includes more functionality, enabled by the GUI.

Autocomplate works from a mixture of bookmarks, recent commands, and files in the current directory. Autocomplete messages display automatically in grey; press the **right arrow key** to complete the line to one.

Type `shelp` in the CLI input to list every command and key shortcut.

## Command history

## System path editor

## SSH / remote capability
Broken / WIP. Don't use.


## Clipboard
Peregrine includes an advanced clipboard, which stores multiple items. 


## Shortcuts and integrations

### Git

Shortcuts to reduce typing:
- `pull`: `git pull`
- `clone`: `git clone`
- `push`: `git push`
- `checkout: `git checkout`
- `sync "message"`: `git add .` -> `git commit -am "message"` -> `git push`

### Python
Automatically launches `python` and `pip` commands using a local virtual environment, if detected in the current directory

### Rust
Shortcuts to reduce typing:

- `run`: `cargo run`
- `build`: `cargo build`
- `fmt`: `cargo +nightly fmt`

### Clipboard history

Open **Clipboard** in the Panels row (or press **Ctrl+Shift+V**) to see text and file groups copied or cut
in Peregrine, including terminal input and selected output. History is shared
across tabs, newest first, deduplicated, and searchable. **Use** puts an older
entry on the system clipboard for normal paste in Peregrine or another app.
**Paste to CLI** inserts text at the command input selection without running it;
newlines become spaces. **Paste to folder** copies or moves the entry into the
active local folder using the same background operation as Files paste.

Normal paste always uses the system clipboard; history never silently overrides
it. Text/files pasted from other apps are also remembered, but Peregrine does not
monitor their clipboard activity. File entries reference paths, not snapshots:
renaming or deleting a source elsewhere can make an entry unavailable. Successful
moves remove their old paths from all history entries; failed paths remain for
retry. Cut entries retain their move semantics when reused.

History holds at most 100 entries of up to 1 MiB each, in memory for this session
only. Larger copies still go to the system clipboard. Remove and Clear history
affect only this history. The pane's visibility is also session-only for now.
