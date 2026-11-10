//! Space used by Steam games and cargo target folders. Scanned only on request.

use std::{
    cmp::Reverse,
    fs,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
};

use shell::TargetDir;

/// A scan run on a background thread when asked; never refreshed on its own.
pub struct Scan<T> {
    pub result: Option<Result<T, String>>,
    job: Option<mpsc::Receiver<Result<T, String>>>,
}

impl<T> Default for Scan<T> {
    fn default() -> Self {
        Self {
            result: None,
            job: None,
        }
    }
}

impl<T: Send + 'static> Scan<T> {
    pub fn start(
        &mut self,
        work: impl FnOnce() -> Result<T, String> + Send + 'static,
        wake: impl Fn() + Send + 'static,
    ) {
        let (send, receive) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("space-scan".into())
            .spawn(move || {
                let _ = send.send(work());
                wake();
            });

        match spawned {
            Ok(_) => self.job = Some(receive),
            Err(e) => self.result = Some(Err(format!("Could not start scan: {e}"))),
        }
    }

    /// Collects a finished result. True while the scan runs.
    pub fn running(&mut self) -> bool {
        let Some(job) = &self.job else {
            return false;
        };

        match job.try_recv() {
            Ok(result) => self.result = Some(result),
            Err(mpsc::TryRecvError::Disconnected) => self.result = Some(Err("Scan failed".into())),
            Err(mpsc::TryRecvError::Empty) => return true,
        }
        self.job = None;

        false
    }
}

pub struct SteamLibrary {
    pub path: PathBuf,
    /// Of its `steamapps` folder: games, workshop content, shader caches.
    pub size: u64,
}

/// Each Steam library's size.
pub fn steam() -> Result<Vec<SteamLibrary>, String> {
    let root = steam_root().ok_or("Steam not found")?;

    let mut libraries = library_folders(&root);
    if libraries.is_empty() {
        libraries.push(root);
    }

    Ok(in_parallel(libraries, |path| SteamLibrary {
        size: shell::dir_size(&path.join("steamapps")),
        path,
    }))
}

/// Libraries listed in Steam's `libraryfolders.vdf`, the install folder's among them.
fn library_folders(root: &Path) -> Vec<PathBuf> {
    let Ok(text) = fs::read_to_string(root.join("steamapps").join("libraryfolders.vdf")) else {
        return Vec::new();
    };

    // Lines like `"path"		"D:\\SteamLibrary"`.
    let mut libraries: Vec<PathBuf> = text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('"').skip(1).step_by(2);
            if fields.next()? != "path" {
                return None;
            }

            Some(PathBuf::from(fields.next()?.replace(r"\\", r"\")))
        })
        .collect();
    libraries.dedup();

    libraries
}

#[cfg(windows)]
fn steam_root() -> Option<PathBuf> {
    super::platform::steam_root()
}

#[cfg(not(windows))]
fn steam_root() -> Option<PathBuf> {
    let home = shell::get_home()?;
    let candidates: &[&str] = if cfg!(target_os = "macos") {
        &["Library/Application Support/Steam"]
    } else {
        &[
            ".local/share/Steam",
            ".steam/steam",
            ".var/app/com.valvesoftware.Steam/.local/share/Steam",
        ]
    };

    candidates
        .iter()
        .map(|c| home.join(c))
        .find(|p| p.join("steamapps").is_dir())
}

pub struct RootTargets {
    pub root: PathBuf,
    /// Biggest first.
    pub targets: Vec<TargetDir>,
    pub size: u64,
}

/// Cargo target folders under each root, as `rm_targets` finds them.
pub fn rust_targets(roots: Vec<PathBuf>) -> Vec<RootTargets> {
    in_parallel(roots, |root| {
        let mut targets = shell::find_targets(&root);
        targets.sort_by_key(|t| Reverse(t.size));
        let size = targets.iter().map(|t| t.size).sum();

        RootTargets {
            root,
            targets,
            size,
        }
    })
}

/// `f` on each item, a thread each, in item order. Libraries and roots are
/// often on different drives.
fn in_parallel<T: Send, R: Send>(items: Vec<T>, f: impl Fn(T) -> R + Sync) -> Vec<R> {
    let f = &f;

    thread::scope(|scope| {
        let handles: Vec<_> = items
            .into_iter()
            .map(|item| scope.spawn(move || f(item)))
            .collect();

        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    })
}
