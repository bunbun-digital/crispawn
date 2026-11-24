// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
use crispawn_core::{
    plan::Plan,
    types::{EventSink, NoEvents, Snapshot},
};
use crispawn_engine::{backup, control::RunControl, restore, verify, Hooks, NoHooks, Result};
use std::{
    fs,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc,
    },
    time::Duration,
};
struct Counters {
    control: Arc<RunControl>,
    requested: AtomicBool,
    reads: AtomicU64,
    writes: AtomicU64,
    prefetch_target: Option<u64>,
}
impl Hooks for Counters {
    fn at(&self, point: &'static str) -> Result<()> {
        if point == "before_data_sync" && !self.requested.load(Ordering::SeqCst) {
            if let Some(target) = self.prefetch_target {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while self.reads.load(Ordering::SeqCst) < target {
                    if std::time::Instant::now() >= deadline {
                        return Err(crispawn_engine::Error::new(
                            "TEST_DEADLINE",
                            "producer did not prefetch while writer was held",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
        if point == "source_read" {
            self.reads.fetch_add(1, Ordering::SeqCst);
        }
        if point == "data_write" {
            self.writes.fetch_add(1, Ordering::SeqCst);
        }
        if point == "after_checkpoint" && !self.requested.swap(true, Ordering::SeqCst) {
            self.control.pause();
        }
        Ok(())
    }
}
struct Events(mpsc::Sender<Snapshot>);
impl EventSink for Events {
    fn emit(&self, event: Snapshot) {
        let _ = self.0.send(event);
    }
}
fn paused_copy(cancel: bool, format_version: u32) {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let archive = dir.path().join("archive");
    fs::write(&source, b"original bytes".repeat(400000)).unwrap();
    let plan = Plan {
        format_version,
        range_bytes: 65536,
        read_bytes: 8192,
        container_bytes: 262144,
        workers: 4,
        checkpoint_records: 1,
        memory_limit_bytes: if format_version == 3 {
            1 << 30
        } else {
            512 << 20
        },
        ..Plan::default()
    };
    let control = Arc::new(RunControl::new());
    let prefetch_target =
        (format_version == 3).then(|| plan.range_count(fs::metadata(&source).unwrap().len()));
    let hooks = Counters {
        control: control.clone(),
        requested: AtomicBool::new(false),
        reads: AtomicU64::new(0),
        writes: AtomicU64::new(0),
        prefetch_target,
    };
    let (sender, receiver) = mpsc::channel();
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            backup::backup(
                &source,
                &archive,
                plan,
                dir.path(),
                &Events(sender),
                &hooks,
                control.as_ref(),
            )
        });
        let paused = loop {
            let event = match receiver.recv_timeout(Duration::from_secs(10)) {
                Ok(event) => event,
                Err(error) => {
                    control.cancel();
                    panic!("pause acknowledgement failed: {error}");
                }
            };
            if event.code == "PAUSE_ACK" {
                break event;
            }
        };
        assert_eq!(paused.phase, "PAUSED");
        if format_version == 3 {
            assert_eq!(paused.read_raw_bytes, fs::metadata(&source).unwrap().len());
            assert!(paused.durable_raw_bytes < paused.read_raw_bytes);
        } else {
            assert_eq!(paused.durable_raw_bytes, paused.read_raw_bytes);
        }
        let counts = (
            hooks.reads.load(Ordering::SeqCst),
            hooks.writes.load(Ordering::SeqCst),
        );
        let journal_bytes = fs::metadata(archive.join("journal.jsonl")).unwrap().len();
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(
            counts,
            (
                hooks.reads.load(Ordering::SeqCst),
                hooks.writes.load(Ordering::SeqCst)
            )
        );
        assert_eq!(
            journal_bytes,
            fs::metadata(archive.join("journal.jsonl")).unwrap().len()
        );
        if cancel {
            control.cancel();
        } else {
            control.resume();
        }
        let result = worker.join().unwrap();
        if cancel {
            assert_eq!(result.unwrap_err().code, "CANCELLED");
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
        } else {
            assert!(result.unwrap().dataset_committed);
        }
    });
    let target = dir.path().join("target");
    restore::restore(&archive, &target, false, dir.path(), &NoEvents, &NoHooks).unwrap();
    assert_eq!(fs::read(&source).unwrap(), fs::read(target).unwrap());
}
#[test]
fn pause_acknowledges_durable_quiescence_then_resume_completes() {
    paused_copy(false, 1);
}
#[test]
fn cancelling_while_paused_preserves_an_explicitly_resumable_archive() {
    paused_copy(true, 1);
}
#[test]
fn v2_pause_acknowledges_durable_quiescence_then_resume_completes() {
    paused_copy(false, 2);
}
#[test]
fn v2_cancelled_pause_is_resumable_with_contiguous_file_hashes() {
    paused_copy(true, 2);
}
#[test]
fn v3_pause_retains_compressed_prefetch_while_acknowledging_io_quiescence() {
    paused_copy(false, 3);
}
#[test]
fn v3_cancelled_pause_preserves_contiguous_hashes_and_resumes() {
    paused_copy(true, 3);
}
#[test]
fn verification_pause_and_cancellation_preserve_the_committed_archive() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let archive = dir.path().join("archive");
    fs::write(&source, b"backup data".repeat(65536)).unwrap();
    backup::backup(
        &source,
        &archive,
        Plan::default(),
        dir.path(),
        &NoEvents,
        &NoHooks,
        &AtomicBool::new(false),
    )
    .unwrap();
    let control = RunControl::new();
    control.pause();
    let (sender, receiver) = mpsc::channel();
    std::thread::scope(|scope| {
        let handle = scope
            .spawn(|| verify::verify_controlled(&archive, dir.path(), &Events(sender), &control));
        loop {
            if receiver.recv_timeout(Duration::from_secs(10)).unwrap().code == "PAUSE_ACK" {
                break;
            }
        }
        control.cancel();
        assert_eq!(handle.join().unwrap().unwrap_err().code, "CANCELLED");
    });
    assert!(
        verify::verify(&archive, dir.path(), &NoEvents)
            .unwrap()
            .dataset_committed
    );
    let target = dir.path().join("target");
    assert_eq!(
        restore::restore_controlled(
            &archive,
            &target,
            false,
            dir.path(),
            &NoEvents,
            &NoHooks,
            &control
        )
        .unwrap_err()
        .code,
        "CANCELLED"
    );
    assert!(!target.exists());
}
