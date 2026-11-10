//! Support for the Files panel: opening a file with its OS-default program, and
//! the details shown for the selected row.

use std::{
    env, fs, io,
    path::{Path, PathBuf},
    thread::{self, JoinHandle},
    time::SystemTime,
};

use shell::BrowserFile;

/// A confirmed deletion, moving items to the OS trash on a worker thread.
pub struct Delete {
    pub paths: Vec<PathBuf>,
    handle: Option<JoinHandle<Vec<Result<(), String>>>>,
}

impl Delete {
    pub fn start(mut paths: Vec<PathBuf>, ctx: egui::Context) -> Self {
        // Deep search can select both a folder and one of its descendants.
        // Move the folder first, then count its contents as handled too.
        paths.sort_by_key(|p| p.components().count());
        paths.dedup();
        let sources = paths.clone();
        let handle = thread::spawn(move || {
            let mut moved_dirs: Vec<PathBuf> = Vec::new();
            let results = sources
                .iter()
                .map(|path| {
                    if moved_dirs.iter().any(|dir| path.starts_with(dir)) {
                        return Ok(());
                    }
                    let is_dir = fs::symlink_metadata(path).is_ok_and(|md| md.is_dir());
                    // Never fall back to permanent deletion if trashing fails.
                    trash::delete(path).map_err(|e| e.to_string())?;
                    if is_dir {
                        moved_dirs.push(path.clone());
                    }
                    Ok(())
                })
                .collect();
            ctx.request_repaint();
            results
        });
        Self {
            paths,
            handle: Some(handle),
        }
    }

    pub fn try_finish(&mut self) -> Option<Vec<Result<(), String>>> {
        if !self.handle.as_ref()?.is_finished() {
            return None;
        }
        Some(self.handle.take()?.join().unwrap_or_else(|_| {
            vec![Err("the delete operation crashed".to_owned()); self.paths.len()]
        }))
    }
}

/// A zip archive being written or extracted on a worker thread (see
/// [ZipJob::compress] and [ZipJob::extract]).
pub struct ZipJob {
    /// Extracting, rather than compressing.
    pub extracting: bool,
    /// Returns what it made: the archive, or the folder extracted to.
    handle: Option<JoinHandle<Result<PathBuf, String>>>,
}

impl ZipJob {
    /// Start compressing `srcs`, files and folders in or under `dir`, into a
    /// new zip archive in `dir` named as Explorer names one: after `lead`, so
    /// `notes.txt` makes `notes.zip`, or if that's taken, `notes (2).zip`.
    /// Entries are named by their paths relative to `dir`. `ctx` is repainted
    /// when it's done. A failed archive is removed.
    pub fn compress(mut srcs: Vec<PathBuf>, dir: PathBuf, lead: &Path, ctx: egui::Context) -> Self {
        let stem = if lead.is_dir() {
            lead.file_name()
        } else {
            lead.file_stem()
        };
        let name = format!("{}.zip", stem.unwrap_or_default().to_string_lossy());

        // As for [Delete]: a folder and something in it are both selectable
        // in Deep search results, but it's only added once.
        srcs.sort_by_key(|p| p.components().count());
        srcs.dedup();

        let handle = thread::spawn(move || {
            let archive = crate::clipboard::free_name(&dir, name.as_ref(), false, true);
            let result = write_zip(&archive, &srcs, &dir);
            if result.is_err() {
                let _ = fs::remove_file(&archive);
            }
            ctx.request_repaint();
            result.map(|()| archive)
        });

        Self {
            extracting: false,
            handle: Some(handle),
        }
    }

    /// Start extracting the zip archive at `archive` into a new folder beside
    /// it. Where everything in it is in one folder, that folder is what's
    /// made, as macOS and Linux file managers do; otherwise it's named after
    /// the archive. Either way it takes a free name, e.g. `notes (2)`. `ctx`
    /// is repainted when it's done. A failed extraction's folder is removed.
    pub fn extract(archive: PathBuf, ctx: egui::Context) -> Self {
        let handle = thread::spawn(move || {
            let result = extract_zip(&archive);
            ctx.request_repaint();
            result
        });

        Self {
            extracting: true,
            handle: Some(handle),
        }
    }

    pub fn try_finish(&mut self) -> Option<Result<PathBuf, String>> {
        if !self.handle.as_ref()?.is_finished() {
            return None;
        }
        let result = self.handle.take()?.join();

        Some(result.unwrap_or_else(|_| Err("the zip operation crashed".to_owned())))
    }
}

/// Whether `path` is named as a zip archive.
pub fn is_zip(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
}

/// Extract `archive` as [ZipJob::extract] says, returning the folder made.
fn extract_zip(archive: &Path) -> Result<PathBuf, String> {
    let err = |e: &dyn std::fmt::Display| format!("{}: {e}", archive.display());

    let file = fs::File::open(archive).map_err(|e| err(&e))?;
    let mut zip = zip::ZipArchive::new(io::BufReader::new(file)).map_err(|e| err(&e))?;

    // Leaves out the likes of `__MACOSX` and `.DS_Store` in looking for it.
    let filter = zip::read::root_dir_common_filter;
    let root = zip.root_dir(filter).map_err(|e| err(&e))?;
    let name = match &root {
        Some(root) => root.file_name(),
        None => archive.file_stem(),
    };
    let dir = archive.parent().unwrap_or(Path::new("."));
    let dest = crate::clipboard::free_name(dir, name.unwrap_or_default(), true, true);
    fs::create_dir(&dest).map_err(|e| format!("{}: {e}", dest.display()))?;

    let extracted = match root {
        Some(_) => zip.extract_unwrapped_root_dir(&dest, filter),
        None => zip.extract(&dest),
    };
    if let Err(e) = extracted {
        // Made just now, so there's nothing else in it.
        let _ = fs::remove_dir_all(&dest);
        return Err(err(&e));
    }

    // Zip stores local times, but `extract` would take them for UTC.
    for i in 0..zip.len() {
        let Ok(entry) = zip.by_index_raw(i) else {
            continue;
        };
        let (Some(rel), Some(time)) = (entry.enclosed_name(), entry.last_modified()) else {
            continue;
        };
        let rel = match &root {
            Some(root) => rel.strip_prefix(root).map(Path::to_path_buf).unwrap_or(rel),
            None => rel,
        };
        if entry.is_file()
            && let Some(time) = local_time(time)
            && let Ok(file) = fs::File::options().write(true).open(dest.join(rel))
        {
            let _ = file.set_modified(time);
        }
    }

    Ok(dest)
}

/// `t`, a time as zip stores it, in this time zone.
fn local_time(t: zip::DateTime) -> Option<SystemTime> {
    let date = chrono::NaiveDate::from_ymd_opt(t.year().into(), t.month().into(), t.day().into())?;
    let time = date.and_hms_opt(t.hour().into(), t.minute().into(), t.second().into())?;

    Some(time.and_local_timezone(chrono::Local).earliest()?.into())
}

/// Write `srcs` and everything in them to a new zip archive at `archive`,
/// named relative to `dir` (see [ZipJob::compress]).
fn write_zip(archive: &Path, srcs: &[PathBuf], dir: &Path) -> Result<(), String> {
    let file = fs::File::create_new(archive).map_err(|e| format!("{}: {e}", archive.display()))?;
    let mut zip = zip::ZipWriter::new(io::BufWriter::new(file));

    // Links are followed, but not into `dir` or a folder being added, so
    // they can't lead round in a loop, or into the archive itself.
    let mut within: Vec<PathBuf> = fs::canonicalize(dir).into_iter().collect();
    let mut added: Vec<&Path> = Vec::new();

    for src in srcs {
        if added.iter().any(|a| src.starts_with(a)) {
            continue;
        }
        let rel = src
            .strip_prefix(dir)
            .ok()
            .or_else(|| src.file_name().map(Path::new))
            .unwrap_or(src);
        let name: Vec<_> = rel.iter().map(|c| c.to_string_lossy()).collect();
        add_to_zip(&mut zip, src, &name.join("/"), &mut within)?;
        added.push(src);
    }

    zip.finish()
        .and_then(|w| w.into_inner().map_err(|e| e.into_error().into()))
        .map_err(|e| format!("{}: {e}", archive.display()))?;

    Ok(())
}

/// Add `path`, a file or folder and everything in it, to `zip` as `name`.
/// Links are followed, except to a folder in `within` (canonical): one it's
/// inside.
fn add_to_zip<W: io::Write + io::Seek>(
    zip: &mut zip::ZipWriter<W>,
    path: &Path,
    name: &str,
    within: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let err = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());

    let md = fs::metadata(path).map_err(|e| err(&e))?;
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(zip_time(&md));
    #[cfg(unix)]
    let options = {
        use std::os::unix::fs::PermissionsExt;
        options.unix_permissions(md.permissions().mode())
    };

    if md.is_dir() {
        let canonical = fs::canonicalize(path).map_err(|e| err(&e))?;
        if within.contains(&canonical) {
            return Ok(());
        }

        zip.add_directory(name, options).map_err(|e| err(&e))?;
        within.push(canonical);
        for entry in fs::read_dir(path).map_err(|e| err(&e))? {
            let entry = entry.map_err(|e| err(&e))?;
            let child = format!("{name}/{}", entry.file_name().to_string_lossy());
            add_to_zip(zip, &entry.path(), &child, within)?;
        }
        within.pop();

        return Ok(());
    }

    let options = options.large_file(md.len() >= u64::from(u32::MAX));
    zip.start_file(name, options).map_err(|e| err(&e))?;
    let mut file = fs::File::open(path).map_err(|e| err(&e))?;
    io::copy(&mut file, zip).map_err(|e| err(&e))?;

    Ok(())
}

/// `md`'s modified time, as zip stores it: local time, 1980 to 2107. Out of
/// that range, the start of 1980.
fn zip_time(md: &fs::Metadata) -> zip::DateTime {
    use chrono::{Datelike, Timelike};

    let Ok(t) = md.modified() else {
        return zip::DateTime::default();
    };
    let t = chrono::DateTime::<chrono::Local>::from(t);
    let Ok(year) = u16::try_from(t.year()) else {
        return zip::DateTime::default();
    };

    // The rest always fit in a u8.
    let [month, day, hour, min, sec] =
        [t.month(), t.day(), t.hour(), t.minute(), t.second()].map(|n| n as u8);

    zip::DateTime::from_date_and_time(year, month, day, hour, min, sec).unwrap_or_default()
}

/// What the Files panel's listing is ordered by, picked with the buttons
/// above it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Modified,
    /// By extension.
    Type,
    Size,
}

impl SortKey {
    pub const ALL: [Self; 4] = [Self::Name, Self::Modified, Self::Type, Self::Size];

    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Modified => "Modified",
            Self::Type => "Type",
            Self::Size => "Size",
        }
    }

    pub fn hover(self) -> &'static str {
        match self {
            Self::Name => "Sort by name",
            Self::Modified => "Sort by date modified",
            Self::Type => "Sort by type (file extension)",
            Self::Size => "Sort by size",
        }
    }

    /// Picking a key starts newest or largest first; names and types A to Z.
    pub fn default_descending(self) -> bool {
        matches!(self, Self::Modified | Self::Size)
    }
}

/// The Files panel's listing order: by `key`, reversed if `descending`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileSort {
    pub key: SortKey,
    pub descending: bool,
}

impl Default for FileSort {
    fn default() -> Self {
        Self {
            key: SortKey::Name,
            descending: false,
        }
    }
}

/// Order `files` as `sort` says. Folders always come first; they have no
/// size or type, so by those they're in name order. Ties go by name, A to Z.
/// Only Modified and Size stat the entries.
pub fn sort_browser_files(files: &mut Vec<BrowserFile>, sort: FileSort) {
    let stat = matches!(sort.key, SortKey::Modified | SortKey::Size);
    let mut keyed: Vec<_> = files
        .drain(..)
        .map(|f| {
            let md = if stat {
                fs::metadata(&f.path).ok()
            } else {
                None
            };
            let modified = md.as_ref().and_then(|m| m.modified().ok());
            let size = md.map(|m| m.len());
            let ext = f
                .path
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            (f.disp_name.to_lowercase(), ext, modified, size, f)
        })
        .collect();

    keyed.sort_by(|a, b| {
        let (name_a, ext_a, mod_a, size_a, fa) = a;
        let (name_b, ext_b, mod_b, size_b, fb) = b;
        let folders = fb.is_folder.cmp(&fa.is_folder);
        let both_files = !fa.is_folder && !fb.is_folder;
        let by = match sort.key {
            SortKey::Name => name_a.cmp(name_b),
            SortKey::Modified => mod_a.cmp(mod_b),
            SortKey::Type if both_files => ext_a.cmp(ext_b),
            SortKey::Size if both_files => size_a.cmp(size_b),
            SortKey::Type | SortKey::Size => std::cmp::Ordering::Equal,
        };
        let by = if sort.descending { by.reverse() } else { by };
        folders.then(by).then_with(|| name_a.cmp(name_b))
    });

    files.extend(keyed.into_iter().map(|k| k.4));
}

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

/// What the details pane shows while several rows are selected, in place of
/// a [FileDetails]. Read by [SelectionSummary::read] when the selection
/// changes, likewise.
pub struct SelectionSummary {
    /// Each one's name, and whether it's a folder, in listing order.
    pub items: Vec<(String, bool)>,
    pub files: usize,
    pub folders: usize,
    /// The files' total size. As in Explorer, folders' contents aren't
    /// counted, since that means walking every one of them.
    pub size: u64,
    /// The oldest and newest of their modification times.
    pub modified: Option<(SystemTime, SystemTime)>,
    /// The dir they're all in, if it's the same one.
    pub dir: Option<PathBuf>,
}

impl SelectionSummary {
    /// Stat each of `paths`. One that's gone since it was listed counts as
    /// a file with no size.
    pub fn read(paths: &[PathBuf]) -> Self {
        let mut s = Self {
            items: Vec::with_capacity(paths.len()),
            files: 0,
            folders: 0,
            size: 0,
            modified: None,
            dir: paths
                .first()
                .and_then(|p| p.parent())
                .map(Path::to_path_buf),
        };
        for path in paths {
            let md = fs::metadata(path).ok();
            let is_folder = md.as_ref().is_some_and(fs::Metadata::is_dir);
            if is_folder {
                s.folders += 1;
            } else {
                s.files += 1;
                s.size += md.as_ref().map_or(0, fs::Metadata::len);
            }
            if let Some(t) = md.and_then(|md| md.modified().ok()) {
                s.modified = Some(s.modified.map_or((t, t), |(lo, hi)| (lo.min(t), hi.max(t))));
            }
            if s.dir.as_deref() != path.parent() {
                s.dir = None;
            }
            let name = path.file_name().unwrap_or(path.as_os_str());
            s.items
                .push((name.to_string_lossy().into_owned(), is_folder));
        }
        s
    }
}

/// Whether `a` and `b` are on the same drive (Windows) or filesystem
/// (elsewhere), where a move is just a rename. A file manager moves what's
/// dragged to a folder on the same one, and copies it to another.
#[cfg(windows)]
pub fn same_volume(a: &Path, b: &Path) -> bool {
    use std::path::Component;
    // The drive letter or UNC share each starts with.
    let prefix = |p: &Path| match p.components().next() {
        Some(Component::Prefix(pre)) => Some(pre.as_os_str().to_string_lossy().to_lowercase()),
        _ => None,
    };
    prefix(a) == prefix(b)
}

#[cfg(unix)]
pub fn same_volume(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev(),
        _ => true,
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

/// Open the image at `path` in Paint. On Windows 11 `mspaint` is an app
/// alias, found on the PATH.
#[cfg(windows)]
pub fn open_in_paint(path: &Path) -> io::Result<()> {
    shell::quiet_command("mspaint")
        .arg(path)
        .spawn()
        .map(|_| ())
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
    let name = valid_name(new_name)?;
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

/// Create an empty folder (`folder`) or file named `name` in `dir`, as
/// `mkdir` or `touch` would, returning its path. The name is trimmed and
/// checked as [rename_entry]'s is. Unlike `touch`, an entry that's already
/// there by that name is an error, rather than having its time updated.
pub fn create_entry(dir: &Path, name: &str, folder: bool) -> io::Result<PathBuf> {
    let name = valid_name(name)?;
    let path = dir.join(name);
    let made = if folder {
        fs::create_dir(&path)
    } else {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map(drop)
    };
    match made {
        // Said plainly, rather than as the OS's "Cannot create a file when
        // that file already exists".
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{name} already exists"),
        )),
        made => made.map(|()| path),
    }
}

/// `name` with surrounding spaces trimmed (and on Windows, trailing dots,
/// which it would drop anyway), if it can name an entry: not blank, `.` or
/// `..`, and without a path separator, which would put it in another dir.
fn valid_name(name: &str) -> io::Result<&str> {
    let mut trimmed = name.trim();
    if cfg!(windows) {
        trimmed = trimmed.trim_end_matches(['.', ' ']);
    }
    let seps: &[char] = if cfg!(windows) { &['/', '\\'] } else { &['/'] };
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." || trimmed.contains(seps) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("\"{name}\" isn't a valid name"),
        ));
    }
    Ok(trimmed)
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
        app_data(home),
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
        glyph: "🗃",
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
