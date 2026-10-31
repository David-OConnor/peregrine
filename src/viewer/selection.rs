//! Selection scrolling, plus a selection spanning rendered Markdown labels.
//! egui drops a label selection when either endpoint goes off screen, so
//! Markdown keeps endpoints in a cached, complete layout of the document.

use std::{ops::Range, time::Duration};

use egui::{
    CursorIcon, Event, Id, Key, Modifiers, PointerButton, Pos2, Rangef, Rect, Response, Sense, Ui,
    Vec2, epaint::TextShape, vec2,
};

use super::search::{self, Layout};

/// Scroll while a selection drag is held at an edge, including between
/// pointer events. Farther beyond the edge scrolls faster, capped at 900 pt/s.
pub(super) fn auto_scroll(ui: &Ui, dragging: bool, horizontal: bool) {
    if !dragging || !ui.input(|i| i.pointer.primary_down()) {
        return;
    }
    let Some(pointer) = ui.ctx().pointer_interact_pos() else {
        return;
    };
    let clip = ui.clip_rect();
    let speed = |at: f32, min: f32, max: f32| {
        let edge = 20_f32.min((max - min) / 4.);
        let distance = if at < min + edge {
            at - min - edge
        } else if at > max - edge {
            at - max + edge
        } else {
            0.
        };
        (distance * 15.).clamp(-900., 900.)
    };
    let velocity = vec2(
        if horizontal {
            speed(pointer.x, clip.left(), clip.right())
        } else {
            0.
        },
        speed(pointer.y, clip.top(), clip.bottom()),
    );
    if velocity != Vec2::ZERO {
        let dt = ui.input(|i| i.stable_dt).clamp(1. / 240., 1. / 20.);
        ui.scroll_with_delta_animation(-velocity * dt, egui::style::ScrollAnimation::none());
        ui.ctx().request_repaint_after(Duration::from_millis(16));
    }
}

pub(super) fn text(ui: &Ui, response: &Response, horizontal: bool) {
    auto_scroll(ui, response.dragged_by(PointerButton::Primary), horizontal);
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Cursor {
    piece: usize,
    character: usize,
}

#[derive(Default)]
pub(super) struct Markdown {
    layout: Option<Layout>,
    texts: Vec<TextShape>,
    anchor: Option<Cursor>,
    primary: Option<Cursor>,
    dragging: bool,
}

impl Markdown {
    /// Call after rendering, in the document's scroll area. `show` lays out
    /// all its text once, off screen, when a new selection needs measuring.
    pub(super) fn show(
        &mut self,
        ui: &Ui,
        top_left: Pos2,
        layout: Layout,
        doc: Id,
        show: impl FnOnce(&mut Ui),
    ) {
        if !ui.is_enabled() || !ui.ctx().memory(|m| m.is_above_modal_layer(ui.layer_id())) {
            self.dragging = false;
            return;
        }
        let selection_id = Id::new(("viewer_markdown_selection", doc));
        let response = ui.interact(
            Rect::from_min_size(ui.clip_rect().min, Vec2::ZERO),
            selection_id,
            Sense::focusable_noninteractive(),
        );
        if self.layout.as_ref() != Some(&layout) {
            self.anchor = None;
            self.primary = None;
            self.dragging = false;
            self.texts.clear();
            self.layout = Some(layout);
        }
        let pointer = ui.ctx().pointer_interact_pos();
        let pressed = ui.input(|i| i.pointer.primary_pressed());
        let clip = ui.clip_rect();
        let inside = pointer.is_some_and(|p| clip.contains(p));
        let select_all = (!ui.ctx().egui_wants_keyboard_input() || response.has_focus())
            && ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::A));
        if self.texts.is_empty() && ((pressed && inside) || select_all) {
            self.texts = search::measure_text(ui, ui.available_width(), doc, show);
            // Fenced code's copy buttons are UI chrome, not document text.
            self.texts.retain(|text| {
                !matches!(text.galley.text(), "🗐" | "✔")
                    || !text.galley.job.sections.iter().all(|section| {
                        section.format.font_id.family == egui::FontFamily::Proportional
                    })
            });
        }
        if pressed {
            self.dragging = false;
            if inside {
                let at = pointer.unwrap() - top_left.to_vec2();
                if self
                    .texts
                    .iter()
                    .any(|text| Rect::from_min_size(text.pos, text.galley.size()).contains(at))
                {
                    let cursor = self.cursor_at(at);
                    if !ui.input(|i| i.modifiers.shift) || self.anchor.is_none() {
                        self.anchor = cursor;
                    }
                    self.primary = cursor;
                    self.dragging = true;
                    response.request_focus();
                } else {
                    self.anchor = None;
                    self.primary = None;
                }
            } else {
                self.anchor = None;
                self.primary = None;
            }
        }
        if self.dragging && ui.input(|i| i.pointer.primary_down()) {
            if let Some(pointer) = pointer {
                // Select up to the visible edge, then extend as scrolling
                // exposes the following rows, even with a stationary mouse.
                self.primary = self.cursor_at(clip.clamp(pointer) - top_left.to_vec2());
            }
            if ui.input(|i| i.pointer.is_decidedly_dragging()) {
                ui.ctx().set_dragged_id(selection_id);
                auto_scroll(ui, true, false);
                ui.ctx().set_cursor_icon(CursorIcon::Text);
            }
        } else {
            self.dragging = false;
        }
        if inside
            && !self.texts.is_empty()
            && ui.input(|i| {
                i.pointer.button_double_clicked(PointerButton::Primary)
                    || i.pointer.button_triple_clicked(PointerButton::Primary)
            })
        {
            if let Some(cursor) = self.primary {
                let chars: Vec<char> = self.texts[cursor.piece].galley.text().chars().collect();
                let line = ui.input(|i| i.pointer.button_triple_clicked(PointerButton::Primary));
                let belongs = |c: char| {
                    if line {
                        c != '\n'
                    } else {
                        c.is_alphanumeric() || c == '_'
                    }
                };
                let mut range = cursor.character..cursor.character;
                while range.start > 0 && belongs(chars[range.start - 1]) {
                    range.start -= 1;
                }
                while range.end < chars.len() && belongs(chars[range.end]) {
                    range.end += 1;
                }
                self.anchor = Some(Cursor {
                    character: range.start,
                    ..cursor
                });
                self.primary = Some(Cursor {
                    character: range.end,
                    ..cursor
                });
            }
        }
        if !ui.ctx().egui_wants_keyboard_input() || response.has_focus() {
            if select_all && !self.texts.is_empty() {
                self.anchor = Some(Cursor {
                    piece: 0,
                    character: 0,
                });
                let piece = self.texts.len() - 1;
                self.primary = Some(Cursor {
                    piece,
                    character: self.texts[piece].galley.text().chars().count(),
                });
            }
            if ui.input(|i| i.events.iter().any(|e| matches!(e, Event::Copy))) {
                if let Some(text) = self.selected_text() {
                    ui.ctx().copy_text(text);
                }
            }
            if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
                self.anchor = None;
                self.primary = None;
            }
        }
        if let Some((start, end)) = self.range() {
            let fill = ui.visuals().selection.bg_fill.gamma_multiply(0.55);
            for piece in start.piece..=end.piece {
                let text = &self.texts[piece];
                let range = self.piece_range(piece, start, end);
                let offset = top_left.to_vec2() + text.pos.to_vec2();
                let visible = Rangef::new(clip.top() - offset.y, clip.bottom() - offset.y);
                search::row_pieces(&text.galley, &[range], visible, |_, rect| {
                    ui.painter().rect_filled(rect.translate(offset), 0., fill);
                });
            }
        }
    }

    fn cursor_at(&self, at: Pos2) -> Option<Cursor> {
        let (piece, text) = self.texts.iter().enumerate().min_by(|(_, a), (_, b)| {
            let distance =
                |t: &TextShape| Rect::from_min_size(t.pos, t.galley.size()).distance_sq_to_pos(at);
            distance(a).total_cmp(&distance(b))
        })?;
        Some(Cursor {
            piece,
            character: text.galley.cursor_from_pos(at - text.pos).index.0,
        })
    }

    fn range(&self) -> Option<(Cursor, Cursor)> {
        let (a, b) = (self.anchor?, self.primary?);
        (a != b).then_some((a.min(b), a.max(b)))
    }

    fn piece_range(&self, piece: usize, start: Cursor, end: Cursor) -> Range<usize> {
        let from = if piece == start.piece {
            start.character
        } else {
            0
        };
        let to = if piece == end.piece {
            end.character
        } else {
            self.texts[piece].galley.text().chars().count()
        };
        from..to
    }

    fn selected_text(&self) -> Option<String> {
        let (start, end) = self.range()?;
        let mut copied = String::new();
        for piece in start.piece..=end.piece {
            let text = &self.texts[piece];
            if piece > start.piece {
                let previous = &self.texts[piece - 1];
                let same_line = (previous.pos.y - text.pos.y).abs() < 2.;
                if !same_line && !copied.ends_with('\n') {
                    copied.push('\n');
                } else if same_line
                    && text.pos.x - (previous.pos.x + previous.galley.size().x) > 1.
                    && !copied.ends_with(char::is_whitespace)
                    && !text.galley.text().starts_with(char::is_whitespace)
                {
                    copied.push(' ');
                }
            }
            let range = self.piece_range(piece, start, end);
            copied.extend(
                text.galley
                    .text()
                    .chars()
                    .skip(range.start)
                    .take(range.len()),
            );
        }
        Some(copied)
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, sync::Arc};

    use egui::{Context, RawInput, ScrollArea, TextEdit, pos2};
    use egui_commonmark::CommonMarkCache;

    use super::*;
    use crate::{
        markdown::{self, Details},
        ui::scratchpad::ScratchpadTab,
        viewer::{Kind, editor, search, show_text, syntax},
    };

    fn raw(time: f64, events: Vec<Event>) -> RawInput {
        RawInput {
            time: Some(time),
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(420., 260.))),
            events,
            ..Default::default()
        }
    }

    fn button(at: Pos2, pressed: bool) -> Event {
        Event::PointerButton {
            pos: at,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        }
    }

    #[test]
    fn view_and_edit_selections_scroll_with_stationary_pointer_and_stop_on_release() {
        let text: Arc<str> = (0..150)
            .map(|i| {
                format!(
                    "line {i:03} with text to select {}\n",
                    "more text ".repeat(20)
                )
            })
            .collect::<String>()
            .into();
        let path =
            std::env::temp_dir().join(format!("peregrine-selection-{}.txt", std::process::id()));
        fs::write(&path, text.as_bytes()).unwrap();
        for (editing, kind) in [
            (false, Kind::Text),
            (true, Kind::Text),
            (false, Kind::Code("rs")),
            (true, Kind::Code("rs")),
        ] {
            let ctx = Context::default();
            let doc = Id::new("doc");
            let mut layout = syntax::Source::default();
            let mut search = search::Search::default();
            let mut editor = editor::Editor::load(&path).unwrap();
            let mut time = 0.;
            let mut frame = |events| {
                time += 1. / 60.;
                ctx.run_ui(raw(time, events), |ui| {
                    if editing {
                        editor.show(ui, kind, ScratchpadTab::Spaces, &mut search, doc);
                    } else {
                        show_text(ui, &text, kind, 151, &mut layout, &mut search, doc);
                    }
                })
                .drop_without_applying_deltas();
            };
            frame(vec![]);
            frame(vec![]);
            let id = if editing {
                editor::id(doc)
            } else {
                Id::new(("viewer_text", doc))
            };
            let rect = ctx.read_response(id).unwrap().rect;
            let start = rect.min + vec2(3., 4.);
            let bottom = pos2(start.x + 20., 290.);
            frame(vec![Event::PointerMoved(start), button(start, true)]);
            frame(vec![Event::PointerMoved(bottom)]);
            let cursor = || {
                TextEdit::load_state(&ctx, id)
                    .unwrap()
                    .cursor
                    .char_range()
                    .unwrap()
                    .primary
                    .index
                    .0
            };
            let first = cursor();
            for _ in 0..40 {
                frame(vec![]);
            }
            assert!(
                cursor() > first + 200,
                "selection must extend as new rows scroll into view (edit={editing})"
            );
            let down = cursor();
            frame(vec![Event::PointerMoved(pos2(start.x, -30.))]);
            for _ in 0..8 {
                frame(vec![]);
            }
            assert!(
                cursor() < down,
                "dragging above the top must scroll upwards"
            );
            if matches!(kind, Kind::Code(_)) {
                frame(vec![Event::PointerMoved(pos2(450., 100.))]);
                let sideways = cursor();
                for _ in 0..30 {
                    frame(vec![]);
                }
                assert!(
                    cursor() > sideways + 10,
                    "code selection must scroll sideways"
                );
            }
            frame(vec![button(pos2(start.x, -30.), false)]);
            let released = cursor();
            for _ in 0..8 {
                frame(vec![]);
            }
            assert_eq!(cursor(), released);
        }
        assert_eq!(fs::read(&path).unwrap(), text.as_bytes());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn rendered_markdown_keeps_and_copies_an_offscreen_selection_anchor() {
        let text: Arc<str> = (0..100)
            .map(|i| {
                let paragraph = format!("Paragraph {i:03}: some **formatted** text.\n\n");
                if i % 10 == 0 {
                    format!("{paragraph}```rs\nlet value = {i};\n```\n\n")
                } else {
                    paragraph
                }
            })
            .collect::<String>()
            .into();
        let blocks = markdown::split_details(&text);
        let ctx = Context::default();
        let mut selection = Markdown::default();
        let mut cache = CommonMarkCache::default();
        let mut details_open = Vec::new();
        let path = Path::new("selection.md");
        let doc = Id::new("doc");
        let mut time = 0.;
        let mut frame = |selection: &mut Markdown, events| {
            time += 1. / 60.;
            ctx.run_ui(raw(time, events), |ui| {
                ScrollArea::vertical()
                    .animated(false)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.style_mut().interaction.selectable_labels = false;
                        let top_left = ui.max_rect().min;
                        let width = ui.available_width();
                        markdown::show(
                            ui,
                            &text,
                            &blocks,
                            path,
                            &mut cache,
                            doc,
                            &mut Details::Shown(&mut details_open),
                        );
                        let layout =
                            Layout::markdown(&text, width, ui.min_rect().height(), &details_open);
                        selection.show(ui, top_left, layout, doc, |ui| {
                            markdown::show(
                                ui,
                                &text,
                                &blocks,
                                path,
                                &mut cache,
                                doc,
                                &mut Details::Measured(&details_open),
                            );
                        });
                    });
            })
        };
        frame(&mut selection, vec![]).drop_without_applying_deltas();
        frame(&mut selection, vec![]).drop_without_applying_deltas();
        let start = pos2(2., 5.);
        let bottom = pos2(110., 290.);
        frame(
            &mut selection,
            vec![Event::PointerMoved(start), button(start, true)],
        )
        .drop_without_applying_deltas();
        frame(&mut selection, vec![Event::PointerMoved(bottom)]).drop_without_applying_deltas();
        let first = selection.selected_text().unwrap().len();
        for _ in 0..60 {
            frame(&mut selection, vec![]).drop_without_applying_deltas();
        }
        let copied = selection.selected_text().unwrap();
        assert!(copied.contains("Paragraph 000"));
        assert!(copied.contains("let value = 0;"));
        assert!(
            !copied.contains('🗐'),
            "copy buttons must not appear in selected text"
        );
        assert!(
            copied.len() > first + 300,
            "selection must retain its off-screen anchor and grow"
        );
        frame(&mut selection, vec![button(bottom, false)]).drop_without_applying_deltas();
        let output = frame(&mut selection, vec![Event::Copy]);
        assert!(
            output
                .platform_output
                .commands
                .iter()
                .any(|c| matches!(c, egui::OutputCommand::CopyText(s) if s == &copied))
        );
        output.drop_without_applying_deltas();
    }
}
