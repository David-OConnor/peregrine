//! Line numbers down the left of a source or plain-text window, as in Sublime
//! Text or PyCharm: grey, right-aligned, one per line, and none beside the
//! continuation of a wrapped line. The gutter sits outside the text's scroll
//! area, so it scrolls up and down with the text but stays put when the text
//! scrolls sideways.

use egui::{Align2, Galley, Pos2, Rect, TextStyle, Ui, pos2};

/// Space left of the numbers, and between them and the text, in digits.
const PADDING: [f32; 2] = [2., 3.];

/// The fewest digits the gutter has room for, so text starts in the same
/// place in most files and the gutter doesn't widen as a short file grows.
const MIN_DIGITS: u32 = 3;

/// The gutter's width when numbering `lines` lines.
pub(super) fn width(ui: &Ui, lines: usize) -> f32 {
    let digits = (lines.max(1).ilog10() + 1).max(MIN_DIGITS);
    digit_width(ui) * (digits as f32 + PADDING[0] + PADDING[1])
}

fn digit_width(ui: &Ui) -> f32 {
    let font = TextStyle::Monospace.resolve(ui.style());
    ui.fonts_mut(|fonts| fonts.glyph_width(&font, '0'))
}

/// Number the lines of `galley`, drawn with its top left at `pos`, in
/// `gutter`: each of its rows that begins a line and shows within the
/// gutter's height.
pub(super) fn paint(ui: &Ui, gutter: Rect, galley: &Galley, pos: Pos2) {
    // The text's own font, so each number sits on its line's baseline.
    let font = TextStyle::Monospace.resolve(ui.style());
    let color = ui.visuals().weak_text_color();
    let right = gutter.right() - digit_width(ui) * PADDING[1];
    let painter = ui.painter_at(gutter);
    let mut line = 0;
    let mut starts_line = true;
    for row in &galley.rows {
        let rect = row.rect().translate(pos.to_vec2());
        if rect.top() >= gutter.bottom() {
            break;
        }
        if starts_line {
            line += 1;
            if rect.bottom() > gutter.top() {
                let at = pos2(right, rect.top());
                painter.text(at, Align2::RIGHT_TOP, line.to_string(), font.clone(), color);
            }
        }
        starts_line = row.ends_with_newline;
    }
}
