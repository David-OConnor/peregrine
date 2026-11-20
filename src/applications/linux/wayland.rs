use std::{collections::HashMap, env, os::unix::net::UnixStream, path::PathBuf};

use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    backend::ObjectId,
    event_created_child,
    globals::{GlobalListContents, registry_queue_init},
    protocol::wl_registry,
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1 as ext_handle, ext_foreign_toplevel_list_v1 as ext_list,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1 as wlr_handle, zwlr_foreign_toplevel_manager_v1 as wlr_list,
};

use super::Window;

/// Use only window lists the compositor advertises to regular clients.
pub(super) fn windows() -> Option<Vec<Window>> {
    // Do not consume a WAYLAND_SOCKET descriptor inherited by the application.
    let display =
        PathBuf::from(env::var_os("WAYLAND_DISPLAY").unwrap_or_else(|| "wayland-0".into()));
    let socket = if display.is_absolute() {
        display
    } else {
        PathBuf::from(env::var_os("XDG_RUNTIME_DIR")?).join(display)
    };
    let conn = Connection::from_socket(UnixStream::connect(socket).ok()?).ok()?;
    let (globals, mut queue) = registry_queue_init::<Events>(&conn).ok()?;
    let qh = queue.handle();
    // Use a separate connection so polling never waits on the GUI thread.
    let _ext = globals
        .bind::<ext_list::ExtForeignToplevelListV1, _, _>(&qh, 1..=1, ())
        .ok();
    let _wlr = if _ext.is_none() {
        Some(
            globals
                .bind::<wlr_list::ZwlrForeignToplevelManagerV1, _, _>(&qh, 1..=1, ())
                .ok()?,
        )
    } else {
        None
    };
    let mut events = Events::default();
    queue.roundtrip(&mut events).ok()?;
    if events.finished {
        return None;
    }
    Some(
        events
            .windows
            .into_iter()
            .filter_map(|(id, window)| {
                let (app_id, title) = window.committed?;
                Some(Window {
                    key: id.protocol_id(),
                    app_id,
                    title,
                })
            })
            .collect(),
    )
}

#[derive(Default)]
struct Toplevel {
    app_id: String,
    title: String,
    committed: Option<(String, String)>,
}

#[derive(Default)]
struct Events {
    windows: HashMap<ObjectId, Toplevel>,
    finished: bool,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Events {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ext_list::ExtForeignToplevelListV1, ()> for Events {
    fn event(
        state: &mut Self,
        _: &ext_list::ExtForeignToplevelListV1,
        event: ext_list::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_list::Event::Finished = event {
            state.finished = true;
        }
    }

    event_created_child!(Events, ext_list::ExtForeignToplevelListV1, [
        ext_list::EVT_TOPLEVEL_OPCODE => (ext_handle::ExtForeignToplevelHandleV1, ())
    ]);
}

impl Dispatch<wlr_list::ZwlrForeignToplevelManagerV1, ()> for Events {
    fn event(
        state: &mut Self,
        _: &wlr_list::ZwlrForeignToplevelManagerV1,
        event: wlr_list::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wlr_list::Event::Finished = event {
            state.finished = true;
        }
    }

    event_created_child!(Events, wlr_list::ZwlrForeignToplevelManagerV1, [
        wlr_list::EVT_TOPLEVEL_OPCODE => (wlr_handle::ZwlrForeignToplevelHandleV1, ())
    ]);
}

// Both protocols publish app ID and title with the same done/closed semantics.
macro_rules! handle_events {
    ($module:ident, $handle:ident) => {
        impl Dispatch<$module::$handle, ()> for Events {
            fn event(
                state: &mut Self,
                handle: &$module::$handle,
                event: $module::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                use $module::Event;
                if let Event::Closed = event {
                    state.windows.remove(&handle.id());
                    handle.destroy();
                    return;
                }
                let window = state.windows.entry(handle.id()).or_default();
                match event {
                    Event::AppId { app_id } => window.app_id = app_id,
                    Event::Title { title } => window.title = title,
                    Event::Done => {
                        window.committed = Some((window.app_id.clone(), window.title.clone()));
                    }
                    _ => {}
                }
            }
        }
    };
}

handle_events!(ext_handle, ExtForeignToplevelHandleV1);
handle_events!(wlr_handle, ZwlrForeignToplevelHandleV1);
