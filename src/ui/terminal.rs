//! Variable-height scrollback virtualization. Heights are measured once with
//! uncached layout; only visible lines enter egui's much larger galley cache.

use std::sync::Arc;

use egui::{
    Color32, CursorIcon, FontId, Galley, Rect, ScrollArea, Sense, TextStyle, Ui,
    text::CCursor,
    text_selection::{CCursorRange, TextCursorState, visuals::paint_text_selection},
    vec2,
};

use super::{COLOR_PROMPT, COLOR_STDERR, COLOR_STDERR_OK, COLOR_STDOUT, clipboard};
use crate::{
    OutType,
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

#[cfg(test)]
mod tests {
    use egui::{CentralPanel, Context, Event, Modifiers, PointerButton, RawInput};

    use super::*;

    fn frame(ctx: &Context, out: &mut Output, width: f32, events: Vec<Event>) -> egui::FullOutput {
        ctx.run_ui(
            RawInput {
                screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(width, 400.))),
                events,
                ..Default::default()
            },
            |ui| {
                CentralPanel::default().show(ui, |ui| show(out, ui));
            },
        )
    }

    #[test]
    fn large_scrollback_keeps_only_visible_galleys_and_reflows_on_resize() {
        let ctx = Context::default();
        let mut out = Output::default();
        for i in 0..10_000 {
            out.push(&format!("{i:08}: {}", "x".repeat(90)), OutType::StdOut);
        }
        for _ in 0..3 {
            frame(&ctx, &mut out, 1400., vec![]).drop_without_applying_deltas();
        }
        assert!(out.lines.len() > 9000);
        assert!(ctx.fonts_mut(|f| f.num_galleys_in_cache()) < 150);
        let wide = out.lines.back().unwrap().height.unwrap();
        frame(&ctx, &mut out, 240., vec![]).drop_without_applying_deltas();
        assert!(out.lines.back().unwrap().height.unwrap() > wide);
        assert!(ctx.fonts_mut(|f| f.num_galleys_in_cache()) < 150);
    }

    #[test]
    fn measured_heights_match_layout_for_ascii_and_wrapped_styled_unicode() {
        let ctx = Context::default();
        for width in [180., 800.] {
            let mut out = Output::default();
            out.push("plain text\n\x1b[3mitalic ASCII\x1b[0m\n\n😀é שלום text that wraps with a narrow window\tend", OutType::StdOut);
            let result = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(width, 400.))),
                    ..Default::default()
                },
                |ui| {
                    let font = TextStyle::Monospace.resolve(ui.style());
                    measure(&mut out, ui, &font, width);
                    for line in &out.lines {
                        let actual = galley(ui, line, &font, width).size().y;
                        assert!(
                            (line.height.unwrap() - actual).abs() < 0.01,
                            "height for {:?}: measured {}, actual {actual}",
                            line.text,
                            line.height.unwrap()
                        );
                    }
                },
            );
            result.drop_without_applying_deltas();
        }
    }

    #[test]
    fn eviction_preserves_the_retained_text_being_read() {
        fn view(ctx: &Context, out: &mut Output, events: Vec<Event>) -> u64 {
            let mut first = None;
            let result = ctx.run_ui(
                RawInput {
                    time: Some(ctx.input(|i| i.time) + 1.),
                    screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(1400., 400.))),
                    events,
                    ..Default::default()
                },
                |ui| {
                    CentralPanel::default().show(ui, |ui| {
                        let id =
                            ui.make_persistent_id(egui::IdSalt::new(("term_out_scroll", out.id)));
                        show(out, ui);
                        let state = egui::scroll_area::State::load(ctx, id).unwrap();
                        first = Some(out.lines[row_at(out, state.offset.y)].id);
                    });
                },
            );
            result.drop_without_applying_deltas();
            first.unwrap()
        }
        let ctx = Context::default();
        let mut out = Output::default();
        for i in 0..10_000 {
            out.push(&format!("{i:08}: {}", "x".repeat(90)), OutType::StdOut);
        }
        for _ in 0..3 {
            view(&ctx, &mut out, vec![]);
        }
        view(
            &ctx,
            &mut out,
            vec![
                Event::PointerMoved(egui::pos2(40., 40.)),
                Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: vec2(0., 500.),
                    phase: egui::TouchPhase::Move,
                    modifiers: Modifiers::NONE,
                },
            ],
        );
        for _ in 0..60 {
            view(&ctx, &mut out, vec![]);
        }
        let before = view(&ctx, &mut out, vec![]);
        for i in 10_000..14_000 {
            out.push(&format!("{i:08}: {}", "x".repeat(90)), OutType::StdOut);
        }
        assert!(out.truncated);
        let after = view(&ctx, &mut out, vec![]);
        assert_eq!(before, after);
    }

    #[test]
    fn drag_scroll_and_copy_retains_offscreen_selection() {
        let ctx = Context::default();
        let mut out = Output::default();
        for i in 0..1000 {
            out.push(&format!("{i:08}: selectable output"), OutType::StdOut);
        }
        for _ in 0..3 {
            frame(&ctx, &mut out, 800., vec![]).drop_without_applying_deltas();
        }
        let start = egui::pos2(20., 25.);
        let end = egui::pos2(180., 130.);
        frame(
            &ctx,
            &mut out,
            800.,
            vec![
                Event::PointerMoved(start),
                Event::PointerButton {
                    pos: start,
                    button: PointerButton::Primary,
                    pressed: true,
                    modifiers: Modifiers::NONE,
                },
            ],
        )
        .drop_without_applying_deltas();
        frame(&ctx, &mut out, 800., vec![Event::PointerMoved(end)]).drop_without_applying_deltas();
        frame(
            &ctx,
            &mut out,
            800.,
            vec![Event::PointerButton {
                pos: end,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            }],
        )
        .drop_without_applying_deltas();
        let selected = out.selected_text().expect("drag selects output");
        assert!(selected.contains('\n'));
        // Scroll far enough to remove both selection endpoints from the view.
        for _ in 0..3 {
            frame(
                &ctx,
                &mut out,
                800.,
                vec![Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: vec2(0., 500.),
                    phase: egui::TouchPhase::Move,
                    modifiers: Modifiers::NONE,
                }],
            )
            .drop_without_applying_deltas();
        }
        let copied = frame(&ctx, &mut out, 800., vec![Event::Copy]);
        assert!(
            copied
                .platform_output
                .commands
                .iter()
                .any(|c| matches!(c, egui::OutputCommand::CopyText(text) if text == &selected))
        );
        copied.drop_without_applying_deltas();
        let copied = frame(
            &ctx,
            &mut out,
            800.,
            vec![Event::PointerButton {
                pos: start,
                button: PointerButton::Secondary,
                pressed: true,
                modifiers: Modifiers::NONE,
            }],
        );
        assert!(
            copied
                .platform_output
                .commands
                .iter()
                .any(|c| matches!(c, egui::OutputCommand::CopyText(text) if text == &selected))
        );
        copied.drop_without_applying_deltas();
    }
}
