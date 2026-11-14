//! On-demand system snapshots. No work runs until the pane opens.

use std::{
    mem,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

#[cfg(target_os = "linux")]
#[path = "system_status/linux.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "system_status/macos.rs"]
mod platform;
#[cfg(windows)]
#[path = "system_status/windows.rs"]
mod platform;
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
#[path = "system_status/unsupported.rs"]
mod platform;

pub mod processes;
pub mod space;

/// What a process's memory figure means here.
pub use platform::PROCESS_MEMORY;
use processes::{HALF_HOUR, MINUTE, Recent, Usage};

pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
const STORAGE_INTERVAL: Duration = Duration::from_secs(15);
const HARDWARE_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Default, Debug)]
pub struct Hardware {
    pub cpu: String,
    pub physical_cores: Option<usize>,
    pub logical_cores: Option<usize>,
    pub simd: String,
    pub installed_memory: Option<u64>,
    pub gpus: Vec<Gpu>,
    pub disks: Vec<Disk>,
    pub notes: Vec<String>,
}

#[derive(Debug)]
pub struct Gpu {
    pub name: String,
    pub driver: Option<String>,
    pub status: String,
    pub driver_available: Option<bool>,
    pub memory: Option<u64>,
}

#[derive(Debug)]
pub struct Disk {
    pub name: String,
    pub capacity: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct Volume {
    pub name: String,
    pub total: u64,
    pub available: u64,
}

#[derive(Debug)]
pub struct Memory {
    pub total: u64,
    pub available: u64,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct CpuTimes {
    pub busy: u64,
    pub total: u64,
}

/// The whole machine's use, averaged over the last minute and half hour of
/// recording. None before there's any.
#[derive(Clone, Copy, Debug, Default)]
pub struct Averages {
    pub minute: Option<f32>,
    pub half_hour: Option<f32>,
}

/// The history behind an [Averages].
struct Recents {
    minute: Recent,
    half_hour: Recent,
}

impl Default for Recents {
    fn default() -> Self {
        Self {
            minute: Recent::new(&MINUTE),
            half_hour: Recent::new(&HALF_HOUR),
        }
    }
}

impl Recents {
    fn add(&mut self, usage: Usage) {
        self.minute.add(usage);
        self.half_hour.add(usage);
    }

    fn averages(&self, figure: impl Fn(&Usage) -> Option<f32>) -> Averages {
        Averages {
            minute: figure(&self.minute.total()),
            half_hour: figure(&self.half_hour.total()),
        }
    }
}

#[derive(Debug)]
pub struct Snapshot {
    pub hardware: Arc<Hardware>,
    pub cpu_usage: Option<f32>,
    /// Percent, as [Self::cpu_usage].
    pub cpu_averages: Averages,
    pub cpu_available: bool,
    pub memory: Option<Memory>,
    /// Bytes used.
    pub memory_averages: Averages,
    pub processes: Option<processes::Processes>,
    pub volumes: Arc<Vec<Volume>>,
    pub sampled_at: Instant,
}

struct Worker {
    /// Asks for a sample. True clears stopped programs first.
    request: mpsc::Sender<bool>,
    response: mpsc::Receiver<Snapshot>,
}

#[derive(Default)]
pub struct Monitor {
    pub snapshot: Option<Snapshot>,
    pub error: Option<String>,
    pub steam: space::Scan<Vec<space::SteamLibrary>>,
    pub rust_targets: space::Scan<Vec<space::RootTargets>>,
    pub cpu_processes: processes::Table,
    pub memory_processes: processes::Table,
    worker: Option<Worker>,
    pending: bool,
    /// A clear of stopped programs not yet sent to the sampler.
    clear_stopped: bool,
    next_sample: Option<Instant>,
}

impl Monitor {
    /// Forgets the programs not running now, from both process lists.
    pub fn clear_stopped(&mut self) {
        self.clear_stopped = true;
        self.hide_stopped();
        self.cpu_processes.invalidate();
        self.memory_processes.invalidate();
    }

    /// Hides from the snapshot what the sampler's yet to forget.
    fn hide_stopped(&mut self) {
        if let Some(processes) = self.snapshot.as_mut().and_then(|s| s.processes.as_mut()) {
            processes.rows.retain(|row| row.instances > 0);
        }
    }

    /// Called only while visible. At most one request is in flight.
    pub fn update(&mut self, wake: impl Fn() + Send + 'static) {
        if let Some(worker) = &self.worker {
            match worker.response.try_recv() {
                Ok(snapshot) => {
                    self.next_sample = Some(snapshot.sampled_at + SAMPLE_INTERVAL);
                    self.snapshot = Some(snapshot);
                    self.pending = false;
                    self.error = None;

                    if self.clear_stopped {
                        self.hide_stopped();
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.worker = None;
                    self.pending = false;
                    self.error = Some("System sampler stopped".into());
                    self.next_sample = Some(Instant::now() + HARDWARE_INTERVAL);
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }

        if self.pending || self.next_sample.is_some_and(|next| Instant::now() < next) {
            return;
        }

        if self.worker.is_none() {
            let (request, requests) = mpsc::channel();
            let (responses, response) = mpsc::sync_channel(1);
            let result = thread::Builder::new()
                .name("system-status".into())
                .spawn(move || {
                    let mut sampler = Sampler::default();

                    while let Ok(clear_stopped) = requests.recv() {
                        if clear_stopped {
                            sampler.process_tracker.clear_stopped();
                        }

                        if responses.send(sampler.sample()).is_err() {
                            break;
                        }

                        wake();
                    }
                });

            if let Err(error) = result {
                self.error = Some(format!("Could not start system sampler: {error}"));
                self.next_sample = Some(Instant::now() + HARDWARE_INTERVAL);

                return;
            }

            self.worker = Some(Worker { request, response });
        }

        if let Some(worker) = &self.worker {
            let clear_stopped = mem::take(&mut self.clear_stopped);
            self.pending = worker.request.send(clear_stopped).is_ok();
        }
    }

    pub fn next_update(&self) -> Option<Duration> {
        if self.pending {
            return None;
        }

        self.next_sample
            .map(|next| next.saturating_duration_since(Instant::now()))
    }
}

#[derive(Default)]
struct Sampler {
    hardware: Arc<Hardware>,
    volumes: Arc<Vec<Volume>>,
    hardware_at: Option<Instant>,
    storage_at: Option<Instant>,
    cpu: Option<(CpuTimes, Instant)>,
    cpu_recent: Recents,
    memory_at: Option<Instant>,
    memory_recent: Recents,
    process_reader: platform::ProcessReader,
    process_tracker: processes::Tracker,
}

impl Sampler {
    fn sample(&mut self) -> Snapshot {
        let now = Instant::now();

        if self
            .hardware_at
            .is_none_or(|at| now.duration_since(at) >= HARDWARE_INTERVAL)
        {
            let mut hardware = platform::hardware();
            hardware.simd = simd_summary();
            self.hardware = Arc::new(hardware);
            self.hardware_at = Some(now);
        }

        if self
            .storage_at
            .is_none_or(|at| now.duration_since(at) >= STORAGE_INTERVAL)
        {
            self.volumes = Arc::new(platform::volumes());
            self.storage_at = Some(now);
        }

        let current = platform::cpu_times();
        let cpu = current.and_then(|current| {
            let (previous, at) = self.cpu?;
            let seconds = recording_seconds(at, now)?;
            let total = current.total.checked_sub(previous.total)?;
            let busy = current.busy.checked_sub(previous.busy)?;

            (total > 0).then(|| ((100. * busy as f32 / total as f32).clamp(0., 100.), seconds))
        });
        self.cpu = current.map(|times| (times, now));

        if let Some((usage, seconds)) = cpu {
            // As though of one CPU, so it averages to the meter's percent.
            self.cpu_recent.add(Usage {
                seconds,
                cpu_seconds: seconds * f64::from(usage) / 100.,
                ..Default::default()
            });
        }

        let memory = platform::memory();

        if let Some(memory) = &memory
            && let Some(seconds) = self.memory_at.and_then(|at| recording_seconds(at, now))
        {
            let used = memory.total.saturating_sub(memory.available);

            self.memory_recent.add(Usage {
                seconds,
                byte_seconds: used as f64 * seconds,
                ..Default::default()
            });
        }

        self.memory_at = memory.is_some().then_some(now);

        let cpus = self
            .hardware
            .logical_cores
            .or_else(|| {
                thread::available_parallelism()
                    .ok()
                    .map(|cores| cores.get())
            })
            .unwrap_or(1);
        let processes = self
            .process_reader
            .read()
            .map(|readings| self.process_tracker.record(&readings, Instant::now(), cpus));

        Snapshot {
            hardware: Arc::clone(&self.hardware),
            cpu_usage: cpu.map(|(usage, _)| usage),
            cpu_averages: self.cpu_recent.averages(|usage| usage.cpu_percent(1)),
            cpu_available: current.is_some(),
            memory,
            memory_averages: self.memory_recent.averages(Usage::memory),
            processes,
            volumes: Arc::clone(&self.volumes),
            sampled_at: Instant::now(),
        }
    }
}

/// Seconds from `last` to `now`, or None after a gap too long to have been
/// recording through, as when the pane was closed.
fn recording_seconds(last: Instant, now: Instant) -> Option<f64> {
    let elapsed = now.saturating_duration_since(last);

    (!elapsed.is_zero() && elapsed <= SAMPLE_INTERVAL * 3).then_some(elapsed.as_secs_f64())
}

fn simd_summary() -> String {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        for (available, name) in [
            (std::is_x86_feature_detected!("avx512f"), "AVX-512"),
            (std::is_x86_feature_detected!("avx2"), "AVX2"),
            (std::is_x86_feature_detected!("avx"), "AVX"),
            (std::is_x86_feature_detected!("sse4.2"), "SSE4.2"),
            (std::is_x86_feature_detected!("sse4.1"), "SSE4.1"),
            (std::is_x86_feature_detected!("ssse3"), "SSSE3"),
            (std::is_x86_feature_detected!("sse3"), "SSE3"),
            (std::is_x86_feature_detected!("sse2"), "SSE2"),
            (std::is_x86_feature_detected!("sse"), "SSE"),
        ] {
            if available {
                return name.into();
            }
        }
    }

    #[cfg(target_arch = "aarch64")]
    {
        let mut names = Vec::new();

        if std::arch::is_aarch64_feature_detected!("neon") {
            names.push("NEON");
        }

        if std::arch::is_aarch64_feature_detected!("sve2") {
            names.push("SVE2");
        } else if std::arch::is_aarch64_feature_detected!("sve") {
            names.push("SVE");
        }

        if !names.is_empty() {
            return names.join(" · ");
        }
    }

    #[cfg(all(target_arch = "arm", target_os = "linux"))]
    {
        // SAFETY: reads this process's hardware capabilities; HWCAP_NEON is bit 12.
        if unsafe { libc::getauxval(libc::AT_HWCAP) } & (1 << 12) != 0 {
            return "NEON".into();
        }
    }

    "Unavailable".into()
}
