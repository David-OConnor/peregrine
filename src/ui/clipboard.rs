//! Session clipboard history. Ordinary paste always uses the OS clipboard;
//! history actions explicitly choose older content, without changing focus
//! or inserting text until requested.
use std::borrow::Cow;

use egui::{Button, ScrollArea, TextEdit, Ui};

use crate::{
    OutType,
    clipboard::{self, Content, History, Paste, TextCapture},
    state::State,
};

#[derive(Default)]
pub struct ClipboardPane {
    pub history: History,
    pub visible: bool,
    search: String,
    pub pending_text: Option<String>,
    shortcut_down: bool,
}

impl ClipboardPane {
    /// egui-winit converts Ctrl+Shift+V presses to Paste (or emits nothing
    /// for a file/empty clipboard), but still forwards the V release.
    fn shortcut(&mut self, events: &mut Vec<egui::Event>, modifiers: egui::Modifiers) {
        events.retain(|event| match event {
            egui::Event::Paste(_) if modifiers.ctrl && modifiers.shift => {
                if !self.shortcut_down {
                    self.visible = !self.visible;
                    self.shortcut_down = true;
                }
                false
            }
            egui::Event::Key {
                key: egui::Key::V,
                pressed,
                modifiers,
                ..
            } if self.shortcut_down || (modifiers.ctrl && modifiers.shift) => {
                if !self.shortcut_down {
                    self.visible = !self.visible;
                }
                self.shortcut_down = *pressed;
                false
            }
            egui::Event::WindowFocused(false) => {
                self.shortcut_down = false;
                true
            }
            _ => true,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key: egui::Key::V,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers,
        }
    }

    #[test]
    fn shortcut_consumes_paste_and_toggles_once_until_release() {
        let mut pane = ClipboardPane::default();
        let modifiers = egui::Modifiers::CTRL | egui::Modifiers::SHIFT;
        let mut events = vec![egui::Event::Paste("text".into())];
        pane.shortcut(&mut events, modifiers);
        assert!(pane.visible);
        assert!(events.is_empty());
        let mut repeated = vec![
            egui::Event::Paste("text".into()),
            release(egui::Modifiers::NONE),
        ];
        pane.shortcut(&mut repeated, modifiers);
        assert!(pane.visible);
        assert!(repeated.is_empty());
        // File and empty clipboards produce only a key release.
        let mut events = vec![release(modifiers)];
        pane.shortcut(&mut events, modifiers);
        assert!(!pane.visible);
        assert!(events.is_empty());
        let mut normal = vec![
            egui::Event::Paste("normal".into()),
            release(egui::Modifiers::CTRL),
        ];
        pane.shortcut(&mut normal, egui::Modifiers::CTRL);
        assert_eq!(normal.len(), 2);
        assert!(!pane.visible);
    }
}

pub fn capture(state: &mut State, ctx: &egui::Context) {
    ctx.input_mut(|input| {
        state
            .ui
            .clipboard
            .shortcut(&mut input.events, input.modifiers)
    });
    ctx.add_plugin(TextCapture::default());
    let pending = ctx
        .with_plugin(|p: &mut TextCapture| std::mem::take(&mut p.pending.entries))
        .unwrap_or_default();
    for entry in pending.into_iter().rev() {
        state.ui.clipboard.history.record(entry);
    }
    // Imported text is remembered when pasted, not by monitoring other apps.
    for text in ctx.input(|i| {
        i.events
            .iter()
            .filter_map(|e| match e {
                egui::Event::Paste(text) => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    }) {
        state.ui.clipboard.history.record(Content::Text(text));
    }
}

pub fn draw(state: &mut State, ui: &mut Ui) {
    super::panel_header(ui, "Clipboard", &mut state.ui.clipboard.visible);
    ui.label("Choose an entry, then paste normally, or use its paste button.");
    ui.small("Session only · 100 entries · up to 1 MiB each");
    ui.add(TextEdit::singleline(&mut state.ui.clipboard.search).hint_text("Search"));
    
    if ui
        .button("Clear history")
        .on_hover_text("Leaves the system clipboard unchanged")
        .clicked()
    {
        state.ui.clipboard.history.entries.clear();
    }
    ui.separator();
    let query = state.ui.clipboard.search.to_lowercase();
    let mut action = None;
    let mut remove = None;
    ScrollArea::vertical()
        .id_salt("clipboard_entries")
        .show(ui, |ui| {
            if state.ui.clipboard.history.entries.is_empty() {
                ui.label("Copy text or files in Peregrine to begin.");
            }
            for (index, entry) in state.ui.clipboard.history.entries.iter().enumerate() {
                let (title, body) = match entry {
                    Content::Text(text) => (
                        format!("Text · {} characters", text.chars().count()),
                        Cow::Borrowed(text.as_str()),
                    ),
                    Content::Files { paths, cut } => (
                        format!(
                            "{} · {} file(s)",
                            if *cut { "Cut" } else { "Copy" },
                            paths.len()
                        ),
                        Cow::Owned(
                            paths
                                .iter()
                                .map(|p| p.display().to_string())
                                .collect::<Vec<_>>()
                                .join("\n"),
                        ),
                    ),
                };
                if !body.to_lowercase().contains(&query) {
                    continue;
                }
                ui.push_id(index, |ui| {
                    ui.strong(title);
                    let mut preview: String = body.chars().take(240).collect();
                    if body.chars().count() > 240 {
                        preview.push('…');
                    }
                    ui.label(preview);
                    egui::CollapsingHeader::new("Full contents").show(ui, |ui| {
                        ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
                            ui.label(body.as_ref());
                        });
                    });
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .button("Use")
                            .on_hover_text("Copy this entry to the system clipboard")
                            .clicked()
                        {
                            action = Some((entry.clone(), false));
                        }
                        let (label, enabled) = match entry {
                            Content::Text(_) => ("Paste to CLI", true),
                            Content::Files { .. } => (
                                "Paste to folder",
                                state.paste.is_none() && !state.is_remote(),
                            ),
                        };
                        if ui
                            .add_enabled(enabled, Button::new(label))
                            .on_hover_text(match entry {
                                Content::Text(_) => {
                                    "Insert at the command input selection; never execute"
                                        .to_owned()
                                }
                                Content::Files { .. } => {
                                    format!("Destination: {}", state.cwd().display())
                                }
                            })
                            .clicked()
                        {
                            action = Some((entry.clone(), true));
                        }
                        if ui.small_button("Remove").clicked() {
                            remove = Some(index);
                        }
                    });
                    ui.separator();
                });
            }
        });
    if let Some(index) = remove {
        state.ui.clipboard.history.entries.remove(index);
    }
    if let Some((entry, paste)) = action {
        match &entry {
            Content::Text(text) => {
                ui.ctx().copy_text(text.clone());
                if paste {
                    state.ui.clipboard.pending_text = Some(text.clone());
                    state.ui.files_active = false;
                }
            }
            Content::Files { paths, cut } => {
                if let Err(e) = clipboard::set_files(paths, *cut) {
                    state.push_out(
                        format!("clipboard: {e} (history remains available)"),
                        OutType::StdErr,
                    );
                }
                if paste {
                    state.paste = Some(Paste::start(
                        paths.clone(),
                        state.cwd().to_path_buf(),
                        *cut,
                        ui.ctx().clone(),
                    ));
                }
            }
        }
        state.ui.clipboard.history.record(entry);
    }
}
