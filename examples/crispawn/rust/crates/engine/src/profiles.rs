// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! Measured starting profiles. These are starting points, not an optimality claim.
use crate::{Error, Result};
use crispawn_core::{error::require, plan::Plan, types::Codec};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    #[default]
    Balanced,
    LongMatches,
    HighCompression,
    Incompressible,
}

pub fn memory_bound(plan: &Plan, verifiers: u32) -> u64 {
    processing_workspace(plan, verifiers) + queue_budget(plan, verifiers)
}

pub(crate) fn processing_workspace(plan: &Plan, verifiers: u32) -> u64 {
    plan.memory_bound()
        + if plan.segmented_storage {
            // Two active writers, four queued part buffers and readback buffers.
            // Keep this runtime cost separate from historical plan validation.
            6 * plan.storage_buffer_bytes() + (2 << 20)
        } else {
            u64::from(verifiers) * ((128 << 20) + 2 * plan.range_bytes)
        }
}

pub(crate) fn queue_budget(plan: &Plan, _verifiers: u32) -> u64 {
    if plan.format_version != 3 {
        return 0;
    }
    // At most two ready encoding units, independent of installed RAM and codec
    // ratio. This is a handoff window, not a share of all spare memory.
    2 * (plan.range_bytes + 8192)
}

pub(crate) fn live_memory_budget(configured: u64, available: u64, automatic_v2: bool) -> u64 {
    if automatic_v2 {
        // A worst-case safety ceiling for active jobs, not an allocation or a
        // cache partition. Ready data is independently bounded to two units.
        available.saturating_sub(512 << 20).min(3 << 30)
    } else {
        configured.min(available.saturating_sub(192 << 20))
    }
}

pub fn select(
    template: &Plan,
    profile: Profile,
    cpus: u32,
    memory_bytes: u64,
    source_bytes: u64,
) -> Result<(Plan, u32)> {
    require(cpus > 0, "INVALID_RESOURCES", "no CPU allocation")?;
    let mut plan = template.clone();
    plan.format_version = 3;
    plan.segmented_storage = true;
    plan.read_bytes = 1 << 20;
    plan.range_bytes = 16 << 20;
    plan.container_bytes = plan.container_bytes.max(plan.range_bytes + 352);
    plan.memory_limit_bytes = memory_bytes.min(template.memory_limit_bytes);
    plan.workers = cpus.saturating_mul(2).min(8);
    let verifiers = crate::storage_worker::WORKERS;
    (plan.codec, plan.compression_level) = match profile {
        Profile::Balanced => (Codec::Zstd, 3),
        Profile::LongMatches => (Codec::Zstd, 6),
        Profile::HighCompression => (Codec::Zstd, 9),
        Profile::Incompressible => (Codec::Raw, 0),
    };
    loop {
        if memory_bound(&plan, verifiers) <= plan.memory_limit_bytes {
            plan.validate(source_bytes)?;
            return Ok((plan, verifiers));
        }
        if plan.workers > 1 {
            plan.workers = plan.workers.div_ceil(2);
        } else if plan.range_bytes > 1 << 20 {
            plan.range_bytes /= 2;
        } else {
            return Err(Error::new(
                "MEMORY_BUDGET",
                "starting profile cannot fit available RAM",
            ));
        }
    }
}

/// Limit RAM using both the machine and the effective cgroup, leaving OS/UI headroom.
pub fn available_memory() -> u64 {
    let available = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let mut words = line.split_whitespace();
                (words.next()? == "MemAvailable:")
                    .then(|| words.next()?.parse::<u64>().ok())
                    .flatten()
            })
        })
        .unwrap_or(256 << 10)
        .saturating_mul(1024);
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
    let relative = cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .unwrap_or("/");
    let mut directory =
        std::path::Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/'));
    let mut limit = available;
    loop {
        if let (Ok(max), Ok(current)) = (
            std::fs::read_to_string(directory.join("memory.max")),
            std::fs::read_to_string(directory.join("memory.current")),
        ) {
            if let (Ok(max), Ok(current)) =
                (max.trim().parse::<u64>(), current.trim().parse::<u64>())
            {
                limit = limit.min(max.saturating_sub(current));
            }
        }
        if directory == std::path::Path::new("/sys/fs/cgroup") || !directory.pop() {
            break;
        }
    }
    limit.saturating_sub(192 << 20)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn live_automatic_budget_scales_with_ram_and_retains_os_headroom() {
        for gib in [4u64, 8, 16] {
            let available = gib << 30;
            let budget = live_memory_budget(4 << 30, available, true);
            assert!(available - budget >= 512 << 20 && budget <= 3 << 30);
            let mut template = Plan::current();
            template.memory_limit_bytes = budget;
            let (mut p, v) = select(&template, Profile::Balanced, 8, budget, 1 << 40).unwrap();
            crate::online::reserve(&mut p, 8, v).unwrap();
            assert!(memory_bound(&p, v) <= budget);
            assert!(queue_budget(&p, v) >= 2 * p.range_bytes + 8192);
            assert_eq!(queue_budget(&p, v), 2 * (p.range_bytes + 8192));
            assert!(queue_budget(&p, v) < 33 << 20);
            if gib >= 8 {
                assert!(p.workers >= 8);
            }
        }
        assert_eq!(live_memory_budget(1 << 30, 16 << 30, false), 1 << 30);
        assert_eq!(live_memory_budget(4 << 30, 64 << 20, true), 0);
    }
    #[test]
    fn measured_starts_cover_cpu_and_ram_profiles() {
        for (cpus, expected) in [(2, 4), (4, 8), (8, 8)] {
            for memory in [4u64, 8, 16] {
                for profile in [
                    Profile::Balanced,
                    Profile::LongMatches,
                    Profile::HighCompression,
                    Profile::Incompressible,
                ] {
                    let mut template = Plan::current();
                    template.memory_limit_bytes = memory << 30;
                    let (plan, verify) =
                        select(&template, profile, cpus, memory << 30, 1 << 40).unwrap();
                    assert_eq!(plan.workers, expected);
                    assert_eq!(verify, 2);
                    assert_eq!(plan.read_bytes, 1 << 20);
                    assert_eq!(plan.range_bytes, 16 << 20);
                    assert!(memory_bound(&plan, verify) <= plan.memory_limit_bytes);
                }
            }
        }
    }
    #[test]
    fn low_memory_reduces_concurrency_and_refuses_impossible_profiles() {
        let mut template = Plan::current();
        template.memory_limit_bytes = 4 << 30;
        let (plan, v) = select(&template, Profile::Balanced, 8, 512 << 20, 1 << 30).unwrap();
        assert!(plan.workers < 8);
        assert!(memory_bound(&plan, v) <= 512 << 20);
        assert_eq!(
            select(&template, Profile::Balanced, 8, 64 << 20, 1 << 30)
                .unwrap_err()
                .code,
            "MEMORY_BUDGET"
        );
    }
}
