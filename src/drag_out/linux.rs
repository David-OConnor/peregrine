//! Dragging files out of the window on Linux: on X11 (see `super::x11`) or
//! Wayland (see `super::wayland`), whichever the window is on. Either way
//! the drag goes on on a thread of its own, as the window must keep
//! drawing, and the UI asks after it each frame ([Drag::poll]).

use std::{
    panic::{self, AssertUnwindSafe},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
};

use super::{Outcome, protocol, wayland::Wayland, x11};

/// The main window, which drags start from.
#[derive(Clone, Copy)]
pub enum Window {
    /// Its X11 window.
    X11(u32),
    /// Our own objects on winit's Wayland connection, made for it.
    Wayland(&'static Wayland),
}

impl Window {
    pub fn of(frame: &eframe::Frame) -> Option<Self> {
        use raw_window_handle::{
            HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
        };
        match frame.window_handle().ok()?.as_raw() {
            RawWindowHandle::Xlib(h) => Some(Self::X11(h.window as u32)),
            RawWindowHandle::Xcb(h) => Some(Self::X11(h.window.get())),
            RawWindowHandle::Wayland(h) => {
                let RawDisplayHandle::Wayland(d) = frame.display_handle().ok()?.as_raw() else {
                    return None;
                };
                Wayland::get(d.display, h.surface).map(Self::Wayland)
            }
            _ => None,
        }
    }
}

/// Drag `paths` to other programs, from where the pointer is, until
/// they're dropped or the pointer comes back over `window`. Returns at
/// once; [Drag::poll] says how it ended. `ctx` is woken when it has.
pub fn start(window: Window, paths: &[PathBuf], ctx: &egui::Context) -> Drag {
    use std::os::unix::ffi::OsStrExt;
    // Taken from the listing, so already absolute; this just makes sure.
    let paths: Vec<PathBuf> = paths
        .iter()
        .map(|p| std::path::absolute(p).unwrap_or_else(|_| p.clone()))
        .collect();
    let list = protocol::uri_list(paths.iter().map(|p| p.as_os_str().as_bytes()));
    match window {
        Window::X11(w) => x11::start(w, list, ctx),
        Window::Wayland(wl) => wl.start(list, ctx),
    }
}

/// A drag handed to the OS (see [start]), going on on another thread.
pub struct Drag(Arc<Shared>);

/// What a drag's thread and the UI tell each other.
#[derive(Default)]
pub(super) struct Shared {
    /// Set by [Drag::cancel].
    pub cancel: AtomicBool,
    /// What the program under the pointer would do with them, as far as
    /// we know (a [Feedback]).
    pub feedback: AtomicU8,
    /// How it ended, once it has, until [Drag::poll] takes it.
    outcome: Mutex<Option<Result<Outcome, String>>>,
}

/// What the program under the pointer would do with the files, for the
/// cursor, on X11, where the window still has the pointer while it's
/// dragged out (see [Drag::cursor]). On Wayland the compositor draws it.
#[repr(u8)]
pub(super) enum Feedback {
    Unknown,
    /// Nothing there takes them.
    NoDrop,
    Copy,
    Move,
    Link,
}

impl Drag {
    /// One already over.
    pub(super) fn over(outcome: Result<Outcome, String>) -> Self {
        let shared = Shared::default();
        *shared.outcome.lock().unwrap() = Some(outcome);
        Self(Arc::new(shared))
    }

    /// Run `drag` on a thread of its own, with the [Shared] state it
    /// reports to, which [Drag] reads. `ctx` is woken once it's over.
    pub(super) fn spawn(
        ctx: &egui::Context,
        drag: impl FnOnce(&Shared) -> Result<Outcome, String> + Send + 'static,
    ) -> Self {
        let shared = Arc::new(Shared::default());
        let (theirs, ctx) = (shared.clone(), ctx.clone());
        let spawned = std::thread::Builder::new()
            .name("drag out".into())
            .spawn(move || {
                // Never left going for want of an answer.
                let outcome = panic::catch_unwind(AssertUnwindSafe(|| drag(&theirs)))
                    .unwrap_or_else(|_| Err("drag: it failed".into()));
                *theirs.outcome.lock().unwrap() = Some(outcome);
                ctx.request_repaint();
            });
        match spawned {
            Ok(_) => Self(shared),
            Err(e) => Self::over(Err(format!("drag: can't start its thread ({e})"))),
        }
    }

    /// How it ended, once it has (and then only once).
    pub fn poll(&mut self) -> Option<Result<Outcome, String>> {
        self.0.outcome.lock().unwrap().take()
    }

    /// Call it off, as Escape does.
    pub fn cancel(&self) {
        self.0.cancel.store(true, Ordering::Relaxed);
    }

    /// The cursor to show while it's out of the window, when it's ours to
    /// show: whether the program under the pointer takes them, and how.
    pub fn cursor(&self) -> Option<egui::CursorIcon> {
        use egui::CursorIcon;
        const NO_DROP: u8 = Feedback::NoDrop as u8;
        const COPY: u8 = Feedback::Copy as u8;
        const MOVE: u8 = Feedback::Move as u8;
        const LINK: u8 = Feedback::Link as u8;
        match self.0.feedback.load(Ordering::Relaxed) {
            NO_DROP => Some(CursorIcon::NoDrop),
            COPY => Some(CursorIcon::Copy),
            MOVE => Some(CursorIcon::Grabbing),
            LINK => Some(CursorIcon::Alias),
            _ => None,
        }
    }
}

impl Shared {
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub fn set_feedback(&self, feedback: Feedback) {
        self.feedback.store(feedback as u8, Ordering::Relaxed);
    }
}

/// Before each frame: on Wayland, take our events and give egui the
/// pointer's moves over the window during a drag (see `super::wayland`).
pub fn raw_input_hook(window: Window, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
    if let Window::Wayland(wl) = window {
        wl.raw_input_hook(ctx, raw_input);
    }
}
