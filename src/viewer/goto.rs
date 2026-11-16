//! Ctrl+G: go to a line, as in PyCharm or Sublime Text. A small box takes
//! `line` or `line:column`, both from 1.

use egui::{
    Align, Context, Galley, Id, Key, Modal, Modifiers, Pos2, TextEdit, Ui,
    text::{CCursor, CCursorRange},
};

use crate::ui::line_edit;

const BOX_WIDTH: f32 = 200.;

/// One viewer tab's go-to-line box.
#[derive(Default)]
pub(super) struct GoTo {
    open: bool,
    input: String,
    /// Focus the box: it just opened.
    focus: bool,
    /// Scroll to this char of the text once it's laid out.
    reveal: Option<usize>,
}

impl GoTo {
    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    /// Open the box on Ctrl+G. Ctrl on every OS: on Mac, Cmd+G is find next.
    pub(super) fn keys(&mut self, ctx: &Context) {
        if ctx.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::G)) {
            self.open = true;
            self.focus = true;
            self.input.clear();
        }
    }

    /// The box, while open, over the tab with id `doc`. Enter moves the
    /// caret in text box `text_id`, showing `text`, to the line asked for.
    /// Unless `editing`, it selects the line instead: a read-only box draws
    /// no caret.
    pub(super) fn show(&mut self, ctx: &Context, doc: Id, text: &str, text_id: Id, editing: bool) {
        if !self.open {
            return;
        }

        let lines = text.bytes().filter(|&b| b == b'\n').count() + 1;
        let invalid = !self.input.trim().is_empty() && parse(&self.input).is_none();
        let mut go = false;
        let modal = Modal::new(Id::new(("viewer_goto", doc))).show(ctx, |ui| {
            ui.label("Go to line");
            let mut edit = TextEdit::singleline(&mut self.input)
                .hint_text(format!("Line[:column], 1 to {lines}"))
                .desired_width(BOX_WIDTH)
                // Enter goes, keeping focus until it does.
                .return_key(None);
            if invalid {
                edit = edit.text_color(ui.visuals().error_fg_color);
            }
            let response = ui.add(edit);
            if std::mem::take(&mut self.focus) {
                response.request_focus();
            }
            go = response.has_focus()
                && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
        });

        if go && let Some((line, column)) = parse(&self.input) {
            let (start, caret, end) = locate(text, line, column);
            let range = if editing {
                CCursorRange::one(CCursor::new(caret))
            } else {
                CCursorRange::two(CCursor::new(start), CCursor::new(end))
            };
            line_edit::set_selection(ctx, text_id, range);
            ctx.memory_mut(|m| m.request_focus(text_id));
            self.reveal = Some(caret);
            self.open = false;
        } else if modal.should_close() {
            self.open = false;
            if editing {
                ctx.memory_mut(|m| m.request_focus(text_id));
            }
        }
    }

    /// Scroll to the line gone to, in `galley`, the text drawn at `pos`.
    /// Call in its scroll area, after drawing it.
    pub(super) fn scroll(&mut self, ui: &Ui, galley: &Galley, pos: Pos2) {
        let Some(at) = self.reveal.take() else {
            return;
        };

        let rect = galley.pos_from_cursor(CCursor::new(at));
        ui.scroll_to_rect(rect.translate(pos.to_vec2()), Some(Align::Center));
    }
}

/// `line` or `line:column`, from 1.
fn parse(input: &str) -> Option<(usize, usize)> {
    let (line, column) = match input.split_once(':') {
        Some((line, column)) => (line, column.trim()),
        None => (input, ""),
    };
    let positive = |s: &str| s.trim().parse().ok().filter(|&n: &usize| n > 0);
    let line = positive(line)?;
    let column = if column.is_empty() {
        1
    } else {
        positive(column)?
    };

    Some((line, column))
}

/// The chars of `text` where line `line` starts, where column `column` is
/// on it, and where it ends; all from 1, and clamped to the text.
fn locate(text: &str, line: usize, column: usize) -> (usize, usize, usize) {
    let rows: Vec<&str> = text.split('\n').take(line).collect();
    // `split` always yields at least one row.
    let (row, before) = rows.split_last().unwrap();
    let start: usize = before.iter().map(|r| r.chars().count() + 1).sum();
    let len = row.strip_suffix('\r').unwrap_or(row).chars().count();

    (start, start + (column - 1).min(len), start + len)
}
