//! A way to store one-off notes. These are persisted in state, and can be saved to
//! file at any point.
//!
//! The panel is a toolbar over one text box, which fills the rest of it.
//! Shell's line-editing keys (see [line_edit]) work in it, as in the CLI
//! input, where they make sense for several lines; Tab indents rather than
//! going to the CLI input.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use egui::{
    Button, Color32, Context, Event, Id, Key, KeyboardShortcut, Modal, Modifiers, RichText,
    ScrollArea, TextEdit, TextStyle, Ui, ViewportCommand, os::OperatingSystem,
};

use super::{
    line_edit::{self, Edit, LineKey},
    panel_header, scale_panel_fonts,
};
use crate::{OutType, state::State};

/// The panel's width until it's dragged to another.
pub const COL_WIDTH: f32 = 320.;

/// How long after the last change the text is saved to the state file, so
/// typing doesn't rewrite it at every key.
const SAVE_DELAY: Duration = Duration::from_secs(1);

/// The name the Save popup offers, in the active tab's dir.
const DEFAULT_NAME: &str = "scratchpad";

/// The file formats the Save popup offers, by extension.
const FORMATS: [&str; 2] = ["txt", "md"];

/// The text box's.
pub(super) fn edit_id() -> Id {
    Id::new("scratchpad_text")
}

#[derive(Default)]
pub struct Scratchpad {
    pub text: String,
    /// When `text` last changed, until it's saved to the state file.
    changed: Option<Instant>,
    /// Clear was clicked: waiting on its confirmation.
    clear_pending: bool,
    /// The Save popup, while open.
    save: Option<SaveDialog>,
    /// Where the text was last saved, for the next Save to offer.
    last_saved: Option<PathBuf>,
}

impl Scratchpad {
    /// Whether the text has gone [SAVE_DELAY] without changing since it last
    /// changed, so it's time to save the state file. Asks for a pass at that
    /// time if it hasn't come yet.
    pub fn save_due(&mut self, ctx: &Context) -> bool {
        let Some(changed) = self.changed else {
            return false;
        };
        let since = changed.elapsed();
        if since >= SAVE_DELAY {
            self.changed = None;
            return true;
        }
        ctx.request_repaint_after(SAVE_DELAY - since);
        false
    }

    fn mark_changed(&mut self) {
        self.changed = Some(Instant::now());
    }
}

/// The Save popup's path, as typed.
struct SaveDialog {
    path: String,
    /// Why the last attempt failed, until the path is edited.
    error: Option<String>,
    /// Just opened: the path field takes focus.
    focus: bool,
}

/// A toolbar button that was clicked.
#[derive(Clone, Copy)]
enum Action {
    Save,
    Clear,
    CopyAll,
    Cut,
    Copy,
    Paste,
}

/// Its popups, drawn over the whole window. Call once per pass, whether or
/// not the panel is shown.
pub(super) fn popups(state: &mut State, ctx: &Context) {
    clear_popup(state, ctx);
    save_popup(state, ctx);
}

/// The panel: its header, the toolbar, and the text box filling the rest.
pub(super) fn draw(state: &mut State, ui: &mut Ui) {
    panel_header(ui, "Scratchpad", &mut state.ui.panel_vis.scratchpad);

    let ctx = ui.ctx().clone();
    let id = edit_id();
    let pad = &mut state.ui.scratchpad;

    let selection = TextEdit::load_state(&ctx, id)
        .and_then(|s| s.cursor.char_range())
        .filter(|r| !r.is_empty());

    let action = toolbar(ui, !pad.text.is_empty(), selection.is_some());
    match action {
        Some(Action::Save) => {
            let path = pad
                .last_saved
                .clone()
                .unwrap_or_else(|| state.cwd().join(format!("{DEFAULT_NAME}.{}", FORMATS[0])));
            state.ui.scratchpad.save = Some(SaveDialog {
                path: path.display().to_string(),
                error: None,
                focus: true,
            });
        }
        Some(Action::Clear) => pad.clear_pending = true,
        Some(Action::CopyAll) => ctx.copy_text(pad.text.clone()),
        Some(Action::Copy) => {
            if let Some(range) = selection {
                ctx.copy_text(range.slice_str(&pad.text).to_owned());
            }
        }
        Some(Action::Cut) => {
            if let Some(range) = selection {
                ctx.copy_text(range.slice_str(&pad.text).to_owned());
                let [start, end] = range.sorted_cursors();
                let mut chars: Vec<char> = pad.text.chars().collect();
                chars.drain(start.index.0..end.index.0);
                pad.text = chars.into_iter().collect();
                line_edit::set_caret(&ctx, id, start.index.0);
                pad.mark_changed();
            }
        }
        // The text box, focused again below, takes the paste in next pass,
        // at its caret, as if Ctrl+V were pressed.
        Some(Action::Paste) => ctx.send_viewport_cmd(ViewportCommand::RequestPaste),
        None => {}
    }
    let pad = &mut state.ui.scratchpad;
    // Back to the text, from the button. (Save and Clear's popups give it
    // back once closed.)
    if action.is_some_and(|a| !matches!(a, Action::Save | Action::Clear)) {
        ctx.memory_mut(|m| m.request_focus(id));
    }

    // Before the box is drawn, so it shows their result.
    if ctx.memory(|m| m.has_focus(id)) {
        let mac = ctx.os() == OperatingSystem::Mac;
        let keys = ui.input_mut(|i| {
            let keys = line_edit::take_keys(&mut i.events, mac);
            // Ctrl+P / Ctrl+N are Up / Down, as in Emacs.
            for key in &keys {
                if let LineKey::History { up } = key {
                    i.events.push(Event::Key {
                        key: if *up { Key::ArrowUp } else { Key::ArrowDown },
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: Modifiers::NONE,
                    });
                }
            }
            keys
        });
        for key in keys {
            let edit = match key {
                LineKey::Edit(edit) => edit,
                // There's no input to end here.
                LineKey::Eof => Edit::DeleteChar,
                // The Clear button does this, with a confirmation.
                LineKey::History { .. } | LineKey::ClearScreen => continue,
            };
            line_edit::apply_in_box(&ctx, id, &mut pad.text, edit, &mut state.ui.kill_ring);
            pad.mark_changed();
        }
    }

    ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        let edit = TextEdit::multiline(&mut pad.text)
            .id(id)
            .font(TextStyle::Monospace)
            .desired_width(f32::INFINITY)
            .min_size(ui.available_size())
            .lock_focus(true);
        if ui.add(edit).changed() {
            pad.mark_changed();
        }
    });
}

/// The row of buttons above the text. `has_text` and `has_selection` enable
/// those that need them.
fn toolbar(ui: &mut Ui, has_text: bool, has_selection: bool) -> Option<Action> {
    let shortcut = |key| {
        ui.ctx()
            .format_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, key))
    };
    let (cut_key, copy_key, paste_key) = (shortcut(Key::X), shortcut(Key::C), shortcut(Key::V));

    let mut action = None;
    ui.horizontal(|ui| {
        if ui
            .button("Save")
            .on_hover_text("Save the text to a .txt or .md file")
            .clicked()
        {
            action = Some(Action::Save);
        }
        if ui
            .add_enabled(has_text, Button::new("Clear"))
            .on_hover_text("Delete all the text")
            .clicked()
        {
            action = Some(Action::Clear);
        }
        ui.separator();
        if ui
            .add_enabled(has_text, Button::new("Copy all"))
            .on_hover_text("Copy all the text to the clipboard")
            .clicked()
        {
            action = Some(Action::CopyAll);
        }
        ui.separator();
        if ui
            .add_enabled(has_selection, Button::new("Cut"))
            .on_hover_text(format!("Cut the selected text ({cut_key})"))
            .clicked()
        {
            action = Some(Action::Cut);
        }
        if ui
            .add_enabled(has_selection, Button::new("Copy"))
            .on_hover_text(format!("Copy the selected text ({copy_key})"))
            .clicked()
        {
            action = Some(Action::Copy);
        }
        if ui
            .button("Paste")
            .on_hover_text(format!("Paste at the cursor ({paste_key})"))
            .clicked()
        {
            action = Some(Action::Paste);
        }
    });
    action
}

/// Confirms Clear.
fn clear_popup(state: &mut State, ctx: &Context) {
    let pad = &mut state.ui.scratchpad;
    if !pad.clear_pending {
        return;
    }

    let mut confirm = false;
    let mut cancel = false;
    let modal = Modal::new(Id::new("scratchpad_clear_popup")).show(ctx, |ui| {
        scale_panel_fonts(ui, state.gui_font_size);
        ui.heading("Clear the scratchpad?");
        ui.label("This deletes all its text.");
        ui.add_space(8.);
        ui.horizontal(|ui| {
            let clear = ui.add(
                Button::new(RichText::new("Clear").color(Color32::WHITE))
                    .fill(Color32::from_rgb(180, 45, 45)),
            );
            let keep = ui.button("Cancel");
            // Keep keyboard focus in the modal, starting on Cancel, so Enter
            // doesn't throw the text away.
            if !clear.has_focus() && !keep.has_focus() {
                keep.request_focus();
            }
            confirm = clear.clicked();
            cancel = keep.clicked();
        });
    });

    if confirm {
        pad.text.clear();
        line_edit::set_caret(ctx, edit_id(), 0);
        pad.mark_changed();
    }
    if confirm || cancel || modal.should_close() {
        pad.clear_pending = false;
        ctx.memory_mut(|m| m.request_focus(edit_id()));
        ctx.request_repaint();
    }
}

/// Asks where to save the text, as a .txt or .md file.
fn save_popup(state: &mut State, ctx: &Context) {
    let cwd = state.cwd().to_path_buf();
    let Some(dialog) = &mut state.ui.scratchpad.save else {
        return;
    };

    let mut save = false;
    let mut cancel = false;
    let modal = Modal::new(Id::new("scratchpad_save_popup")).show(ctx, |ui| {
        scale_panel_fonts(ui, state.gui_font_size);
        ui.set_width(420.0_f32.min((ctx.content_rect().width() - 48.).max(100.)));
        ui.heading("Save the scratchpad");

        // The format is the path's extension; picking one changes it.
        ui.horizontal(|ui| {
            ui.label("Format:");
            let mut path = PathBuf::from(&dialog.path);
            let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase());
            for format in FORMATS {
                if ui
                    .radio(ext.as_deref() == Some(format), format!(".{format}"))
                    .clicked()
                {
                    path.set_extension(format);
                    dialog.path = path.display().to_string();
                }
            }
        });

        let field = ui.add(
            TextEdit::singleline(&mut dialog.path)
                .desired_width(f32::INFINITY)
                .hint_text("File path"),
        );
        if std::mem::take(&mut dialog.focus) {
            field.request_focus();
        }
        if field.changed() {
            dialog.error = None;
        }
        let path = target(&dialog.path, &cwd);
        let is_dir = path.is_dir();
        if is_dir {
            ui.colored_label(
                ui.visuals().error_fg_color,
                "That's a folder; add a file name.",
            );
        } else if path.exists() {
            ui.label(RichText::new("This file exists; saving replaces it.").weak());
        }
        if let Some(error) = &dialog.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }

        ui.add_space(8.);
        ui.horizontal(|ui| {
            let enabled = !dialog.path.trim().is_empty() && !is_dir;
            save = ui.add_enabled(enabled, Button::new("Save")).clicked()
                || (enabled && field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)));
            cancel = ui.button("Cancel").clicked();
        });
    });

    if save {
        let path = target(&dialog.path, &cwd);
        match fs::write(&path, &state.ui.scratchpad.text) {
            Ok(()) => {
                state.push_out(
                    format!("Saved the scratchpad to {}", path.display()),
                    OutType::StdOut,
                );
                state.ui.scratchpad.last_saved = Some(path);
                state.ui.scratchpad.save = None;
            }
            Err(e) => dialog.error = Some(format!("Couldn't save: {e}")),
        }
    } else if cancel || modal.should_close() {
        state.ui.scratchpad.save = None;
    }

    if state.ui.scratchpad.save.is_none() {
        ctx.memory_mut(|m| m.request_focus(edit_id()));
        ctx.request_repaint();
    }
}

/// `typed` as a path, relative to `cwd` if it isn't absolute.
fn target(typed: &str, cwd: &Path) -> PathBuf {
    let path = Path::new(typed.trim());
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}
