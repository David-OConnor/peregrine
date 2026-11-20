//! What a language server adds to the editor on screen: problems
//! underlined (and their lines' numbers colored; see `gutter`), a popup with
//! the problems and the server's docs for the code under the pointer, the
//! completion list, and a status in the toolbar. And its keys:
//!
//! - Ctrl+Space: complete. The list also opens after the server's trigger
//!   characters (`.`, `::`) and as a word is typed. Up / Down and Page Up /
//!   Page Down pick, Enter or Tab takes, Escape closes.
//! - F12, or Ctrl+click (Cmd+click on Mac): go to definition.
//! - Ctrl+Q: the popup for the code at the caret, as JetBrains IDEs.
//! - Shift+Alt+F, as VS Code, or Ctrl+Alt+L, as JetBrains IDEs: format.
//! - F8 / Shift+F8: the next / previous problem.

use std::{
    ops::Range,
    sync::Arc,
    time::{Duration, Instant},
};

use egui::{
    Area, Button, Color32, Context, Event, Frame, Galley, Id, Key, Modifiers, Order, Pos2, Rangef,
    Rect, RichText, ScrollArea, Shape, Stroke, TextStyle, Ui, Visuals,
    os::OperatingSystem,
    pos2,
    text::{CCursor, CharIndex, LayoutJob, TextFormat, TextWrapping},
    text_edit::TextEditOutput,
    vec2,
};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use serde_json::json;

use super::{
    DocLsp, Edit, apply,
    process::{Kind, Reply, lock},
    protocol::{self, CompletionItem, Lines},
    server::Health,
};

/// How long the pointer rests on code before its popup opens.
const HOVER_DELAY: Duration = Duration::from_millis(500);
const HOVER_WIDTH: f32 = 560.;
const HOVER_HEIGHT: f32 = 360.;
/// The completion list's size.
const LIST_ROWS: usize = 10;
const LIST_WIDTH: f32 = 460.;

/// The popup for the code under the pointer, or at the caret.
#[derive(Default)]
pub(super) struct Hover {
    /// Where the pointer was last frame.
    pointer: Option<Pos2>,
    /// The char it rests on, and since when.
    rest: Option<(usize, Instant)>,
    /// The request out for the popup's docs.
    request: Option<i64>,
    shown: Option<Shown>,
}

impl Hover {
    pub(super) fn request(&self) -> Option<i64> {
        self.request
    }
}

struct Shown {
    /// The word it's about, in chars: the pointer may move along it.
    word: Range<usize>,
    /// Severity and message.
    problems: Vec<(u8, String)>,
    /// Markdown, once the server's reply is in.
    docs: Option<String>,
    /// Opened from the keyboard: it stays as the pointer moves.
    sticky: bool,
    /// The word on screen this frame; `None` if scrolled out of view.
    anchor: Option<Rect>,
    /// Where the popup was last drawn.
    rect: Option<Rect>,
}

/// The completion list.
pub(super) struct Completion {
    /// The request out for it, and the text it's about.
    request: Option<(i64, Arc<str>)>,
    /// The text `items` are for.
    base: Arc<str>,
    items: Vec<CompletionItem>,
    /// The server wants asking again as the word grows, rather than its
    /// items narrowed down.
    incomplete: bool,
    /// Where the word being completed starts, in chars.
    start: usize,
    /// The items that match what's typed of the word, best first.
    shown: Vec<usize>,
    selected: usize,
    /// The row at the top of the list, and whether to scroll so `selected`
    /// shows.
    top: usize,
    reveal: bool,
    /// The word's start on screen, this frame.
    anchor: Option<Rect>,
    /// Where the list was last drawn.
    rect: Option<Rect>,
    /// A row clicked, taken next frame.
    clicked: Option<usize>,
}

impl Completion {
    pub(super) fn request(&self) -> Option<i64> {
        self.request.as_ref().map(|(id, _)| *id)
    }

    fn open(&self) -> bool {
        !self.shown.is_empty()
    }

    /// Whether the word typed, from `start` to `caret` of `text`, is just
    /// what the selected item would put there.
    fn typed_in_full(&self, text: &str, caret: usize) -> bool {
        let Some(item) = self.shown.get(self.selected).map(|&i| &self.items[i]) else {
            return false;
        };
        let insert = item
            .text_edit
            .as_ref()
            .map(|e| e.new_text.as_str())
            .or(item.insert_text.as_deref())
            .unwrap_or(&item.label);
        let len = caret.saturating_sub(self.start);
        let word: String = text.chars().skip(self.start).take(len).collect();
        !word.is_empty() && insert == word
    }

    /// Narrow the items down to those matching the word from `start` to
    /// `caret` in `text`: those starting with it first, then those holding
    /// it, then those holding its letters in order. Each group in the
    /// server's order.
    fn filter(&mut self, text: &str, caret: usize) {
        let word: String = text
            .chars()
            .skip(self.start)
            .take(caret.saturating_sub(self.start))
            .collect();
        let lower = word.to_lowercase();
        let mut scored: Vec<(u8, &str, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                let key = item.filter_text.as_deref().unwrap_or(&item.label);
                let sort = item.sort_text.as_deref().unwrap_or(&item.label);
                score(key, &word, &lower).map(|score| (score, sort, i))
            })
            .collect();
        scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
        self.shown = scored.into_iter().map(|(_, _, i)| i).collect();
        // The server's pick, before anything's typed.
        self.selected = if word.is_empty() {
            let items = &self.items;
            self.shown
                .iter()
                .position(|&i| items[i].preselect == Some(true))
                .unwrap_or(0)
        } else {
            0
        };
        self.top = 0;
        self.reveal = true;
    }
}

/// How well `candidate` matches `word` (`lower` in lowercase): lower is
/// better, `None` not at all.
fn score(candidate: &str, word: &str, lower: &str) -> Option<u8> {
    if candidate.starts_with(word) {
        return Some(0);
    }
    let candidate = candidate.to_lowercase();
    if candidate.starts_with(lower) {
        Some(1)
    } else if candidate.contains(lower) {
        Some(2)
    } else {
        let mut chars = candidate.chars();
        lower.chars().all(|c| chars.any(|d| d == c)).then_some(3)
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The byte offset of char `at` of `text`.
fn byte_of(text: &str, at: usize) -> usize {
    text.char_indices().nth(at).map_or(text.len(), |(b, _)| b)
}

/// Where the word that ends at char `caret` of `text` starts.
fn word_start(text: &str, caret: usize) -> usize {
    let before = text[..byte_of(text, caret)]
        .chars()
        .rev()
        .take_while(|&c| is_word(c))
        .count();
    caret - before
}

/// The word char `at` of `text` is in, or just that char if it's no word's.
fn word_around(text: &str, at: usize) -> Range<usize> {
    let byte = byte_of(text, at);
    if !text[byte..].chars().next().is_some_and(is_word) {
        return at..at + 1;
    }
    let before = text[..byte]
        .chars()
        .rev()
        .take_while(|&c| is_word(c))
        .count();
    let after = text[byte..].chars().take_while(|&c| is_word(c)).count();
    at - before..at + after
}

/// The char under `pos`, relative to `galley`'s top left: `None` past the
/// end of a line, or of the text.
fn char_under(galley: &Galley, pos: Pos2) -> Option<usize> {
    let cursor = galley.cursor_from_pos(pos.to_vec2());
    let edge = galley.pos_from_cursor(cursor);
    if !(edge.min.y..=edge.max.y).contains(&pos.y) {
        return None;
    }
    // The cursor is at the char edge nearest the pointer.
    let at = if pos.x >= edge.min.x {
        cursor.index.0
    } else {
        cursor.index.0.checked_sub(1)?
    };
    let left = galley.pos_from_cursor(CCursor::new(at));
    let right = galley.pos_from_cursor(CCursor::new(at + 1));
    (right.min.y == left.min.y && (left.min.x..right.min.x).contains(&pos.x)).then_some(at)
}

/// The color problems of `severity` are drawn in.
pub(in crate::viewer) fn severity_color(visuals: &Visuals, severity: u8) -> Color32 {
    match severity {
        1 => visuals.error_fg_color,
        2 => visuals.warn_fg_color,
        3 => visuals.hyperlink_color,
        _ => visuals.weak_text_color(),
    }
}

/// A wavy line from `x0` to `x1` about `y`.
fn squiggle(x0: f32, x1: f32, y: f32, color: Color32) -> Shape {
    let mut points = Vec::new();
    let (mut x, mut up) = (x0, true);
    while x < x1 {
        points.push(pos2(x, if up { y - 1. } else { y + 1. }));
        x += 2.;
        up = !up;
    }
    points.push(pos2(x1, if up { y - 1. } else { y + 1. }));
    Shape::line(points, Stroke::new(1., color))
}

impl DocLsp {
    /// Before the editor's text box is drawn: take the server's replies,
    /// and this tab's keys if the box has `focused`. `text` is the
    /// editor's, with its caret at `caret`; `spaces` is how many spaces Tab
    /// indents by, `None` for a tab. Returns what changed in `text`, or
    /// where the caret goes.
    pub(in crate::viewer) fn before(
        &mut self,
        ui: &Ui,
        text: &mut String,
        caret: usize,
        focused: bool,
        spaces: Option<usize>,
    ) -> Option<Edit> {
        self.refresh_diagnostics();
        let mut edit = self.take_replies(text, caret);
        self.take_hover_reply();
        self.take_completion_reply(caret);

        // Anything done hides the popup: it's about what was there.
        let acted = ui.input(|i| {
            let over_popup = i
                .pointer
                .interact_pos()
                .zip(self.hover.shown.as_ref().and_then(|s| s.rect))
                .is_some_and(|(p, r)| r.contains(p));
            i.events.iter().any(|e| {
                matches!(
                    e,
                    Event::Text(_) | Event::Paste(_) | Event::Key { pressed: true, .. }
                )
            }) || (i.pointer.any_pressed() && !over_popup)
        });
        if acted {
            self.hide_hover();
        }

        if let Some(row) = self.completion.as_mut().and_then(|c| c.clicked.take()) {
            if let Some(c) = &mut self.completion {
                c.selected = row;
            }
            edit = self.accept(text, caret).or(edit);
        }
        if focused && edit.is_none() {
            edit = self.keys(ui, text, caret, spaces);
        }
        if let Some(forward) = self.step.take()
            && let Some(at) = self.next_problem(caret, forward)
        {
            self.show_hover(at, true, false);
            edit = Some(Edit {
                caret: at,
                select: 0,
                reveal: true,
                focus: true,
            });
        }
        self.tick(ui.ctx());
        edit
    }

    fn keys(
        &mut self,
        ui: &Ui,
        text: &mut String,
        caret: usize,
        spaces: Option<usize>,
    ) -> Option<Edit> {
        let open = self.completion.as_ref().is_some_and(Completion::open);
        // The word typed is the pick already: Enter and Tab type as usual,
        // so `return` and Enter makes a new line.
        let typed_in_full = self
            .completion
            .as_ref()
            .filter(|c| c.open())
            .is_some_and(|c| c.typed_in_full(&self.text, caret));
        let mac = ui.ctx().os() == OperatingSystem::Mac;
        let (step, accept, complete, define, docs, format, problem) = ui.input_mut(|i| {
            let (mut step, mut accept) = (0, false);
            if open {
                let count = |i: &mut egui::InputState, key| {
                    i.count_and_consume_key(Modifiers::NONE, key) as isize
                };
                step = count(i, Key::ArrowDown) - count(i, Key::ArrowUp);
                step += (count(i, Key::PageDown) - count(i, Key::PageUp)) * LIST_ROWS as isize;
                accept = !typed_in_full
                    && (i.consume_key(Modifiers::NONE, Key::Enter)
                        || i.consume_key(Modifiers::NONE, Key::Tab));
            }
            let complete = i.consume_key(Modifiers::CTRL, Key::Space);
            let define = i.consume_key(Modifiers::NONE, Key::F12);
            let docs = i.consume_key(Modifiers::CTRL, Key::Q);
            let format = i.consume_key(Modifiers::SHIFT | Modifiers::ALT, Key::F)
                || i.consume_key(Modifiers::CTRL | Modifiers::ALT, Key::L);
            // Shifted first: the unshifted shortcut matches shifted keys too.
            let problem = if i.consume_key(Modifiers::SHIFT, Key::F8) {
                Some(false)
            } else if i.consume_key(Modifiers::NONE, Key::F8) {
                Some(true)
            } else {
                None
            };
            // Not also typed: Ctrl+Space's space, or on Mac, Option+Shift+F's `Ï`.
            if complete || (format && mac) {
                i.events.retain(|e| !matches!(e, Event::Text(_)));
            }
            (step, accept, complete, define, docs, format, problem)
        });

        if let Some(c) = &mut self.completion
            && step != 0
            && c.open()
        {
            let last = c.shown.len() as isize - 1;
            let at = c.selected as isize + step;
            // Arrows wrap around; pages stop at the ends.
            c.selected = if step.abs() == 1 {
                at.rem_euclid(last + 1)
            } else {
                at.clamp(0, last)
            } as usize;
            c.reveal = true;
        }
        if accept {
            return self.accept(text, caret);
        }
        if complete {
            self.request_completion(caret, None);
        }
        if define {
            self.request_definition(caret);
        }
        if docs {
            self.show_hover(caret, true, true);
        }
        if format {
            self.request_format(spaces);
        }
        self.step = problem.or(self.step);
        None
    }

    /// After the editor's text box is drawn, in its scroll area: follow
    /// edits, draw the problems, and watch the pointer. `text` is the text
    /// laid out.
    pub(in crate::viewer) fn after(&mut self, ui: &Ui, output: &TextEditOutput, text: &Arc<str>) {
        let changed = !Arc::ptr_eq(&self.text, text);
        self.track(text);

        let pointer = ui.input(|i| i.pointer.hover_pos());
        let on_list = pointer
            .zip(self.completion.as_ref().and_then(|c| c.rect))
            .is_some_and(|(p, r)| r.contains(p));
        // From the state, which stays while a click on the list takes focus.
        let caret = output
            .state
            .cursor
            .char_range()
            .filter(|r| r.primary.index == r.secondary.index)
            .map(|r| r.primary.index.0);
        match caret {
            Some(caret) if output.response.has_focus() || on_list => {
                if changed {
                    self.typed(ui, caret);
                } else if self
                    .completion
                    .as_ref()
                    .is_some_and(|c| word_start(&self.text, caret) != c.start || caret < c.start)
                {
                    self.close_completion();
                }
            }
            _ => self.close_completion(),
        }

        if output.response.clicked()
            && ui.input(|i| i.modifiers.command)
            && let Some(caret) = caret
        {
            self.request_definition(caret);
        }

        let galley = &output.galley;
        let origin = output.galley_pos.to_vec2();
        let clip = ui.clip_rect();
        if let Some(c) = &mut self.completion {
            let rect = galley
                .pos_from_cursor(CCursor::new(c.start))
                .translate(origin);
            c.anchor = clip.intersects(rect).then_some(rect);
        }
        if let Some(shown) = &mut self.hover.shown {
            let start = galley.pos_from_cursor(CCursor::new(shown.word.start));
            let end = galley.pos_from_cursor(CCursor::new(shown.word.end));
            let rect = start.union(end).translate(origin);
            shown.anchor = clip.intersects(rect).then_some(rect);
        }
        self.paint_problems(ui, galley, output.galley_pos);
        self.watch_pointer(ui, output, pointer);
        self.tick(ui.ctx());
    }

    /// The text just changed, with the caret now at `caret`: open, narrow
    /// down or close the completion list.
    fn typed(&mut self, ui: &Ui, caret: usize) {
        let Some(triggers) = lock(&self.server).caps().completion.clone() else {
            return;
        };
        let typed = ui.input(|i| {
            i.events.iter().rev().find_map(|e| match e {
                Event::Text(text) => Some(text.clone()),
                _ => None,
            })
        });
        let text = Arc::clone(&self.text);
        let head = &text[..byte_of(&text, caret)];
        if typed.is_some()
            && let Some(trigger) = triggers
                .iter()
                .find(|t| !t.is_empty() && head.ends_with(t.as_str()))
        {
            self.request_completion(caret, Some(trigger));
            return;
        }
        let start = word_start(&text, caret);
        match &mut self.completion {
            // Still in the word it's for.
            Some(c) if c.start == start => {
                if c.incomplete && typed.is_some() {
                    self.request_completion(caret, None);
                } else if c.request.is_none() {
                    c.filter(&text, caret);
                    if !c.open() {
                        self.close_completion();
                    }
                }
            }
            _ => {
                self.close_completion();
                let word_typed = typed
                    .as_deref()
                    .and_then(|t| t.chars().last())
                    .is_some_and(is_word);
                let digits = head[byte_of(head, start)..].starts_with(|c: char| c.is_ascii_digit());
                if word_typed && caret > start && !digits {
                    self.request_completion(caret, None);
                }
            }
        }
    }

    /// Ask for completions at `caret`, after `trigger` if it was one of the
    /// server's trigger characters.
    fn request_completion(&mut self, caret: usize, trigger: Option<&str>) {
        if lock(&self.server).caps().completion.is_none() {
            return;
        }
        let start = word_start(&self.text, caret);
        let again = self
            .completion
            .as_ref()
            .is_some_and(|c| c.start == start && c.incomplete);
        let mut context = json!({"triggerKind": match trigger {
            Some(_) => 2,
            None if again => 3,
            None => 1,
        }});
        if let Some(trigger) = trigger {
            context["triggerCharacter"] = json!(trigger);
        }
        if let Some(old) = self.completion.as_ref().and_then(Completion::request) {
            lock(&self.server).cancel(old);
        }
        self.flush();
        let position = self.position(caret);
        let params =
            json!({"textDocument": {"uri": self.uri}, "position": position, "context": context});
        let id = lock(&self.server).request("textDocument/completion", params, Kind::Completion);
        let Some(id) = id else {
            self.completion = None;
            return;
        };
        let request = Some((id, Arc::clone(&self.text)));
        match &mut self.completion {
            // As the word grows: the list stays up meanwhile.
            Some(c) if c.start == start => c.request = request,
            _ => {
                self.completion = Some(Completion {
                    request,
                    base: Arc::clone(&self.text),
                    items: Vec::new(),
                    incomplete: false,
                    start,
                    shown: Vec::new(),
                    selected: 0,
                    top: 0,
                    reveal: true,
                    anchor: None,
                    rect: None,
                    clicked: None,
                });
            }
        }
    }

    fn take_completion_reply(&mut self, caret: usize) {
        let Some(id) = self.completion.as_ref().and_then(Completion::request) else {
            return;
        };
        let Some(reply) = lock(&self.server).take_reply(id) else {
            return;
        };
        let text = Arc::clone(&self.text);
        let Some(c) = &mut self.completion else {
            return;
        };
        let base = c.request.take().map(|(_, base)| base);
        match reply {
            Reply::Completion { items, incomplete } => {
                c.items = items;
                c.incomplete = incomplete;
                if let Some(base) = base {
                    c.base = base;
                }
                c.filter(&text, caret);
                if !c.open() && !c.incomplete {
                    self.completion = None;
                }
            }
            _ => self.completion = None,
        }
    }

    fn close_completion(&mut self) {
        if let Some(c) = self.completion.take()
            && let Some(id) = c.request()
        {
            lock(&self.server).cancel(id);
        }
    }

    /// Take the selected completion: put it in `text` in place of the word
    /// being completed, which ends at `caret`, with any other edits it
    /// brings (such as an import).
    fn accept(&mut self, text: &mut String, caret: usize) -> Option<Edit> {
        let c = self.completion.take()?;
        let item = c.items.get(*c.shown.get(c.selected)?)?;
        let encoding = self.encoding();
        let lines = Lines::new(&c.base);
        let (new_text, start) = match &item.text_edit {
            Some(edit) => (
                edit.new_text.clone(),
                edit.range()
                    .map(|r| lines.char_index(&c.base, r.start, encoding)),
            ),
            None => (
                item.insert_text
                    .clone()
                    .unwrap_or_else(|| item.label.clone()),
                None,
            ),
        };
        let start = start.filter(|&s| s <= caret).unwrap_or(c.start).min(caret);
        // A snippet's first placeholder is selected, to type over.
        let (new_text, place) = if item.insert_text_format == Some(2) {
            protocol::strip_snippet(&new_text)
        } else {
            (new_text, None)
        };
        let inserted = new_text.chars().count();
        let place = place.unwrap_or(inserted..inserted);
        let mut edits = vec![(start..caret, new_text)];
        // Before the word, where the text hasn't changed since the list came.
        let mut moved = 0;
        for extra in item.additional_text_edits.iter().flatten() {
            let from = lines.char_index(&c.base, extra.range.start, encoding);
            let to = lines.char_index(&c.base, extra.range.end, encoding);
            if from <= to && to <= start {
                moved += extra.new_text.chars().count() as isize - (to - from) as isize;
                edits.push((from..to, extra.new_text.clone()));
            }
        }
        apply(text, edits);
        Some(Edit {
            caret: start.saturating_add_signed(moved) + place.start,
            select: place.len(),
            reveal: false,
            focus: true,
        })
    }

    /// The first problem after `caret` (before it, if not `forward`),
    /// wrapping around. Hints are left out.
    fn next_problem(&self, caret: usize, forward: bool) -> Option<usize> {
        let starts = self
            .diagnostics
            .iter()
            .filter(|d| d.severity < 4)
            .map(|d| d.range.start);
        if forward {
            starts.clone().find(|&s| s > caret).or_else(|| starts.min())
        } else {
            starts
                .clone()
                .filter(|&s| s < caret)
                .max()
                .or_else(|| starts.max())
        }
    }

    /// Open the popup for char `at`: its problems, and if `docs`, the
    /// server's docs. `sticky` keeps it as the pointer moves.
    fn show_hover(&mut self, at: usize, sticky: bool, docs: bool) {
        self.hide_hover();
        let problems: Vec<(u8, String)> = self
            .diagnostics
            .iter()
            .filter(|d| d.range.start <= at && at < d.range.end.max(d.range.start + 1))
            .map(|d| (d.severity, d.message.clone()))
            .collect();
        let byte = byte_of(&self.text, at);
        let on_code = self.text[byte..]
            .chars()
            .next()
            .is_some_and(|c| !c.is_whitespace());
        let request = if docs && on_code && lock(&self.server).caps().hover {
            self.request_at("textDocument/hover", at, Kind::Hover)
        } else {
            None
        };
        if problems.is_empty() && request.is_none() {
            return;
        }
        self.hover.request = request;
        self.hover.shown = Some(Shown {
            word: word_around(&self.text, at),
            problems,
            docs: None,
            sticky,
            anchor: None,
            rect: None,
        });
    }

    fn take_hover_reply(&mut self) {
        let Some(reply) = self
            .hover
            .request
            .and_then(|id| lock(&self.server).take_reply(id))
        else {
            return;
        };
        self.hover.request = None;
        if let Some(shown) = &mut self.hover.shown {
            if let Reply::Hover(docs) = reply {
                shown.docs = docs;
            }
            if shown.problems.is_empty() && shown.docs.is_none() {
                self.hover.shown = None;
            }
        }
    }

    /// Close the popup, and don't reopen it till the pointer moves.
    fn hide_hover(&mut self) {
        if let Some(id) = self.hover.request.take() {
            lock(&self.server).cancel(id);
        }
        self.hover.shown = None;
        self.hover.rest = None;
    }

    /// Open the popup once the pointer rests on code, and close it once
    /// the pointer leaves the word and the popup.
    fn watch_pointer(&mut self, ui: &Ui, output: &TextEditOutput, pointer: Option<Pos2>) {
        let down = ui.input(|i| i.pointer.any_down());
        let over = pointer
            .filter(|_| output.response.hovered() && !down)
            .and_then(|p| char_under(&output.galley, p - output.galley_pos.to_vec2()));
        if let Some(shown) = &self.hover.shown
            && !shown.sticky
        {
            // With room for the gap between them.
            let on_popup = pointer
                .zip(shown.rect)
                .is_some_and(|(p, r)| r.expand(6.).contains(p));
            let on_word = over.is_some_and(|at| shown.word.contains(&at));
            if !on_popup && !on_word {
                self.hide_hover();
            }
        }
        // Moved onto another char, or it scrolled under the pointer: wait
        // again. After a key hid the popup, only once the pointer moves.
        let moved = pointer != self.hover.pointer;
        self.hover.pointer = pointer;
        self.hover.rest = match (over, self.hover.rest) {
            (Some(at), Some((was, since))) if at == was => Some((was, since)),
            (Some(at), rest) if moved || rest.is_some() => Some((at, Instant::now())),
            _ => None,
        };
        if self.completion.as_ref().is_some_and(Completion::open) || self.hover.shown.is_some() {
            return;
        }
        if let Some((at, since)) = self.hover.rest {
            let wait = HOVER_DELAY.saturating_sub(since.elapsed());
            if wait.is_zero() {
                self.hover.rest = None;
                self.show_hover(at, false, true);
            } else {
                ui.ctx().request_repaint_after(wait);
            }
        }
    }

    /// Underline the problems in view in `galley`, drawn at `pos`.
    fn paint_problems(&self, ui: &Ui, galley: &Galley, pos: Pos2) {
        if self.diagnostics.is_empty() {
            return;
        }
        let clip = ui.clip_rect();
        let visible = Rangef::new(clip.top() - pos.y, clip.bottom() - pos.y);
        let font = TextStyle::Monospace.resolve(ui.style());
        let char_width = ui.fonts_mut(|f| f.glyph_width(&font, 'x'));
        let mut shapes = Vec::new();
        let mut start = 0;
        for row in &galley.rows {
            let rect = row.rect();
            if rect.min.y >= visible.max {
                break;
            }
            let end = start + row.glyphs.len();
            if rect.max.y > visible.min {
                let x = |at: usize| {
                    pos.x + row.pos.x + row.x_offset(CharIndex(at.clamp(start, end) - start))
                };
                for d in &self.diagnostics {
                    if d.range.start > end {
                        break;
                    }
                    // A problem between chars, such as a missing `;`, is
                    // underlined a char wide.
                    let last = d.range.end.max(d.range.start + 1);
                    if last <= start {
                        continue;
                    }
                    let (x0, mut x1) = (x(d.range.start), x(last));
                    if x1 - x0 < 1. {
                        x1 = x0 + char_width;
                    }
                    let color = severity_color(ui.visuals(), d.severity);
                    shapes.push(squiggle(x0, x1, pos.y + rect.max.y - 1.5, color));
                }
            }
            start = end + usize::from(row.ends_with_newline);
        }
        ui.painter().extend(shapes);
    }

    /// Escape, while the popup or the completion list is up: close it.
    /// Returns whether it did, for the editor to take back the focus egui
    /// gave up on Escape. Call before anything else takes Escape.
    pub(in crate::viewer) fn take_escape(&mut self, ui: &Ui) -> bool {
        let up = self
            .completion
            .as_ref()
            .is_some_and(|c| c.open() && c.anchor.is_some())
            || self.hover.shown.as_ref().is_some_and(|s| s.rect.is_some());
        if up && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            self.close_completion();
            self.hide_hover();
            return true;
        }
        false
    }

    /// The completion list and the popup, over the text. Call after
    /// drawing the text box. `doc` is the tab's id. `cache` renders the
    /// docs; it's made the first time there are any.
    pub(in crate::viewer) fn popups(
        &mut self,
        ctx: &Context,
        doc: Id,
        cache: &mut Option<CommonMarkCache>,
    ) {
        self.completion_list(ctx, doc);
        self.hover_popup(ctx, doc, cache);
    }

    fn completion_list(&mut self, ctx: &Context, doc: Id) {
        let Some(c) = &mut self.completion else {
            return;
        };
        let Some(anchor) = c.anchor.filter(|_| c.open()) else {
            c.rect = None;
            return;
        };
        let area = Area::new(Id::new(("lsp_completion", doc)))
            .order(Order::Foreground)
            .fixed_pos(anchor.left_bottom() + vec2(0., 2.))
            .constrain(true);
        let response = area.show(ctx, |ui| {
            Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_width(LIST_WIDTH);
                let font = TextStyle::Monospace.resolve(ui.style());
                let row_height = ui.text_style_height(&TextStyle::Monospace)
                    + 2. * ui.spacing().button_padding.y;
                let row_step = row_height + ui.spacing().item_spacing.y;
                let mut scroll = ScrollArea::vertical()
                    .max_height(row_step * LIST_ROWS as f32)
                    .auto_shrink([false, true]);
                if std::mem::take(&mut c.reveal) {
                    if c.selected < c.top {
                        c.top = c.selected;
                    } else if c.selected >= c.top + LIST_ROWS {
                        c.top = c.selected + 1 - LIST_ROWS;
                    }
                    scroll = scroll.vertical_scroll_offset(c.top as f32 * row_step);
                }
                let output = scroll.show_rows(ui, row_height, c.shown.len(), |ui, rows| {
                    let width = ui.available_width();
                    for row in rows {
                        let item = &c.items[c.shown[row]];
                        let mut job = LayoutJob::default();
                        job.append(
                            &item.label,
                            0.,
                            TextFormat::simple(font.clone(), ui.visuals().strong_text_color()),
                        );
                        if let Some(detail) = item.detail() {
                            let format =
                                TextFormat::simple(font.clone(), ui.visuals().weak_text_color());
                            job.append(detail, font.size, format);
                        }
                        job.wrap = TextWrapping::truncate_at_width(
                            width - 2. * ui.spacing().button_padding.x,
                        );
                        let button = Button::selectable(row == c.selected, job)
                            .min_size(vec2(width, row_height));
                        if ui.add(button).clicked() {
                            c.clicked = Some(row);
                        }
                    }
                });
                c.top = (output.state.offset.y / row_step).round() as usize;
            });
        });
        c.rect = Some(response.response.rect);
    }

    fn hover_popup(&mut self, ctx: &Context, doc: Id, cache: &mut Option<CommonMarkCache>) {
        let Some(shown) = &mut self.hover.shown else {
            return;
        };
        let has_content = !shown.problems.is_empty() || shown.docs.is_some();
        let Some(anchor) = shown.anchor.filter(|_| has_content) else {
            shown.rect = None;
            return;
        };
        let area = Area::new(Id::new(("lsp_hover", doc)))
            .order(Order::Foreground)
            .fixed_pos(anchor.left_bottom() + vec2(0., 4.))
            .constrain(true);
        let response = area.show(ctx, |ui| {
            Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_max_width(HOVER_WIDTH);
                ScrollArea::vertical()
                    .max_height(HOVER_HEIGHT)
                    .show(ui, |ui| {
                        for (severity, message) in &shown.problems {
                            ui.label(
                                RichText::new(message)
                                    .color(severity_color(ui.visuals(), *severity)),
                            );
                        }
                        if let Some(docs) = &shown.docs {
                            if !shown.problems.is_empty() {
                                ui.separator();
                            }
                            let cache = cache.get_or_insert_with(CommonMarkCache::default);
                            CommonMarkViewer::new().show(ui, cache, docs);
                        }
                    });
            });
        });
        shown.rect = Some(response.response.rect);
    }

    /// The server's state, the problem count and any notice, in the
    /// toolbar. Clicking the count goes to the next problem.
    pub(in crate::viewer) fn toolbar(&mut self, ui: &mut Ui) {
        let visuals = ui.visuals().clone();
        let server = Arc::clone(&self.server);
        let server = lock(&server);
        let name = &server.name;
        let mut tip = Vec::new();
        let (text, color, stopped) = match server.health() {
            Health::Starting => (
                format!("{name}: starting…"),
                visuals.weak_text_color(),
                false,
            ),
            Health::Ready => (name.clone(), visuals.weak_text_color(), false),
            Health::Busy(work) => {
                tip.push(work.to_owned());
                (
                    format!("{name}: {}", shorten(work, 40)),
                    visuals.weak_text_color(),
                    false,
                )
            }
            Health::Stopped(why) => {
                tip.push(why.to_owned());
                let first = why.lines().next().unwrap_or_default();
                (shorten(first, 90), visuals.error_fg_color, true)
            }
        };
        if let Some(message) = server.message() {
            tip.push(message.to_owned());
        }
        tip.push(format!("Project: {}", server.root.display()));
        drop(server);
        ui.label(RichText::new(text).color(color))
            .on_hover_text(tip.join("\n\n"));
        if stopped
            && ui
                .button("Restart")
                .on_hover_text("Start the language server again")
                .clicked()
        {
            self.restart();
        }

        let count = |severity| {
            self.diagnostics
                .iter()
                .filter(|d| d.severity == severity)
                .count()
        };
        let (errors, warnings) = (count(1), count(2));
        if errors + warnings > 0 {
            let plural =
                |n: usize, what: &str| format!("{n} {what}{}", if n == 1 { "" } else { "s" });
            let (text, color) = match (errors, warnings) {
                (0, w) => (plural(w, "warning"), visuals.warn_fg_color),
                (e, 0) => (plural(e, "error"), visuals.error_fg_color),
                (e, w) => (
                    format!("{}, {}", plural(e, "error"), plural(w, "warning")),
                    visuals.error_fg_color,
                ),
            };
            let button = Button::new(RichText::new(text).color(color)).frame_when_inactive(false);
            if ui
                .add(button)
                .on_hover_text("Go to the next problem (F8; Shift+F8 for the one before)")
                .clicked()
            {
                self.step = Some(true);
            }
        }
        if let Some(notice) = &self.notice {
            ui.label(RichText::new(notice).color(visuals.warn_fg_color));
        }
    }

    /// Each line's worst problem's severity, or 0, for the line numbers.
    pub(in crate::viewer) fn line_marks(&mut self) -> &[u8] {
        self.marks()
    }
}

/// `text` cut to `max` chars, with `…` for what's cut.
fn shorten(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words() {
        let text = "let foo_bar = x.y;";
        assert_eq!(word_start(text, 11), 4);
        assert_eq!(word_start(text, 12), 12);
        assert_eq!(word_around(text, 6), 4..11);
        assert_eq!(word_around(text, 3), 3..4);
        assert_eq!(word_start("é1é", 3), 0);
    }

    #[test]
    fn matches_rank() {
        assert_eq!(score("foo_bar", "foo", "foo"), Some(0));
        assert_eq!(score("FooBar", "foob", "foob"), Some(1));
        assert_eq!(score("xfoo", "foo", "foo"), Some(2));
        assert_eq!(score("f_o_o", "foo", "foo"), Some(3));
        assert_eq!(score("bar", "foo", "foo"), None);
        assert_eq!(score("anything", "", ""), Some(0));
    }
}
