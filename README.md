# Peregrine


An integrated terminal and file browser. For Linux, Windows, and MacOS. I built this for my own file-system and CLI workflows. Wherever I notice a friction point, I update this program to address it. For example, when using a `cd` command to navigate in a terminal, I've made it trivial to access ones I've used before, or to find the right directory using the final dir's name (or first few chars) without typing its full path. Another example is moving files and folders between projects without managing a collection of tabs, or repeatedly typing full paths.

The initial inspiration: I do not enjoy manually navigating directories (In a terminal with `cd` commands, nor my OS's file browser). In the case of ones I use frequent, there is no reason I must!

One description: an **Integrate File Environment or IFE**: like an IDE, but a general tool for files. Of course, most IDEs (Like Jetbrains') include functionality this program has (Plus a lot more!) - the differentiator is this is light, and is designed to work across the whole system instead of a project.

It's based on the abstraction that navigating a filesystem in a GUI, and executing commands in a terminal are complementary operations that should interact. It also integrates a clipboard manager with history, a system PATH viewer/editor, and lightweight viewers for text and image files. (Misc system-level functionality I want to improve over OS defaults)

This application takes its CLI component and shortcuts from [Shell](https://github.com/david-oconnor/shell), and shares its relevant systems including directory and command history and bookmarks. For information on terminal commands,see [its Readme](https://github.com/David-OConnor/shell/blob/main/README.md).


![Demo of the Peregrine UI](/screenshots/demo_0.png)

## Install
- [Download and unzip](https://github.com/David-OConnor/peregrine/releases)
- Launch the application, or run the *setup* script to add a shortcut to your OS's launcher.


## Who this is for, and its purpose
This may be useful to those who execute CLI commands, and find the OS's native file browser and terminal to be naive. For example, if you execute commands like `git`, `python`, `cargo`, `gmx`, `orca`, `cd`, `ls`, `grep`/`rg` in a terminal. It makes accessing you usual workflows easier by remembering directories and commands. It has intuitive bookmark, history, and autocomplete systems.

If you are navigating to a regularly-used directory via CLI and are doing too much typing, this may help.


## Who this is not for
If you use a fully CLI workflow, or if your terminal program must be available when you are using an arbitrary computer or via SSH, this is not what you're looking for. If you perform all file operations in a terminal, or all in a GUI, this is not for you - it's for workflows that use both. This is not a shell scripting language, and its CLI functionality runs on top of your OS's native terminal.

## Hotkeys of immediate interest
**Tab**: Clears and focuses the CLI input. This is useful for, e.g. immediately navigating to a directory
you know, regardless of which UI item you're using. E.g. to go to a bookmark or recent directory named `molchanica`:

- Press **Tab**
- Type `cd mol`
- Press **Enter***


## Everything is searchable
You can search in every GUI panel: bookmarks, recent directories, recent commands, clipboard etc. The file browser has a quick filter, and full Ripgrep search for file names or contents.

![Demo of the Peregrine UI showing the file viewer and a markdown file](/screenshots/demo_1.png)


## Directory bookmarks
(Description todo)


## Recent directories
(Description todo)


## File browser
You can rename a file by selecting it, then clicking its name again. Double-clicking a file opens it in its default file handler. Broadly, the UX for selection, moving, and file operations follow conventions.

Drag files and folders onto a folder (or the ⬆ and shortcut buttons) to move them there; hold Ctrl to copy. Drag them out of the window to drop them on Explorer or Nautilus, the desktop, a web page's upload box, or any other program that takes files from a file manager (Windows, and Linux on X11 or Wayland).

Opening most popular text-based file formats (.txt, Markdown, TOML, most code files) or image (.png, .jpg, .bmp) file in the browser will open an integrated viewer; you can then open in your OS's default handler from there. In the image viewer, Ctrl+scroll zooms and dragging pans. 


## CLI / terminal

Most CLI commands from *Shell* work here, but this version includes more functionality, enabled by the GUI.

Autocomplate works from a mixture of bookmarks, recent commands, and files in the current directory. Autocomplete messages display automatically in grey; press the **right arrow key** to complete the line to one.

Type `shelp` in the CLI input to list every command and key shortcut.


## Command history
(Description todo)


## System path editor
(Description todo)


## SSH / remote capability
`ssh [user@]host [port]`, or a saved remote's button in the tab strip or Remote panel, connects the current tab. Connecting happens in the background, so an unreachable host never freezes the window. Once connected, commands typed in that tab run in your remote login shell, which keeps its working directory and environment between them. Full-screen programs (`vim`, `nano`, `top`, `less`) take over the output pane while they run, with the keyboard going to them; lines typed while any other remote command runs go to it as input, and Ctrl+C interrupts it. `exit` logs out; `disconnect` closes the connection.

Logging in uses your SSH keys (from an SSH agent, or `~/.ssh/id_*` without a passphrase), or a password saved in the OS keyring via the Remote panel. When a password is needed, the Remote panel's form opens for it.


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
- Drag a panel, or the terminal, by the dotted strip across its top to move it left or right. The toggles above follow the order. With the terminal hidden, the panel beside it fills the middle.
- Select CLI output text and right-click to copy it to the system clipboard and clipboard history (Supplements *Ctrl + C*)
- Numerical settings can be changed by clicking and dragging the mouse (or by typing)


## Keyboard shortcuts
The About window lists these too.

- **Tab**: Focus the CLI input, and clear it. Works anywhere in the window; in the CLI input itself, Tab autocompletes instead
- **Ctrl + V**, **Ctrl + C**, **Ctrl + X** paste, copy, and cut using the native advanced clipboard, and/or system one
- **Ctrl + Shift + V**: Toggle the clipboard pane

### File browser
- **Enter** opens a file. (Same as double-clicking) Native viewer for text, markdown, code, and image files. OS-default otherwise.
- **Shift or Ctrl + arrow keys** extend the selection
- **Ctrl + A** selects everything shown
- **Escape** clears selections
- **Delete** moves the selected items to the Recycle Bin (Trash on Mac and Linux), after confirming
- **Ctrl + Shift + N** creates a new folder. On Mac, **Cmd + Shift + N**

### File viewer
On Mac, use **Cmd** in place of **Ctrl**.

- **Ctrl + F**: Find. Focuses the find box, selecting its text
- **Enter** / **Shift + Enter** (in the find box): Next / previous match
- **F3** / **Shift + F3**: Next / previous match. Also **Ctrl + G** / **Ctrl + Shift + G**
- **Alt + C**: Toggle match case. On Mac, **Cmd + Option + C**
- **Escape** (in the find box): Clear the search
- **F5**: Reload the file
- **Left** and **Right** arrow keys (image viewer): Previous and next image in the directory

### Tabs
On Mac, use **Cmd** in place of **Ctrl**, except for the **Ctrl + Tab** and **Ctrl + PageUp / PageDown** shortcuts, which use **Ctrl** on every OS.

- **Ctrl + T** or **Ctrl + N**: New tab, in the current tab's directory
- **Ctrl + W**: Close the current tab
- **Ctrl + Shift + T**: Reopen the last closed tab, with its output and Back/Forward history
- **Ctrl + Tab** or **Ctrl + PageDown**: Next tab. On Mac, also **Cmd + Option + Right**
- **Ctrl + Shift + Tab** or **Ctrl + PageUp**: Previous tab. On Mac, also **Cmd + Option + Left**
- **Ctrl + 1** to **Ctrl + 8**: Jump to that tab, counting from the left
- **Ctrl + 9**: Jump to the last tab


### Terminal
This section is mostly a copy+paste from [Shell's readme](https://github.com/david-oconnor/shell/README.md), as we use the same shortcuts here.

On Windows and Linux, **Ctrl + W**, **Ctrl + T**, and **Ctrl + N** stay tab shortcuts (see above). Use **Ctrl + Backspace** to delete the word before the cursor, and **↓** for the next history entry. On Mac, where tabs use **Cmd**, all of these work, and the **Alt** shortcuts are **Cmd + Option**, since Option + a letter types a symbol.

- **Tab**: while using with cd, autocompletes, including to bookmarks.
- **Ctrl + A** / **Home**: Move the cursor to the start of the line.
- **Ctrl + E** / **End**: Move the cursor to the end of the line. With the cursor already there, accepts the dimmed history suggestion.
- **Ctrl + ←** / **Ctrl +** →: Move the cursor back / forward one word.
- **Ctrl + W**: Delete the word before the cursor. (Mac)
- **Alt + D**: Delete the word after the cursor.
- **Ctrl + K**: Delete from the cursor to the end of the line.
- **Ctrl + U**: Delete from the cursor to the start of the line.
- **Ctrl + Y**: Paste the last deleted text. Deletions made one after another paste back together.
- **Ctrl + T**: Swap the two characters around the cursor. (Mac) **Alt + T** swaps words.
- **Alt + C** / **Alt + U** / **Alt + L**: Capitalize / uppercase / lowercase the word at the cursor.
- **Ctrl + _**: Undo. **Ctrl + Z** does too.
- **Ctrl + L**: Clear the screen.
- **Ctrl + N** / **Ctrl + P**: Next / previous history entry, as ↓ / ↑. (**Ctrl + N** on Mac)
- **Ctrl + C**: Cancel the current input. Stops the running command instead, if there is one, and copies if text is selected.
- **Ctrl + D**: Delete the character under the cursor. On an empty line, ends a running program's input; with nothing running, it's `exit`.

### Scratchpad
Notes kept between runs, in a side panel (the **Scratchpad** toggle). The Terminal keys above work in it too, on the cursor's line: **Ctrl + A** / **Ctrl + E** go to its start / end, **Ctrl + K** / **Ctrl + U** delete to them (**Ctrl + K** at a line's end joins the next line on), and **Ctrl + P** / **Ctrl + N** move up / down a line. As in the CLI input, **Ctrl + W**, **Ctrl + T** and **Ctrl + N** stay tab shortcuts on Windows and Linux. **Tab** indents, rather than going to the CLI input.

### Bookmarks
- **Alt + 0** to **Alt + 9**: Go to that bookmark, by its number in the Bookmarks panel. (Same as `bm 1` etc in the CLI.) On Mac, **Cmd + Option + 0** to **9**
