//! WSL tabs, on Windows: a tab whose dir is in a WSL distro, under
//! `\\wsl.localhost\<distro>\`, or one that went into a Windows drive from
//! there, as `/mnt/c`. The Files panel reads the distro through that share,
//! which doesn't reach `/mnt/c`, so that's read as `C:\` (see [fs_path]).
//! Commands run in the distro through `wsl.exe` (see [command_args]), and
//! `cd` takes Linux paths (see [resolve]). Elsewhere, no path is a WSL one.

use std::{
    fs,
    path::{Component, Path, PathBuf, Prefix},
    sync::Mutex,
};

/// The host Windows serves distros' files from, and its older name.
const HOSTS: [&str; 2] = ["wsl.localhost", "wsl$"];

/// Where WSL mounts the Windows drives.
const MOUNTS: &str = "/mnt/";

/// An installed distro.
pub struct Distro {
    pub name: String,
    /// Its default user's ID.
    uid: u32,
}

/// The installed distros, the default first. Read from the registry, so
/// cheap; nothing is started.
pub fn distros() -> Vec<Distro> {
    sys::distros()
}

/// A WSL tab's distro, and its dir there, e.g. `/home/me` or `/mnt/c/Users`.
#[derive(Clone, Debug, PartialEq)]
pub struct Here {
    pub distro: String,
    pub linux: String,
}

impl Here {
    /// For a tab at `cwd`: in WSL when that's in a distro, or when it's a
    /// Windows drive's dir and the tab is in `distro`.
    pub fn of(cwd: &Path, distro: Option<&str>) -> Option<Self> {
        if let Some((distro, linux)) = split(cwd) {
            return Some(Self { distro, linux });
        }

        Some(Self {
            distro: distro?.to_owned(),
            linux: mount_path(cwd)?,
        })
    }

    /// As shown: `Ubuntu:~/code`, or `Ubuntu:/mnt/c/Users`.
    pub fn display(&self) -> String {
        format!("{}:{}", self.distro, tilde(&self.distro, &self.linux))
    }
}

/// `path`'s distro, and its path in there, e.g. `("Ubuntu", "/home/me")`.
pub fn split(path: &Path) -> Option<(String, String)> {
    // Cheap enough for every path drawn.
    if !cfg!(windows) || !path.as_os_str().as_encoded_bytes().starts_with(br"\\") {
        return None;
    }

    let mut comps = path.components();
    let Some(Component::Prefix(prefix)) = comps.next() else {
        return None;
    };
    let (Prefix::UNC(host, distro) | Prefix::VerbatimUNC(host, distro)) = prefix.kind() else {
        return None;
    };
    if !HOSTS.iter().any(|h| host.eq_ignore_ascii_case(h)) {
        return None;
    }

    let mut names = Vec::new();
    for c in comps {
        match c {
            Component::Normal(name) => names.push(name.to_string_lossy()),
            Component::ParentDir => {
                names.pop();
            }
            _ => {}
        }
    }

    Some((
        distro.to_string_lossy().into_owned(),
        format!("/{}", names.join("/")),
    ))
}

/// `C:\Users` as WSL mounts it: `/mnt/c/Users`.
fn mount_path(path: &Path) -> Option<String> {
    let mut comps = path.components();
    let Some(Component::Prefix(prefix)) = comps.next() else {
        return None;
    };
    let (Prefix::Disk(letter) | Prefix::VerbatimDisk(letter)) = prefix.kind() else {
        return None;
    };

    let mut linux = format!("{MOUNTS}{}", char::from(letter.to_ascii_lowercase()));
    for c in comps {
        if let Component::Normal(name) = c {
            linux.push('/');
            linux.push_str(&name.to_string_lossy());
        }
    }

    Some(linux)
}

/// `linux` in `distro`, as Windows names it.
pub fn unc(distro: &str, linux: &str) -> PathBuf {
    let mut path = PathBuf::from(format!(r"\\{}\{distro}\", HOSTS[0]));
    path.push(linux.trim_start_matches('/').replace('/', r"\"));

    path
}

/// Where Windows reaches `path`: a distro's `/mnt/c/x` is `C:\x`, which its
/// share won't serve. Anything else as it is.
pub fn fs_path(path: &Path) -> PathBuf {
    let drive = split(path).and_then(|(_, linux)| {
        let rest = linux.strip_prefix(MOUNTS)?;
        let (letter, rest) = rest.split_at_checked(1)?;
        let letter = letter.chars().next().filter(char::is_ascii_alphabetic)?;
        if !(rest.is_empty() || rest.starts_with('/')) {
            return None;
        }

        let rest = rest.trim_start_matches('/').replace('/', r"\");
        Some(PathBuf::from(format!(
            r"{}:\{rest}",
            letter.to_ascii_uppercase()
        )))
    });

    drive.unwrap_or_else(|| path.to_path_buf())
}

/// Each distro's home, once looked up; `None` where that failed.
static HOMES: Mutex<Vec<(String, Option<String>)>> = Mutex::new(Vec::new());

/// [home], if looked up already.
fn cached(distro: &str) -> Option<Option<String>> {
    let homes = HOMES.lock().ok()?;
    homes
        .iter()
        .find(|(d, _)| d.eq_ignore_ascii_case(distro))
        .map(|(_, h)| h.clone())
}

/// `distro`'s default user's home, e.g. `/home/me`. Read once, from its
/// `/etc/passwd`, which starts the distro if it isn't running.
pub fn home(distro: &str) -> Option<String> {
    cached(distro).unwrap_or_else(|| look_up_home(distro))
}

/// [home], read afresh, e.g. for a distro that wouldn't start before.
pub fn look_up_home(distro: &str) -> Option<String> {
    let found = distros()
        .into_iter()
        .find(|d| d.name.eq_ignore_ascii_case(distro))
        .and_then(|d| {
            let passwd = fs::read_to_string(unc(&d.name, "/etc/passwd")).ok()?;
            passwd.lines().find_map(|line| {
                let fields: Vec<&str> = line.split(':').collect();
                (fields.len() > 5 && fields[2].parse() == Ok(d.uid)).then(|| fields[5].to_owned())
            })
        });

    if let Ok(mut homes) = HOMES.lock() {
        homes.retain(|(d, _)| !d.eq_ignore_ascii_case(distro));
        homes.push((distro.to_owned(), found.clone()));
    }

    found
}

/// [home], only if it's been looked up: never touches the distro, so it's
/// for drawing.
pub fn known_home(distro: &str) -> Option<String> {
    cached(distro).flatten().filter(|h| h.len() > 1)
}

/// `\\wsl.localhost\<distro>\`.
pub fn unc_root(distro: &str) -> PathBuf {
    unc(distro, "/")
}

/// The home of the distro a WSL tab is in, as Windows names it; its root
/// when there's no home. May start the distro.
pub fn home_dir(here: &Here) -> PathBuf {
    unc(&here.distro, &home(&here.distro).unwrap_or_default())
}

/// [known_home], as Windows names it, spelled as `path` is.
pub fn known_home_dir(path: &Path) -> Option<PathBuf> {
    let (distro, _) = split(path)?;
    let root = path.ancestors().last()?;
    let home = known_home(&distro)?;

    Some(root.join(home.trim_start_matches('/').replace('/', r"\")))
}

/// `linux`, with `~` for `distro`'s home once that's known.
fn tilde(distro: &str, linux: &str) -> String {
    let shown = known_home(distro).and_then(|h| {
        let rest = linux.strip_prefix(&h)?;
        (rest.is_empty() || rest.starts_with('/')).then(|| format!("~{rest}"))
    });

    shown.unwrap_or_else(|| linux.to_owned())
}

/// `path` as shown: `Ubuntu:~/code`, or `Ubuntu:/etc`. `None` outside WSL.
pub fn display(path: &Path) -> Option<String> {
    let (distro, linux) = split(path)?;

    Some(format!("{distro}:{}", tilde(&distro, &linux)))
}

/// Whether `arg` is a Windows path from the root: `C:\x`, `C:/x`, `\\host`.
pub fn windows_absolute(arg: &str) -> bool {
    let b = arg.as_bytes();

    arg.starts_with(r"\\") || (b.len() > 1 && b[0].is_ascii_alphabetic() && b[1] == b':')
}

/// `linux`, without `.`, `..` or doubled `/`s. `\` counts as `/`, since
/// that's what's typed on Windows.
fn normalize(linux: &str) -> String {
    let mut names: Vec<&str> = Vec::new();
    for name in linux.split(['/', '\\']) {
        match name {
            "" | "." => {}
            ".." => {
                names.pop();
            }
            _ => names.push(name),
        }
    }

    format!("/{}", names.join("/"))
}

/// `~` or `~/x` in `distro`.
fn expand_tilde(distro: &str, arg: &str) -> Option<String> {
    let rest = arg.strip_prefix('~')?;
    if !(rest.is_empty() || rest.starts_with(['/', '\\'])) {
        return None;
    }

    Some(format!("{}/{rest}", home(distro).unwrap_or_default()))
}

/// Where a `cd`, `view` or path bar path goes when it's a Linux one, as
/// Windows names it (see [fs_path] for where to read it). In a WSL tab
/// `here`, any but a Windows one: `code`, `..`, `/etc`, `~/x`. From
/// anywhere, `Ubuntu:/etc`, `Ubuntu:~/code`, or `Ubuntu:` for its home.
pub fn resolve(here: Option<&Here>, arg: &str) -> Option<PathBuf> {
    if !cfg!(windows) || arg.is_empty() {
        return None;
    }

    // Longer than a drive letter.
    if let Some((name, rest)) = arg.split_once(':')
        && name.len() > 1
        && (rest.is_empty() || rest.starts_with(['/', '~']))
    {
        let distro = distros()
            .into_iter()
            .find(|d| d.name.eq_ignore_ascii_case(name))?
            .name;
        let linux = match rest {
            "" => expand_tilde(&distro, "~")?,
            r if r.starts_with('~') => expand_tilde(&distro, r)?,
            r => r.to_owned(),
        };

        return Some(unc(&distro, &normalize(&linux)));
    }

    let here = here?;
    if windows_absolute(arg) {
        return None;
    }
    let linux = if arg.starts_with('~') {
        expand_tilde(&here.distro, arg)?
    } else if arg.starts_with(['/', '\\']) {
        arg.to_owned()
    } else {
        format!("{}/{arg}", here.linux)
    };

    Some(unc(&here.distro, &normalize(&linux)))
}

/// `arg`, a Windows path in `here`'s distro, as Linux names it (with any
/// final `/`). `None` for anything else.
pub fn linux_arg(here: &Here, arg: &str) -> Option<String> {
    let (distro, linux) = split(Path::new(arg))?;
    if !distro.eq_ignore_ascii_case(&here.distro) {
        return None;
    }
    let slash = if arg.ends_with(['/', '\\']) && linux != "/" {
        "/"
    } else {
        ""
    };

    Some(format!("{linux}{slash}"))
}

/// The `wsl.exe` arguments that run `line` in `here`, in the user's login
/// shell, so `~/.profile`'s PATH applies.
pub fn command_args(here: &Here, line: &str) -> Vec<String> {
    // `-e` hands the arguments over as they are, where without it, `wsl.exe`
    // would requote the line.
    let args = [
        "-d",
        &here.distro,
        "--cd",
        &here.linux,
        "-e",
        "sh",
        "-c",
        r#"exec "${SHELL:-sh}" -lc "$1""#,
        "sh",
        line,
    ];

    args.map(str::to_owned).to_vec()
}

/// `path` with its `.` and `..` worked out, without reading the disk.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if out.file_name().is_some() {
                    out.pop();
                }
            }
            c => out.push(c),
        }
    }

    out
}

/// A `cd` line whose argument the completion code can't follow as Linux
/// would, respelled as Windows reaches it: a Linux path from the root, a
/// `Ubuntu:` one, or a relative one that crosses into or out of `/mnt/c`.
/// [Self::back] spells a completed line as it was typed.
pub struct CdLine {
    pub line: String,
    /// `cd `.
    lead: String,
    /// How the argument starts as typed, and as respelled (with `/`s), up
    /// to what they share, which completion only adds to.
    typed: String,
    respelled: String,
}

impl CdLine {
    pub fn new(here: Option<&Here>, cwd: &Path, line: &str) -> Option<Self> {
        let arg = line.strip_prefix("cd ")?.trim_start();
        // `~` the completion code knows, from its home.
        if arg.starts_with('~') || arg.parse::<usize>().is_ok() {
            return None;
        }
        let lead = &line[..line.len() - arg.len()];

        let fs = fs_path(&resolve(here, arg)?);
        let crosses = arg.starts_with(['/', '\\']) || arg.contains(':');
        if !crosses && fs == lexical(&cwd.join(arg)) {
            return None;
        }

        let mut resolved = fs.to_string_lossy().into_owned();
        if arg.ends_with(['/', '\\']) && !resolved.ends_with('\\') {
            resolved.push('\\');
        }
        let respelled = resolved.replace('\\', "/");

        let shared = arg
            .chars()
            .rev()
            .zip(respelled.chars().rev())
            .take_while(|(a, b)| a == b || (*a == '\\' && *b == '/'))
            .map(|(a, _)| a.len_utf8())
            .sum::<usize>();

        Some(Self {
            line: format!("{lead}{resolved}"),
            lead: lead.to_owned(),
            typed: arg[..arg.len() - shared].to_owned(),
            respelled: respelled[..respelled.len() - shared].to_owned(),
        })
    }

    /// `line`, a completion of [Self::line], spelled as the user typed it.
    pub fn back(&self, line: &str) -> Option<String> {
        let arg = line.strip_prefix(&self.lead)?.replace('\\', "/");
        let rest = arg.strip_prefix(&self.respelled)?;

        Some(format!("{}{}{rest}", self.lead, self.typed))
    }
}

#[cfg(windows)]
mod sys {
    use std::{
        ffi::c_void,
        ptr::{null, null_mut},
    };

    use windows_sys::Win32::{
        Foundation::ERROR_SUCCESS,
        System::Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_READ, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegCloseKey,
            RegEnumKeyExW, RegGetValueW, RegOpenKeyExW,
        },
    };

    use super::Distro;

    const LXSS: &str = r"Software\Microsoft\Windows\CurrentVersion\Lxss";
    /// A distro's `State` once it's installed.
    const INSTALLED: u32 = 1;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    /// The string `name` under `key\sub` (NUL-terminated; empty for `key`).
    fn string(key: HKEY, sub: &[u16], name: &str) -> Option<String> {
        let name = wide(name);
        let sub = if sub.len() > 1 { sub.as_ptr() } else { null() };
        let mut buf = vec![0u16; 64];
        loop {
            let mut bytes = (buf.len() * 2) as u32;
            // SAFETY: the strings are NUL-terminated or null; `buf` has room
            // for `bytes` bytes.
            let err = unsafe {
                RegGetValueW(
                    key,
                    sub,
                    name.as_ptr(),
                    RRF_RT_REG_SZ,
                    null_mut(),
                    buf.as_mut_ptr().cast::<c_void>(),
                    &mut bytes,
                )
            };
            if err == ERROR_SUCCESS {
                buf.truncate(bytes as usize / 2);
                while buf.last() == Some(&0) {
                    buf.pop();
                }
                return Some(String::from_utf16_lossy(&buf));
            }
            if bytes as usize <= buf.len() * 2 {
                return None;
            }
            buf = vec![0; bytes as usize / 2 + 1];
        }
    }

    /// The DWORD `name` under `key\sub`.
    fn dword(key: HKEY, sub: &[u16], name: &str) -> Option<u32> {
        let name = wide(name);
        let mut value = 0u32;
        let mut bytes = 4u32;
        // SAFETY: as for [string], with room for one DWORD.
        let err = unsafe {
            RegGetValueW(
                key,
                sub.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_DWORD,
                null_mut(),
                (&raw mut value).cast::<c_void>(),
                &mut bytes,
            )
        };

        (err == ERROR_SUCCESS).then_some(value)
    }

    pub fn distros() -> Vec<Distro> {
        let mut key: HKEY = null_mut();
        let path = wide(LXSS);
        // SAFETY: `path` is NUL-terminated; `key` is closed below.
        if unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, KEY_READ, &mut key) }
            != ERROR_SUCCESS
        {
            return Vec::new();
        }

        let default = string(key, &[0], "DefaultDistribution");
        let mut found = Vec::new();

        for i in 0.. {
            // A GUID, in braces.
            let mut sub = [0u16; 64];
            let mut len = sub.len() as u32;
            // SAFETY: `sub` holds `len` characters; the rest are optional.
            let err = unsafe {
                RegEnumKeyExW(
                    key,
                    i,
                    sub.as_mut_ptr(),
                    &mut len,
                    null(),
                    null_mut(),
                    null_mut(),
                    null_mut(),
                )
            };
            if err != ERROR_SUCCESS {
                break;
            }
            let sub = &sub[..=len as usize];

            let Some(name) = string(key, sub, "DistributionName") else {
                continue;
            };
            // Docker Desktop's own, which aren't for logging in to.
            let usable = !name.starts_with("docker-desktop")
                && dword(key, sub, "State").is_none_or(|s| s == INSTALLED);
            if !usable {
                continue;
            }

            let guid = String::from_utf16_lossy(&sub[..len as usize]);
            let is_default = default.as_deref() == Some(guid.as_str());
            let distro = Distro {
                name,
                uid: dword(key, sub, "DefaultUid").unwrap_or(0),
            };
            if is_default {
                found.insert(0, distro);
            } else {
                found.push(distro);
            }
        }

        // SAFETY: opened above.
        unsafe { RegCloseKey(key) };

        found
    }
}

#[cfg(not(windows))]
mod sys {
    pub fn distros() -> Vec<super::Distro> {
        Vec::new()
    }
}
