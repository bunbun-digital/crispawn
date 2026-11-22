// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
use crispawn_core::{
    plan::Plan,
    types::{self, Codec, EventSink, NoEvents, Snapshot},
};
use crispawn_engine::{backup, io, restore, verify, Error, Hooks, NoHooks, Result};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
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
        let source = dir.path().join("source.img");
        let archive = dir.path().join("backup");
        let mut seed = 0xa7819321u32;
        let mut raw = (0..4096 * 67 + 137)
            .map(|i| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                if i / 4096 % 3 == 0 {
                    0
                } else if i / 4096 % 3 == 1 {
                    (i % 17) as u8
                } else {
                    seed as u8
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
    fn scratch(&self) -> &Path {
        self.dir.path()
    }
    fn plan(codec: Codec) -> Plan {
        Plan {
            range_bytes: 4096,
            container_bytes: 16 * 1024,
            read_bytes: 777,
            workers: if codec == Codec::Xz { 2 } else { 3 },
            regions: 7,
            checkpoint_records: 3,
            codec,
            compression_level: if codec == Codec::Raw { 0 } else { 3 },
            ..Plan::default()
        }
    }
    fn make(&self, codec: Codec) {
        backup::backup(
            &self.source,
            &self.archive,
            Self::plan(codec),
            self.scratch(),
            &NoEvents,
            &NoHooks,
            &AtomicBool::new(false),
        )
        .unwrap();
    }
    fn restore_and_compare(&self) {
        let target = self.dir.path().join("restored.img");
        restore::restore(
            &self.archive,
            &target,
            false,
            self.scratch(),
            &NoEvents,
            &NoHooks,
        )
        .unwrap();
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
    fn once(point: &'static str) -> Self {
        Self {
            point,
            nth: 1,
            count: AtomicUsize::new(0),
        }
    }
}
impl Hooks for Fail {
    fn at(&self, p: &'static str) -> Result<()> {
        if p == self.point && self.count.fetch_add(1, Ordering::SeqCst) + 1 == self.nth {
            Err(Error::new(
                "INJECTED_IO_ERROR",
                format!("{p}: simulated EIO/ENOSPC/sync failure"),
            ))
        } else {
            Ok(())
        }
    }
}

#[test]
fn all_codecs_pack_by_stored_size_and_restore_every_byte() {
    for codec in [Codec::Raw, Codec::Zstd, Codec::Xz, Codec::Gzip] {
        let f = Fixture::new();
        let events = Events::default();
        let plan = Fixture::plan(codec);
        backup::backup(
            &f.source,
            &f.archive,
            plan.clone(),
            f.scratch(),
            &events,
            &NoHooks,
            &AtomicBool::new(false),
        )
        .unwrap();
        let report = verify::verify(&f.archive, f.scratch(), &NoEvents).unwrap();
        assert_eq!(report.source_bytes, f.raw.len().to_string());
        f.restore_and_compare();
        let mut rows = crispawn_engine::archive::CsvRows::<types::FileEntry>::open(
            &f.archive.join("files.csv"),
            &types::FILE_COLUMNS,
        )
        .unwrap();
        let mut files = vec![];
        while let Some(row) = rows.next_row().unwrap() {
            files.push(row);
        }
        assert!(files.len() > 1);
        for entry in &files[..files.len() - 1] {
            assert!(entry.stored_bytes <= plan.container_bytes);
            assert!(entry.stored_bytes > plan.container_bytes - (plan.range_bytes + 160));
        }
        let events = events.0.lock().unwrap();
        let mut previous = 0;
        for e in events.iter().filter(|e| e.phase == "COPYING") {
            assert!(e.durable_raw_bytes >= previous);
            assert!(e.durable_raw_bytes <= f.raw.len() as u64);
            previous = e.durable_raw_bytes;
        }
        assert_eq!(events.last().unwrap().code, "BACKUP_COMMITTED");
        assert_eq!(events.last().unwrap().durable_raw_bytes, f.raw.len() as u64);
    }
}
#[test]
fn resume_after_each_durability_boundary() {
    for point in [
        "source_read",
        "codec_encode",
        "after_data_write",
        "data_sync",
        "after_data_sync",
        "journal_write",
        "after_journal_write",
        "journal_sync",
        "after_checkpoint",
        "after_footer_sync",
        "after_rename",
        "after_seal",
        "index_write",
        "after_indices",
        "before_commit",
    ] {
        let f = Fixture::new();
        let fail = Fail::once(point);
        let error = backup::backup(
            &f.source,
            &f.archive,
            Fixture::plan(Codec::Zstd),
            f.scratch(),
            &NoEvents,
            &fail,
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert_eq!(error.code, "INJECTED_IO_ERROR", "{point}");
        assert!(!f.archive.join("COMMITTED").exists(), "{point}");
        assert!(verify::verify(&f.archive, f.scratch(), &NoEvents).is_err());
        backup::resume(
            &f.source,
            &f.archive,
            f.scratch(),
            &NoEvents,
            &NoHooks,
            &AtomicBool::new(false),
        )
        .unwrap_or_else(|e| panic!("resume at {point}: {e}"));
        f.restore_and_compare();
    }
}
#[test]
fn torn_journal_and_uncommitted_tail_are_not_promoted() {
    let f = Fixture::new();
    let fail = Fail::once("after_checkpoint");
    assert!(backup::backup(
        &f.source,
        &f.archive,
        Fixture::plan(Codec::Raw),
        f.scratch(),
        &NoEvents,
        &fail,
        &AtomicBool::new(false)
    )
    .is_err());
    OpenOptions::new()
        .append(true)
        .open(f.archive.join("journal.jsonl"))
        .unwrap()
        .write_all(b"{\"body\":")
        .unwrap();
    let partial = fs::read_dir(f.archive.join("data"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.to_string_lossy().ends_with(".part"))
        .unwrap();
    OpenOptions::new()
        .append(true)
        .open(partial)
        .unwrap()
        .write_all(b"uncommitted garbage")
        .unwrap();
    backup::resume(
        &f.source,
        &f.archive,
        f.scratch(),
        &NoEvents,
        &NoHooks,
        &AtomicBool::new(false),
    )
    .unwrap();
    f.restore_and_compare();
}
#[test]
fn source_change_stops_resume_before_mutating_archive() {
    let f = Fixture::new();
    assert!(backup::backup(
        &f.source,
        &f.archive,
        Fixture::plan(Codec::Raw),
        f.scratch(),
        &NoEvents,
        &Fail::once("after_checkpoint"),
        &AtomicBool::new(false)
    )
    .is_err());
    let journal = fs::read(f.archive.join("journal.jsonl")).unwrap();
    let file = OpenOptions::new().write(true).open(&f.source).unwrap();
    file.write_at(b"changed", 0).unwrap();
    file.sync_all().unwrap();
    let e = backup::resume(
        &f.source,
        &f.archive,
        f.scratch(),
        &NoEvents,
        &NoHooks,
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert_eq!(e.code, "SOURCE_CHANGED");
    assert_eq!(fs::read(f.archive.join("journal.jsonl")).unwrap(), journal);
    assert!(!f.archive.join("COMMITTED").exists());
}
#[test]
fn unknown_data_is_preserved_and_resume_refuses_it() {
    let f = Fixture::new();
    assert!(backup::backup(
        &f.source,
        &f.archive,
        Fixture::plan(Codec::Raw),
        f.scratch(),
        &NoEvents,
        &Fail::once("after_checkpoint"),
        &AtomicBool::new(false)
    )
    .is_err());
    fs::write(f.archive.join("data/foreign.bin"), b"keep me").unwrap();
    let journal = fs::read(f.archive.join("journal.jsonl")).unwrap();
    assert_eq!(
        backup::resume(
            &f.source,
            &f.archive,
            f.scratch(),
            &NoEvents,
            &NoHooks,
            &AtomicBool::new(false)
        )
        .unwrap_err()
        .code,
        "UNREFERENCED_DATA"
    );
    assert_eq!(
        fs::read(f.archive.join("data/foreign.bin")).unwrap(),
        b"keep me"
    );
    assert_eq!(fs::read(f.archive.join("journal.jsonl")).unwrap(), journal);
}
#[test]
fn restore_to_larger_dirty_target_preserves_tail_and_writes_zero_ranges() {
    let f = Fixture::new();
    f.make(Codec::Zstd);
    let target = f.dir.path().join("large.img");
    fs::write(&target, vec![0xa5; f.raw.len() + 513]).unwrap();
    restore::restore(&f.archive, &target, true, f.scratch(), &NoEvents, &NoHooks).unwrap();
    let out = fs::read(&target).unwrap();
    assert_eq!(&out[..f.raw.len()], f.raw.as_slice());
    assert_eq!(&out[f.raw.len()..], &[0xa5; 513]);
}
#[test]
fn corrupt_archive_never_creates_restore_target() {
    for offset in [0, 100, 300] {
        let f = Fixture::new();
        f.make(Codec::Raw);
        let path = f.archive.join("data/data-000000.pbk");
        let file = OpenOptions::new().write(true).open(path).unwrap();
        file.write_at(b"!", offset).unwrap();
        file.sync_all().unwrap();
        let target = f.dir.path().join("restore.img");
        assert!(
            restore::restore(&f.archive, &target, false, f.scratch(), &NoEvents, &NoHooks).is_err()
        );
        assert!(!target.exists());
    }
}
#[test]
fn rejects_devices_symlinks_small_targets_and_existing_archives() {
    let f = Fixture::new();
    assert!(io::open_regular(Path::new("/dev/null"), false).is_err());
    let link = f.dir.path().join("source-link");
    std::os::unix::fs::symlink(&f.source, &link).unwrap();
    assert!(io::open_regular(&link, false).is_err());
    f.make(Codec::Raw);
    let marker = fs::read(f.archive.join("COMMITTED")).unwrap();
    assert!(backup::backup(
        &f.source,
        &f.archive,
        Fixture::plan(Codec::Raw),
        f.scratch(),
        &NoEvents,
        &NoHooks,
        &AtomicBool::new(false)
    )
    .is_err());
    assert_eq!(fs::read(f.archive.join("COMMITTED")).unwrap(), marker);
    let target = f.dir.path().join("small.img");
    fs::write(&target, b"preserve").unwrap();
    assert!(restore::restore(&f.archive, &target, true, f.scratch(), &NoEvents, &NoHooks).is_err());
    assert_eq!(fs::read(&target).unwrap(), b"preserve");
    let alias = f.dir.path().join("archive-alias");
    fs::hard_link(f.archive.join("data/data-000000.pbk"), &alias).unwrap();
    assert!(restore::restore(&f.archive, &alias, true, f.scratch(), &NoEvents, &NoHooks).is_err());
    assert!(verify::verify(&f.archive, f.scratch(), &NoEvents).is_ok());
}
#[test]
fn cancellation_checkpoints_and_resumes() {
    struct Cancel<'a>(&'a AtomicBool);
    impl Hooks for Cancel<'_> {
        fn at(&self, p: &'static str) -> Result<()> {
            if p == "after_checkpoint" {
                self.0.store(true, Ordering::SeqCst);
            }
            Ok(())
        }
    }
    let f = Fixture::new();
    let cancel = AtomicBool::new(false);
    let error = backup::backup(
        &f.source,
        &f.archive,
        Fixture::plan(Codec::Raw),
        f.scratch(),
        &NoEvents,
        &Cancel(&cancel),
        &cancel,
    )
    .unwrap_err();
    assert_eq!(error.code, "CANCELLED");
    assert!(!f.archive.join("COMMITTED").exists());
    backup::resume(
        &f.source,
        &f.archive,
        f.scratch(),
        &NoEvents,
        &NoHooks,
        &AtomicBool::new(false),
    )
    .unwrap();
    f.restore_and_compare();
}
#[test]
fn restore_sync_failure_never_reports_success() {
    let f = Fixture::new();
    f.make(Codec::Zstd);
    let target = f.dir.path().join("failed.img");
    let events = Events::default();
    assert!(restore::restore(
        &f.archive,
        &target,
        false,
        f.scratch(),
        &events,
        &Fail::once("restore_sync")
    )
    .is_err());
    assert!(!events
        .0
        .lock()
        .unwrap()
        .iter()
        .any(|s| s.code == "RESTORE_VERIFIED"));
}
#[test]
fn restore_readback_rejects_one_byte_corruption_without_reporting_success() {
    struct CorruptTarget {
        target: PathBuf,
        offset: u64,
        fired: AtomicBool,
    }
    impl Hooks for CorruptTarget {
        fn at(&self, point: &'static str) -> Result<()> {
            if point == "restore_verify_read" && !self.fired.swap(true, Ordering::SeqCst) {
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&self.target)?;
                let mut byte = [0];
                assert_eq!(file.read_at(&mut byte, self.offset)?, 1);
                byte[0] ^= 0xff;
                assert_eq!(file.write_at(&byte, self.offset)?, 1);
                file.sync_all()?;
            }
            Ok(())
        }
    }
    for codec in [Codec::Raw, Codec::Zstd, Codec::Xz, Codec::Gzip] {
        for position in [0, 3 * 4096 + 31, 2 * 4096 + 31, usize::MAX] {
            let f = Fixture::new();
            f.make(codec);
            let offset = if position == usize::MAX {
                f.raw.len() - 1
            } else {
                position
            };
            let target = f.dir.path().join("corrupt-restored.img");
            let events = Events::default();
            let hook = CorruptTarget {
                target: target.clone(),
                offset: offset as u64,
                fired: AtomicBool::new(false),
            };
            let error = restore::restore(&f.archive, &target, false, f.scratch(), &events, &hook)
                .unwrap_err();
            assert_eq!(error.code, "RESTORE_HASH_MISMATCH", "{codec:?} at {offset}");
            assert!(hook.fired.load(Ordering::SeqCst));
            assert_ne!(fs::read(&target).unwrap()[offset], f.raw[offset]);
            let events = events.0.lock().unwrap();
            assert!(events.iter().any(|s| s.phase == "VERIFYING_RESTORE"));
            assert!(!events.iter().any(|s| s.code == "RESTORE_VERIFIED"));
            assert!(!events.iter().any(|s| s.phase == "COMPLETE"));
        }
    }
}
#[test]
fn restore_readback_io_error_never_reports_success() {
    let f = Fixture::new();
    f.make(Codec::Zstd);
    let target = f.dir.path().join("readback-error.img");
    let events = Events::default();
    let error = restore::restore(
        &f.archive,
        &target,
        false,
        f.scratch(),
        &events,
        &Fail::once("restore_verify_read"),
    )
    .unwrap_err();
    assert_eq!(error.code, "INJECTED_IO_ERROR");
    let events = events.0.lock().unwrap();
    assert!(events.iter().any(|s| s.phase == "VERIFYING_RESTORE"));
    assert!(!events.iter().any(|s| s.code == "RESTORE_VERIFIED"));
    assert!(!events.iter().any(|s| s.phase == "COMPLETE"));
}
#[test]
fn sparse_file_io_above_four_gibibytes() {
    let f = tempfile::tempfile().unwrap();
    let offset = (1u64 << 32) + 511;
    f.set_len(offset + 9).unwrap();
    io::write_all_at(&f, b"123456789", offset).unwrap();
    let mut out = [0; 9];
    io::read_exact_at(&f, &mut out, offset, 3).unwrap();
    assert_eq!(&out, b"123456789");
}

fn resign_indices(root: &Path) {
    let mut hashes = std::collections::BTreeMap::new();
    for name in ["files.csv", "metadata.json", "plan.json", "ranges.csv"] {
        hashes.insert(
            name.to_owned(),
            io::hash_file(&io::open_regular(&root.join(name), false).unwrap()).unwrap(),
        );
    }
    let checksums = hashes
        .iter()
        .map(|(name, hash)| format!("{hash}  {name}\n"))
        .collect::<String>();
    fs::write(root.join("checksums.sha256"), &checksums).unwrap();
    hashes.insert(
        "checksums.sha256".into(),
        types::hex(&types::hash(checksums.as_bytes())),
    );
    let meta: types::Metadata = io::read_json(&root.join("metadata.json")).unwrap();
    fs::write(
        root.join("COMMITTED"),
        serde_json::to_vec(&verify::Commit {
            format_version: 1,
            backup_id: meta.backup_id,
            hashes,
            verification_kind: "buffered_reopen".into(),
        })
        .unwrap(),
    )
    .unwrap();
}
#[test]
fn independently_checks_index_semantics_even_if_checksums_are_recomputed() {
    for case in [
        "duplicate",
        "gap",
        "reorder",
        "traversal",
        "huge_integer",
        "overlong_line",
    ] {
        let f = Fixture::new();
        f.make(Codec::Zstd);
        if case == "traversal" {
            let p = f.archive.join("files.csv");
            let text = fs::read_to_string(&p)
                .unwrap()
                .replace("data/data-000000.pbk", "../source.img");
            fs::write(p, text).unwrap();
        } else {
            let p = f.archive.join("ranges.csv");
            let text = fs::read_to_string(&p).unwrap();
            let mut rows = text.lines().map(str::to_owned).collect::<Vec<_>>();
            match case {
                "duplicate" => rows[2] = rows[1].clone(),
                "gap" => {
                    rows.pop();
                }
                "reorder" => rows.swap(1, 2),
                "huge_integer" => rows[1] = rows[1].replacen(",0,", ",18446744073709551616,", 1),
                "overlong_line" => rows[1] = "x".repeat(4096),
                _ => unreachable!(),
            }
            fs::write(p, rows.join("\n") + "\n").unwrap();
        }
        resign_indices(&f.archive);
        let target = f.dir.path().join("must-not-exist.img");
        assert!(
            restore::restore(&f.archive, &target, false, f.scratch(), &NoEvents, &NoHooks).is_err(),
            "{case}"
        );
        assert!(!target.exists(), "{case}");
    }
}
#[test]
fn complete_but_corrupt_journal_line_is_not_silently_truncated() {
    let f = Fixture::new();
    assert!(backup::backup(
        &f.source,
        &f.archive,
        Fixture::plan(Codec::Raw),
        f.scratch(),
        &NoEvents,
        &Fail::once("after_checkpoint"),
        &AtomicBool::new(false)
    )
    .is_err());
    let p = f.archive.join("journal.jsonl");
    let mut entry: serde_json::Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
    entry["digest"] = "f".repeat(64).into();
    let bad = serde_json::to_string(&entry).unwrap() + "\n";
    fs::write(&p, &bad).unwrap();
    assert_eq!(
        backup::resume(
            &f.source,
            &f.archive,
            f.scratch(),
            &NoEvents,
            &NoHooks,
            &AtomicBool::new(false)
        )
        .unwrap_err()
        .code,
        "JOURNAL_HASH_MISMATCH"
    );
    assert_eq!(fs::read_to_string(p).unwrap(), bad);
}
#[test]
fn cooperative_locks_refuse_conflicting_jobs() {
    let f = Fixture::new();
    f.make(Codec::Raw);
    let guard = io::Lock::acquire(&f.archive, true).unwrap();
    assert_eq!(
        verify::verify(&f.archive, f.scratch(), &NoEvents)
            .unwrap_err()
            .code,
        "ARCHIVE_BUSY"
    );
    drop(guard);
    assert!(verify::verify(&f.archive, f.scratch(), &NoEvents).is_ok());
    let source = io::open_regular(&f.source, true).unwrap();
    io::lock_file(&source, true).unwrap();
    let other = f.dir.path().join("other");
    assert_eq!(
        backup::backup(
            &f.source,
            &other,
            Fixture::plan(Codec::Raw),
            f.scratch(),
            &NoEvents,
            &NoHooks,
            &AtomicBool::new(false)
        )
        .unwrap_err()
        .code,
        "FILE_BUSY"
    );
    assert!(!other.exists());
}

#[test]
fn seal_never_overwrites_a_conflicting_data_file() {
    struct Conflict(PathBuf);
    impl Hooks for Conflict {
        fn at(&self, point: &'static str) -> Result<()> {
            if point == "after_footer_sync" {
                fs::write(
                    self.0.join("data/data-000000.pbk"),
                    b"preserve conflicting content",
                )?;
            }
            Ok(())
        }
    }
    let f = Fixture::new();
    assert!(backup::backup(
        &f.source,
        &f.archive,
        Fixture::plan(Codec::Raw),
        f.scratch(),
        &NoEvents,
        &Conflict(f.archive.clone()),
        &AtomicBool::new(false)
    )
    .is_err());
    assert_eq!(
        fs::read(f.archive.join("data/data-000000.pbk")).unwrap(),
        b"preserve conflicting content"
    );
    assert!(f.archive.join("data/data-000000.pbk.part").exists());
    assert!(!f.archive.join("COMMITTED").exists());
}
