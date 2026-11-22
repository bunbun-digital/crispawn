// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! Resource-aware proposal and measured selection for the concurrent-pipeline study.
//! This does not silently replace the production v2 writer or its integrity checks.
use crate::{Error, Result};
use crispawn_core::{plan::Plan, types::Codec};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resources {
    pub cpus: u32,
    pub memory_bytes: u64,
    pub source_bps: f64,
    pub destination_write_bps: f64,
    pub destination_read_bps: f64,
}
impl Resources {
    pub fn memory_budget(&self) -> u64 {
        self.memory_bytes
            .saturating_sub((512 << 20).max(self.memory_bytes / 4))
    }
    pub fn validate(&self) -> Result<()> {
        if !(1..=32).contains(&self.cpus)
            || self.memory_budget() < 256 << 20
            || [
                self.source_bps,
                self.destination_write_bps,
                self.destination_read_bps,
            ]
            .iter()
            .any(|x| !x.is_finite() || *x <= 0.0)
        {
            return Err(Error::new(
                "INVALID_RESOURCES",
                "finite positive bandwidth, CPU and RAM limits required",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cost {
    pub codec: Codec,
    pub level: i32,
    pub frame_bytes: u64,
    pub stored_ratio: f64,
    pub encode_cpu_per_byte: f64,
    pub verify_cpu_per_byte: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choice {
    pub id: String,
    pub plan: Plan,
    pub verify_workers: u32,
    pub conservative_memory_bytes: u64,
    pub predicted_bps: f64,
}

impl Choice {
    pub fn new(
        resources: &Resources,
        cost: &Cost,
        read: u64,
        workers: u32,
        verify: u32,
    ) -> Option<Self> {
        if resources.validate().is_err()
            || workers == 0
            || workers > 32
            || verify == 0
            || verify > resources.cpus
        {
            return None;
        }
        if !cost.stored_ratio.is_finite()
            || !(0.0..=1.01).contains(&cost.stored_ratio)
            || [cost.encode_cpu_per_byte, cost.verify_cpu_per_byte]
                .iter()
                .any(|x| !x.is_finite() || *x <= 0.0)
        {
            return None;
        }
        let plan = Plan {
            format_version: 2,
            range_bytes: cost.frame_bytes,
            read_bytes: read,
            workers,
            codec: cost.codec,
            compression_level: cost.level,
            memory_limit_bytes: resources.memory_budget(),
            ..Plan::current()
        };
        if plan.validate(2 << 30).is_err() {
            return None;
        }
        // Simultaneous decode/readback has its own bounded workspace; it must
        // not borrow the old serial verification reserve more than once.
        let memory = plan.memory_bound() + u64::from(verify) * ((128 << 20) + 2 * cost.frame_bytes);
        if memory > resources.memory_budget() {
            return None;
        }
        let read_cost = 1.0 / resources.source_bps;
        let write_cost = cost.stored_ratio / resources.destination_write_bps;
        let readback_cost = cost.stored_ratio / resources.destination_read_bps;
        // Shared destination service and CPU capacity are charged for both
        // encoding and verification. CPU idleness is not itself a penalty.
        let seconds_per_byte = read_cost
            .max(write_cost + readback_cost)
            .max((cost.encode_cpu_per_byte + cost.verify_cpu_per_byte) / f64::from(resources.cpus))
            .max((read_cost + cost.encode_cpu_per_byte) / f64::from(workers))
            .max((readback_cost + cost.verify_cpu_per_byte) / f64::from(verify));
        Some(Self {
            id: format!(
                "b{}-c{}-p{}-v{}-{}-{}",
                read,
                cost.frame_bytes,
                workers,
                verify,
                cost.codec.name(),
                cost.level
            ),
            plan,
            verify_workers: verify,
            conservative_memory_bytes: memory,
            predicted_bps: 1.0 / seconds_per_byte,
        })
    }
}

fn validate_costs(resources: &Resources, costs: &[Cost]) -> Result<()> {
    resources.validate()?;
    if costs.is_empty()
        || costs.iter().any(|c| {
            !c.stored_ratio.is_finite()
                || !(0.0..=1.01).contains(&c.stored_ratio)
                || [c.encode_cpu_per_byte, c.verify_cpu_per_byte]
                    .iter()
                    .any(|x| !x.is_finite() || *x <= 0.0)
        })
    {
        return Err(Error::new(
            "INVALID_MEASUREMENT",
            "finite measured codec costs required",
        ));
    }
    Ok(())
}

/// Seed the race with comparable full-CPU settings and a legacy control.
/// Do not spend the remaining budget varying a family picked by one stale
/// bandwidth estimate: explore around the measured winner instead.
pub fn propose(resources: &Resources, costs: &[Cost], limit: usize) -> Result<Vec<Choice>> {
    validate_costs(resources, costs)?;
    if limit < 4 {
        return Err(Error::new(
            "INSUFFICIENT_EXPLORATION",
            "at least four pilot trials required",
        ));
    }
    let n = resources.cpus;
    let v = n.div_ceil(2);
    let best = |family: Option<Codec>, level: Option<i32>, frame: Option<u64>, p, v| {
        costs
            .iter()
            .filter(|c| {
                family.is_none_or(|x| c.codec == x)
                    && level.is_none_or(|x| c.level == x)
                    && frame.is_none_or(|x| c.frame_bytes == x)
            })
            .filter_map(|c| Choice::new(resources, c, 1 << 20, p, v))
            .max_by(|a, b| {
                a.predicted_bps.total_cmp(&b.predicted_bps).then_with(|| {
                    b.conservative_memory_bytes
                        .cmp(&a.conservative_memory_bytes)
                })
            })
    };
    let mut out: Vec<Choice> = vec![];
    let mut add = |c: Option<Choice>| {
        if let Some(c) = c {
            if !out.iter().any(|x| x.id == c.id) {
                out.push(c);
            }
        }
    };
    add(best(None, None, None, n, v));
    add(best(Some(Codec::Zstd), Some(3), Some(8 << 20), 2.min(n), 1));
    for family in [Codec::Raw, Codec::Zstd, Codec::Xz, Codec::Gzip] {
        add(best(Some(family), None, None, n, v));
    }
    for level in [1, 3, 6, 9] {
        add(best(Some(Codec::Zstd), Some(level), Some(16 << 20), n, v)
            .or_else(|| best(Some(Codec::Zstd), Some(level), None, n, v)));
    }
    add(best(
        Some(Codec::Zstd),
        Some(9),
        Some(16 << 20),
        (2 * n).min(32),
        v,
    )
    .or_else(|| best(Some(Codec::Zstd), None, None, (2 * n).min(32), v)));
    drop(add);
    out.truncate(limit);
    if out.is_empty() {
        return Err(Error::new(
            "NO_FEASIBLE_CANDIDATE",
            "resource and memory bounds",
        ));
    }
    Ok(out)
}

/// Change one coordinate at a time while holding the other parameters fixed.
/// This closes the P4/zstd3 hole left by probing only P2/zstd3 and P4/zstd9.
pub fn neighborhood(
    resources: &Resources,
    costs: &[Cost],
    measured: &Measurement,
) -> Result<Vec<Choice>> {
    validate_costs(resources, costs)?;
    select(std::slice::from_ref(measured), measured.verified_bytes)?;
    let base = &measured.choice;
    let p = base.plan.workers;
    let v = base.verify_workers;
    let n = resources.cpus;
    let frame = base.plan.range_bytes;
    let current = costs
        .iter()
        .find(|c| {
            c.codec == base.plan.codec
                && c.level == base.plan.compression_level
                && c.frame_bytes == frame
        })
        .ok_or_else(|| Error::new("MISSING_CODEC_COST", "measured plan cost"))?;
    let cpu = measured.cpu_seconds / measured.seconds / f64::from(n);
    let mut out: Vec<Choice> = vec![];
    let mut add = |cost: Option<&Cost>, read, p, v| {
        if let Some(cost) = cost {
            if let Some(mut c) = Choice::new(resources, cost, read, p, v) {
                c.plan.checkpoint_records = base.plan.checkpoint_records;
                c.plan.container_bytes = base.plan.container_bytes;
                c.plan.regions = base.plan.regions;
                if c.plan.validate(2 << 30).is_ok()
                    && c.id != base.id
                    && !out.iter().any(|x| x.id == c.id)
                {
                    out.push(c);
                }
            }
        }
    };
    let raw = costs
        .iter()
        .find(|c| c.codec == Codec::Raw && c.frame_bytes == frame);
    if raw.is_some_and(|c| {
        measured.stored_bytes as f64 / measured.verified_bytes as f64 >= c.stored_ratio * 0.995
    }) {
        add(raw, base.plan.read_bytes, p, v);
    }
    let mut ps = if cpu < 0.85 {
        vec![p.saturating_mul(2), n, p / 2]
    } else {
        vec![p / 2, p.saturating_mul(2), n]
    };
    // These are coupled read/encode workers, not a CPU allocation. Waiting
    // I/O can need more than 2*N tasks; actual CPU capacity remains N and the
    // Plan memory bound and 32-worker engine limit still apply.
    if cpu < 0.70 {
        ps.extend([p.saturating_mul(4), p.saturating_mul(8)]);
    }
    add(Some(current), base.plan.read_bytes, ps[0], v);
    let vs = if cpu < 0.85 {
        vec![v.saturating_mul(2), n.div_ceil(2), n, 1, v / 2]
    } else {
        vec![v / 2, 1, v.saturating_mul(2), n.div_ceil(2), n]
    };
    add(Some(current), base.plan.read_bytes, p, vs[0]);
    for b in [4 << 20, 256 << 10, 1 << 20] {
        add(Some(current), b, p, v);
    }
    for c in costs
        .iter()
        .filter(|c| c.codec == current.codec && c.level == current.level && c.frame_bytes != frame)
    {
        add(Some(c), base.plan.read_bytes, p, v);
    }
    let mut levels: Vec<_> = costs
        .iter()
        .filter(|c| c.codec == current.codec && c.frame_bytes == frame)
        .map(|c| c.level)
        .collect();
    levels.sort_unstable();
    levels.dedup();
    if let Some(i) = levels.iter().position(|x| *x == current.level) {
        let mut adjacent = vec![];
        if i + 1 < levels.len() {
            adjacent.push(levels[i + 1]);
        }
        if i > 0 {
            adjacent.push(levels[i - 1]);
        }
        if cpu >= 0.85 {
            adjacent.reverse();
        }
        for level in adjacent {
            add(
                costs.iter().find(|c| {
                    c.codec == current.codec && c.level == level && c.frame_bytes == frame
                }),
                base.plan.read_bytes,
                p,
                v,
            );
        }
    }
    for newp in ps {
        add(Some(current), base.plan.read_bytes, newp, v);
    }
    for newv in vs {
        add(Some(current), base.plan.read_bytes, p, newv);
    }
    drop(add);
    Ok(out)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Measurement {
    pub choice: Choice,
    pub seconds: f64,
    pub cpu_seconds: f64,
    pub stored_bytes: u64,
    pub verified_bytes: u64,
}

pub fn select(measurements: &[Measurement], expected_bytes: u64) -> Result<usize> {
    if expected_bytes == 0
        || measurements.is_empty()
        || measurements.iter().any(|m| {
            !m.seconds.is_finite()
                || m.seconds <= 0.0
                || !m.cpu_seconds.is_finite()
                || m.cpu_seconds < 0.0
                || m.verified_bytes != expected_bytes
        })
    {
        return Err(Error::new(
            "INVALID_MEASUREMENT",
            "complete verified positive-duration pipeline trials required",
        ));
    }
    let fastest = measurements
        .iter()
        .map(|m| m.seconds)
        .fold(f64::INFINITY, f64::min);
    let smallest = measurements
        .iter()
        .filter(|m| m.seconds <= fastest * 1.03)
        .map(|m| m.stored_bytes)
        .min()
        .unwrap();
    Ok(measurements
        .iter()
        .enumerate()
        .filter(|(_, m)| m.seconds <= fastest * 1.03)
        .filter(|(_, m)| m.stored_bytes as f64 <= smallest as f64 * 1.005)
        .min_by(|(_, a), (_, b)| {
            a.cpu_seconds.total_cmp(&b.cpu_seconds).then_with(|| {
                a.choice
                    .conservative_memory_bytes
                    .cmp(&b.choice.conservative_memory_bytes)
            })
        })
        .unwrap()
        .0)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn resources(cpus: u32, write: f64) -> Resources {
        Resources {
            cpus,
            memory_bytes: 4 << 30,
            source_bps: 1e9,
            destination_write_bps: write,
            destination_read_bps: write * 2.0,
        }
    }
    fn costs() -> Vec<Cost> {
        vec![
            Cost {
                codec: Codec::Raw,
                level: 0,
                frame_bytes: 8 << 20,
                stored_ratio: 1.0,
                encode_cpu_per_byte: 1e-9,
                verify_cpu_per_byte: 1e-9,
            },
            Cost {
                codec: Codec::Zstd,
                level: 6,
                frame_bytes: 8 << 20,
                stored_ratio: 0.5,
                encode_cpu_per_byte: 12e-9,
                verify_cpu_per_byte: 2e-9,
            },
            Cost {
                codec: Codec::Xz,
                level: 3,
                frame_bytes: 16 << 20,
                stored_ratio: 0.2,
                encode_cpu_per_byte: 40e-9,
                verify_cpu_per_byte: 4e-9,
            },
            Cost {
                codec: Codec::Gzip,
                level: 1,
                frame_bytes: 8 << 20,
                stored_ratio: 0.65,
                encode_cpu_per_byte: 15e-9,
                verify_cpu_per_byte: 3e-9,
            },
        ]
    }
    #[test]
    fn all_cores_are_explored_before_the_budget_is_consumed() {
        let p = propose(&resources(8, 100e6), &costs(), 12).unwrap();
        assert_eq!(p[0].plan.workers, 8);
        for codec in [Codec::Raw, Codec::Zstd, Codec::Xz, Codec::Gzip] {
            assert!(p.iter().any(|x| x.plan.codec == codec));
        }
        for b in [256 << 10, 1 << 20, 4 << 20] {
            let m = Measurement {
                choice: p[0].clone(),
                seconds: 1.0,
                cpu_seconds: 1.0,
                stored_bytes: 5,
                verified_bytes: 10,
            };
            let local = neighborhood(&resources(8, 100e6), &costs(), &m).unwrap();
            assert!(p.iter().chain(local.iter()).any(|x| x.plan.read_bytes == b));
        }
    }
    #[test]
    fn output_bound_and_cpu_bound_rankings_change() {
        let c = costs();
        let fast = propose(&resources(2, 1e9), &c, 20).unwrap();
        assert_eq!(fast[0].plan.codec, Codec::Raw);
        let slow = propose(&resources(8, 20e6), &c, 20).unwrap();
        let best = slow
            .iter()
            .max_by(|a, b| a.predicted_bps.total_cmp(&b.predicted_bps))
            .unwrap();
        assert_eq!(best.plan.codec, Codec::Xz);
    }
    #[test]
    fn memory_and_cpu_limits_are_not_ignored() {
        for cpus in [2, 4, 8] {
            let r = resources(cpus, 100e6);
            for p in propose(&r, &costs(), 16).unwrap() {
                assert!(p.plan.workers <= 32);
                assert!(p.verify_workers <= cpus);
                assert!(p.conservative_memory_bytes <= r.memory_budget());
            }
        }
    }
    #[test]
    fn legacy_control_and_compression_level_breadth_survive_shortlisting() {
        let mut c = costs();
        for level in [1, 3, 6, 9] {
            for frame_bytes in [8 << 20, 16 << 20] {
                c.push(Cost {
                    codec: Codec::Zstd,
                    level,
                    frame_bytes,
                    stored_ratio: 0.55,
                    encode_cpu_per_byte: 2e-9,
                    verify_cpu_per_byte: 1e-9,
                });
            }
        }
        for cpus in [2, 4, 8] {
            let choices = propose(&resources(cpus, 100e6), &c, 16).unwrap();
            assert!(choices.iter().any(|x| x.plan.codec == Codec::Zstd
                && x.plan.compression_level == 3
                && x.plan.range_bytes == 8 << 20
                && x.plan.workers == 2
                && x.verify_workers == 1
                && x.plan.read_bytes == 1 << 20));
            for level in [1, 3, 6, 9] {
                assert!(choices.iter().any(|x| x.plan.codec == Codec::Zstd
                    && x.plan.compression_level == level
                    && x.plan.workers == cpus));
            }
            assert!(choices.iter().any(|x| x.plan.workers == cpus * 2));
        }
    }
    #[test]
    fn unverified_or_nonfinite_trials_never_win() {
        let c = propose(&resources(8, 100e6), &costs(), 8)
            .unwrap()
            .remove(0);
        let mut m = Measurement {
            choice: c,
            seconds: 1.0,
            cpu_seconds: 1.0,
            stored_bytes: 5,
            verified_bytes: 10,
        };
        assert!(select(&[m.clone()], 11).is_err());
        m.seconds = f64::NAN;
        assert!(select(&[m], 10).is_err());
    }
    #[test]
    fn measured_total_time_beats_cpu_saturation() {
        let choices = propose(&resources(8, 100e6), &costs(), 8).unwrap();
        let m = [
            Measurement {
                choice: choices[0].clone(),
                seconds: 1.0,
                cpu_seconds: 0.5,
                stored_bytes: 100,
                verified_bytes: 10,
            },
            Measurement {
                choice: choices[1].clone(),
                seconds: 2.0,
                cpu_seconds: 16.0,
                stored_bytes: 50,
                verified_bytes: 10,
            },
        ];
        assert_eq!(select(&m, 10).unwrap(), 0);
    }
    #[test]
    fn tiny_header_size_differences_do_not_justify_wasted_compression_cpu() {
        let choices = propose(&resources(8, 100e6), &costs(), 8).unwrap();
        let a = Measurement {
            choice: choices[0].clone(),
            seconds: 1.0,
            cpu_seconds: 4.0,
            stored_bytes: 100_000_000,
            verified_bytes: 200_000_000,
        };
        let b = Measurement {
            choice: choices[1].clone(),
            seconds: 1.01,
            cpu_seconds: 0.5,
            stored_bytes: 100_004_096,
            verified_bytes: 200_000_000,
        };
        assert_eq!(select(&[a, b], 200_000_000).unwrap(), 1);
    }
    #[test]
    fn local_parallelism_preserves_codec_level_and_other_settings() {
        let r = resources(2, 100e6);
        let mut c = costs();
        let mut medium = c[1].clone();
        medium.level = 3;
        c.push(medium.clone());
        let mut choice = Choice::new(&r, &medium, 1 << 20, 2, 1).unwrap();
        choice.plan.regions = 4;
        choice.plan.checkpoint_records = 16;
        choice.plan.container_bytes = 128 << 20;
        let m = Measurement {
            choice,
            seconds: 1.0,
            cpu_seconds: 0.8,
            stored_bytes: 50,
            verified_bytes: 100,
        };
        let local = neighborhood(&r, &c, &m).unwrap();
        let doubled = local
            .iter()
            .find(|x| {
                x.plan.workers == 4 && x.plan.codec == Codec::Zstd && x.plan.compression_level == 3
            })
            .unwrap();
        assert_eq!(doubled.plan.read_bytes, m.choice.plan.read_bytes);
        assert_eq!(doubled.plan.regions, 4);
        assert_eq!(doubled.plan.checkpoint_records, 16);
        assert_eq!(doubled.plan.container_bytes, 128 << 20);
        assert!(local.iter().all(|x| x.plan.workers <= 32
            && x.verify_workers <= 2
            && x.conservative_memory_bytes <= r.memory_budget()));
    }
    #[test]
    fn incompressible_data_gets_a_matched_raw_control() {
        let r = resources(8, 100e6);
        let c = costs();
        let choice = Choice::new(&r, &c[1], 4 << 20, 4, 2).unwrap();
        let m = Measurement {
            choice,
            seconds: 1.0,
            cpu_seconds: 1.0,
            stored_bytes: 100,
            verified_bytes: 100,
        };
        let local = neighborhood(&r, &c, &m).unwrap();
        let raw = local.iter().find(|x| x.plan.codec == Codec::Raw).unwrap();
        assert_eq!(raw.plan.workers, 4);
        assert_eq!(raw.verify_workers, 2);
        assert_eq!(raw.plan.read_bytes, 4 << 20);
        assert_eq!(raw.plan.range_bytes, 8 << 20);
    }
}
