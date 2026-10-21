//! A terminal's screen, for full-screen programs (vim, nano, less, top), run
//! locally (see [crate::term]) or over SSH (see [crate::remote]): drawn cell
//! by cell from its emulator, with the keyboard going straight to the
//! program while it has focus. Text typed arrives as egui text events; other
//! keys are encoded as an xterm would (see `shell::ssh::keys`). Ctrl+C,
//! Ctrl+X and Ctrl+V reach egui as copy, cut and paste, so those are turned
//! back into what a terminal sends.

use egui::{
    Align2, Color32, Event, EventFilter, FontId, Id, Key, Modifiers, Rect, Sense, Stroke,
    TextStyle, Ui, pos2, vec2,
};
use shell::ssh::keys::{TermKey, TermMods, encode_key, encode_paste};

use crate::{ansi, ui::terminal::COLOR_STDOUT};

/// The screen widget's id, so other code can tell when it has focus.
pub fn grid_id() -> Id {
    Id::new("terminal_grid")
}

/// Whether the screen has keyboard focus, in which case keys are the
/// program's, not shortcuts.
pub fn has_focus(ctx: &egui::Context) -> bool {
    ctx.memory(|m| m.has_focus(grid_id()))
}

/// The size of one character cell in the monospace font.
fn cell_size(ui: &Ui, font: &FontId) -> egui::Vec2 {
    ui.fonts_mut(|fonts| {
        let width = fonts.glyph_width(font, 'M');
        let height = fonts
            .layout_no_wrap("Mg".into(), font.clone(), Color32::WHITE)
            .size()
            .y;
        vec2(width.max(1.), height.max(1.))
    })
}

/// How many character columns and rows fit the space left in `ui`.
pub fn size_in_cells(ui: &Ui) -> (u16, u16) {
    let font = TextStyle::Monospace.resolve(ui.style());
    let cell = cell_size(ui, &font);
    // Leave room for the scrollback's scroll bar.
    let avail = ui.available_size() - vec2(ui.spacing().scroll.bar_width + 4., 0.);
    let cols = (avail.x / cell.x).floor().clamp(1., u16::MAX as f32) as u16;
    let rows = (avail.y / cell.y).floor().clamp(1., u16::MAX as f32) as u16;
    (cols, rows)
}

/// Draw `screen`, and while it has focus, take the frame's keys for the
/// program. `take_focus` gives it focus, as a program that just took over
/// should have. `pending_paste` is text from the Clipboard panel's "Paste to
/// CLI". Returns what to send the program.
pub fn show(
    screen: &vt100::Screen,
    take_focus: bool,
    ui: &mut Ui,
    pending_paste: Option<String>,
) -> Vec<u8> {
    let font = TextStyle::Monospace.resolve(ui.style());
    let cell = cell_size(ui, &font);
    let rect = ui.available_rect_before_wrap();
    let response = ui.interact(rect, grid_id(), Sense::click());

    if response.clicked() || take_focus {
        response.request_focus();
    }
    let focused = response.has_focus();
    let mut bytes = Vec::new();
    if focused {
        // Tab, the arrows and Escape are the program's, not egui's focus
        // navigation.
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                grid_id(),
                EventFilter {
                    tab: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: true,
                },
            )
        });
        bytes = take_input(screen, ui);
    }
    if let Some(text) = pending_paste {
        bytes.extend(encode_paste(&text, screen.bracketed_paste()));
    }

    paint(screen, ui, rect, &font, cell, focused);
    bytes
}

/// Take this frame's keyboard events out of egui, encoded for the program.
fn take_input(screen: &vt100::Screen, ui: &mut Ui) -> Vec<u8> {
    let (app_cursor, bracketed) = (screen.application_cursor(), screen.bracketed_paste());
    let mut out = Vec::new();
    ui.input_mut(|input| {
        let mods = input.modifiers;
        input.events.retain(|event| {
            match event {
                Event::Text(text) => {
                    // Alt+key is ESC then the key, as in xterm's default.
                    if mods.alt && !mods.ctrl {
                        out.push(0x1b);
                    }
                    out.extend_from_slice(text.as_bytes());
                }
                Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => match key_bytes(*key, *modifiers, app_cursor) {
                    Some(bytes) => out.extend(bytes),
                    // Printable keys come as `Text` instead.
                    None => return true,
                },
                Event::Copy => out.push(0x03),
                Event::Cut => out.push(0x18),
                Event::Paste(text) => out.extend(encode_paste(text, bracketed)),
                _ => return true,
            }
            false
        });
    });
    out
}

/// The bytes for a key press other than plain typing.
fn key_bytes(key: Key, mods: Modifiers, app_cursor: bool) -> Option<Vec<u8>> {
    let term_mods = TermMods {
        ctrl: mods.ctrl,
        alt: mods.alt,
        shift: mods.shift,
    };
    let term_key = match key {
        Key::Enter => TermKey::Enter,
        Key::Backspace => TermKey::Backspace,
        Key::Tab => TermKey::Tab,
        Key::Escape => TermKey::Esc,
        Key::ArrowUp => TermKey::Up,
        Key::ArrowDown => TermKey::Down,
        Key::ArrowLeft => TermKey::Left,
        Key::ArrowRight => TermKey::Right,
        Key::Home => TermKey::Home,
        Key::End => TermKey::End,
        Key::Insert => TermKey::Insert,
        Key::Delete => TermKey::Delete,
        Key::PageUp => TermKey::PageUp,
        Key::PageDown => TermKey::PageDown,
        Key::F1 => TermKey::F(1),
        Key::F2 => TermKey::F(2),
        Key::F3 => TermKey::F(3),
        Key::F4 => TermKey::F(4),
        Key::F5 => TermKey::F(5),
        Key::F6 => TermKey::F(6),
        Key::F7 => TermKey::F(7),
        Key::F8 => TermKey::F(8),
        Key::F9 => TermKey::F(9),
        Key::F10 => TermKey::F(10),
        Key::F11 => TermKey::F(11),
        Key::F12 => TermKey::F(12),
        // Ctrl+letter and the like send no text event; encode them here.
        _ if mods.ctrl => TermKey::Char(ctrl_char(key)?),
        _ => return None,
    };
    encode_key(term_key, term_mods, app_cursor)
}

/// The character on a key, for Ctrl combinations.
fn ctrl_char(key: Key) -> Option<char> {
    let name = key.name();
    if name.len() == 1 {
        // A–Z and 0–9.
        return name.chars().next().map(|c| c.to_ascii_lowercase());
    }
    Some(match key {
        Key::Space => ' ',
        Key::OpenBracket => '[',
        Key::CloseBracket => ']',
        Key::Backslash => '\\',
        Key::Slash => '/',
        Key::Minus => '-',
        Key::Questionmark => '?',
        _ => return None,
    })
}

/// How one cell is drawn.
#[derive(Clone, Copy, PartialEq)]
struct CellStyle {
    fg: Color32,
    bg: Option<Color32>,
    underline: bool,
}

impl CellStyle {
    fn of(cell: &vt100::Cell) -> Self {
        let color = |c: vt100::Color| match c {
            vt100::Color::Default => None,
            vt100::Color::Idx(n) => Some(ansi::xterm256(n)),
            vt100::Color::Rgb(r, g, b) => Some(Color32::from_rgb(r, g, b)),
        };
        let mut fg = color(cell.fgcolor()).unwrap_or(COLOR_STDOUT);
        let mut bg = color(cell.bgcolor());
        if cell.bold() {
            fg = brighten(fg);
        }
        if cell.dim() {
            fg = fg.gamma_multiply(0.6);
        }
        if cell.inverse() {
            let back = bg.unwrap_or(Color32::BLACK);
            bg = Some(fg);
            fg = back;
        }
        Self {
            fg,
            bg,
            underline: cell.underline(),
        }
    }
}

/// Bold as brighter, as the scrollback shows it.
fn brighten(c: Color32) -> Color32 {
    let lift = |v: u8| ((u16::from(v) + 255) / 2) as u8;
    Color32::from_rgb(lift(c.r()), lift(c.g()), lift(c.b()))
}

fn paint(
    screen: &vt100::Screen,
    ui: &Ui,
    rect: Rect,
    font: &FontId,
    cell: egui::Vec2,
    focused: bool,
) {
    let (rows, cols) = screen.size();
    let painter = ui.painter_at(rect);

    // One run of same-styled text at a time, laid out in one go. A wide
    // character (CJK, emoji) gets a run of its own, so it can't push the
    // columns after it out of line.
    let paint_run = |row: u16, start: u16, width: u16, text: &str, style: CellStyle| {
        let min = rect.min + vec2(f32::from(start) * cell.x, f32::from(row) * cell.y);
        let run = Rect::from_min_size(min, vec2(f32::from(width) * cell.x, cell.y));
        if let Some(bg) = style.bg {
            painter.rect_filled(run, 0., bg);
        }
        if !text.trim().is_empty() {
            painter.text(min, Align2::LEFT_TOP, text, font.clone(), style.fg);
        }
        if style.underline {
            painter.line_segment(
                [
                    pos2(run.left(), run.bottom() - 1.),
                    pos2(run.right(), run.bottom() - 1.),
                ],
                Stroke::new(1., style.fg),
            );
        }
    };

    for row in 0..rows {
        let mut run = String::new();
        let mut run_start = 0;
        let mut run_width = 0;
        let mut run_style = None;
        let mut col = 0;
        while col < cols {
            let Some(c) = screen.cell(row, col) else {
                break;
            };
            if c.is_wide_continuation() {
                col += 1;
                continue;
            }
            let style = CellStyle::of(c);
            let wide = c.is_wide();
            if run_style != Some(style) || wide {
                if let Some(s) = run_style {
                    paint_run(row, run_start, run_width, &run, s);
                }
                run.clear();
                run_start = col;
                run_width = 0;
                run_style = Some(style);
            }
            run.push_str(if c.has_contents() { c.contents() } else { " " });
            let w = if wide { 2 } else { 1 };
            run_width += w;
            col += w;
            if wide {
                paint_run(row, run_start, run_width, &run, style);
                run.clear();
                run_style = None;
            }
        }
        if let Some(s) = run_style {
            paint_run(row, run_start, run_width, &run, s);
        }
    }

    if !screen.hide_cursor() {
        let (row, col) = screen.cursor_position();
        let at = Rect::from_min_size(
            rect.min + vec2(f32::from(col) * cell.x, f32::from(row) * cell.y),
            cell,
        );
        if focused {
            painter.rect_filled(at, 0., COLOR_STDOUT.gamma_multiply(0.6));
        } else {
            painter.rect_stroke(
                at,
                0.,
                Stroke::new(1., COLOR_STDOUT),
                egui::StrokeKind::Inside,
            );
        }
    }
}
