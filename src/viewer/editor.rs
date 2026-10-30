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
    Context, Event, Id, Rect, ScrollArea, Shape, TextBuffer, TextEdit, Ui, UiBuilder, ViewportId,
};

use super::{Kind, MAX_BYTES, gutter, search, selection, syntax};
use crate::ui::scratchpad::{self, ScratchpadTab};

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

pub(super) fn id(window: ViewportId) -> Id {
    Id::new(("viewer_editor", window))
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
            layout: syntax::Source::default(),
            focus: true,
        })
    }

    pub(super) fn dirty(&self) -> bool {
        self.dirty
    }

    pub(super) fn save(&mut self, path: &Path) -> Result<(), String> {
        if !self.dirty {
            return Ok(());
        }
        // Comparing contents also catches a replacement with the same mtime.
        if read_bytes(path, self.disk.len() as u64 + 1)? != self.disk {
            return Err("The file changed on disk. Your edits are kept. Copy them before reloading or discarding.".into());
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
        Ok(())
    }

    /// Fill the window with egui's native selection, clipboard, navigation,
    /// undo/redo and Tab indentation, with the viewer's gutter and find bar.
    /// Tab inserts a tab, or 4 spaces if `tab` is [ScratchpadTab::Spaces].
    pub(super) fn show(
        &mut self,
        ui: &mut Ui,
        kind: Kind,
        tab: ScratchpadTab,
        search: &mut search::Search,
        window: ViewportId,
    ) {
        if ui.ctx().memory(|m| m.has_focus(id(window))) {
            ui.input_mut(|i| {
                for event in &mut i.events {
                    if let Event::Paste(text) = event {
                        *text = text.replace("\r\n", "\n");
                    }
                }
                if tab == ScratchpadTab::Spaces {
                    scratchpad::tab_to_spaces(&mut i.events);
                }
            });
        }
        let language = match kind {
            Kind::Code(language) => Some(language),
            Kind::Markdown => Some("md"),
            _ => None,
        };
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
                    .id_salt(("viewer_editor_scroll", window))
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
                                .id(id(window))
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
            self.dirty = self.text != self.original;
            self.lines = self.text.bytes().filter(|&b| b == b'\n').count() + 1;
            // Update the toolbar and title, drawn before the editor.
            ui.ctx().request_repaint();
        }
    }

    pub(super) fn reset_state(ctx: &Context, window: ViewportId) {
        TextEdit::store_state(ctx, id(window), Default::default());
    }
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

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        time::Instant,
    };

    use egui::{Key, Modifiers, RawInput, os::OperatingSystem};

    use super::{
        super::{Action, Content, Doc, Inner},
        *,
    };

    struct TestFile(std::path::PathBuf);

    impl TestFile {
        fn new(bytes: &[u8]) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "peregrine-editor-{}-{}.txt",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::write(&path, bytes).unwrap();
            Self(path)
        }
    }

    impl Drop for TestFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn replace(editor: &mut Editor, text: &str) {
        editor.text = text.into();
        editor.dirty = editor.text != editor.original;
    }

    #[test]
    fn save_preserves_encoding_bom_and_line_endings() {
        for encoding in [
            Encoding::Utf8 { bom: false },
            Encoding::Utf8 { bom: true },
            Encoding::Utf16 {
                little_endian: true,
            },
            Encoding::Utf16 {
                little_endian: false,
            },
        ] {
            for newline in ["\n", "\r\n"] {
                let file =
                    TestFile::new(&encoding.encode(&format!("first{newline}雪 🦅{newline}")));
                let mut editor = Editor::load(&file.0).unwrap();
                replace(&mut editor, "edited\n雪 🦅\nlast");
                editor.save(&file.0).unwrap();
                assert_eq!(
                    fs::read(&file.0).unwrap(),
                    encoding.encode(&format!("edited{newline}雪 🦅{newline}last"))
                );
                assert!(!editor.dirty());
                replace(&mut editor, "another save\n");
                editor.save(&file.0).unwrap();
                assert_eq!(
                    fs::read(&file.0).unwrap(),
                    encoding.encode(&format!("another save{newline}"))
                );
            }
        }
    }

    #[test]
    fn cannot_edit_truncated_lossy_or_binary_text() {
        for bytes in [
            vec![b'a'; MAX_BYTES as usize + 1],
            vec![0xFF],
            vec![0xFF, 0xFE, 0x00],
            vec![0xFF, 0xFE, 0x00, 0xD8],
            b"binary\0text".to_vec(),
        ] {
            let file = TestFile::new(&bytes);
            assert!(Editor::load(&file.0).is_err());
            assert_eq!(fs::read(&file.0).unwrap(), bytes);
        }
        let file = TestFile::new(&vec![b'a'; MAX_BYTES as usize]);
        assert!(Editor::load(&file.0).is_ok());
    }

    #[test]
    fn external_change_and_write_error_keep_draft() {
        let file = TestFile::new(b"original");
        let mut editor = Editor::load(&file.0).unwrap();
        replace(&mut editor, "my draft");
        fs::write(&file.0, b"external").unwrap();
        assert!(
            editor
                .save(&file.0)
                .unwrap_err()
                .contains("changed on disk")
        );
        assert_eq!(fs::read(&file.0).unwrap(), b"external");
        assert_eq!(editor.text, "my draft");
        assert!(editor.dirty());

        fs::remove_file(&file.0).unwrap();
        assert!(editor.save(&file.0).is_err());
        assert!(editor.dirty());
        fs::write(&file.0, b"original").unwrap();
        editor.save(&file.0).unwrap();
        assert_eq!(fs::read(&file.0).unwrap(), b"my draft");
    }

    #[test]
    fn viewing_reopening_and_reloading_do_not_lose_edits() {
        let file = TestFile::new(b"original");
        let ctx = Context::default();
        let mut inner = Inner::default();
        inner.open_now(&ctx, file.0.clone(), false);
        let doc = &mut inner.docs[0];
        assert!(doc.editor.is_none());
        doc.start_editing(&ctx);
        replace(doc.editor.as_mut().unwrap(), "draft");
        fs::write(&file.0, b"external").unwrap();
        doc.last_poll = Instant::now() - super::super::RELOAD_POLL;
        doc.reload_if_changed(&ctx);
        assert_eq!(doc.editor.as_ref().unwrap().text, "draft");
        inner.open_now(&ctx, file.0.clone(), false);
        let doc = &mut inner.docs[0];
        assert_eq!(doc.editor.as_ref().unwrap().text, "draft");
        for action in [
            Action::View,
            Action::Reload,
            Action::Close,
            Action::OpenExternal,
        ] {
            doc.request(&ctx, action);
            assert!(doc.pending.is_some());
            assert!(doc.open);
            assert_eq!(doc.editor.as_ref().unwrap().text, "draft");
        }
        // An explicit discard shows the latest contents on disk.
        doc.perform(&ctx, Action::View);
        assert!(doc.editor.is_none());
        assert!(doc.pending.is_none());
        assert!(matches!(&doc.content, Ok(Content::Text(text)) if text.as_ref() == "external"));
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

    #[test]
    fn editor_keyboard_clipboard_undo_redo_and_save() {
        let file = TestFile::new("hello 雪\r\n".as_bytes());
        let ctx = Context::default();
        ctx.set_os(OperatingSystem::Windows);
        let window = ViewportId::ROOT;
        let mut doc = Doc::new(&ctx, window, file.0.clone(), Kind::Text);
        doc.start_editing(&ctx);
        let mut time = 0.;
        let mut frame = |doc: &mut Doc, events| {
            time += 1.;
            ctx.run_ui(
                RawInput {
                    time: Some(time),
                    screen_rect: Some(Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(820., 900.),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| {
                    doc.editor.as_mut().unwrap().show(
                        ui,
                        Kind::Text,
                        doc.tab,
                        &mut doc.search,
                        window,
                    );
                },
            )
        };
        frame(&mut doc, vec![]).drop_without_applying_deltas();
        frame(
            &mut doc,
            vec![
                key(Key::A, Modifiers::COMMAND),
                Event::Paste("new 雪\r\nline".into()),
            ],
        )
        .drop_without_applying_deltas();
        assert_eq!(doc.editor.as_ref().unwrap().text, "new 雪\nline");
        assert!(doc.dirty());
        frame(&mut doc, vec![key(Key::Z, Modifiers::COMMAND)]).drop_without_applying_deltas();
        assert_eq!(doc.editor.as_ref().unwrap().text, "hello 雪\n");
        assert!(!doc.dirty());
        frame(
            &mut doc,
            vec![key(Key::Z, Modifiers::COMMAND | Modifiers::SHIFT)],
        )
        .drop_without_applying_deltas();
        assert_eq!(doc.editor.as_ref().unwrap().text, "new 雪\nline");
        let copied = frame(&mut doc, vec![key(Key::A, Modifiers::COMMAND), Event::Copy]);
        assert!(copied.platform_output.commands.iter().any(|command| matches!(command, egui::OutputCommand::CopyText(text) if text == "new 雪\nline")));
        copied.drop_without_applying_deltas();
        frame(&mut doc, vec![Event::Cut]).drop_without_applying_deltas();
        assert!(doc.editor.as_ref().unwrap().text.is_empty());
        frame(
            &mut doc,
            vec![
                Event::Paste("saved".into()),
                key(Key::Enter, Modifiers::NONE),
                key(Key::Tab, Modifiers::NONE),
            ],
        )
        .drop_without_applying_deltas();
        // Tab inserts 4 spaces until set to insert a tab.
        assert_eq!(doc.editor.as_ref().unwrap().text, "saved\n    ");
        doc.tab = ScratchpadTab::Tab;
        frame(&mut doc, vec![key(Key::Tab, Modifiers::NONE)]).drop_without_applying_deltas();
        assert_eq!(doc.editor.as_ref().unwrap().text, "saved\n    \t");
        assert!(ctx.memory(|m| m.has_focus(id(window))));
        assert!(doc.save(&ctx));
        assert_eq!(fs::read(&file.0).unwrap(), b"saved\r\n    \t");
        assert!(!doc.dirty());
    }

    #[test]
    fn save_shortcut_includes_edits_in_the_same_input_batch() {
        let file = TestFile::new(b"original");
        let ctx = Context::default();
        let window = ViewportId::ROOT;
        let mut inner = Inner::default();
        let mut doc = Doc::new(&ctx, window, file.0.clone(), Kind::Text);
        doc.start_editing(&ctx);
        inner.docs.push(doc);
        ctx.run_ui(RawInput::default(), |ui| inner.show_window(ui, window))
            .drop_without_applying_deltas();
        ctx.run_ui(
            RawInput {
                time: Some(1.),
                events: vec![
                    key(Key::A, Modifiers::COMMAND),
                    Event::Paste("saved draft".into()),
                    key(Key::S, Modifiers::COMMAND),
                ],
                ..Default::default()
            },
            |ui| inner.show_window(ui, window),
        )
        .drop_without_applying_deltas();
        assert_eq!(fs::read(&file.0).unwrap(), b"saved draft");
        assert!(!inner.docs[0].dirty());
    }

    #[test]
    fn closing_the_app_keeps_unsaved_viewer_drafts() {
        let file = TestFile::new(b"original");
        let ctx = Context::default();
        ctx.set_embed_viewports(false);
        let viewers = super::super::FileViewers::default();
        {
            let mut inner = viewers.lock();
            inner.open_now(&ctx, file.0.clone(), false);
            inner.docs[0].start_editing(&ctx);
            replace(inner.docs[0].editor.as_mut().unwrap(), "unsaved draft");
        }
        let mut input = RawInput::default();
        input
            .viewports
            .get_mut(&ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let output = ctx.run_ui(input, |ui| viewers.show(ui.ctx()));
        assert!(
            output.viewport_output[&ViewportId::ROOT]
                .commands
                .contains(&egui::ViewportCommand::CancelClose)
        );
        assert!(viewers.lock().quit_pending);
        assert_eq!(
            viewers.lock().docs[0].editor.as_ref().unwrap().text,
            "unsaved draft"
        );
        assert_eq!(fs::read(&file.0).unwrap(), b"original");
        output.drop_without_applying_deltas();
    }
}
