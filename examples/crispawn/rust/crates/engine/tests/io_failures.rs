// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
use crispawn_core::{
    plan::Plan,
    types::{Codec, NoEvents},
};
use crispawn_engine::{backup, control::RunControl, io, verify, Hooks, NoHooks, Result};
use std::{
    fs,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

struct Fault {
    point: &'static str,
    errno: i32,
    fired: AtomicBool,
}
impl Hooks for Fault {
    fn at(&self, point: &'static str) -> Result<()> {
        if point == self.point && !self.fired.swap(true, Ordering::Relaxed) {
            Err(std::io::Error::from_raw_os_error(self.errno).into())
        } else {
            Ok(())
        }
    }
}
fn plan() -> Plan {
    Plan {
        range_bytes: 4096,
        read_bytes: 1024,
        container_bytes: 32768,
        regions: 4,
        workers: 4,
        checkpoint_records: 2,
        codec: Codec::Zstd,
        compression_level: 3,
        memory_limit_bytes: 2 << 30,
        ..Plan::current()
    }
}

#[test]
fn io_failures_never_commit_and_can_resume_after_repair() {
    for (errno, code) in [
        (libc::EIO, "IO_ERROR"),
        (libc::ENOSPC, "DESTINATION_FULL"),
        (libc::EDQUOT, "DESTINATION_FULL"),
        (libc::EROFS, "READ_ONLY_DESTINATION"),
        (libc::ENODEV, "DEVICE_UNAVAILABLE"),
        (libc::ETIMEDOUT, "IO_TIMEOUT"),
        (libc::EMFILE, "FILE_DESCRIPTOR_LIMIT"),
        (libc::EFBIG, "FILE_TOO_LARGE"),
    ] {
        for point in [
            "source_read",
            "before_data_write",
            "before_data_sync",
            "before_footer",
            "after_data_rename",
            "verify_read",
            "before_indices",
            "before_commit",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("source");
            let archive = dir.path().join("archive");
            let raw = b"original disk data".repeat(20000);
            fs::write(&source, &raw).unwrap();
            let fault = Fault {
                point,
                errno,
                fired: AtomicBool::new(false),
            };
            let control = RunControl::new();
            control.set_verification_workers(2);
            let begin = Instant::now();
            let error = backup::backup(
                &source,
                &archive,
                plan(),
                dir.path(),
                &NoEvents,
                &fault,
                &control,
            )
            .unwrap_err();
            assert_eq!(error.code, code, "{point}: errno={errno}");
            assert!(fault.fired.load(Ordering::Relaxed));
            assert!(!archive.join("COMMITTED").exists(), "{point}");
            assert!(
                begin.elapsed() < Duration::from_secs(10),
                "failure did not drain: {point}"
            );
            assert_eq!(fs::read(&source).unwrap(), raw);
            backup::resume(&source, &archive, dir.path(), &NoEvents, &NoHooks, &control).unwrap();
            assert!(
                verify::verify(&archive, dir.path(), &NoEvents)
                    .unwrap()
                    .dataset_committed
            );
        }
    }
}

#[test]
fn sealed_files_are_verified_concurrently_before_copy_finishes() {
    use crispawn_core::types::{EventSink, Snapshot};
    use std::sync::atomic::AtomicUsize;
    struct Slow {
        active: AtomicUsize,
        peak: AtomicUsize,
    }
    impl Hooks for Slow {
        fn at(&self, point: &'static str) -> Result<()> {
            if point == "source_read" {
                std::thread::sleep(Duration::from_millis(2));
            }
            if point == "verify_read" {
                let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(active, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(10));
                self.active.fetch_sub(1, Ordering::SeqCst);
            }
            Ok(())
        }
    }
    struct Events(Mutex<Vec<Snapshot>>);
    impl EventSink for Events {
        fn emit(&self, s: Snapshot) {
            self.0.lock().unwrap().push(s);
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let archive = dir.path().join("archive");
    fs::write(&source, vec![0x5a; 2 << 20]).unwrap();
    let hooks = Slow {
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
    };
    let control = RunControl::new();
    control.set_verification_workers(2);
    let events = Events(Mutex::new(vec![]));
    let mut plan = plan();
    plan.codec = Codec::Raw;
    plan.compression_level = 0;
    backup::backup(
        &source,
        &archive,
        plan,
        dir.path(),
        &events,
        &hooks,
        &control,
    )
    .unwrap();
    assert!(hooks.peak.load(Ordering::SeqCst) >= 2);
    let snapshots = events.0.lock().unwrap();
    assert!(snapshots
        .iter()
        .any(|s| s.verified_raw_bytes > 0 && s.read_raw_bytes < s.total_bytes));
    assert!(!snapshots.iter().any(|s| s.phase == "VERIFYING"));
    for s in snapshots.iter() {
        assert!(s.verified_raw_bytes <= s.durable_raw_bytes);
        assert!(s.verification_read_raw_bytes <= s.total_bytes);
    }
    assert_eq!(snapshots.last().unwrap().verified_raw_bytes, 2 << 20);
}

#[test]
fn corruption_after_concurrent_verification_prevents_commit() {
    use std::os::unix::fs::FileExt;
    struct Corrupt(std::path::PathBuf);
    impl Hooks for Corrupt {
        fn at(&self, point: &'static str) -> Result<()> {
            if point == "before_commit" {
                let path = fs::read_dir(self.0.join("data"))?.next().unwrap()?.path();
                fs::OpenOptions::new()
                    .write(true)
                    .open(path)?
                    .write_at(b"corrupt", 100)?;
            }
            Ok(())
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let archive = dir.path().join("archive");
    fs::write(&source, vec![0x5a; 1 << 20]).unwrap();
    let error = backup::backup(
        &source,
        &archive,
        plan(),
        dir.path(),
        &NoEvents,
        &Corrupt(archive.clone()),
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert_eq!(error.code, "FILE_CHANGED");
    assert!(!archive.join("COMMITTED").exists());
}

struct Interrupted;
impl io::Reader for Interrupted {
    fn read_at(&self, _: &mut [u8], _: u64) -> std::io::Result<usize> {
        Err(std::io::ErrorKind::Interrupted.into())
    }
}
impl io::RestoreTarget for Interrupted {
    fn write_at(&self, _: &[u8], _: u64) -> std::io::Result<usize> {
        Err(std::io::ErrorKind::Interrupted.into())
    }
}
#[test]
fn repeated_interruptions_and_invalid_io_contracts_are_bounded() {
    assert_eq!(
        io::read_exact_at(&Interrupted, &mut [0; 1], 0, 1)
            .unwrap_err()
            .code,
        "IO_RETRY_EXHAUSTED"
    );
    assert_eq!(
        io::write_all_at(&Interrupted, b"x", 0).unwrap_err().code,
        "IO_RETRY_EXHAUSTED"
    );
    assert_eq!(
        io::read_exact_at(&Interrupted, &mut [0; 1], 0, 0)
            .unwrap_err()
            .code,
        "INVALID_READ_SIZE"
    );
    struct Oversized;
    impl io::RestoreTarget for Oversized {
        fn write_at(&self, b: &[u8], _: u64) -> std::io::Result<usize> {
            Ok(b.len() + 1)
        }
    }
    assert_eq!(
        io::write_all_at(&Oversized, b"x", 0).unwrap_err().code,
        "INVALID_WRITER"
    );
}

#[test]
fn source_health_checks_cover_start_middle_and_end_and_trap_eio() {
    struct Source(Mutex<Vec<u64>>);
    impl io::Reader for Source {
        fn read_at(&self, b: &mut [u8], offset: u64) -> std::io::Result<usize> {
            self.0.lock().unwrap().push(offset);
            if offset > 0 {
                Err(std::io::Error::from_raw_os_error(libc::EIO))
            } else {
                b.fill(0);
                Ok(b.len())
            }
        }
    }
    let source = Source(Mutex::new(vec![]));
    let error = io::source_preflight(&source, 1 << 20).unwrap_err();
    assert_eq!(error.code, "IO_ERROR");
    assert!(error.detail.contains("source preflight offset"));
    assert_eq!(source.0.lock().unwrap().len(), 5);
}

#[test]
fn real_full_device_reports_capacity_error_and_destination_probe_cleans_up() {
    let full = fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .unwrap();
    assert_eq!(
        io::write_all_at(&full, b"x", 0).unwrap_err().code,
        "DESTINATION_FULL"
    );
    let dir = tempfile::tempdir().unwrap();
    io::destination_preflight(dir.path()).unwrap();
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn transient_reads_and_partial_writes_retry_at_the_same_offset() {
    use std::sync::atomic::AtomicUsize;
    struct FlakyRead {
        calls: AtomicUsize,
        offsets: Mutex<Vec<u64>>,
    }
    impl io::Reader for FlakyRead {
        fn read_at(&self, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
            self.offsets.lock().unwrap().push(offset);
            match self.calls.fetch_add(1, Ordering::SeqCst) {
                0 => return Err(std::io::Error::from_raw_os_error(libc::EIO)),
                2 => return Err(std::io::Error::from_raw_os_error(libc::ETIMEDOUT)),
                _ => {}
            }
            let count = buffer.len().min(3);
            for (i, byte) in buffer[..count].iter_mut().enumerate() {
                *byte = ((offset + i as u64) % 251) as u8;
            }
            Ok(count)
        }
    }
    let reader = FlakyRead {
        calls: AtomicUsize::new(0),
        offsets: Mutex::new(vec![]),
    };
    let mut buffer = [0; 19];
    io::read_exact_at(&reader, &mut buffer, 500, 7).unwrap();
    assert_eq!(
        buffer,
        (500..519)
            .map(|n| (n % 251) as u8)
            .collect::<Vec<_>>()
            .as_slice()
    );
    assert_eq!(&reader.offsets.lock().unwrap()[..4], &[500, 500, 503, 503]);
    struct FlakyWrite {
        calls: AtomicUsize,
        bytes: Mutex<Vec<u8>>,
        offsets: Mutex<Vec<u64>>,
    }
    impl io::RestoreTarget for FlakyWrite {
        fn write_at(&self, buffer: &[u8], offset: u64) -> std::io::Result<usize> {
            self.offsets.lock().unwrap().push(offset);
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(std::io::Error::from_raw_os_error(libc::EIO));
            }
            let count = buffer.len().min(3);
            let mut bytes = self.bytes.lock().unwrap();
            bytes.resize(offset as usize + count, 0);
            bytes[offset as usize..offset as usize + count].copy_from_slice(&buffer[..count]);
            Ok(count)
        }
    }
    let writer = FlakyWrite {
        calls: AtomicUsize::new(0),
        bytes: Mutex::new(vec![]),
        offsets: Mutex::new(vec![]),
    };
    io::write_all_at(&writer, b"0123456789", 0).unwrap();
    assert_eq!(&*writer.bytes.lock().unwrap(), b"0123456789");
    assert_eq!(&writer.offsets.lock().unwrap()[..2], &[0, 0]);
}

#[test]
fn permanent_reads_exhaust_retries_and_full_or_read_only_writes_fail_immediately() {
    use std::sync::atomic::AtomicUsize;
    struct Fail {
        calls: AtomicUsize,
        errno: i32,
    }
    impl io::Reader for Fail {
        fn read_at(&self, _: &mut [u8], _: u64) -> std::io::Result<usize> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(std::io::Error::from_raw_os_error(self.errno))
        }
    }
    impl io::RestoreTarget for Fail {
        fn write_at(&self, _: &[u8], _: u64) -> std::io::Result<usize> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(std::io::Error::from_raw_os_error(self.errno))
        }
    }
    let source = Fail {
        calls: AtomicUsize::new(0),
        errno: libc::EIO,
    };
    assert_eq!(
        io::read_exact_at(&source, &mut [0; 4], 0, 4)
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EIO)
    );
    assert_eq!(source.calls.load(Ordering::SeqCst), 4);
    for errno in [libc::ENOSPC, libc::EDQUOT, libc::EROFS] {
        let target = Fail {
            calls: AtomicUsize::new(0),
            errno,
        };
        assert!(io::write_all_at(&target, b"data", 0).is_err());
        assert_eq!(target.calls.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn destination_space_guard_failure_preserves_partial_copy_and_supports_resume() {
    struct LowSpace(std::sync::atomic::AtomicUsize);
    impl Hooks for LowSpace {
        fn at(&self, point: &'static str) -> Result<()> {
            if point == "destination_space_check" && self.0.fetch_add(1, Ordering::Relaxed) == 2 {
                return Err(crispawn_engine::Error::new(
                    "DESTINATION_LOW_SPACE",
                    "trusted test sample below threshold",
                ));
            }
            Ok(())
        }
    }
    for version in [1, 2, 3] {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let archive = dir.path().join("archive");
        let raw = b"preserve incomplete copy and source".repeat(10000);
        fs::write(&source, &raw).unwrap();
        let mut selected = plan();
        selected.format_version = version;
        let control = RunControl::new();
        let error = backup::backup(
            &source,
            &archive,
            selected,
            dir.path(),
            &NoEvents,
            &LowSpace(std::sync::atomic::AtomicUsize::new(0)),
            &control,
        )
        .unwrap_err();
        assert_eq!(error.code, "DESTINATION_LOW_SPACE");
        assert!(!archive.join("COMMITTED").exists());
        assert_eq!(fs::read(&source).unwrap(), raw);
        backup::resume(&source, &archive, dir.path(), &NoEvents, &NoHooks, &control).unwrap();
        assert!(
            verify::verify(&archive, dir.path(), &NoEvents)
                .unwrap()
                .dataset_committed
        );
    }
}
