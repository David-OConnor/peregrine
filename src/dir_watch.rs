//! Notices the Files panel's dir changing on disk, by polling on a thread.
//!
//! Not OS change notifications: those hold the dir open, which on Windows
//! blocks ejecting its drive and renaming its parents, and many network and
//! FUSE mounts never send them. The UI is only woken on a change, so an idle
//! window still draws nothing.

use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use egui::Context;

const MIN_WAIT: Duration = Duration::from_secs(1);
const MAX_WAIT: Duration = Duration::from_secs(30);
/// Waits are this many times the last scan's length, so a huge or slow dir
/// costs at most about 5% of a core.
const WAIT_PER_SCAN: u32 = 20;

#[derive(Default)]
pub struct DirWatch {
    dir: Option<PathBuf>,
    /// To the polling thread, started by the first [Self::watch].
    targets: Option<Sender<Option<PathBuf>>>,
    changed: Arc<AtomicBool>,
}

impl DirWatch {
    /// Poll `dir` from now on, or with `None`, nothing. Cheap unless `dir`
    /// differs from last time, so it's fine to call every frame.
    pub fn watch(&mut self, dir: Option<&Path>, ctx: &Context) {
        if self.dir.as_deref() == dir {
            return;
        }

        self.dir = dir.map(Path::to_path_buf);

        let targets = self.targets.get_or_insert_with(|| {
            let (tx, rx) = mpsc::channel();
            let changed = Arc::clone(&self.changed);
            let ctx = ctx.clone();
            thread::spawn(move || poll(&rx, &changed, &ctx));

            tx
        });

        let _ = targets.send(self.dir.clone());
    }

    /// Whether the dir has changed since the last [Self::clear].
    pub fn changed(&self) -> bool {
        self.changed.load(Ordering::Relaxed)
    }

    /// Call before re-reading the dir, so a change during the read counts.
    pub fn clear(&self) {
        self.changed.store(false, Ordering::Relaxed);
    }
}

/// Set `changed` and wake the UI whenever the dir last sent on `targets`
/// scans differently. Ends once the sender's dropped.
fn poll(targets: &Receiver<Option<PathBuf>>, changed: &AtomicBool, ctx: &Context) {
    let mut dir: Option<PathBuf> = None;
    let mut wait = MIN_WAIT;
    // `None` for a new dir, so its first scan counts as a change: the listing
    // was read before it, and anything between the two would go unseen.
    let mut last = None;

    loop {
        let next = match dir {
            Some(_) => targets.recv_timeout(wait),
            None => targets.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };

        match next {
            Ok(new) => {
                dir = targets.try_iter().last().unwrap_or(new);
                last = None;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }

        let Some(path) = &dir else {
            continue;
        };

        let start = Instant::now();
        let print = fingerprint(path);
        wait = (start.elapsed() * WAIT_PER_SCAN).clamp(MIN_WAIT, MAX_WAIT);

        if print.is_some() && print != last {
            last = print;
            changed.store(true, Ordering::Relaxed);
            ctx.request_repaint();
        }
    }
}

/// A hash of what the listing and its details pane show of `dir`'s entries.
/// `None` if it can't be read for now. Every entry's metadata comes with the
/// listing on Windows, so this only costs a stat each elsewhere.
fn fingerprint(dir: &Path) -> Option<u64> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        // Deleted: reads as empty, as the listing will.
        Err(e) if e.kind() == ErrorKind::NotFound => return Some(0),
        // Maybe a network blip. Keep what's shown.
        Err(_) => return None,
    };

    let mut print = 0u64;
    for entry in entries {
        let entry = entry.ok()?;
        let mut hasher = DefaultHasher::new();

        entry.file_name().hash(&mut hasher);
        if let Ok(md) = entry.metadata() {
            md.is_dir().hash(&mut hasher);
            md.len().hash(&mut hasher);
            md.modified().ok().hash(&mut hasher);
            attributes(&md).hash(&mut hasher);
        }

        // Summed, since `read_dir`'s order isn't specified.
        print = print.wrapping_add(hasher.finish());
    }

    Some(print)
}

#[cfg(windows)]
fn attributes(md: &fs::Metadata) -> u32 {
    use std::os::windows::fs::MetadataExt;

    md.file_attributes()
}

#[cfg(unix)]
fn attributes(md: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    md.permissions().mode()
}
