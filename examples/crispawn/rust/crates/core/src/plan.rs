// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
use crate::{
    error::require,
    types::{decimal, Codec},
    Result,
};
use serde::{Deserialize, Serialize};

pub const MAX_RANGE_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_RANGES: u64 = 16_777_216;
pub const MAX_FILES: u64 = 65_536;
pub const DECODER_MEMORY: u64 = 128 * 1024 * 1024;
pub const MAX_BACKUP_FILE_BYTES: u64 = 2_000_000_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub format_version: u32,
    #[serde(with = "decimal")]
    pub range_bytes: u64,
    #[serde(with = "decimal")]
    pub container_bytes: u64,
    #[serde(with = "decimal")]
    pub read_bytes: u64,
    pub workers: u32,
    pub regions: u32,
    pub checkpoint_records: u32,
    pub codec: Codec,
    pub compression_level: i32,
    #[serde(with = "decimal")]
    pub memory_limit_bytes: u64,
}
impl Default for Plan {
    fn default() -> Self {
        Self {
            format_version: 1,
            range_bytes: 8 << 20,
            container_bytes: 256 << 20,
            read_bytes: 1 << 20,
            workers: 2,
            regions: 8,
            checkpoint_records: 8,
            codec: Codec::Raw,
            compression_level: 0,
            memory_limit_bytes: 512 << 20,
        }
    }
}
impl Plan {
    /// Format used for new product backups. Default remains the legacy plan
    /// for explicit v1 library fixtures and compatibility tests.
    pub fn current() -> Self {
        Self {
            format_version: 2,
            ..Self::default()
        }
    }
    pub fn validate(&self, source_bytes: u64) -> Result<()> {
        require(
            matches!(self.format_version, 1 | 2 | 3),
            "UNKNOWN_VERSION",
            "plan",
        )?;
        require(source_bytes > 0, "EMPTY_SOURCE", "source must be nonempty")?;
        require(
            (512..=MAX_RANGE_BYTES).contains(&self.range_bytes) && self.range_bytes % 512 == 0,
            "INVALID_PLAN",
            "range size must be a multiple of 512, at most 16 MiB",
        )?;
        require(
            self.container_bytes >= self.range_bytes + 352
                && self.container_bytes
                    <= if self.format_version >= 2 {
                        MAX_BACKUP_FILE_BYTES
                    } else {
                        1 << 30
                    },
            "INVALID_PLAN",
            "container must fit a maximum record and headers within the format's file-size limit",
        )?;
        require(
            self.read_bytes > 0 && self.read_bytes <= self.range_bytes,
            "INVALID_PLAN",
            "I/O request size",
        )?;
        require(
            (1..=32).contains(&self.workers)
                && (1..=64).contains(&self.regions)
                && (1..=64).contains(&self.checkpoint_records),
            "INVALID_PLAN",
            "workers/regions/checkpoint bounds",
        )?;
        let level_ok = match self.codec {
            Codec::Raw => self.compression_level == 0,
            Codec::Zero => false,
            Codec::Zstd => (1..=9).contains(&self.compression_level),
            Codec::Xz => {
                if self.format_version == 3 {
                    matches!(self.compression_level, 0 | 3)
                } else {
                    (0..=6).contains(&self.compression_level)
                }
            }
            Codec::Gzip => {
                if self.format_version == 3 {
                    matches!(self.compression_level, 1 | 6 | 9)
                } else {
                    (0..=9).contains(&self.compression_level)
                }
            }
        };
        require(level_ok, "INVALID_PLAN", "codec or level")?;
        require(
            self.range_count(source_bytes) <= MAX_RANGES,
            "LIMIT_EXCEEDED",
            "too many ranges",
        )?;
        let records_per_file = (self.container_bytes - 192) / (self.range_bytes + 160);
        let ranges = self.range_count(source_bytes);
        let mut worst_files = ranges / records_per_file + u64::from(ranges % records_per_file != 0);
        if self.format_version >= 2 {
            // Every parallel region may have its own smaller tail file.
            worst_files = ranges.min(worst_files + u64::from(self.regions).min(ranges) - 1);
        }
        require(
            worst_files <= MAX_FILES,
            "LIMIT_EXCEEDED",
            "worst-case raw data requires too many containers",
        )?;
        require(
            self.memory_bound() <= self.memory_limit_bytes,
            "MEMORY_BUDGET",
            "conservative pipeline memory bound exceeds limit",
        )
    }
    pub fn range_count(&self, n: u64) -> u64 {
        n / self.range_bytes + u64::from(n % self.range_bytes != 0)
    }
    pub fn range(&self, n: u64, id: u64) -> Result<(u64, u64)> {
        require(id < self.range_count(n), "RANGE_BOUNDS", "range id")?;
        let offset = id * self.range_bytes;
        Ok((offset, self.range_bytes.min(n - offset)))
    }
    pub fn memory_bound(&self) -> u64 {
        // V3 may explore every declared encoder; reserve the largest workspace
        // even while the current frame uses the inexpensive baseline.
        let workspace = match if self.format_version == 3 {
            Codec::Xz
        } else {
            self.codec
        } {
            Codec::Xz => 128 << 20,
            Codec::Zstd => 32 << 20,
            Codec::Gzip => 8 << 20,
            _ => 0,
        };
        let buffers = if self.format_version >= 2 {
            // At most 2P jobs: completed jobs retain raw+encoded data, while
            // active encoders may temporarily retain one additional copy.
            (5 * u64::from(self.workers) + 2) * self.range_bytes
        } else {
            (2 * u64::from(self.workers) + 1) * 2 * self.range_bytes
        };
        buffers + u64::from(self.workers) * workspace + (128 << 20)
    }
}

/// Fair round robin over contiguous regions. No allocation proportional to disk size.
pub struct Schedule {
    count: u64,
    regions: u64,
    round: u64,
    region: u64,
    from_ends: bool,
}
impl Schedule {
    pub fn new(count: u64, regions: u32) -> Self {
        Self {
            count,
            regions: u64::from(regions).min(count).max(1),
            round: 0,
            region: 0,
            from_ends: false,
        }
    }
    /// Visit front/rear regions alternately; each region remains ascending so
    /// every sealed file covers a single contiguous span.
    pub fn from_ends(count: u64, regions: u32) -> Self {
        Self {
            from_ends: true,
            ..Self::new(count, regions)
        }
    }
}
impl Iterator for Schedule {
    type Item = u64;
    fn next(&mut self) -> Option<u64> {
        loop {
            if self.round >= self.count / self.regions + u64::from(self.count % self.regions != 0) {
                return None;
            }
            let r = if self.from_ends {
                if self.region % 2 == 0 {
                    self.region / 2
                } else {
                    self.regions - 1 - self.region / 2
                }
            } else {
                self.region
            };
            let start = self.count * r / self.regions;
            let end = self.count * (r + 1) / self.regions;
            let id = start + self.round;
            self.region += 1;
            if self.region == self.regions {
                self.region = 0;
                self.round += 1;
            }
            if id < end {
                return Some(id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scheduling_is_complete_and_interleaved() {
        for n in 1..300 {
            for k in [1, 2, 3, 8, 64] {
                let ids: Vec<_> = Schedule::new(n, k).collect();
                let mut sorted = ids.clone();
                sorted.sort_unstable();
                assert_eq!(sorted, (0..n).collect::<Vec<_>>());
            }
        }
        assert_eq!(
            Schedule::new(12, 3).take(6).collect::<Vec<_>>(),
            vec![0, 4, 8, 1, 5, 9]
        );
    }
    #[test]
    fn limits_are_checked_before_allocation() {
        let mut p = Plan::default();
        assert!(p.validate(1).is_ok());
        p.workers = 0;
        assert!(p.validate(1024).is_err());
        p = Plan::default();
        p.codec = Codec::Xz;
        p.compression_level = 6;
        p.workers = 32;
        assert!(p.validate(1024).is_err());
        p = Plan::default();
        assert!(p.validate(u64::MAX).is_err());
    }
}
