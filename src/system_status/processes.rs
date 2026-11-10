//! Which programs are using the CPU and memory: right now, and on average.
//!
//! Each system sample reads every process once, through the platform's
//! `ProcessReader`, and [Tracker] adds that to each program's history on the
//! sampler's thread. That happens whether or not the lists are expanded, so
//! there's a history to show when they are.
//!
//! Processes are grouped by name: a program's helpers make up one row, and it
//! keeps its history when it's closed and reopened. A row's averages count
//! only the time it had at least one process running. Its clock stops when the
//! last one ends, so closing a program leaves its figures as they were, and
//! its "last minute" is the last minute it ran.

use std::{
    cmp::Ordering,
    collections::HashMap,
    mem,
    sync::Arc,
    time::{Duration, Instant},
};

use super::SAMPLE_INTERVAL;

/// A stretch of a program's running time to average over.
struct Window {
    seconds: f64,
    /// How much running time each bucket of its [Recent] holds.
    bucket_seconds: f64,
}

impl Window {
    /// One more than fill the window, for the bucket still filling.
    const fn buckets(&self) -> usize {
        (self.seconds / self.bucket_seconds) as usize + 1
    }
}

const MINUTE: Window = Window {
    seconds: 60.,
    bucket_seconds: SAMPLE_INTERVAL.as_secs_f64(),
};

const HALF_HOUR: Window = Window {
    seconds: 30. * 60.,
    bucket_seconds: 30.,
};

/// One process, as read from the OS at one sample.
pub(super) struct Reading {
    pub pid: u32,
    /// When it started, in any unit: tells a reused pid from its last owner.
    pub started: u64,
    pub name: String,
    /// CPU time it has used since it started, over all its threads.
    pub cpu_ns: u64,
    /// Memory that's its alone, in bytes. Counting pages shared between a
    /// program's processes would count them once for each.
    pub memory: u64,
}

/// A column of the process lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Period {
    Now,
    All,
    HalfHour,
    Minute,
}

impl Period {
    pub const ALL: [Self; 4] = [Self::Now, Self::All, Self::HalfHour, Self::Minute];
}

/// One row's figures by [Period]: for CPU, percent of the whole machine, as
/// the CPU meter shows; for memory, bytes. None where nothing's been measured:
/// "now" for a program that isn't running, or averages before a second sample.
pub type Figures = [Option<f32>; Period::ALL.len()];

/// One program, as of the latest sample.
#[derive(Debug)]
pub struct Row {
    pub name: Arc<str>,
    lowercase: Arc<str>,
    /// How many of its processes are running now.
    pub instances: u32,
    /// All the time it's been recorded running.
    pub running: Duration,
    pub cpu: Figures,
    pub memory: Figures,
}

#[derive(Debug)]
pub struct Processes {
    /// By name.
    pub rows: Vec<Row>,
    /// How long it's been recording, not counting time the panel was closed.
    pub recorded: Duration,
}

/// What a stretch of running time added up to.
#[derive(Clone, Copy, Default)]
struct Usage {
    seconds: f64,
    cpu_seconds: f64,
    byte_seconds: f64,
}

impl Usage {
    fn add(&mut self, other: Self) {
        self.seconds += other.seconds;
        self.cpu_seconds += other.cpu_seconds;
        self.byte_seconds += other.byte_seconds;
    }

    fn cpu_percent(&self, cpus: usize) -> Option<f32> {
        (self.seconds > 0.).then(|| (100. * self.cpu_seconds / (self.seconds * cpus as f64)) as f32)
    }

    fn memory(&self) -> Option<f32> {
        (self.seconds > 0.).then(|| (self.byte_seconds / self.seconds) as f32)
    }
}

/// A program's running time over the last [Window], in a fixed number of
/// buckets, so a long window costs no more per sample than a short one.
struct Recent {
    window: &'static Window,
    buckets: Box<[Usage]>,
    newest: usize,
}

impl Recent {
    fn new(window: &'static Window) -> Self {
        Self {
            window,
            buckets: vec![Usage::default(); window.buckets()].into(),
            newest: 0,
        }
    }

    fn add(&mut self, usage: Usage) {
        if self.buckets[self.newest].seconds >= self.window.bucket_seconds {
            self.newest = (self.newest + 1) % self.buckets.len();
            self.buckets[self.newest] = Usage::default();
        }

        self.buckets[self.newest].add(usage);
    }

    /// Stops at the window's length, as slow samples can overfill buckets.
    fn total(&self) -> Usage {
        let count = self.buckets.len();
        let mut total = Usage::default();

        for age in 0..count {
            total.add(self.buckets[(self.newest + count - age) % count]);

            if total.seconds >= self.window.seconds {
                break;
            }
        }

        total
    }
}

/// A program's processes, added up at the current sample.
#[derive(Clone, Copy, Default)]
struct Tick {
    instances: u32,
    cpu_ns: u64,
    memory: u64,
}

/// Everything recorded for one program.
struct History {
    lowercase: Arc<str>,
    tick: Tick,
    all: Usage,
    half_hour: Recent,
    minute: Recent,
}

impl History {
    fn new(name: &str) -> Self {
        Self {
            lowercase: name.to_lowercase().into(),
            tick: Tick::default(),
            all: Usage::default(),
            half_hour: Recent::new(&HALF_HOUR),
            minute: Recent::new(&MINUTE),
        }
    }
}

#[derive(Default)]
pub(super) struct Tracker {
    histories: HashMap<Arc<str>, History>,
    /// Each process's total CPU time at the last sample.
    cpu_times: HashMap<(u32, u64), u64>,
    /// The next sample's [Self::cpu_times], kept to reuse its allocation.
    next_cpu_times: HashMap<(u32, u64), u64>,
    last: Option<Instant>,
    recorded: Duration,
}

impl Tracker {
    /// Adds the sample `readings`, taken at `at`, on a machine of `cpus`
    /// logical processors.
    pub(super) fn record(&mut self, readings: &[Reading], at: Instant, cpus: usize) -> Processes {
        // After a gap (the panel was closed, say), nothing was recording, so
        // the sample only sets a new baseline.
        let elapsed = self
            .last
            .map(|last| at.saturating_duration_since(last))
            .filter(|elapsed| !elapsed.is_zero() && *elapsed <= SAMPLE_INTERVAL * 3);
        self.last = Some(at);

        for history in self.histories.values_mut() {
            history.tick = Tick::default();
        }

        for reading in readings {
            let name = reading.name.as_str();

            if !self.histories.contains_key(name) {
                self.histories.insert(name.into(), History::new(name));
            }

            let Some(history) = self.histories.get_mut(name) else {
                continue;
            };
            let key = (reading.pid, reading.started);
            // One not seen before started since the last sample, so all the
            // CPU time it's used falls in this interval.
            let before = self.cpu_times.get(&key).copied().unwrap_or(0);
            self.next_cpu_times.insert(key, reading.cpu_ns);

            let tick = &mut history.tick;
            tick.instances += 1;
            tick.cpu_ns = tick
                .cpu_ns
                .saturating_add(reading.cpu_ns.saturating_sub(before));
            tick.memory = tick.memory.saturating_add(reading.memory);
        }

        mem::swap(&mut self.cpu_times, &mut self.next_cpu_times);
        self.next_cpu_times.clear();

        let seconds = elapsed.map_or(0., |elapsed| elapsed.as_secs_f64());
        self.recorded += elapsed.unwrap_or_default();

        let mut rows = Vec::with_capacity(self.histories.len());

        for (name, history) in &mut self.histories {
            let tick = history.tick;
            let running = tick.instances > 0;
            let now = Usage {
                seconds: if running { seconds } else { 0. },
                cpu_seconds: (tick.cpu_ns as f64 / 1e9).min(seconds * cpus as f64),
                byte_seconds: tick.memory as f64 * seconds,
            };

            // Adding nothing could still roll a [Recent] on, and lose data.
            if now.seconds > 0. {
                history.all.add(now);
                history.half_hour.add(now);
                history.minute.add(now);
            }

            // Seen only in a baseline, then gone: there's nothing to show.
            if !running && history.all.seconds == 0. {
                continue;
            }

            let half_hour = history.half_hour.total();
            let minute = history.minute.total();

            rows.push(Row {
                name: Arc::clone(name),
                lowercase: Arc::clone(&history.lowercase),
                instances: tick.instances,
                running: Duration::from_secs_f64(history.all.seconds),
                cpu: [
                    now.cpu_percent(cpus),
                    history.all.cpu_percent(cpus),
                    half_hour.cpu_percent(cpus),
                    minute.cpu_percent(cpus),
                ],
                memory: [
                    running.then_some(tick.memory as f32),
                    history.all.memory(),
                    half_hour.memory(),
                    minute.memory(),
                ],
            });
        }

        rows.sort_unstable_by(|a, b| {
            a.lowercase
                .cmp(&b.lowercase)
                .then_with(|| a.name.cmp(&b.name))
        });

        Processes {
            rows,
            recorded: self.recorded,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sort {
    Name,
    /// Highest first.
    Usage(Period),
}

impl Default for Sort {
    fn default() -> Self {
        Self::Usage(Period::All)
    }
}

/// What one Processes list is showing.
#[derive(Default)]
pub struct Table {
    pub search: String,
    pub sort: Sort,
    /// Indexes of the rows to show, in order. Worked out when the rows, the
    /// search, or the sort change, not on every frame.
    pub shown: Vec<usize>,
    shown_for: Option<Instant>,
}

impl Table {
    /// Call when the search or sort changes.
    pub fn invalidate(&mut self) {
        self.shown_for = None;
    }

    /// Brings [Self::shown] up to date for `rows`, of the sample taken at
    /// `sampled_at`, ordering by the column `figures` gets from a row.
    pub fn refresh(
        &mut self,
        rows: &[Row],
        figures: impl Fn(&Row) -> &Figures,
        sampled_at: Instant,
    ) {
        if self.shown_for == Some(sampled_at) {
            return;
        }

        self.shown_for = Some(sampled_at);

        let needle = self.search.to_lowercase();
        self.shown.clear();
        self.shown
            .extend((0..rows.len()).filter(|&i| rows[i].lowercase.contains(&needle)));

        // Stable, and the rows come by name, so ties stay in name order.
        if let Sort::Usage(period) = self.sort {
            self.shown.sort_by(|&a, &b| {
                match (
                    figures(&rows[a])[period as usize],
                    figures(&rows[b])[period as usize],
                ) {
                    (Some(a), Some(b)) => b.total_cmp(&a),
                    (Some(_), None) => Ordering::Less,
                    (None, Some(_)) => Ordering::Greater,
                    (None, None) => Ordering::Equal,
                }
            });
        }
    }
}
