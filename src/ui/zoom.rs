//! Ctrl + `+` / `-` and Ctrl+scroll step a font size by a point (Cmd on Mac):
//! the terminal's ([State::font_size]) when the terminal is what's typed in or
//! pointed at, otherwise the GUI's ([State::gui_font_size]).

use egui::{Context, Event, Id, Key, Modifiers, MouseWheelUnit, Order, Pos2};

use super::terminal;
use crate::state::State;

/// Touchpad scrolling, in points, that makes one step: about a wheel notch.
const POINTS_PER_STEP: f32 = 40.;

/// Font-size steps asked for this frame: up when positive.
#[derive(Default)]
struct Steps {
    keys: i32,
    wheel: i32,
}

/// Act on this frame's zoom keys and Ctrl+scroll in the main window. Keys go
/// to the terminal while it has the keyboard; scrolling, while the pointer is
/// over it.
pub fn handle(state: &mut State, ctx: &Context) {
    let steps = take_steps(ctx);

    if steps.keys != 0 {
        let to_terminal = keys_to_terminal(state, ctx);
        step(state, to_terminal, steps.keys);
    }
    if steps.wheel != 0 {
        let to_terminal = ctx
            .pointer_latest_pos()
            .is_some_and(|p| over_terminal(state, ctx, p));
        step(state, to_terminal, steps.wheel);
    }
}

/// [handle] for a window of its own with no terminal in it.
pub fn handle_gui(state: &mut State, ctx: &Context) {
    let steps = take_steps(ctx);

    step(state, false, steps.keys + steps.wheel);
}

fn step(state: &mut State, terminal: bool, n: i32) {
    if n == 0 {
        return;
    }

    if terminal {
        state.adjust_font_size(n as f32);
    } else {
        state.adjust_gui_font_size(n as f32);
    }
}

/// Zoom modifiers: Ctrl (Cmd on Mac), without Alt, so AltGr keys type.
fn zoom_mods(m: Modifiers) -> bool {
    m.command && !m.alt
}

/// Count and remove this frame's zoom key presses (repeats included) and
/// Ctrl+scroll events, so neither egui's own zoom nor a widget acts on them.
/// Ctrl+Shift+`-` is left alone: it's undo in the CLI input.
fn take_steps(ctx: &Context) -> Steps {
    // Scrolling short of a step carries over to the next frame.
    let carry_id = Id::new("font_zoom_carry");
    let mut carry: f32 = ctx.data(|d| d.get_temp(carry_id)).unwrap_or_default();
    let mut steps = Steps::default();

    ctx.input_mut(|i| {
        i.events.retain(|event| match event {
            Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } if zoom_mods(*modifiers) => match key {
                Key::Plus | Key::Equals => {
                    steps.keys += 1;
                    false
                }
                Key::Minus if !modifiers.shift => {
                    steps.keys -= 1;
                    false
                }
                _ => true,
            },
            Event::MouseWheel {
                unit,
                delta,
                modifiers,
                ..
            } if zoom_mods(*modifiers) => {
                let notches = match unit {
                    MouseWheelUnit::Point => delta.y / POINTS_PER_STEP,
                    MouseWheelUnit::Line | MouseWheelUnit::Page => delta.y,
                };
                // Reversing starts afresh.
                if carry * notches < 0. {
                    carry = 0.;
                }
                carry += notches;
                false
            }
            _ => true,
        });
    });

    steps.wheel = carry.trunc() as i32;
    carry -= steps.wheel as f32;
    ctx.data_mut(|d| d.insert_temp(carry_id, carry));

    steps
}

/// Whether zoom keys are for the terminal: it has the keyboard, or nothing
/// does and the pointer's over it.
fn keys_to_terminal(state: &State, ctx: &Context) -> bool {
    if terminal::has_focus(ctx) {
        return true;
    }
    if ctx.memory(|m| m.focused().is_some()) {
        return false;
    }

    ctx.pointer_latest_pos()
        .is_some_and(|p| over_terminal(state, ctx, p))
}

/// Whether `pos` is on the terminal pane or the CLI font control, as last
/// drawn, with no window or popup over it.
fn over_terminal(state: &State, ctx: &Context, pos: Pos2) -> bool {
    let uncovered = ctx
        .layer_id_at(pos)
        .is_none_or(|l| l.order == Order::Background);

    uncovered && (state.ui.term_rect.contains(pos) || state.ui.cli_font_rect.contains(pos))
}
