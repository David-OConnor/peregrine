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
