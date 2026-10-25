//! Where the child windows (file viewers, PATH) open: where one was last
//! moved or resized to, saved between runs (see [crate::prefs]).

use egui::{Context, Pos2, Vec2};
use serde::{Deserialize, Serialize};

/// A window's place on screen, in points.
#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Placement {
    /// The outer frame's top left.
    pub pos: Pos2,
    /// The content area's size.
    pub size: Vec2,
}

impl Placement {
    /// The current viewport's placement. `None` while minimized, maximized
    /// or full screen: new windows shouldn't open like that.
    pub fn of(ctx: &Context) -> Option<Self> {
        ctx.input(|i| {
            let info = i.viewport();
            let special = [info.minimized, info.maximized, info.fullscreen];
            if special.contains(&Some(true)) {
                return None;
            }
            let size = info.inner_rect?.size();
            if size.x <= 0. || size.y <= 0. {
                return None;
            }
            Some(Self {
                pos: info.outer_rect?.min,
                size,
            })
        })
    }

    /// Moved, and shrunk if need be, onto the work area of the monitor it's
    /// nearest, for when that monitor's gone or the layout changed since.
    /// Only on Windows, where a window placed off every monitor stays
    /// there; eframe does the same for the main window. `ctx` is the main
    /// window's, for its scale.
    pub fn on_screen(self, ctx: &Context) -> Self {
        #[cfg(windows)]
        if let Some(work) = work_area(self, ctx.pixels_per_point()) {
            let bar = Vec2::new(0., TITLE_BAR_HEIGHT);
            let size = self.size.min(work.size() - bar);
            let max = (work.max - size - bar).max(work.min);

            return Self {
                pos: self.pos.clamp(work.min, max),
                size,
            };
        }
        #[cfg(not(windows))]
        let _ = ctx;

        self
    }
}

/// Roughly, the outer frame's height above the content area: Windows 10
/// and 11's title bar, as eframe reckons it.
#[cfg(windows)]
const TITLE_BAR_HEIGHT: f32 = 32.;

/// The work area (the monitor less the taskbar), in points, of the monitor
/// nearest `p`, at `ppp` pixels per point.
#[cfg(windows)]
fn work_area(p: Placement, ppp: f32) -> Option<egui::Rect> {
    use windows_sys::Win32::{
        Foundation::RECT,
        Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromRect},
    };

    let px = |v: f32| (v * ppp).round() as i32;
    let max = p.pos + p.size + Vec2::new(0., TITLE_BAR_HEIGHT);
    let rect = RECT {
        left: px(p.pos.x),
        top: px(p.pos.y),
        right: px(max.x),
        bottom: px(max.y),
    };
    // SAFETY: plain data, zeroed as the API expects before `cbSize` is set.
    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = size_of::<MONITORINFO>() as u32;

    // SAFETY: `rect` and `info` outlive the calls. With
    // `MONITOR_DEFAULTTONEAREST` there's always a monitor.
    if unsafe { GetMonitorInfoW(MonitorFromRect(&rect, MONITOR_DEFAULTTONEAREST), &mut info) } == 0
    {
        return None;
    }

    let w = info.rcWork;
    let pt = |x: i32, y: i32| Pos2::new(x as f32 / ppp, y as f32 / ppp);

    Some(egui::Rect::from_min_max(
        pt(w.left, w.top),
        pt(w.right, w.bottom),
    ))
}
