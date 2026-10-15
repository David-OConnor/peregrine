//! Clipboard history, kept between runs. Ordinary paste always uses the OS
//! clipboard; history actions explicitly choose older content, without
//! changing focus or inserting text until requested.
use std::borrow::Cow;

use chrono::{DateTime, Local, Utc};
use egui::{Align, Button, Color32, Frame, Layout, RichText, ScrollArea, Stroke, TextEdit, Ui};

use crate::{
    OutType,
    clipboard::{self, Content, History, Paste, SysClipboard, TextCapture},
    state::State,
};

/// Medium blue for the contents of text entries, per panel theme.
const TEXT_COLOR_DARK: Color32 = Color32::from_rgb(90, 150, 235);
const TEXT_COLOR_LIGHT: Color32 = Color32::from_rgb(30, 90, 200);

/// The outline and "sys" label of the entry the system clipboard holds.
const SYS_COLOR_DARK: Color32 = Color32::from_rgb(70, 220, 100);
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

/// An entry's first row: "Text 14:05 · 12 characters", with `sys` (its
/// colour) at the right if it's on the system clipboard.
fn title_row(ui: &mut Ui, kind: &str, detail: &str, added: &DateTime<Utc>, sys: Option<Color32>) {
    let added = added.with_timezone(&Local);
    ui.horizontal(|ui| {
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
/// Copy events use the same label selection and TextCapture path as Ctrl+C.
#[derive(Default)]
struct OutputCopy {
    target: Option<(egui::ViewportId, egui::LayerId, egui::Rect)>,
    selection_in_output: bool,
    copying: bool,
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
                    }
                    if *button == egui::PointerButton::Secondary {
                        if *pressed {
                            self.copying = in_output
                                && self.selection_in_output
                                && ctx
                                    .with_plugin(
                                        |s: &mut egui::text_selection::LabelSelectionState| {
                                            s.has_selection()
                                        },
                                    )
                                    .unwrap_or(false);
                            if self.copying {
                                *event = egui::Event::Copy;
                            }
                        } else if self.copying {
                            self.copying = false;
                            return false;
                        }
                    }
                }
                egui::Event::WindowFocused(false) => self.copying = false,
                _ => {}
            }
            true
        });
    }
}

/// Remember the visible output area, excluding its scrollbar and other panes.
pub fn copy_output_on_right_click(ui: &Ui, rect: egui::Rect) {
    ui.ctx().add_plugin(OutputCopy::default());
    ui.ctx().with_plugin(|p: &mut OutputCopy| {
        p.target = Some((ui.ctx().viewport_id(), ui.layer_id(), rect));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    struct OutputUi {
        ctx: egui::Context,
        state: State,
        rows: [egui::Rect; 3],
        time: f64,
    }

    impl OutputUi {
        fn new() -> Self {
            let mut test = Self {
                ctx: egui::Context::default(),
                state: State::default(),
                rows: [egui::Rect::NOTHING; 3],
                time: 0.0,
            };
            test.state.ui.cli_input[0] = "input must not be copied".into();
            test.frame(vec![]);
            test.frame(vec![]);
            test
        }

        fn frame(&mut self, events: Vec<egui::Event>) -> Vec<String> {
            self.time += 0.1;
            let output = self.ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(400.0, 240.0),
                    )),
                    time: Some(self.time),
                    events,
                    ..Default::default()
                },
                |ui| {
                    capture(&mut self.state, ui.ctx());
                    egui::Panel::left("other")
                        .default_size(100.0)
                        .show(ui, |ui| {
                            self.rows[2] = ui.label("Sidebar text").rect;
                        });
                    egui::Panel::bottom("input").show(ui, |ui| {
                        super::super::term_in(&mut self.state, ui);
                    });
                    egui::CentralPanel::default().show(ui, |ui| {
                        let output =
                            ScrollArea::vertical()
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    let font = egui::TextStyle::Monospace.resolve(ui.style());
                                    for (index, text) in
                                        ["alpha café 🦅", "\x1b[31mbravo charlie\x1b[0m"]
                                            .iter()
                                            .enumerate()
                                    {
                                        let job = crate::ansi::to_layout_job(
                                            &crate::ansi::parse(text),
                                            egui::Color32::WHITE,
                                            font.clone(),
                                        );
                                        self.rows[index] = ui.label(job).rect;
                                    }
                                });
                        copy_output_on_right_click(ui, output.inner_rect);
                    });
                },
            );
            let copied = output
                .platform_output
                .commands
                .iter()
                .filter_map(|command| match command {
                    egui::OutputCommand::CopyText(text) => Some(text.clone()),
                    _ => None,
                })
                .collect();
            output.drop_without_applying_deltas();
            copied
        }

        fn button(
            &mut self,
            pos: egui::Pos2,
            button: egui::PointerButton,
            pressed: bool,
        ) -> Vec<String> {
            self.frame(vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ])
        }

        fn select(&mut self, start: usize, end: usize) {
            let start = self.rows[start].left_center() + egui::vec2(0.1, 0.0);
            let end = self.rows[end].right_center() - egui::vec2(0.1, 0.0);
            self.button(start, egui::PointerButton::Primary, true);
            self.frame(vec![egui::Event::PointerMoved(end)]);
            self.button(end, egui::PointerButton::Primary, false);
        }
    }

    #[test]
    fn output_right_click_matches_ctrl_c_and_records_clipboard_history() {
        let mut test = OutputUi::new();
        test.select(0, 1);
        let keyboard = test.frame(vec![egui::Event::Copy]);
        assert_eq!(keyboard, vec!["alpha café 🦅\nbravo charlie"]);
        test.frame(vec![]);
        test.state.ui.clipboard.history.entries.clear();

        // Clicking inside the selection must preserve both selected rows.
        let pos = test.rows[0].center();
        let mouse = test.button(pos, egui::PointerButton::Secondary, true);
        assert_eq!(mouse, keyboard);
        assert!(
            test.button(pos, egui::PointerButton::Secondary, false)
                .is_empty()
        );
        assert_eq!(
            test.state.ui.clipboard.history.contents(),
            vec![Content::Text(keyboard[0].clone())]
        );
        // The same selection remains available for subsequent copies.
        assert_eq!(test.frame(vec![egui::Event::Copy]), keyboard);
    }

    #[test]
    fn output_right_click_requires_an_output_selection() {
        let mut test = OutputUi::new();
        let pos = test.rows[0].center();
        assert!(
            test.button(pos, egui::PointerButton::Secondary, true)
                .is_empty()
        );
        assert!(
            test.button(pos, egui::PointerButton::Secondary, false)
                .is_empty()
        );

        test.select(2, 2);
        assert!(
            test.button(pos, egui::PointerButton::Secondary, true)
                .is_empty()
        );
        assert!(
            test.button(pos, egui::PointerButton::Secondary, false)
                .is_empty()
        );

        test.select(0, 1);
        let sidebar = test.rows[2].center();
        assert!(
            test.button(sidebar, egui::PointerButton::Secondary, true)
                .is_empty()
        );
        assert!(
            test.button(sidebar, egui::PointerButton::Secondary, false)
                .is_empty()
        );
        test.frame(vec![]);
        assert!(test.state.ui.clipboard.history.entries.is_empty());
    }

    #[test]
    fn another_apps_copy_joins_the_history_and_takes_the_mark() {
        let mut test = OutputUi::new();
        test.select(0, 1);
        let copied = test.frame(vec![egui::Event::Copy]);
        test.frame(vec![]);
        let clipboard = &test.state.ui.clipboard;
        assert_eq!(clipboard.sys.find_in(&clipboard.history), Some(0));

        let theirs = Content::Text("from another app".into());
        clipboard.sys.observe(Some(theirs.clone()));
        test.frame(vec![]);
        let clipboard = &test.state.ui.clipboard;
        assert_eq!(
            clipboard.history.contents(),
            vec![theirs.clone(), Content::Text(copied[0].clone())]
        );
        assert_eq!(clipboard.sys.find_in(&clipboard.history), Some(0));

        // Pasting it keeps the entry the watcher made, and its time.
        let added = clipboard.history.entries[0].added;
        test.frame(vec![egui::Event::Paste("from another app".into())]);
        let clipboard = &test.state.ui.clipboard;
        assert_eq!(clipboard.history.entries.len(), 2);
        assert_eq!(clipboard.history.entries[0].added, added);
    }

    #[test]
    fn added_label_shows_the_date_only_before_today() {
        let now = Local::now();
        assert_eq!(added_label(&now), now.format("%H:%M").to_string());
        let earlier = now - chrono::TimeDelta::days(1);
        assert_eq!(
            added_label(&earlier),
            earlier.format("%m/%d %H:%M").to_string()
        );
    }

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
        let mut visible = false;
        let modifiers = egui::Modifiers::CTRL | egui::Modifiers::SHIFT;
        let mut events = vec![egui::Event::Paste("text".into())];
        pane.shortcut(&mut events, modifiers, &mut visible);
        assert!(visible);
        assert!(events.is_empty());
        let mut repeated = vec![
            egui::Event::Paste("text".into()),
            release(egui::Modifiers::NONE),
        ];
        pane.shortcut(&mut repeated, modifiers, &mut visible);
        assert!(visible);
        assert!(repeated.is_empty());
        // File and empty clipboards produce only a key release.
        let mut events = vec![release(modifiers)];
        pane.shortcut(&mut events, modifiers, &mut visible);
        assert!(!visible);
        assert!(events.is_empty());
        let mut normal = vec![
            egui::Event::Paste("normal".into()),
            release(egui::Modifiers::CTRL),
        ];
        pane.shortcut(&mut normal, egui::Modifiers::CTRL, &mut visible);
        assert_eq!(normal.len(), 2);
        assert!(!visible);
    }
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
    ui.label("Choose an entry, then paste normally, or use its paste button.");
    ui.small("Kept between runs · 100 entries · up to 1 MiB each");
    ui.add(TextEdit::singleline(&mut state.ui.clipboard.search).hint_text("Search"));

    if ui
        .button("Clear history")
        .on_hover_text("Leaves the system clipboard unchanged")
        .clicked()
    {
        state.ui.clipboard.history.clear();
    }
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
        .show(ui, |ui| {
            if state.ui.clipboard.history.entries.is_empty() {
                ui.label("Copy text or files to begin.");
            }
            for (index, history_entry) in state.ui.clipboard.history.entries.iter().enumerate() {
                let entry = &history_entry.content;
                let (kind, detail, body) = match entry {
                    Content::Text(text) => (
                        "Text",
                        format!("{} characters", text.chars().count()),
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
                            let mut preview: String = body.chars().take(240).collect();
                            let truncated = body.chars().count() > 240;
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
                            ui.label(styled(&preview));
                            if truncated {
                                egui::CollapsingHeader::new("Full contents").show(ui, |ui| {
                                    ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
                                        ui.label(styled(&body));
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
