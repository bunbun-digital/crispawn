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
fn one_compression_frame_spans_independently_checked_physical_files_and_restores() {
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    let archive = t.path().join("archive");
    let raw = bytes();
    fs::write(&source, &raw).unwrap();
    let r = backup::backup(
        &source,
        &archive,
        plan(),
        t.path(),
        &NoEvents,
        &NoHooks,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(r.dataset_committed);
    assert_eq!(r.verification_kind, "compressed_blake3");
    let pieces = parts(&archive);
    assert!(pieces.len() >= 3);
    assert!(pieces
        .iter()
        .all(|p| fs::metadata(p).unwrap().len() <= 16 << 20));
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
    b[100] ^= 1;
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
        backup::resume(
            &source,
            &archive,
            t.path(),
            &NoEvents,
            &NoHooks,
            &AtomicBool::new(false),
        )
        .unwrap_or_else(|e| panic!("{point}: {e}"));
        assert!(old.iter().all(|p| p.exists()));
        let restored = t.path().join("restored");
        restore::restore(&archive, &restored, false, t.path(), &NoEvents, &NoHooks).unwrap();
        assert_eq!(fs::read(restored).unwrap(), raw, "{point}");
    }
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
