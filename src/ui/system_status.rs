//! Displays information about the system: What processor is used, what GPUs are used, GPU driver version,
//! How much RAM is installed, how much is used, CPU usage, disk space available, a list of hard disks and their total
//! storage, etc. How many CPU cores, SIMD instruction sets supported, etc.

use egui::{ProgressBar, RichText, ScrollArea, Ui};

use super::panel_header;
use crate::{state::State, system_status::Snapshot};

pub(super) const COL_WIDTH: f32 = 360.;

const SECTION_SPACING: f32 = 10.;  // 2x of these plus a horizontal line


fn section_separator(ui: &mut Ui) {
    ui.add_space(SECTION_SPACING);
    ui.separator();
    ui.add_space(SECTION_SPACING);
}


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
        meter(ui, usage / 100., format!("{usage:.0}%"));
    } else if snapshot.cpu_available {
        ui.weak("CPU usage: waiting for two samples");
    } else {
        ui.weak("CPU usage unavailable");
    }

    section_separator(ui);
    ui.strong("Memory");

    if let Some(installed) = hardware.installed_memory {
        ui.label(format!("{} installed", bytes(installed)));
    } else if let Some(memory) = &snapshot.memory {
        ui.label(format!("{} usable RAM", bytes(memory.total)));
    }

    if let Some(memory) = &snapshot.memory {
        let used = memory.total.saturating_sub(memory.available);
        let fraction = ratio(used, memory.total);
        let (used_text, total_text) = compact_pair(used, memory.total);

        meter(
            ui,
            fraction,
            format!("{used_text} / {total_text} used · {:.0}%", fraction * 100.),
        );
        ui.weak(format!("{} available", bytes(memory.available)));
    } else {
        ui.weak("Memory usage unavailable");
    }

    section_separator(ui);
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

    section_separator(ui);
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
        let (free_text, total_text) = compact_pair(volume.available, volume.total);
        ui.label(&volume.name);
        meter(
            ui,
            ratio(used, volume.total),
            format!("{free_text} free / {total_text}"),
        );
        ui.add_space(4.);
    }

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

const MIB: f64 = 1024. * 1024.;
const GIB: f64 = MIB * 1024.;
const TIB: f64 = GIB * 1024.;

/// The display unit for a byte count: (divisor, decimal places, suffix).
fn byte_unit(value: u64) -> (f64, usize, &'static str) {
    let value = value as f64;

    if value >= TIB {
        return (TIB, 1, "TiB");
    }

    if value >= GIB {
        return (GIB, 1, "GiB");
    }

    (MIB, 0, "MiB")
}

fn bytes(value: u64) -> String {
    let (divisor, precision, suffix) = byte_unit(value);
    format!("{:.precision$} {suffix}", value as f64 / divisor)
}

/// Formats `value` and `total` both scaled to the unit `bytes` would pick for `total`, with a
/// short unit letter on the total only, e.g. ("10.7", "15.8 G").
fn compact_pair(value: u64, total: u64) -> (String, String) {
    let (divisor, precision, suffix) = byte_unit(total);
    let format = |v: u64| format!("{:.precision$}", v as f64 / divisor);

    (format(value), format!("{} {}", format(total), &suffix[..1]))
}
