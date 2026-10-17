//! Find in a text, source or Markdown viewer window: the bar under its
//! toolbar, the keys that drive it, and the matches it highlights.
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
    Rangef, Rect, RichText, Shape, Stroke, StrokeKind, TextEdit, Ui, UiBuilder, ViewportId,
    epaint::TextShape,
    layers::ShapeIdx,
    os::OperatingSystem,
    pos2,
    text::{CCursor, CCursorRange, CharIndex},
    vec2,
    widgets::text_edit::TextEditState,
};

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

/// One viewer window's find bar and its matches.
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
fn row_pieces(
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

fn box_id(window: ViewportId) -> Id {
    Id::new(("viewer_find", window))
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
    /// their key events. Call before the bar is drawn.
    pub(super) fn keys(&mut self, ctx: &Context) {
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
    /// and next, and the match count.
    pub(super) fn bar(&mut self, ui: &mut Ui, window: ViewportId) {
        let id = box_id(window);
        if self.focus {
            // Select the query, so typing replaces it.
            let mut state = TextEditState::load(ui.ctx(), id).unwrap_or_default();
            let all = CCursorRange::two(CCursor::new(0), CCursor::new(self.query.chars().count()));
            state.cursor.set_char_range(Some(all));
            state.store(ui.ctx(), id);
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
    }

    /// Source or plain text: find the query in `text`, if either changed.
    /// Call before [Self::paint_text].
    pub(super) fn find_in_text(&mut self, ctx: &Context, text: &Arc<str>) {
        let Some(needle) = self.needle() else {
            self.clear();
            return;
        };
        let (start, len) = Layout::address(text);
        let layout = Layout::Text(start, len);
        if self.stale(&needle, &layout) {
            let found = Found::Chars(find(text, &needle, MAX_MATCHES));
            self.set_found(ctx, found, needle, layout);
        }
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
        window: ViewportId,
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
            let found = measure(ui, *width, window, &needle, show);
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
fn measure(
    ui: &Ui,
    width: f32,
    window: ViewportId,
    needle: &Needle,
    show: impl FnOnce(&mut Ui),
) -> Found {
    let ctx = ui.ctx();
    let id = Id::new(("viewer_find_measure", window));
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

    let mut rects = Vec::new();
    let mut count = 0;
    let mut texts = Vec::new();
    for clipped in painted.all_entries() {
        collect_text(&clipped.shape, &mut texts);
    }
    for text in texts {
        if count >= MAX_MATCHES {
            break;
        }
        let ranges = find(text.galley.text(), needle, MAX_MATCHES - count);
        let offset = text.pos - OFFSCREEN;
        row_pieces(&text.galley, &ranges, Rangef::EVERYTHING, |i, r| {
            rects.push((count + i, r.translate(offset)));
        });
        count += ranges.len();
    }
    Found::Rects { rects, count }
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
#[allow(clippy::single_range_in_vec_init)] // Lists of matches, one long.
mod tests {
    use super::*;

    fn search(text: &str, query: &str, case_sensitive: bool, ignore: bool) -> Vec<Range<usize>> {
        match Needle::new(query, case_sensitive, ignore) {
            Some(needle) => find(text, &needle, MAX_MATCHES),
            None => Vec::new(),
        }
    }

    #[test]
    fn finds_ignoring_case_by_default() {
        assert_eq!(
            search("Foo foo FOO", "foo", false, false),
            [0..3, 4..7, 8..11]
        );
        assert_eq!(search("Foo foo FOO", "foo", true, false), [4..7]);
        assert_eq!(search("Foo foo FOO", "FOO", true, false), [8..11]);
    }

    #[test]
    fn matches_do_not_overlap() {
        assert_eq!(search("aaaa", "aa", false, false), [0..2, 2..4]);
    }

    #[test]
    fn ranges_count_chars_not_bytes() {
        assert_eq!(search("héllo wörld", "wör", false, false), [6..9]);
        assert_eq!(search("ÉCOLE école", "école", false, false), [0..5, 6..11]);
    }

    #[test]
    fn ignores_separators() {
        let text = "foo_bar foo-bar foo bar foobar FooBar";
        assert_eq!(
            search(text, "foo bar", false, true),
            [0..7, 8..15, 16..23, 24..30, 31..37]
        );
        assert_eq!(search(text, "foobar", false, false), [24..30, 31..37]);
        assert_eq!(search(text, "foo_bar", false, false), [0..7]);
        // A match neither starts nor ends on a skipped separator.
        assert_eq!(search("_foo_ x", "foo", false, true), [1..4]);
    }

    #[test]
    fn separators_alone_are_searched_as_typed() {
        assert_eq!(search("a -- b", "--", false, true), [2..4]);
    }

    #[test]
    fn does_not_match_across_lines() {
        assert!(search("foo\nbar", "foo bar", false, true).is_empty());
    }

    #[test]
    fn empty_query_finds_nothing() {
        assert!(Needle::new("", false, false).is_none());
        assert!(search("abc", "", false, true).is_empty());
    }

    #[test]
    fn stops_at_the_limit() {
        let needle = Needle::new("a", false, false).unwrap();
        assert_eq!(find("aaaaa", &needle, 3), [0..1, 1..2, 2..3]);
    }

    // The rest draw with a headless egui context, as a viewer window would.

    use std::path::Path;

    use egui::{Context, FontId, Label, RawInput, ScrollArea, epaint::RectShape, text::LayoutJob};
    use egui_commonmark::CommonMarkCache;

    use crate::markdown::{self, Details};

    const SCREEN: Rect = Rect::from_min_max(pos2(0., 0.), pos2(600., 400.));

    fn input(events: Vec<Event>) -> RawInput {
        RawInput {
            screen_rect: Some(SCREEN),
            events,
            ..Default::default()
        }
    }

    /// One pass of `ctx`, its texture updates (fonts) discarded, as there's
    /// no renderer to take them.
    fn run(ctx: &Context, input: RawInput, f: impl FnMut(&mut Ui)) -> egui::FullOutput {
        let mut output = ctx.run_ui(input, f);
        output.textures_delta.clear();
        output
    }

    fn key(key: Key, modifiers: Modifiers) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    fn rect_shapes<'a>(shape: &'a Shape, out: &mut Vec<&'a RectShape>) {
        match shape {
            Shape::Rect(rect) => out.push(rect),
            Shape::Vec(shapes) => shapes.iter().for_each(|s| rect_shapes(s, out)),
            _ => {}
        }
    }

    /// The highlights drawn, and whether each is the current match's.
    fn highlights(output: &egui::FullOutput) -> Vec<(Rect, bool)> {
        let mut rects = Vec::new();
        for clipped in &output.shapes {
            rect_shapes(&clipped.shape, &mut rects);
        }
        rects
            .into_iter()
            .filter(|r| r.fill == MATCH_FILL || r.fill == CURRENT_FILL)
            .map(|r| (r.rect, r.fill == CURRENT_FILL))
            .collect()
    }

    /// Lay out `text` in the monospace font, wrapped at `width`.
    fn galley(ctx: &Context, text: &str, width: f32) -> Arc<Galley> {
        let mut galley = None;
        run(ctx, input(Vec::new()), |ui| {
            galley = Some(ui.fonts_mut(|f| {
                f.layout_job(LayoutJob::simple(
                    text.to_owned(),
                    FontId::monospace(12.),
                    Color32::WHITE,
                    width,
                ))
            }));
        });
        galley.unwrap()
    }

    #[test]
    fn locates_matches_in_a_galley() {
        let ctx = Context::default();
        let galley = galley(&ctx, "xx foo\nbar foo", f32::INFINITY);
        // Where egui puts a text cursor before char `i`.
        let at = |i: usize| galley.pos_from_cursor(CCursor::new(i));
        let mut pieces = Vec::new();
        row_pieces(&galley, &[3..6, 11..14], Rangef::EVERYTHING, |i, r| {
            pieces.push((i, r))
        });
        assert_eq!(pieces.len(), 2);
        let (i, first) = pieces[0];
        assert_eq!(i, 0);
        assert_eq!(first, Rect::from_min_max(at(3).min, at(6).max));
        let (i, second) = pieces[1];
        assert_eq!(i, 1);
        assert_eq!(second, Rect::from_min_max(at(11).min, at(14).max));
        assert!(second.min.y >= first.max.y - 0.01, "on the second line");

        // Only rows within the range given.
        let mut count = 0;
        row_pieces(&galley, &[3..6, 11..14], Rangef::new(-10., 1.), |_, _| {
            count += 1
        });
        assert_eq!(count, 1);
        assert_eq!(char_at_y(&galley, second.center().y), 7);
    }

    #[test]
    fn a_wrapped_match_has_a_piece_per_row() {
        let ctx = Context::default();
        let wide = galley(&ctx, "aaaa", f32::INFINITY).rect.width();
        // "bbbb" wraps onto a row of its own.
        let galley = galley(&ctx, "aaaa bbbb", wide * 1.5);
        assert_eq!(galley.rows.len(), 2);
        let ranges = search("aaaa bbbb", "aa bb", false, true);
        assert_eq!(ranges, [2..7]);
        let mut pieces = Vec::new();
        row_pieces(&galley, &ranges, Rangef::EVERYTHING, |i, r| {
            pieces.push((i, r))
        });
        assert_eq!(pieces.len(), 2);
        assert!(pieces.iter().all(|&(i, r)| i == 0 && r.width() > 0.));
        assert!(pieces[1].1.min.y > pieces[0].1.min.y);
    }

    /// Passes, a sixtieth of a second each, that see a scroll's animation
    /// through.
    const SETTLE_PASSES: usize = 40;

    /// Draw `text` as a source window would, with `search`'s highlights.
    fn text_pass(
        ctx: &Context,
        search: &mut Search,
        text: &Arc<str>,
        events: Vec<Event>,
    ) -> egui::FullOutput {
        run(ctx, input(events), |ui| {
            search.keys(ui.ctx());
            search.bar(ui, ViewportId::ROOT);
            ScrollArea::vertical()
                .id_salt("text")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    search.find_in_text(ui.ctx(), text);
                    let galley = ui.fonts_mut(|f| {
                        f.layout_job(LayoutJob::simple(
                            text.to_string(),
                            FontId::monospace(12.),
                            Color32::WHITE,
                            f32::INFINITY,
                        ))
                    });
                    let behind = ui.painter().add(Shape::Noop);
                    let pos = ui.add(Label::new(Arc::clone(&galley))).rect.min;
                    search.paint_text(ui, &galley, pos, behind);
                });
        })
    }

    #[test]
    fn steps_through_source_text_and_scrolls_to_matches() {
        let mut text = String::from("fn find_me() {}\n");
        for i in 0..300 {
            text.push_str(&format!("let line_{i} = {i};\n"));
        }
        text.push_str("// Find-Me at the end\n");
        let text: Arc<str> = text.into();
        let ctx = Context::default();
        let mut search = Search {
            query: "find me".to_owned(),
            ignore_separators: true,
            ..Default::default()
        };

        let output = text_pass(&ctx, &mut search, &text, Vec::new());
        assert_eq!(search.found.len(), 2);
        assert_eq!(search.current, 0);
        let drawn = highlights(&output);
        assert_eq!(drawn.len(), 1, "only the match on screen");
        assert!(drawn[0].1, "the first match is current");

        // F3 to the last match, which scrolls down to it.
        text_pass(
            &ctx,
            &mut search,
            &text,
            vec![key(Key::F3, Modifiers::NONE)],
        );
        assert_eq!(search.current, 1);
        let mut output = text_pass(&ctx, &mut search, &text, Vec::new());
        for _ in 0..SETTLE_PASSES {
            output = text_pass(&ctx, &mut search, &text, Vec::new());
        }
        let drawn = highlights(&output);
        assert!(
            drawn
                .iter()
                .any(|&(r, current)| current && SCREEN.contains_rect(r)),
            "the current match is scrolled on screen: {drawn:?}"
        );

        // Shift+F3 back, wrapping around.
        text_pass(
            &ctx,
            &mut search,
            &text,
            vec![key(Key::F3, Modifiers::SHIFT)],
        );
        assert_eq!(search.current, 0);
        text_pass(
            &ctx,
            &mut search,
            &text,
            vec![key(Key::F3, Modifiers::SHIFT)],
        );
        assert_eq!(search.current, 1);

        // Matching case finds only the first.
        search.case_sensitive = true;
        search.query = "find_me".to_owned();
        search.ignore_separators = false;
        text_pass(&ctx, &mut search, &text, Vec::new());
        assert_eq!(search.found.len(), 1);
    }

    #[test]
    fn keys_drive_the_find_box() {
        let text: Arc<str> = "one two one two one".into();
        let ctx = Context::default();
        let mut search = Search::default();
        let focused = |ctx: &Context| ctx.memory(|m| m.has_focus(box_id(ViewportId::ROOT)));

        text_pass(
            &ctx,
            &mut search,
            &text,
            vec![key(Key::F, Modifiers::COMMAND)],
        );
        assert!(focused(&ctx));
        text_pass(&ctx, &mut search, &text, vec![Event::Text("ONE".into())]);
        text_pass(&ctx, &mut search, &text, Vec::new());
        assert_eq!(search.query, "ONE");
        assert_eq!(search.found.len(), 3);
        assert_eq!(search.current, 0);

        // Enter and Shift+Enter step, and keep the box focused.
        text_pass(
            &ctx,
            &mut search,
            &text,
            vec![key(Key::Enter, Modifiers::NONE)],
        );
        assert_eq!(search.current, 1);
        text_pass(
            &ctx,
            &mut search,
            &text,
            vec![key(Key::Enter, Modifiers::NONE)],
        );
        text_pass(
            &ctx,
            &mut search,
            &text,
            vec![key(Key::Enter, Modifiers::SHIFT)],
        );
        assert_eq!(search.current, 1);
        assert!(focused(&ctx));

        // Alt+C matches case, without typing a "c".
        let toggle = vec![key(Key::C, Modifiers::ALT), Event::Text("c".into())];
        text_pass(&ctx, &mut search, &text, toggle);
        text_pass(&ctx, &mut search, &text, Vec::new());
        assert!(search.case_sensitive);
        assert_eq!(search.query, "ONE");
        assert_eq!(search.found.len(), 0);

        // Ctrl+F again selects the query, so typing replaces it.
        text_pass(
            &ctx,
            &mut search,
            &text,
            vec![key(Key::F, Modifiers::COMMAND)],
        );
        text_pass(&ctx, &mut search, &text, vec![Event::Text("two".into())]);
        text_pass(&ctx, &mut search, &text, Vec::new());
        assert_eq!(search.query, "two");
        assert_eq!(search.found.len(), 2);

        // Escape clears it.
        text_pass(
            &ctx,
            &mut search,
            &text,
            vec![key(Key::Escape, Modifiers::NONE)],
        );
        text_pass(&ctx, &mut search, &text, Vec::new());
        assert_eq!(search.query, "");
        assert_eq!(search.found.len(), 0);
    }

    /// A Markdown document with matches for "find me" (ignoring separators)
    /// in a heading, inline code, a code block, a table, a `<details>`
    /// section, and far below the first screenful.
    fn document(details_open: bool) -> Arc<str> {
        let open = if details_open { " open" } else { "" };
        let mut doc = format!(
            "# Find me\n\
             \n\
             Some text, then `find-me` inline.\n\
             \n\
             ```rust\n\
             let find_me = 1;\n\
             ```\n\
             \n\
             | a | b |\n\
             |---|---|\n\
             | findme | x |\n\
             \n\
             <details{open}>\n\
             <summary>More</summary>\n\
             \n\
             Hidden find me.\n\
             \n\
             </details>\n\
             \n"
        );
        for i in 0..100 {
            doc.push_str(&format!("Filler paragraph {i}.\n\n"));
        }
        doc.push_str("Find me at the bottom.\n");
        doc.into()
    }

    struct MarkdownWindow {
        text: Arc<str>,
        blocks: Vec<markdown::Block>,
        open: Vec<bool>,
        cache: CommonMarkCache,
        search: Search,
    }

    impl MarkdownWindow {
        fn new(text: Arc<str>) -> Self {
            Self {
                blocks: markdown::split_details(&text),
                text,
                open: Vec::new(),
                cache: CommonMarkCache::default(),
                search: Search {
                    query: "find me".to_owned(),
                    ignore_separators: true,
                    ..Default::default()
                },
            }
        }

        /// Draw as `crate::viewer` does.
        fn pass(&mut self, ctx: &Context, events: Vec<Event>) -> egui::FullOutput {
            let Self {
                text,
                blocks,
                open,
                cache,
                search,
            } = self;
            let window = ViewportId::ROOT;
            let path = Path::new("doc.md");
            run(ctx, input(events), |ui| {
                search.keys(ui.ctx());
                search.bar(ui, window);
                ScrollArea::vertical()
                    .id_salt("md")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let top_left = ui.max_rect().min;
                        let width = ui.available_width();
                        let mut details = Details::Shown(open);
                        markdown::show(ui, text, blocks, path, cache, window, &mut details);
                        let layout = Layout::markdown(text, width, ui.min_rect().height(), open);
                        search.markdown(ui, top_left, layout, window, |ui| {
                            let mut details = Details::Measured(open);
                            markdown::show(ui, text, blocks, path, cache, window, &mut details);
                        });
                    });
            })
        }

        /// Draw until the layout and any scrolling settle.
        fn settle(&mut self, ctx: &Context) -> egui::FullOutput {
            let mut output = self.pass(ctx, Vec::new());
            for _ in 0..SETTLE_PASSES {
                output = self.pass(ctx, Vec::new());
            }
            output
        }
    }

    #[test]
    fn finds_markdown_text_wherever_it_is_drawn() {
        let ctx = Context::default();
        let mut window = MarkdownWindow::new(document(false));
        let output = window.settle(&ctx);
        // Heading, inline code, code block, table, bottom. Not the closed
        // section's.
        assert_eq!(window.search.found.len(), 5);
        assert_eq!(window.search.current, 0);

        // Nothing from the off-screen layout reaches the screen.
        for clipped in &output.shapes {
            let r = clipped.shape.visual_bounding_rect();
            assert!(
                !r.is_positive() || r.max.x > -1000.,
                "drawn off screen: {r:?}"
            );
        }
        let drawn = highlights(&output);
        assert!(
            drawn.len() >= 4,
            "the matches on screen are highlighted: {drawn:?}"
        );
        assert!(drawn.iter().all(|(r, _)| r.width() > 1. && r.height() > 1.));
        assert_eq!(drawn.iter().filter(|(_, current)| *current).count(), 1);

        // Shift+F3 wraps to the bottom match, and scrolls to it.
        window.pass(&ctx, vec![key(Key::F3, Modifiers::SHIFT)]);
        assert_eq!(window.search.current, 4);
        let output = window.settle(&ctx);
        let drawn = highlights(&output);
        assert!(
            drawn
                .iter()
                .any(|&(r, current)| current && SCREEN.contains_rect(r)),
            "the current match is on screen: {drawn:?}"
        );
        assert_eq!(drawn.len(), 1, "the matches at the top are scrolled away");
    }

    #[test]
    fn finds_markdown_text_in_open_details() {
        let ctx = Context::default();
        let mut window = MarkdownWindow::new(document(true));
        window.settle(&ctx);
        assert_eq!(window.search.found.len(), 6);
        assert_eq!(window.open, [true]);
    }
}
