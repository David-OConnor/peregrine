//! Support for the Files panel: opening a file with its OS-default program, and
//! the details shown for the selected row.

use std::{
    env, fs, io,
    path::{Path, PathBuf},
    time::SystemTime,
};

use shell::BrowserFile;

/// What the Files panel's details pane shows for the selected row. Read once
/// on selection (and again when the listing refreshes) by [FileDetails::read],
/// so rendering never touches the filesystem.
pub struct FileDetails {
    pub path: PathBuf,
    pub name: String,
    /// Friendly type, e.g. "Text Document" or "File folder". See [type_name].
    pub type_name: String,
    pub is_folder: bool,
    /// Size in bytes. `None` for folders.
    pub size: Option<u64>,
    /// Number of direct children. `None` for files, or an unreadable folder.
    pub item_count: Option<usize>,
    pub modified: Option<SystemTime>,
    pub created: Option<SystemTime>,
    pub readonly: bool,
    pub hidden: bool,
    pub executable: bool,
    /// Where it points, if this entry is a symlink.
    pub link_target: Option<PathBuf>,
}

impl FileDetails {
    /// Stat `f`. `None` if it no longer exists (e.g. deleted since the
    /// listing was read).
    pub fn read(f: &BrowserFile) -> Option<Self> {
        let link_md = fs::symlink_metadata(&f.path).ok()?;
        let link_target = if link_md.file_type().is_symlink() {
            fs::read_link(&f.path).ok()
        } else {
            None
        };
        // Follow a symlink so size and dates describe its target; a dangling
        // link falls back to the link itself.
        let md = fs::metadata(&f.path).unwrap_or(link_md);

        let item_count = if f.is_folder {
            fs::read_dir(&f.path).ok().map(|e| e.count())
        } else {
            None
        };

        Some(Self {
            path: f.path.clone(),
            name: f.disp_name.clone(),
            type_name: type_name(&f.path, f.is_folder),
            is_folder: f.is_folder,
            size: (!f.is_folder).then_some(md.len()),
            item_count,
            modified: md.modified().ok(),
            created: md.created().ok(),
            readonly: md.permissions().readonly(),
            hidden: is_hidden(&f.disp_name, &md),
            executable: f.is_executable,
            link_target,
        })
    }

    /// [Self::read] for a file or folder Deep search found, which (being in
    /// a subdir, usually) has no row in the listing to read from.
    pub fn read_file(path: &Path) -> Option<Self> {
        let md = fs::metadata(path).ok()?;
        Self::read(&BrowserFile {
            path: path.to_path_buf(),
            disp_name: path.file_name()?.to_string_lossy().into_owned(),
            is_folder: md.is_dir(),
            is_executable: !md.is_dir() && is_executable(path, &md),
            hidden_attr: false,
        })
    }
}

/// Same test `shell` uses for the listing's [BrowserFile::is_executable]:
/// Unix +x, or on Windows an `.exe` or `.msi`.
#[cfg(unix)]
fn is_executable(_path: &Path, md: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    md.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(path: &Path, _md: &fs::Metadata) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("exe") || e.eq_ignore_ascii_case("msi"))
}

#[cfg(windows)]
fn is_hidden(_name: &str, md: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN;
    md.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0
}

#[cfg(not(windows))]
fn is_hidden(name: &str, _md: &fs::Metadata) -> bool {
    name.starts_with('.')
}

/// `path` as a NUL-terminated UTF-16 string, for Win32 calls.
#[cfg(windows)]
fn to_wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// The type name Explorer shows in its "Type" column ("Text Document",
/// "Application", "File folder"). `SHGFI_USEFILEATTRIBUTES` makes this a
/// lookup on the extension alone, without opening the file.
#[cfg(windows)]
fn type_name(path: &Path, is_folder: bool) -> String {
    use windows_sys::Win32::{
        Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL},
        UI::Shell::{SHFILEINFOW, SHGFI_TYPENAME, SHGFI_USEFILEATTRIBUTES, SHGetFileInfoW},
    };

    let wide = to_wide(path);
    let attrs = if is_folder {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
    // SAFETY: `wide` is NUL-terminated and outlives the call; `info` is a
    // plain-data out-param sized by `cbfileinfo`.
    let mut info: SHFILEINFOW = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        SHGetFileInfoW(
            wide.as_ptr(),
            attrs,
            &mut info,
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_TYPENAME | SHGFI_USEFILEATTRIBUTES,
        )
    };
    let len = info
        .szTypeName
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(info.szTypeName.len());
    if ok != 0 && len > 0 {
        String::from_utf16_lossy(&info.szTypeName[..len])
    } else {
        generic_type_name(path, is_folder)
    }
}

#[cfg(not(windows))]
fn type_name(path: &Path, is_folder: bool) -> String {
    generic_type_name(path, is_folder)
}

/// Fallback type name from the extension alone, e.g. `TOML file`.
fn generic_type_name(path: &Path, is_folder: bool) -> String {
    if is_folder {
        return "Folder".to_string();
    }
    match path.extension() {
        Some(ext) => format!("{} file", ext.to_string_lossy().to_uppercase()),
        None => "File".to_string(),
    }
}

/// A byte count for display, e.g. `1.4 MB (1,482,112 bytes)`. Binary
/// (1024-based) units, matching what Windows Explorer reports.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    let mut value = bytes as f64 / 1024.;
    let mut unit = 0;
    while value >= 1024. && unit < UNITS.len() - 1 {
        value /= 1024.;
        unit += 1;
    }
    format!(
        "{value:.1} {} ({} bytes)",
        UNITS[unit],
        group_thousands(bytes)
    )
}

/// `1482112` -> `1,482,112`.
fn group_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Open `path` with the operating system's default associated program, the
/// same way double-clicking it in Explorer (Windows) or a file manager
/// (Linux) would. Returns once the program is launched, not when it exits.
///
/// - Windows: `ShellExecuteW` with no verb, which runs the file type's
///   default action — exactly what an Explorer double-click does. Calling it
///   directly (rather than `cmd /C start`) means names containing `&`, `^`
///   or `%` aren't mangled by cmd's parser.
/// - macOS: `open <path>`.
/// - Other (Linux/BSD): `xdg-open <path>`.
#[cfg(windows)]
pub fn open_with_default_app(path: &Path) -> io::Result<()> {
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};

    let wide = to_wide(path);
    // SAFETY: `wide` is NUL-terminated and outlives the call; the other
    // pointer args are documented as optional (null).
    let result = unsafe {
        ShellExecuteW(
            null_mut(),
            null(),
            wide.as_ptr(),
            null(),
            null(),
            SW_SHOWNORMAL,
        )
    };
    // Values above 32 mean success; anything else is an error code, with the
    // detail available from GetLastError.
    if result as isize > 32 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(windows))]
pub fn open_with_default_app(path: &Path) -> io::Result<()> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    shell::quiet_command(program).arg(path).spawn().map(|_| ())
}

/// Rename `path` to `new_name` within its own directory, as a file manager's
/// in-place rename does, returning the new path. Surrounding spaces are
/// trimmed (and on Windows, trailing dots, which it would drop anyway).
///
/// A name with a path separator is refused, since it would move the entry
/// rather than rename it; so is one that's taken, since `fs::rename` silently
/// replaces an existing file. A change of case alone is allowed: on a
/// case-insensitive filesystem the "existing" entry is this one.
pub fn rename_entry(path: &Path, new_name: &str) -> io::Result<PathBuf> {
    let mut name = new_name.trim();
    if cfg!(windows) {
        name = name.trim_end_matches(['.', ' ']);
    }
    let seps: &[char] = if cfg!(windows) { &['/', '\\'] } else { &['/'] };
    if name.is_empty() || name == "." || name == ".." || name.contains(seps) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("\"{new_name}\" isn't a valid name"),
        ));
    }
    let (Some(parent), Some(old_name)) = (path.parent(), path.file_name()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "can't rename a filesystem root",
        ));
    };
    if old_name == name {
        return Ok(path.to_path_buf());
    }

    let new_path = parent.join(name);
    // Another entry with exactly this name, or (on a case-insensitive
    // filesystem) one that differs from it in more than case.
    let exact = fs::read_dir(parent)?
        .flatten()
        .any(|e| e.file_name() == name);
    let case_only = old_name.to_string_lossy().to_lowercase() == name.to_lowercase();
    if exact || (!case_only && fs::symlink_metadata(&new_path).is_ok()) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{name} already exists"),
        ));
    }

    fs::rename(path, &new_path)?;
    Ok(new_path)
}

/// One of the Files panel's jump-to buttons.
pub struct Shortcut {
    pub glyph: &'static str,
    pub hover: &'static str,
    /// `None` when it can't be worked out (no home dir); the button is then
    /// shown disabled, so the row's layout doesn't shift.
    pub dir: Option<PathBuf>,
}

/// The Files panel's jump-to buttons that go to an ordinary dir, in display
/// order. Home, Desktop and Downloads are the same everywhere; the others are
/// each OS's counterpart of the same place (see [app_data], [programs] and
/// [trash_dir]). Windows has no single root, and its Recycle Bin isn't a
/// listable dir, so there those two are the drive menu ([drives]) and
/// [open_recycle_bin] instead, and aren't listed here.
pub fn shortcuts(home: Option<&Path>) -> Vec<Shortcut> {
    let in_home = |sub: &str| home.map(|h| h.join(sub));
    #[allow(unused_mut)]
    let mut list = vec![
        Shortcut {
            glyph: "🏠",
            hover: "Home directory",
            dir: home.map(Path::to_path_buf),
        },
        app_data(home),
        Shortcut {
            glyph: "🖥",
            hover: "Desktop",
            dir: in_home("Desktop"),
        },
        Shortcut {
            glyph: "📥",
            hover: "Downloads",
            dir: in_home("Downloads"),
        },
        programs(),
    ];
    #[cfg(not(windows))]
    list.extend([
        Shortcut {
            glyph: "🖴",
            hover: "Filesystem root (/)",
            dir: Some(PathBuf::from("/")),
        },
        Shortcut {
            glyph: "🗑",
            hover: "Trash",
            dir: trash_dir(home),
        },
    ]);
    list
}

/// Per-user app data: `AppData` on Windows, `~/Library` on macOS, and the
/// XDG tree `~/.local` elsewhere. (A Mac may have a `~/.local` too, but only
/// from Unix tools like pipx; its own apps keep everything in `~/Library`.)
fn app_data(home: Option<&Path>) -> Shortcut {
    let (hover, sub) = if cfg!(windows) {
        ("AppData", "AppData")
    } else if cfg!(target_os = "macos") {
        ("~/Library", "Library")
    } else {
        ("~/.local", ".local")
    };
    Shortcut {
        glyph: "⚙",
        hover,
        dir: home.map(|h| h.join(sub)),
    }
}

/// Where installed programs live: `Program Files` on Windows (from
/// `%ProgramFiles%`, so it's the native one on a 64-bit OS), `/Applications`
/// on macOS, and `/usr` elsewhere.
fn programs() -> Shortcut {
    let (hover, dir) = if cfg!(windows) {
        let dir = env::var_os("ProgramFiles")
            .map_or_else(|| PathBuf::from(r"C:\Program Files"), PathBuf::from);
        ("Program Files", dir)
    } else if cfg!(target_os = "macos") {
        ("Applications", PathBuf::from("/Applications"))
    } else {
        ("/usr", PathBuf::from("/usr"))
    };
    Shortcut {
        glyph: "📦",
        hover,
        dir: Some(dir),
    }
}

/// The dir holding trashed items under their own names: `~/.Trash` on macOS,
/// and elsewhere the freedesktop.org trash's `files` dir, in
/// `$XDG_DATA_HOME` (`~/.local/share` by default). The latter only exists
/// once something has been trashed.
#[cfg(not(windows))]
fn trash_dir(home: Option<&Path>) -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        return home.map(|h| h.join(".Trash"));
    }
    // The spec says to ignore a relative `$XDG_DATA_HOME`.
    let data = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.map(|h| h.join(".local/share")))?;
    Some(data.join("Trash/files"))
}

/// Show the Recycle Bin in Explorer. It can't be listed like a dir: Windows
/// keeps one hidden `$Recycle.Bin` per drive, holding each item under a
/// generated name, with its real name and location in a separate file.
#[cfg(windows)]
pub fn open_recycle_bin() -> io::Result<()> {
    open_with_default_app(Path::new("shell:RecycleBinFolder"))
}

/// A drive, as Explorer's "This PC" lists it.
#[cfg(windows)]
pub struct Drive {
    /// e.g. `C:\`.
    pub root: PathBuf,
    /// 🖴 local disk, 💾 removable, 💿 optical or 🖧 network.
    pub glyph: &'static str,
    /// Explorer's name for it: its volume label, or failing that its kind,
    /// then the letter, e.g. "Data (D:)" or "Local Disk (C:)".
    pub name: String,
}

/// Every drive letter in use, A to Z.
#[cfg(windows)]
pub fn drives() -> Vec<Drive> {
    use windows_sys::Win32::{
        Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives},
        System::WindowsProgramming::{DRIVE_CDROM, DRIVE_REMOTE, DRIVE_REMOVABLE},
    };

    // SAFETY: no arguments; returns a bitmask of the letters in use, A = bit 0.
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| {
            let letter = char::from(b'A' + i);
            let root = PathBuf::from(format!(r"{letter}:\"));
            let wide = to_wide(&root);
            // SAFETY: `wide` is NUL-terminated and outlives the call.
            let kind = unsafe { GetDriveTypeW(wide.as_ptr()) };
            let (glyph, kind_name) = match kind {
                DRIVE_REMOVABLE => ("💾", "Removable Disk"),
                DRIVE_CDROM => ("💿", "CD Drive"),
                DRIVE_REMOTE => ("🖧", "Network Drive"),
                _ => ("🖴", "Local Disk"),
            };
            // An unreachable server or a disc spinning up can each stall for
            // seconds, so only drives that answer at once are asked.
            let label = if matches!(kind, DRIVE_CDROM | DRIVE_REMOTE) {
                None
            } else {
                volume_label(&wide)
            };
            let name = format!("{} ({letter}:)", label.as_deref().unwrap_or(kind_name));
            Drive { root, glyph, name }
        })
        .collect()
}

/// The volume label of the drive at `root` (a NUL-terminated `X:\`). `None`
/// if it has none, or (e.g. an empty card reader) no media.
#[cfg(windows)]
fn volume_label(root: &[u16]) -> Option<String> {
    use std::ptr::null_mut;

    use windows_sys::Win32::{
        Storage::FileSystem::GetVolumeInformationW,
        System::Diagnostics::Debug::{SEM_FAILCRITICALERRORS, SetThreadErrorMode},
    };

    // MAX_PATH + 1, the documented maximum.
    let mut buf = [0u16; 261];
    let mut old_mode = 0;
    // SAFETY: `root` is NUL-terminated and `buf`'s length is passed with it;
    // the other out-params are documented as optional (null). The error mode
    // stops a drive with no media from popping up a "no disk" dialog, and is
    // restored afterwards.
    let ok = unsafe {
        SetThreadErrorMode(SEM_FAILCRITICALERRORS, &mut old_mode);
        let ok = GetVolumeInformationW(
            root.as_ptr(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
            0,
        );
        SetThreadErrorMode(old_mode, null_mut());
        ok
    };
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    (ok != 0 && len > 0).then(|| String::from_utf16_lossy(&buf[..len]))
}
