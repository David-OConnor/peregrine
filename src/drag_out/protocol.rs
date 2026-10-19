//! What's handed to programs taking files dragged out of the window on
//! Linux, as data: the file list they read, and the XDND messages X11
//! programs exchange about the drag. Kept apart from the X11 and Wayland
//! code so it's tested on every OS.

// Only the Linux code uses it; elsewhere it's built for its tests.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::fmt::Write;

/// The type of [uri_list]'s data, which file managers, browsers, editors
/// and terminals all read for dropped files.
pub const URI_LIST: &str = "text/uri-list";

/// `paths`' file list as programs read it from a drag (`text/uri-list`,
/// RFC 2483): each path's `file://` URI on a line of its own, ended with
/// CRLF. `paths` are absolute paths, as raw bytes: on Linux they needn't be
/// UTF-8.
pub fn uri_list<'a>(paths: impl IntoIterator<Item = &'a [u8]>) -> Vec<u8> {
    let mut list = String::new();
    for path in paths {
        list.push_str(&file_uri(path));
        list.push_str("\r\n");
    }
    list.into_bytes()
}

/// The `file://` URI for the absolute path `path`: every byte but letters,
/// digits, `-._~` and the `/` between names percent-encoded, so spaces,
/// `#`, `%` and names that aren't UTF-8 come through as they are.
pub fn file_uri(path: &[u8]) -> String {
    let mut uri = String::from("file://");
    for &b in path {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            uri.push(b as char);
        } else {
            let _ = write!(uri, "%{b:02X}");
        }
    }
    uri
}

/// The XDND version spoken: 5, the last, where the target says whether the
/// drop worked.
pub const XDND_VERSION: u32 = 5;

/// The oldest XDND version a target can speak and still be dragged to.
pub const XDND_OLDEST: u32 = 3;

/// What's asked of the target, from the modifier keys held, as in GTK and
/// Qt: Ctrl copies, Shift moves, both link, and neither leaves it to the
/// target, suggesting a copy. Nautilus, for one, then moves within a
/// drive, as Explorer does.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    Copy,
    Move,
    Link,
}

impl Action {
    pub fn from_keys(ctrl: bool, shift: bool) -> Self {
        match (ctrl, shift) {
            (true, true) => Self::Link,
            (false, true) => Self::Move,
            _ => Self::Copy,
        }
    }
}

/// The data of each XDND client message the source sends, and of those the
/// target answers with, as 32-bit words (`data.l[0..5]`). Windows and atoms
/// are X11 ids.
pub mod xdnd {
    /// `XdndEnter`: the pointer has come over the target, carrying data of
    /// `types` (up to three; more would go in `XdndTypeList`).
    pub fn enter(source: u32, version: u32, types: &[u32]) -> [u32; 5] {
        assert!(types.len() <= 3, "more than three types need XdndTypeList");
        let mut data = [source, version << 24, 0, 0, 0];
        data[2..2 + types.len()].copy_from_slice(types);
        data
    }

    /// `XdndPosition`: the pointer is at (`x`, `y`) on the screen, and
    /// `action` is asked for. `time` is the X server's.
    pub fn position(source: u32, x: i16, y: i16, time: u32, action: u32) -> [u32; 5] {
        let xy = (x as u16 as u32) << 16 | y as u16 as u32;
        [source, 0, xy, time, action]
    }

    /// `XdndLeave`: the pointer has gone, or the drag was called off.
    pub fn leave(source: u32) -> [u32; 5] {
        [source, 0, 0, 0, 0]
    }

    /// `XdndDrop`: let go of over the target, which should now ask for the
    /// data.
    pub fn drop(source: u32, time: u32) -> [u32; 5] {
        [source, 0, time, 0, 0]
    }

    /// The target's answer to a position (`XdndStatus`).
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct Status {
        /// The window it's from: the target.
        pub target: u32,
        /// Whether it would take the drop there.
        pub accepts: bool,
        /// What it would do with it: an `XdndAction…` atom.
        pub action: u32,
    }

    pub fn status(data: [u32; 5]) -> Status {
        Status {
            target: data[0],
            accepts: data[1] & 1 != 0,
            action: data[4],
        }
    }

    /// The target's word that it's done with the drop (`XdndFinished`):
    /// whether it took it. Before version 5 it doesn't say, so it's taken
    /// to have.
    pub fn finished(data: [u32; 5], version: u32) -> bool {
        version < 5 || data[1] & 1 != 0
    }
}
