//! Icons for the Files listing's rows: a small picture to the left of a
//! file's name, where a folder has its 📁, and the color the name is drawn
//! in. Only common text, code, config and image files are recognized, by
//! extension or, for a few, by whole name; any other file gets an empty
//! space the icon's size, so the names still line up.
//!
//! The icons are painted rather than emoji, so each can carry a letter and a
//! color of its own:
//! - Text (`.txt`, `.md`, `.pdf`): a page with lines on it.
//! - Code (`.rs`, `.py`): a page in the language's color with its initial
//!   (or a symbol, as `$` for shell scripts) on it.
//! - Config (`.toml`, `.json`): a page with sliders on it.
//! - Images (`.png`, `.jpg`): a framed picture of hills under a sun.
//!
//! Each language or format has its own color, from its logo or GitHub's
//! language colors, lightened or darkened only as far as it takes to stand
//! out from the panel. Every text, code and config file's name is one light
//! blue, and every image's the magenta `ls` gives images.

use std::{ffi::OsStr, path::Path};

use egui::{
    Color32, FontId, Painter, Pos2, Rect, Rgba, Shape, Stroke, StrokeKind, TextStyle, Ui, Vec2,
    pos2, vec2,
};

use crate::ui;

#[derive(Clone, Copy)]
enum Icon {
    /// A page with lines of text.
    Text,
    /// A page filled with the type's color, with this character on it.
    Code(char),
    /// A page with sliders.
    Config,
    /// A framed picture.
    Image,
}

/// File types by extension (lowercase): each one's icon, and its color as
/// `0xRRGGBB`.
const BY_EXTENSION: &[(&[&str], Icon, u32)] = &[
    // Plain text is paper-colored; the others are GitHub's language colors.
    (&["txt", "text"], Icon::Text, 0xB4BCC8),
    (&["md", "markdown"], Icon::Text, 0x083FA1),
    (&["csv", "tsv"], Icon::Text, 0x237346),
    (&["log"], Icon::Text, 0x8A929C),
    // Adobe's red.
    (&["pdf"], Icon::Text, 0xFA0F00),
    // A blue from LibreOffice's palette, lighter than Markdown's.
    (&["odt"], Icon::Text, 0x1C99E0),
    (&["rs"], Icon::Code('R'), 0xDEA584),
    // Python's logo yellow.
    (&["py", "pyw", "pyi"], Icon::Code('P'), 0xFFD43B),
    // Shell scripts get a prompt, not a letter.
    (&["sh", "bash", "zsh", "fish"], Icon::Code('$'), 0x89E051),
    // PowerShell's logo blue.
    (&["ps1", "psm1", "psd1"], Icon::Code('>'), 0x5391FE),
    (&["bat", "cmd"], Icon::Code('>'), 0xC1F12E),
    // C's logo color.
    (&["c", "h"], Icon::Code('C'), 0xA8B9CC),
    (
        &["cpp", "cc", "cxx", "hpp", "hh", "hxx"],
        Icon::Code('C'),
        0xF34B7D,
    ),
    // NVIDIA green.
    (&["cu", "cuh", "cuda"], Icon::Code('C'), 0x76B900),
    // Go's brand blue.
    (&["go"], Icon::Code('G'), 0x00ADD8),
    (&["js", "mjs", "cjs", "jsx"], Icon::Code('J'), 0xF7A41D),
    // TypeScript's logo blue.
    (&["ts", "mts", "cts", "tsx"], Icon::Code('T'), 0x3178C6),
    (&["java"], Icon::Code('J'), 0xB07219),
    // C#'s logo purple.
    (&["cs"], Icon::Code('#'), 0x9B4F96),
    (&["kt", "kts"], Icon::Code('K'), 0x7F52FF),
    (&["swift"], Icon::Code('S'), 0xF05138),
    (&["rb"], Icon::Code('R'), 0xCC342D),
    (&["php"], Icon::Code('P'), 0x777BB4),
    (&["html", "htm"], Icon::Code('H'), 0xE34F26),
    // CSS's logo purple.
    (&["css", "scss"], Icon::Code('C'), 0x663399),
    (&["toml"], Icon::Config, 0x9C4221),
    // JSON's own logo is black and white; editors show it yellow.
    (&["json", "jsonc", "json5"], Icon::Config, 0xCBCB41),
    (&["yaml", "yml"], Icon::Config, 0xCB171E),
    (&["xml"], Icon::Config, 0x0060AC),
    (&["ini", "cfg", "conf", "lock"], Icon::Config, 0xD1DBE0),
    // Images are all drawn in their names' color (see [FileType::name_color]).
    (
        &[
            "png", "jpg", "jpeg", "jfif", "gif", "bmp", "webp", "svg", "ico", "tif", "tiff", "avif",
        ],
        Icon::Image,
        0,
    ),
];

/// The same, for files known by their whole name (lowercase), which mostly
/// have no extension, or only a leading dot.
const BY_NAME: &[(&[&str], Icon, u32)] = &[
    (&["readme", "license", "licence"], Icon::Text, 0xB4BCC8),
    (&["makefile"], Icon::Code('M'), 0x427819),
    // Docker's logo blue.
    (&["dockerfile"], Icon::Code('D'), 0x2496ED),
    // Git's logo orange.
    (
        &[".gitignore", ".gitattributes", ".gitmodules"],
        Icon::Config,
        0xF05032,
    ),
    (&[".editorconfig"], Icon::Config, 0xD1DBE0),
    (&[".env"], Icon::Config, 0xECD53F),
];

/// Name colors: a light blue for text, code and config files, and for
/// images the magenta `ls` gives them, each per panel theme.
const TEXT_NAME_DARK: Color32 = Color32::from_rgb(0x8C, 0xC8, 0xFF);
const TEXT_NAME_LIGHT: Color32 = Color32::from_rgb(0x1A, 0x62, 0xB4);
const IMAGE_NAME_DARK: Color32 = Color32::from_rgb(0xE0, 0x7C, 0xE0);
const IMAGE_NAME_LIGHT: Color32 = Color32::from_rgb(0xA0, 0x27, 0x9A);

/// How far an icon's lines must stand out from the panel, as a WCAG
/// contrast ratio: the 3:1 asked of icons and other non-text graphics.
const MIN_CONTRAST: f32 = 3.0;

/// A file type the Files listing has an icon for (see [file_type]).
#[derive(Clone, Copy)]
pub(super) struct FileType {
    icon: Icon,
    /// Its own color, before [legible] fits it to the panel.
    color: Color32,
}

impl FileType {
    /// The color a name of this type is drawn in, on a `dark` panel or not.
    pub(super) fn name_color(self, dark: bool) -> Color32 {
        match (self.icon, dark) {
            (Icon::Image, true) => IMAGE_NAME_DARK,
            (Icon::Image, false) => IMAGE_NAME_LIGHT,
            (_, true) => TEXT_NAME_DARK,
            (_, false) => TEXT_NAME_LIGHT,
        }
    }
}

/// The type of the file at `path`, if it's one with an icon.
pub(super) fn file_type(path: &Path) -> Option<FileType> {
    let lower = |s: &OsStr| s.to_string_lossy().to_lowercase();
    let find = |table: &[(&[&str], Icon, u32)], key: String| {
        let &(_, icon, hex) = table
            .iter()
            .find(|(keys, ..)| keys.contains(&key.as_str()))?;
        let [_, r, g, b] = hex.to_be_bytes();
        Some(FileType {
            icon,
            color: Color32::from_rgb(r, g, b),
        })
    };
    path.file_name()
        .and_then(|n| find(BY_NAME, lower(n)))
        .or_else(|| path.extension().and_then(|e| find(BY_EXTENSION, lower(e))))
}

/// The space a row's icon takes: the size of a folder's 📁, so file and
/// folder names line up.
pub(super) fn slot_size(ui: &Ui) -> Vec2 {
    let font = TextStyle::Monospace.resolve(ui.style());
    let folder = ui
        .painter()
        .layout_no_wrap("📁".to_owned(), font, Color32::PLACEHOLDER);
    folder.size()
}

/// Paint `t`'s icon centered in `slot`, a space of [slot_size].
pub(super) fn paint(ui: &Ui, slot: Rect, t: FileType) {
    let painter = ui.painter();
    let visuals = ui.visuals();
    let bg = visuals.panel_fill;
    // Sized by the font, as the 📁 is.
    let size = TextStyle::Monospace.resolve(ui.style()).size;
    let color = match t.icon {
        Icon::Image => t.name_color(visuals.dark_mode),
        _ => legible(t.color, bg),
    };
    let stroke = Stroke::new(1.0, color);

    let mut shapes = Vec::new();
    match t.icon {
        Icon::Text => {
            let p = page(painter, slot, size);
            outlined_page(&mut shapes, p, stroke);
            // Three lines, the last one short, as a paragraph ends.
            for (y, len) in [(0.42, 1.0), (0.6, 1.0), (0.78, 0.6)] {
                let y = painter.round_to_pixel_center(p.top() + y * p.height());
                let left = p.left() + 0.24 * p.width();
                let right = left + len * 0.52 * p.width();
                shapes.push(Shape::line_segment([pos2(left, y), pos2(right, y)], stroke));
            }
        }
        Icon::Code(ch) => {
            let p = page(painter, slot, size);
            let edge = Stroke::new(1.0, color);
            let fold = fold(p);
            shapes.push(Shape::convex_polygon(page_outline(p), t.color, edge));
            // The folded corner, a shade darker.
            let corner = vec![
                pos2(p.right() - fold, p.top()),
                pos2(p.right() - fold, p.top() + fold),
                pos2(p.right(), p.top() + fold),
            ];
            let shade = t.color.lerp_to_gamma(Color32::BLACK, 0.3);
            shapes.push(Shape::convex_polygon(corner, shade, edge));

            // Black or white, whichever reads better on the color.
            let ink = if contrast(t.color, Color32::BLACK) > contrast(t.color, Color32::WHITE) {
                Color32::from_gray(16)
            } else {
                Color32::WHITE
            };
            let font = FontId::new(0.72 * size, ui::bold_family());
            let galley = painter.layout_no_wrap(ch.to_string(), font, ink);
            // The glyph itself (not its line's box, which has room for
            // accents and descenders) a little below center, clear of the
            // fold.
            let center = p.center() + vec2(0.0, 0.2 * fold);
            let pos = center - galley.mesh_bounds.center().to_vec2();
            shapes.push(Shape::galley(pos, galley, ink));
        }
        Icon::Config => {
            let p = page(painter, slot, size);
            outlined_page(&mut shapes, p, stroke);
            // Two sliders, set to different places.
            for (y, knob) in [(0.5, 0.64), (0.76, 0.36)] {
                let y = painter.round_to_pixel_center(p.top() + y * p.height());
                let (left, right) = (p.left() + 0.2 * p.width(), p.right() - 0.2 * p.width());
                shapes.push(Shape::line_segment([pos2(left, y), pos2(right, y)], stroke));
                let knob = pos2(p.left() + knob * p.width(), y);
                shapes.push(Shape::circle_filled(knob, 0.14 * p.width(), color));
            }
        }
        Icon::Image => {
            let size = vec2((1.05 * size).min(slot.width()), 0.8 * size);
            let r = snap(painter, Rect::from_center_size(slot.center(), size));
            shapes.push(Shape::rect_stroke(r, 1.5, stroke, StrokeKind::Middle));
            let at = |x: f32, y: f32| pos2(r.left() + x * r.width(), r.top() + y * r.height());
            // A tall hill with a lower one in front of it to the right, and
            // the sun above.
            let hills = [
                [at(0.0, 1.0), at(0.36, 0.38), at(0.74, 1.0)],
                [at(0.48, 1.0), at(0.72, 0.6), at(1.0, 1.0)],
            ];
            for hill in hills {
                shapes.push(Shape::convex_polygon(hill.to_vec(), color, Stroke::NONE));
            }
            shapes.push(Shape::circle_filled(
                at(0.74, 0.3),
                0.11 * r.height(),
                color,
            ));
        }
    }
    painter.extend(shapes);
}

/// A page's rect, for a font of `size`, centered in `slot`.
fn page(painter: &Painter, slot: Rect, size: f32) -> Rect {
    let height = 0.95 * size;
    snap(
        painter,
        Rect::from_center_size(slot.center(), vec2(0.76 * height, height)),
    )
}

/// How far down and in a page's top right corner is folded.
fn fold(page: Rect) -> f32 {
    0.34 * page.width()
}

/// The outline of `page`, clockwise from its top left, with the top right
/// corner cut off where it's folded.
fn page_outline(page: Rect) -> Vec<Pos2> {
    let fold = fold(page);
    vec![
        page.left_top(),
        pos2(page.right() - fold, page.top()),
        pos2(page.right(), page.top() + fold),
        page.right_bottom(),
        page.left_bottom(),
    ]
}

/// `page` drawn in outline, with the crease of its folded corner.
fn outlined_page(shapes: &mut Vec<Shape>, page: Rect, stroke: Stroke) {
    let fold = fold(page);
    shapes.push(Shape::closed_line(page_outline(page), stroke));
    let crease = vec![
        pos2(page.right() - fold, page.top()),
        pos2(page.right() - fold, page.top() + fold),
        pos2(page.right(), page.top() + fold),
    ];
    shapes.push(Shape::line(crease, stroke));
}

/// `rect` with its edges on pixel centers, so 1-point lines along them
/// stay sharp.
fn snap(painter: &Painter, rect: Rect) -> Rect {
    let p = |v: f32| painter.round_to_pixel_center(v);
    Rect::from_min_max(
        pos2(p(rect.left()), p(rect.top())),
        pos2(p(rect.right()), p(rect.bottom())),
    )
}

/// `color`, blended toward white on a dark `bg` or black on a light one only
/// as far as it takes to stand out from it by [MIN_CONTRAST].
fn legible(color: Color32, bg: Color32) -> Color32 {
    let toward = if luminance(bg) < 0.18 {
        Color32::WHITE
    } else {
        Color32::BLACK
    };
    (0..=10)
        .map(|i| color.lerp_to_gamma(toward, i as f32 / 10.0))
        .find(|&c| contrast(c, bg) >= MIN_CONTRAST)
        .unwrap_or(toward)
}

/// WCAG contrast ratio between two colors, from 1 (the same) to 21.
fn contrast(a: Color32, b: Color32) -> f32 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// WCAG relative luminance, 0 for black to 1 for white.
fn luminance(c: Color32) -> f32 {
    let c = Rgba::from(c);
    0.2126 * c.r() + 0.7152 * c.g() + 0.0722 * c.b()
}
