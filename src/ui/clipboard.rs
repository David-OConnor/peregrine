//! Clipboard history, kept between runs. Ordinary paste always uses the OS
//! clipboard; history actions explicitly choose older content, without
//! changing focus or inserting text until requested.
use std::{
    borrow::Cow,
    path::{MAIN_SEPARATOR, Path, PathBuf},
};

use chrono::{DateTime, Local, Utc};
use egui::{
    Align, Button, Color32, Frame, Layout, RichText, ScrollArea, Stroke, TextEdit, TextWrapMode, Ui,
};

use crate::{
    OutType,
    clipboard::{self, Content, History, Paste, SysClipboard, TextCapture},
    state::State,
};

/// Medium blue for the contents of text entries, per panel theme.
const TEXT_COLOR_DARK: Color32 = Color32::from_rgb(90, 150, 235);
const TEXT_COLOR_LIGHT: Color32 = Color32::from_rgb(30, 90, 200);

/// The outline and "sys" label of the entry the system clipboard holds.
const SYS_COLOR_DARK: Color32 = Color32::from_rgb(50, 140, 70);
const SYS_COLOR_LIGHT: Color32 = Color32::from_rgb(20, 150, 50);

/// When an entry was copied, in local time: "14:05" today, "10/14 14:05"
/// before.
fn added_label(added: &DateTime<Local>) -> String {
    if added.date_naive() == Local::now().date_naive() {
        added.format("%H:%M").to_string()
    } else {
        added.format("%m/%d %H:%M").to_string()
    }
}

/// `path` as just its dir's name and its own: "Downloads\a.txt".
fn short_path(path: &Path) -> String {
    let Some(name) = path.file_name() else {
        return path.display().to_string();
    };
    match path.parent().and_then(Path::file_name) {
        Some(dir) => format!(
            "{}{MAIN_SEPARATOR}{}",
            dir.to_string_lossy(),
            name.to_string_lossy()
        ),
        None => name.to_string_lossy().into_owned(),
    }
}

/// [Short](short_path) paths, one per line, bulleted when there are several.
fn compact_paths(paths: &[&Path]) -> String {
    let bullet = if paths.len() > 1 { "• " } else { "" };
    paths
        .iter()
        .map(|p| format!("{bullet}{}", short_path(p)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `text`'s lines, if every one is an absolute path.
fn text_paths(text: &str) -> Option<Vec<&Path>> {
    let paths: Vec<&Path> = text.lines().map(Path::new).collect();
    (!paths.is_empty() && paths.iter().all(|p| p.is_absolute())).then_some(paths)
}

/// An entry's first row: "Text 14:05 · 12 characters", with `sys` (its
/// colour) at the right if it's on the system clipboard.
fn title_row(ui: &mut Ui, kind: &str, detail: &str, added: &DateTime<Utc>, sys: Option<Color32>) {
    let added = added.with_timezone(&Local);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 5.0;
        ui.strong(kind);
        ui.label(RichText::new(added_label(&added)).weak())
            .on_hover_text(added.format("Copied %Y-%m-%d %H:%M:%S").to_string());
        ui.strong(format!("· {detail}"));
        if let Some(color) = sys {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new("sys").strong().color(color))
                    .on_hover_text("On the system clipboard: an ordinary paste gives this");
            });
        }
    });
}

#[derive(Default)]
pub struct ClipboardPane {
    pub history: History,
    /// What's on the system clipboard, to mark its entry.
    pub sys: SysClipboard,
    search: String,
    pub pending_text: Option<String>,
    shortcut_down: bool,
}

impl ClipboardPane {
    /// egui-winit converts Ctrl+Shift+V presses to Paste (or emits nothing
    /// for a file/empty clipboard), but still forwards the V release.
    /// `visible` is the pane's [shell::PanelVis] flag, toggled by it.
    fn shortcut(
        &mut self,
        events: &mut Vec<egui::Event>,
        modifiers: egui::Modifiers,
        visible: &mut bool,
    ) {
        events.retain(|event| match event {
            egui::Event::Paste(_) if modifiers.ctrl && modifiers.shift => {
                if !self.shortcut_down {
                    *visible = !*visible;
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
                    *visible = !*visible;
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

/// Translate output right-clicks before egui treats them as selection drags.
/// Selected scrollback is copied from its model, including offscreen rows.
#[derive(Default)]
struct OutputCopy {
    target: Option<(egui::ViewportId, egui::LayerId, egui::Rect)>,
    selection_in_output: bool,
    copying: bool,
    has_selection: bool,
    copy_requested: bool,
}

impl egui::plugin::Plugin for OutputCopy {
    fn debug_name(&self) -> &'static str {
        "Output right-click copy"
    }

    fn input_hook(&mut self, ctx: &egui::Context, input: &mut egui::RawInput) {
        let Some((viewport, layer, rect)) = self.target else {
            return;
        };
        if input.viewport_id != viewport {
            return;
        }
        input.events.retain_mut(|event| {
            match event {
                egui::Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    ..
                } => {
                    let in_output = rect.contains(*pos) && ctx.layer_id_at(*pos) == Some(layer);
                    if *button == egui::PointerButton::Primary && *pressed {
                        self.selection_in_output = in_output;
                        if !in_output {
                            self.has_selection = false;
                        }
                    }
                    if *button == egui::PointerButton::Secondary {
                        if *pressed {
                            self.copying =
                                in_output && self.selection_in_output && self.has_selection;
                            if self.copying {
                                self.copy_requested = true;
                                return false;
                            }
                        } else if self.copying {
                            self.copying = false;
                            return false;
                        }
                    }
                }
                egui::Event::Copy if self.has_selection && self.selection_in_output => {
                    self.copy_requested = true;
                    return false;
                }
                egui::Event::WindowFocused(false) => self.copying = false,
                _ => {}
            }
            true
        });
    }
}

/// Remember the visible output area, excluding its scrollbar and other panes.
pub fn copy_output_on_right_click(ui: &Ui, rect: egui::Rect, has_selection: bool) {
    ui.ctx().add_plugin(OutputCopy::default());
    ui.ctx().with_plugin(|p: &mut OutputCopy| {
        p.target = Some((ui.ctx().viewport_id(), ui.layer_id(), rect));
        p.has_selection = has_selection;
    });
}

pub(super) fn take_output_copy(ctx: &egui::Context) -> bool {
    ctx.with_plugin(|p: &mut OutputCopy| std::mem::take(&mut p.copy_requested))
        .unwrap_or(false)
}

pub fn capture(state: &mut State, ctx: &egui::Context) {
    ctx.input_mut(|input| {
        state.ui.clipboard.shortcut(
            &mut input.events,
            input.modifiers,
            &mut state.ui.panel_vis.clipboard,
        )
    });
    ctx.add_plugin(TextCapture::default());
    let pending = ctx
        .with_plugin(|p: &mut TextCapture| std::mem::take(&mut p.pending.entries))
        .unwrap_or_default();
    // A Ctrl+C / X in the Files panel last frame. If it copied text instead
    // (selected in the details pane, say, or in its search box), that's
    // what the system clipboard holds now; otherwise the selected files go
    // there.
    if let Some(cut) = state.ui.files_copy_pending.take()
        && pending.is_empty()
    {
        state.copy_selected(cut);
    }
    // Newest first: the first is what the system clipboard now holds.
    if let Some(newest) = pending.first() {
        state.ui.clipboard.sys.set(newest.content.clone());
    }
    for entry in pending.into_iter().rev() {
        state.ui.clipboard.history.record(entry.content);
    }
    // What other apps copied, as the watcher found it. These came after
    // Peregrine's own copies above, which the OS got at the end of last
    // frame, so the newest is what's on the clipboard now.
    let found = state.ui.clipboard.sys.take_found();
    if let Some(newest) = found.last() {
        state.ui.clipboard.sys.set(newest.clone());
    }
    for content in found {
        state.ui.clipboard.history.record(content);
    }
    // Text pasted from another app before the watcher has seen it. Text it
    // already knows keeps its entry's time.
    for text in ctx.input(|i| {
        i.events
            .iter()
            .filter_map(|e| match e {
                egui::Event::Paste(text) => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    }) {
        let content = Content::Text(text);
        if !state.ui.clipboard.sys.holds(&content) {
            state.ui.clipboard.sys.set(content.clone());
            state.ui.clipboard.history.record(content);
        }
    }
}

pub fn draw(state: &mut State, ui: &mut Ui) {
    super::panel_header(ui, "Clipboard", &mut state.ui.panel_vis.clipboard);

    ui.horizontal(|ui| {
        // Reserve the button's width before the search box fills the rest.
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui
                .button("Clear")
                .on_hover_text("Clears clipboard history and the system clipboard")
                .clicked()
            {
                state.ui.clipboard.history.clear();
                if let Err(e) = state.ui.clipboard.sys.clear() {
                    state.push_out(format!("clipboard: {e}"), OutType::StdErr);
                }
            }
            ui.add(
                TextEdit::singleline(&mut state.ui.clipboard.search)
                    .hint_text("Search")
                    .desired_width(f32::INFINITY),
            );
        });
    });

    ui.separator();

    let query = state.ui.clipboard.search.to_lowercase();
    let sys_index = state.ui.clipboard.sys.find_in(&state.ui.clipboard.history);
    let sys_color = if ui.visuals().dark_mode {
        SYS_COLOR_DARK
    } else {
        SYS_COLOR_LIGHT
    };

    let mut action = None;
    let mut remove = None;

    ScrollArea::vertical()
        .id_salt("clipboard_entries")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.style_mut().wrap_mode = Some(TextWrapMode::Wrap);
            if state.ui.clipboard.history.entries.is_empty() {
                ui.label("Copy text or files to begin.");
            }
            for (index, history_entry) in state.ui.clipboard.history.entries.iter().enumerate() {
                let entry = &history_entry.content;
                let (kind, detail, body) = match entry {
                    Content::Text(text) => (
                        "Text",
                        format!("{} chars", text.chars().count()),
                        Cow::Borrowed(text.as_str()),
                    ),
                    Content::Files { paths, cut } => (
                        if *cut { "Cut" } else { "Copy" },
                        format!("{} file(s)", paths.len()),
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

                // Paths show as just their dir and name; hover for the full ones.
                let compact = match entry {
                    Content::Files { paths, .. } => Some(compact_paths(
                        &paths.iter().map(PathBuf::as_path).collect::<Vec<_>>(),
                    )),
                    Content::Text(text) => text_paths(text).map(|paths| compact_paths(&paths)),
                };

                let shown = compact.as_deref().unwrap_or(&body);
                let on_sys = sys_index == Some(index);

                ui.push_id(index, |ui| {
                    // Every entry has an outline, so marking one doesn't shift it.
                    let outline = if on_sys {
                        sys_color
                    } else {
                        Color32::TRANSPARENT
                    };
                    Frame::new()
                        .stroke(Stroke::new(2.0, outline))
                        .corner_radius(4)
                        .inner_margin(4)
                        .show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            let sys = on_sys.then_some(sys_color);
                            title_row(ui, kind, &detail, &history_entry.added, sys);
                            let mut preview: String = shown.chars().take(240).collect();
                            let truncated = shown.chars().count() > 240;
                            if truncated {
                                preview.push('…');
                            }

                            let color = matches!(entry, Content::Text(_)).then(|| {
                                if ui.visuals().dark_mode {
                                    TEXT_COLOR_DARK
                                } else {
                                    TEXT_COLOR_LIGHT
                                }
                            });

                            let styled = |text: &str| {
                                let text = RichText::new(text);
                                match color {
                                    Some(c) => text.color(c),
                                    None => text,
                                }
                            };

                            let label = ui.label(styled(&preview));
                            if compact.is_some() {
                                label.on_hover_text(body.as_ref());
                            }

                            if truncated {
                                egui::CollapsingHeader::new("Full contents").show(ui, |ui| {
                                    ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
                                        ui.label(styled(shown));
                                    });
                                });
                            }

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
                                        state.paste.is_none()
                                            && state.deleting.is_none()
                                            && !state.is_remote(),
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
                        });
                    ui.separator();
                });
            }
        });
    if let Some(index) = remove {
        state.ui.clipboard.history.remove(index);
    }
    if let Some((entry, paste)) = action {
        state.ui.clipboard.sys.set(entry.clone());
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
