//! The Files panel's Cut / Copy / Paste, through the OS clipboard, so files
//! go to and from Explorer, Finder or another file manager as well as
//! between our own tabs.
//!
//! On Windows a cut is marked the way Explorer marks one, with the
//! "Preferred DropEffect" clipboard format, so a file cut here is moved when
//! pasted in Explorer and vice versa. Elsewhere the clipboard only carries
//! the file list, so we remember what we cut ourselves; files cut in another
//! app are copied.
//!
//! A paste runs on a background thread ([Paste]), so copying a big folder
//! doesn't freeze the window.

use std::{
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
    thread::{self, JoinHandle},
};

/// One copy operation. A group of files remains a single history entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Content {
    Text(String),
    Files { paths: Vec<PathBuf>, cut: bool },
}

/// Bounded, session-only history. Never persisted to disk.
#[derive(Default)]
pub struct History {
    pub entries: Vec<Content>,
}

impl History {
    pub const LIMIT: usize = 100;
    pub const MAX_ENTRY_BYTES: usize = 1024 * 1024;

    pub fn record(&mut self, content: Content) {
        let size = match &content {
            Content::Text(text) => text.len(),
            Content::Files { paths, .. } => paths.iter().map(|p| p.as_os_str().len()).sum(),
        };
        if size == 0 || size > Self::MAX_ENTRY_BYTES {
            return;
        }
        self.entries.retain(|old| old != &content);
        self.entries.insert(0, content);
        self.entries.truncate(Self::LIMIT);
    }

    /// Remove moved paths from every historical file group, including copies.
    /// Failed paths remain available for retry after a partial move.
    pub fn forget_paths(&mut self, paths: &[PathBuf]) {
        for entry in &mut self.entries {
            if let Content::Files { paths: held, .. } = entry {
                held.retain(|p| !paths.contains(p));
            }
        }
        self.entries
            .retain(|entry| !matches!(entry, Content::Files { paths, .. } if paths.is_empty()));
    }
}

/// Capture the actual text egui sends to the OS, including label selections
/// finalized after the app UI returns. Leave backend clipboard delivery intact.
#[derive(Default)]
pub struct TextCapture {
    pub pending: History,
}

impl egui::plugin::Plugin for TextCapture {
    fn debug_name(&self) -> &'static str {
        "Clipboard history"
    }

    fn output_hook(&mut self, ctx: &egui::Context, output: &mut egui::FullOutput) {
        for command in &output.platform_output.commands {
            if let egui::OutputCommand::CopyText(text) = command {
                self.pending.record(Content::Text(text.clone()));
                ctx.request_repaint();
            }
        }
    }
}

/// Put `paths` on the clipboard, to be copied wherever they're pasted, or
/// with `cut`, moved.
pub fn set_files(paths: &[PathBuf], cut: bool) -> Result<(), String> {
    os::set_files(paths, cut)
}

/// The files on the clipboard, and whether they were cut (to be moved)
/// rather than copied. `None` if there are none, e.g. it holds text.
pub fn get_files() -> Result<Option<(Vec<PathBuf>, bool)>, String> {
    os::get_files()
}

/// Empty the clipboard if it still holds exactly `paths`, as Explorer does
/// once files it cut have been moved: they're no longer where it says.
pub fn clear_if_holds(paths: &[PathBuf]) {
    if let Ok(Some((held, _))) = get_files()
        && held == paths
    {
        os::clear();
    }
}

#[cfg(windows)]
mod os {
    use std::path::PathBuf;

    use clipboard_win::{Clipboard, formats::CF_HDROP, raw};

    /// Explorer's flags for a copy (`DROPEFFECT_COPY | DROPEFFECT_LINK`)
    /// and a cut (`DROPEFFECT_MOVE`).
    const EFFECT_COPY: u32 = 5;
    const EFFECT_MOVE: u32 = 2;

    /// The clipboard format holding those flags.
    fn drop_effect_format() -> Option<u32> {
        raw::register_format("Preferred DropEffect").map(|f| f.get())
    }

    /// Other apps may have the clipboard open for a moment, so retry a few
    /// times before giving up.
    fn open() -> Result<Clipboard, String> {
        Clipboard::new_attempts(10).map_err(|e| format!("can't open the clipboard: {e}"))
    }

    pub fn set_files(paths: &[PathBuf], cut: bool) -> Result<(), String> {
        let names: Vec<&str> = paths
            .iter()
            .map(|p| {
                p.to_str()
                    .ok_or_else(|| format!("{} isn't valid Unicode", p.display()))
            })
            .collect::<Result<_, _>>()?;

        let _clip = open()?;
        raw::empty().map_err(|e| e.to_string())?;
        raw::set_file_list(&names).map_err(|e| e.to_string())?;
        if let Some(fmt) = drop_effect_format() {
            let effect = if cut { EFFECT_MOVE } else { EFFECT_COPY };
            raw::set_without_clear(fmt, &effect.to_le_bytes()).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn get_files() -> Result<Option<(Vec<PathBuf>, bool)>, String> {
        let _clip = open()?;
        if !raw::is_format_avail(CF_HDROP) {
            return Ok(None);
        }
        let mut paths = Vec::new();
        raw::get_file_list_path(&mut paths).map_err(|e| e.to_string())?;

        let cut = drop_effect_format()
            .filter(|&fmt| raw::is_format_avail(fmt))
            .and_then(|fmt| {
                let mut buf = Vec::new();
                raw::get_vec(fmt, &mut buf).ok()?;
                Some(u32::from_le_bytes(buf.get(..4)?.try_into().ok()?))
            })
            .is_some_and(|effect| effect & EFFECT_MOVE != 0);

        Ok((!paths.is_empty()).then_some((paths, cut)))
    }

    pub fn clear() {
        if let Ok(_clip) = open() {
            let _ = raw::empty();
        }
    }
}

#[cfg(not(windows))]
mod os {
    use std::{path::PathBuf, sync::Mutex};

    /// Kept open: on X11 the clipboard's contents are served by the app that
    /// set them, for as long as it holds a clipboard handle.
    static CLIPBOARD: Mutex<Option<arboard::Clipboard>> = Mutex::new(None);
    /// What we last cut, so a paste of exactly these files moves them.
    static CUT: Mutex<Option<Vec<PathBuf>>> = Mutex::new(None);

    fn with<T>(
        f: impl FnOnce(&mut arboard::Clipboard) -> Result<T, arboard::Error>,
    ) -> Result<T, String> {
        let mut guard = CLIPBOARD.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            *guard = Some(arboard::Clipboard::new().map_err(|e| e.to_string())?);
        }
        f(guard.as_mut().unwrap()).map_err(|e| e.to_string())
    }

    pub fn set_files(paths: &[PathBuf], cut: bool) -> Result<(), String> {
        with(|c| c.set().file_list(paths))?;
        *CUT.lock().unwrap_or_else(|e| e.into_inner()) = cut.then(|| paths.to_vec());
        Ok(())
    }

    pub fn get_files() -> Result<Option<(Vec<PathBuf>, bool)>, String> {
        let paths = match with(|c| c.get().file_list()) {
            Ok(p) if !p.is_empty() => p,
            // arboard reports "no file list" as an error too.
            _ => return Ok(None),
        };
        let cut = CUT
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|c| *c == paths);
        Ok(Some((paths, cut)))
    }

    pub fn clear() {
        let _ = with(|c| c.clear());
        *CUT.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// A paste started by [Paste::start], copying or moving files into a dir on
/// a background thread. [Paste::try_finish] collects the outcome.
pub struct Paste {
    /// What's being pasted.
    pub srcs: Vec<PathBuf>,
    /// Moving (they were cut) rather than copying.
    pub moving: bool,
    /// Returns each source's new path, or why it couldn't be pasted.
    handle: Option<JoinHandle<Vec<Result<PathBuf, String>>>>,
}

impl Paste {
    /// Start copying (or with `moving`, moving) `srcs` into `dest`. `ctx` is
    /// repainted when it's done.
    pub fn start(srcs: Vec<PathBuf>, dest: PathBuf, moving: bool, ctx: egui::Context) -> Self {
        let srcs_ = srcs.clone();
        let handle = thread::spawn(move || {
            let results = srcs_
                .iter()
                .map(|src| paste_one(src, &dest, moving))
                .collect();
            ctx.request_repaint();
            results
        });
        Self {
            srcs,
            moving,
            handle: Some(handle),
        }
    }

    /// The outcome for each source, in order, once the paste has finished.
    pub fn try_finish(&mut self) -> Option<Vec<Result<PathBuf, String>>> {
        if !self.handle.as_ref()?.is_finished() {
            return None;
        }
        let results = self.handle.take()?.join();
        Some(results.unwrap_or_else(|_| vec![Err("the paste crashed".to_owned())]))
    }
}

/// Copy or move `src` into `dest_dir`, returning its new path. A copy that
/// would clash with an existing name gets Explorer's "name - Copy.ext" (then
/// "- Copy (2)", ...), so pasting back into the same folder duplicates it. A
/// move never replaces anything: a clash is an error. Moving something into
/// the folder it's already in does nothing.
fn paste_one(src: &Path, dest_dir: &Path, moving: bool) -> Result<PathBuf, String> {
    let Some(name) = src.file_name() else {
        return Err(format!(
            "can't paste {}: it's a filesystem root",
            src.display()
        ));
    };
    let Ok(md) = fs::symlink_metadata(src) else {
        return Err(format!("{} no longer exists", src.display()));
    };
    if md.is_dir() && dest_dir.starts_with(src) {
        return Err(format!(
            "can't {} {} into itself",
            if moving { "move" } else { "copy" },
            src.display()
        ));
    }

    let err = |e: io::Error| format!("{}: {e}", src.display());

    if !moving {
        let target = free_name(dest_dir, name, md.is_dir());
        copy_entry(src, &target).map_err(err)?;
        return Ok(target);
    }

    if src.parent() == Some(dest_dir) {
        return Ok(src.to_path_buf());
    }
    let target = dest_dir.join(name);
    if fs::symlink_metadata(&target).is_ok() {
        return Err(format!(
            "can't move {}: {} already exists",
            src.display(),
            target.display()
        ));
    }
    match fs::rename(src, &target) {
        Ok(()) => {}
        // To another drive: copy, then delete the original.
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            copy_entry(src, &target).map_err(err)?;
            remove_entry(src, &md).map_err(err)?;
        }
        Err(e) => return Err(err(e)),
    }
    Ok(target)
}

/// `dir/name` if nothing's there, otherwise the first free "stem - Copy.ext",
/// "stem - Copy (2).ext", ... Folders keep any dots in their name whole.
fn free_name(dir: &Path, name: &OsStr, is_folder: bool) -> PathBuf {
    let plain = dir.join(name);
    if fs::symlink_metadata(&plain).is_err() {
        return plain;
    }
    let path = Path::new(name);
    let (stem, ext) = match (is_folder, path.file_stem(), path.extension()) {
        (false, Some(stem), Some(ext)) => (stem, format!(".{}", ext.to_string_lossy())),
        _ => (name, String::new()),
    };
    let stem = stem.to_string_lossy();
    (1..)
        .map(|n| {
            let copy = if n == 1 {
                " - Copy".to_owned()
            } else {
                format!(" - Copy ({n})")
            };
            dir.join(format!("{stem}{copy}{ext}"))
        })
        .find(|p| fs::symlink_metadata(p).is_err())
        .unwrap()
}

/// Copy a file, or a folder and everything in it, to `dst`. Links inside are
/// copied as links, not followed (see [copy_link]).
fn copy_entry(src: &Path, dst: &Path) -> io::Result<()> {
    let md = fs::symlink_metadata(src)?;
    if md.is_dir() {
        fs::create_dir(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_entry(&entry.path(), &dst.join(entry.file_name()))?;
        }
        Ok(())
    } else if md.is_symlink() {
        copy_link(src, dst)
    } else {
        fs::copy(src, dst).map(|_| ())
    }
}

#[cfg(unix)]
fn copy_link(src: &Path, dst: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(fs::read_link(src)?, dst)
}

/// A link to a file is copied as the file it points to, as Explorer does,
/// since making a link needs Developer Mode or admin rights. A link to a
/// folder (or a junction) is remade as a link, if that's allowed, rather
/// than copying everything it leads to.
#[cfg(windows)]
fn copy_link(src: &Path, dst: &Path) -> io::Result<()> {
    if fs::metadata(src)?.is_dir() {
        std::os::windows::fs::symlink_dir(fs::read_link(src)?, dst)
    } else {
        fs::copy(src, dst).map(|_| ())
    }
}

/// Delete what a cross-drive move has copied. `md` is its
/// `symlink_metadata`, so a link is removed, not what it points to.
fn remove_entry(path: &Path, md: &fs::Metadata) -> io::Result<()> {
    if md.is_dir() {
        fs::remove_dir_all(path)
    } else {
        // A link to a folder on Windows is removed as a folder.
        fs::remove_file(path).or_else(|_| fs::remove_dir(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_is_deduplicated_bounded_and_newest_first() {
        let mut history = History::default();
        for n in 0..105 {
            history.record(Content::Text(n.to_string()));
        }
        assert_eq!(history.entries.len(), History::LIMIT);
        history.record(Content::Text("80".into()));
        assert_eq!(history.entries[0], Content::Text("80".into()));
        assert_eq!(history.entries.len(), History::LIMIT);
        history.record(Content::Text(String::new()));
        history.record(Content::Text("x".repeat(History::MAX_ENTRY_BYTES + 1)));
        assert_eq!(history.entries.len(), History::LIMIT);
        assert_eq!(history.entries[0], Content::Text("80".into()));
    }

    #[test]
    fn partial_move_removes_only_successful_paths_from_all_groups() {
        let mut history = History::default();
        let a = PathBuf::from("a");
        let b = PathBuf::from("b");
        history.record(Content::Files {
            paths: vec![a.clone()],
            cut: false,
        });
        history.record(Content::Files {
            paths: vec![a.clone(), b.clone()],
            cut: true,
        });
        history.record(Content::Text("keep".into()));
        history.forget_paths(&[a]);
        assert_eq!(
            history.entries,
            vec![
                Content::Text("keep".into()),
                Content::Files {
                    paths: vec![b],
                    cut: true
                },
            ]
        );
    }

    #[test]
    fn text_capture_preserves_backend_delivery_and_order() {
        use egui::plugin::Plugin;
        let mut capture = TextCapture::default();
        let mut output = egui::FullOutput::default();
        output.platform_output.commands = vec![
            egui::OutputCommand::CopyText("first".into()),
            egui::OutputCommand::CopyText("second".into()),
        ];
        capture.output_hook(&egui::Context::default(), &mut output);
        assert_eq!(output.platform_output.commands.len(), 2);
        assert_eq!(
            capture.pending.entries,
            vec![
                Content::Text("second".into()),
                Content::Text("first".into())
            ]
        );
    }

    /// A fresh, empty dir for one test, under the system temp dir.
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("peregrine_paste_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn copy_into_same_dir_numbers_the_copies() {
        let dir = scratch("same_dir");
        let src = dir.join("notes.txt");
        fs::write(&src, "hi").unwrap();

        let first = paste_one(&src, &dir, false).unwrap();
        let second = paste_one(&src, &dir, false).unwrap();
        assert_eq!(first, dir.join("notes - Copy.txt"));
        assert_eq!(second, dir.join("notes - Copy (2).txt"));
        assert_eq!(fs::read_to_string(&second).unwrap(), "hi");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn copies_folders_whole_keeping_dotted_names() {
        let dir = scratch("folder");
        let src = dir.join("my.folder");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("sub").join("a.txt"), "a").unwrap();

        let copy = paste_one(&src, &dir, false).unwrap();
        assert_eq!(copy, dir.join("my.folder - Copy"));
        assert_eq!(
            fs::read_to_string(copy.join("sub").join("a.txt")).unwrap(),
            "a"
        );
        assert!(paste_one(&src, &src.join("sub"), false).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn moves_without_replacing() {
        let dir = scratch("move");
        let (a, b) = (dir.join("a"), dir.join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("f.txt"), "1").unwrap();
        fs::write(a.join("g.txt"), "2").unwrap();
        fs::write(b.join("g.txt"), "other").unwrap();

        assert_eq!(
            paste_one(&a.join("f.txt"), &b, true).unwrap(),
            b.join("f.txt")
        );
        assert!(!a.join("f.txt").exists());
        assert!(paste_one(&a.join("g.txt"), &b, true).is_err());
        assert_eq!(fs::read_to_string(b.join("g.txt")).unwrap(), "other");
        assert!(a.join("g.txt").exists());
        // Into the folder it's already in: nothing happens.
        assert_eq!(
            paste_one(&a.join("g.txt"), &a, true).unwrap(),
            a.join("g.txt")
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
