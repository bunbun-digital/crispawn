// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
use crispawn_core::{
    plan::{Plan, MAX_BACKUP_FILE_BYTES},
    types::{self, Codec, EventSink, NoEvents, Snapshot},
};
use crispawn_engine::{
    archive::CsvRows,
    backup, io, restore,
    v2::{FileEntry, FILE_COLUMNS},
    verify, Error, Hooks, NoHooks, Result,
};
use std::{
    fs,
    os::unix::fs::FileExt,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Mutex,
    },
};

struct Fixture {
    dir: tempfile::TempDir,
    source: PathBuf,
    archive: PathBuf,
    raw: Vec<u8>,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let archive = dir.path().join("archive");
        let mut seed = 919191u32;
        let mut raw = (0..(4096 * 97 + 137))
            .map(|i| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                match i / 4096 % 4 {
                    0 => 0,
                    1 => (i % 17) as u8,
                    _ => seed as u8,
                }
            })
            .collect::<Vec<_>>();
        raw[..16].copy_from_slice(b"PRIMARY-GPT-DATA");
        let n = raw.len();
        raw[n - 16..].copy_from_slice(b"BACKUP--GPT-DATA");
        fs::write(&source, &raw).unwrap();
        Self {
            dir,
            source,
            archive,
            raw,
        }
    }
    fn plan(codec: Codec) -> Plan {
        Plan {
            range_bytes: 4096,
            read_bytes: 777,
            container_bytes: 16 * 1024,
            workers: 2,
            regions: 7,
            checkpoint_records: 2,
            codec,
            compression_level: if codec == Codec::Raw { 0 } else { 3 },
            memory_limit_bytes: 2 << 30,
            ..Plan::current()
        }
    }
    fn make(
        &self,
        plan: Plan,
        hooks: &dyn Hooks,
        events: &dyn EventSink,
    ) -> Result<verify::Report> {
        backup::backup(
            &self.source,
            &self.archive,
            plan,
            self.dir.path(),
            events,
            hooks,
            &AtomicBool::new(false),
        )
    }
    fn entries(&self) -> Vec<FileEntry> {
        let mut rows = CsvRows::open(&self.archive.join("files.csv"), &FILE_COLUMNS).unwrap();
        let mut entries = vec![];
        while let Some(e) = rows.next_row().unwrap() {
            entries.push(e);
        }
        entries
    }
    fn restore(&self) {
        let target = self.dir.path().join("restored");
        let report = restore::restore(
            &self.archive,
            &target,
            false,
            self.dir.path(),
            &NoEvents,
            &NoHooks,
        )
        .unwrap();
        assert!(report.restored_ranges_verified);
        assert_eq!(fs::read(target).unwrap(), self.raw);
    }
}
#[derive(Default)]
struct Events(Mutex<Vec<Snapshot>>);
impl EventSink for Events {
    fn emit(&self, s: Snapshot) {
        self.0.lock().unwrap().push(s);
    }
}
struct Fail {
    point: &'static str,
    nth: usize,
    count: AtomicUsize,
}
impl Fail {
    fn new(point: &'static str, nth: usize) -> Self {
        Self {
            point,
            nth,
            count: AtomicUsize::new(0),
        }
    }
}
impl Hooks for Fail {
    fn at(&self, p: &'static str) -> Result<()> {
        if p == self.point && self.count.fetch_add(1, Ordering::SeqCst) + 1 == self.nth {
            Err(Error::new("INJECTED_IO_ERROR", p))
        } else {
            Ok(())
        }
    }
}

#[test]
fn all_codecs_workers_contiguous_files_have_canonical_original_blake3_and_restore() {
    for codec in [Codec::Raw, Codec::Zstd, Codec::Xz, Codec::Gzip] {
        for workers in [1, 2, 4] {
            let f = Fixture::new();
            let mut plan = Fixture::plan(codec);
            plan.workers = workers;
            f.make(plan.clone(), &NoHooks, &NoEvents).unwrap();
            let meta: types::Metadata = io::read_json(&f.archive.join("metadata.json")).unwrap();
            assert_eq!(meta.format_version, 2);
            assert_eq!(meta.hash_algorithm, "blake3");
            assert!(!f.archive.join("ranges.csv").exists());
            let entries = f.entries();
            let mut covered = 0;
            for entry in &entries {
                assert_eq!(entry.source_offset_bytes, covered);
                let end = covered + entry.source_length_bytes;
                assert_eq!(
                    entry.raw_blake3,
                    blake3::hash(&f.raw[covered as usize..end as usize])
                        .to_hex()
                        .as_str()
                );
                assert_eq!(
                    entry.stored_bytes,
                    fs::metadata(f.archive.join(&entry.relative_path))
                        .unwrap()
                        .len()
                );
                assert!(
                    entry.stored_bytes <= plan.container_bytes
                        && entry.stored_bytes <= MAX_BACKUP_FILE_BYTES
                );
                let count = plan.range_count(f.raw.len() as u64);
                let regions = u64::from(plan.regions).min(count);
                let region = (((covered / plan.range_bytes + 1) * regions - 1) / count) as u64;
                let region_end =
                    ((count * (region + 1) / regions) * plan.range_bytes).min(f.raw.len() as u64);
                assert!(end <= region_end);
                if end != region_end {
                    assert!(entry.stored_bytes > plan.container_bytes - (plan.range_bytes + 32));
                }
                covered = end;
            }
            assert_eq!(covered, f.raw.len() as u64);
            verify::verify(&f.archive, f.dir.path(), &NoEvents).unwrap();
            f.restore();
            // Final journal contains only final file digests, never old prefixes.
            for line in fs::read_to_string(f.archive.join("journal.jsonl"))
                .unwrap()
                .lines()
            {
                let v: serde_json::Value = serde_json::from_str(line).unwrap();
                if v["change"]["kind"] == "checkpoint" {
                    let p = &v["change"]["prefix"];
                    let id = p["file_id"].as_str().unwrap().parse::<u64>().unwrap();
                    let e = entries.iter().find(|e| e.file_id == id).unwrap();
                    assert_eq!(p["raw_blake3"], e.raw_blake3);
                    assert_eq!(p["raw_bytes"], e.source_length_bytes.to_string());
                }
            }
        }
    }
}
#[test]
fn failures_at_durability_boundaries_resume_and_verify_without_losing_source_bytes() {
    for point in [
        "before_journal_write",
        "after_journal_write",
        "after_journal_sync",
        "before_data_write",
        "after_data_write",
        "before_data_sync",
        "after_data_sync",
        "after_checkpoint",
        "before_footer",
        "after_footer_sync",
        "after_data_rename",
        "after_seal",
        "before_indices",
        "after_indices",
        "before_commit",
    ] {
        for nth in [1, 3] {
            eprintln!("resume boundary {point}/{nth}");
            let f = Fixture::new();
            let events = Events::default();
            let result = f.make(Fixture::plan(Codec::Zstd), &Fail::new(point, nth), &events);
            if result.is_ok() {
                continue;
            }
            assert_eq!(
                result.unwrap_err().code,
                "INJECTED_IO_ERROR",
                "{point}/{nth}"
            );
            assert!(!f.archive.join("COMMITTED").exists(), "{point}/{nth}");
            assert!(!events
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|s| s.phase == "COMPLETE"));
            backup::resume(
                &f.source,
                &f.archive,
                f.dir.path(),
                &NoEvents,
                &NoHooks,
                &AtomicBool::new(false),
            )
            .unwrap_or_else(|e| panic!("resume {point}/{nth}: {e}"));
            verify::verify(&f.archive, f.dir.path(), &NoEvents).unwrap();
            f.restore();
        }
    }
}
#[test]
fn cap_is_decimal_two_gigabytes_and_checked_before_creating_output() {
    let f = Fixture::new();
    let mut p = Fixture::plan(Codec::Raw);
    p.container_bytes = MAX_BACKUP_FILE_BYTES;
    p.validate(f.raw.len() as u64).unwrap();
    p.container_bytes += 1;
    assert_eq!(
        f.make(p, &NoHooks, &NoEvents).unwrap_err().code,
        "INVALID_PLAN"
    );
    assert!(!f.archive.exists());
}
#[test]
fn restore_overwrites_zero_regions_preserves_larger_target_tail_and_rejects_readback_corruption() {
    for codec in [Codec::Raw, Codec::Zstd, Codec::Xz, Codec::Gzip] {
        let f = Fixture::new();
        f.make(Fixture::plan(codec), &NoHooks, &NoEvents).unwrap();
        let target = f.dir.path().join("target");
        let mut expected = f.raw.clone();
        expected.extend_from_slice(&[0x3c; 8192]);
        let mut initial = vec![0xa5; f.raw.len()];
        initial.extend_from_slice(&[0x3c; 8192]);
        fs::write(&target, &initial).unwrap();
        restore::restore(&f.archive, &target, true, f.dir.path(), &NoEvents, &NoHooks).unwrap();
        assert_eq!(fs::read(&target).unwrap(), expected);
        struct Corrupt {
            path: PathBuf,
            offset: u64,
            once: AtomicBool,
            gate: Mutex<()>,
        }
        impl Hooks for Corrupt {
            fn span(&self, point: &'static str, offset: u64, length: u64) -> Result<()> {
                // Concurrent confirmation may begin before later spans are
                // written. Inject corruption when this span is actually read,
                // rather than corrupting future data which will be overwritten.
                if self.offset >= offset && self.offset - offset < length {
                    self.at(point)
                } else {
                    Ok(())
                }
            }
            fn at(&self, p: &'static str) -> Result<()> {
                if p == "restore_verify_read" {
                    let _guard = self.gate.lock().unwrap();
                    if !self.once.swap(true, Ordering::SeqCst) {
                        let file = fs::OpenOptions::new()
                            .read(true)
                            .write(true)
                            .open(&self.path)?;
                        let mut b = [0];
                        file.read_at(&mut b, self.offset)?;
                        b[0] ^= 0x80;
                        file.write_at(&b, self.offset)?;
                        file.sync_all()?;
                    }
                }
                Ok(())
            }
        }
        for offset in [0, 4096, 123456, f.raw.len() as u64 - 1] {
            let events = Events::default();
            let hook = Corrupt {
                path: target.clone(),
                offset,
                once: AtomicBool::new(false),
                gate: Mutex::new(()),
            };
            let error = restore::restore(&f.archive, &target, true, f.dir.path(), &events, &hook)
                .unwrap_err();
            assert_eq!(error.code, "RESTORE_HASH_MISMATCH");
            assert!(!events
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|s| s.phase == "COMPLETE"));
        }
        let events = Events::default();
        let error = restore::restore(
            &f.archive,
            &target,
            true,
            f.dir.path(),
            &events,
            &Fail::new("restore_verify_read", 1),
        )
        .unwrap_err();
        assert_eq!(error.code, "INJECTED_IO_ERROR");
        assert!(!events
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.phase == "COMPLETE"));
    }
}
#[test]
fn archive_corruption_is_rejected_before_existing_target_is_changed() {
    let f = Fixture::new();
    f.make(Fixture::plan(Codec::Raw), &NoHooks, &NoEvents)
        .unwrap();
    let target = f.dir.path().join("target");
    fs::write(&target, vec![0xa5; f.raw.len()]).unwrap();
    let entry = f.entries().into_iter().next().unwrap();
    let path = f.archive.join(entry.relative_path);
    let mut bytes = fs::read(&path).unwrap();
    let at = 64 + 32;
    bytes[at] ^= 1;
    fs::write(path, bytes).unwrap();
    assert!(
        restore::restore(&f.archive, &target, true, f.dir.path(), &NoEvents, &NoHooks).is_err()
    );
    assert!(fs::read(target).unwrap().iter().all(|b| *b == 0xa5));
}

#[test]
fn verify_rejects_bad_frame_lengths_headers_footer_and_unknown_files() {
    for kind in [
        "payload-overflow",
        "raw-overflow",
        "bad-codec",
        "reserved-header",
        "truncated-footer",
        "trailing-data",
        "unknown-file",
    ] {
        let f = Fixture::new();
        f.make(Fixture::plan(Codec::Raw), &NoHooks, &NoEvents)
            .unwrap();
        let entry = f.entries().into_iter().next().unwrap();
        let path = f.archive.join(&entry.relative_path);
        let mut bytes = fs::read(&path).unwrap();
        match kind {
            "payload-overflow" => {
                bytes[64 + 24..64 + 28].copy_from_slice(&u32::MAX.to_le_bytes());
                let crc = crispawn_engine::v2_format::crc32(&bytes[64..64 + 28]);
                bytes[64 + 28..64 + 32].copy_from_slice(&crc.to_le_bytes());
            }
            "raw-overflow" => {
                bytes[64 + 16..64 + 24].copy_from_slice(&u64::MAX.to_le_bytes());
                let crc = crispawn_engine::v2_format::crc32(&bytes[64..64 + 28]);
                bytes[64 + 28..64 + 32].copy_from_slice(&crc.to_le_bytes());
            }
            "bad-codec" => {
                bytes[64 + 8..64 + 12].copy_from_slice(&99u32.to_le_bytes());
                let crc = crispawn_engine::v2_format::crc32(&bytes[64..64 + 28]);
                bytes[64 + 28..64 + 32].copy_from_slice(&crc.to_le_bytes());
            }
            "reserved-header" => {
                bytes[48] = 1;
                let crc = crispawn_engine::v2_format::crc32(&bytes[..60]);
                bytes[60..64].copy_from_slice(&crc.to_le_bytes());
            }
            "truncated-footer" => {
                bytes.pop();
            }
            "trailing-data" => bytes.push(0),
            "unknown-file" => fs::write(f.archive.join("data/unknown"), b"preserve me").unwrap(),
            _ => unreachable!(),
        }
        fs::write(path, bytes).unwrap();
        let events = Events::default();
        assert!(
            verify::verify_controlled(&f.archive, f.dir.path(), &events, &AtomicBool::new(false))
                .is_err(),
            "{kind}"
        );
        assert!(!events
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.phase == "COMPLETE"));
    }
}

#[test]
fn resume_rejects_changed_source_unknown_data_hardlinks_and_corrupt_journal_without_mutation() {
    for kind in ["source", "unknown", "hardlink", "journal"] {
        let f = Fixture::new();
        assert!(f
            .make(
                Fixture::plan(Codec::Zstd),
                &Fail::new("after_checkpoint", 1),
                &NoEvents
            )
            .is_err());
        match kind {
            "source" => {
                let file = fs::OpenOptions::new().write(true).open(&f.source).unwrap();
                file.write_at(b"CHANGED!", 0).unwrap();
                file.sync_all().unwrap();
            }
            "unknown" => fs::write(f.archive.join("data/foreign-file"), b"preserve me").unwrap(),
            "hardlink" => {
                let part = fs::read_dir(f.archive.join("data"))
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .path();
                fs::hard_link(part, f.dir.path().join("linked-partial")).unwrap();
            }
            "journal" => {
                let mut bytes = fs::read(f.archive.join("journal.jsonl")).unwrap();
                bytes[8] ^= 1;
                fs::write(f.archive.join("journal.jsonl"), bytes).unwrap();
            }
            _ => unreachable!(),
        }
        let journal = fs::read(f.archive.join("journal.jsonl")).unwrap();
        let saved: Vec<_> = fs::read_dir(f.archive.join("data"))
            .unwrap()
            .map(|e| {
                let p = e.unwrap().path();
                let b = fs::read(&p).unwrap();
                (p, b)
            })
            .collect();
        assert!(
            backup::resume(
                &f.source,
                &f.archive,
                f.dir.path(),
                &NoEvents,
                &NoHooks,
                &AtomicBool::new(false)
            )
            .is_err(),
            "{kind}"
        );
        assert_eq!(journal, fs::read(f.archive.join("journal.jsonl")).unwrap());
        for (path, bytes) in saved {
            assert_eq!(fs::read(path).unwrap(), bytes, "{kind}");
        }
        assert!(!f.archive.join("COMMITTED").exists());
    }
}

#[test]
fn resume_ignores_only_incomplete_final_journal_tail_and_validates_prefix_first() {
    let f = Fixture::new();
    assert!(f
        .make(
            Fixture::plan(Codec::Zstd),
            &Fail::new("after_checkpoint", 1),
            &NoEvents
        )
        .is_err());
    use std::io::Write;
    fs::OpenOptions::new()
        .append(true)
        .open(f.archive.join("journal.jsonl"))
        .unwrap()
        .write_all(b"{\"partial")
        .unwrap();
    backup::resume(
        &f.source,
        &f.archive,
        f.dir.path(),
        &NoEvents,
        &NoHooks,
        &AtomicBool::new(false),
    )
    .unwrap();
    f.restore();
}

#[test]
fn verification_read_progress_never_confirms_an_unfinished_file_digest() {
    let f = Fixture::new();
    let events = Events::default();
    let mut plan = Fixture::plan(Codec::Zstd);
    plan.container_bytes = 2_000_000_000;
    plan.regions = 1;
    f.make(plan, &NoHooks, &events).unwrap();
    assert_eq!(f.entries().len(), 1);
    let events = events.0.lock().unwrap();
    let mut reads = 0;
    assert!(!events.iter().any(|s| s.phase == "VERIFYING"));
    for snapshot in events.iter().filter(|s| s.phase == "COPYING") {
        assert!(snapshot.verification_read_raw_bytes >= reads);
        reads = snapshot.verification_read_raw_bytes;
        if snapshot.verification_read_raw_bytes < f.raw.len() as u64 {
            assert_eq!(snapshot.verified_raw_bytes, 0);
        }
    }
    assert_eq!(reads, f.raw.len() as u64);
    assert_eq!(
        events.last().unwrap().verified_raw_bytes,
        f.raw.len() as u64
    );
}

#[test]
fn index_semantics_and_per_file_blake3_are_checked_even_with_recomputed_metadata_checksums() {
    for kind in [
        "gap",
        "duplicate",
        "length-overflow",
        "oversized-file",
        "wrong-hash",
    ] {
        let f = Fixture::new();
        f.make(Fixture::plan(Codec::Raw), &NoHooks, &NoEvents)
            .unwrap();
        let mut entries = f.entries();
        match kind {
            "gap" => entries[0].source_offset_bytes += 4096,
            "duplicate" => {
                entries[1].file_id = entries[0].file_id;
                entries[1].relative_path = entries[0].relative_path.clone();
            }
            "length-overflow" => entries[0].source_length_bytes = u64::MAX,
            "oversized-file" => entries[0].stored_bytes = 2_000_000_001,
            "wrong-hash" => entries[0].raw_blake3 = "00".repeat(32),
            _ => unreachable!(),
        }
        let mut writer = csv::Writer::from_writer(vec![]);
        for e in &entries {
            writer.serialize(e).unwrap();
        }
        fs::write(f.archive.join("files.csv"), writer.into_inner().unwrap()).unwrap();
        let mut hashes = std::collections::BTreeMap::new();
        for name in ["files.csv", "metadata.json", "plan.json"] {
            hashes.insert(
                name.to_owned(),
                io::hash_file(&io::open_regular(&f.archive.join(name), false).unwrap()).unwrap(),
            );
        }
        let checksums: String = hashes
            .iter()
            .map(|(name, hash)| format!("{hash}  {name}\n"))
            .collect();
        fs::write(f.archive.join("checksums.sha256"), checksums).unwrap();
        hashes.insert(
            "checksums.sha256".into(),
            io::hash_file(&io::open_regular(&f.archive.join("checksums.sha256"), false).unwrap())
                .unwrap(),
        );
        let mut commit: verify::Commit = io::read_json(&f.archive.join("COMMITTED")).unwrap();
        commit.hashes = hashes;
        fs::write(f.archive.join("COMMITTED"), io::json(&commit).unwrap()).unwrap();
        let error = verify::verify(&f.archive, f.dir.path(), &NoEvents).unwrap_err();
        assert_eq!(
            error.code,
            match kind {
                "duplicate" | "gap" | "length-overflow" => "INVALID_INDEX",
                "oversized-file" => "FILE_SIZE_LIMIT",
                _ => "FILE_HASH_MISMATCH",
            },
            "{kind}"
        );
    }
}
