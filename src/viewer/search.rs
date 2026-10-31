//! Find in a text, source or Markdown viewer tab: the bar under its
//! toolbar, the keys that drive it, and the matches it highlights. In Edit
//! mode, it can replace them too.
//!
//! Source and plain text are searched as one string, and their matches
//! located in its laid-out galley each pass. Markdown is drawn by
//! `egui_commonmark`, which only paints what's on screen, so its text is
//! found by drawing the document once more, off screen and unclipped,
//! whenever the query or the layout changes, and searching the text that
//! paints (see [measure]).

use std::{ops::Range, str::Chars, sync::Arc};

use egui::{
    Align, Button, Color32, Context, Event, Galley, Id, Key, LayerId, Modifiers, Order, Pos2,
    Rangef, Rect, RichText, Shape, Stroke, StrokeKind, TextEdit, Ui, UiBuilder, epaint::TextShape,
    layers::ShapeIdx, os::OperatingSystem, pos2, text::CharIndex, vec2,
};

use crate::ui::line_edit;

/// Stop counting here, so a one-letter query in a big file stays quick.
const MAX_MATCHES: usize = 100_000;

const BOX_WIDTH: f32 = 240.;

/// Highlights, premultiplied: every match in yellow, the current one in
/// orange. Translucent, as Markdown's are drawn over the text.
const MATCH_FILL: Color32 = Color32::from_rgba_premultiplied(75, 60, 0, 75);
const CURRENT_FILL: Color32 = Color32::from_rgba_premultiplied(150, 70, 0, 150);
const CURRENT_STROKE: Color32 = Color32::from_rgb(255, 140, 0);

/// Where Markdown is laid out to be searched: far left of the window, so
/// nothing drawn there can be seen or clicked.
const OFFSCREEN: Pos2 = pos2(-50_000., 0.);

/// One viewer tab's find bar and its matches.
#[derive(Default)]
pub(super) struct Search {
    query: String,
    case_sensitive: bool,
    /// Skip spaces, underscores and dashes, so `foo bar` finds `foo_bar`,
    /// `foo-bar` and `foobar`.
    ignore_separators: bool,
    found: Found,
    /// The match shown in the stronger color, which next / previous step
    /// from. Valid while there are any.
    current: usize,
    /// What `found` was found with, and in.
    found_for: Option<(Needle, Layout)>,
    /// The query changed: make the first match from the top of the view
    /// current, and scroll to it.
    from_view: bool,
    /// Scroll the current match into view.
    reveal: bool,
    /// Focus the box, with its text selected.
    focus: bool,
    /// Markdown: search the layout again next pass, as a table or image
    /// can take a pass to settle.
    remeasure: bool,
    /// Editing can reuse an allocation's address; don't rely on it alone.
    text_changed: bool,
    /// Show the replace box, under the find bar, in Edit mode.
    replacing: bool,
    replacement: String,
    /// Focus the replace box, with its text selected.
    focus_replace: bool,
    /// Asked for in the bar, for the editor to make before it's drawn.
    replace: Option<Replace>,
    /// After a replacement: make the first match from this char current.
    after: Option<usize>,
}

#[derive(Clone, Copy, PartialEq)]
enum Replace {
    /// The current match, then step to the next.
    Current,
    All,
}

/// The matches found.
#[derive(Default)]
enum Found {
    #[default]
    None,
    /// Source and plain text: char (not byte) ranges of the text, in order.
    Chars(Vec<Range<usize>>),
    /// Markdown: the area each piece of a match covers, relative to the top
    /// left of the document, with the index of its match. A match wrapped
    /// onto two rows has two.
    Rects {
        rects: Vec<(usize, Rect)>,
        count: usize,
    },
}

impl Found {
    fn len(&self) -> usize {
        match self {
            Self::None => 0,
            Self::Chars(ranges) => ranges.len(),
            Self::Rects { count, .. } => *count,
        }
    }
}

/// What a search looks for.
#[derive(Clone, PartialEq)]
struct Needle {
    /// The query's chars, folded to lowercase unless matching case, less
    /// separators if they're ignored.
    chars: Vec<char>,
    case_sensitive: bool,
    ignore_separators: bool,
}

/// What a search ran over: if this changes, it runs again.
#[derive(PartialEq)]
pub(super) enum Layout {
    /// Source or plain text, by address: each reload makes a new one.
    Text(usize, usize),
    /// Markdown as laid out: the text, how wide and tall it's drawn, and
    /// which `<details>` sections are open.
    Markdown {
        text: (usize, usize),
        width: f32,
        height: f32,
        open: Vec<bool>,
    },
}

impl Layout {
    fn address(text: &Arc<str>) -> (usize, usize) {
        (Arc::as_ptr(text) as *const u8 as usize, text.len())
    }

    pub(super) fn markdown(text: &Arc<str>, width: f32, height: f32, open: &[bool]) -> Self {
        Self::Markdown {
            text: Self::address(text),
            width,
            height,
            open: open.to_vec(),
        }
    }
}

fn is_separator(c: char) -> bool {
    matches!(c, ' ' | '\t' | '_' | '-')
}

fn fold(c: char) -> char {
    if c.is_ascii() {
        c.to_ascii_lowercase()
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

impl Needle {
    /// `None` for an empty query. A query of only separators is searched
    /// for as typed, rather than finding nothing.
    fn new(query: &str, case_sensitive: bool, ignore_separators: bool) -> Option<Self> {
        let fold = |c| if case_sensitive { c } else { fold(c) };
        let mut chars: Vec<char> = query
            .chars()
            .filter(|&c| !(ignore_separators && is_separator(c)))
            .map(fold)
            .collect();
        let mut ignore_separators = ignore_separators;
        if chars.is_empty() {
            chars = query.chars().map(fold).collect();
            ignore_separators = false;
        }
        (!chars.is_empty()).then_some(Self {
            chars,
            case_sensitive,
            ignore_separators,
        })
    }

    /// How many chars at the start of `text` make a match, if they do.
    fn match_len(&self, mut text: Chars) -> Option<usize> {
        let mut len = 0;
        for &want in &self.chars {
            loop {
                let c = text.next()?;
                len += 1;
                let c = if self.case_sensitive { c } else { fold(c) };
                if c == want {
                    break;
                }
                if !(self.ignore_separators && is_separator(c)) {
                    return None;
                }
            }
        }
        Some(len)
    }
}

/// Where `needle` is in `text`: char ranges, in order and not overlapping,
/// at most `limit` of them. A match starts and ends on one of the needle's
/// chars, never a skipped separator.
fn find(text: &str, needle: &Needle, limit: usize) -> Vec<Range<usize>> {
    let mut found = Vec::new();
    let mut chars = text.chars();
    let mut index = 0;
    while found.len() < limit {
        let from = chars.clone();
        let Some(c) = chars.next() else {
            break;
        };
        if !(needle.ignore_separators && is_separator(c))
            && let Some(len) = needle.match_len(from.clone())
        {
            found.push(index..index + len);
            chars = from;
            chars.nth(len - 1);
            index += len;
        } else {
            index += 1;
        }
    }
    found
}

/// Call `f` with each piece of `ranges` (sorted char ranges of `galley`'s
/// text) on a row of `galley` within `visible` (a y range, relative to the
/// galley), and the index of the range it's from. Pieces are relative to
/// the galley's top left.
pub(super) fn row_pieces(
    galley: &Galley,
    ranges: &[Range<usize>],
    visible: Rangef,
    mut f: impl FnMut(usize, Rect),
) {
    let Some(last) = ranges.last() else {
        return;
    };
    let mut start = 0;
    for row in &galley.rows {
        let rect = row.rect();
        if rect.min.y >= visible.max || start >= last.end {
            break;
        }
        let end = start + row.glyphs.len();
        if rect.max.y > visible.min {
            let first = ranges.partition_point(|r| r.end <= start);
            for (i, r) in ranges.iter().enumerate().skip(first) {
                if r.start >= end {
                    break;
                }
                let (a, b) = (r.start.max(start) - start, r.end.min(end) - start);
                if a < b {
                    let x = |column| row.pos.x + row.x_offset(CharIndex(column));
                    f(i, Rect::from_x_y_ranges(x(a)..=x(b), rect.y_range()));
                }
            }
        }
        start = end + usize::from(row.ends_with_newline);
    }
}

/// The char index the first row of `galley` showing below `y` (relative to
/// the galley) starts at.
fn char_at_y(galley: &Galley, y: f32) -> usize {
    let mut start = 0;
    for row in &galley.rows {
        if row.rect().max.y > y {
            break;
        }
        start += row.char_count_including_newline().0;
    }
    start
}

fn highlight(rect: Rect, current: bool) -> Shape {
    if current {
        Shape::Vec(vec![
            Shape::rect_filled(rect, 2., CURRENT_FILL),
            Shape::rect_stroke(
                rect,
                2.,
                Stroke::new(1., CURRENT_STROKE),
                StrokeKind::Outside,
            ),
        ])
    } else {
        Shape::rect_filled(rect, 2., MATCH_FILL)
    }
}

fn box_id(doc: Id) -> Id {
    Id::new(("viewer_find", doc))
}

fn replace_box_id(doc: Id) -> Id {
    Id::new(("viewer_replace", doc))
}

impl Search {
    fn needle(&self) -> Option<Needle> {
        Needle::new(&self.query, self.case_sensitive, self.ignore_separators)
    }

    /// Keep `found` for `needle` in `layout`. Returns whether it's stale,
    /// i.e. needs finding again; if so, notes what changed.
    fn stale(&mut self, needle: &Needle, layout: &Layout) -> bool {
        let Some((was_needle, was_layout)) = &self.found_for else {
            self.from_view = true;
            return true;
        };
        if was_needle != needle {
            self.from_view = true;
            return true;
        }
        was_layout != layout
    }

    /// Record a new set of matches, found with `needle` in `layout`.
    fn set_found(&mut self, ctx: &Context, found: Found, needle: Needle, layout: Layout) {
        self.found = found;
        self.current = self.current.min(self.found.len().saturating_sub(1));
        self.found_for = Some((needle, layout));
        // For the count in the bar, already drawn this pass.
        ctx.request_repaint();
    }

    fn clear(&mut self) {
        self.found = Found::None;
        self.found_for = None;
        self.from_view = false;
        self.reveal = false;
        self.remeasure = false;
    }

    /// Step to the next match, or the previous one, wrapping around.
    fn step(&mut self, forward: bool) {
        let n = self.found.len();
        if n == 0 {
            return;
        }
        self.current = if forward {
            (self.current + 1) % n
        } else {
            (self.current + n - 1) % n
        };
        self.reveal = true;
    }

    /// Act on the find keys pressed in this window this pass, and remove
    /// their key events, with Replace's and Escape's if `editing` `doc`.
    /// Call before the bar is drawn.
    pub(super) fn keys(&mut self, ctx: &Context, doc: Id, editing: bool) {
        // Escape from the text, or the find or replace box, which egui has
        // already unfocused: close find and replace, back in the text.
        let left = [super::editor::id(doc), box_id(doc), replace_box_id(doc)];
        if editing
            && ctx.memory(|m| left.iter().any(|&id| m.had_focus_last_frame(id)))
            && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
        {
            self.query.clear();
            self.replacing = false;
            ctx.memory_mut(|m| m.request_focus(super::editor::id(doc)));
        }
        // As VS Code: Alt+C, or Cmd+Option+C on Mac, where Option+C types ç.
        let case_key = if ctx.os() == OperatingSystem::Mac {
            Modifiers::MAC_CMD | Modifiers::ALT
        } else {
            Modifiers::ALT
        };
        ctx.input_mut(|i| {
            if i.consume_key(Modifiers::COMMAND, Key::F) {
                self.focus = true;
            }
            if editing && i.consume_key(Modifiers::COMMAND, Key::R) {
                self.replacing = true;
                // What to find first, if there's nothing yet.
                if self.query.is_empty() {
                    self.focus = true;
                } else {
                    self.focus_replace = true;
                }
            }
            // Shifted first: an unshifted shortcut matches shifted keys too.
            if i.consume_key(Modifiers::SHIFT, Key::F3)
                || i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::G)
            {
                self.step(false);
            } else if i.consume_key(Modifiers::NONE, Key::F3)
                || i.consume_key(Modifiers::COMMAND, Key::G)
            {
                self.step(true);
            }
            if i.consume_key(case_key, Key::C) {
                self.case_sensitive = !self.case_sensitive;
                // Don't also type the letter into the box.
                i.events.retain(|e| !matches!(e, Event::Text(_)));
            }
        });
    }

    /// The find bar: query box, clear, case and separator toggles, previous
    /// and next, and the match count. If `editing`, a Replace toggle, which
    /// shows the replace box under them.
    pub(super) fn bar(&mut self, ui: &mut Ui, doc: Id, editing: bool) {
        let id = box_id(doc);
        if self.focus {
            // Select the query, so typing replaces it.
            line_edit::select_all(ui.ctx(), id, &self.query);
        }
        let count = self.found.len();
        // Not while the first search for a new query is pending, so "No
        // results" doesn't flash up as you start typing.
        let none_found = self.found_for.is_some() && count == 0;
        ui.horizontal(|ui| {
            let mut edit = TextEdit::singleline(&mut self.query)
                .id(id)
                .hint_text("Find (Ctrl+F)")
                .desired_width(BOX_WIDTH)
                // Enter steps to the next match, keeping focus.
                .return_key(None);
            if none_found {
                edit = edit.text_color(ui.visuals().error_fg_color);
            }
            let response = ui.add(edit);
            if std::mem::take(&mut self.focus) {
                response.request_focus();
            }
            if response.has_focus() {
                let step = ui.input_mut(|i| {
                    if i.consume_key(Modifiers::SHIFT, Key::Enter) {
                        Some(false)
                    } else if i.consume_key(Modifiers::NONE, Key::Enter) {
                        Some(true)
                    } else {
                        None
                    }
                });
                if let Some(forward) = step {
                    self.step(forward);
                }
            }
            // egui drops focus on Escape before widgets run.
            if response.lost_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                self.query.clear();
            }
            if ui
                .add_enabled(!self.query.is_empty(), Button::new("x").small())
                .on_hover_text("Clear the search (Esc)")
                .clicked()
            {
                self.query.clear();
            }
            if editing
                && ui
                    .toggle_value(&mut self.replacing, "Replace")
                    .on_hover_text("Show or hide the replace box (Ctrl+R)")
                    .clicked()
                && self.replacing
            {
                self.focus_replace = true;
            }

            ui.separator();
            ui.toggle_value(&mut self.case_sensitive, "Aa")
                .on_hover_text("Match case (Alt+C)");
            ui.toggle_value(&mut self.ignore_separators, "a_b")
                .on_hover_text(
                    "Ignore spaces, underscores and dashes: \"foo bar\" also finds \
                     foo_bar, foo-bar and foobar",
                );
            ui.separator();

            if ui
                .add_enabled(count > 0, Button::new("⬆"))
                .on_hover_text("Previous match (Shift+F3, Shift+Enter)")
                .clicked()
            {
                self.step(false);
            }
            if ui
                .add_enabled(count > 0, Button::new("⬇"))
                .on_hover_text("Next match (F3, Enter)")
                .clicked()
            {
                self.step(true);
            }
            if count > 0 {
                let more = if count >= MAX_MATCHES { "+" } else { "" };
                ui.label(format!("{} of {count}{more}", self.current + 1));
            } else if none_found {
                ui.label(RichText::new("No results").color(ui.visuals().error_fg_color));
            }
        });
        if editing && self.replacing {
            self.replace_bar(ui, doc, count);
        }
    }

    /// The replace box, under the find box, and its Replace and Replace all
    /// buttons, for `count` matches.
    fn replace_bar(&mut self, ui: &mut Ui, doc: Id, count: usize) {
        let id = replace_box_id(doc);
        if self.focus_replace {
            line_edit::select_all(ui.ctx(), id, &self.replacement);
        }
        ui.horizontal(|ui| {
            let edit = TextEdit::singleline(&mut self.replacement)
                .id(id)
                .hint_text("Replace with")
                .desired_width(BOX_WIDTH)
                // Enter replaces, keeping focus.
                .return_key(None);
            let response = ui.add(edit);
            if std::mem::take(&mut self.focus_replace) {
                response.request_focus();
            }
            if response.has_focus() && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter))
            {
                self.replace = Some(Replace::Current);
            }
            if ui
                .add_enabled(count > 0, Button::new("Replace"))
                .on_hover_text("Replace the current match, and go to the next (Enter)")
                .clicked()
            {
                self.replace = Some(Replace::Current);
            }
            if ui
                .add_enabled(count > 0, Button::new("Replace all"))
                .on_hover_text("Replace every match")
                .clicked()
            {
                self.replace = Some(Replace::All);
            }
        });
    }

    /// Make the replacement asked for in the bar, if any, in `text`: the
    /// editor's, which the matches were found in. Returns the char after the
    /// last replacement, if any was made.
    pub(super) fn take_replace(&mut self, text: &mut String) -> Option<usize> {
        let replace = self.replace.take()?;
        let needle = self.needle()?;
        // Found again, as Replace all goes past MAX_MATCHES.
        let mut ranges = find(text, &needle, usize::MAX);
        if replace == Replace::Current {
            ranges = vec![ranges.get(self.current)?.clone()];
        }
        if ranges.is_empty() {
            return None;
        }

        let len = self.replacement.chars().count();
        let mut replaced = String::with_capacity(text.len());
        let mut chars = text.chars();
        let (mut at, mut end) = (0, 0);
        for range in ranges {
            replaced.extend(chars.by_ref().take(range.start - at));
            replaced.push_str(&self.replacement);
            chars.nth(range.len() - 1);
            end += range.start - at + len;
            at = range.end;
        }
        replaced.extend(chars);
        *text = replaced;
        // On to the next match, past the text put in, even if that matches.
        if replace == Replace::Current {
            self.after = Some(end);
        }
        Some(end)
    }

    /// Source or plain text: find the query in `text`, if either changed.
    /// Call before [Self::paint_text].
    pub(super) fn find_in_text(&mut self, ctx: &Context, text: &Arc<str>) {
        let changed = std::mem::take(&mut self.text_changed);
        let Some(needle) = self.needle() else {
            self.clear();
            return;
        };
        let (start, len) = Layout::address(text);
        let layout = Layout::Text(start, len);
        if self.stale(&needle, &layout) || changed {
            let found = find(text, &needle, MAX_MATCHES);
            if let Some(after) = self.after.take()
                && !found.is_empty()
            {
                self.current = found.partition_point(|r| r.start < after) % found.len();
                self.reveal = true;
            }
            self.set_found(ctx, Found::Chars(found), needle, layout);
        }
    }

    pub(super) fn text_changed(&mut self) {
        self.text_changed = true;
    }

    /// Source or plain text: highlight the matches showing in `galley`,
    /// drawn at `pos` in `ui`, at `behind` (a shape reserved before the
    /// text), and scroll to the current one if asked.
    pub(super) fn paint_text(&mut self, ui: &Ui, galley: &Galley, pos: Pos2, behind: ShapeIdx) {
        let Found::Chars(ranges) = &self.found else {
            return;
        };
        if ranges.is_empty() {
            self.from_view = false;
            self.reveal = false;
            return;
        }
        let clip = ui.clip_rect();
        if std::mem::take(&mut self.from_view) {
            let top = char_at_y(galley, clip.top() - pos.y);
            self.current = ranges.partition_point(|r| r.start < top) % ranges.len();
            self.reveal = true;
        }
        if std::mem::take(&mut self.reveal) {
            let mut target = Rect::NOTHING;
            let current = self.current..self.current + 1;
            row_pieces(galley, &ranges[current], Rangef::EVERYTHING, |_, r| {
                target = target.union(r);
            });
            ui.scroll_to_rect(target.translate(pos.to_vec2()), Some(Align::Center));
        }

        let visible = Rangef::new(clip.top() - pos.y, clip.bottom() - pos.y);
        let mut shapes = Vec::new();
        row_pieces(galley, ranges, visible, |i, r| {
            shapes.push(highlight(r.translate(pos.to_vec2()), i == self.current));
        });
        ui.painter().set(behind, Shape::Vec(shapes));
    }

    /// Markdown: find the query in the document as laid out, if either
    /// changed, highlight the matches showing and scroll to the current one
    /// if asked. Call in the document's scroll area, after drawing it with
    /// its top left at `top_left`. `layout` describes that drawing, and
    /// `show` draws it again, in the `Ui` given.
    pub(super) fn markdown(
        &mut self,
        ui: &Ui,
        top_left: Pos2,
        layout: Layout,
        doc: Id,
        show: impl FnOnce(&mut Ui),
    ) {
        let Some(needle) = self.needle() else {
            self.clear();
            return;
        };
        let stale = self.stale(&needle, &layout);
        if stale || std::mem::take(&mut self.remeasure) {
            let Layout::Markdown { width, .. } = &layout else {
                return;
            };
            let found = measure(ui, *width, doc, &needle, show);
            self.remeasure = stale;
            self.set_found(ui.ctx(), found, needle, layout);
        }
        let Found::Rects { rects, count } = &self.found else {
            return;
        };
        if *count == 0 {
            self.from_view = false;
            self.reveal = false;
            return;
        }
        let clip = ui.clip_rect();
        let offset = top_left.to_vec2();
        if std::mem::take(&mut self.from_view) {
            let top = clip.top() - top_left.y;
            self.current = rects
                .iter()
                .find(|(_, r)| r.max.y > top)
                .map_or(0, |&(i, _)| i);
            self.reveal = true;
        }
        if std::mem::take(&mut self.reveal) {
            let target = rects
                .iter()
                .filter(|(i, _)| *i == self.current)
                .fold(Rect::NOTHING, |t, (_, r)| t.union(*r));
            ui.scroll_to_rect(target.translate(offset), Some(Align::Center));
        }

        // Over the text: a code block's background would hide them behind.
        let shapes: Vec<Shape> = rects
            .iter()
            .map(|&(i, r)| (i, r.translate(offset)))
            .filter(|(_, r)| clip.intersects(*r))
            .map(|(i, r)| highlight(r, i == self.current))
            .collect();
        ui.painter().add(Shape::Vec(shapes));
    }
}

/// Draw a Markdown document with `show`, `width` wide, off screen and
/// unclipped so all of its text paints, then find `needle` in that text.
/// It's drawn on a layer of its own, whose shapes are taken back before
/// they're ever shown.
fn measure(ui: &Ui, width: f32, doc: Id, needle: &Needle, show: impl FnOnce(&mut Ui)) -> Found {
    let mut rects = Vec::new();
    let mut count = 0;
    for text in measure_text(ui, width, doc, show) {
        if count >= MAX_MATCHES {
            break;
        }
        let ranges = find(text.galley.text(), needle, MAX_MATCHES - count);
        row_pieces(&text.galley, &ranges, Rangef::EVERYTHING, |i, r| {
            rects.push((count + i, r.translate(text.pos.to_vec2())));
        });
        count += ranges.len();
    }
    Found::Rects { rects, count }
}

/// All rendered text, including off-screen labels, positioned relative to
/// the document's origin. Shared by find and Markdown's persistent selection.
pub(super) fn measure_text(
    ui: &Ui,
    width: f32,
    doc: Id,
    show: impl FnOnce(&mut Ui),
) -> Vec<TextShape> {
    let ctx = ui.ctx();
    let id = Id::new(("viewer_find_measure", doc));
    let layer = LayerId::new(Order::Background, id);
    let mut measure = Ui::new(
        ctx.clone(),
        id,
        UiBuilder::new()
            .layer_id(layer)
            .max_rect(Rect::from_min_size(OFFSCREEN, vec2(width, f32::INFINITY)))
            .style(Arc::clone(ui.style())),
    );
    measure.set_clip_rect(Rect::EVERYTHING);
    measure.style_mut().interaction.selectable_labels = false;
    show(&mut measure);
    drop(measure);
    let painted = ctx.graphics_mut(|g| std::mem::take(g.entry(layer)));

    let mut texts = Vec::new();
    for clipped in painted.all_entries() {
        collect_text(&clipped.shape, &mut texts);
    }
    texts
        .into_iter()
        .map(|text| {
            let mut text = text.clone();
            text.pos -= OFFSCREEN.to_vec2();
            text
        })
        .collect()
}

/// The text shapes in `shape`, in the order they paint.
fn collect_text<'a>(shape: &'a Shape, out: &mut Vec<&'a TextShape>) {
    match shape {
        Shape::Text(text) => out.push(text),
        Shape::Vec(shapes) => {
            for shape in shapes {
                collect_text(shape, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_current_steps_on_and_replace_all_takes_every_match() {
        let ctx = Context::default();
        let mut search = Search {
            query: "Foo".into(),
            replacement: "foo bar".into(),
            ..Default::default()
        };
        let mut text = String::from("foo 雪 FOO foo");
        search.find_in_text(&ctx, &Arc::from(text.as_str()));
        search.current = 1;

        search.replace = Some(Replace::Current);
        assert_eq!(search.take_replace(&mut text), Some(13));
        assert_eq!(text, "foo 雪 foo bar foo");
        // The next match is current, not the one just put in.
        search.text_changed();
        search.find_in_text(&ctx, &Arc::from(text.as_str()));
        assert_eq!(search.current, 2);

        search.replace = Some(Replace::All);
        assert_eq!(search.take_replace(&mut text), Some(29));
        assert_eq!(text, "foo bar 雪 foo bar bar foo bar");
        // Nothing asked for, or nothing to find: no change.
        assert_eq!(search.take_replace(&mut text), None);
        search.query.clear();
        search.replace = Some(Replace::All);
        assert_eq!(search.take_replace(&mut text), None);
        assert_eq!(text, "foo bar 雪 foo bar bar foo bar");
    }

    #[test]
    fn escape_in_edit_mode_closes_find_and_replace_back_in_the_text() {
        use egui::RawInput;

        use super::super::{Doc, Inner, Kind, editor};

        let path = std::env::temp_dir().join(format!(
            "peregrine-search-escape-{}.txt",
            std::process::id()
        ));
        std::fs::write(&path, "foo bar foo").unwrap();
        let ctx = Context::default();
        let doc = Id::new("doc");
        let mut inner = Inner::default();
        inner
            .docs
            .push(Doc::new(&ctx, doc, path.clone(), Kind::Text));
        inner.docs[0].start_editing(&ctx);
        let mut time = 0.;
        let mut frame = |inner: &mut Inner, events| {
            time += 1.;
            ctx.run_ui(
                RawInput {
                    time: Some(time),
                    events,
                    ..Default::default()
                },
                |ui| inner.show_window(ui),
            )
            .drop_without_applying_deltas();
        };
        let escape = Event::Key {
            key: Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        frame(&mut inner, vec![]);
        for from in [editor::id(doc), box_id(doc), replace_box_id(doc)] {
            let search = &mut inner.docs[0].search;
            search.query = "foo".into();
            search.replacing = true;
            ctx.memory_mut(|m| m.request_focus(from));
            frame(&mut inner, vec![]);
            assert!(ctx.memory(|m| m.has_focus(from)));
            frame(&mut inner, vec![escape.clone()]);
            let search = &inner.docs[0].search;
            assert!(search.query.is_empty());
            assert!(!search.replacing);
            assert!(ctx.memory(|m| m.has_focus(editor::id(doc))));
        }
        let _ = std::fs::remove_file(&path);
    }
}
