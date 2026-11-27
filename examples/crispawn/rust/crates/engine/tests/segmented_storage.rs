// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-digital. Licensed under Apache-2.0.
use crispawn_core::{
    plan::Plan,
    types::{Codec, NoEvents},
};
use crispawn_engine::{backup, restore, verify, NoHooks};
use crispawn_engine::{Error, Hooks, Result};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};
#[derive(Default)]
struct Progress(std::sync::Mutex<Vec<u64>>);
impl crispawn_core::types::EventSink for Progress {
    fn emit(&self, _: crispawn_core::types::Snapshot) {}
    fn storage_progress(&self, _: &str, s: &crispawn_core::live::StorageStatus) {
        self.0.lock().unwrap().push(s.confirmed_source_bytes);
    }
}
impl Progress {
    fn check(&self, total: u64) {
        let p = self.0.lock().unwrap();
        assert_eq!(p.last(), Some(&total));
        assert!(p.windows(2).all(|v| v[0] <= v[1]));
        assert!(p.iter().all(|&v| v <= total));
    }
}
fn plan() -> Plan {
    Plan {
        format_version: 3,
        segmented_storage: true,
        range_bytes: 4 << 20,
        read_bytes: 1 << 20,
        container_bytes: 64 << 20,
        workers: 2,
        regions: 2,
        codec: Codec::Zstd,
        compression_level: 3,
        memory_limit_bytes: 1 << 30,
        ..Plan::default()
    }
}
fn bytes() -> Vec<u8> {
    let mut n = 0x123456789abcdefu64;
    (0..8 << 20)
        .map(|_| {
            n ^= n << 13;
            n ^= n >> 7;
            n ^= n << 17;
            n as u8
        })
        .collect()
}
fn parts(root: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    for d in fs::read_dir(root.join("data/segments")).unwrap() {
        for f in fs::read_dir(d.unwrap().path()).unwrap() {
            out.push(f.unwrap().path());
        }
    }
    out.sort();
    out
}
#[test]
fn skipping_copy_readback_is_explicit_and_restore_still_verifies_target() {
    use crispawn_engine::control::RunControl;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[derive(Default)]
    struct Readback(AtomicUsize);
    impl Hooks for Readback {
        fn at(&self, point: &'static str) -> Result<()> {
            if point == "restore_verify_read" {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }
    }
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    let original = bytes();
    fs::write(&source, &original).unwrap();
    let archive = t.path().join("archive");
    let control = RunControl::new();
    control.set_backup_verification(false);
    let progress = Progress::default();
    let hook = Readback::default();
    let report = backup::backup(
        &source,
        &archive,
        plan(),
        t.path(),
        &progress,
        &hook,
        &control,
    )
    .unwrap();
    assert_eq!(report.verification_kind, "skipped");
    assert!(report.dataset_committed);
    let states = progress.0.lock().unwrap();
    assert!(!states.is_empty() && states.iter().all(|v| *v == 0));
    let commit: serde_json::Value =
        serde_json::from_slice(&fs::read(archive.join("COMMITTED")).unwrap()).unwrap();
    assert_eq!(commit["verification_kind"], "skipped");
    // An explicit later verify does perform the stored-byte and original checks.
    verify::verify(&archive, t.path(), &NoEvents).unwrap();
    let target = t.path().join("target");
    let restored = restore::restore(&archive, &target, false, t.path(), &NoEvents, &hook).unwrap();
    assert!(restored.restored_ranges_verified && hook.0.load(Ordering::SeqCst) > 0);
    assert_eq!(fs::read(target).unwrap(), original);
    // The stored hash remains meaningful even when initial readback was skipped.
    use std::os::unix::fs::FileExt;
    let part = parts(&archive)[0].clone();
    let changed = fs::read(&part).unwrap()[0] ^ 0xff;
    fs::OpenOptions::new()
        .write(true)
        .open(&part)
        .unwrap()
        .write_at(&[changed], 0)
        .unwrap();
    assert!(verify::verify(&archive, t.path(), &NoEvents).is_err());
    let enabled_progress = Progress::default();
    let enabled = backup::backup(
        &source,
        &t.path().join("enabled"),
        plan(),
        t.path(),
        &enabled_progress,
        &hook,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(enabled.verification_kind, "compressed_blake3");
    enabled_progress.check(original.len() as u64);
}
#[test]
fn skipped_readback_does_not_skip_final_flush_failure() {
    struct Fail;
    impl Hooks for Fail {
        fn at(&self, point: &'static str) -> Result<()> {
            if point == "before_final_storage_sync" {
                Err(Error::new(
                    "FINAL_FLUSH_TEST",
                    "final flush is still mandatory",
                ))
            } else {
                Ok(())
            }
        }
    }
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    fs::write(&source, bytes()).unwrap();
    let control = crispawn_engine::control::RunControl::new();
    control.set_backup_verification(false);
    let archive = t.path().join("archive");
    let error = backup::backup(
        &source,
        &archive,
        plan(),
        t.path(),
        &NoEvents,
        &Fail,
        &control,
    )
    .unwrap_err();
    assert_eq!(error.code, "FINAL_FLUSH_TEST");
    assert!(!archive.join("COMMITTED").exists());
}
#[cfg(target_os = "linux")]
#[test]
fn copy_has_no_part_fsync_and_final_flush_failure_never_commits() {
    use std::process::Command;
    let t = tempfile::tempdir().unwrap();
    let library = t.path().join("sync-trace.so");
    let status = Command::new("cc")
        .args(["-shared", "-fPIC", "-O2", "-o"])
        .arg(&library)
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/storage_sync_trace.c"
        ))
        .arg("-ldl")
        .status()
        .unwrap();
    assert!(status.success());
    for (fail, skip) in [(false, false), (true, false), (false, true), (true, true)] {
        let case = t.path().join(format!("writeback-error-{fail}-skip-{skip}"));
        fs::create_dir(&case).unwrap();
        let archive = case.join("archive");
        let trace = case.join("sync.tsv");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "storage_sync_fixture_child",
                "--ignored",
                "--nocapture",
            ])
            .env("LD_PRELOAD", &library)
            .env("CRISPAWN_TEST_SYNC_TRACE", &trace)
            .env("CRISPAWN_TEST_ARCHIVE", &archive);
        if fail {
            command.env("CRISPAWN_TEST_SYNCFS_EIO", "1");
        }
        if skip {
            command.env("CRISPAWN_TEST_SKIP_READBACK", "1");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let text = fs::read_to_string(trace).unwrap();
        let lines: Vec<_> = text.lines().collect();
        let barriers: Vec<_> = lines
            .iter()
            .filter(|line| line.starts_with("syncfs\t") && line.contains("/archive\t"))
            .collect();
        assert_eq!(barriers.len(), 1, "{text}");
        assert!(barriers[0].ends_with("committed=0"));
        assert!(
            !lines.iter().any(|line| (line.starts_with("fsync\t")
                || line.starts_with("fdatasync\t"))
                && (line.contains("/segments/") || line.contains("/data/data-"))),
            "{text}"
        );
        assert!(
            lines
                .iter()
                .filter(|line| line.contains("journal.jsonl"))
                .count()
                <= 1,
            "{text}"
        );
        let log_syncs = lines
            .iter()
            .filter(|line| line.contains("/continuous/events-"))
            .count();
        assert!(
            log_syncs <= 2,
            "log rotation/progress must not force device flushes during copy: {text}"
        );
        assert_eq!(archive.join("COMMITTED").exists(), !fail);
        let copy_reads = lines
            .iter()
            .filter(|line| line.starts_with("pread") && line.ends_with("committed=0"))
            .count();
        assert_eq!(
            copy_reads > 0,
            !skip,
            "copy readback must follow the explicit setting: {text}"
        );
        if !fail {
            let barrier = lines
                .iter()
                .position(|line| line.starts_with("syncfs\t") && line.contains("/archive\t"))
                .unwrap();
            assert!(
                lines[barrier + 1..]
                    .iter()
                    .any(|line| line.starts_with("fsync\t")),
                "commit marker must also be synced"
            );
        }
    }
}
#[cfg(target_os = "linux")]
#[test]
#[ignore = "launched only by the syscall tracing parent with a private LD_PRELOAD"]
fn storage_sync_fixture_child() {
    let archive = PathBuf::from(std::env::var_os("CRISPAWN_TEST_ARCHIVE").unwrap());
    let root = archive.parent().unwrap();
    let source = root.join("source");
    fs::write(&source, bytes()).unwrap();
    let diag =
        crispawn_engine::diagnostics::Diagnostics::new(&root.join("ram"), 64 << 10, "a".repeat(32))
            .unwrap();
    diag.record("start", serde_json::json!({"test":true}));
    diag.flush().unwrap();
    diag.begin_persistent(&root.join("continuous"), 0).unwrap();
    diag.phase("COPYING", "testing continuous cached log writes");
    for n in 0..40 {
        diag.record(
            "during_copy",
            serde_json::json!({"n":n,"padding":"x".repeat(2048)}),
        );
    }
    diag.flush().unwrap();
    let control = crispawn_engine::control::RunControl::new();
    control.set_backup_verification(std::env::var_os("CRISPAWN_TEST_SKIP_READBACK").is_none());
    let result = backup::backup(
        &source,
        &archive,
        plan(),
        root,
        &NoEvents,
        &NoHooks,
        &control,
    );
    diag.record("copy_result", serde_json::json!({"success":result.is_ok()}));
    diag.close_persistent().unwrap();
    diag.stop();
    if std::env::var_os("CRISPAWN_TEST_SYNCFS_EIO").is_some() {
        let error = result.unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EIO));
        assert!(!archive.join("COMMITTED").exists());
    } else {
        assert!(result.unwrap().dataset_committed);
        verify::verify(&archive, root, &NoEvents).unwrap();
    }
}
#[test]
fn one_compression_frame_spans_independently_checked_physical_files_and_restores() {
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    let archive = t.path().join("archive");
    let raw = bytes();
    fs::write(&source, &raw).unwrap();
    let progress = Progress::default();
    let r = backup::backup(
        &source,
        &archive,
        plan(),
        t.path(),
        &progress,
        &NoHooks,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(r.dataset_committed);
    assert_eq!(r.verification_kind, "compressed_blake3");
    progress.check(raw.len() as u64);
    // Schema 2: frames span independently checked appended chunks inside one
    // large part per stream; the footer shares that part (no tiny extra file).
    let pieces = parts(&archive);
    assert!(!pieces.is_empty());
    assert!(pieces
        .iter()
        .all(|p| (97..=1 << 30).contains(&fs::metadata(p).unwrap().len())));
    let index = fs::read_to_string(archive.join("files.csv")).unwrap();
    assert!(index
        .lines()
        .next()
        .unwrap()
        .contains("storage_descriptor_blake3"));
    verify::verify(&archive, t.path(), &NoEvents).unwrap();
    let restored = t.path().join("restored");
    restore::restore(&archive, &restored, false, t.path(), &NoEvents, &NoHooks).unwrap();
    assert_eq!(fs::read(restored).unwrap(), raw);
}
#[test]
fn changed_part_is_rejected_before_restore_target_is_created() {
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    let archive = t.path().join("archive");
    fs::write(&source, bytes()).unwrap();
    backup::backup(
        &source,
        &archive,
        plan(),
        t.path(),
        &NoEvents,
        &NoHooks,
        &AtomicBool::new(false),
    )
    .unwrap();
    let p = parts(&archive).remove(0);
    let mut b = fs::read(&p).unwrap();
    let middle = b.len() / 2;
    b[middle] ^= 1;
    fs::write(&p, b).unwrap();
    assert_eq!(
        verify::verify(&archive, t.path(), &NoEvents)
            .unwrap_err()
            .code,
        "STORED_HASH_MISMATCH"
    );
    let target = t.path().join("target");
    assert!(restore::restore(&archive, &target, false, t.path(), &NoEvents, &NoHooks).is_err());
    assert!(!target.exists());
}
#[test]
fn interrupted_segmented_archives_resume_without_deleting_already_written_parts() {
    struct Fail(&'static str, std::sync::atomic::AtomicBool);
    impl Hooks for Fail {
        fn at(&self, p: &'static str) -> Result<()> {
            if p == self.0 && !self.1.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return Err(Error::new("INJECTED_IO_ERROR", p));
            }
            Ok(())
        }
    }
    for point in [
        "after_data_write",
        "after_data_sync",
        "after_journal_write",
        "after_footer_sync",
        "after_data_rename",
        "after_seal",
        "after_indices",
        "before_commit",
    ] {
        let t = tempfile::tempdir().unwrap();
        let source = t.path().join("source");
        let archive = t.path().join("archive");
        let raw = bytes();
        fs::write(&source, &raw).unwrap();
        let e = backup::backup(
            &source,
            &archive,
            plan(),
            t.path(),
            &NoEvents,
            &Fail(point, AtomicBool::new(false)),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert_eq!(e.code, "INJECTED_IO_ERROR", "{point}");
        assert!(!archive.join("COMMITTED").exists());
        let old = if archive.join("data/segments").exists() {
            parts(&archive)
        } else {
            vec![]
        };
        let progress = Progress::default();
        backup::resume(
            &source,
            &archive,
            t.path(),
            &progress,
            &NoHooks,
            &AtomicBool::new(false),
        )
        .unwrap_or_else(|e| panic!("{point}: {e}"));
        progress.check(raw.len() as u64);
        assert!(old.iter().all(|p| p.exists()));
        let restored = t.path().join("restored");
        restore::restore(&archive, &restored, false, t.path(), &NoEvents, &NoHooks).unwrap();
        assert_eq!(fs::read(restored).unwrap(), raw, "{point}");
    }
}
#[test]
fn part_changed_during_final_flush_is_rejected_before_commit() {
    struct Change(PathBuf);
    impl Hooks for Change {
        fn at(&self, point: &'static str) -> Result<()> {
            if point == "after_final_storage_sync" {
                let path = parts(&self.0).remove(0);
                let file = fs::OpenOptions::new().write(true).open(&path)?;
                file.set_len(file.metadata()?.len() + 1)?;
            }
            Ok(())
        }
    }
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    let archive = t.path().join("archive");
    fs::write(&source, bytes()).unwrap();
    let error = backup::backup(
        &source,
        &archive,
        plan(),
        t.path(),
        &NoEvents,
        &Change(archive.clone()),
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert_eq!(error.code, "FILE_CHANGED");
    assert!(!archive.join("COMMITTED").exists());
}
#[test]
fn restore_target_cannot_alias_a_physical_part_through_an_external_hard_link() {
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    let archive = t.path().join("archive");
    fs::write(&source, &bytes()[..512]).unwrap();
    let p = Plan {
        range_bytes: 512,
        read_bytes: 512,
        container_bytes: 4096,
        workers: 1,
        regions: 1,
        ..plan()
    };
    backup::backup(
        &source,
        &archive,
        p,
        t.path(),
        &NoEvents,
        &NoHooks,
        &AtomicBool::new(false),
    )
    .unwrap();
    let part = parts(&archive)
        .into_iter()
        .find(|p| fs::metadata(p).unwrap().len() >= 512)
        .unwrap();
    let before = fs::read(&part).unwrap();
    let alias = t.path().join("target-alias");
    fs::hard_link(&part, &alias).unwrap();
    assert_eq!(
        restore::restore(&archive, &alias, true, t.path(), &NoEvents, &NoHooks)
            .unwrap_err()
            .code,
        "TARGET_IS_ARCHIVE"
    );
    assert_eq!(fs::read(part).unwrap(), before);
}
#[test]
fn segmented_storage_restores_zstd_gzip_xz_raw_and_zero_streams() {
    for codec in [Codec::Zstd, Codec::Gzip, Codec::Xz, Codec::Raw, Codec::Zero] {
        let t = tempfile::tempdir().unwrap();
        let source = t.path().join("source");
        let archive = t.path().join("archive");
        let mut raw = bytes();
        for (i, chunk) in raw.chunks_mut(16384).enumerate() {
            if codec == Codec::Zero || i % 2 == 0 {
                chunk.fill(0);
            }
        }
        fs::write(&source, &raw).unwrap();
        let mut p = plan();
        p.codec = if codec == Codec::Zero {
            Codec::Zstd
        } else {
            codec
        };
        p.compression_level = match p.codec {
            Codec::Raw => 0,
            Codec::Gzip => 6,
            _ => 3,
        };
        backup::backup(
            &source,
            &archive,
            p,
            t.path(),
            &NoEvents,
            &NoHooks,
            &AtomicBool::new(false),
        )
        .unwrap();
        let csv = fs::read_to_string(archive.join("files.csv")).unwrap();
        assert!(csv.contains(codec.name()), "{codec:?}: {csv}");
        verify::verify(&archive, t.path(), &NoEvents).unwrap();
        let target = t.path().join("target");
        restore::restore(&archive, &target, false, t.path(), &NoEvents, &NoHooks).unwrap();
        assert_eq!(fs::read(target).unwrap(), raw, "{codec:?}");
    }
}
