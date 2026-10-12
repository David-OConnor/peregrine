//! Support for the Files panel: opening a file with its default program, and
//! the details shown for the selected row.

use std::{
    fs, io,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_sizes() {
        assert_eq!(format_size(0), "0 bytes");
        assert_eq!(format_size(1023), "1023 bytes");
        assert_eq!(format_size(1024), "1.0 KB (1,024 bytes)");
        assert_eq!(format_size(1_482_112), "1.4 MB (1,482,112 bytes)");
        assert_eq!(
            format_size(3 * 1024 * 1024 * 1024),
            "3.0 GB (3,221,225,472 bytes)"
        );
    }

    #[test]
    fn generic_type_names() {
        assert_eq!(
            generic_type_name(Path::new("a/Cargo.toml"), false),
            "TOML file"
        );
        assert_eq!(generic_type_name(Path::new("a/LICENSE"), false), "File");
        assert_eq!(generic_type_name(Path::new("a/src"), true), "Folder");
    }
}
