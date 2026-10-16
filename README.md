# Peregrine


An integrated and powerful terminal and file browser. For Linux, Windows, and MacOS. I built this for my own file-system and CLI workflows. Wherever I notice a friction point, I update this program to address it. For example, when using a `cd` command to navigate in a terminal, I've made it trivial to access ones I've used before, or to find the right directory using just the final dir's name without typing its full path. Another example is moving files and folders between my software projects without managing a collection of file browser tabs, or repeatedly typing full paths.

The initial inspiration: I do not enjoy manually navigating sequences of directories (In a terminal with `cd` commands, nor my OS's file browser). In the case of ones I use frequent, there is no reason I must!

It's based on the abstraction that navigating a filesystem in a GUI, and executing commands in a terminal are complementary operations that should interact. It also integrates a clipboard manager with history, a system PATH viewer/editor, and lightweight viewers for text and image files.

This application takes its CLI component from [Shell](https://github.com/david-oconnor/shell), and shares its relevant systems including directory and command history and bookmarks. For information on terminal commands,see [its Readme](https://github.com/David-OConnor/shell/blob/main/README.md).

Use is intuitive, leveraging what you already know about file browsers and terminals. Additional functionality this application provides is mostly obvious; exceptions and assumptions are included in this readme.


![Demo of the Peregrine UI](/screenshots/demo_0.png)

## Install
- [Download and unzip](https://github.com/David-OConnor/peregrine/releases)
- Launch the application, or run the *setup* script to add a shortcut to your OS's launcher.


## Who this is for, and its purpose
This may be useful to those who execute CLI commands regularly, and find the OS's native file browser and terminal to be naive. For example, if you regularly execute commands like `git`, `python`, `cargo`, `gmx`, `orca`, `cd`, `ls`, `grep`/`rg` in a terminal. It makes accessing you usual workflows easier by remembering directories and commands. It has intuitive bookmark, history, and autocomplete systems.

If you are navigating to a regularly-used directory via CLI and are doing too much typing, this may help.


## Who this is not for
If you use a fully CLI workflow, or if your terminal program must be available when you are using an arbitrary computer or via SSH, this is not what you're looking for. If you perform all file operations in a terminal, or all in a GUI, this is not for you - it's for workflows that use both. This is not a new shell scripting language, and its CLI functionality runs on top of your OS's native terminal.


## Everything is searchable
You can search in every GUI panel: bookmarks, recent directories, recent commands, clipboard etc. The file browser has a quick filter, and full Ripgrep search for file names or contents.

![Demo of the Peregrine UI showing the file viewer and a markdown file](/screenshots/demo_1.png)

## Directory bookmarks



## Recent directories


## File browser
You can rename a file by selecting it, then clicking its name again. Double-clicking a file opens it in its default file handler.

Select several items with Ctrl+click and Shift+click, as in your OS's file browser; the details pane then totals them up. Drag items onto a folder (or onto ⬆ or a shortcut button) to move them there; hold Ctrl as you let go to copy them instead. As in Explorer, items dragged to another drive are copied unless you hold Shift.

Click the red trash button beside Paste, or press Delete while the file list has focus, to move the selected files and folders to the OS's Recycle Bin or Trash. A confirmation dialog lists the items first; Cancel or Esc keeps them.

Opening most popular text-based file formats (.txt, Markdown, TOML, most code files) or image (.png, .jpg, .bmp) file in the browser will open an integrated viewer; you can then open in your OS's default handler from there. In the image viewer, Ctrl+scroll zooms and dragging pans. 

Code files are syntax highlighted.


## CLI / terminal

Most CLI commands from *Shell* work here, but this version includes more functionality, enabled by the GUI.

Autocomplate works from a mixture of bookmarks, recent commands, and files in the current directory. Autocomplete messages display automatically in grey; press the **right arrow key** to complete the line to one.

Type `shelp` in the CLI input to list every command and key shortcut.

## Command history

## System path editor

## SSH / remote capability
Broken / WIP. Don't use.


## Clipboard
Peregrine includes an advanced clipboard, which stores multiple items. Its history is kept between runs, in the state file (`shell_state.ss`).


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


## Non-obvious functionality

- Most controls have tooltips on hover. These list what it does, and its keyboard shortcut.
- Middle clicking a directory or command in a panel opens that directory or launches that command in a new tab
- Middle click a tab to close it
- Drag a tab left or right to reorder it
- Select CLI output text and right-click to copy it to the system clipboard and clipboard history (Supplements *Ctrl + C*)


## Keyboard shortcuts
- **Ctrl + V**, **Ctrl + C**, **Ctrl + X** paste, copy, and cut using the native advanced clipboard, and/or system one
- **Ctrl + Shift + V**: Toggle the clipboard pane
- Image viewer: Left and Right arrow keys cycle to the prev and next image in the directory

### File browser
- **Enter** opens a file. (Same as double-clicking) Native viewer for text, markdown, code, and image files. OS-default otherwise.
- **Shift or Ctrl + arrow keys** extend the selection
- **Ctrl + A** selects everything shown
- **Escape** clears selections

### Tabs
On Mac, use **Cmd** in place of **Ctrl**, except for the **Ctrl + Tab** and **Ctrl + PageUp / PageDown** shortcuts, which use **Ctrl** on every OS.

- **Ctrl + T** or **Ctrl + N**: New tab, in the current tab's directory
- **Ctrl + W**: Close the current tab
- **Ctrl + Shift + T**: Reopen the last closed tab, with its output and Back/Forward history
- **Ctrl + Tab** or **Ctrl + PageDown**: Next tab. On Mac, also **Cmd + Option + Right**
- **Ctrl + Shift + Tab** or **Ctrl + PageUp**: Previous tab. On Mac, also **Cmd + Option + Left**
- **Ctrl + 1** to **Ctrl + 8**: Jump to that tab, counting from the left
- **Ctrl + 9**: Jump to the last tab
