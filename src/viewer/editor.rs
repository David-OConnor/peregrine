//! A small text editor sharing the viewer's background syntax highlighter.
//! Only complete, losslessly decoded files can be edited. The original bytes
//! also let Save notice changes made by another program before overwriting.

use std::{
    fs::{self, File},
    io::Read,
    path::Path,
    sync::Arc,
};

use egui::{
    Context, Event, Id, Key, Modifiers, Rect, ScrollArea, Shape, TextBuffer, TextEdit, Ui,
    UiBuilder,
    text::{CCursor, CCursorRange},
};

use super::{Kind, MAX_BYTES, comment, gutter, search, selection, syntax};
use crate::ui::{
    line_edit,
    scratchpad::{self, ScratchpadTab},
};

#[derive(Clone, Copy)]
enum Encoding {
    Utf8 { bom: bool },
    Utf16 { little_endian: bool },
}

impl Encoding {
    fn decode(bytes: &[u8]) -> Result<(Self, String), String> {
        let invalid =
            || "This file contains invalid text; open it externally to edit it.".to_string();
        let (encoding, text) = match bytes {
            [0xFF, 0xFE, rest @ ..] | [0xFE, 0xFF, rest @ ..] => {
                let little_endian = bytes[0] == 0xFF;
                let (pairs, remainder) = rest.as_chunks::<2>();
                if !remainder.is_empty() {
                    return Err(invalid());
                }
                let units: Vec<u16> = pairs
                    .iter()
                    .map(|&pair| {
                        if little_endian {
                            u16::from_le_bytes(pair)
                        } else {
                            u16::from_be_bytes(pair)
                        }
                    })
                    .collect();
                (
                    Self::Utf16 { little_endian },
                    String::from_utf16(&units).map_err(|_| invalid())?,
                )
            }
            _ => {
                let (bom, bytes) = match bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
                    Some(rest) => (true, rest),
                    None => (false, bytes),
                };
                (
                    Self::Utf8 { bom },
                    std::str::from_utf8(bytes)
                        .map_err(|_| invalid())?
                        .to_owned(),
                )
            }
        };
        if text.contains('\0') {
            return Err("This file contains binary data; open it externally to edit it.".into());
        }
        Ok((encoding, text))
    }

    fn encode(self, text: &str) -> Vec<u8> {
        match self {
            Self::Utf8 { bom } => {
                let mut bytes = Vec::with_capacity(text.len() + 3);
                if bom {
                    bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
                }
                bytes.extend_from_slice(text.as_bytes());
                bytes
            }
            Self::Utf16 { little_endian } => {
                let mut bytes = if little_endian {
                    vec![0xFF, 0xFE]
                } else {
                    vec![0xFE, 0xFF]
                };
                for unit in text.encode_utf16() {
                    bytes.extend_from_slice(&if little_endian {
                        unit.to_le_bytes()
                    } else {
                        unit.to_be_bytes()
                    });
                }
                bytes
            }
        }
    }
}

pub(super) fn id(doc: Id) -> Id {
    Id::new(("viewer_editor", doc))
}

pub(super) struct Editor {
    text: String,
    original: String,
    /// Updated only when the text changes, for highlighting and find caches.
    snapshot: Arc<str>,
    disk: Vec<u8>,
    encoding: Encoding,
    crlf: bool,
    dirty: bool,
    /// Save found the file changed by another program, and took in what's
    /// there now: the next Save overwrites it, unless it changes again.
    conflict: bool,
    lines: usize,
    layout: syntax::Source,
    focus: bool,
}

impl Editor {
    pub(super) fn load(path: &Path) -> Result<Self, String> {
        let disk = read_complete(path)?;
        let (encoding, text) = Encoding::decode(&disk)?;
        let crlf = text.contains("\r\n");
        // egui edits LF newlines. Save restores Windows line endings.
        let text = text.replace("\r\n", "\n");
        Ok(Self {
            lines: text.bytes().filter(|&b| b == b'\n').count() + 1,
            snapshot: Arc::from(text.as_str()),
            original: text.clone(),
            text,
            disk,
            encoding,
            crlf,
            dirty: false,
            conflict: false,
            layout: syntax::Source::default(),
            focus: true,
        })
    }

    pub(super) fn dirty(&self) -> bool {
        self.dirty
    }

    /// Whether the next Save overwrites another program's change; see [Self::save].
    pub(super) fn conflict(&self) -> bool {
        self.conflict
    }

    /// Write the draft, unless the file changed on disk since it was read or
    /// last saved: then warn instead, and overwrite on the next Save.
    pub(super) fn save(&mut self, path: &Path) -> Result<(), String> {
        if !self.dirty {
            return Ok(());
        }
        // Comparing contents also catches a replacement with the same mtime.
        // A deleted file reads as empty, so Save can recreate it.
        let deleted = path.try_exists().is_ok_and(|exists| !exists);
        let read = |limit| {
            if deleted {
                Ok(Vec::new())
            } else {
                read_bytes(path, limit)
            }
        };
        if read(self.disk.len() as u64 + 1)? != self.disk {
            // All of it, for the next Save to compare.
            self.disk = read(u64::MAX)?;
            self.conflict = true;
            return Err("The file changed on disk. Your edits are kept: Overwrite (Ctrl+S) to replace it with them, or Reload to discard them.".into());
        }
        let text = if self.crlf {
            self.text.replace('\n', "\r\n")
        } else {
            self.text.clone()
        };
        let bytes = self.encoding.encode(&text);
        fs::write(path, &bytes).map_err(|e| format!("Couldn't save {}: {e}", path.display()))?;
        self.disk = bytes;
        self.original.clone_from(&self.text);
        self.dirty = false;
        self.conflict = false;
        Ok(())
    }

    /// Record a change to the text, made by the box or before it's drawn.
    fn changed(&mut self, ctx: &Context) {
        self.dirty = self.text != self.original;
        self.lines = self.text.bytes().filter(|&b| b == b'\n').count() + 1;
        // Update the toolbar and title, drawn before the editor.
        ctx.request_repaint();
    }

    /// Fill the window with egui's native selection, clipboard, navigation,
    /// undo/redo and Tab indentation, with the viewer's gutter and find bar.
    /// Tab inserts a tab, or 4 spaces if `tab` is [ScratchpadTab::Spaces].
    /// Ctrl+Y deletes the line, in place of egui's redo. Ctrl+/ comments out
    /// the line, or uncomments it, in source and Markdown.
    pub(super) fn show(
        &mut self,
        ui: &mut Ui,
        kind: Kind,
        tab: ScratchpadTab,
        search: &mut search::Search,
        doc: Id,
    ) {
        let ctx = ui.ctx().clone();
        let language = match kind {
            Kind::Code(language) => Some(language),
            Kind::Markdown => Some("md"),
            _ => None,
        };
        // Edits made before the box is drawn, so it shows them. Its undo
        // takes them back as it does typing.
        let mut select = search
            .take_replace(&mut self.text)
            .map(|at| CCursorRange::one(CCursor::new(at)));
        if ctx.memory(|m| m.has_focus(id(doc))) {
            let (deletes, comments) = ui.input_mut(|i| {
                for event in &mut i.events {
                    if let Event::Paste(text) = event {
                        *text = text.replace("\r\n", "\n");
                    }
                }
                if tab == ScratchpadTab::Spaces {
                    scratchpad::tab_to_spaces(&mut i.events);
                }
                (
                    i.count_and_consume_key(Modifiers::COMMAND, Key::Y),
                    i.count_and_consume_key(Modifiers::COMMAND, Key::Slash),
                )
            });
            if deletes + comments > 0 {
                let mut range = TextEdit::load_state(&ctx, id(doc))
                    .and_then(|s| s.cursor.char_range())
                    .unwrap_or_default();
                for _ in 0..deletes {
                    range = CCursorRange::one(CCursor::new(delete_lines(&mut self.text, range)));
                    select = Some(range);
                }
                // Plain text has no comments.
                let comment = language.filter(|_| comments > 0).and_then(syntax::comment);
                if let Some(comment) = comment {
                    for _ in 0..comments {
                        range = comment::toggle(&mut self.text, range, comment);
                        select = Some(range);
                    }
                }
            }
        }
        if let Some(select) = select {
            line_edit::set_selection(&ctx, id(doc), select);
            self.changed(&ctx);
            search.text_changed();
        }
        let code = matches!(kind, Kind::Code(_));
        let space = ui.available_rect_before_wrap();
        let (gutter, body) =
            space.split_left_right_at_x(space.left() + gutter::width(ui, self.lines));
        let Self {
            text,
            snapshot,
            layout,
            ..
        } = self;
        let output = ui
            .scope_builder(UiBuilder::new().max_rect(body), |ui| {
                let scroll = if code {
                    ScrollArea::both()
                } else {
                    ScrollArea::vertical()
                };
                scroll
                    .id_salt(("viewer_editor_scroll", doc))
                    .animated(false)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // With no frame, this reserved shape stays behind the text.
                        let behind = ui.painter().add(Shape::Noop);
                        let output = {
                            // An edit re-highlights only the lines it changes;
                            // the rest keep their colors meanwhile.
                            let mut layouter = |ui: &Ui, buffer: &dyn TextBuffer, width| {
                                if snapshot.as_ref() != buffer.as_str() {
                                    *snapshot = Arc::from(buffer.as_str());
                                }
                                layout.galley(
                                    ui,
                                    snapshot,
                                    language,
                                    if code { f32::INFINITY } else { width },
                                )
                            };
                            TextEdit::multiline(text)
                                .id(id(doc))
                                .code_editor()
                                .cursor_at_end(false)
                                .desired_width(f32::INFINITY)
                                .min_size(ui.available_size())
                                .margin(0)
                                .frame(egui::Frame::NONE)
                                .layouter(&mut layouter)
                                .show(ui)
                        };
                        selection::text(ui, &output.response, code);
                        if std::mem::take(&mut self.focus) {
                            output.response.request_focus();
                        }
                        if output.response.changed() {
                            search.text_changed();
                        }
                        search.find_in_text(ui.ctx(), snapshot);
                        search.paint_text(ui, &output.galley, output.galley_pos, behind);
                        output
                    })
            })
            .inner;
        let edit = output.inner;
        let gutter = Rect::from_x_y_ranges(gutter.x_range(), output.inner_rect.y_range());
        gutter::paint(ui, gutter, &edit.galley, edit.galley_pos);
        if edit.response.changed() {
            self.changed(&ctx);
        }
    }

    pub(super) fn reset_state(ctx: &Context, doc: Id) {
        TextEdit::store_state(ctx, id(doc), Default::default());
    }
}

/// Ctrl+Y, as in JetBrains IDEs: delete the lines `range` touches, with their
/// newline, returning where the caret goes: its column, on the line now in
/// their place. A selection ending at the start of a line leaves that line.
fn delete_lines(text: &mut String, range: CCursorRange) -> usize {
    let mut chars: Vec<char> = text.chars().collect();
    let line_start = |chars: &[char], i: usize| {
        chars[..i]
            .iter()
            .rposition(|&c| c == '\n')
            .map_or(0, |n| n + 1)
    };
    let [start, end] = range.sorted_cursors().map(|c| c.index.0.min(chars.len()));
    let caret = range.primary.index.0.min(chars.len());
    let column = caret - line_start(&chars, caret);

    let mut from = line_start(&chars, start);
    let last = if end > start && end == line_start(&chars, end) {
        end - 1
    } else {
        end
    };
    let to = chars[last..]
        .iter()
        .position(|&c| c == '\n')
        .map_or(chars.len(), |n| last + n + 1);
    // The last line has no newline of its own: take the one before it.
    if from > 0 && to == chars.len() && chars[from..to].last() != Some(&'\n') {
        from -= 1;
    }
    chars.drain(from..to);

    let start = line_start(&chars, from);
    let len = chars[start..]
        .iter()
        .position(|&c| c == '\n')
        .unwrap_or(chars.len() - start);
    *text = chars.into_iter().collect();
    start + column.min(len)
}

fn read_complete(path: &Path) -> Result<Vec<u8>, String> {
    let bytes = read_bytes(path, MAX_BYTES + 1)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(
            "Files larger than 1 MB can be viewed here; open externally to edit the whole file."
                .into(),
        );
    }
    Ok(bytes)
}

fn read_bytes(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let read = || -> std::io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        File::open(path)?.take(limit).read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    read().map_err(|e| format!("Couldn't read {}: {e}", path.display()))
}
