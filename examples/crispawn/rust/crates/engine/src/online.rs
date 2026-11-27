// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! Bounded feedback during productive copy. No discarded tuning copy or optimality claim.
use crate::{profiles, Result};
use crispawn_core::{error::require, plan::Plan, types::Codec};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Limit read/encode work independently of bounded result prefetch. Waiting
/// workers never hold a permit needed by the coordinator or hash workers.
pub(crate) struct ReadGate {
    state: Mutex<(u32, u32)>,
    wake: Condvar,
    maximum: u32,
}
pub(crate) struct ReadPermit(Arc<ReadGate>);
impl ReadGate {
    pub fn new(limit: u32, maximum: u32) -> Self {
        Self {
            state: Mutex::new((limit, 0)),
            wake: Condvar::new(),
            maximum,
        }
    }
    pub fn set(&self, limit: u32) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).0 = limit.clamp(1, self.maximum);
        self.wake.notify_all();
    }
    pub fn acquire(
        self: &Arc<Self>,
        control: &dyn crate::control::ExecutionControl,
    ) -> Result<ReadPermit> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while state.1 >= state.0 {
            control.check_cancel()?;
            state = self
                .wake
                .wait_timeout(state, std::time::Duration::from_millis(25))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        control.check_cancel()?;
        state.1 += 1;
        Ok(ReadPermit(self.clone()))
    }
}
impl Drop for ReadPermit {
    fn drop(&mut self) {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner()).1 -= 1;
        self.0.wake.notify_one();
    }
}

const MIB: u64 = 1 << 20;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    pub codec: Codec,
    pub level: i32,
}
const PROFILES: [Profile; PROFILE_COUNT] = [
    Profile {
        codec: Codec::Zstd,
        level: 3,
    },
    Profile {
        codec: Codec::Zstd,
        level: 6,
    },
    Profile {
        codec: Codec::Zstd,
        level: 9,
    },
    Profile {
        codec: Codec::Xz,
        level: 0,
    },
    Profile {
        codec: Codec::Xz,
        level: 3,
    },
    Profile {
        codec: Codec::Zstd,
        level: 1,
    },
    Profile {
        codec: Codec::Raw,
        level: 0,
    },
    Profile {
        codec: Codec::Gzip,
        level: 1,
    },
    Profile {
        codec: Codec::Gzip,
        level: 6,
    },
    Profile {
        codec: Codec::Gzip,
        level: 9,
    },
    // Used only while writer backlog holds xz (see `pressure`).
    Profile {
        codec: Codec::Xz,
        level: 6,
    },
];
const PRIOR: [f64; PROFILE_COUNT] = [
    1.0, 1.61, 1.55, 78.71, 120.55, 0.8, 0.02, 4.0, 8.0, 10.0, 150.0,
];
const PROFILE_COUNT: usize = 11;
/// Indices into PROFILES used while the writer is the bottleneck: xz-6, then xz-3.
const HOLD: [usize; 2] = [10, 4];
/// Queue fill (encoded bytes / budget) treated as a writer backlog.
const BACKLOG_FILL: f64 = 0.5;
/// Queue fill below which idle writers mean the encoders cannot keep up.
const STARVED_FILL: f64 = 0.1;
/// Every Nth held frame is also measured with zstd-3, for logs only.
pub const PROBE_EVERY: u64 = 64;
#[derive(Default, Clone, Copy)]
struct Cost {
    count: u64,
    ratio: f64,
    cpu: f64,
}
impl Cost {
    fn add(&mut self, ratio: f64, cpu: f64) {
        let w = if self.count == 0 { 1.0 } else { 0.25 };
        self.ratio += w * (ratio - self.ratio);
        self.cpu += w * (cpu - self.cpu);
        self.count += 1;
    }
}
pub struct Observation {
    pub lane: usize,
    pub requested: Profile,
    pub actual: Codec,
    pub raw: u64,
    pub stored: u64,
    pub encode_cpu: f64,
    pub read_seconds: f64,
    pub read_bytes: u64,
    pub writer_seconds: f64,
    /// zstd-3 stored size of the same raw frame, measured for logs only.
    pub probe_stored: Option<u64>,
}
/// Writer-backlog codec hold. Only newly read frames are affected; frames
/// already encoded or queued are never re-encoded.
pub struct Hold {
    pub level: Option<usize>,
    backlog_since: Option<Instant>,
    starved_since: Option<Instant>,
    pub enter_after: Duration,
    pub release_after: Duration,
}
/// Immutable plan describes ceilings, including the largest permitted encoder.
/// Starting P remains min(2N,8); allocating capacity never makes it the active P.
pub fn reserve(plan: &mut Plan, cpus: u32, verifiers: u32) -> Result<()> {
    require(
        (1..=16).contains(&plan.workers) && cpus > 0,
        "INVALID_RESOURCES",
        "online worker/CPU bounds",
    )?;
    plan.format_version = 3;
    let initial = plan.workers;
    plan.workers = cpus.saturating_mul(2).clamp(initial, 16);
    while plan.workers > initial
        && profiles::memory_bound(plan, verifiers) > plan.memory_limit_bytes
    {
        plan.workers -= 1;
    }
    require(
        profiles::memory_bound(plan, verifiers) <= plan.memory_limit_bytes,
        "MEMORY_BUDGET",
        "online ceiling plus concurrent checking exceeds RAM",
    )
}
pub fn cpu_seconds(clock: libc::clockid_t) -> f64 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes exactly one timespec.
    if unsafe { libc::clock_gettime(clock, &mut t) } == 0 {
        t.tv_sec as f64 + t.tv_nsec as f64 * 1e-9
    } else {
        0.0
    }
}
pub struct Feedback {
    pub active: Plan,
    pub verifiers: u32,
    pub cpu_fraction: f64,
    maximum: u32,
    max_verify: u32,
    cpus: u32,
    cost: [[Cost; PROFILE_COUNT]; 64],
    best: [usize; 64],
    zero: [bool; 64],
    compressible: [bool; 64],
    jobs: [u64; 64],
    pending: [Option<usize>; 64],
    read_cost: [Cost; 3],
    epoch: u64,
    bytes: u64,
    epoch_bytes: u64,
    start: Instant,
    start_cpu: f64,
    previous_rate: f64,
    plateau: u32,
    seconds_per_raw: f64,
    io_per_stored: f64,
    pub hold: Hold,
}
impl Feedback {
    pub fn new(plan: &Plan, verifiers: u32, cpus: u32) -> Self {
        let mut active = plan.clone();
        active.workers = cpus.saturating_mul(2).min(8).min(plan.workers).max(1);
        let best = PROFILES
            .iter()
            .position(|p| p.codec == plan.codec && p.level == plan.compression_level)
            .unwrap_or(0);
        Self {
            active,
            verifiers,
            maximum: plan.workers,
            max_verify: verifiers,
            cpus: cpus.max(1),
            cpu_fraction: 0.0,
            cost: [[Cost::default(); PROFILE_COUNT]; 64],
            best: [best; 64],
            zero: [false; 64],
            compressible: std::array::from_fn(|i| i >= (plan.regions as usize).div_ceil(2)),
            jobs: [0; 64],
            pending: [None; 64],
            read_cost: [Cost::default(); 3],
            epoch: 0,
            bytes: 0,
            epoch_bytes: 0,
            start: Instant::now(),
            start_cpu: cpu_seconds(libc::CLOCK_PROCESS_CPUTIME_ID),
            previous_rate: 0.0,
            plateau: 0,
            seconds_per_raw: 0.0,
            io_per_stored: 1.0 / 100e6 + 1.0 / 500e6,
            hold: Hold {
                level: None,
                backlog_since: None,
                starved_since: None,
                enter_after: Duration::from_secs(30),
                release_after: Duration::from_secs(10),
            },
        }
    }
    /// Current held profile, if the writer backlog keeps xz selected.
    pub fn held(&self) -> Option<Profile> {
        self.hold.level.map(|l| PROFILES[HOLD[l]])
    }
    /// Whether this held frame should also be measured with zstd-3 for logs.
    pub fn probe_due(&self, lane: usize) -> bool {
        self.hold.level.is_some() && self.jobs[lane] % PROBE_EVERY == 0
    }
    /// Update the writer-backlog hold from the encoded queue fill and writer
    /// activity. Enter xz-6 after a sustained backlog with spare CPU; step down
    /// (xz-3, then the adaptive choice) only when writers starve. Returns a
    /// reason with the inputs whenever the mode changes.
    pub fn pressure(&mut self, fill: f64, idle_writers: bool) -> Option<String> {
        let now = Instant::now();
        let backlog = fill >= BACKLOG_FILL && self.cpu_fraction < 0.80;
        let starved = (fill < STARVED_FILL && idle_writers) || self.cpu_fraction >= 0.90;
        let h = &mut self.hold;
        let previous = h.level;
        if backlog && h.level != Some(0) {
            let since = *h.backlog_since.get_or_insert(now);
            if now.duration_since(since) >= h.enter_after {
                h.level = Some(0);
                h.backlog_since = None;
            }
        } else if !backlog {
            h.backlog_since = None;
        }
        if starved && h.level.is_some() {
            let since = *h.starved_since.get_or_insert(now);
            if now.duration_since(since) >= h.release_after {
                h.level = match h.level {
                    Some(0) => Some(1),
                    _ => None,
                };
                h.starved_since = None;
                h.backlog_since = None;
            }
        } else if !starved {
            h.starved_since = None;
        }
        (h.level != previous).then(|| {
            if let Some(p) = self.held() {
                (self.active.codec, self.active.compression_level) = (p.codec, p.level);
            }
            format!(
                "writer_backlog_hold from={} to={} queue_fill={fill:.3} idle_writers={idle_writers} cpu_fraction={:.3}",
                previous.map_or("adaptive".into(), |l| crate::v2_format::profile_name(PROFILES[HOLD[l]].codec, PROFILES[HOLD[l]].level)),
                self.held().map_or("adaptive".into(), |p| crate::v2_format::profile_name(p.codec, p.level)),
                self.cpu_fraction,
            )
        })
    }
    pub fn resumed(&mut self) {
        self.epoch_bytes = 0;
        self.start = Instant::now();
        self.start_cpu = cpu_seconds(libc::CLOCK_PROCESS_CPUTIME_ID);
        self.previous_rate = 0.0;
        self.plateau = 0;
    }
    pub fn next(&mut self, lane: usize) -> (Profile, u64) {
        self.jobs[lane] += 1;
        // A writer backlog with spare CPU keeps xz for new reads, including
        // hard-to-compress data. Zero frames are detected before any encoder.
        if let Some(profile) = self.held().filter(|_| self.cpu_fraction < 0.95) {
            return (profile, self.active.read_bytes.min(self.active.range_bytes));
        }
        let mut chosen = if self.cpu_fraction >= 0.95 {
            5
        } else if self.cpu_fraction >= 0.90 {
            0
        } else {
            self.best[lane]
        };
        if self.jobs[lane] % 32 == 0 {
            chosen = 0;
        } else if !self.zero[lane]
            && self.cpu_fraction < 0.80
            && self.pending[lane].is_none()
            && (self.compressible[lane] || self.jobs[lane] % 32 == 4)
        {
            let base = self.cost[lane][0];
            if base.count >= 2 {
                for i in 1..5 {
                    if self.cost[lane][i].count > 0 && self.jobs[lane] % 64 != 4 {
                        continue;
                    }
                    let floor = base.cpu * PRIOR[i] / f64::from(self.cpus);
                    if self.seconds_per_raw > 0.0 && floor > self.seconds_per_raw * 0.9 {
                        continue;
                    }
                    chosen = i;
                    self.pending[lane] = Some(i);
                    break;
                }
            }
        }
        if self.zero[lane] {
            chosen = 0;
        }
        (
            PROFILES[chosen],
            self.active.read_bytes.min(self.active.range_bytes),
        )
    }
    pub fn observe(&mut self, o: &Observation, verify_backlog: u64) -> Option<String> {
        let i = PROFILES.iter().position(|p| *p == o.requested).unwrap_or(0);
        let ratio = o.stored as f64 / o.raw as f64;
        if o.actual != Codec::Zero {
            self.cost[o.lane][i].add(ratio, o.encode_cpu / o.raw as f64);
        }
        if self.pending[o.lane] == Some(i) {
            self.pending[o.lane] = None;
        }
        self.zero[o.lane] = o.actual == Codec::Zero;
        if !self.zero[o.lane] {
            if ratio < 0.95 {
                self.compressible[o.lane] = true;
            }
            if ratio >= 0.98 && i == self.best[o.lane] && self.hold.level.is_none() {
                self.compressible[o.lane] = false;
                self.best[o.lane] = if i == 6 { 6 } else { 0 };
            }
            if o.stored >= 256 << 10 {
                let measured = o.writer_seconds / o.stored as f64;
                self.io_per_stored = 0.8 * self.io_per_stored + 0.2 * measured.max(1e-10);
            }
        }
        if self.compressible[o.lane] && self.cpu_fraction < 0.90 {
            let score = |c: Cost| (c.cpu / f64::from(self.cpus)).max(c.ratio * self.io_per_stored);
            let old = self.best[o.lane];
            let winner = (0..PROFILES.len())
                .filter(|&n| self.cost[o.lane][n].count > 0)
                .min_by(|&a, &b| {
                    score(self.cost[o.lane][a]).total_cmp(&score(self.cost[o.lane][b]))
                })
                .unwrap_or(old);
            if self.cost[o.lane][old].count == 0
                || score(self.cost[o.lane][winner]) < score(self.cost[o.lane][old]) * 0.92
            {
                self.best[o.lane] = winner;
            }
        }
        if let Some(b) = [256 << 10, MIB, 4 * MIB]
            .iter()
            .position(|v| *v == o.read_bytes)
        {
            self.read_cost[b].add(0.0, o.read_seconds / o.raw as f64);
        }
        self.bytes += o.raw;
        self.epoch_bytes += o.raw;
        let seconds = self.start.elapsed().as_secs_f64();
        if self.epoch_bytes < 64 * MIB || seconds < 0.5 {
            return None;
        }
        let now = cpu_seconds(libc::CLOCK_PROCESS_CPUTIME_ID);
        self.cpu_fraction =
            ((now - self.start_cpu) / seconds / f64::from(self.cpus)).clamp(0.0, 1.0);
        let rate = self.epoch_bytes as f64 / seconds;
        self.seconds_per_raw = 1.0 / rate;
        self.plateau = if self.previous_rate > 0.0 && rate < self.previous_rate * 1.05 {
            self.plateau + 1
        } else {
            0
        };
        if self.cpu_fraction < 0.85 && self.plateau < 2 && self.active.workers < self.maximum {
            self.active.workers += 1;
        } else if self.cpu_fraction > 0.95 && self.active.workers > self.cpus {
            self.active.workers -= 1;
        }
        if verify_backlog > self.active.range_bytes * 4 && self.cpu_fraction < 0.85 {
            self.verifiers = (self.verifiers + 1).min(self.max_verify);
        } else if self.cpu_fraction > 0.95 && self.verifiers > 1 {
            self.verifiers -= 1;
        }
        self.epoch += 1;
        self.active.read_bytes = match self.epoch {
            1 => 4 * MIB,
            2 => 256 << 10,
            n if n % 8 == 0 => [256 << 10, MIB, 4 * MIB][(n as usize / 8) % 3],
            _ => {
                let best = (0..3)
                    .filter(|&b| self.read_cost[b].count >= 2)
                    .min_by(|&a, &b| self.read_cost[a].cpu.total_cmp(&self.read_cost[b].cpu))
                    .unwrap_or(1);
                [256 << 10, MIB, 4 * MIB][best]
            }
        }
        .min(self.active.range_bytes);
        let shown = self.held().unwrap_or(PROFILES[self.best[o.lane]]);
        (self.active.codec, self.active.compression_level) = (shown.codec, shown.level);
        self.epoch_bytes = 0;
        self.start = Instant::now();
        self.start_cpu = now;
        self.previous_rate = rate;
        Some(format!("productive_copy_feedback epoch={} observed_bytes={} cpu_fraction={:.3} raw_bps={:.0} front={} rear={}",self.epoch,self.bytes,self.cpu_fraction,rate,
            crate::v2_format::profile_name(PROFILES[self.best[0]].codec,PROFILES[self.best[0]].level),crate::v2_format::profile_name(PROFILES[self.best[(self.active.regions-1) as usize]].codec,PROFILES[self.best[(self.active.regions-1) as usize]].level)))
    }
    pub fn verification_service(&mut self, stored: u64, seconds: f64) {
        if stored >= MIB && seconds > 0.0 {
            self.io_per_stored =
                (0.8 * self.io_per_stored + 0.2 * (seconds / stored as f64)).max(1e-10);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancelled_workers_leave_the_concurrency_gate_without_waiting_for_a_permit() {
        let gate = Arc::new(ReadGate::new(1, 2));
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let _held = gate.acquire(&cancel).unwrap();
        std::thread::scope(|scope| {
            let gate = gate.clone();
            let cancel = &cancel;
            let waiting = scope.spawn(move || gate.acquire(cancel).map(|_| ()).unwrap_err().code);
            std::thread::sleep(std::time::Duration::from_millis(50));
            cancel.store(true, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(waiting.join().unwrap(), "CANCELLED");
        });
    }
    #[test]
    fn initial_profiles_and_ceiling_are_separate_and_fit_worst_codec() {
        for cpus in [2, 4, 8] {
            for memory in [4u64, 8, 16] {
                let mut template = Plan::current();
                template.memory_limit_bytes = memory << 30;
                let (mut plan, v) = profiles::select(
                    &template,
                    profiles::Profile::Balanced,
                    cpus,
                    memory << 30,
                    1 << 40,
                )
                .unwrap();
                reserve(&mut plan, cpus, v).unwrap();
                let feedback = Feedback::new(&plan, v, cpus);
                assert_eq!(feedback.active.workers, (2 * cpus).min(8));
                assert!(profiles::memory_bound(&plan, v) <= memory << 30);
            }
        }
    }
    #[test]
    fn zeros_never_request_expensive_encoder_and_cpu_pressure_falls_back() {
        let mut p = Plan::current();
        p.format_version = 3;
        p.memory_limit_bytes = 4 << 30;
        let mut f = Feedback::new(&p, 1, 2);
        f.zero[1] = true;
        f.best[1] = 4;
        assert_eq!(f.next(1).0, PROFILES[0]);
        f.cpu_fraction = 0.99;
        f.best[0] = 2;
        assert_eq!(f.next(0).0, PROFILES[5]);
    }
    #[test]
    fn explicitly_selected_starting_codec_is_respected_in_productive_frames() {
        for (codec, level) in [
            (Codec::Raw, 0),
            (Codec::Gzip, 1),
            (Codec::Gzip, 6),
            (Codec::Gzip, 9),
            (Codec::Zstd, 6),
            (Codec::Zstd, 9),
        ] {
            let mut p = Plan::current();
            p.format_version = 3;
            p.codec = codec;
            p.compression_level = level;
            p.memory_limit_bytes = 4 << 30;
            let mut f = Feedback::new(&p, 1, 2);
            assert_eq!(f.next(0).0, Profile { codec, level });
        }
    }
    #[test]
    fn zero_rear_region_does_not_suppress_a_compressible_front_probe() {
        let mut p = Plan::current();
        p.format_version = 3;
        p.memory_limit_bytes = 4 << 30;
        let mut f = Feedback::new(&p, 4, 8);
        f.zero[7] = true;
        f.compressible[0] = true;
        f.seconds_per_raw = 1.0 / 64e6;
        f.io_per_stored = 1.0 / 8e6;
        for _ in 0..2 {
            f.cost[0][0].add(0.8, 1e-9);
        }
        let (profile, read_bytes) = f.next(0);
        assert_eq!(profile, PROFILES[1]);
        let mut observation = Observation {
            lane: 0,
            requested: profile,
            actual: profile.codec,
            raw: 16 * MIB,
            stored: 4 * MIB,
            encode_cpu: 0.03,
            read_seconds: 0.25,
            read_bytes,
            writer_seconds: 0.5,
            probe_stored: None,
        };
        f.observe(&observation, 0);
        assert_eq!(
            f.best[0], 1,
            "a measured I/O saving must outweigh the extra encoder cost"
        );
        observation.actual = Codec::Raw;
        observation.stored = observation.raw;
        f.observe(&observation, 0);
        assert_eq!(f.best[0], 0, "incompressible data returns to the baseline");
        assert!(f.zero[7]);
    }
    #[test]
    fn sustained_writer_backlog_holds_xz_and_starved_writers_step_down() {
        let mut p = Plan::current();
        p.format_version = 3;
        p.memory_limit_bytes = 4 << 30;
        let mut f = Feedback::new(&p, 2, 16);
        f.hold.enter_after = Duration::from_millis(30);
        f.hold.release_after = Duration::from_millis(30);
        f.cpu_fraction = 0.05;
        assert!(
            f.pressure(0.9, false).is_none(),
            "a short backlog is not a hold"
        );
        std::thread::sleep(Duration::from_millis(40));
        let reason = f.pressure(0.9, false).unwrap();
        assert!(reason.contains("to=xz:6"), "{reason}");
        // Hard-to-compress data and zero lanes still use the held encoder.
        f.zero[3] = true;
        assert_eq!(
            f.next(3).0,
            Profile {
                codec: Codec::Xz,
                level: 6
            }
        );
        let held = Observation {
            lane: 0,
            requested: f.next(0).0,
            actual: Codec::Xz,
            raw: 16 * MIB,
            stored: 16 * MIB,
            encode_cpu: 3.0,
            read_seconds: 0.1,
            read_bytes: MIB,
            writer_seconds: 6.0,
            probe_stored: None,
        };
        f.observe(&held, 0);
        assert_eq!(
            f.next(0).0,
            Profile {
                codec: Codec::Xz,
                level: 6
            }
        );
        // A brief dip in the queue is not starvation.
        assert!(f.pressure(0.05, false).is_none());
        assert!(f.pressure(0.05, true).is_none());
        std::thread::sleep(Duration::from_millis(40));
        assert!(f.pressure(0.05, true).unwrap().contains("to=xz:3"));
        std::thread::sleep(Duration::from_millis(40));
        assert!(f.pressure(0.05, true).is_none());
        std::thread::sleep(Duration::from_millis(40));
        assert!(f.pressure(0.05, true).unwrap().contains("to=adaptive"));
        assert_ne!(f.next(0).0.level, 6);
        // CPU pressure never enters a hold and releases an existing one.
        f.cpu_fraction = 0.85;
        assert!(f.pressure(0.9, false).is_none());
        std::thread::sleep(Duration::from_millis(40));
        assert!(f.pressure(0.9, false).is_none());
        assert!(f.held().is_none());
    }
    #[test]
    fn held_frames_are_probed_for_logs_at_a_fixed_interval() {
        let mut p = Plan::current();
        p.format_version = 3;
        p.memory_limit_bytes = 4 << 30;
        let mut f = Feedback::new(&p, 2, 16);
        assert!(!f.probe_due(0));
        f.hold.level = Some(0);
        let probes = (0..PROBE_EVERY * 2)
            .filter(|_| {
                f.next(0);
                f.probe_due(0)
            })
            .count();
        assert_eq!(probes, 2);
    }
}
