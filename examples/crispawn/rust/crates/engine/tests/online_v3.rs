// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
use crispawn_core::{
    plan::{Plan, Schedule},
    types::{Codec, NoEvents},
};
use crispawn_engine::{
    archive::CsvRows,
    backup, restore,
    v2::{FileEntry, V3_COLUMNS},
    v2_format::{self, Frame},
    verify, Error, Hooks, NoHooks, Result,
};
use std::{
    fs,
    sync::atomic::{AtomicBool, Ordering},
};

fn plan() -> Plan {
    Plan {
        segmented_storage: false,
        format_version: 3,
        range_bytes: 64 << 10,
        read_bytes: 64 << 10,
        container_bytes: 256 << 10,
        workers: 2,
        regions: 4,
        checkpoint_records: 2,
        codec: Codec::Zstd,
        compression_level: 3,
        memory_limit_bytes: 1 << 30,
    }
}
fn fixture() -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut state = 0x1926_1122_7744_2103u64;
    for n in 0..24 {
        for i in 0..(64 << 10) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            bytes.push(match n % 3 {
                0 => 0,
                1 => (i % 13) as u8,
                _ => state as u8,
            });
        }
    }
    bytes.extend_from_slice(&[0xa1; 513]);
    bytes
}
#[test]
fn productive_v3_records_actual_codecs_and_restores_exact_original_spans() {
    let dir = tempfile::tempdir().unwrap();
    let raw = fixture();
    let source = dir.path().join("source");
    fs::write(&source, &raw).unwrap();
    let archive = dir.path().join("archive");
    backup::backup(
        &source,
        &archive,
        plan(),
        dir.path(),
        &NoEvents,
        &NoHooks,
        &AtomicBool::new(false),
    )
    .unwrap();
    let mut rows = CsvRows::<FileEntry>::open(&archive.join("files.csv"), &V3_COLUMNS).unwrap();
    let mut covered = 0;
    let mut codecs = std::collections::BTreeSet::new();
    while let Some(entry) = rows.next_row().unwrap() {
        assert_eq!(entry.source_offset_bytes, covered);
        covered += entry.source_length_bytes;
        assert!(entry.stored_bytes <= 256 << 10);
        let expected = blake3::hash(&raw[entry.source_offset_bytes as usize..covered as usize]);
        assert_eq!(entry.raw_blake3, expected.to_hex().to_string());
        let profiles = entry.compression_profiles.unwrap();
        assert!(v2_format::valid_profiles(&profiles));
        codecs.extend(profiles.split(';').map(str::to_owned));
    }
    assert_eq!(covered, raw.len() as u64);
    assert!(codecs.contains("zero:0") && codecs.contains("raw:0") && codecs.contains("zstd:3"));
    verify::verify(&archive, dir.path(), &NoEvents).unwrap();
    let target = dir.path().join("target");
    restore::restore(&archive, &target, false, dir.path(), &NoEvents, &NoHooks).unwrap();
    assert_eq!(fs::read(target).unwrap(), raw);
}
#[test]
fn interrupted_v3_reconstructs_codec_sets_and_blake3_before_resuming() {
    struct Fail(AtomicBool);
    impl Hooks for Fail {
        fn at(&self, p: &'static str) -> Result<()> {
            if p == "after_checkpoint" && !self.0.swap(true, Ordering::SeqCst) {
                Err(Error::new("INJECTED_IO_ERROR", "one durable checkpoint"))
            } else {
                Ok(())
            }
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let raw = fixture();
    let source = dir.path().join("source");
    fs::write(&source, &raw).unwrap();
    let archive = dir.path().join("archive");
    assert_eq!(
        backup::backup(
            &source,
            &archive,
            plan(),
            dir.path(),
            &NoEvents,
            &Fail(AtomicBool::new(false)),
            &AtomicBool::new(false)
        )
        .unwrap_err()
        .code,
        "INJECTED_IO_ERROR"
    );
    backup::resume(
        &source,
        &archive,
        dir.path(),
        &NoEvents,
        &NoHooks,
        &AtomicBool::new(false),
    )
    .unwrap();
    let target = dir.path().join("target");
    restore::restore(&archive, &target, false, dir.path(), &NoEvents, &NoHooks).unwrap();
    assert_eq!(fs::read(target).unwrap(), raw);
}
#[test]
fn versioned_frame_vocabulary_rejects_unknown_levels_and_cross_version_headers() {
    let p = plan();
    for (codec, levels) in [
        (Codec::Zstd, vec![1, 3, 6, 9]),
        (Codec::Xz, vec![0, 3, 6]),
        (Codec::Gzip, vec![1, 6, 9]),
    ] {
        for level in levels {
            let frame = Frame {
                codec,
                level,
                raw_bytes: 8192,
                payload_bytes: 128,
            };
            assert!(Frame::decode(&frame.encode(3), &p).is_ok());
            assert!(Frame::decode(&frame.encode(2), &p).is_err());
        }
    }
    // xz:6 is written only while a writer backlog holds xz (schema-2 archives).
    let invalid = Frame {
        codec: Codec::Xz,
        level: 9,
        raw_bytes: 8192,
        payload_bytes: 128,
    };
    assert!(Frame::decode(&invalid.encode(3), &p).is_err());
    assert!(v2_format::valid_profiles("xz:6"));
    for value in ["", "xz:9", "raw:00", "zero:0;raw:0", "zstd:3;zstd:3"] {
        assert!(!v2_format::valid_profiles(value));
    }
}
#[test]
fn front_and_rear_region_order_has_no_gaps_duplicates_or_disk_sized_allocation() {
    for n in 1..129 {
        for k in [1, 2, 3, 8, 64] {
            let mut ids = Schedule::from_ends(n, k).collect::<Vec<_>>();
            ids.sort_unstable();
            assert_eq!(ids, (0..n).collect::<Vec<_>>());
        }
    }
    assert_eq!(
        Schedule::from_ends(16, 4).take(4).collect::<Vec<_>>(),
        vec![0, 12, 4, 8]
    );
}

#[test]
fn productive_compression_continues_while_destination_checkpoint_is_waiting() {
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};
    struct Overlap {
        queued: AtomicUsize,
        waiting: AtomicBool,
        observed: AtomicBool,
    }
    impl Hooks for Overlap {
        fn at(&self, point: &'static str) -> Result<()> {
            if point == "before_data_sync" && !self.waiting.swap(true, Ordering::SeqCst) {
                let before = self.queued.load(Ordering::SeqCst);
                let end = Instant::now() + Duration::from_secs(5);
                while self.queued.load(Ordering::SeqCst) < before + 2 && Instant::now() < end {
                    std::thread::sleep(Duration::from_millis(2));
                }
                self.observed.store(
                    self.queued.load(Ordering::SeqCst) >= before + 2,
                    Ordering::SeqCst,
                );
            }
            Ok(())
        }
        fn span(&self, point: &'static str, _offset: u64, _length: u64) -> Result<()> {
            if point == "compressed_queued" && self.queued.fetch_add(1, Ordering::SeqCst) == 0 {
                let end = Instant::now() + Duration::from_secs(5);
                while !self.waiting.load(Ordering::SeqCst) && Instant::now() < end {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            Ok(())
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let raw = fixture();
    fs::write(&source, &raw).unwrap();
    let hook = Overlap {
        queued: AtomicUsize::new(0),
        waiting: AtomicBool::new(false),
        observed: AtomicBool::new(false),
    };
    let mut p = plan();
    p.checkpoint_records = 1;
    let archive = dir.path().join("archive");
    backup::backup(
        &source,
        &archive,
        p,
        dir.path(),
        &NoEvents,
        &hook,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(
        hook.observed.load(Ordering::SeqCst),
        "producer stopped while destination waited for sync"
    );
    let target = dir.path().join("target");
    restore::restore(&archive, &target, false, dir.path(), &NoEvents, &NoHooks).unwrap();
    assert_eq!(fs::read(target).unwrap(), raw);
}

#[test]
fn producer_and_destination_failures_release_v3_pipeline_without_commit() {
    use std::time::{Duration, Instant};
    struct Fail {
        point: &'static str,
        fired: AtomicBool,
    }
    impl Hooks for Fail {
        fn at(&self, p: &'static str) -> Result<()> {
            if p == self.point && !self.fired.swap(true, Ordering::SeqCst) {
                Err(Error::new("INJECTED_IO_ERROR", "bounded pipeline failure"))
            } else {
                Ok(())
            }
        }
    }
    for point in [
        "source_read",
        "codec_encode",
        "before_data_write",
        "before_data_sync",
        "after_checkpoint",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let raw = fixture();
        fs::write(&source, &raw).unwrap();
        let archive = dir.path().join("archive");
        let start = Instant::now();
        let error = backup::backup(
            &source,
            &archive,
            plan(),
            dir.path(),
            &NoEvents,
            &Fail {
                point,
                fired: AtomicBool::new(false),
            },
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert_eq!(error.code, "INJECTED_IO_ERROR", "{point}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "pipeline failure did not drain: {point}"
        );
        assert!(!archive.join("COMMITTED").exists());
        backup::resume(
            &source,
            &archive,
            dir.path(),
            &NoEvents,
            &NoHooks,
            &AtomicBool::new(false),
        )
        .unwrap();
        let target = dir.path().join("target");
        restore::restore(&archive, &target, false, dir.path(), &NoEvents, &NoHooks).unwrap();
        assert_eq!(fs::read(target).unwrap(), raw);
    }
}

#[test]
fn runtime_parallelism_bounds_real_reads_not_just_the_result_queue() {
    use std::sync::atomic::AtomicUsize;
    struct Meter {
        active: AtomicUsize,
        peak: AtomicUsize,
    }
    impl Hooks for Meter {
        fn at(&self, point: &'static str) -> Result<()> {
            if point == "source_read" {
                let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(active, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(50));
            } else if point == "after_codec_encode" {
                self.active.fetch_sub(1, Ordering::SeqCst);
            }
            Ok(())
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    fs::write(&source, fixture()).unwrap();
    let mut p = plan();
    p.workers = 16;
    p.memory_limit_bytes = 4 << 30;
    let cpus = std::thread::available_parallelism()
        .map(|v| v.get() as u32)
        .unwrap_or(1);
    let initial = cpus.saturating_mul(2).min(8) as usize;
    let meter = Meter {
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
    };
    backup::backup(
        &source,
        &dir.path().join("archive"),
        p,
        dir.path(),
        &NoEvents,
        &meter,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(meter.active.load(Ordering::SeqCst), 0);
    let peak = meter.peak.load(Ordering::SeqCst);
    assert!(
        peak <= initial,
        "observed {peak} read/encode workers; selected starting parallelism is {initial}"
    );
    assert!(peak >= 2);
}
