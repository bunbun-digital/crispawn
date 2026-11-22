// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! Experimental online controller. Private study records, never product v2.
use super::*;
use std::sync::atomic::AtomicBool;

#[derive(Clone, Copy, Debug, Serialize, PartialEq)]
pub struct Profile {
    pub codec: Codec,
    pub level: i32,
}
const PROFILES: [Profile; 5] = [
    Profile {
        codec: Codec::Zstd,
        level: 3,
    },
    Profile {
        codec: Codec::Zstd,
        level: 9,
    },
    Profile {
        codec: Codec::Zstd,
        level: 6,
    },
    Profile {
        codec: Codec::Xz,
        level: 0,
    },
    Profile {
        codec: Codec::Xz,
        level: 3,
    },
];

#[derive(Clone, Debug, Serialize)]
pub struct Settings {
    pub epoch: u64,
    pub read_bytes: u64,
    pub workers: u32,
    pub verify_workers: u32,
}
#[derive(Clone, Copy, Default)]
struct CostEstimate {
    count: u64,
    ratio: f64,
    encoder: f64,
}
impl CostEstimate {
    fn add(&mut self, ratio: f64, encoder: f64) {
        let weight = if self.count == 0 { 1.0 } else { 0.35 };
        self.ratio += weight * (ratio - self.ratio);
        self.encoder += weight * (encoder - self.encoder);
        self.count += 1;
    }
}
pub struct FrameObservation {
    pub lane: usize,
    pub requested: Profile,
    pub actual: Codec,
    pub raw: u64,
    pub stored: u64,
    pub encoder_cpu: f64,
    pub read_seconds: f64,
    pub read_bytes: u64,
    pub writer_seconds: f64,
}
pub struct Controller {
    pub settings: Settings,
    pub max_workers: u32,
    resources: Resources,
    costs: [[CostEstimate; 5]; 2],
    best: [usize; 2],
    compressible: [bool; 2],
    zero: [bool; 2],
    pending_probe: [Option<usize>; 2],
    lane_jobs: [u64; 2],
    read_cost: [CostEstimate; 3],
    total_raw: u64,
    epoch_raw: u64,
    epoch_start: Instant,
    epoch_cpu: f64,
    previous_rate: f64,
    plateau: u32,
    io_per_stored_byte: f64,
    writer_total_seconds: f64,
    writer_total_bytes: u64,
    cpu_fraction: f64,
    // Relative encoder+hash CPU costs from the previous mixed-data study.
    // These are exploration priors, not new per-device probe measurements.
    prior: [f64; 5],
    last_seconds_per_raw: f64,
    pub changes: Vec<serde_json::Value>,
}
impl Controller {
    pub fn new(resources: &Resources) -> Self {
        let mut value = Self {
            settings: Settings {
                epoch: 0,
                read_bytes: MIB,
                workers: 1,
                verify_workers: 1,
            },
            max_workers: 32,
            resources: resources.clone(),
            costs: [[CostEstimate::default(); 5]; 2],
            best: [0, 0],
            compressible: [false, true],
            zero: [false, false],
            pending_probe: [None, None],
            lane_jobs: [0, 0],
            read_cost: [CostEstimate::default(); 3],
            total_raw: 0,
            epoch_raw: 0,
            epoch_start: Instant::now(),
            epoch_cpu: cpu_seconds(),
            previous_rate: 0.0,
            plateau: 0,
            io_per_stored_byte: 1.0 / 100e6 + 1.0 / 500e6,
            writer_total_seconds: 0.0,
            writer_total_bytes: 0,
            cpu_fraction: 0.0,
            prior: [1.0, 1.55, 1.61, 78.71, 120.55],
            last_seconds_per_raw: 0.0,
            changes: vec![],
        };
        while !value.fits(value.max_workers, resources.cpus, PROFILES[0]) {
            value.max_workers -= 1;
            assert!(value.max_workers > 0);
        }
        value
    }
    fn fits(&self, p: u32, v: u32, profile: Profile) -> bool {
        let plan = crispawn_core::plan::Plan {
            format_version: 2,
            range_bytes: 16 * MIB,
            read_bytes: MIB,
            workers: p,
            codec: profile.codec,
            compression_level: profile.level,
            memory_limit_bytes: self.resources.memory_budget(),
            ..crispawn_core::plan::Plan::current()
        };
        plan.memory_bound() + u64::from(v) * (160 * MIB) <= self.resources.memory_budget()
    }
    pub fn next_profile(&mut self, lane: usize) -> (Profile, Settings) {
        self.lane_jobs[lane] += 1;
        let mut chosen = if self.cpu_fraction >= 0.85
            || !self.fits(
                self.settings.workers,
                self.settings.verify_workers,
                PROFILES[self.best[lane]],
            ) {
            0
        } else {
            self.best[lane]
        };
        // Continue observing the baseline on real, not discarded, source data.
        if self.lane_jobs[lane].is_multiple_of(32) {
            chosen = 0;
        } else if !self.zero[lane]
            && self.cpu_fraction < 0.80
            && self.pending_probe[lane].is_none()
            && (self.compressible[lane]
                || lane == 1
                || self.lane_jobs[lane] <= 4
                || self.lane_jobs[lane] % 16 == 4)
        {
            for index in 1..PROFILES.len() {
                let periodic_strong = index == 1 && self.lane_jobs[lane] % 16 == 4;
                if (self.costs[lane][index].count > 0 && !periodic_strong)
                    || !self.fits(
                        self.settings.workers,
                        self.settings.verify_workers,
                        PROFILES[index],
                    )
                {
                    continue;
                }
                let base = self.costs[lane][0];
                if base.count == 0 {
                    break;
                }
                // A large predicted CPU cost cannot be rescued by saturating
                // CPU utilization. Skip it when even zero stored bytes lose.
                let cpu_floor = base.encoder * self.prior[index] / f64::from(self.resources.cpus);
                if self.last_seconds_per_raw > 0.0 && cpu_floor > self.last_seconds_per_raw * 0.95 {
                    continue;
                }
                chosen = index;
                self.pending_probe[lane] = Some(index);
                break;
            }
        }
        (PROFILES[chosen], self.settings.clone())
    }
    pub fn observe(&mut self, observation: &FrameObservation) {
        let index = PROFILES
            .iter()
            .position(|p| *p == observation.requested)
            .unwrap();
        let lane = observation.lane;
        let ratio = observation.stored as f64 / observation.raw as f64;
        // Zero detection bypasses the encoder. Its tiny cost cannot predict
        // XZ/Zstd work when the rear cursor later enters actual data.
        if observation.actual != Codec::Zero {
            self.costs[lane][index].add(ratio, observation.encoder_cpu / observation.raw as f64);
        }
        if self.pending_probe[lane] == Some(index) {
            self.pending_probe[lane] = None;
        }
        self.zero[lane] = observation.actual == Codec::Zero;
        if !self.zero[lane] {
            if ratio < 0.95 {
                self.compressible[lane] = true;
            }
            // A cheap codec may miss long-distance repetition which the
            // selected stronger codec handles. Only that selected codec's
            // failure disproves its current compression opportunity.
            if ratio >= 0.98 && index == self.best[lane] {
                self.compressible[lane] = false;
                self.best[lane] = 0;
            }
        }
        let score = |cost: &CostEstimate| {
            (cost.encoder / f64::from(self.resources.cpus))
                .max(cost.ratio * self.io_per_stored_byte)
        };
        if self.compressible[lane] && !self.zero[lane] && self.cpu_fraction < 0.85 {
            self.best[lane] = (0..PROFILES.len())
                .filter(|&i| {
                    self.costs[lane][i].count > 0
                        && self.fits(
                            self.settings.workers,
                            self.settings.verify_workers,
                            PROFILES[i],
                        )
                })
                .min_by(|&a, &b| {
                    score(&self.costs[lane][a]).total_cmp(&score(&self.costs[lane][b]))
                })
                .unwrap_or(0);
        }
        self.writer_total_seconds += observation.writer_seconds;
        self.writer_total_bytes += observation.stored;
        if self.writer_total_bytes >= MIB {
            let measured = self.writer_total_seconds / self.writer_total_bytes as f64;
            // Write/sync service plus a readback allowance. All comparisons
            // still finish actual concurrent readback; this is only a prior.
            self.io_per_stored_byte =
                (self.io_per_stored_byte * 0.8 + measured * 1.2 * 0.2).max(1e-10);
        }
        let bi = [256 << 10, MIB, 4 * MIB]
            .iter()
            .position(|b| *b == observation.read_bytes)
            .unwrap();
        self.read_cost[bi].add(0.0, observation.read_seconds / observation.raw as f64);
        self.total_raw += observation.raw;
        self.epoch_raw += observation.raw;
        let seconds = self.epoch_start.elapsed().as_secs_f64();
        if self.epoch_raw < 64 * MIB || seconds < 0.12 {
            return;
        }
        let cpu = cpu_seconds();
        self.cpu_fraction =
            ((cpu - self.epoch_cpu) / seconds / f64::from(self.resources.cpus)).clamp(0.0, 1.0);
        let rate = self.epoch_raw as f64 / seconds;
        self.last_seconds_per_raw = 1.0 / rate;
        self.plateau = if self.previous_rate > 0.0 && rate < self.previous_rate * 1.05 {
            self.plateau + 1
        } else {
            0
        };
        let before = self.settings.clone();
        let minimum_exploration = (2 * self.resources.cpus).min(self.max_workers);
        if self.cpu_fraction < 0.85
            && (self.settings.workers < minimum_exploration || self.plateau < 2)
            && self.settings.workers < self.max_workers
        {
            self.settings.workers += 1;
        } else if self.cpu_fraction > 0.95 && self.settings.workers > self.resources.cpus {
            self.settings.workers -= 1;
        }
        self.settings.verify_workers = self.settings.workers.div_ceil(2).min(self.resources.cpus);
        while !self.fits(
            self.settings.workers,
            self.settings.verify_workers,
            PROFILES[0],
        ) {
            self.settings.workers -= 1;
            self.settings.verify_workers =
                self.settings.workers.div_ceil(2).min(self.resources.cpus);
        }
        self.settings.epoch += 1;
        self.settings.read_bytes = match self.settings.epoch {
            1 => 4 * MIB,
            2 => 256 << 10,
            n if n.is_multiple_of(4) => [256 << 10, MIB, 4 * MIB][(n as usize / 4) % 3],
            _ => {
                let best = (0..3)
                    .filter(|i| self.read_cost[*i].count > 0)
                    .min_by(|a, b| {
                        self.read_cost[*a]
                            .encoder
                            .total_cmp(&self.read_cost[*b].encoder)
                    })
                    .unwrap_or(1);
                [256 << 10, MIB, 4 * MIB][best]
            }
        };
        let change = json!({"event":"settings_change","epoch":self.settings.epoch,
            "raw_bytes_observed":self.total_raw,"before":before,"after":self.settings,
            "cpu_fraction":self.cpu_fraction,"raw_bps":rate,"best_front":PROFILES[self.best[0]],
            "best_rear":PROFILES[self.best[1]],"reason":"productive_copy_feedback"});
        event(change.clone());
        self.changes.push(change);
        self.epoch_raw = 0;
        self.epoch_start = Instant::now();
        self.epoch_cpu = cpu;
        self.previous_rate = rate;
    }
}

fn two_ends(half: u64, bytes: u64, frame: u64) -> AnyResult<Vec<(u64, u64)>> {
    if bytes == 0 || frame == 0 || bytes > half {
        return Err("invalid two-ended span".into());
    }
    let mut lo = 0;
    let mut hi = bytes;
    let mut out = vec![];
    while lo < hi {
        let len = frame.min(hi - lo);
        out.push((half + lo, len));
        lo += len;
        if lo < hi {
            let len = frame.min(hi - lo);
            hi -= len;
            out.push((half + hi, len));
        }
    }
    let mut sorted = out.clone();
    sorted.sort_unstable();
    let mut end = half;
    for (offset, len) in sorted {
        if offset != end {
            return Err("two-ended coverage mismatch".into());
        }
        end += len;
    }
    if end != half + bytes {
        return Err("incomplete two-ended span".into());
    }
    Ok(out)
}

#[path = "online_pipeline.rs"]
mod pipeline;
pub fn compare(config: &Config) -> AnyResult<serde_json::Value> {
    let resources = config.resources.as_ref().ok_or("resources required")?;
    resources.validate()?;
    let source = config.source.as_ref().ok_or("source required")?;
    let previous = config
        .fixed_reference
        .as_ref()
        .ok_or("previous fixed control required")?;
    let make = |level, workers, frame| {
        Choice::new(
            resources,
            &Cost {
                codec: Codec::Zstd,
                level,
                frame_bytes: frame,
                stored_ratio: 1.0,
                encode_cpu_per_byte: 1e-9,
                verify_cpu_per_byte: 1e-9,
            },
            MIB,
            workers,
            1,
        )
        .ok_or("control exceeds memory budget")
    };
    let legacy = make(3, 2.min(resources.cpus), 8 * MIB)?;
    let initial = make(3, 1, 16 * MIB)?;
    let variants = [
        ("legacy_p2", &legacy, false),
        ("previous_fixed", previous, false),
        ("online", &initial, true),
    ];
    let mut runs = vec![];
    for round in 0..3 {
        let mut order = vec![0, 1, 2];
        if round == 1 {
            order.reverse();
        } else if round == 2 {
            order.rotate_left(1);
        }
        for index in order {
            let (name, choice, adaptive) = variants[index];
            let result = pipeline::run(
                source,
                &config.destination,
                resources,
                name,
                choice,
                config.validate_bytes,
                adaptive,
            )?;
            runs.push(json!({"round":round,"variant":name,"result":result}));
        }
    }
    Ok(
        json!({"schema_version":1,"scope":"experimental private study records, not product v2 archive acceptance",
        "resources":resources,"heldout_bytes":config.validate_bytes,"runs":runs,
        "selection_probe_bytes":0,"source_calibration_inside_copy":false,
        "cpu_priors":"relative costs from previous r5 mixed-data study; same coefficients for every profile",
        "hash_pool":"same allocation-sized BLAKE3 pool for all variants",
        "new_algorithm_has_no_per_case_r5_selection_input":true}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn resources(cpus: u32) -> Resources {
        Resources {
            cpus,
            memory_bytes: 4 << 30,
            source_bps: 1e9,
            destination_write_bps: 5e8,
            destination_read_bps: 1e9,
        }
    }
    #[test]
    fn both_ends_cover_all_bytes_once_including_short_and_odd_tails() {
        for bytes in [1, 511, 512, 8193, 16 * MIB, 33 * MIB + 17] {
            let ranges = two_ends(bytes, bytes, 8 * MIB).unwrap();
            assert_eq!(ranges[0].0, bytes);
            if ranges.len() > 1 {
                assert_eq!(ranges[1].0 + ranges[1].1, 2 * bytes);
            }
            assert_eq!(ranges.iter().map(|r| r.1).sum::<u64>(), bytes);
        }
        assert!(two_ends(1, 2, 1).is_err());
        assert!(two_ends(1, 1, 0).is_err());
    }
    #[test]
    fn starts_small_and_keeps_resource_bounds() {
        for cpus in [2, 4, 8] {
            let p = Controller::new(&resources(cpus));
            assert_eq!(p.settings.workers, 1);
            assert_eq!(p.settings.read_bytes, MIB);
            assert!(p.max_workers <= 32);
            assert!(p.fits(p.max_workers, cpus, PROFILES[0]));
        }
    }
    #[test]
    fn zero_does_not_trigger_expensive_compression_and_incompressible_returns_to_zstd() {
        let mut p = Controller::new(&resources(8));
        p.zero[1] = true;
        assert_eq!(p.next_profile(1).0, PROFILES[0]);
        p.best[0] = 1;
        p.observe(&FrameObservation {
            lane: 0,
            requested: PROFILES[1],
            actual: Codec::Raw,
            raw: 16 * MIB,
            stored: 16 * MIB + 128,
            encoder_cpu: 0.001,
            read_seconds: 0.01,
            read_bytes: MIB,
            writer_seconds: 0.01,
        });
        assert_eq!(p.best[0], 0);
    }
    #[test]
    fn expensive_probe_has_one_inflight_copy_and_is_pruned_when_cpu_cost_loses() {
        let mut p = Controller::new(&resources(8));
        p.costs[1][0].add(0.5, 1e-9);
        p.last_seconds_per_raw = 4e-9;
        assert_eq!(p.next_profile(1).0, PROFILES[1]);
        assert_eq!(p.next_profile(1).0, PROFILES[0]);
        p.pending_probe[1] = None;
        p.costs[1][1].add(0.4, 1.5e-9);
        p.costs[1][2].add(0.5, 1.6e-9);
        assert_eq!(p.next_profile(1).0, PROFILES[0]);
    }
    #[test]
    fn cheap_codec_miss_does_not_hide_a_stronger_codec_or_veto_its_success() {
        let mut p = Controller::new(&resources(8));
        let baseline = FrameObservation {
            lane: 0,
            requested: PROFILES[0],
            actual: Codec::Raw,
            raw: 16 * MIB,
            stored: 16 * MIB + 128,
            encoder_cpu: 0.01,
            read_seconds: 0.01,
            read_bytes: MIB,
            writer_seconds: 0.01,
        };
        p.observe(&baseline);
        assert_eq!(p.next_profile(0).0, PROFILES[1]);
        p.observe(&FrameObservation {
            requested: PROFILES[1],
            actual: Codec::Zstd,
            stored: 8 * MIB,
            encoder_cpu: 0.02,
            ..baseline
        });
        assert_eq!(p.best[0], 1);
        p.observe(&baseline);
        assert_eq!(
            p.best[0], 1,
            "a baseline-only miss must not erase stronger-codec evidence"
        );
    }
    #[test]
    fn stronger_codec_is_revisited_and_high_cpu_returns_to_baseline() {
        let mut p = Controller::new(&resources(8));
        p.costs[0][0].add(1.0, 1e-9);
        p.costs[0][1].add(1.0, 2e-9);
        p.compressible[0] = false;
        p.lane_jobs[0] = 19;
        assert_eq!(p.next_profile(0).0, PROFILES[1]);
        p.best[0] = 1;
        p.cpu_fraction = 0.99;
        assert_eq!(p.next_profile(0).0, PROFILES[0]);
    }
}
