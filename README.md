# Peregrine


An integrated and powerful terminal and file browser. For Linux, Windows, and MacOS. I built this for my own file-system and CLI workflows. Wherever I notice a friction point, I update this program to address it. For example, when using a `cd` command to navigate in a terminal, I've made it trivial to access ones I've used before, or to find the right directory using just the final dir's name without typing its full path. Another example is moving files and folders between my software projects without managing a collection of file browser tabs, or repeatedly typing full paths.

It's based on the abstraction that navigating a filesystem in a GUI, and executing commands in a terminal are complementary operations that should interact.

This application includes the CLI component from [Shell](https://github.com/david-oconnor/shell), and shares its relevant systems including directory and command history and bookmarks. For information on terminal commands,see [its Readme](https://github.com/David-OConnor/shell/blob/main/README.md).


## Install
- [Download the latest release](https://github.com/David-OConnor/peregrine/releases)
- Launch the application, or run the *setup* script to add a shortcut to your OS's launcher.


## Who this is for, and its purpose
This may be useful to those who execute CLI commands regularly, and finds native terminal behavior naive. For example, if you regularly execute commands like `git`, `python`, `cargo`, `gmx`, `orca`, `cd`, `ls`, `grep`/`rg` in a terminal. It makes accessing you usual workflows easier by remembering directories and commands. It has intuitive bookmark, history, and autocomplete systems.

If you are navigating to a regularly-used directory via CLI and are doing too much typing, this may help.


## Who this is not for
If you use a fully CLI or keyboard-driven workflow, or if your terminal program must be available when you are using an arbitrary computer or via SSH, this is not what you're looking for.

If you perform all file operations in a terminal, or all in a GUI, this is not for you - it's for workflows that use both.


## Everything is searchable
You can search in every GUI panel: bookmarks, recent directories, recent commands, clipboard etc. The file browser has a quick filter, and full Ripgrep search for file names or contents.


## Directory bookmarks



## Recent directories


## File browser
You can rename a file by selecting it, then clicking its name again. Double-clicking a file opens it in its default file handler.

Opening a Markdown (.md), text (.txt) or image (.png, .jpg, .bmp) file in the browser will open an integrated viewer; you can then open in your OS's default handler from there. In the image viewer, Ctrl+scroll zooms and dragging pans. 

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


## Misc tips

- Middle clicking a directory or command in an appropriate panel opens that directory or launches that command in a new tab
- Middle click a tab to close it
- Select CLI output text and right-click to copy it to the system clipboard and clipboard history
- Most buttons have tooltips on hover


## Keyboard shortcuts
- Ctrl + v, Ctrl + c, Ctrl + x paste, copy, and cut using the native advanced clipboard, and/or system one
- Ctrl + shift + v: Toggle the clipboard pane
- Image viewer: Left and Right arrow keys cycle to the prev and next image in the directory
- File browser: Enter opens a file (Same as double-clicking)
