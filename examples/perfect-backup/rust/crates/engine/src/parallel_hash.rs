// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! Standard BLAKE3 using an explicitly bounded CPU pool, shared with the pipeline.
//!
//! `update_rayon` handles chunk counters and tree reduction. Hashing each input
//! buffer separately and concatenating final digests would not produce BLAKE3.
use crate::{Error, Result};
use perfect_backup_core::error::require;

pub struct CpuPool {
    pool: rayon::ThreadPool,
}

impl CpuPool {
    pub fn new(workers: u32) -> Result<Self> {
        require(
            (1..=32).contains(&workers),
            "INVALID_PLAN",
            "CPU pool must contain between 1 and 32 workers",
        )?;
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(workers as usize)
            .thread_name(|i| format!("pb-cpu-{i}"))
            .build()
            .map_err(|e| Error::new("CPU_POOL", e.to_string()))?;
        Ok(Self { pool })
    }

    pub fn threads(&self) -> usize {
        self.pool.current_num_threads()
    }

    pub fn pool(&self) -> &rayon::ThreadPool {
        &self.pool
    }

    pub fn update(&self, hash: &mut blake3::Hasher, bytes: &[u8]) {
        self.pool.install(|| {
            hash.update_rayon(bytes);
        });
    }

    pub fn hash(&self, bytes: &[u8]) -> [u8; 32] {
        let mut hash = blake3::Hasher::new();
        self.update(&mut hash, bytes);
        *hash.finalize().as_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_blake3_matches_across_boundaries_and_pool_sizes() {
        let bytes = (0..(2 << 20) + 513)
            .map(|i| ((i * 31 + i / 251) % 256) as u8)
            .collect::<Vec<_>>();
        for workers in [1, 2, 4] {
            let pool = CpuPool::new(workers).unwrap();
            for length in [0, 1, 63, 64, 65, 1023, 1024, 1025, 65536, bytes.len()] {
                let input = &bytes[..length];
                let expected = *blake3::hash(input).as_bytes();
                assert_eq!(pool.hash(input), expected);
                for chunk in [777, 1024, 8192, 65536] {
                    let mut hash = blake3::Hasher::new();
                    for part in input.chunks(chunk) {
                        pool.update(&mut hash, part);
                    }
                    assert_eq!(*hash.finalize().as_bytes(), expected);
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn one_file_hash_consumes_cpu_on_multiple_workers() {
        fn thread_cpu_ns() -> u64 {
            let mut value = std::mem::MaybeUninit::<libc::timespec>::uninit();
            // SAFETY: clock_gettime writes a valid timespec to owned storage.
            let status =
                unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, value.as_mut_ptr()) };
            assert_eq!(status, 0);
            // SAFETY: a successful clock_gettime initialized both fields.
            let value = unsafe { value.assume_init() };
            value.tv_sec as u64 * 1_000_000_000 + value.tv_nsec as u64
        }
        let pool = CpuPool::new(4).unwrap();
        let bytes = vec![0x6b; 64 << 20];
        let before = pool.pool().broadcast(|_| thread_cpu_ns());
        for _ in 0..32 {
            let mut hash = blake3::Hasher::new();
            pool.update(&mut hash, &bytes);
            std::hint::black_box(hash.finalize());
        }
        let after = pool.pool().broadcast(|_| thread_cpu_ns());
        let active = after
            .iter()
            .zip(before)
            .filter(|(a, b)| **a - *b > 1_000_000)
            .count();
        assert!(
            active >= 2,
            "expected actual multiworker CPU use, got {active}"
        );
    }
}
