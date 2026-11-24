// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
use crispawn_core::{plan::Plan, types::Codec, workflow::Action};
use crispawn_engine::{
    job::{Controller, JobEvent, JobEvents, NoJobEvents, Operation},
    job_config::{JobConfig, Tuning, TuningMode},
    presentation,
    probe::{self, Trial},
};
use std::{
    fs,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

fn config(root: &Path) -> JobConfig {
    fs::write(
        root.join("source.img"),
        [
            b"disk data ".repeat(65536),
            vec![0; 65536],
            b"last".to_vec(),
        ]
        .concat(),
    )
    .unwrap();
    JobConfig {
        schema_version: 1,
        source_file: Some(root.join("source.img")),
        source_device: None,
        archive: root.join("archive"),
        scratch_dir: root.into(),
        restore_target_file: Some(root.join("restored.img")),
        restore_target_device: None,
        report_file: None,
        tuning: Tuning {
            mode: TuningMode::Fixed,
            plan: Plan {
                range_bytes: 65536,
                container_bytes: 262144,
                read_bytes: 8192,
                checkpoint_records: 2,
                codec: Codec::Zstd,
                compression_level: 3,
                ..Plan::current()
            },
            ..Tuning::default()
        },
    }
}
#[derive(Default)]
struct Events(Mutex<Vec<JobEvent>>);
impl JobEvents for Events {
    fn emit(&self, e: JobEvent) {
        self.0.lock().unwrap().push(e);
    }
}
#[test]
fn unchecked_copy_completes_without_verification_credit_and_restore_keeps_checks() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path());
    config.tuning.plan.format_version = 3;
    config.tuning.plan.segmented_storage = true;
    config.tuning.verify_after_write = false;
    config.tuning.mode = TuningMode::Auto;
    let events = Arc::new(Events::default());
    let report = Controller::new(events.clone())
        .unwrap()
        .run(&config, Operation::Backup)
        .unwrap();
    assert_eq!(report.status, "complete");
    assert_eq!(report.verify_workers, 0);
    assert_eq!(report.engine.unwrap().verification_kind, "skipped");
    for e in events.0.lock().unwrap().iter() {
        if let JobEvent::Progress { snapshot, .. } = e {
            assert_eq!(snapshot.verified_raw_bytes, 0);
        }
    }
    let restored = Controller::new(Arc::new(NoJobEvents))
        .unwrap()
        .run(&config, Operation::Restore)
        .unwrap();
    assert!(restored.engine.unwrap().restored_ranges_verified);
    assert_eq!(
        fs::read(config.source_file.unwrap()).unwrap(),
        fs::read(config.restore_target_file.unwrap()).unwrap()
    );
}
#[test]
fn unattended_controller_backup_verify_restore_and_view_are_separate() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    let events = Arc::new(Events::default());
    let controller = Controller::new(events.clone()).unwrap();
    assert_eq!(
        controller.request(Action::Pause).unwrap_err().code,
        "CONTROL_UNAVAILABLE"
    );
    let report = controller.run(&config, Operation::Backup).unwrap();
    assert_eq!(report.status, "complete");
    assert!(report.engine.unwrap().dataset_committed);
    assert!(!report.guest_poweroff_requested);
    let disk: serde_json::Value =
        serde_json::from_slice(&fs::read(&report.report_file).unwrap()).unwrap();
    assert_eq!(disk["status"], "complete");
    assert_eq!(
        disk["selected_plan"],
        serde_json::to_value(&config.tuning.plan).unwrap()
    );
    let captured = events.0.lock().unwrap();
    let selected = captured
        .iter()
        .position(|e| matches!(e, JobEvent::PlanSelected { .. }))
        .unwrap();
    let copying = captured
        .iter()
        .position(|e| matches!(e, JobEvent::Progress { phase, .. } if phase == "COPYING"))
        .unwrap();
    assert!(selected < copying);
    let view = captured
        .iter()
        .map(presentation::text)
        .collect::<Vec<_>>()
        .join("\n");
    for expected in ["圧縮=zstd", "全体=", "読出=", "同期済=", "ETA=", "[完了]"] {
        assert!(view.contains(expected), "{expected}");
    }
    for event in captured.iter() {
        if let JobEvent::Progress { snapshot, .. } = event {
            assert!(snapshot.durable_raw_bytes <= snapshot.read_raw_bytes);
            assert!(snapshot.read_raw_bytes <= snapshot.total_bytes);
        }
    }
    drop(captured);
    Controller::new(Arc::new(NoJobEvents))
        .unwrap()
        .run(&config, Operation::Verify)
        .unwrap();
    Controller::new(Arc::new(NoJobEvents))
        .unwrap()
        .run(&config, Operation::Restore)
        .unwrap();
    assert_eq!(
        fs::read(config.source().unwrap()).unwrap(),
        fs::read(config.restore_target_file.unwrap()).unwrap()
    );
}
#[test]
fn normal_auto_starts_from_profile_without_discarded_trial_copies() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path());
    config.tuning = Tuning::default();
    let events = Arc::new(Events::default());
    let report = Controller::new(events.clone())
        .unwrap()
        .run(&config, Operation::Backup)
        .unwrap();
    assert!(report.probe.is_none());
    let plan = report.selected_plan.unwrap();
    assert_eq!(plan.codec, Codec::Zstd);
    assert_eq!(plan.compression_level, 3);
    assert_eq!(plan.range_bytes, 16 << 20);
    assert_eq!(plan.read_bytes, 1 << 20);
    assert!(report.verify_workers >= 1);
    assert!(!events
        .0
        .lock()
        .unwrap()
        .iter()
        .any(|e| matches!(e, JobEvent::ProbeTrial { .. })));
}

#[test]
fn explicit_probe_controller_records_each_trial_and_selects_a_measured_plan() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path());
    config.tuning = Tuning {
        mode: TuningMode::Probe,
        max_trials: 5,
        ..Tuning::default()
    };
    let report = Controller::new(Arc::new(NoJobEvents))
        .unwrap()
        .run(&config, Operation::Backup)
        .unwrap();
    let p = report.probe.unwrap();
    assert_eq!(p.trials.len(), 5);
    assert!(p
        .trials
        .iter()
        .any(|t| t.candidate_id == p.selected_candidate_id && t.plan == p.selected_plan));
    assert_eq!(report.selected_plan.unwrap(), p.selected_plan);
    assert!(!p.full_16m_frame_observed);
    assert!(!fs::read_dir(dir.path()).unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".crispawn-probe-")));
}
#[test]
fn existing_archive_and_existing_report_are_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path());
    fs::create_dir(&config.archive).unwrap();
    fs::write(config.archive.join("keep"), b"existing data").unwrap();
    let e = Controller::new(Arc::new(NoJobEvents))
        .unwrap()
        .run(&config, Operation::Backup)
        .unwrap_err();
    assert_eq!(e.code, "ARCHIVE_EXISTS");
    assert_eq!(
        fs::read(config.archive.join("keep")).unwrap(),
        b"existing data"
    );
    config.archive = dir.path().join("second");
    config.report_file = Some(dir.path().join("old-report.json"));
    fs::write(config.report_file.as_ref().unwrap(), b"preserve report").unwrap();
    assert!(Controller::new(Arc::new(NoJobEvents))
        .unwrap()
        .run(&config, Operation::Backup)
        .is_err());
    assert_eq!(
        fs::read(config.report_file.unwrap()).unwrap(),
        b"preserve report"
    );
    assert!(!config.archive.exists());
}
#[test]
fn cancelled_preflight_records_failure_without_archive_or_poweroff() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path());
    config.report_file = Some(dir.path().join("cancel.json"));
    let controller = Controller::new(Arc::new(NoJobEvents)).unwrap();
    controller.cancel();
    assert_eq!(
        controller.run(&config, Operation::Backup).unwrap_err().code,
        "CANCELLED"
    );
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(config.report_file.unwrap()).unwrap()).unwrap();
    assert_eq!(report["status"], "cancelled");
    assert_eq!(report["guest_poweroff_requested"], false);
    assert!(!config.archive.exists());
}
struct MutateSource {
    path: std::path::PathBuf,
    changed: AtomicBool,
}
impl JobEvents for MutateSource {
    fn emit(&self, e: JobEvent) {
        if matches!(
            e,
            JobEvent::ProbeTrial {
                elapsed_seconds: Some(_),
                ..
            }
        ) && !self.changed.swap(true, Ordering::SeqCst)
        {
            fs::write(&self.path, b"changed during probe").unwrap();
        }
    }
}
#[test]
fn source_change_during_probe_is_fatal_instead_of_a_codec_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path());
    config.tuning = Tuning {
        mode: TuningMode::Probe,
        max_trials: 1,
        ..Tuning::default()
    };
    let events = Arc::new(MutateSource {
        path: config.source_file.clone().unwrap(),
        changed: AtomicBool::new(false),
    });
    let e = Controller::new(events)
        .unwrap()
        .run(&config, Operation::Backup)
        .unwrap_err();
    assert_eq!(e.code, "SOURCE_CHANGED");
    assert!(!config.archive.exists());
}
#[test]
fn config_paths_use_the_config_directory_and_schema_is_strict() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    let path = dir.path().join("job.json");
    let mut value = serde_json::to_value(config).unwrap();
    value["source_file"] = "source.img".into();
    value["archive"] = "archive".into();
    value["scratch_dir"] = ".".into();
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let loaded = JobConfig::load(&path).unwrap();
    assert_eq!(loaded.source_file.unwrap(), dir.path().join("source.img"));
    value["extra"] = true.into();
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(JobConfig::load(&path).unwrap_err().code, "INVALID_JSON");
    value.as_object_mut().unwrap().remove("extra");
    value["schema_version"] = 2.into();
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(JobConfig::load(&path).unwrap_err().code, "UNKNOWN_VERSION");
}
#[test]
fn probe_windows_are_bounded_nonoverlapping_and_span_the_source() {
    for size in [
        1,
        513,
        16 << 20,
        48 << 20,
        (48 << 20) + 1,
        1 << 40,
        u64::MAX,
    ] {
        let ranges = probe::window_ranges(size).unwrap();
        let mut last = 0;
        for (offset, length) in &ranges {
            assert!(*offset >= last && *length > 0 && *length <= 48 << 20);
            assert!(offset.checked_add(*length).unwrap() <= size);
            last = offset + length;
        }
        assert_eq!(ranges[0].0, 0);
        assert_eq!(last, size);
    }
    assert_eq!(probe::window_ranges(0).unwrap_err().code, "EMPTY_SOURCE");
}
fn trial(seconds: f64, plan: Plan) -> Trial {
    Trial {
        candidate_id: format!("{seconds}"),
        plan,
        elapsed_seconds: seconds,
        copy_seconds: seconds * 0.8,
        verify_seconds: seconds * 0.2,
        stored_bytes: "100".into(),
    }
}
#[test]
fn measured_selection_uses_total_time_memory_ties_and_rejects_invalid_measurements() {
    let small = Plan {
        workers: 1,
        ..Plan::default()
    };
    let large = Plan {
        workers: 2,
        ..Plan::default()
    };
    assert_eq!(
        probe::select_trial(&[trial(1.0, large.clone()), trial(1.01, small.clone())]).unwrap(),
        1
    );
    assert_eq!(
        probe::select_trial(&[trial(1.0, large), trial(1.1, small)]).unwrap(),
        0
    );
    for bad in [f64::NAN, f64::INFINITY, 0.0, -1.0] {
        assert!(probe::select_trial(&[trial(bad, Plan::default())]).is_err());
    }
    assert!(probe::select_trial(&[]).is_err());
    assert!(probe::estimate(f64::INFINITY).is_none());
}
#[test]
fn text_view_escapes_terminal_controls_and_does_not_mutate_events() {
    let event = JobEvent::ControlRejected {
        code: "TEST".into(),
        detail: "\x1b[31m\nmessage".into(),
    };
    let first = presentation::text(&event);
    assert!(!first.contains('\x1b') && !first.contains('\n'));
    assert_eq!(first, presentation::text(&event));
}

#[test]
fn resumed_eta_uses_only_bytes_processed_in_the_current_run() {
    let e = probe::remaining(10.0, 1000, 600, 500, 3.0).unwrap();
    assert_eq!(e.seconds, 43);
    assert!(probe::remaining(10.0, 1000, 500, 500, 3.0).is_none());
    assert!(probe::remaining(0.0, 1000, 600, 500, 3.0).is_none());
    assert!(probe::remaining(10.0, 1000, 1001, 500, 3.0).is_none());
    assert_eq!(
        probe::remaining(10.0, 1000, 1000, 500, 3.0)
            .unwrap()
            .seconds,
        3
    );
}

#[test]
fn report_cannot_be_placed_inside_an_existing_archive() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path());
    fs::create_dir(&config.archive).unwrap();
    config.report_file = Some(config.archive.join("job.json"));
    assert_eq!(config.validate().unwrap_err().code, "INVALID_CONFIG");
    assert!(fs::read_dir(config.archive).unwrap().next().is_none());
}

#[test]
fn backup_open_is_bound_to_the_identity_observed_before_probe() {
    use crispawn_core::types::NoEvents;
    use crispawn_engine::{backup, io, NoHooks};
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    let source = config.source().unwrap();
    let before = io::identity(&io::open_regular(source, false).unwrap()).unwrap();
    fs::rename(source, dir.path().join("old-source")).unwrap();
    fs::write(source, b"a different source").unwrap();
    let error = backup::backup_checked(
        source,
        &config.archive,
        config.tuning.plan.clone(),
        dir.path(),
        &NoEvents,
        &NoHooks,
        &AtomicBool::new(false),
        Some(&before),
    )
    .unwrap_err();
    assert_eq!(error.code, "SOURCE_CHANGED");
    assert!(!config.archive.exists());
    assert_eq!(fs::read(source).unwrap(), b"a different source");
    assert!(dir.path().join("old-source").exists());
}
