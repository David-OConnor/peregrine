//! The PATH panel's backend: where each [Scope] keeps its PATH entries on
//! this OS, and reading and writing them. See `ui::path_panel` for the editor.
//!
//! - Windows: User and System are the `Path` registry values that Windows'
//!   own "Edit environment variable" dialog edits. A new process gets
//!   System's entries followed by User's. Writing System's needs admin, so
//!   this app relaunches itself elevated to do it (see [run_elevated_helper]).
//! - Linux / macOS: User is a block this panel keeps in a shell startup file
//!   (`~/.bashrc` by default), which adds its entries to the front of PATH.
//!   System is the PATH line in `/etc/environment` on Linux, and `/etc/paths`
//!   on macOS; writing either needs root, asked for with the OS's password
//!   prompt.
//! - Everywhere: Session is this process's own PATH, which the commands run
//!   from it inherit. Changing it lasts until the app exits, like
//!   `export PATH=...` in a terminal.

use std::{
    collections::{HashMap, HashSet},
    env, fs, io,
    path::Path,
};

/// Which PATH the panel is editing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    User = 0,
    System = 1,
    Session = 2,
}

impl Scope {
    pub const ALL: [Self; 3] = [Self::User, Self::System, Self::Session];

    pub fn label(self) -> &'static str {
        match self {
            Self::User => "User",
            Self::System => "System",
            Self::Session => "This app",
        }
    }
}

/// What separates PATH entries.
pub const SEP: char = if cfg!(windows) { ';' } else { ':' };

/// A scope's entries as read, plus what the panel shows alongside them.
pub struct Loaded {
    pub entries: Vec<String>,
    /// Set when this scope can't be edited here, saying why.
    pub read_only: Option<String>,
    /// Other things that change this scope's PATH, which the panel lists but
    /// doesn't edit: e.g. other lines in the startup file that set PATH.
    pub others: Vec<String>,
    /// The heading `others` are listed under.
    pub others_title: String,
}

impl Loaded {
    fn entries(entries: Vec<String>) -> Self {
        Self {
            entries,
            read_only: None,
            others: Vec::new(),
            others_title: String::new(),
        }
    }
}

/// Split a PATH value into its entries, trimmed, dropping empty ones. A
/// newline separates entries too, so a list with one per line splits the
/// same way. On Windows a `;` inside double quotes is part of its entry, as
/// Windows reads PATH.
pub fn split(value: &str) -> Vec<String> {
    let mut entries = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut end_entry = |cur: &mut String| {
        let e = cur.trim();
        if !e.is_empty() {
            entries.push(e.to_owned());
        }
        cur.clear();
    };
    for c in value.chars() {
        match c {
            '\r' | '\n' => {
                end_entry(&mut cur);
                quoted = false;
            }
            '"' if cfg!(windows) => {
                quoted = !quoted;
                cur.push(c);
            }
            c if c == SEP && !quoted => end_entry(&mut cur),
            c => cur.push(c),
        }
    }
    end_entry(&mut cur);
    entries
}

pub fn join(entries: &[String]) -> String {
    entries.join(&SEP.to_string())
}

/// `entry` with its environment variables filled in from this process's, as
/// the OS does when it reads PATH: `%NAME%` on Windows; `$NAME`, `${NAME}`
/// and a leading `~` elsewhere. A variable that isn't set is left as written.
pub fn expand(entry: &str) -> String {
    if cfg!(windows) {
        expand_percent(entry)
    } else {
        expand_dollar(entry)
    }
}

fn expand_percent(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) if end > 0 => match env::var(&after[..end]) {
                Ok(val) => {
                    out.push_str(&val);
                    rest = &after[end + 1..];
                }
                // Not a variable: keep the first `%`, and let the second
                // start the next try.
                Err(_) => {
                    out.push('%');
                    out.push_str(&after[..end]);
                    rest = &after[end..];
                }
            },
            _ => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn expand_dollar(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    if let Some(home) = shell::get_home()
        && (rest == "~" || rest.starts_with("~/"))
    {
        out.push_str(&home.to_string_lossy());
        rest = &rest[1..];
    }
    while let Some(start) = rest.find('$') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let (name, len) = if let Some(braced) = after.strip_prefix('{') {
            match braced.find('}') {
                Some(end) => (&braced[..end], end + 2),
                None => ("", 0),
            }
        } else {
            let end = after
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(after.len());
            (&after[..end], end)
        };
        match env::var(name) {
            Ok(val) if !name.is_empty() => {
                out.push_str(&val);
                rest = &after[len..];
            }
            _ => {
                out.push('$');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// What entries are compared by, to spot duplicates: expanded, ignoring a
/// trailing separator, and on Windows ignoring case and taking `/` as `\`.
pub fn key(entry: &str) -> String {
    let expanded = expand(entry);
    if cfg!(windows) {
        let k = expanded.replace('/', "\\").to_lowercase();
        // Keep the slash of a drive root, `c:\`.
        match k.trim_end_matches('\\') {
            t if t.len() <= 2 => k,
            t => t.to_owned(),
        }
    } else {
        match expanded.trim_end_matches('/') {
            "" => expanded,
            t => t.to_owned(),
        }
    }
}

/// A character `entry` can't hold in `scope`, if it has one: the separator,
/// which would split it in two, and on Linux/macOS also the quoting
/// characters of the files User and System are written to.
pub fn bad_char(scope: Scope, entry: &str) -> Option<char> {
    let quoting: &[char] = if cfg!(windows) || scope == Scope::Session {
        &[]
    } else {
        &['"', '\\', '`']
    };
    entry.chars().find(|&c| c == SEP || quoting.contains(&c))
}

/// Refuse a System (or This app) PATH without the OS's core command dir,
/// which would break most commands: `%SystemRoot%\system32` on Windows,
/// `/usr/bin` or `/bin` elsewhere.
pub fn check_system(entries: &[String]) -> Result<(), String> {
    let needed: Vec<String> = if cfg!(windows) {
        let root = env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        vec![key(&format!(r"{root}\system32"))]
    } else {
        vec!["/usr/bin".into(), "/bin".into()]
    };
    if entries.iter().any(|e| needed.contains(&key(e))) {
        Ok(())
    } else {
        let what = if cfg!(windows) {
            r"%SystemRoot%\system32"
        } else {
            "/usr/bin"
        };
        Err(format!(
            "it must keep {what}; without it most commands stop working"
        ))
    }
}

/// The scope whose entries are searched before `scope`'s own when a command
/// is run: on Windows a new program's PATH is System's then User's; on
/// Linux/macOS the User block goes in front of System's PATH.
pub fn searched_before(scope: Scope) -> Option<Scope> {
    match scope {
        Scope::User if cfg!(windows) => Some(Scope::System),
        Scope::System if !cfg!(windows) => Some(Scope::User),
        _ => None,
    }
}

/// The extensions Windows tries, in order, when running a command by a name
/// without one: PATHEXT's, lowercased.
#[cfg(windows)]
fn path_exts() -> Vec<String> {
    env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD;.VBS;.VBE;.JS;.JSE;.WSF;.WSH;.MSC".into())
        .split(';')
        .map(|e| e.trim().to_lowercase())
        .filter(|e| e.starts_with('.') && e.len() > 1)
        .collect()
}

/// The command that runs `file` from a PATH dir: on Windows its name less
/// the extension, lowercased (`python` runs `Python.exe`); elsewhere the
/// whole name.
pub fn command_name(file: &str) -> String {
    if cfg!(windows) {
        Path::new(file).file_stem().map_or_else(
            || file.to_lowercase(),
            |s| s.to_string_lossy().to_lowercase(),
        )
    } else {
        file.to_owned()
    }
}

/// The programs in one PATH dir, as running a command by name sees them.
pub struct DirPrograms {
    /// Their file names, sorted ignoring case.
    pub names: Vec<String>,
    /// For each [command_name], the file in this dir it runs. On Windows,
    /// with `x.bat` and `x.exe` both here, `x` runs the one whose extension
    /// comes first in PATHEXT.
    runs: HashMap<String, String>,
}

impl DirPrograms {
    /// Read the programs in `dir`: on Windows, files with an extension in
    /// PATHEXT (`.exe`, `.cmd`, ...); elsewhere, files (or links to them) with
    /// an execute bit set.
    pub fn read(dir: &Path) -> io::Result<Self> {
        #[cfg(windows)]
        let exts = path_exts();
        // Per command, the file it runs and that file's rank (its
        // extension's place in PATHEXT on Windows).
        let mut runs: HashMap<String, (usize, String)> = HashMap::new();
        let mut names = Vec::new();
        for entry in fs::read_dir(dir)?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            #[cfg(windows)]
            let rank = {
                let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
                let ext = Path::new(&name)
                    .extension()
                    .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()));
                match ext.and_then(|e| exts.iter().position(|x| *x == e)) {
                    Some(rank) if !is_dir => rank,
                    _ => continue,
                }
            };
            #[cfg(not(windows))]
            let rank = {
                use std::os::unix::fs::PermissionsExt;
                // Follows links, as running the command does.
                match fs::metadata(entry.path()) {
                    Ok(md) if md.is_file() && md.permissions().mode() & 0o111 != 0 => 0,
                    _ => continue,
                }
            };
            let cmd = command_name(&name);
            if runs.get(&cmd).is_none_or(|(r, _)| rank < *r) {
                runs.insert(cmd, (rank, name.clone()));
            }
            names.push(name);
        }
        names.sort_by_key(|n| n.to_lowercase());
        Ok(Self {
            names,
            runs: runs.into_iter().map(|(cmd, (_, f))| (cmd, f)).collect(),
        })
    }

    /// The file in this dir that running `cmd` runs, if any.
    pub fn runs(&self, cmd: &str) -> Option<&str> {
        self.runs.get(cmd).map(String::as_str)
    }
}

/// Read `scope`'s entries. `rc_file` is the startup file (in the home dir)
/// User is kept in on Linux/macOS; Windows ignores it.
pub fn load(scope: Scope, rc_file: &str) -> io::Result<Loaded> {
    match scope {
        Scope::User => os::load_user(rc_file),
        Scope::System => os::load_system(),
        Scope::Session => Ok(Loaded::entries(session_entries())),
    }
}

/// Write `entries` as `scope`'s PATH. Returns an error message to show.
pub fn save(scope: Scope, rc_file: &str, entries: &[String]) -> Result<(), String> {
    match scope {
        Scope::User => os::save_user(rc_file, entries),
        Scope::System => os::save_system(entries),
        Scope::Session => {
            set_session(entries);
            Ok(())
        }
    }
}

/// Where `scope`'s PATH is kept, for display.
pub fn location(scope: Scope, rc_file: &str) -> String {
    match scope {
        Scope::User => os::user_location(rc_file),
        Scope::System => os::SYSTEM_LOCATION.to_owned(),
        Scope::Session => "This app's process (every tab)".to_owned(),
    }
}

/// What the panel says under `scope`'s location: how it's applied, and when.
pub fn note(scope: Scope, rc_file: &str) -> String {
    match scope {
        Scope::User => os::user_note(rc_file),
        Scope::System => os::SYSTEM_NOTE.to_owned(),
        Scope::Session => "The PATH commands run from here use. Changes last until this app \
                           exits, like `export PATH=...` in a terminal. Saving User or System \
                           also updates it."
            .to_owned(),
    }
}

/// When a saved change to `scope` reaches other programs, for the message
/// after saving.
pub fn takes_effect(scope: Scope) -> &'static str {
    match scope {
        Scope::User => os::USER_TAKES_EFFECT,
        Scope::System => os::SYSTEM_TAKES_EFFECT,
        Scope::Session => "",
    }
}

/// The startup files User can be kept in (Linux/macOS), relative to home.
pub const RC_FILES: [&str; 5] = [
    ".bashrc",
    ".bash_profile",
    ".profile",
    ".zshrc",
    ".zprofile",
];

/// The startup file User is kept in by default: the one the user's login
/// shell (`$SHELL`) reads for each new terminal.
pub fn default_rc() -> &'static str {
    let shell = env::var("SHELL").unwrap_or_default();
    let name = Path::new(&shell)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let macos = cfg!(target_os = "macos");
    match name.as_str() {
        "zsh" => ".zshrc",
        // macOS terminals start login shells, which read .bash_profile,
        // not .bashrc.
        "bash" if macos => ".bash_profile",
        "bash" => ".bashrc",
        "sh" | "dash" | "ksh" => ".profile",
        _ if macos => ".zshrc",
        _ => ".bashrc",
    }
}

/// This process's PATH entries.
fn session_entries() -> Vec<String> {
    split(&env::var_os("PATH").unwrap_or_default().to_string_lossy())
}

fn set_session(entries: &[String]) {
    // SAFETY: Rust's own env access is locked, and the other threads here
    // (Deep search, SSH) don't read the environment through libc while this
    // runs on the UI thread.
    unsafe { env::set_var("PATH", join(entries)) };
}

/// After `scope`'s saved PATH changed from `old` to `new`, make the same
/// change to this app's PATH, so commands run from it see the change without
/// a restart. Removed entries are dropped unless another saved scope still
/// has them; added ones go in expanded, at the front for a Linux/macOS User
/// block (as that adds them) and at the end otherwise. Returns how many were
/// added and removed.
pub fn apply_to_session(
    scope: Scope,
    rc_file: &str,
    old: &[String],
    new: &[String],
) -> (usize, usize) {
    let other = match scope {
        Scope::User => Scope::System,
        Scope::System => Scope::User,
        Scope::Session => return (0, 0),
    };
    let still_saved: HashSet<String> = load(other, rc_file)
        .map(|l| l.entries.iter().map(|e| key(e)).collect())
        .unwrap_or_default();
    let old_keys: HashSet<String> = old.iter().map(|e| key(e)).collect();
    let new_keys: HashSet<String> = new.iter().map(|e| key(e)).collect();

    let mut session = session_entries();
    let before = session.len();
    session.retain(|e| {
        let k = key(e);
        !old_keys.contains(&k) || new_keys.contains(&k) || still_saved.contains(&k)
    });
    let removed = before - session.len();

    let mut present: HashSet<String> = session.iter().map(|e| key(e)).collect();
    let added: Vec<String> = new
        .iter()
        .filter(|e| !old_keys.contains(&key(e)) && present.insert(key(e)))
        .map(|e| expand(e))
        .collect();
    let n_added = added.len();
    if scope == Scope::User && !cfg!(windows) {
        session.splice(0..0, added);
    } else {
        session.extend(added);
    }

    if n_added + removed > 0 {
        set_session(&session);
    }
    (n_added, removed)
}

/// If this process was started as the elevated helper that writes the
/// System PATH (see `os::write_system_elevated`), do that and return the
/// exit code; `main` exits with it before opening any window. `None` for a
/// normal start.
pub fn run_elevated_helper() -> Option<i32> {
    #[cfg(windows)]
    return os::run_elevated_helper();
    #[cfg(not(windows))]
    None
}

#[cfg(windows)]
mod os {
    use std::{env, ffi::OsStr, io, os::windows::ffi::OsStrExt, ptr::null_mut};

    use windows_sys::Win32::{
        Foundation::{
            CloseHandle, ERROR_ACCESS_DENIED, ERROR_CANCELLED, ERROR_FILE_NOT_FOUND,
            ERROR_INVALID_PARAMETER, ERROR_MORE_DATA, ERROR_SUCCESS,
        },
        System::{
            Registry::{
                HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, REG_EXPAND_SZ, RRF_NOEXPAND,
                RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegGetValueW, RegSetKeyValueW,
            },
            Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject},
        },
        UI::{
            Shell::{
                SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
            },
            WindowsAndMessaging::{
                HWND_BROADCAST, SMTO_ABORTIFHUNG, SW_HIDE, SendMessageTimeoutW, WM_SETTINGCHANGE,
            },
        },
    };

    use super::{Loaded, join, split};

    const USER_KEY: &str = "Environment";
    const SYSTEM_KEY: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment";
    /// The argument that starts this exe as the elevated System PATH writer,
    /// followed by the new value.
    const HELPER_ARG: &str = "--set-system-path";

    pub const SYSTEM_LOCATION: &str = r"HKEY_LOCAL_MACHINE\...\Session Manager\Environment";
    pub const SYSTEM_NOTE: &str = "PATH for every account; a new program gets these entries, \
        then User's. Saving needs admin: Windows will ask.";
    pub const USER_TAKES_EFFECT: &str =
        "Programs started from now on get it; open terminals keep their old PATH";
    pub const SYSTEM_TAKES_EFFECT: &str = USER_TAKES_EFFECT;

    pub fn user_location(_rc_file: &str) -> String {
        r"HKEY_CURRENT_USER\Environment".to_owned()
    }

    pub fn user_note(_rc_file: &str) -> String {
        "Your account's PATH; a new program gets System's entries, then these.".to_owned()
    }

    pub fn load_user(_rc_file: &str) -> io::Result<Loaded> {
        Ok(Loaded::entries(split(&read_path(
            HKEY_CURRENT_USER,
            USER_KEY,
        )?)))
    }

    pub fn load_system() -> io::Result<Loaded> {
        Ok(Loaded::entries(split(&read_path(
            HKEY_LOCAL_MACHINE,
            SYSTEM_KEY,
        )?)))
    }

    pub fn save_user(_rc_file: &str, entries: &[String]) -> Result<(), String> {
        write_path(HKEY_CURRENT_USER, USER_KEY, &join(entries)).map_err(|e| e.to_string())?;
        broadcast_change();
        Ok(())
    }

    /// Write the System PATH directly if this app is running as admin, and
    /// otherwise through an elevated copy of it, which Windows asks the user
    /// to allow.
    pub fn save_system(entries: &[String]) -> Result<(), String> {
        let value = join(entries);
        match write_path(HKEY_LOCAL_MACHINE, SYSTEM_KEY, &value) {
            Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
                write_system_elevated(&value)?
            }
            r => r.map_err(|e| e.to_string())?,
        }
        broadcast_change();
        Ok(())
    }

    /// `s` as a NUL-terminated UTF-16 string, for Win32 calls.
    fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
        s.as_ref().encode_wide().chain(Some(0)).collect()
    }

    /// The `Path` value under `root\subkey`, unexpanded (`%SystemRoot%` stays
    /// as written). Empty if there's no such value.
    pub(super) fn read_path(root: HKEY, subkey: &str) -> io::Result<String> {
        let (subkey, name) = (wide(subkey), wide("Path"));
        let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND;
        // The first call, with no buffer, asks for the size; loop in case
        // the value grows between calls.
        let mut buf: Vec<u16> = Vec::new();
        loop {
            let mut bytes = (buf.len() * 2) as u32;
            let data = if buf.is_empty() {
                null_mut()
            } else {
                buf.as_mut_ptr().cast()
            };
            // SAFETY: the strings are NUL-terminated; `data` is null or
            // has room for `bytes` bytes.
            let err = unsafe {
                RegGetValueW(
                    root,
                    subkey.as_ptr(),
                    name.as_ptr(),
                    flags,
                    null_mut(),
                    data,
                    &mut bytes,
                )
            };
            match err {
                ERROR_SUCCESS if !buf.is_empty() => {
                    buf.truncate(bytes as usize / 2);
                    while buf.last() == Some(&0) {
                        buf.pop();
                    }
                    return Ok(String::from_utf16_lossy(&buf));
                }
                ERROR_SUCCESS | ERROR_MORE_DATA => buf = vec![0; bytes as usize / 2 + 1],
                ERROR_FILE_NOT_FOUND => return Ok(String::new()),
                e => return Err(io::Error::from_raw_os_error(e as i32)),
            }
        }
    }

    /// Set the `Path` value under `root\subkey`. Written as `REG_EXPAND_SZ`,
    /// as Windows keeps PATH, so `%SystemRoot%` and the like are expanded
    /// when it's read.
    pub(super) fn write_path(root: HKEY, subkey: &str, value: &str) -> io::Result<()> {
        let (subkey, name) = (wide(subkey), wide("Path"));
        let data = wide(value);
        // SAFETY: the strings are NUL-terminated; `data` is `cbdata` bytes.
        let err = unsafe {
            RegSetKeyValueW(
                root,
                subkey.as_ptr(),
                name.as_ptr(),
                REG_EXPAND_SZ,
                data.as_ptr().cast(),
                (data.len() * 2) as u32,
            )
        };
        if err == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(err as i32))
        }
    }

    /// Tell running programs the environment changed, as Windows' own dialog
    /// does, so Explorer gives programs it starts from now on the new PATH.
    /// On a thread, as a hung window can hold it up for the timeout.
    fn broadcast_change() {
        std::thread::spawn(|| {
            let param = wide("Environment");
            let mut result = 0;
            // SAFETY: `param` is NUL-terminated and outlives the call.
            unsafe {
                SendMessageTimeoutW(
                    HWND_BROADCAST,
                    WM_SETTINGCHANGE,
                    0,
                    param.as_ptr() as isize,
                    SMTO_ABORTIFHUNG,
                    5000,
                    &mut result,
                )
            };
        });
    }

    /// Run this exe as admin with [HELPER_ARG] and `value`, which Windows
    /// asks the user to allow, and wait for it to write the System PATH.
    /// Blocks the UI meanwhile, like a command does; while Windows' prompt
    /// is up the app can't be used anyway.
    fn write_system_elevated(value: &str) -> Result<(), String> {
        let exe = env::current_exe().map_err(|e| e.to_string())?;
        let (verb, file) = (wide("runas"), wide(&exe));
        let params = wide(format!("{HELPER_ARG} {}", quote_arg(value)));
        let mut info = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
            lpVerb: verb.as_ptr(),
            lpFile: file.as_ptr(),
            lpParameters: params.as_ptr(),
            nShow: SW_HIDE,
            ..Default::default()
        };
        // SAFETY: the strings in `info` are NUL-terminated and outlive the
        // call.
        if unsafe { ShellExecuteExW(&mut info) } == 0 {
            let e = io::Error::last_os_error();
            return Err(if e.raw_os_error() == Some(ERROR_CANCELLED as i32) {
                "not allowed at the admin prompt".to_owned()
            } else {
                format!("couldn't run as admin: {e}")
            });
        }
        if info.hProcess.is_null() {
            return Err("couldn't run as admin".to_owned());
        }
        let mut code = 1;
        // SAFETY: `hProcess` is the helper's handle, ours to wait on and
        // close.
        unsafe {
            WaitForSingleObject(info.hProcess, INFINITE);
            GetExitCodeProcess(info.hProcess, &mut code);
            CloseHandle(info.hProcess);
        }
        if code == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(code as i32).to_string())
        }
    }

    /// See [super::run_elevated_helper].
    pub fn run_elevated_helper() -> Option<i32> {
        let mut args = env::args_os().skip(1);
        if args.next()? != HELPER_ARG {
            return None;
        }
        let code = match args.next().map(|v| v.into_string()) {
            Some(Ok(value)) => match write_path(HKEY_LOCAL_MACHINE, SYSTEM_KEY, &value) {
                Ok(()) => 0,
                Err(e) => e.raw_os_error().unwrap_or(1),
            },
            _ => ERROR_INVALID_PARAMETER as i32,
        };
        Some(code)
    }

    /// `s` quoted as one command-line argument, so the program's argument
    /// parser (Rust's, or the C runtime's) reads it back unchanged:
    /// backslashes are doubled where they precede a quote, as is the quote.
    pub(super) fn quote_arg(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        let mut backslashes = 0;
        for c in s.chars() {
            match c {
                '\\' => {
                    backslashes += 1;
                    continue;
                }
                '"' => out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1)),
                _ => out.extend(std::iter::repeat_n('\\', backslashes)),
            }
            backslashes = 0;
            out.push(c);
        }
        // Before the closing quote, each backslash needs doubling.
        out.extend(std::iter::repeat_n('\\', backslashes * 2));
        out.push('"');
        out
    }
}

#[cfg(not(windows))]
mod os {
    use std::{
        fs,
        io::{self, Write},
        path::PathBuf,
        process::{Command, Stdio},
    };

    use super::Loaded;

    /// The lines that open and close the block this panel keeps in the
    /// User startup file.
    const BLOCK_START: &str = "# >>> peregrine PATH >>>";
    const BLOCK_END: &str = "# <<< peregrine PATH <<<";

    pub const USER_TAKES_EFFECT: &str =
        "New terminals get it; open ones keep their old PATH until they `source` the file";

    #[cfg(target_os = "macos")]
    pub const SYSTEM_LOCATION: &str = "/etc/paths";
    #[cfg(target_os = "macos")]
    pub const SYSTEM_NOTE: &str = "The base PATH for every account, one dir per line; \
        User's entries go in front of it. Saving needs an admin password.";
    #[cfg(target_os = "macos")]
    pub const SYSTEM_TAKES_EFFECT: &str = "New terminals get it";

    #[cfg(not(target_os = "macos"))]
    pub const SYSTEM_LOCATION: &str = "/etc/environment";
    #[cfg(not(target_os = "macos"))]
    pub const SYSTEM_NOTE: &str = "The base PATH for every account's login sessions; \
        User's entries go in front of it. Saving needs root: you'll be asked for your password.";
    #[cfg(not(target_os = "macos"))]
    pub const SYSTEM_TAKES_EFFECT: &str = "It takes effect from your next login";

    pub fn user_location(rc_file: &str) -> String {
        format!("~/{rc_file}")
    }

    pub fn user_note(rc_file: &str) -> String {
        let reader = match rc_file {
            ".bashrc" => {
                "each new bash terminal. Desktop apps get their PATH from \
                 ~/.profile at login instead"
            }
            ".bash_profile" => "bash login shells (each new terminal on macOS)",
            ".profile" => "login shells, and on most Linux desktops, the desktop session at login",
            ".zshrc" => "each new zsh terminal",
            ".zprofile" => "zsh login shells (each new terminal on macOS)",
            _ => "the shell",
        };
        format!(
            "Your PATH additions: a block this panel keeps in ~/{rc_file} adds them to the \
             front of PATH, skipping any already there. Read by {reader}."
        )
    }

    fn rc_path(rc_file: &str) -> io::Result<PathBuf> {
        shell::get_home()
            .map(|h| h.join(rc_file))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home dir ($HOME)"))
    }

    /// `path`'s text, or empty if it doesn't exist.
    fn read_or_empty(path: &str) -> io::Result<String> {
        match fs::read_to_string(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
            r => r,
        }
    }

    pub fn load_user(rc_file: &str) -> io::Result<Loaded> {
        let path = rc_path(rc_file)?;
        let text = read_or_empty(&path.to_string_lossy())?;
        let (entries, others) = parse_rc(&text);
        Ok(Loaded {
            entries,
            read_only: None,
            others,
            others_title: format!("Other PATH lines in ~/{rc_file}"),
        })
    }

    /// The entries in `text`'s managed block, and its other lines that set
    /// or use PATH (numbered, outside comments), for the panel to list.
    pub(super) fn parse_rc(text: &str) -> (Vec<String>, Vec<String>) {
        let mut entries = Vec::new();
        let mut others = Vec::new();
        let mut in_block = false;
        for (i, line) in text.lines().enumerate() {
            let t = line.trim();
            if t == BLOCK_START {
                in_block = true;
            } else if t == BLOCK_END {
                in_block = false;
            } else if in_block {
                // An entry's line is just it in quotes, maybe followed by a
                // line continuation.
                let t = t.strip_suffix('\\').map_or(t, str::trim_end);
                if let Some(e) = t.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
                    entries.push(e.to_owned());
                }
            } else if !t.starts_with('#') && mentions_path(t) {
                others.push(format!("{}: {t}", i + 1));
            }
        }
        (entries, others)
    }

    /// Whether `line` has `PATH` as a whole word, as in `export PATH=` or
    /// `$PATH`, but not `MANPATH`.
    fn mentions_path(line: &str) -> bool {
        let word = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        line.match_indices("PATH").any(|(i, _)| {
            !word(line[..i].chars().next_back()) && !word(line[i + 4..].chars().next())
        })
    }

    /// The managed block adding `entries` to the front of PATH, in order,
    /// skipping any PATH already has. POSIX sh, so bash and zsh read it too.
    /// Sourcing it twice adds nothing more.
    pub(super) fn block(entries: &[String]) -> String {
        let mut s = format!(
            "{BLOCK_START}\n\
             # Kept by Shell GUI's PATH panel: edit the list there. Adds these dirs\n\
             # to the front of PATH, in this order, skipping any it already has.\n\
             __sg_path=\n\
             for __sg_dir in \\\n"
        );
        for (i, e) in entries.iter().enumerate() {
            let cont = if i + 1 < entries.len() { " \\" } else { "" };
            s += &format!("    \"{e}\"{cont}\n");
        }
        s += "do\n\
              \x20   case \":$__sg_path:$PATH:\" in\n\
              \x20       *\":$__sg_dir:\"*) ;;\n\
              \x20       *) __sg_path=\"${__sg_path:+$__sg_path:}$__sg_dir\" ;;\n\
              \x20   esac\n\
              done\n\
              if [ -n \"$__sg_path\" ]; then\n\
              \x20   export PATH=\"$__sg_path${PATH:+:$PATH}\"\n\
              fi\n\
              unset __sg_path __sg_dir\n";
        s += BLOCK_END;
        s.push('\n');
        s
    }

    /// `text` with its managed block replaced by `block` (dropped when
    /// `block` is empty), or `block` added at the end if it had none. All
    /// else is kept as it was. Errors if a block is never closed, rather
    /// than drop the rest of the file with it.
    pub(super) fn replace_block(text: &str, block: &str) -> Result<String, String> {
        let mut out = String::with_capacity(text.len() + block.len());
        let mut in_block = false;
        let mut placed = false;
        for line in text.split_inclusive('\n') {
            let t = line.trim();
            if in_block {
                in_block = t != BLOCK_END;
            } else if t == BLOCK_START {
                in_block = true;
                if !placed {
                    out.push_str(block);
                    placed = true;
                }
            } else {
                out.push_str(line);
            }
        }
        if in_block {
            return Err(format!(
                "it has a `{BLOCK_START}` line without a `{BLOCK_END}` after it; fix that by hand"
            ));
        }
        if !placed && !block.is_empty() {
            if !out.is_empty() {
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push('\n');
            }
            out.push_str(block);
        }
        Ok(out)
    }

    /// Rewrite the managed block in the startup file, first copying the file
    /// as it was to `<file>.peregrine.bak`. Written in place, so a startup
    /// file that's a symlink (to a dotfiles repo, say) stays one.
    pub fn save_user(rc_file: &str, entries: &[String]) -> Result<(), String> {
        let path = rc_path(rc_file).map_err(|e| e.to_string())?;
        let shown = user_location(rc_file);
        let old = read_or_empty(&path.to_string_lossy()).map_err(|e| format!("{shown}: {e}"))?;
        let block = if entries.is_empty() {
            String::new()
        } else {
            block(entries)
        };
        let new = replace_block(&old, &block).map_err(|e| format!("{shown}: {e}"))?;
        if new == old {
            return Ok(());
        }
        if !old.is_empty() {
            let mut backup = path.clone().into_os_string();
            backup.push(".peregrine.bak");
            fs::write(&backup, &old).map_err(|e| format!("backing up {shown}: {e}"))?;
        }
        fs::write(&path, new).map_err(|e| format!("{shown}: {e}"))
    }

    /// Run `program args` (which gets root through the OS's password
    /// prompt), feeding it `input` if given, and wait for it.
    fn run_as_root(
        program: &str,
        args: &[&str],
        input: Option<&str>,
    ) -> Result<std::process::Output, String> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("couldn't run {program}: {e}"))?;
        if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
            // A failed write shows up as the program's failure below.
            let _ = stdin.write_all(input.as_bytes());
        }
        child.wait_with_output().map_err(|e| e.to_string())
    }

    #[cfg(not(target_os = "macos"))]
    const ETC_ENVIRONMENT: &str = "/etc/environment";

    /// The value of `line` if it sets PATH, as `/etc/environment` (pam_env)
    /// reads it: `PATH=...`, maybe after `export`, maybe quoted.
    #[cfg(not(target_os = "macos"))]
    pub(super) fn env_path_value(line: &str) -> Option<&str> {
        let t = line.trim();
        let t = t.strip_prefix("export ").map_or(t, str::trim_start);
        let v = t.strip_prefix("PATH=")?.trim();
        for q in ['"', '\''] {
            if let Some(inner) = v.strip_prefix(q).and_then(|v| v.strip_suffix(q)) {
                return Some(inner);
            }
        }
        Some(v)
    }

    #[cfg(not(target_os = "macos"))]
    pub fn load_system() -> io::Result<Loaded> {
        let text = read_or_empty(ETC_ENVIRONMENT)?;
        // pam_env reads the lines in order, so the last PATH line wins.
        Ok(match text.lines().rev().find_map(env_path_value) {
            Some(value) => Loaded::entries(super::split(value)),
            None => Loaded {
                entries: Vec::new(),
                read_only: Some(format!(
                    "{ETC_ENVIRONMENT} doesn't set PATH, so this system's base PATH comes \
                     from elsewhere (e.g. /etc/profile or /etc/login.defs). Add a PATH= \
                     line there by hand to edit it here."
                )),
                others: Vec::new(),
                others_title: String::new(),
            },
        })
    }

    /// `text` with its last PATH line set to `entries`; the other lines are
    /// kept as they were.
    #[cfg(not(target_os = "macos"))]
    pub(super) fn set_env_path(text: &str, entries: &[String]) -> String {
        let last = text
            .split_inclusive('\n')
            .enumerate()
            .filter(|(_, l)| env_path_value(l).is_some())
            .map(|(i, _)| i)
            .last();
        let mut out = String::with_capacity(text.len());
        for (i, line) in text.split_inclusive('\n').enumerate() {
            if Some(i) == last {
                out += &format!("PATH=\"{}\"\n", super::join(entries));
            } else {
                out.push_str(line);
            }
        }
        out
    }

    /// Write `/etc/environment` with its PATH line changed, as root through
    /// `pkexec` (polkit's password prompt). The new text goes over stdin to
    /// `tee`, not through a temp file another program could swap.
    #[cfg(not(target_os = "macos"))]
    pub fn save_system(entries: &[String]) -> Result<(), String> {
        let text = read_or_empty(ETC_ENVIRONMENT).map_err(|e| e.to_string())?;
        let new = set_env_path(&text, entries);
        let out = run_as_root("pkexec", &["tee", ETC_ENVIRONMENT], Some(&new)).map_err(|e| {
            format!("{e}. Without pkexec, edit {ETC_ENVIRONMENT} as root (e.g. `sudoedit`)")
        })?;
        match out.status.code() {
            Some(0) => Ok(()),
            Some(126) => Err("the password prompt was dismissed".to_owned()),
            Some(127) => Err(format!(
                "not authorized: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            _ => Err(String::from_utf8_lossy(&out.stderr).trim().to_owned()),
        }
    }

    #[cfg(target_os = "macos")]
    pub fn load_system() -> io::Result<Loaded> {
        let lines = |text: &str| -> Vec<String> {
            text.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(str::to_owned)
                .collect()
        };
        let entries = lines(&read_or_empty("/etc/paths")?);
        // path_helper adds these files' dirs after /etc/paths', in name order.
        let mut files: Vec<PathBuf> = fs::read_dir("/etc/paths.d")
            .map(|d| d.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        files.sort();
        let others = files
            .iter()
            .flat_map(|f| {
                let text = fs::read_to_string(f).unwrap_or_default();
                lines(&text)
                    .into_iter()
                    .map(move |l| format!("{}: {l}", f.display()))
            })
            .collect();
        Ok(Loaded {
            entries,
            read_only: None,
            others,
            others_title: "Also added from /etc/paths.d".to_owned(),
        })
    }

    /// `s` in single quotes for sh.
    #[cfg(target_os = "macos")]
    fn sh_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', r"'\''"))
    }

    /// Write `/etc/paths` as root, through AppleScript's admin password
    /// prompt.
    #[cfg(target_os = "macos")]
    pub fn save_system(entries: &[String]) -> Result<(), String> {
        let args: Vec<String> = entries.iter().map(|e| sh_quote(e)).collect();
        let sh = format!("printf '%s\\n' {} > /etc/paths", args.join(" "));
        let script = format!(
            "do shell script \"{}\" with administrator privileges",
            sh.replace('\\', "\\\\").replace('"', "\\\"")
        );
        let out = run_as_root("osascript", &["-e", &script], None)?;
        let err = String::from_utf8_lossy(&out.stderr);
        if out.status.success() {
            Ok(())
        } else if err.contains("-128") {
            Err("the password prompt was cancelled".to_owned())
        } else {
            Err(err.trim().to_owned())
        }
    }
}
