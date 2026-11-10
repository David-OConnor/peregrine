//! Displays information about the system: What processor is used, what GPUs are used, GPU driver version,
//! How much RAM is installed, how much is used, CPU usage, disk space available, a list of hard disks and their total
//! storage, etc. How many CPU cores, SIMD instruction sets supported, etc.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use egui::{
    Align, Button, CollapsingHeader, Context, Label, Layout, ProgressBar, RichText, ScrollArea,
    TextStyle, Ui,
};
use egui_extras::{Column, TableBuilder};

use super::{chars_width, panel_header, path_job, search};
use crate::{
    state::State,
    system_status::{
        PROCESS_MEMORY, Snapshot,
        processes::{Figures, Period, Processes, Row, Sort, Table},
        space::{self, RootTargets, Scan, SteamLibrary},
    },
};

pub(super) const COL_WIDTH: f32 = 360.;

const SECTION_SPACING: f32 = 10.; // 2x of these plus a horizontal line

/// The most a Processes list takes before it scrolls.
const PROCESS_LIST_MAX_HEIGHT: f32 = 280.;

/// What sets the CPU and Memory Processes lists apart.
struct Measure {
    id: &'static str,
    figures: fn(&Row) -> &Figures,
    text: fn(f32) -> String,
    now_hint: &'static str,
}

const CPU: Measure = Measure {
    id: "cpu_processes",
    figures: |row| &row.cpu,
    text: |percent| format!("{percent:.1}%"),
    now_hint: "Share of the whole machine's CPU, since the last reading",
};

const MEMORY: Measure = Measure {
    id: "memory_processes",
    figures: |row| &row.memory,
    text: |bytes| short_bytes(bytes as u64),
    now_hint: PROCESS_MEMORY,
};

fn section_separator(ui: &mut Ui) {
    ui.add_space(SECTION_SPACING);
    ui.separator();
    ui.add_space(SECTION_SPACING);
}

pub(super) fn draw(state: &mut State, ui: &mut Ui) {
    panel_header(ui, "System status", &mut state.ui.panel_vis.system_status);
    ui.add_space(SECTION_SPACING);

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

    let mut roots_changed = false;

    ScrollArea::vertical()
        .id_salt("system_status_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            contents(
                snapshot,
                &mut monitor.cpu_processes,
                &mut monitor.memory_processes,
                ui,
            );

            let home = state.home.as_deref();
            ui.add_space(6.);
            steam(&mut monitor.steam, home, ui);
            roots_changed = rust_targets(
                &mut monitor.rust_targets,
                &mut state.rust_target_roots,
                &state.dir_bookmarks,
                home,
                ui,
            );

            for note in &snapshot.hardware.notes {
                ui.weak(note);
            }
        });

    if roots_changed {
        let _ = state.save();
    }
}

fn contents(snapshot: &Snapshot, cpu_table: &mut Table, memory_table: &mut Table, ui: &mut Ui) {
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

    process_list(ui, cpu_table, snapshot, &CPU);

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

        meter(
            ui,
            fraction,
            format!(
                "{} / {} used · {:.0}%",
                short_bytes(used),
                short_bytes(memory.total),
                fraction * 100.
            ),
        );
        ui.weak(format!("{} available", bytes(memory.available)));
    } else {
        ui.weak("Memory usage unavailable");
    }

    process_list(ui, memory_table, snapshot, &MEMORY);

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
        ui.label(&volume.name);
        meter(
            ui,
            ratio(used, volume.total),
            format!(
                "{} free / {}",
                short_bytes(volume.available),
                short_bytes(volume.total)
            ),
        );
        ui.add_space(4.);
    }
}

/// A folding list of what each program is using of `measure`, now and on
/// average. Nothing here runs while it's folded.
fn process_list(ui: &mut Ui, table: &mut Table, snapshot: &Snapshot, measure: &Measure) {
    let Some(processes) = &snapshot.processes else {
        ui.weak("Per-process usage unavailable");
        return;
    };

    CollapsingHeader::new("Processes")
        .id_salt(measure.id)
        .show(ui, |ui| {
            ui.style_mut().interaction.selectable_labels = false;

            if search::search_box(ui, &mut table.search, &format!("{}_search", measure.id)) {
                table.invalidate();
            }

            process_rows(ui, table, processes, snapshot.sampled_at, measure);

            if table.shown.is_empty() {
                ui.label(RichText::new("No matches.").italics().weak());
            }

            ui.weak(format!(
                "Recording for {}. Averages count only the time a process was running.",
                duration_text(processes.recorded)
            ));
        });
}

/// The table of a [process_list]. Its columns' headings sort it.
fn process_rows(
    ui: &mut Ui,
    table: &mut Table,
    processes: &Processes,
    sampled_at: Instant,
    measure: &Measure,
) {
    // Tall enough for a heading's button, at any GUI font size.
    let row_height = ui
        .spacing()
        .interact_size
        .y
        .max(ui.text_style_height(&TextStyle::Button) + 2. * ui.spacing().button_padding.y);
    let figures_width = chars_width(ui, TextStyle::Body, 6.5);
    let heading = |period| match period {
        Period::Now => ("Now", measure.now_hint),
        Period::All => ("All", "Average over all the time it's run while recording"),
        Period::HalfHour => ("30m", "Average over the last 30 minutes it ran"),
        Period::Minute => ("1m", "Average over the last minute it ran"),
    };

    TableBuilder::new(ui)
        .id_salt(measure.id)
        .striped(true)
        .cell_layout(Layout::right_to_left(Align::Center))
        .column(Column::remainder().at_least(60.).clip(true))
        .columns(Column::exact(figures_width), Period::ALL.len())
        .auto_shrink([false, true])
        .min_scrolled_height(0.)
        .max_scroll_height(PROCESS_LIST_MAX_HEIGHT)
        .header(row_height, |mut header| {
            header.col(|ui| {
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    sort_button(ui, table, Sort::Name, "Name", "Sort by name");
                });
            });

            for period in Period::ALL {
                let (label, hint) = heading(period);

                header.col(|ui| {
                    sort_button(ui, table, Sort::Usage(period), label, hint);
                });
            }
        })
        .body(|body| {
            // Here, so a click on a heading shows this frame.
            table.refresh(&processes.rows, measure.figures, sampled_at);

            body.rows(row_height, table.shown.len(), |mut row| {
                let process = &processes.rows[table.shown[row.index()]];
                let text = |text: &str| {
                    let text = RichText::new(text);

                    if process.instances == 0 {
                        text.weak()
                    } else {
                        text
                    }
                };

                row.col(|ui| {
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.add(
                            Label::new(text(&process.name))
                                .truncate()
                                .show_tooltip_when_elided(false),
                        )
                        .on_hover_ui(|ui| process_tooltip(ui, process));
                    });
                });

                for value in (measure.figures)(process) {
                    let value = value.map_or_else(|| "—".to_owned(), measure.text);

                    row.col(|ui| {
                        ui.label(text(&value));
                    });
                }
            });
        });
}

fn sort_button(ui: &mut Ui, table: &mut Table, sort: Sort, label: &str, hint: &str) {
    let selected = table.sort == sort;

    if ui
        .add(Button::selectable(selected, label))
        .on_hover_text(hint)
        .clicked()
        && !selected
    {
        table.sort = sort;
        table.invalidate();
    }
}

fn process_tooltip(ui: &mut Ui, process: &Row) {
    ui.strong(&*process.name);

    match process.instances {
        0 => ui.label("Not running"),
        1 => ui.label("1 process running"),
        count => ui.label(format!("{count} processes running")),
    };

    ui.label(format!(
        "Recorded running for {}",
        duration_text(process.running)
    ));
}

fn duration_text(duration: Duration) -> String {
    let seconds = duration.as_secs();

    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m {:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h {:02}m", seconds / 3600, seconds / 60 % 60),
    }
}

fn repaint(ctx: &Context) -> impl Fn() + Send + 'static {
    let ctx = ctx.clone();

    move || ctx.request_repaint()
}

/// A row: `label`'s button, then a spinner while `scan` runs, or `total` of
/// its result, or its error. True when clicked; the caller starts the scan.
fn scan_row<T: Send + 'static>(
    ui: &mut Ui,
    scan: &mut Scan<T>,
    label: &str,
    disabled_hint: Option<&str>,
    total: impl Fn(&T) -> String,
) -> bool {
    ui.horizontal(|ui| {
        let running = scan.running();
        let mut button = ui.add_enabled(disabled_hint.is_none() && !running, Button::new(label));

        if let Some(hint) = disabled_hint {
            button = button.on_disabled_hover_text(hint);
        }

        let clicked = button.clicked();

        if running || clicked {
            ui.spinner();
        } else {
            match &scan.result {
                Some(Ok(value)) => {
                    ui.strong(total(value));
                }
                Some(Err(e)) => {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                }
                None => {}
            }
        }

        clicked
    })
    .inner
}

fn steam(scan: &mut Scan<Vec<SteamLibrary>>, home: Option<&Path>, ui: &mut Ui) {
    let total = |libraries: &Vec<SteamLibrary>| short_bytes(libraries.iter().map(|l| l.size).sum());

    if scan_row(ui, scan, "Space used by Steam", None, total) {
        scan.start(space::steam, repaint(ui.ctx()));
    }

    // Per library, when there are several.
    if !scan.running()
        && let Some(Ok(libraries)) = &scan.result
        && libraries.len() > 1
    {
        for library in libraries {
            ui.horizontal(|ui| {
                ui.add_space(ui.spacing().indent);
                ui.label(path_job(ui, &library.path, home));
                ui.weak(short_bytes(library.size));
            });
        }
    }

    ui.add_space(4.);
}

/// The Rust targets row, and its roots: the folders scanned. Returns whether
/// `roots` changed.
fn rust_targets(
    scan: &mut Scan<Vec<RootTargets>>,
    roots: &mut Vec<PathBuf>,
    bookmarks: &[PathBuf],
    home: Option<&Path>,
    ui: &mut Ui,
) -> bool {
    // Results for roots since removed don't count.
    let total = |found: &Vec<RootTargets>| {
        let current: Vec<_> = found.iter().filter(|f| roots.contains(&f.root)).collect();
        let size = current.iter().map(|f| f.size).sum();
        let count: usize = current.iter().map(|f| f.targets.len()).sum();
        let noun = if count == 1 { "folder" } else { "folders" };

        format!("{} · {count} {noun}", short_bytes(size))
    };
    let hint = roots
        .is_empty()
        .then_some("Add a root from bookmarks first");

    if scan_row(ui, scan, "Space used by Rust targets", hint, total) {
        let roots = roots.clone();
        scan.start(move || Ok(space::rust_targets(roots)), repaint(ui.ctx()));
    }

    let found = if scan.running() {
        None
    } else {
        scan.result.as_ref().and_then(|r| r.as_ref().ok())
    };
    let mut remove = None;

    for (i, root) in roots.iter().enumerate() {
        ui.horizontal(|ui| {
            ui.add_space(ui.spacing().indent);

            if ui.small_button("✕").on_hover_text("Remove root").clicked() {
                remove = Some(i);
            }

            ui.label(path_job(ui, root, home));

            if let Some(f) = found.and_then(|found| found.iter().find(|f| f.root == *root)) {
                ui.weak(short_bytes(f.size))
                    .on_hover_ui(|ui| target_list(ui, f));
            }
        });
    }

    let mut add = None;

    ui.horizontal(|ui| {
        ui.add_space(ui.spacing().indent);

        let candidates: Vec<&PathBuf> = bookmarks.iter().filter(|b| !roots.contains(b)).collect();

        ui.add_enabled_ui(!candidates.is_empty(), |ui| {
            ui.menu_button("Add root from bookmarks", |ui| {
                for bookmark in candidates {
                    if ui.button(path_job(ui, bookmark, home)).clicked() {
                        add = Some(bookmark.clone());
                    }
                }
            })
            .response
            .on_hover_text("Searched recursively for cargo target folders")
            .on_disabled_hover_text("No bookmarks to add");
        });
    });

    if let Some(i) = remove {
        roots.remove(i);
    }

    if let Some(root) = &add {
        roots.push(root.clone());
    }

    remove.is_some() || add.is_some()
}

/// A root's biggest target folders, by crate.
fn target_list(ui: &mut Ui, found: &RootTargets) {
    const SHOWN: usize = 12;

    if found.targets.is_empty() {
        ui.weak("No cargo target folders");
        return;
    }

    for target in found.targets.iter().take(SHOWN) {
        let krate = target.path.parent().unwrap_or(&target.path);
        let name = match krate.strip_prefix(&found.root) {
            Ok(rel) if !rel.as_os_str().is_empty() => rel.display().to_string(),
            _ => ".".into(),
        };

        ui.label(format!("{}  {name}", short_bytes(target.size)));
    }

    let more = found.targets.len().saturating_sub(SHOWN);

    if more > 0 {
        ui.weak(format!("{more} more"));
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

/// As [bytes], with a one-letter unit and no decimals from 100 up, e.g. "500 G", "1.8 T".
fn short_bytes(value: u64) -> String {
    let (divisor, precision, suffix) = byte_unit(value);
    let scaled = value as f64 / divisor;
    let precision = if scaled >= 100. { 0 } else { precision };

    format!("{scaled:.precision$} {}", &suffix[..1])
}
