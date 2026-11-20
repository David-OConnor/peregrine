use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
};

use egui::{Color32, ColorImage};
use x11rb::{
    connection::Connection,
    protocol::xproto::{Atom, AtomEnum, ConnectionExt, GetPropertyReply},
};

use super::{Application, ICON_PIXELS, RunningApps, raster_icon};

#[path = "linux/wayland.rs"]
mod wayland;

struct Window {
    key: u32,
    app_id: String,
    title: String,
}

pub(super) fn collect(apps: &mut RunningApps<'_>) -> Result<Option<&'static str>, String> {
    let mut desktop = Desktop::default();
    let wayland_session = env::var_os("WAYLAND_DISPLAY").is_some()
        || env::var_os("WAYLAND_SOCKET").is_some()
        || env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland");
    if wayland_session && let Some(windows) = wayland::windows() {
        for window in windows {
            add_window(apps, &mut desktop, window, || None);
        }
        return Ok(None);
    }

    let notice = wayland_session.then_some("This Wayland session exposes only X11 applications.");
    match x11_windows(apps, &mut desktop) {
        Some(()) => Ok(notice),
        None if wayland_session => {
            Err("This Wayland session does not expose its application list.".into())
        }
        None => Err("The desktop window list is unavailable.".into()),
    }
}

fn add_window(
    apps: &mut RunningApps<'_>,
    desktop: &mut Desktop,
    window: Window,
    window_icon: impl FnOnce() -> Option<ColorImage>,
) {
    let app_id = window.app_id.trim().trim_end_matches(".desktop");
    let fallback = window.title.trim();
    if app_id.is_empty() && fallback.is_empty() {
        return;
    }
    let key = if app_id.is_empty() {
        format!("window:{}:{fallback}", window.key)
    } else {
        app_id.to_ascii_lowercase()
    };
    apps.add(key, || {
        let (name, desktop_icon) = desktop.details(app_id);
        let name = name.unwrap_or_else(|| {
            if app_id.is_empty() {
                fallback.into()
            } else {
                app_id.into()
            }
        });
        let icon = desktop_icon
            .or_else(window_icon)
            .or_else(|| desktop.icon("application-x-executable"));
        Application { name, icon }
    });
}

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        UTF8_STRING,
        _GTK_APPLICATION_ID,
        _NET_CLIENT_LIST,
        _NET_WM_ICON,
        _NET_WM_NAME,
        _NET_WM_PID,
        _NET_WM_STATE,
        _NET_WM_STATE_SKIP_TASKBAR,
        _NET_WM_WINDOW_TYPE,
        _NET_WM_WINDOW_TYPE_NORMAL,
        _NET_WM_WINDOW_TYPE_DIALOG,
    }
}

fn x11_windows(apps: &mut RunningApps<'_>, desktop: &mut Desktop) -> Option<()> {
    let (conn, _) = x11rb::connect(None).ok()?;
    let atoms = Atoms::new(&conn).ok()?.reply().ok()?;
    let mut has_client_list = false;
    for screen in &conn.setup().roots {
        let clients = property(
            &conn,
            screen.root,
            atoms._NET_CLIENT_LIST,
            AtomEnum::WINDOW,
            65536,
        );
        let Some(windows) = clients.as_ref().and_then(GetPropertyReply::value32) else {
            continue;
        };
        has_client_list = true;
        for key in windows {
            let get = |atom, ty, len| property(&conn, key, atom, ty, len);
            if get(atoms._NET_WM_STATE, AtomEnum::ATOM, 64).and_then(|state| {
                state
                    .value32()
                    .map(|mut a| a.any(|a| a == atoms._NET_WM_STATE_SKIP_TASKBAR))
            }) == Some(true)
            {
                continue;
            }
            if let Some(types) = get(atoms._NET_WM_WINDOW_TYPE, AtomEnum::ATOM, 16)
                && let Some(mut types) = types.value32()
                && !types.any(|t| {
                    t == atoms._NET_WM_WINDOW_TYPE_NORMAL || t == atoms._NET_WM_WINDOW_TYPE_DIALOG
                })
            {
                continue;
            }
            let app_id = property(
                &conn,
                key,
                atoms._GTK_APPLICATION_ID,
                atoms.UTF8_STRING,
                256,
            )
            .and_then(text_property)
            .or_else(|| {
                get(AtomEnum::WM_CLASS.into(), AtomEnum::STRING, 256).and_then(|reply| {
                    // WM_CLASS contains the instance name, then the application class.
                    reply
                        .value
                        .split(|byte| *byte == 0)
                        .take(2)
                        .filter(|part| !part.is_empty())
                        .last()
                        .map(|part| String::from_utf8_lossy(part).into_owned())
                })
            })
            .or_else(|| {
                let pid = get(atoms._NET_WM_PID, AtomEnum::CARDINAL, 1)?
                    .value32()?
                    .next()?;
                let exe = fs::read_link(format!("/proc/{pid}/exe")).ok()?;
                Some(exe.file_name()?.to_string_lossy().into_owned())
            })
            .unwrap_or_default();
            let title = property(&conn, key, atoms._NET_WM_NAME, atoms.UTF8_STRING, 256)
                .and_then(text_property)
                .or_else(|| {
                    get(AtomEnum::WM_NAME.into(), AtomEnum::STRING, 256).and_then(text_property)
                })
                .unwrap_or_default();
            add_window(apps, desktop, Window { key, app_id, title }, || {
                let icons = get(atoms._NET_WM_ICON, AtomEnum::CARDINAL, 512 * 512 + 4096)?;
                x11_icon(&icons.value32()?.collect::<Vec<_>>())
            });
        }
    }
    has_client_list.then_some(())
}

fn property(
    conn: &impl Connection,
    window: u32,
    name: Atom,
    ty: impl Into<Atom>,
    len: u32,
) -> Option<GetPropertyReply> {
    conn.get_property(false, window, name, ty.into(), 0, len)
        .ok()?
        .reply()
        .ok()
}

fn text_property(reply: GetPropertyReply) -> Option<String> {
    let text = String::from_utf8_lossy(&reply.value);
    let text = text.trim_matches('\0').trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn x11_icon(mut values: &[u32]) -> Option<ColorImage> {
    let mut best = None;
    while let [width, height, pixels @ ..] = values {
        let (width, height) = (*width, *height);
        let count = width.checked_mul(height)? as usize;
        let Some((icon, rest)) = pixels.split_at_checked(count) else {
            break;
        };
        if count != 0 {
            let distance = width.max(height).abs_diff(ICON_PIXELS);
            if best.is_none_or(|(d, _, _, _)| distance < d) {
                best = Some((distance, width, height, icon));
            }
        }
        values = rest;
    }
    let (_, width, height, pixels) = best?;
    if width.max(height) > ICON_PIXELS {
        let rgba: Vec<_> = pixels
            .iter()
            .flat_map(|pixel| {
                let [a, r, g, b] = pixel.to_be_bytes();
                [r, g, b, a]
            })
            .collect();
        let image = image::RgbaImage::from_raw(width, height, rgba)?;
        Some(raster_icon(image.into()))
    } else {
        let pixels = pixels
            .iter()
            .map(|pixel| {
                let [a, r, g, b] = pixel.to_be_bytes();
                Color32::from_rgba_unmultiplied(r, g, b, a)
            })
            .collect();
        Some(ColorImage::new([width as usize, height as usize], pixels))
    }
}

#[derive(Default)]
struct Desktop {
    data_dirs: Vec<PathBuf>,
    icons_dirs: Vec<PathBuf>,
    entries: Option<HashMap<String, DesktopEntry>>,
}

#[derive(Clone, Default)]
struct DesktopEntry {
    name: String,
    icon: String,
}

impl Desktop {
    fn load_entries(&mut self) {
        let home = env::var_os("HOME").map(PathBuf::from);
        let user_data = env::var_os("XDG_DATA_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|home| home.join(".local/share")));
        let system_data = env::var_os("XDG_DATA_DIRS")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
        self.data_dirs.extend(user_data);
        self.data_dirs.extend(env::split_paths(&system_data));
        if let Some(home) = &home {
            self.data_dirs
                .push(home.join(".local/share/flatpak/exports/share"));
            self.icons_dirs.push(home.join(".icons"));
        }
        self.data_dirs.extend([
            "/var/lib/flatpak/exports/share".into(),
            "/var/lib/snapd/desktop".into(),
        ]);
        self.icons_dirs
            .extend(self.data_dirs.iter().map(|dir| dir.join("icons")));
        let mut entries = HashMap::new();
        for dir in &self.data_dirs {
            let base = dir.join("applications");
            for path in ignore::WalkBuilder::new(&base)
                .standard_filters(false)
                .max_depth(Some(5))
                .build()
                .filter_map(Result::ok)
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "desktop"))
            {
                let path = path.path();
                let Ok(relative) = path.strip_prefix(&base) else {
                    continue;
                };
                let id = relative
                    .with_extension("")
                    .to_string_lossy()
                    .replace('/', "-");
                let Ok(text) = fs::read_to_string(path) else {
                    continue;
                };
                let fields = section(&text, "Desktop Entry");
                let field = |key| fields.get(key).copied().unwrap_or_default();
                let entry = DesktopEntry {
                    name: field("Name").into(),
                    icon: field("Icon").into(),
                };
                entries
                    .entry(id.to_ascii_lowercase())
                    .or_insert_with(|| entry.clone());
                if !field("StartupWMClass").is_empty() {
                    entries
                        .entry(field("StartupWMClass").to_ascii_lowercase())
                        .or_insert(entry);
                }
            }
        }
        self.entries = Some(entries);
    }

    fn details(&mut self, app_id: &str) -> (Option<String>, Option<ColorImage>) {
        if self.entries.is_none() {
            self.load_entries();
        }
        let entry = self
            .entries
            .as_ref()
            .and_then(|entries| entries.get(&app_id.to_ascii_lowercase()));
        let name = entry
            .filter(|entry| !entry.name.is_empty())
            .map(|entry| entry.name.clone());
        let icon = entry
            .and_then(|entry| {
                let path = Path::new(&entry.icon);
                if path.is_absolute() {
                    load_icon(path)
                } else {
                    self.icon(&entry.icon)
                }
            })
            .or_else(|| self.icon(app_id));
        (name, icon)
    }

    fn icon(&self, name: &str) -> Option<ColorImage> {
        if name.is_empty() {
            return None;
        }
        let path = Path::new(name);
        if path.file_name()?.to_str()? != name {
            return None;
        }
        let category = if name == "application-x-executable" {
            "mimetypes"
        } else {
            "apps"
        };
        let themes = ["hicolor", "Adwaita", "breeze"];
        let sizes = [
            "32x32", "48x48", "scalable", "64x64", "128x128", "256x256", "24x24", "16x16", "22x22",
        ];
        let try_files = |dir: &Path| {
            let path = dir.join(name);
            [
                path.clone(),
                dir.join(format!("{name}.png")),
                dir.join(format!("{name}.svg")),
            ]
            .iter()
            .find_map(|path| load_icon(path))
        };
        for theme in themes {
            for base in &self.icons_dirs {
                for size in sizes {
                    if let Some(icon) = try_files(&base.join(theme).join(size).join(category)) {
                        return Some(icon);
                    }
                }
            }
        }
        self.icons_dirs
            .iter()
            .find_map(|dir| try_files(dir))
            .or_else(|| {
                self.data_dirs
                    .iter()
                    .find_map(|dir| try_files(&dir.join("pixmaps")))
            })
    }
}

fn section<'a>(text: &'a str, header: &str) -> HashMap<&'a str, &'a str> {
    let header = format!("[{header}]");
    text.lines()
        .map(str::trim)
        .skip_while(|line| *line != header)
        .skip(1)
        .take_while(|line| !line.starts_with('['))
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .collect()
}

fn load_icon(path: &Path) -> Option<ColorImage> {
    let file = fs::metadata(path).ok()?;
    if !file.is_file() || file.len() > 8 * 1024 * 1024 {
        return None;
    }
    if path.extension().is_some_and(|ext| ext == "svg") {
        let data = fs::read(path).ok()?;
        let tree = resvg::usvg::Tree::from_data(&data, &resvg::usvg::Options::default()).ok()?;
        let size = tree.size();
        let scale = ICON_PIXELS as f32 / size.width().max(size.height());
        let mut pixmap = resvg::tiny_skia::Pixmap::new(
            (size.width() * scale).round().max(1.) as u32,
            (size.height() * scale).round().max(1.) as u32,
        )?;
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );
        return Some(ColorImage::from_rgba_premultiplied(
            [pixmap.width() as usize, pixmap.height() as usize],
            pixmap.data(),
        ));
    }
    image::open(path).ok().map(raster_icon)
}
