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
//!
//! [SysClipboard] tracks what's on the OS clipboard, so the history can mark
//! the entry that a plain paste would give, and take in what other apps copy.

use std::{
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    thread::{self, JoinHandle},
    time::Duration,
};

use chrono::Utc;
/// One copy operation, and a history entry: it, and when it was last put on
/// the clipboard. Copying it again moves the entry to the top with a new
/// time. They live in `shell` so the state file can hold the history.
pub use shell::{ClipboardContent as Content, ClipboardEntry as Entry};

/// Bounded history, newest first. Saved in the state file (see
/// [crate::state::State::save]), so it carries over between runs.
#[derive(Default)]
pub struct History {
    pub entries: Vec<Entry>,
    /// Set when `entries` changes, until `ui::draw` saves it.
    pub unsaved: bool,
}

impl History {
    pub const LIMIT: usize = 100;
    pub const MAX_ENTRY_BYTES: usize = 1024 * 1024;

    /// The history as the state file had it, within the limits.
    pub fn from_saved(mut entries: Vec<Entry>) -> Self {
        entries.retain(|e| Self::fits(&e.content));
        entries.truncate(Self::LIMIT);
        Self {
            entries,
            unsaved: false,
        }
    }

    /// Whether `content` is small enough to record, and not empty.
    pub fn fits(content: &Content) -> bool {
        let size = match content {
            Content::Text(text) => text.len(),
            Content::Files { paths, .. } => paths.iter().map(|p| p.as_os_str().len()).sum(),
        };
        size != 0 && size <= Self::MAX_ENTRY_BYTES
    }

    pub fn record(&mut self, content: Content) {
        if !Self::fits(&content) {
            return;
        }
        self.entries.retain(|old| old.content != content);
        self.entries.insert(
            0,
            Entry {
                content,
                added: Utc::now(),
            },
        );
        self.entries.truncate(Self::LIMIT);
        self.unsaved = true;
    }

    pub fn remove(&mut self, index: usize) {
        self.entries.remove(index);
        self.unsaved = true;
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.unsaved = true;
    }

    /// Every entry's content, newest first.
    #[cfg(test)]
    pub fn contents(&self) -> Vec<Content> {
        self.entries.iter().map(|e| e.content.clone()).collect()
    }

    /// Remove moved paths from every historical file group, including copies.
    /// Failed paths remain available for retry after a partial move.
    pub fn forget_paths(&mut self, paths: &[PathBuf]) {
        if paths.is_empty() {
            return;
        }
        for entry in &mut self.entries {
            if let Content::Files { paths: held, .. } = &mut entry.content {
                held.retain(|p| !paths.contains(p));
            }
        }
        self.entries.retain(
            |entry| !matches!(&entry.content, Content::Files { paths, .. } if paths.is_empty()),
        );
        self.unsaved = true;
    }
}

/// What's on the OS clipboard, as far as we know. Peregrine's own copies and
/// pastes [set](Self::set) it directly; [watch](Self::watch) starts a
/// background thread that picks up what other apps copy, for the history.
#[derive(Default)]
pub struct SysClipboard {
    seen: Arc<Mutex<Seen>>,
    watching: bool,
}

#[derive(Default)]
struct Seen {
    /// `None` when it holds nothing we'd record, e.g. an image, text too big
    /// for the history, or something a password manager has marked private.
    current: Option<Content>,
    /// What the watcher found there that Peregrine didn't put there, oldest
    /// first, waiting to be added to the history. Several can pile up while
    /// the window is minimized and not drawing.
    found: Vec<Content>,
}

impl Seen {
    /// Take in what the watcher just read from the clipboard. Returns whether
    /// it's news: the same as Peregrine put there itself isn't.
    fn observe(&mut self, content: Option<Content>) -> bool {
        if self.current == content {
            return false;
        }
        self.current = content.clone();
        if let Some(content) = content {
            self.found.push(content);
            if self.found.len() > History::LIMIT {
                self.found.remove(0);
            }
        }
        true
    }
}

impl SysClipboard {
    /// How often the watcher looks for changes. Where the OS numbers the
    /// clipboard's contents (Windows), that's a check of the number, which
    /// costs next to nothing, and the clipboard is only read when it has
    /// changed. Elsewhere the clipboard is read each time, so less often.
    const POLL_NUMBERED: Duration = Duration::from_secs(1);
    const POLL_READ: Duration = Duration::from_secs(30);

    fn seen(&self) -> std::sync::MutexGuard<'_, Seen> {
        self.seen.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Note that `content` was just put on the OS clipboard.
    pub fn set(&self, content: Content) {
        self.seen().current = Some(content);
    }

    /// Whether we know the OS clipboard holds `content`.
    pub fn holds(&self, content: &Content) -> bool {
        self.seen().current.as_ref() == Some(content)
    }

    /// What other apps have copied since the last call, oldest first.
    pub fn take_found(&self) -> Vec<Content> {
        std::mem::take(&mut self.seen().found)
    }

    /// As if the watcher had just read `content` from the clipboard.
    #[cfg(test)]
    pub fn observe(&self, content: Option<Content>) -> bool {
        self.seen().observe(content)
    }

    /// The index of the `history` entry the OS clipboard holds, if any.
    pub fn find_in(&self, history: &History) -> Option<usize> {
        let seen = self.seen();
        let current = seen.current.as_ref()?;
        history.entries.iter().position(|e| e.content == *current)
    }

    /// Start the watcher thread, if it isn't running yet. It repaints `ctx`
    /// when what's on the clipboard changes, and ends once `self` is dropped.
    pub fn watch(&mut self, ctx: &egui::Context) {
        if std::mem::replace(&mut self.watching, true) {
            return;
        }
        let seen = Arc::downgrade(&self.seen);
        let ctx = ctx.clone();
        let spawned = thread::Builder::new()
            .name("clipboard watch".to_owned())
            .spawn(move || watch(&seen, &ctx));
        if spawned.is_err() {
            self.watching = false;
        }
    }
}

fn watch(seen: &Weak<Mutex<Seen>>, ctx: &egui::Context) {
    let mut seq_seen = None;
    loop {
        let seq = os::seq_num();
        if seq.is_none() || seq != seq_seen {
            // Unreadable (another app has it open): try again next time.
            if let Ok(content) = os::get() {
                seq_seen = seq;
                // Line endings as egui gives a pasted text, which is how it
                // was recorded.
                let content = content
                    .map(|c| match c {
                        Content::Text(text) if text.contains('\r') => {
                            Content::Text(text.replace("\r\n", "\n"))
                        }
                        c => c,
                    })
                    .filter(History::fits);
                let Some(seen) = seen.upgrade() else {
                    return;
                };
                if seen
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .observe(content)
                {
                    ctx.request_repaint();
                }
            }
        }
        if seen.strong_count() == 0 {
            return;
        }
        thread::sleep(if seq.is_some() {
            SysClipboard::POLL_NUMBERED
        } else {
            SysClipboard::POLL_READ
        });
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

    use clipboard_win::{
        Clipboard,
        formats::{CF_HDROP, CF_UNICODETEXT},
        raw,
    };

    use super::{Content, History};

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
        read_files()
    }

    /// [get_files], with the clipboard already open.
    fn read_files() -> Result<Option<(Vec<PathBuf>, bool)>, String> {
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

    /// Changes whenever the clipboard's contents do. Reading it doesn't open
    /// the clipboard, so it's cheap enough to poll.
    pub fn seq_num() -> Option<u32> {
        raw::seq_num().map(|n| n.get())
    }

    /// Whether the app that set the clipboard asked clipboard monitors and
    /// histories to leave it alone, as password managers do: either format
    /// present, or the second one holding 0.
    fn is_private() -> bool {
        let format = |name| raw::register_format(name).map(|f| f.get());
        if format("ExcludeClipboardContentFromMonitorProcessing").is_some_and(raw::is_format_avail)
        {
            return true;
        }
        format("CanIncludeInClipboardHistory")
            .filter(|&f| raw::is_format_avail(f))
            .is_some_and(|f| {
                let mut buf = Vec::new();
                raw::get_vec(f, &mut buf).is_ok() && buf.get(..4) == Some(&0u32.to_le_bytes()[..])
            })
    }

    /// The clipboard's files, else its text. `None` if it holds neither, or
    /// if it's [private](is_private). Only failing to open the clipboard is
    /// an error.
    pub fn get() -> Result<Option<Content>, String> {
        let _clip = open()?;
        if is_private() {
            return Ok(None);
        }
        if let Ok(Some((paths, cut))) = read_files() {
            return Ok(Some(Content::Files { paths, cut }));
        }
        if !raw::is_format_avail(CF_UNICODETEXT) {
            return Ok(None);
        }
        // It's UTF-16, so at most twice the size it'll be as UTF-8. Skip
        // reading text too big for the history (plus a little for the
        // closing NUL and allocation rounding).
        let limit = 2 * History::MAX_ENTRY_BYTES + 1024;
        if raw::size(CF_UNICODETEXT).is_some_and(|n| n.get() > limit) {
            return Ok(None);
        }
        let mut buf = Vec::new();
        if raw::get_string(&mut buf).is_err() {
            return Ok(None);
        }
        Ok(String::from_utf8(buf).ok().map(Content::Text))
    }
}

#[cfg(not(windows))]
mod os {
    use std::{path::PathBuf, sync::Mutex};

    use super::Content;

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

    /// There's no cheap way to tell that the clipboard has changed, so the
    /// watcher reads it every time.
    pub fn seq_num() -> Option<u32> {
        None
    }

    /// The clipboard's files, else its text, else `None`.
    pub fn get() -> Result<Option<Content>, String> {
        if let Some((paths, cut)) = get_files()? {
            return Ok(Some(Content::Files { paths, cut }));
        }
        Ok(with(|c| c.get_text()).ok().map(Content::Text))
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
