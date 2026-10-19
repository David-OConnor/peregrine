//! SSH / remote

use eframe::epaint::text::TextWrapMode;
use egui::{RichText, ScrollArea, TextEdit, Ui};

use crate::{
    OutType,
    remote::Remote,
    state::State,
    ui,
    ui::{COLOR_EXECUTABLE, COLOR_STDERR_OK, ListAction},
};

/// Saved SSH remotes. Lists each `user@host:port` with Connect / Edit / delete
/// controls, an add/edit form, and — when the active tab is connected or
/// connecting — a status line with Disconnect. Passwords are kept in the OS
/// keyring (see `shell::secrets`), never in the panel or state file; SSH keys
/// need no setup here.
pub fn remote_terminals(state: &mut State, ui: &mut Ui) {
    ui::panel_header(ui, "Remote", &mut state.ui.panel_vis.remote_terminals);

    // Connection status for the active tab.
    if let Some(remote) = state.remote_here() {
        let (status, button) = match remote {
            Remote::Connecting(c) => (
                RichText::new(format!("◌ Connecting to {}…", c.label())).color(COLOR_STDERR_OK),
                "Cancel",
            ),
            Remote::Live(l) => (
                RichText::new(format!(
                    "● {}:{}",
                    l.session.label(),
                    l.session.display_cwd()
                ))
                .color(COLOR_EXECUTABLE)
                .strong(),
                "Disconnect",
            ),
        };
        ui.label(status);
        if ui.button(button).clicked() {
            state.disconnect_remote();
        }
        ui.separator();
    }

    // Add / edit form toggle.
    ui.horizontal(|ui| {
        let label = if state.ui.remote_form_open {
            "Close form"
        } else {
            "+ Add remote"
        };
        if ui.button(label).clicked() {
            state.ui.remote_form_open = !state.ui.remote_form_open;
            if !state.ui.remote_form_open {
                state.ui.remote_edit_idx = None;
            }
        }
    });

    if state.ui.remote_form_open {
        remote_form(state, ui);
    }

    ui.separator();

    let mut action: Option<ListAction> = None;

    ScrollArea::vertical()
        .id_salt("remote_terminals_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.style_mut().wrap_mode = Some(TextWrapMode::Wrap);
            if state.remote_terminals.is_empty() {
                ui.label(RichText::new("(no saved remotes)").italics());
                return;
            }
            for (i, rt) in state.remote_terminals.iter().enumerate() {
                ui.label(format!("{i}: {}@{}:{}", rt.username, rt.host, rt.port));
                ui.horizontal(|ui| {
                    if ui.button("Connect").clicked() {
                        action = Some(ListAction::ConnectRemote(i));
                    }
                    if ui.small_button("Edit").clicked() {
                        action = Some(ListAction::EditRemote(i));
                    }
                    if ui
                        .small_button("x")
                        .on_hover_text("Delete remote")
                        .clicked()
                    {
                        action = Some(ListAction::RemoveRemote(i));
                    }
                });
                ui.separator();
            }
        });

    ui::apply_action(state, action);
}

/// The add/edit form for a saved remote. Writes through
/// [State::save_remote_from_form] on Save (storing the password in the keyring
/// when one was entered).
fn remote_form(state: &mut State, ui: &mut Ui) {
    egui::Grid::new("remote_form_grid")
        .num_columns(2)
        .spacing([6.0, 4.0])
        .show(ui, |ui| {
            ui.label("Host");
            ui.add(
                TextEdit::singleline(&mut state.ui.remote_host)
                    .hint_text("example.com")
                    .desired_width(f32::INFINITY),
            );
            ui.end_row();

            ui.label("Port");
            ui.add(
                TextEdit::singleline(&mut state.ui.remote_port)
                    .hint_text("22")
                    .desired_width(f32::INFINITY),
            );
            ui.end_row();

            ui.label("User");
            ui.add(
                TextEdit::singleline(&mut state.ui.remote_user)
                    .hint_text("root")
                    .desired_width(f32::INFINITY),
            );
            ui.end_row();

            ui.label("Password");
            ui.add(
                TextEdit::singleline(&mut state.ui.remote_password)
                    .password(true)
                    .hint_text("(stored in OS keyring)")
                    .desired_width(f32::INFINITY),
            );
            ui.end_row();
        });

    ui.horizontal(|ui| {
        if ui.button("Save").clicked() {
            if let Err(e) = state.save_remote_from_form() {
                state.push_out(format!("remote: {e}"), OutType::StdErr);
            }
        }
        if ui
            .button("Save & connect")
            .on_hover_text("Save, then connect this tab")
            .clicked()
        {
            match state.save_remote_from_form() {
                Ok((host, port, user)) => state.connect_with(host, port, user),
                Err(e) => state.push_out(format!("remote: {e}"), OutType::StdErr),
            }
        }
        if ui.button("Cancel").clicked() {
            state.ui.remote_form_open = false;
            state.ui.remote_edit_idx = None;
            state.ui.remote_password.clear();
        }
    });
}
