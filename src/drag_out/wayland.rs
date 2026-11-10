//! Dragging files out of the window on Wayland: a drag from the window's
//! own surface (`wl_data_device.start_drag`), offering the files as
//! `text/uri-list`, as a file manager's drag does.
//!
//! winit has no drag and drop on Wayland, and keeps the serials of input
//! events to itself, while a compositor only starts a drag given the serial
//! of the button press it comes from. So we make objects of our own on
//! winit's connection (its `wl_display`, through libwayland, with an event
//! queue of our own): a seat and pointer, made before any press, which get
//! the same button events as winit's, serials and all, as a compositor sends
//! them to all a program's pointers; and a data device to drag with. winit
//! reads the socket for everyone. Between drags, our events are taken off
//! our queue before each frame ([Wayland::raw_input_hook]); during one, a
//! thread of our own reads and handles them, so a program asking for the
//! files gets them however busy the window is.
//!
//! A drag can't be handed back once the compositor has it, so over our own
//! window it goes on as the compositor's, and the window takes it like any
//! other. The pointer's moves over it, and where it's let go of, are given
//! to egui as if winit had seen them ([Injected]), so egui's own drag
//! carries on there, and folders here still take the files.

use std::{
    ffi::c_void,
    fs::File,
    io::Write,
    ptr::NonNull,
    sync::{Arc, Mutex, OnceLock},
    thread,
};

use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
    backend::{Backend, ObjectId},
    event_created_child,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_data_device::{self, WlDataDevice},
        wl_data_device_manager::{DndAction, WlDataDeviceManager},
        wl_data_offer::WlDataOffer,
        wl_data_source::{self, WlDataSource},
        wl_pointer::{self, ButtonState, WlPointer},
        wl_registry::WlRegistry,
        wl_seat::{self, Capability, WlSeat},
        wl_surface::WlSurface,
    },
};

use super::{
    Outcome,
    linux::{Drag, Shared},
    protocol::URI_LIST,
};

/// The primary (left) mouse button, as Linux numbers them. The compositor
/// swaps them for a left-handed mouse.
const BTN_LEFT: u32 = 0x110;

/// How long a drag's thread waits for the socket before looking whether
/// it's been called off.
const WAIT_MS: i64 = 20;

/// Our objects on winit's connection, for dragging from its window.
pub struct Wayland {
    conn: Connection,
    qh: QueueHandle<Events>,
    manager: WlDataDeviceManager,
    device: WlDataDevice,
    /// winit's, for the window.
    surface: WlSurface,
    /// Our event queue, and what its events have told us. A drag's thread
    /// takes them while it goes on.
    idle: Mutex<Option<(EventQueue<Events>, Events)>>,
    /// Pointer events for egui, from a drag over our window.
    injected: Arc<Mutex<Vec<Injected>>>,
}

impl Wayland {
    /// Ours for winit's `display` and the window's `surface`, made the first
    /// time it's asked for. `None` when the compositor can't drag files
    /// (no seat, or a data device manager older than version 3).
    pub fn get(display: NonNull<c_void>, surface: NonNull<c_void>) -> Option<&'static Self> {
        static WAYLAND: OnceLock<Option<Wayland>> = OnceLock::new();
        // Safety: winit's display and surface last as long as the app.
        WAYLAND
            .get_or_init(|| unsafe { Self::new(display, surface) })
            .as_ref()
    }

    /// # Safety
    ///
    /// `display` and `surface` must be live, and stay so.
    unsafe fn new(display: NonNull<c_void>, surface: NonNull<c_void>) -> Option<Self> {
        let backend = unsafe { Backend::from_foreign_display(display.as_ptr().cast()) };
        let conn = Connection::from_backend(backend);
        let (globals, mut queue) = registry_queue_init::<Events>(&conn).ok()?;
        let qh = queue.handle();
        // Version 1: our pointer is only for its buttons, and later ones
        // send more besides (`frame`, `axis_source`...).
        let seat: WlSeat = globals.bind(&qh, 1..=1, ()).ok()?;
        // 3, for drag actions, and word of where the drop went
        // (`dnd_drop_performed`, `dnd_finished`).
        let manager: WlDataDeviceManager = globals.bind(&qh, 3..=3, ()).ok()?;
        let device = manager.get_data_device(&seat, &qh, ());
        let id = unsafe { ObjectId::from_ptr(WlSurface::interface(), surface.as_ptr().cast()) };
        let surface = WlSurface::from_id(&conn, id.ok()?).ok()?;
        let injected = Arc::new(Mutex::new(Vec::new()));
        let mut events = Events {
            surface: surface.id(),
            pointer: None,
            press: None,
            drag: None,
            injected: injected.clone(),
        };
        // For the seat to say it has a pointer, and ours to be made.
        queue.roundtrip(&mut events).ok()?;
        Some(Self {
            conn,
            qh,
            manager,
            device,
            surface,
            idle: Mutex::new(Some((queue, events))),
            injected,
        })
    }

    /// Before each frame: take our events off our queue between drags, so
    /// they don't pile up, and give egui the pointer events a drag over the
    /// window has left it.
    pub fn raw_input_hook(&self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        if let Ok(mut idle) = self.idle.try_lock()
            && let Some((queue, events)) = idle.as_mut()
        {
            let _ = queue.dispatch_pending(events);
        }

        let injected = std::mem::take(&mut *self.injected.lock().unwrap());
        // Surface coordinates are logical pixels, as egui's points are, but
        // for egui's own zoom.
        let zoom = ctx.zoom_factor();
        let pos = |(x, y): (f64, f64)| egui::pos2(x as f32 / zoom, y as f32 / zoom);
        let modifiers = ctx.input(|i| i.modifiers);
        for event in injected {
            raw_input.events.push(match event {
                Injected::Moved(at) => egui::Event::PointerMoved(pos(at)),
                Injected::Released(at) => egui::Event::PointerButton {
                    // Off the window, if not on it, where it can't be a
                    // click on anything.
                    pos: at.map_or(egui::pos2(-1.0, -1.0), pos),
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers,
                },
                Injected::Gone => egui::Event::PointerGone,
            });
        }
    }

    /// Drag `list`, a `text/uri-list`, from where the pointer is, with the
    /// press it was pressed with, on a thread of its own.
    pub fn start(&'static self, list: Vec<u8>, ctx: &egui::Context) -> Drag {
        let Some((mut queue, mut events)) = self.idle.lock().unwrap().take() else {
            return Drag::over(Err("drag: the last one is still going".into()));
        };
        // Whatever winit has read for us since the last frame.
        let _ = queue.dispatch_pending(&mut events);
        let Some(serial) = events.press else {
            // Let go of already: there's no drag left to hand over.
            *self.idle.lock().unwrap() = Some((queue, events));
            return Drag::over(Ok(Outcome::Ended { dropped: false }));
        };

        let source = self.manager.create_data_source(&self.qh, ());
        source.offer(URI_LIST.into());
        // Whatever the target picks, as the compositor lets Ctrl and Shift
        // steer it; never a move on our side (the target does the moving).
        source.set_actions(DndAction::Copy | DndAction::Move);
        self.device
            .start_drag(Some(&source), &self.surface, None, serial);
        events.drag = Some(DragState {
            source,
            list: Arc::new(list),
            own: None,
            at: (0.0, 0.0),
            dropped_here: false,
            end: None,
        });
        let theirs = ctx.clone();
        Drag::spawn(ctx, move |shared| {
            let outcome = self.run(&mut queue, &mut events, shared, &theirs);
            *self.idle.lock().unwrap() = Some((queue, events));
            outcome
        })
    }

    /// The drag's thread: read and handle our events until it's over.
    fn run(
        &self,
        queue: &mut EventQueue<Events>,
        events: &mut Events,
        shared: &Shared,
        ctx: &egui::Context,
    ) -> Result<Outcome, String> {
        let wayland = |e: &dyn std::fmt::Display| format!("drag: Wayland error ({e})");
        let outcome = loop {
            if let Err(e) = queue.dispatch_pending(events) {
                break Err(wayland(&e));
            }
            if !self.injected.lock().unwrap().is_empty() {
                ctx.request_repaint();
            }
            if let Some(end) = events.drag.as_mut().and_then(|d| d.end.take()) {
                break Ok(end);
            }
            if shared.cancelled() {
                break Ok(Outcome::Ended { dropped: false });
            }
            if let Err(e) = self.conn.flush() {
                break Err(wayland(&e));
            }
            // Wait for the socket, unless winit has already read it for us
            // (`None`), and read it, alongside winit as libwayland arranges.
            if let Some(guard) = self.conn.prepare_read() {
                use rustix::event::{PollFd, PollFlags, Timespec, poll};
                let fd = guard.connection_fd();
                let mut fds = [PollFd::new(&fd, PollFlags::IN)];
                let wait = Timespec {
                    tv_sec: 0,
                    tv_nsec: WAIT_MS * 1_000_000,
                };
                let ready = matches!(poll(&mut fds, Some(&wait)), Ok(n) if n > 0);
                if ready && let Err(e) = guard.read() {
                    break Err(wayland(&e));
                }
            }
        };

        let drag = events.drag.take().expect("the drag's state");
        drag.source.destroy();
        if let Some(offer) = drag.own {
            offer.destroy();
        }
        // Our pointer hasn't seen the button let go of, so neither has
        // winit's: the compositor's drag had it. Let egui know, off the
        // window (unless it was let go of over it, and egui knows already).
        if events.press.take().is_some() && !drag.dropped_here {
            let mut injected = self.injected.lock().unwrap();
            injected.extend([Injected::Released(None), Injected::Gone]);
        }
        let _ = self.conn.flush();
        outcome
    }
}

/// A pointer event for egui, from a drag over our window, as if winit had
/// seen it.
#[derive(Clone, Copy)]
enum Injected {
    /// Moved to here, in surface coordinates (logical pixels).
    Moved((f64, f64)),
    /// The primary button let go of here, or off the window.
    Released(Option<(f64, f64)>),
    /// Off the window.
    Gone,
}

/// What our events have told us, and the drag they're about.
struct Events {
    /// The window's surface, to tell it from others.
    surface: ObjectId,
    pointer: Option<WlPointer>,
    /// The serial of the primary button's press, while it's held.
    press: Option<u32>,
    drag: Option<DragState>,
    injected: Arc<Mutex<Vec<Injected>>>,
}

/// A drag going on.
struct DragState {
    source: WlDataSource,
    /// The files, as `text/uri-list`.
    list: Arc<Vec<u8>>,
    /// The window's offer of it, while it's over the window.
    own: Option<WlDataOffer>,
    /// Where it was last over the window.
    at: (f64, f64),
    /// Let go of over the window, and given to egui there.
    dropped_here: bool,
    /// How it ended, once it has.
    end: Option<Outcome>,
}

impl Events {
    fn inject(&self, events: &[Injected]) {
        self.injected.lock().unwrap().extend_from_slice(events);
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for Events {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for Events {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
            && caps.contains(Capability::Pointer)
            && state.pointer.is_none()
        {
            state.pointer = Some(seat.get_pointer(qh, ()));
        }
    }
}

impl Dispatch<WlPointer, ()> for Events {
    fn event(
        state: &mut Self,
        _: &WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let wl_pointer::Event::Button {
            serial,
            button: BTN_LEFT,
            state: WEnum::Value(pressed),
            ..
        } = event
        else {
            return;
        };
        if pressed == ButtonState::Pressed {
            state.press = Some(serial);
        } else {
            state.press = None;
            // Seen here, so the compositor never took the drag (it would
            // have had the button): egui, seeing it too, still has it.
            if let Some(drag) = &mut state.drag {
                drag.end.get_or_insert(Outcome::Returned);
            }
        }
    }
}

impl Dispatch<WlDataDeviceManager, ()> for Events {
    fn event(
        _: &mut Self,
        _: &WlDataDeviceManager,
        _: <WlDataDeviceManager as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlDataDevice, ()> for Events {
    fn event(
        state: &mut Self,
        _: &WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wl_data_device::Event;
        match event {
            Event::Enter {
                serial,
                surface,
                x,
                y,
                id: Some(offer),
            } => match &mut state.drag {
                // Ours, over the window: taken, so the compositor shows
                // it can be dropped, and tells us where it is.
                Some(drag) if surface.id() == state.surface => {
                    offer.accept(serial, Some(URI_LIST.into()));
                    offer.set_actions(DndAction::Copy | DndAction::Move, DndAction::Move);
                    drag.own = Some(offer);
                    drag.at = (x, y);
                    state.inject(&[Injected::Moved((x, y))]);
                }
                // Another program's drag, or ours over some other surface
                // of ours: not taken.
                _ => offer.destroy(),
            },
            Event::Motion { x, y, .. } => {
                if let Some(drag) = &mut state.drag
                    && drag.own.is_some()
                {
                    drag.at = (x, y);
                    state.inject(&[Injected::Moved((x, y))]);
                }
            }
            Event::Leave => {
                if let Some(drag) = &mut state.drag
                    && let Some(offer) = drag.own.take()
                {
                    offer.destroy();
                    state.inject(&[Injected::Gone]);
                }
            }
            Event::Drop => {
                // Let go of over the window: egui drops it there. The offer
                // isn't finished, so the compositor calls the drag off.
                if let Some(drag) = &mut state.drag
                    && let Some(offer) = drag.own.take()
                {
                    offer.destroy();
                    drag.dropped_here = true;
                    let at = drag.at;
                    state.inject(&[Injected::Moved(at), Injected::Released(Some(at))]);
                }
            }
            // The clipboard's offer, as every data device is told of it.
            Event::Selection { id: Some(offer) } => offer.destroy(),
            _ => {}
        }
    }

    event_created_child!(Events, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, ()),
    ]);
}

impl Dispatch<WlDataOffer, ()> for Events {
    fn event(
        _: &mut Self,
        _: &WlDataOffer,
        _: <WlDataOffer as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlDataSource, ()> for Events {
    fn event(
        state: &mut Self,
        _: &WlDataSource,
        event: wl_data_source::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(drag) = &mut state.drag else {
            return;
        };
        match event {
            // Written on a thread of its own, so a slow reader holds up
            // nothing else; dropping the file closes it, which ends the data.
            wl_data_source::Event::Send { mime_type, fd } if mime_type == URI_LIST => {
                let list = drag.list.clone();
                thread::spawn(move || File::from(fd).write_all(&list));
            }
            wl_data_source::Event::DndFinished => {
                drag.end = Some(Outcome::Ended { dropped: true });
            }
            // Let go of where nothing took it, called off, or dropped on
            // the window, where egui took it.
            wl_data_source::Event::Cancelled => {
                drag.end = Some(if drag.dropped_here {
                    Outcome::Returned
                } else {
                    Outcome::Ended { dropped: false }
                });
            }
            _ => {}
        }
    }
}
