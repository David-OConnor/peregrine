//! Variable-height scrollback virtualization. Heights are measured once with
//! uncached layout; only visible lines enter egui's much larger galley cache.

use std::sync::Arc;

use egui::{
    Color32, Context, CursorIcon, Event, FontId, Galley, Id, Key, KeyboardShortcut, Modifiers,
    Rect, RichText, ScrollArea, Sense, TextEdit, TextStyle, Ui,
    os::OperatingSystem,
    text::CCursor,
    text_selection::{CCursorRange, TextCursorState, visuals::paint_text_selection},
    vec2,
};
use shell::DirList;

use super::{
    COLOR_BRANCH, COLOR_HIS, clipboard, grid,
    line_edit::{self, Edit, LineKey},
    scratchpad,
};
use crate::{
    OutType, path_color,
    path_color::Part,
    remote::Remote,
    state::State,
    submit_command,
    terminal::{Cursor, Line, Metrics, Output, Selection},
};

fn color(kind: OutType) -> Color32 {
    match kind {
        OutType::Prompt => COLOR_PROMPT,
        OutType::StdOut => COLOR_STDOUT,
        OutType::StdErr => COLOR_STDERR,
        OutType::StdErrOk => COLOR_STDERR_OK,
    }
}

fn galley(ui: &Ui, line: &Line, font: &FontId, width: f32) -> Arc<Galley> {
    ui.fonts_mut(|fonts| fonts.layout_job(line.job(font, color(line.kind), width)))
}

fn measure(out: &mut Output, ui: &Ui, font: &FontId, width: f32) {
    let metrics = Metrics {
        font: font.clone(),
        width,
        pixels_per_point: ui.ctx().pixels_per_point(),
        spacing: ui.spacing().item_spacing.y,
    };
    if out.metrics.as_ref() != Some(&metrics) {
        for line in &mut out.lines {
            line.height = None;
        }
        out.metrics = Some(metrics);
    }
    out.offsets.clear();
    out.offsets.push(0.);
    let ppp = ui.ctx().pixels_per_point();
    let ascii_width = ui.fonts_mut(|fonts| {
        (b' '..=b'~')
            .map(|c| fonts.glyph_width(font, c as char))
            .fold(0., f32::max)
    });
    // Use a laid-out sample so baseline/height rounding matches the galley,
    // including fractional font sizes and display scale factors.
    let row_height = ui.fonts_mut(|fonts| {
        fonts
            .layout_no_wrap("Mg".into(), font.clone(), Color32::WHITE)
            .size()
            .y
    });

    for line in &mut out.lines {
        if line.height.is_none() {
            if line.text.len() as f32 * ascii_width + 2. <= width
                && line.text.bytes().all(|b| (b' '..=b'~').contains(&b))
            {
                // A conservative width bound proves these lines cannot wrap.
                line.height = Some(row_height);
            } else {
                let job = Arc::new(line.job(font, color(line.kind), width));
                // Measuring all retained rows through FontsView::layout_job
                // would cache every offscreen glyph/mesh as well.
                line.height =
                    Some(ui.fonts_mut(|fonts| {
                        egui::epaint::text::layout(fonts.fonts, ppp, job).size().y
                    }));
            }
        }

        let height = line.height.unwrap();
        out.offsets
            .push(out.offsets.last().unwrap() + height + ui.spacing().item_spacing.y);
    }

    out.first_measured = out.lines.front().map(|line| line.id);
}

fn row_at(out: &Output, y: f32) -> usize {
    out.offsets
        .partition_point(|&at| at <= y)
        .saturating_sub(1)
        .min(out.lines.len().saturating_sub(1))
}

pub(super) fn show(out: &mut Output, ui: &mut Ui) {
    if out.truncated {
        ui.label(
            egui::RichText::new("Older output discarded (scrollback limit).")
                .small()
                .weak(),
        );
    }

    let font = TextStyle::Monospace.resolve(ui.style());
    let scroll_salt = ("term_out_scroll", out.id);
    let mut scroll = ScrollArea::vertical()
        .id_salt(scroll_salt)
        .auto_shrink([false, false])
        .stick_to_bottom(true);
    if let (Some(previous), Some(first)) = (out.first_measured, out.lines.front())
        && first.id > previous
    {
        // Keep the same retained text in view when pruning the front while
        // the user is reading older output. Sticky-tail behavior still wins
        // for a viewport that was following the newest output.
        let removed = (first.id - previous) as usize;
        let height = out
            .offsets
            .get(removed)
            .or(out.offsets.last())
            .copied()
            .unwrap_or(0.);

        if let Some(state) = egui::scroll_area::State::load(
            ui.ctx(),
            ui.make_persistent_id(egui::IdSalt::new(scroll_salt)),
        ) {
            scroll = scroll.vertical_scroll_offset((state.offset.y - height).max(0.));
        }
    }
    let output = scroll.show_viewport(ui, |ui, viewport| {
        let width = ui.available_width().max(1.);
        measure(out, ui, &font, width);

        let height =
            (out.offsets.last().copied().unwrap_or(0.) - ui.spacing().item_spacing.y).max(0.);
        let origin = ui.max_rect().min;
        ui.set_min_height(height);
        let visible_rect = Rect::from_min_max(
            origin + viewport.min.to_vec2(),
            origin + viewport.max.to_vec2(),
        )
        .intersect(ui.clip_rect());

        let response = ui
            .interact(
                Rect::from_min_size(origin, vec2(width, height.max(viewport.height()))),
                ui.id().with(("terminal_selection", out.id)),
                Sense::click_and_drag() - Sense::FOCUSABLE,
            )
            .on_hover_cursor(CursorIcon::Text);

        let pressed = ui.input(|i| i.pointer.primary_pressed());
        let down = ui.input(|i| i.pointer.primary_down());
        let pointer = ui.input(|i| i.pointer.interact_pos());
        if pressed && !response.hovered() {
            out.selection = None;
            out.dragging = false;
        }

        if !out.lines.is_empty() {
            if let Some(pointer) =
                pointer.filter(|_| (pressed && response.hovered()) || out.dragging)
            {
                // Clamp to the viewport while dragging outside it; scrolling
                // below brings further rows into the next bounded pass.
                let pointer = visible_rect.clamp(pointer);
                let i = row_at(out, pointer.y - origin.y);
                let line = &out.lines[i];
                let at = origin + vec2(0., out.offsets[i]);
                let layout = galley(ui, line, &font, width);
                let local = layout.cursor_from_pos(pointer - at);
                let cursor = Cursor {
                    line: line.id,
                    character: local.index.0,
                };
                if response.double_clicked() || response.triple_clicked() {
                    let mut selection = TextCursorState::default();
                    selection.pointer_interaction(ui, &response, local, &layout, false);
                    if let Some(range) = selection.char_range() {
                        out.selection = Some(Selection {
                            anchor: Cursor {
                                line: line.id,
                                character: range.secondary.index.0,
                            },
                            head: Cursor {
                                line: line.id,
                                character: range.primary.index.0,
                            },
                        });
                    }
                } else if pressed && response.hovered() {
                    let anchor = if ui.input(|i| i.modifiers.shift) {
                        out.selection.map_or(cursor, |s| s.anchor)
                    } else {
                        cursor
                    };
                    out.selection = Some(Selection {
                        anchor,
                        head: cursor,
                    });
                    out.dragging = true;
                    // A terminal selection takes over from label selections
                    // in other panels, without moving keyboard input focus.
                    ui.ctx()
                        .with_plugin(|s: &mut egui::text_selection::LabelSelectionState| {
                            s.clear_selection()
                        });
                } else if out.dragging && (down || ui.input(|i| i.pointer.primary_released())) {
                    if let Some(selection) = &mut out.selection {
                        selection.head = cursor;
                    }
                }
            }
            if out.dragging
                && down
                && let Some(pointer) = ui.input(|i| i.pointer.interact_pos())
            {
                let delta = if pointer.y < visible_rect.top() {
                    (visible_rect.top() - pointer.y).min(40.)
                } else if pointer.y > visible_rect.bottom() {
                    (visible_rect.bottom() - pointer.y).max(-40.)
                } else {
                    0.
                };
                if delta != 0. {
                    ui.scroll_with_delta(vec2(0., delta));
                    ui.ctx().request_repaint();
                }
            }

            let first = row_at(out, viewport.min.y).saturating_sub(1);
            let last = (row_at(out, viewport.max.y) + 2).min(out.lines.len());
            for i in first..last {
                let line = &out.lines[i];
                let at = origin + vec2(0., out.offsets[i]);
                let mut layout = galley(ui, line, &font, width);
                if let Some(selection) = out.selection {
                    let (start, end) = selection.sorted();
                    if start.line <= line.id && line.id <= end.line {
                        let from = if start.line == line.id {
                            start.character
                        } else {
                            0
                        };
                        let to = if end.line == line.id {
                            end.character
                        } else {
                            line.text.chars().count()
                        };
                        paint_text_selection(
                            &mut layout,
                            ui.visuals(),
                            &CCursorRange::two(CCursor::new(from), CCursor::new(to)),
                            None,
                        );
                        if line.id < end.line {
                            let cursor = layout.pos_from_cursor(layout.end());
                            ui.painter().rect_filled(
                                Rect::from_min_size(
                                    at + cursor.min.to_vec2(),
                                    vec2(cursor.height() / 2., cursor.height()),
                                ),
                                0.,
                                ui.visuals().selection.bg_fill,
                            );
                        }
                    }
                }
                ui.interact(
                    Rect::from_min_size(at, layout.size()),
                    ui.id().with(line.id),
                    Sense::hover(),
                )
                .widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &line.text)
                });
                ui.painter().galley(at, layout, color(line.kind));
            }
        }
        if !down {
            out.dragging = false;
        }
        visible_rect
    });
    clipboard::copy_output_on_right_click(ui, output.inner_rect, out.has_selection());
    if clipboard::take_output_copy(ui.ctx())
        && let Some(text) = out.selected_text()
    {
        ui.ctx().copy_text(text);
    }
}

/// Bookmarks the active tab's cwd, or removes its bookmark, from anywhere in
/// the window: Ctrl+B, as in the CLI (where it only adds). Cmd+B on Mac.
pub const TOGGLE_BOOKMARK: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::B);

/// Act on [TOGGLE_BOOKMARK] if it was pressed this frame, and remove its key
/// events, so no widget also sees them. Holding it down toggles just once.
/// Modifiers must match exactly, as for the tab shortcuts (see [tab_keys]).
///
/// [tab_keys]: super::tab_keys
pub fn toggle_bookmark_key(state: &mut State, ctx: &Context) {
    if take_shortcut(ctx, TOGGLE_BOOKMARK) {
        let msg = state.toggle_bookmark_cwd();
        state.push_out(msg, OutType::StdOut);
    }
}

/// Shows or hides the terminal pane from anywhere in the window, as in
/// JetBrains IDEs.
pub const TOGGLE_TERMINAL: KeyboardShortcut = KeyboardShortcut::new(Modifiers::ALT, Key::F12);

/// Act on [TOGGLE_TERMINAL] if it was pressed this frame, and remove its key
/// events, as for [toggle_bookmark_key]. Showing the terminal moves the
/// keyboard to it: to the input box, or to a full-screen program's screen
/// while one runs.
pub fn toggle_terminal_key(state: &mut State, ctx: &Context) {
    if !take_shortcut(ctx, TOGGLE_TERMINAL) {
        return;
    }
    let shown = &mut state.ui.panel_vis.terminal;
    *shown = !*shown;
    if !*shown {
        return;
    }

    if state.grid_here() {
        ctx.memory_mut(|m| m.request_focus(grid::grid_id()));
    } else {
        state.ui.focus_input = true;
    }
}

/// Whether `shortcut` was pressed this frame, after removing its key events.
/// A held key counts once.
pub(super) fn take_shortcut(ctx: &Context, shortcut: KeyboardShortcut) -> bool {
    ctx.input_mut(|i| {
        let mut pressed = false;
        i.events.retain(|event| match event {
            Event::Key {
                key,
                pressed: true,
                repeat,
                modifiers,
                ..
            } if *key == shortcut.logical_key && modifiers.matches_exact(shortcut.modifiers) => {
                pressed |= !repeat;
                false
            }
            _ => true,
        });
        pressed
    })
}

/// Alt + these cd the active tab to the bookmark the Bookmarks panel numbers
/// 0 through 9, as `bm <n>` or clicking its row does. Cmd+Option on Mac,
/// where Option + a digit types a symbol.
const BOOKMARK_NUMBER_KEYS: [Key; 10] = [
    Key::Num0,
    Key::Num1,
    Key::Num2,
    Key::Num3,
    Key::Num4,
    Key::Num5,
    Key::Num6,
    Key::Num7,
    Key::Num8,
    Key::Num9,
];

/// Act on the [BOOKMARK_NUMBER_KEYS] pressed this frame, and remove their key
/// events, as for [toggle_bookmark_key]. Holding one down jumps just once. A
/// number past the last bookmark does nothing.
pub fn bookmark_number_keys(state: &mut State, ctx: &Context) {
    let pattern = if ctx.os() == OperatingSystem::Mac {
        Modifiers::MAC_CMD | Modifiers::ALT
    } else {
        Modifiers::ALT
    };

    let jumps = ctx.input_mut(|i| {
        let mut jumps = Vec::new();
        let mut taken = false;
        i.events.retain(|event| {
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
            if !modifiers.matches_exact(pattern) {
                return true;
            }
            let Some(n) = BOOKMARK_NUMBER_KEYS.iter().position(|k| k == key) else {
                return true;
            };

            taken = true;
            if !repeat {
                jumps.push(n);
            }
            false
        });
        // Don't also type the digit into the input box.
        if taken {
            i.events.retain(|e| !matches!(e, Event::Text(_)));
        }
        jumps
    });

    for n in jumps {
        if let Some(i) = state.bookmark_at(n) {
            let target = state.dir_bookmarks[i].clone();
            state.change_saved_dir(target, DirList::Bookmarks);
        }
    }
}

/// The CLI input box's id, so [focus_input_key] can tell when it has focus.
fn input_id() -> Id {
    Id::new("cli_input")
}

/// Tab, from anywhere in the window, empties the CLI input and moves the
/// keyboard to it, in place of egui's moving focus to the next widget. Not
/// in the input itself, where Tab autocompletes, in the Scratchpad, where it
/// indents unless set not to (see [scratchpad::Scratchpad::keeps_tab]), or
/// while a popup is up, where it moves between the popup's buttons. While a
/// full-screen program runs, its screen stands in for the input, so Tab
/// moves the keyboard there instead. Either way, a hidden terminal is shown.
pub fn focus_input_key(state: &mut State, ctx: &Context) {
    if ctx.memory(|m| {
        m.has_focus(input_id())
            || (m.has_focus(scratchpad::edit_id()) && state.ui.scratchpad.keeps_tab())
            || m.top_modal_layer().is_some()
    }) {
        return;
    }

    let pressed = ctx.input_mut(|i| {
        let mut pressed = false;
        i.events.retain(|event| match event {
            Event::Key {
                key: Key::Tab,
                pressed: true,
                modifiers,
                ..
            } if modifiers.is_none() => {
                pressed = true;
                false
            }
            _ => true,
        });
        pressed
    });
    if !pressed {
        return;
    }

    // egui already read the Tab, at the start of the frame, as a move to
    // the next widget.
    ctx.memory_mut(|m| m.move_focus(egui::FocusDirection::None));
    state.ui.panel_vis.terminal = true;

    if state.grid_here() {
        ctx.memory_mut(|m| m.request_focus(grid::grid_id()));
        return;
    }
    let i = state.ui.active_tab;
    state.ui.cli_input[i].clear();
    state.reset_history_nav();
    state.ui.focus_input = true;
}

/// The input row: prompt line on top, with an edit beneath it that wraps long
/// input onto extra rows. Enter submits. When the user is recalling a history entry via Up/Down or
/// a recent dir via Left/Right, a green ` his N` / ` cd N` label is
/// inserted between the cwd and the `$`.
pub fn term_in(state: &mut State, ui: &mut Ui) {
    // Scale the prompt labels and input box with the chosen font size. Both
    // resolve `Monospace`, so overriding it on this panel's `ui` is enough.
    set_mono_size(ui, state.font_size);
    let active = state.ui.active_tab;
    let (his_cursor, cd_cursor) = state.active_nav_cursors();
    // Locally, the cwd's directories are color-coded (see [path_color]), with
    // the `*` and separators left in the prompt color.
    let cwd_parts = if state.is_remote() {
        vec![Part::plain(state.cwd_display())]
    } else {
        let mut parts = vec![Part::plain(state.cwd_star())];
        parts.extend(state.cwd_color_parts());
        parts
    };
    let branch = state.branch[active].clone();
    // While remote, the cwd label already shows `user@host`; don't also show
    // the (stale) local git branch.
    let show_branch = !state.is_remote();
    let connecting = matches!(state.remote_here(), Some(Remote::Connecting(_)));
    // A command running on the tab's remote takes typed lines, as a local
    // one does. Without a prompt hook the remote is a plain terminal, which
    // takes everything.
    let remote_busy = state.live_here().filter(|l| l.busy()).map(|l| {
        l.session
            .command()
            .map_or_else(|| "remote terminal".to_string(), str::to_string)
    });
    // A full-screen program has the keyboard; see `term_out`. Once it's
    // done, the keyboard comes back here.
    let grid = state.grid_here();
    if !grid && grid::has_focus(ui.ctx()) {
        state.ui.focus_input = true;
    }
    let running = state
        .running_here()
        .map(|cmd| cmd.line.clone())
        .or(remote_busy.clone());
    let mut stop = false;
    let mut interrupt = false;
    let mut cancel = false;

    // Wrapped, so a long cwd or running command flows onto extra rows (and
    // the Stop button drops below) instead of running off the right edge.
    ui.horizontal_wrapped(|ui| {
        let mono = TextStyle::Monospace.resolve(ui.style());
        ui.label(path_color::layout_job(&cwd_parts, mono, None, COLOR_PROMPT));

        if let Some(b) = branch {
            if show_branch {
                ui.label(
                    RichText::new(format!("br: {}", shell::truncate_branch(&b, 10)))
                        .color(COLOR_BRANCH)
                        .monospace(),
                );
            }
        }

        if let Some(i) = his_cursor {
            ui.label(
                RichText::new(format!("his {i}"))
                    .color(COLOR_HIS)
                    .monospace(),
            );
        }

        if let Some(i) = cd_cursor {
            ui.label(
                RichText::new(format!("cd {i}"))
                    .color(COLOR_HIS)
                    .monospace(),
            );
        }

        // While a command runs, the input goes to its stdin instead.
        match &running {
            _ if connecting => {
                cancel = ui
                    .button("Cancel")
                    .on_hover_text("Stop connecting")
                    .clicked();
            }
            Some(line) => {
                ui.label(
                    RichText::new(format!("running: {line}"))
                        .color(COLOR_STDERR_OK)
                        .monospace(),
                );
                let hover = if remote_busy.is_some() {
                    "Interrupt the remote command (Ctrl+C)"
                } else {
                    "End the command and everything it started. Ctrl+C interrupts it instead"
                };
                stop = ui.button("Stop").on_hover_text(hover).clicked();
            }
            None => {
                ui.label(RichText::new("$").color(COLOR_PROMPT).monospace());
            }
        }
    });

    // The keys we repurpose are consumed *before* the edit is drawn, while it
    // has focus, so it never sees them: Enter submits, Tab autocompletes,
    // ↑/↓ walk the global history list, and ←/→ walk recent dirs. A
    // multi-line edit would otherwise use these for newlines, tab-indents and
    // moving between wrapped rows. Left/Right only steal the keystroke when
    // the buffer is empty or a cd recall is already active — otherwise they
    // stay as normal caret movement.
    let input_id = input_id();
    let focused = ui.memory(|m| m.has_focus(input_id));
    let consume =
        |ui: &mut Ui, key| focused && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, key));

    let submitted = consume(ui, Key::Enter);
    let tab = consume(ui, Key::Tab);
    // Shell's line-editing keys (see [line_edit]). Ctrl+P / Ctrl+N are Up /
    // Down's, and Ctrl+E and End accept the suggestion as → does.
    let mac = ui.ctx().os() == OperatingSystem::Mac;
    let line_keys = if focused {
        ui.input_mut(|i| line_edit::take_keys(&mut i.events, mac))
    } else {
        Vec::new()
    };
    let history_key = |up| line_keys.contains(&LineKey::History { up });
    let up = !tab && (consume(ui, Key::ArrowUp) || history_key(true));
    let down = !tab && (consume(ui, Key::ArrowDown) || history_key(false));
    let cd_active = cd_cursor.is_some() || state.ui.cli_input[active].is_empty();
    let left = !tab && !up && !down && cd_active && consume(ui, Key::ArrowLeft);
    let right = !tab && !up && !down && !left && cd_active && consume(ui, Key::ArrowRight);

    // → (or End) with the caret at the end accepts the greyed history
    // suggestion, as in the CLI.
    let caret_at_end = TextEdit::load_state(ui.ctx(), input_id)
        .and_then(|s| s.cursor.char_range())
        .is_none_or(|r| {
            r.is_empty()
                && r.primary.index
                    == egui::text::CharIndex(state.ui.cli_input[active].chars().count())
        });
    let hint = if focused && caret_at_end && running.is_none() {
        state.input_hint()
    } else {
        None
    };

    let accept_hint = hint.is_some()
        && !tab
        && !up
        && !down
        && !left
        && !right
        && (consume(ui, Key::ArrowRight) || line_keys.contains(&LineKey::Edit(Edit::End)));

    // Ctrl+C interrupts a running command, as in a terminal, or else cancels
    // the input, unless there's text selected for it to copy (egui sends it
    // as a Copy event, not a key).
    let mut cancel_input = false;
    if focused && ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy))) {
        let input_selected = TextEdit::load_state(ui.ctx(), input_id)
            .and_then(|s| s.cursor.char_range())
            .is_some_and(|r| !r.is_empty());
        let label_selected = ui
            .ctx()
            .with_plugin(|s: &mut egui::text_selection::LabelSelectionState| s.has_selection())
            .unwrap_or(false);
        if !input_selected && !label_selected {
            if running.is_some() {
                interrupt = true;
            } else {
                cancel_input = true;
            }
        }
    }
    if cancel {
        state.disconnect_remote();
    }
    if cancel_input {
        state.ui.cli_input[active].clear();
        state.reset_history_nav();
    }
    if stop || interrupt {
        if remote_busy.is_some() {
            state.interrupt_remote();
        } else if stop {
            state.stop_running();
        } else {
            state.interrupt_running();
        }
        state.ui.focus_input = true;
    }

    // A full-screen program has the keyboard, and the pane.
    if grid {
        ui.label(
            RichText::new(
                "A full-screen program is running; keys go to it. Click it if they don't.",
            )
            .color(COLOR_STDERR_OK)
            .small(),
        );
        return;
    }

    // Before the box is drawn, so it shows their result.
    for key in line_keys {
        match key {
            // Handled with Up / Down, and with the suggestion, above.
            LineKey::History { .. } => {}
            LineKey::Edit(Edit::End) if accept_hint => {}
            LineKey::Edit(edit) => edit_input(state, ui.ctx(), edit),
            LineKey::ClearScreen => state.clear_out(),
            LineKey::Eof if !state.ui.cli_input[active].is_empty() => {
                edit_input(state, ui.ctx(), Edit::DeleteChar);
            }
            // On an empty line, Ctrl+D ends a running program's input (`cat`,
            // a REPL), as in a terminal. With nothing running, it's `exit`.
            LineKey::Eof => {
                if remote_busy.is_some() {
                    if let Some(live) = state.live_here() {
                        live.session.send(&[0x04]);
                    }
                } else if running.is_some() {
                    state.end_running_input();
                } else {
                    submit_command(state, "exit");
                }
            }
        }
    }

    // Input box sits on its own row, directly beneath the prompt line above.
    // It's multi-line so a long command wraps onto extra rows (and the panel
    // grows to fit) instead of scrolling off to the right, but it's still one
    // logical line: Enter never inserts a newline (`return_key(None)`), and
    // pasted newlines are flattened to spaces below. `lock_focus` stops egui
    // from using Tab for widget-to-widget focus navigation, since we use it
    // for path autocompletion.
    if let Some(text) = state.ui.clipboard.pending_text.take() {
        ui.ctx().memory_mut(|m| m.request_focus(input_id));
        ui.ctx()
            .input_mut(|i| i.events.push(egui::Event::Paste(text)));
    }
    let output = TextEdit::multiline(&mut state.ui.cli_input[active])
        .id(input_id)
        .desired_rows(1)
        .desired_width(f32::INFINITY)
        .return_key(None)
        .lock_focus(true)
        .font(egui::TextStyle::Monospace)
        .show(ui);
    let response = output.response;

    // Grey ghost text for the history suggestion, painted just after the
    // end of the typed text. Clipped to the whole box: the edit's own text
    // clip rect ends where the typed text does.
    if let Some(hint) = hint.as_ref().filter(|_| !accept_hint) {
        let end = egui::text::CCursor::new(state.ui.cli_input[active].chars().count());
        let pos = output.galley_pos + output.galley.pos_from_cursor(end).min.to_vec2();
        ui.painter().with_clip_rect(response.rect).text(
            pos,
            egui::Align2::LEFT_TOP,
            hint,
            TextStyle::Monospace.resolve(ui.style()),
            ui.visuals().weak_text_color(),
        );
    }

    // Same flattening a single-line edit applies to pastes. It's
    // char-for-char, so the edit's caret position stays valid.
    let buf = &mut state.ui.cli_input[active];
    if buf.contains(['\r', '\n']) {
        *buf = buf.replace(['\r', '\n'], " ");
    }

    if state.ui.focus_input {
        response.request_focus();
        state.ui.focus_input = false;
    }

    if tab || accept_hint {
        // Completion looks at local files, which mean nothing on a remote.
        if tab && !state.is_remote() {
            state.autocomplete_input();
        } else if let Some(hint) = hint.as_ref().filter(|_| accept_hint) {
            state.ui.cli_input[active].push_str(hint);
        }
        // egui's TextEdit keeps its own caret position, which isn't
        // updated when we replace the buffer behind its back. Move the
        // caret to the end so it follows the completed text.
        if let Some(mut edit_state) = TextEdit::load_state(ui.ctx(), response.id) {
            let end = CCursor::new(state.ui.cli_input[active].chars().count());
            edit_state
                .cursor
                .set_char_range(Some(CCursorRange::one(end)));
            edit_state.store(ui.ctx(), response.id);
        }
    } else if up {
        state.history_nav(true);
    } else if down {
        state.history_nav(false);
    } else if left {
        state.recent_dir_nav(true);
    } else if right {
        state.recent_dir_nav(false);
    }

    if submitted {
        let input = std::mem::take(&mut state.ui.cli_input[active]);
        // A line for a running program goes straight to it; its terminal
        // echoes it when the program wants it echoed (not a password, say),
        // so it isn't echoed here.
        if remote_busy.is_some() {
            state.send_remote_line(&input);
        } else if running.is_some() {
            state.send_running_line(&input);
        } else {
            submit_command(state, &input);
        }
        state.ui.focus_input = true;
    }
}

/// Apply `edit` to the active tab's input, moving the input box's caret to
/// match.
fn edit_input(state: &mut State, ctx: &Context, edit: Edit) {
    let line = &mut state.ui.cli_input[state.ui.active_tab];
    line_edit::apply_in_box(ctx, input_id(), line, edit, &mut state.ui.kill_ring);
}

/// The "terminal" output pane: scrolls, sticks to the bottom so the latest
/// line stays in view, and colors rows by stream kind. While a full-screen
/// program runs in the tab, locally or on its remote, its screen instead.
pub fn term_out(state: &mut State, ui: &mut Ui) {
    set_mono_size(ui, state.font_size);
    state.ui.term_size = grid::size_in_cells(ui);
    let i = state.ui.active_tab;
    if let Some(cmd) = state.running[i].as_mut().filter(|c| c.full_screen()) {
        let paste = state.ui.clipboard.pending_text.take();
        let take_focus = std::mem::take(&mut cmd.term.grid_started);
        let keys = grid::show(cmd.term.screen(), take_focus, ui, paste);
        cmd.send(&keys);
        return;
    }
    if let Some(live) = state.remote[i]
        .as_mut()
        .and_then(Remote::live_mut)
        .filter(|l| l.grid_mode())
    {
        let paste = state.ui.clipboard.pending_text.take();
        let take_focus = std::mem::take(&mut live.grid_started);
        let keys = grid::show(live.screen.screen(), take_focus, ui, paste);
        live.session.send(&keys);
        return;
    }
    show(&mut state.ui.out[i], ui);
}

/// Override the `Monospace` text style's size on `ui` so the terminal panes
/// render at the user-chosen font size. Both panes resolve `Monospace` (the
/// output pane for its layout jobs, the input row for its prompt labels and
/// `TextEdit`), so setting it here is enough to scale all the terminal text.
fn set_mono_size(ui: &mut Ui, size: f32) {
    if let Some(font) = ui
        .style_mut()
        .text_styles
        .get_mut(&egui::TextStyle::Monospace)
    {
        font.size = size;
    }
}

const COLOR_PROMPT: Color32 = Color32::from_rgb(220, 220, 90);
pub const COLOR_STDOUT: Color32 = Color32::from_rgb(210, 210, 210);
const COLOR_STDERR: Color32 = Color32::from_rgb(240, 110, 110);
// Amber: stderr from a command that succeeded (warnings, progress), so red
// is reserved for failures.
pub const COLOR_STDERR_OK: Color32 = Color32::from_rgb(240, 180, 70);
