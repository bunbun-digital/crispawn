// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! Every default B/C/P/codec combination is a named test. No devices or GUI.
use perfect_backup_core::{
    candidates::{all_supported_codec_settings, Candidate, CandidateGrid},
    plan::Plan,
    types::{self, Codec, NoEvents},
};
use perfect_backup_engine::{
    archive::CsvRows, backup, codec, io, restore, verify, Hooks, NoHooks, Result,
};
use std::{
    fs::{self, File},
    io::Read,
    path::Path,
    sync::{atomic::AtomicBool, Mutex, OnceLock},
};

const MIB: u64 = 1024 * 1024;
const PREFIX: usize = 16 * 1024 * 1024;
const SOURCE_BYTES: usize = 32 * 1024 * 1024 + 513;
static CASES: OnceLock<Vec<Candidate>> = OnceLock::new();
static DATA: OnceLock<Vec<u8>> = OnceLock::new();
// Bound memory even when cargo starts many test threads. Each case still uses
// its configured worker pool; this gate only serializes separate backup jobs.
static LARGE_CASE: Mutex<()> = Mutex::new(());
fn cases() -> &'static [Candidate] {
    CASES.get_or_init(|| {
        let template = Plan {
            container_bytes: 16 * MIB + 352,
            checkpoint_records: 2,
            regions: 3,
            memory_limit_bytes: 2 << 30,
            ..Plan::default()
        };
        CandidateGrid::default()
            .evaluate(&template, SOURCE_BYTES as u64)
            .unwrap()
    })
}
fn data() -> &'static [u8] {
    DATA.get_or_init(|| {
        let mut data = vec![0; SOURCE_BYTES];
        let motif = std::array::from_fn::<_, 256, _>(|i| i as u8);
        for block in data[..PREFIX].chunks_mut(256) {
            block.copy_from_slice(&motif);
        }
        let mut seed = 0xa7819321u32;
        for b in &mut data[2 * PREFIX..] {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            *b = seed as u8;
        }
        data[..16].copy_from_slice(b"PRIMARY-GPT-DATA");
        data[SOURCE_BYTES - 16..].copy_from_slice(b"BACKUP--GPT-DATA");
        data
    })
}
fn expected_eligible(codec: Codec, range: u64, workers: u32) -> bool {
    // Independent acceptance table for the documented 512 MiB memory budget.
    match (codec, range / MIB) {
        (Codec::Raw | Codec::Gzip, 8) => true,
        (Codec::Raw | Codec::Gzip, 16) => workers <= 4,
        (Codec::Zstd, 8) => workers <= 4,
        (Codec::Zstd, 16) => workers <= 2,
        (Codec::Xz, 8) => workers <= 2,
        (Codec::Xz, 16) => workers == 1,
        _ => false,
    }
}
struct RejectSourceRead;
impl Hooks for RejectSourceRead {
    fn at(&self, point: &'static str) -> Result<()> {
        assert_ne!(
            point, "source_read",
            "rejected candidate reached disk reading"
        );
        Ok(())
    }
}
fn compare_bytes(path: &Path, expected: &[u8], case: &str) {
    let mut file = File::open(path).unwrap();
    assert_eq!(
        file.metadata().unwrap().len(),
        expected.len() as u64,
        "{case}"
    );
    let mut buffer = vec![0; 65536];
    for (offset, chunk) in expected.chunks(buffer.len()).enumerate() {
        file.read_exact(&mut buffer[..chunk.len()]).unwrap();
        if let Some(i) = chunk.iter().zip(&buffer).position(|(a, b)| a != b) {
            panic!("{case}: restore mismatch at byte {}", offset * 65536 + i);
        }
    }
}
fn run_case(selected: Codec, level: i32, read: u64, range: u64, workers: u32) {
    let candidate = cases()
        .iter()
        .find(|c| {
            c.plan.codec == selected
                && c.plan.compression_level == level
                && c.plan.read_bytes == read
                && c.plan.range_bytes == range
                && c.plan.workers == workers
        })
        .expect("test combination missing from generated catalogue");
    let plan = &candidate.plan;
    let allowed = expected_eligible(selected, range, workers);
    assert!(
        candidate.is_eligible(),
        "{}: all matrix cells must be runnable with the test's 2 GiB budget",
        candidate.id
    );
    let constrained = Plan {
        memory_limit_bytes: 512 << 20,
        ..plan.clone()
    };
    assert_eq!(
        constrained.validate(SOURCE_BYTES as u64).is_ok(),
        allowed,
        "{}: 512 MiB eligibility disagrees with independent table",
        candidate.id
    );
    if !allowed {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.img");
        File::create(&source)
            .unwrap()
            .set_len(SOURCE_BYTES as u64)
            .unwrap();
        let archive = dir.path().join("must-not-exist");
        let e = backup::backup(
            &source,
            &archive,
            constrained,
            dir.path(),
            &NoEvents,
            &RejectSourceRead,
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert_eq!(e.code, "MEMORY_BUDGET", "{}", candidate.id);
        assert!(!archive.exists(), "{}", candidate.id);
    }
    let _guard = LARGE_CASE.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.img");
    let archive = dir.path().join("archive");
    let target = dir.path().join("restored.img");
    fs::write(&source, data()).unwrap();
    let report = backup::backup(
        &source,
        &archive,
        plan.clone(),
        dir.path(),
        &NoEvents,
        &NoHooks,
        &AtomicBool::new(false),
    )
    .unwrap_or_else(|e| panic!("{}: backup: {e}", candidate.id));
    assert!(report.dataset_committed, "{}", candidate.id);
    let stored_plan: Plan = io::read_json(&archive.join("plan.json")).unwrap();
    assert_eq!(stored_plan, *plan, "{}", candidate.id);
    let verified = verify::verify(&archive, dir.path(), &NoEvents)
        .unwrap_or_else(|e| panic!("{}: verify: {e}", candidate.id));
    assert_eq!(
        verified.range_count,
        plan.range_count(SOURCE_BYTES as u64).to_string()
    );
    let restored = restore::restore(&archive, &target, false, dir.path(), &NoEvents, &NoHooks)
        .unwrap_or_else(|e| panic!("{}: restore: {e}", candidate.id));
    assert!(restored.restored_ranges_verified);
    compare_bytes(&target, data(), &candidate.id);
    let mut ranges =
        CsvRows::<types::RangeEntry>::open(&archive.join("ranges.csv"), &types::RANGE_COLUMNS)
            .unwrap();
    let mut count = 0;
    while let Some(e) = ranges.next_row().unwrap() {
        let (offset, length) = plan.range(SOURCE_BYTES as u64, count).unwrap();
        assert_eq!(e.range_id, count, "{}", candidate.id);
        assert_eq!(
            (e.source_offset_bytes, e.source_length_bytes),
            (offset, length),
            "{}",
            candidate.id
        );
        assert_eq!(
            e.raw_sha256,
            types::hex(&types::hash(
                &data()[offset as usize..(offset + length) as usize]
            )),
            "{}",
            candidate.id
        );
        let expected = if offset >= PREFIX as u64 && offset + length <= 2 * PREFIX as u64 {
            Codec::Zero
        } else if length == range {
            selected
        } else {
            Codec::Raw
        };
        assert_eq!(
            e.codec, expected,
            "{}: wrong codec/fallback for range {count}",
            candidate.id
        );
        let actual_level = if matches!(expected, Codec::Raw | Codec::Zero) {
            0
        } else {
            level
        };
        assert_eq!(e.codec_params_id, expected.params(actual_level));
        if expected == Codec::Zero {
            assert_eq!(e.record_length_bytes, 160);
        } else if expected == Codec::Raw {
            assert_eq!(e.record_length_bytes, 160 + length);
        } else {
            assert!(e.record_length_bytes < 160 + length);
        }
        count += 1;
    }
    assert_eq!(count, plan.range_count(SOURCE_BYTES as u64));
    let mut files =
        CsvRows::<types::FileEntry>::open(&archive.join("files.csv"), &types::FILE_COLUMNS)
            .unwrap();
    let mut entries = vec![];
    while let Some(entry) = files.next_row().unwrap() {
        assert!(
            entry.stored_bytes <= plan.container_bytes,
            "{}",
            candidate.id
        );
        entries.push(entry);
    }
    assert!(!entries.is_empty());
    assert_eq!(
        entries.iter().map(|e| e.raw_bytes_total).sum::<u64>(),
        SOURCE_BYTES as u64
    );
    for entry in &entries[..entries.len() - 1] {
        assert!(
            entry.stored_bytes > plan.container_bytes - (range + 160),
            "{}: packing bound",
            candidate.id
        );
    }
    if selected == Codec::Raw {
        assert!(
            entries.len() >= 2,
            "{}: boundary fixture must seal more than one file",
            candidate.id
        );
    }
    run_current_case(plan);
}

/// Exercise the same complete matrix in the current format as well as v1.
/// Each final file must independently match standard BLAKE3 over its one
/// contiguous original span, including region tails and final short sectors.
fn run_current_case(legacy: &Plan) {
    let plan = Plan {
        format_version: 2,
        ..legacy.clone()
    };
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let archive = dir.path().join("archive");
    let target = dir.path().join("restored");
    fs::write(&source, data()).unwrap();
    backup::backup(
        &source,
        &archive,
        plan.clone(),
        dir.path(),
        &NoEvents,
        &NoHooks,
        &AtomicBool::new(false),
    )
    .unwrap();
    verify::verify(&archive, dir.path(), &NoEvents).unwrap();
    assert!(!archive.join("ranges.csv").exists());
    let mut rows = CsvRows::<perfect_backup_engine::v2::FileEntry>::open(
        &archive.join("files.csv"),
        &perfect_backup_engine::v2::FILE_COLUMNS,
    )
    .unwrap();
    let mut covered = 0;
    let mut files = 0;
    while let Some(entry) = rows.next_row().unwrap() {
        assert_eq!(entry.source_offset_bytes, covered);
        let end = covered + entry.source_length_bytes;
        assert_eq!(
            entry.raw_blake3,
            blake3::hash(&data()[covered as usize..end as usize])
                .to_hex()
                .as_str()
        );
        assert!(entry.stored_bytes <= plan.container_bytes && entry.stored_bytes <= 2_000_000_000);
        covered = end;
        files += 1;
    }
    assert_eq!(covered, SOURCE_BYTES as u64);
    assert!(files > 0);
    let report =
        restore::restore(&archive, &target, false, dir.path(), &NoEvents, &NoHooks).unwrap();
    assert!(report.restored_ranges_verified);
    compare_bytes(&target, data(), "v2-matrix");
}

macro_rules! case {
    ($name:ident,$codec:expr,$level:expr,$b:expr,$c:expr,$p:expr) => {
        #[test]
        fn $name() {
            run_case($codec, $level, $b, $c, $p);
        }
    };
}
macro_rules! axes {
    ($codec:expr,$level:expr) => {
        case!(b256k_c8m_p1, $codec, $level, 256 << 10, 8 * MIB, 1);
        case!(b256k_c8m_p2, $codec, $level, 256 << 10, 8 * MIB, 2);
        case!(b256k_c8m_p4, $codec, $level, 256 << 10, 8 * MIB, 4);
        case!(b256k_c8m_p8, $codec, $level, 256 << 10, 8 * MIB, 8);
        case!(b256k_c16m_p1, $codec, $level, 256 << 10, 16 * MIB, 1);
        case!(b256k_c16m_p2, $codec, $level, 256 << 10, 16 * MIB, 2);
        case!(b256k_c16m_p4, $codec, $level, 256 << 10, 16 * MIB, 4);
        case!(b256k_c16m_p8, $codec, $level, 256 << 10, 16 * MIB, 8);
        case!(b1m_c8m_p1, $codec, $level, 1 * MIB, 8 * MIB, 1);
        case!(b1m_c8m_p2, $codec, $level, 1 * MIB, 8 * MIB, 2);
        case!(b1m_c8m_p4, $codec, $level, 1 * MIB, 8 * MIB, 4);
        case!(b1m_c8m_p8, $codec, $level, 1 * MIB, 8 * MIB, 8);
        case!(b1m_c16m_p1, $codec, $level, 1 * MIB, 16 * MIB, 1);
        case!(b1m_c16m_p2, $codec, $level, 1 * MIB, 16 * MIB, 2);
        case!(b1m_c16m_p4, $codec, $level, 1 * MIB, 16 * MIB, 4);
        case!(b1m_c16m_p8, $codec, $level, 1 * MIB, 16 * MIB, 8);
        case!(b4m_c8m_p1, $codec, $level, 4 * MIB, 8 * MIB, 1);
        case!(b4m_c8m_p2, $codec, $level, 4 * MIB, 8 * MIB, 2);
        case!(b4m_c8m_p4, $codec, $level, 4 * MIB, 8 * MIB, 4);
        case!(b4m_c8m_p8, $codec, $level, 4 * MIB, 8 * MIB, 8);
        case!(b4m_c16m_p1, $codec, $level, 4 * MIB, 16 * MIB, 1);
        case!(b4m_c16m_p2, $codec, $level, 4 * MIB, 16 * MIB, 2);
        case!(b4m_c16m_p4, $codec, $level, 4 * MIB, 16 * MIB, 4);
        case!(b4m_c16m_p8, $codec, $level, 4 * MIB, 16 * MIB, 8);
    };
}
macro_rules! codec_suite {
    ($name:ident,$codec:expr,$level:expr) => {
        mod $name {
            use super::*;
            axes!($codec, $level);
        }
    };
}
codec_suite!(raw_0, Codec::Raw, 0);
codec_suite!(zstd_1, Codec::Zstd, 1);
codec_suite!(zstd_3, Codec::Zstd, 3);
codec_suite!(zstd_6, Codec::Zstd, 6);
codec_suite!(xz_0, Codec::Xz, 0);
codec_suite!(xz_1, Codec::Xz, 1);
codec_suite!(xz_3, Codec::Xz, 3);
codec_suite!(xz_6, Codec::Xz, 6);
codec_suite!(gzip_1, Codec::Gzip, 1);
codec_suite!(gzip_6, Codec::Gzip, 6);

#[test]
fn every_supported_codec_level_handles_zero_compressible_and_incompressible_ranges() {
    let mut seed = 918273u32;
    let noisy = (0..32768)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as u8
        })
        .collect::<Vec<_>>();
    for setting in all_supported_codec_settings() {
        for (name, bytes) in [
            ("zero", vec![0; 32768]),
            ("compressible", (0..32768).map(|i| (i % 31) as u8).collect()),
            ("incompressible", noisy.clone()),
        ] {
            let encoded = codec::encode(bytes.clone(), setting.codec, setting.level).unwrap();
            let decoded = codec::decode(encoded.codec, &encoded.payload, bytes.len()).unwrap();
            assert_eq!(decoded, bytes, "{:?} {name}", setting);
            if name == "zero" {
                assert_eq!(encoded.codec, Codec::Zero);
            } else if setting.codec == Codec::Raw
                || name == "incompressible"
                || (setting.codec == Codec::Gzip && setting.level == 0)
            {
                assert_eq!(encoded.codec, Codec::Raw);
            } else {
                assert_eq!(encoded.codec, setting.codec);
            }
        }
    }
}
#[test]
fn each_read_block_size_is_used_and_covers_full_ranges_and_short_tail() {
    struct Count(Mutex<Vec<(u64, usize)>>);
    impl io::Reader for Count {
        fn read_at(&self, b: &mut [u8], offset: u64) -> std::io::Result<usize> {
            self.0.lock().unwrap().push((offset, b.len()));
            b.fill(0x5a);
            Ok(b.len())
        }
    }
    for block in [256 << 10, 1 << 20, 4 << 20] {
        for range in [8 * MIB, 16 * MIB] {
            let reader = Count(Mutex::new(vec![]));
            let mut bytes = vec![0; (range + 513) as usize];
            let base = (1u64 << 32) + 512;
            io::read_exact_at(&reader, &mut bytes, base, block).unwrap();
            assert!(bytes.iter().all(|b| *b == 0x5a));
            let calls = reader.0.lock().unwrap();
            assert_eq!(calls.len() as u64, (range + 513).div_ceil(block));
            let mut offset = base;
            for &(at, len) in calls.iter() {
                assert_eq!(at, offset);
                assert!(len as u64 <= block);
                offset += len as u64;
            }
            assert_eq!(offset, base + range + 513);
            assert_eq!(calls.last().unwrap().1, 513);
        }
    }
}
