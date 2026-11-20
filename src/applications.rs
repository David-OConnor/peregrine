//! Applications reported by the current graphical desktop.

use std::{collections::HashMap, sync::Arc};

use egui::ColorImage;

#[cfg(target_os = "linux")]
#[path = "applications/linux.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "applications/macos.rs"]
mod platform;
#[cfg(windows)]
#[path = "applications/windows.rs"]
mod platform;

pub const ICON_PIXELS: u32 = 32;

pub struct Application {
    pub name: String,
    pub icon: Option<ColorImage>,
}

#[derive(Default)]
pub struct Snapshot {
    pub apps: HashMap<String, Arc<Application>>,
    pub notice: Option<&'static str>,
}

pub fn sample(previous: &Snapshot) -> Result<Snapshot, String> {
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    {
        let mut apps = RunningApps {
            previous: &previous.apps,
            current: HashMap::new(),
        };
        let notice = platform::collect(&mut apps)?;
        Ok(Snapshot {
            apps: apps.current,
            notice,
        })
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = previous;
        Err("Running applications are unavailable on this platform.".into())
    }
}

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
struct RunningApps<'a> {
    previous: &'a HashMap<String, Arc<Application>>,
    current: HashMap<String, Arc<Application>>,
}

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
impl RunningApps<'_> {
    /// Several windows can belong to the same app. Reuse its native
    /// metadata while it remains in the list.
    fn add(&mut self, id: String, load: impl FnOnce() -> Application) {
        self.current.entry(id).or_insert_with_key(|id| {
            self.previous
                .get(id)
                .cloned()
                .unwrap_or_else(|| Arc::new(load()))
        });
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn raster_icon(image: image::DynamicImage) -> ColorImage {
    let image = image.thumbnail(ICON_PIXELS, ICON_PIXELS).into_rgba8();
    ColorImage::from_rgba_unmultiplied(
        [image.width() as usize, image.height() as usize],
        image.as_raw(),
    )
}
