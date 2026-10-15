//! Browser- and IDE-style tab shortcuts: new, close, reopen closed, next,
//! previous, and jump to a tab by number. They're taken out of the frame's
//! input before any widget runs, so the CLI input box doesn't also act on
//! them (egui's text edit deletes a word on Ctrl+W, for one).
//!
//! `Modifiers::COMMAND` is Ctrl on Windows and Linux, and Cmd on Mac.

use egui::{Context, Event, Key, KeyboardShortcut, Modifiers, os::OperatingSystem};

use crate::state::State;

pub const NEW_TAB: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::T);
pub const CLOSE_TAB: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::W);

/// Ctrl/Cmd + these select the first through eighth tab.
const TAB_NUMBER_KEYS: [Key; 8] = [
    Key::Num1,
    Key::Num2,
    Key::Num3,
    Key::Num4,
    Key::Num5,
    Key::Num6,
    Key::Num7,
    Key::Num8,
];

/// A tab command from the keyboard.
#[derive(Clone, Copy, Debug, PartialEq)]
enum TabAction {
    New,
    Close,
    Reopen,
    Next,
    Prev,
    /// The tab at this index.
    Select(usize),
    Last,
}

/// Act on the tab shortcuts pressed this frame, and remove their key events.
pub fn handle(state: &mut State, ctx: &Context) {
    let mac = ctx.os() == OperatingSystem::Mac;
    let actions = ctx.input_mut(|i| take_actions(&mut i.events, mac));
    for action in actions {
        apply(state, action);
    }
}

fn apply(state: &mut State, action: TabAction) {
    let n = state.cwd.len();
    let active = state.ui.active_tab;

    match action {
        TabAction::New => state.add_tab(),
        TabAction::Close => state.close_tab(active),
        TabAction::Reopen => state.reopen_closed_tab(),
        TabAction::Next => state.select_tab((active + 1) % n),
        TabAction::Prev => state.select_tab((active + n - 1) % n),
        // A number past the last tab does nothing, as in a browser.
        TabAction::Select(i) => state.select_tab(i),
        TabAction::Last => state.select_tab(n - 1),
    }
}

/// Remove the key presses in `events` that are tab shortcuts, returning what
/// they ask for in order. Holding a key down keeps stepping through tabs,
/// but doesn't open or close a run of them.
fn take_actions(events: &mut Vec<Event>, mac: bool) -> Vec<TabAction> {
    let mut actions = Vec::new();

    events.retain(|event| {
        let Event::Key {
            key,
            pressed: true,
            repeat,
            modifiers,
            ..
        } = event
        else {
            return true;
        };
        let Some(action) = action_for(*key, *modifiers, mac) else {
            return true;
        };

        if !repeat || matches!(action, TabAction::Next | TabAction::Prev) {
            actions.push(action);
        }
        false
    });

    actions
}

/// The tab command `key` + `modifiers` stands for, if any. `mac` adds
/// Cmd+Option+Left/Right. Modifiers must match exactly, so an AltGr
/// character (Ctrl+Alt on Windows) still types.
fn action_for(key: Key, modifiers: Modifiers, mac: bool) -> Option<TabAction> {
    let is = |pattern: Modifiers| modifiers.matches_exact(pattern);
    let cmd = Modifiers::COMMAND;
    let ctrl = Modifiers::CTRL;

    let action = match key {
        Key::T if is(cmd | Modifiers::SHIFT) => TabAction::Reopen,
        Key::T | Key::N if is(cmd) => TabAction::New,
        Key::W if is(cmd) => TabAction::Close,
        Key::Tab | Key::PageDown if is(ctrl) => TabAction::Next,
        Key::Tab if is(ctrl | Modifiers::SHIFT) => TabAction::Prev,
        Key::PageUp if is(ctrl) => TabAction::Prev,
        Key::ArrowRight if mac && is(cmd | Modifiers::ALT) => TabAction::Next,
        Key::ArrowLeft if mac && is(cmd | Modifiers::ALT) => TabAction::Prev,
        Key::Num9 if is(cmd) => TabAction::Last,
        _ if is(cmd) => TabAction::Select(TAB_NUMBER_KEYS.iter().position(|&k| k == key)?),
        _ => return None,
    };

    Some(action)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ctrl as egui-winit reports it on Windows and Linux: `command` too.
    const CTRL: Modifiers = Modifiers {
        ctrl: true,
        command: true,
        ..Modifiers::NONE
    };
    const CTRL_SHIFT: Modifiers = Modifiers {
        shift: true,
        ..CTRL
    };
    /// Cmd as egui-winit reports it on Mac: `command` too.
    const MAC_CMD: Modifiers = Modifiers {
        mac_cmd: true,
        command: true,
        ..Modifiers::NONE
    };
    const MAC_CMD_OPTION: Modifiers = Modifiers {
        alt: true,
        ..MAC_CMD
    };
    /// Ctrl on Mac, which isn't `command` there.
    const MAC_CTRL: Modifiers = Modifiers::CTRL;

    fn press(key: Key, modifiers: Modifiers, repeat: bool) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat,
            modifiers,
        }
    }

    #[test]
    fn maps_windows_and_linux_keys() {
        let a = |key, m| action_for(key, m, false);
        assert_eq!(a(Key::T, CTRL), Some(TabAction::New));
        assert_eq!(a(Key::N, CTRL), Some(TabAction::New));
        assert_eq!(a(Key::W, CTRL), Some(TabAction::Close));
        assert_eq!(a(Key::T, CTRL_SHIFT), Some(TabAction::Reopen));
        assert_eq!(a(Key::Tab, CTRL), Some(TabAction::Next));
        assert_eq!(a(Key::Tab, CTRL_SHIFT), Some(TabAction::Prev));
        assert_eq!(a(Key::PageDown, CTRL), Some(TabAction::Next));
        assert_eq!(a(Key::PageUp, CTRL), Some(TabAction::Prev));
        assert_eq!(a(Key::Num1, CTRL), Some(TabAction::Select(0)));
        assert_eq!(a(Key::Num8, CTRL), Some(TabAction::Select(7)));
        assert_eq!(a(Key::Num9, CTRL), Some(TabAction::Last));
    }

    #[test]
    fn maps_mac_keys() {
        let a = |key, m| action_for(key, m, true);
        assert_eq!(a(Key::T, MAC_CMD), Some(TabAction::New));
        assert_eq!(a(Key::W, MAC_CMD), Some(TabAction::Close));
        assert_eq!(a(Key::T, MAC_CMD | Modifiers::SHIFT), Some(TabAction::Reopen));
        assert_eq!(a(Key::ArrowRight, MAC_CMD_OPTION), Some(TabAction::Next));
        assert_eq!(a(Key::ArrowLeft, MAC_CMD_OPTION), Some(TabAction::Prev));
        assert_eq!(a(Key::Tab, MAC_CTRL), Some(TabAction::Next));
        assert_eq!(a(Key::Num3, MAC_CMD), Some(TabAction::Select(2)));
        // Ctrl isn't Cmd on Mac.
        assert_eq!(a(Key::T, MAC_CTRL), None);
    }

    #[test]
    fn ignores_other_keys() {
        let alt_gr = Modifiers {
            alt: true,
            ..CTRL
        };
        // AltGr+7 is `{` on a German layout, not a tab jump.
        assert_eq!(action_for(Key::Num7, alt_gr, false), None);
        // Cmd+Option+Arrow is Mac only.
        assert_eq!(action_for(Key::ArrowRight, CTRL | Modifiers::ALT, false), None);
        assert_eq!(action_for(Key::Num0, CTRL, false), None);
        assert_eq!(action_for(Key::Tab, Modifiers::NONE, false), None);
        assert_eq!(action_for(Key::T, Modifiers::NONE, false), None);
    }

    #[test]
    fn takes_shortcuts_and_leaves_other_events() {
        let mut events = vec![
            press(Key::A, Modifiers::NONE, false),
            press(Key::Tab, CTRL, false),
            Event::Text("x".into()),
            press(Key::Tab, CTRL, true),
            press(Key::W, CTRL, false),
            press(Key::W, CTRL, true),
        ];
        let actions = take_actions(&mut events, false);
        assert_eq!(
            actions,
            [TabAction::Next, TabAction::Next, TabAction::Close]
        );
        assert_eq!(
            events,
            [press(Key::A, Modifiers::NONE, false), Event::Text("x".into())]
        );
    }
}
