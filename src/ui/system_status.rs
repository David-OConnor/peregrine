//! Displays information about the system: What processor is used, what GPUs are used, GPU driver version,
//! How much RAM is installed, how much is used, CPU usage, disk space available, a list of hard disks and their total
//! storage, etc. How many CPU cores, SIMD instruction sets supported, etc.

use egui::{ProgressBar, RichText, ScrollArea, Ui};

use super::panel_header;
use crate::{state::State, system_status::Snapshot};

pub(super) const COL_WIDTH: f32 = 300.;

pub(super) fn draw(state: &mut State, ui: &mut Ui) {
    panel_header(ui, "System status", &mut state.ui.panel_vis.system_status);

    if !state.ui.panel_vis.system_status {
        return;
    }

    let monitor = &mut state.ui.system_status;
    let context = ui.ctx().clone();
    monitor.update(move || context.request_repaint());

    if let Some(delay) = monitor.next_update() {
        ui.ctx().request_repaint_after(delay);
    }

    if let Some(error) = &monitor.error {
        ui.colored_label(ui.visuals().error_fg_color, error);
    }

    let Some(snapshot) = &monitor.snapshot else {
        if monitor.error.is_none() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Reading local system…");
            });
        }

        return;
    };

    let age = snapshot.sampled_at.elapsed().as_secs();

    if age > 6 {
        ui.weak(format!("Last sample: {age}s ago"));
    }

    ScrollArea::vertical()
        .id_salt("system_status_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            contents(snapshot, ui);
        });
}

fn contents(snapshot: &Snapshot, ui: &mut Ui) {
    let hardware = &snapshot.hardware;
    ui.weak("Local system · updates every 2s");
    ui.add_space(6.);
    ui.strong("CPU");
    ui.label(&hardware.cpu);
    let cores = match (hardware.physical_cores, hardware.logical_cores) {
        (Some(physical), Some(logical)) => {
            format!("{physical} cores · {logical} logical processors")
        }
        (None, Some(logical)) => format!("{logical} logical processors"),
        (Some(physical), None) => format!("{physical} cores"),
        (None, None) => "Core count unavailable".into(),
    };
    ui.label(cores);
    ui.label(format!("SIMD: {}", hardware.simd))
        .on_hover_text("Highest detected SIMD tier available to this process");

    if let Some(usage) = snapshot.cpu_usage {
        meter(ui, usage / 100., format!("{usage:.0}% CPU"));
    } else if snapshot.cpu_available {
        ui.weak("CPU usage: waiting for two samples");
    } else {
        ui.weak("CPU usage unavailable");
    }

    ui.separator();
    ui.strong("Memory");

    if let Some(installed) = hardware.installed_memory {
        ui.label(format!("{} installed", bytes(installed)));
    } else if let Some(memory) = &snapshot.memory {
        ui.label(format!("{} usable RAM", bytes(memory.total)));
    }

    if let Some(memory) = &snapshot.memory {
        let used = memory.total.saturating_sub(memory.available);
        let fraction = ratio(used, memory.total);
        meter(
            ui,
            fraction,
            format!(
                "{} / {} used · {:.0}%",
                bytes(used),
                bytes(memory.total),
                fraction * 100.
            ),
        );
        ui.weak(format!("{} available", bytes(memory.available)));
    } else {
        ui.weak("Memory usage unavailable");
    }

    ui.separator();
    ui.strong("GPUs");

    if hardware.gpus.is_empty() {
        ui.weak("No GPUs reported");
    }

    for gpu in &hardware.gpus {
        ui.label(RichText::new(&gpu.name).strong());
        if gpu.driver_available == Some(false) {
            ui.colored_label(ui.visuals().warn_fg_color, &gpu.status);
        } else {
            ui.label(&gpu.status);
        }
        ui.label(format!(
            "Driver: {}",
            gpu.driver.as_deref().unwrap_or("version unavailable")
        ));

        if let Some(memory) = gpu.memory {
            ui.weak(format!("{} dedicated memory", bytes(memory)));
        }

        ui.add_space(4.);
    }

    ui.separator();
    ui.strong("Storage");

    if hardware.disks.is_empty() {
        ui.weak("No physical disks reported");
    }

    for disk in &hardware.disks {
        ui.label(&disk.name);
        ui.weak(
            disk.capacity
                .map_or_else(|| "Capacity unavailable".into(), bytes),
        );
    }

    if !snapshot.volumes.is_empty() {
        ui.add_space(6.);
        ui.strong("Volumes");
    }

    for volume in snapshot.volumes.iter() {
        let used = volume.total.saturating_sub(volume.available);
        ui.label(&volume.name);
        meter(
            ui,
            ratio(used, volume.total),
            format!("{} free / {}", bytes(volume.available), bytes(volume.total)),
        );
        ui.add_space(4.);
    }

    ui.weak("Storage refreshes every 15s · hardware every 60s")
        .on_hover_text(
            "Sampling pauses when this pane is hidden. Network and optical volumes are skipped.",
        );

    for note in &hardware.notes {
        ui.weak(note);
    }
}

fn meter(ui: &mut Ui, fraction: f32, text: String) {
    let bar = ProgressBar::new(fraction.clamp(0., 1.))
        .text(text)
        .animate(false);
    let bar = if fraction >= 0.9 {
        bar.fill(ui.visuals().warn_fg_color)
    } else {
        bar
    };

    ui.add(bar);
}

fn ratio(used: u64, total: u64) -> f32 {
    if total == 0 {
        return 0.;
    }

    (used as f64 / total as f64).clamp(0., 1.) as f32
}

fn bytes(value: u64) -> String {
    const GIB: f64 = 1024. * 1024. * 1024.;
    let gib = value as f64 / GIB;

    if gib >= 1024. {
        return format!("{:.1} TiB", gib / 1024.);
    }

    if gib >= 1. {
        return format!("{gib:.1} GiB");
    }

    format!("{:.0} MiB", value as f64 / (1024. * 1024.))
}
