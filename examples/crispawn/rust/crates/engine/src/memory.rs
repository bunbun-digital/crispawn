// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-digital. Licensed under Apache-2.0.
//! Read-only memory observations; never changes kernel writeback settings.
use crispawn_core::live::MemoryStatus;

pub(crate) fn sample() -> MemoryStatus {
    parse(&std::fs::read_to_string("/proc/meminfo").unwrap_or_default())
}

fn parse(text: &str) -> MemoryStatus {
    let mut result = MemoryStatus::default();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let (Some(key), Some(value), Some("kB")) = (words.next(), words.next(), words.next())
        else {
            continue;
        };
        let Ok(value) = value.parse::<u64>() else {
            continue;
        };
        match key {
            "MemTotal:" => result.total_kib = value,
            "MemAvailable:" => result.available_kib = value,
            "Cached:" => result.cached_kib = value,
            "Dirty:" => result.dirty_kib = value,
            "Writeback:" => result.writeback_kib = value,
            "AnonPages:" => result.anonymous_kib = value,
            _ => {}
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn distinguishes_reclaimable_cache_dirty_writeback_and_anonymous_memory() {
        let s = parse("MemTotal: 16000 kB\nMemAvailable: 12000 kB\nCached: 3000 kB\nDirty: 800 kB\nWriteback: 200 kB\nAnonPages: 1000 kB\nSwapCached: 99 kB\n");
        assert_eq!(
            (s.total_kib, s.available_kib, s.cached_kib),
            (16000, 12000, 3000)
        );
        assert_eq!(
            (s.dirty_kib, s.writeback_kib, s.anonymous_kib),
            (800, 200, 1000)
        );
        let missing = parse("Dirty: invalid kB\nWriteback: 64 MB\n");
        assert_eq!((missing.dirty_kib, missing.writeback_kib), (0, 0));
    }
}
