//! On-demand system snapshots. No work runs until the pane opens.

use std::{
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

pub mod space;

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

#[derive(Debug)]
pub struct Snapshot {
    pub hardware: Arc<Hardware>,
    pub cpu_usage: Option<f32>,
    pub cpu_available: bool,
    pub memory: Option<Memory>,
    pub volumes: Arc<Vec<Volume>>,
    pub sampled_at: Instant,
}

struct Worker {
    request: mpsc::Sender<()>,
    response: mpsc::Receiver<Snapshot>,
}

#[derive(Default)]
pub struct Monitor {
    pub snapshot: Option<Snapshot>,
    pub error: Option<String>,
    pub steam: space::Scan<Vec<space::SteamLibrary>>,
    pub rust_targets: space::Scan<Vec<space::RootTargets>>,
    worker: Option<Worker>,
    pending: bool,
    next_sample: Option<Instant>,
}

impl Monitor {
    /// Called only while visible. At most one request is in flight.
    pub fn update(&mut self, wake: impl Fn() + Send + 'static) {
        if let Some(worker) = &self.worker {
            match worker.response.try_recv() {
                Ok(snapshot) => {
                    self.next_sample = Some(snapshot.sampled_at + SAMPLE_INTERVAL);
                    self.snapshot = Some(snapshot);
                    self.pending = false;
                    self.error = None;
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

                    while requests.recv().is_ok() {
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
            self.pending = worker.request.send(()).is_ok();
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
        let cpu_usage = current.and_then(|current| {
            let (previous, at) = self.cpu?;

            if now.duration_since(at) > SAMPLE_INTERVAL * 3 {
                return None;
            }

            let total = current.total.checked_sub(previous.total)?;
            let busy = current.busy.checked_sub(previous.busy)?;

            (total > 0).then(|| (100. * busy as f32 / total as f32).clamp(0., 100.))
        });
        self.cpu = current.map(|times| (times, now));

        Snapshot {
            hardware: Arc::clone(&self.hardware),
            cpu_usage,
            cpu_available: current.is_some(),
            memory: platform::memory(),
            volumes: Arc::clone(&self.volumes),
            sampled_at: Instant::now(),
        }
    }
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
