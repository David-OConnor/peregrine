use std::{
    collections::HashMap,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use egui::{Context, Image, Label, ScrollArea, TextureHandle, TextureOptions, Ui, vec2};

use crate::{
    applications::{self, Snapshot},
    state::State,
};

pub(super) const COL_WIDTH: f32 = 200.;
pub(super) const MIN_COL_WIDTH: f32 = 160.;
const ICON_SIZE: f32 = 20.;
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

type Sample = Result<Snapshot, String>;

#[derive(Default)]
pub struct Panel {
    snapshot: Arc<Snapshot>,
    rows: Vec<String>,
    icons: HashMap<String, TextureHandle>,
    error: Option<String>,
    pending: Option<mpsc::Receiver<Sample>>,
    next: Option<Instant>,
}

impl Panel {
    fn refresh(&mut self, ctx: &Context) {
        if let Some(pending) = &self.pending {
            let result = match pending.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Err("The application list could not be refreshed.".into())
                }
            };
            self.pending = None;
            self.next = Some(Instant::now() + REFRESH_INTERVAL);
            match result {
                Ok(snapshot) => {
                    self.icons.retain(|id, _| snapshot.apps.contains_key(id));
                    self.rows = snapshot.apps.keys().cloned().collect();
                    self.rows.sort_by_cached_key(|id| {
                        (snapshot.apps[id].name.to_lowercase(), id.clone())
                    });
                    self.snapshot = Arc::new(snapshot);
                    self.error = None;
                }
                Err(error) => self.error = Some(error),
            }
        }
        if let Some(next) = self.next
            && let Some(delay) = next.checked_duration_since(Instant::now())
        {
            ctx.request_repaint_after(delay);
            return;
        }

        let (send, receive) = mpsc::channel();
        let previous = Arc::clone(&self.snapshot);
        let wake = ctx.clone();
        match thread::Builder::new()
            .name("applications".into())
            .spawn(move || {
                let _ = send.send(applications::sample(&previous));
                wake.request_repaint();
            }) {
            Ok(_) => self.pending = Some(receive),
            Err(error) => {
                self.error = Some(format!("Could not read running applications: {error}"));
                self.next = Some(Instant::now() + REFRESH_INTERVAL);
                ctx.request_repaint_after(REFRESH_INTERVAL);
            }
        }
    }
}

pub(super) fn draw(state: &mut State, ui: &mut Ui) {
    super::panel_header(
        ui,
        super::panes::Pane::Applications.title(),
        &mut state.ui.panel_vis.applications,
    );
    if !state.ui.panel_vis.applications {
        return;
    }
    let panel = &mut state.ui.applications;
    panel.refresh(ui.ctx());
    if let Some(error) = &panel.error {
        ui.weak(error);
        return;
    }
    if let Some(notice) = panel.snapshot.notice {
        ui.weak(notice);
    }
    if panel.rows.is_empty() {
        if panel.next.is_none() {
            ui.spinner();
        } else {
            ui.weak("No running applications found.");
        }
        return;
    }
    ScrollArea::vertical().show(ui, |ui| {
        for id in &panel.rows {
            let app = &panel.snapshot.apps[id];
            ui.horizontal(|ui| {
                let size = vec2(ICON_SIZE, ICON_SIZE);
                if let Some(icon) = &app.icon {
                    let texture = panel.icons.entry(id.clone()).or_insert_with(|| {
                        ui.ctx().load_texture(
                            format!("app-icon:{id}"),
                            icon.clone(),
                            TextureOptions::LINEAR,
                        )
                    });
                    ui.add_sized(size, Image::from_texture(&*texture).fit_to_exact_size(size));
                } else {
                    ui.allocate_space(size);
                }
                ui.add(Label::new(&app.name).truncate())
                    .on_hover_text(&app.name);
            });
        }
    });
}
