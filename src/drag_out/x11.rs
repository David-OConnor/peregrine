//! Dragging files out of the window on X11: the source side of the XDND
//! protocol (version 5), offering the files as `text/uri-list`, as a file
//! manager's drag does.
//!
//! A drag source usually grabs the pointer, but winit already has it: the
//! press the drag began with grabbed it for the window until the button's
//! let go of (an X server's implicit grab). That's no loss, as winit still
//! sees the pointer leave and come back, and the button let go of, wherever
//! that happens, so egui keeps up. Instead the drag runs on a thread of its
//! own, over a connection of its own, asking the X server where the pointer
//! is every few milliseconds, finding the window under it that takes drops
//! (`XdndAware`), and telling that window about the drag. An unmapped window
//! of ours owns the drag's selection (`XdndSelection`), and gives the files
//! to whatever asks for them.

use std::{
    error::Error,
    thread,
    time::{Duration, Instant},
};

use x11rb::{
    COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, NONE,
    connection::Connection,
    cookie::VoidCookie,
    errors::{ConnectionError, ReplyError},
    protocol::{
        Event,
        xproto::{
            Atom, AtomEnum, ClientMessageEvent, ConnectionExt, CreateWindowAux, EventMask,
            KeyButMask, PropMode, SELECTION_NOTIFY_EVENT, SelectionNotifyEvent,
            SelectionRequestEvent, WindowClass,
        },
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

use super::{
    Outcome,
    linux::{Drag, Feedback, Shared},
    protocol::{Action, XDND_OLDEST, XDND_VERSION, xdnd},
};

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        XdndAware,
        XdndProxy,
        XdndSelection,
        XdndEnter,
        XdndPosition,
        XdndStatus,
        XdndLeave,
        XdndDrop,
        XdndFinished,
        XdndActionCopy,
        XdndActionMove,
        XdndActionLink,
        XdndActionList,
        TARGETS,
        _NET_WM_PID,
        URI_LIST: b"text/uri-list",
    }
}

type Res<T> = Result<T, Box<dyn Error>>;

/// How often the pointer is looked at.
const TICK: Duration = Duration::from_millis(10);
/// How long the program under the pointer has to answer where it was let
/// go of, before the drop is called off.
const STATUS_WAIT: Duration = Duration::from_secs(1);
/// How long it has to say it's done with a drop, before it's taken to be.
const FINISH_WAIT: Duration = Duration::from_secs(10);
/// How many windows deep to look for one that takes drops: past a window
/// manager's frame or two to a program's window, and then some.
const MAX_DEPTH: usize = 8;

/// Drag `list`, a `text/uri-list`, out of `window` (the main window's X11
/// window) on a thread of its own.
pub fn start(window: u32, list: Vec<u8>, ctx: &egui::Context) -> Drag {
    Drag::spawn(ctx, move |shared| {
        run(window, &list, shared).map_err(|e| format!("drag: {e}"))
    })
}

fn run(main: u32, list: &[u8], shared: &Shared) -> Res<Outcome> {
    let (conn, screen) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen].root;
    let atoms = Atoms::new(&conn)?.reply()?;
    let source = conn.generate_id()?;
    conn.create_window(
        COPY_DEPTH_FROM_PARENT,
        source,
        root,
        -10,
        -10,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        COPY_FROM_PARENT,
        &CreateWindowAux::new()
            .override_redirect(1)
            .event_mask(EventMask::PROPERTY_CHANGE),
    )?;
    let mut drag = XDrag {
        conn,
        atoms,
        root,
        main,
        source,
        pid: std::process::id(),
        list,
        time: x11rb::CURRENT_TIME,
        target: None,
        dropped: false,
        finished: None,
        lost: false,
    };
    let outcome = drag.run(shared);
    // Which gives up the selection too.
    let _ = drag.conn.destroy_window(source);
    let _ = drag.conn.flush();
    outcome
}

/// A drag going on, on its own connection.
struct XDrag<'a> {
    conn: RustConnection,
    atoms: Atoms,
    root: u32,
    /// The main window, which the drag is handed back to.
    main: u32,
    /// Our window owning the selection, which the messages come from.
    source: u32,
    /// Ours, as other windows of ours say (`_NET_WM_PID`).
    pid: u32,
    /// The files, as `text/uri-list`.
    list: &'a [u8],
    /// The X server's time when we took the selection, which the messages
    /// give.
    time: u32,
    /// The window under the pointer that takes drops, if any.
    target: Option<Target>,
    /// Whether it's been sent the drop.
    dropped: bool,
    /// Its word that it's done with the drop: whether it took it.
    finished: Option<bool>,
    /// Another program took the selection, which ends the drag.
    lost: bool,
}

/// What's under the pointer.
enum Under {
    /// Our main window.
    Ours,
    /// Nothing that takes drops.
    Nothing,
    Target(TargetInfo),
}

/// A window that takes drops.
#[derive(Clone, Copy, PartialEq)]
struct TargetInfo {
    /// The window under the pointer, which the messages are about.
    window: u32,
    /// Where they're sent: it, or its proxy.
    send_to: u32,
    /// The XDND version spoken with it.
    version: u32,
}

/// The window under the pointer, and what's been said with it.
struct Target {
    info: TargetInfo,
    /// The last position sent hasn't been answered yet.
    waiting: bool,
    /// The last answer.
    status: Option<xdnd::Status>,
    /// The last position sent, and the action asked for.
    sent: Option<((i16, i16), Atom)>,
}

impl XDrag<'_> {
    fn run(&mut self, shared: &Shared) -> Res<Outcome> {
        self.own_selection()?;
        let mut looked_at = None;
        let mut under = Under::Nothing;
        loop {
            self.handle_events()?;
            if self.lost || shared.cancelled() {
                self.leave()?;
                return Ok(Outcome::Ended { dropped: false });
            }

            let pointer = self.conn.query_pointer(self.root)?.reply()?;
            let at = (pointer.root_x, pointer.root_y);
            // Windows don't often move under a pointer that's still.
            if looked_at != Some(at) {
                under = if pointer.same_screen {
                    self.under(at)?
                } else {
                    Under::Nothing
                };
                looked_at = Some(at);
            }
            let info = match under {
                Under::Ours => {
                    self.leave()?;
                    return Ok(Outcome::Returned);
                }
                Under::Nothing => None,
                Under::Target(info) => Some(info),
            };
            if self.target.as_ref().map(|t| t.info) != info {
                self.leave()?;
                if let Some(info) = info {
                    self.enter(info)?;
                }
            }

            if !pointer.mask.contains(KeyButMask::BUTTON1) {
                break;
            }
            let action = Action::from_keys(
                pointer.mask.contains(KeyButMask::CONTROL),
                pointer.mask.contains(KeyButMask::SHIFT),
            );
            self.position(at, action)?;
            shared.set_feedback(self.feedback());
            self.conn.flush()?;
            thread::sleep(TICK);
        }
        shared.set_feedback(Feedback::Unknown);
        self.let_go()
    }

    /// Own the drag's selection, and offer the actions a target may take
    /// (`XdndActionList`). Setting that also tells us the X server's time,
    /// as the property change it makes carries it: taking the selection at
    /// the current time, as a fixed one, keeps programs that check from
    /// asking for the files at a time before we had them.
    fn own_selection(&mut self) -> Res<()> {
        let a = &self.atoms;
        let actions = [a.XdndActionCopy, a.XdndActionMove, a.XdndActionLink];
        self.conn.change_property32(
            PropMode::REPLACE,
            self.source,
            a.XdndActionList,
            AtomEnum::ATOM,
            &actions,
        )?;
        self.conn.flush()?;
        self.time = loop {
            if let Event::PropertyNotify(e) = self.conn.wait_for_event()?
                && e.window == self.source
            {
                break e.time;
            }
        };
        let selection = self.atoms.XdndSelection;
        self.conn
            .set_selection_owner(self.source, selection, self.time)?;
        if self.conn.get_selection_owner(selection)?.reply()?.owner != self.source {
            return Err("can't take the drag's selection".into());
        }
        Ok(())
    }

    /// Take in what's come from the X server: targets' answers, and their
    /// requests for the files.
    fn handle_events(&mut self) -> Res<()> {
        while let Some(event) = self.conn.poll_for_event()? {
            match event {
                Event::ClientMessage(m) if m.type_ == self.atoms.XdndStatus => {
                    let status = xdnd::status(m.data.as_data32());
                    if let Some(t) = &mut self.target {
                        // From it (or its proxy), or the answer it owes
                        // anyway: some answer from a window of their own.
                        let from = [t.info.window, t.info.send_to].contains(&status.target);
                        if from || t.waiting {
                            t.waiting = false;
                            t.status = Some(status);
                        }
                    }
                }
                Event::ClientMessage(m) if m.type_ == self.atoms.XdndFinished => {
                    if let (true, Some(t)) = (self.dropped, &self.target) {
                        self.finished = Some(xdnd::finished(m.data.as_data32(), t.info.version));
                    }
                }
                Event::SelectionRequest(r) => self.answer(r)?,
                Event::SelectionClear(c) if c.selection == self.atoms.XdndSelection => {
                    self.lost = true;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Give the files to a program asking for them (or say what they can
    /// be had as, for `TARGETS`), by setting the property it names on its
    /// window and telling it so.
    fn answer(&self, r: SelectionRequestEvent) -> Res<()> {
        let a = &self.atoms;
        // Old programs name none, leaving it to the type asked for.
        let property = if r.property == NONE {
            r.target
        } else {
            r.property
        };
        let given = r.selection == a.XdndSelection
            && if r.target == a.TARGETS {
                set(self.conn.change_property32(
                    PropMode::REPLACE,
                    r.requestor,
                    property,
                    AtomEnum::ATOM,
                    &[a.TARGETS, a.URI_LIST],
                ))?
            } else if r.target == a.URI_LIST {
                set(self.conn.change_property8(
                    PropMode::REPLACE,
                    r.requestor,
                    property,
                    a.URI_LIST,
                    self.list,
                ))?
            } else {
                false
            };
        let notify = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: r.time,
            requestor: r.requestor,
            selection: r.selection,
            target: r.target,
            property: if given { property } else { NONE },
        };
        self.conn
            .send_event(false, r.requestor, EventMask::NO_EVENT, notify)?;
        self.conn.flush()?;
        Ok(())
    }

    /// What's under the screen point `at`. Under the root window are the
    /// window managers' frames, and in those the programs' windows, which
    /// say whether they take drops; with no frames, they're right under the
    /// root. Should nothing be over it, a desktop drawn on the root window
    /// itself may take them (through a proxy).
    fn under(&self, (x, y): (i16, i16)) -> Res<Under> {
        let mut window = match self.child_at(self.root, x, y)? {
            Some(NONE) => {
                let info = self.aware(self.root)?;
                return Ok(info.map_or(Under::Nothing, Under::Target));
            }
            Some(child) => child,
            None => return Ok(Under::Nothing),
        };
        for _ in 0..MAX_DEPTH {
            if window == self.main {
                return Ok(Under::Ours);
            }
            if let Some(info) = self.aware(window)? {
                // Another window of ours (a viewer) isn't one to drop on.
                return Ok(if self.is_ours(window)? {
                    Under::Nothing
                } else {
                    Under::Target(info)
                });
            }
            match self.child_at(window, x, y)? {
                Some(NONE) | None => break,
                Some(child) => window = child,
            }
        }
        Ok(Under::Nothing)
    }

    /// The child of `window` the screen point (`x`, `y`) is in (`NONE` for
    /// none), or `None` when `window`'s gone.
    fn child_at(&self, window: u32, x: i16, y: i16) -> Res<Option<u32>> {
        let reply = self
            .conn
            .translate_coordinates(self.root, window, x, y)?
            .reply();
        Ok(gone_is_none(reply)?.map(|r| r.child))
    }

    /// Whether `window` takes drops (`XdndAware`, at a version we speak),
    /// itself or through a proxy window (`XdndProxy`), as a desktop often
    /// does, and where to send it messages if so.
    fn aware(&self, window: u32) -> Res<Option<TargetInfo>> {
        let a = &self.atoms;
        // A real proxy names itself too; otherwise it's one left over from
        // a program that's gone.
        let send_to = match self.word(window, a.XdndProxy)? {
            Some(proxy) if self.word(proxy, a.XdndProxy)? == Some(proxy) => proxy,
            _ => window,
        };
        Ok(match self.word(send_to, a.XdndAware)? {
            Some(version) if version >= XDND_OLDEST => Some(TargetInfo {
                window,
                send_to,
                version: version.min(XDND_VERSION),
            }),
            _ => None,
        })
    }

    /// Whether `window` is one of this program's.
    fn is_ours(&self, window: u32) -> Res<bool> {
        Ok(self.word(window, self.atoms._NET_WM_PID)? == Some(self.pid))
    }

    /// The first 32-bit word of `window`'s `property`, if it has one.
    fn word(&self, window: u32, property: Atom) -> Res<Option<u32>> {
        let reply = self
            .conn
            .get_property(false, window, property, AtomEnum::ANY, 0, 1)?
            .reply();
        Ok(gone_is_none(reply)?.and_then(|r| r.value32()?.next()))
    }

    /// The pointer has come over `info`'s window.
    fn enter(&mut self, info: TargetInfo) -> Res<()> {
        let data = xdnd::enter(self.source, info.version, &[self.atoms.URI_LIST]);
        self.send(info, self.atoms.XdndEnter, data)?;
        self.target = Some(Target {
            info,
            waiting: false,
            status: None,
            sent: None,
        });
        Ok(())
    }

    /// The pointer has left the target (if any), or the drag's over.
    fn leave(&mut self) -> Res<()> {
        if let Some(t) = self.target.take() {
            self.send(t.info, self.atoms.XdndLeave, xdnd::leave(self.source))?;
            self.conn.flush()?;
        }
        Ok(())
    }

    /// Tell the target where the pointer is, at `at`, and the action asked
    /// for: once its last answer's in, and only if either has changed.
    fn position(&mut self, at: (i16, i16), action: Action) -> Res<()> {
        let action = match action {
            Action::Copy => self.atoms.XdndActionCopy,
            Action::Move => self.atoms.XdndActionMove,
            Action::Link => self.atoms.XdndActionLink,
        };
        let Some(t) = &mut self.target else {
            return Ok(());
        };
        if t.waiting || t.sent == Some((at, action)) {
            return Ok(());
        }
        t.waiting = true;
        t.sent = Some((at, action));
        let info = t.info;
        let data = xdnd::position(self.source, at.0, at.1, self.time, action);
        self.send(info, self.atoms.XdndPosition, data)
    }

    /// What the target's last answer said it would do with the files.
    fn feedback(&self) -> Feedback {
        let a = &self.atoms;
        match &self.target {
            None => Feedback::NoDrop,
            Some(Target { status: None, .. }) => Feedback::Unknown,
            Some(Target {
                status: Some(s), ..
            }) => match s.action {
                _ if !s.accepts => Feedback::NoDrop,
                act if act == a.XdndActionMove => Feedback::Move,
                act if act == a.XdndActionLink => Feedback::Link,
                _ => Feedback::Copy,
            },
        }
    }

    /// The button's been let go of: drop the files on the target, if it
    /// takes them there, and wait for it to say it's done.
    fn let_go(&mut self) -> Res<Outcome> {
        // Its answer to the last position may still be on its way.
        let deadline = Instant::now() + STATUS_WAIT;
        while self.target.as_ref().is_some_and(|t| t.waiting) && Instant::now() < deadline {
            thread::sleep(TICK);
            self.handle_events()?;
        }
        let accepted = self
            .target
            .as_ref()
            .filter(|t| !t.waiting)
            .and_then(|t| t.status)
            .is_some_and(|s| s.accepts);
        let (true, Some(t)) = (accepted, &self.target) else {
            self.leave()?;
            return Ok(Outcome::Ended { dropped: false });
        };

        let info = t.info;
        self.send(
            info,
            self.atoms.XdndDrop,
            xdnd::drop(self.source, self.time),
        )?;
        self.conn.flush()?;
        self.dropped = true;
        // It asks for the files ([Self::answer]) before it's done.
        let deadline = Instant::now() + FINISH_WAIT;
        loop {
            self.handle_events()?;
            if let Some(took) = self.finished {
                return Ok(Outcome::Ended { dropped: took });
            }
            if self.lost || Instant::now() >= deadline {
                return Ok(Outcome::Ended { dropped: true });
            }
            thread::sleep(TICK);
        }
    }

    /// Send an XDND message to `to`: to its proxy if it has one, though it's
    /// about the window under the pointer.
    fn send(&self, to: TargetInfo, kind: Atom, data: [u32; 5]) -> Res<()> {
        let message = ClientMessageEvent::new(32, to.window, kind, data);
        self.conn
            .send_event(false, to.send_to, EventMask::NO_EVENT, message)?;
        Ok(())
    }
}

/// A reply, or `None` when the request failed on the X server's side, as
/// when the window it's about has just gone.
fn gone_is_none<T>(reply: Result<T, ReplyError>) -> Res<Option<T>> {
    match reply {
        Ok(r) => Ok(Some(r)),
        Err(ReplyError::X11Error(_)) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Whether a property was set, from what setting it returned: one too big
/// to set in one go is refused (handing it over in parts, with `INCR`,
/// isn't done), not a failure.
fn set(set: Result<VoidCookie<'_, RustConnection>, ConnectionError>) -> Res<bool> {
    match set {
        Ok(_) => Ok(true),
        Err(ConnectionError::MaximumRequestLengthExceeded) => Ok(false),
        Err(e) => Err(e.into()),
    }
}
