// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-digital. Licensed under Apache-2.0.
//! Actual blocked write(2), not a mocked transport or physical-USB benchmark.
use crispawn_engine::{diagnostics::Diagnostics, io};
use serde_json::json;
use std::{
    fs,
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

fn records(root: &Path) -> Vec<serde_json::Value> {
    let mut paths = fs::read_dir(root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("events-")
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths
        .iter()
        .flat_map(|p| {
            fs::read_to_string(p)
                .unwrap()
                .lines()
                .map(|s| serde_json::from_str(s).unwrap())
                .collect::<Vec<_>>()
        })
        .collect()
}
#[test]
fn blocked_ntfs_log_write_does_not_block_ram_or_fail_the_copy() {
    let root = tempfile::tempdir().unwrap();
    let library = root.path().join("log-backpressure.so");
    assert!(Command::new("cc")
        .args(["-shared", "-fPIC", "-O2", "-o"])
        .arg(&library)
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/log_backpressure.c"
        ))
        .arg("-ldl")
        .status()
        .unwrap()
        .success());
    for overflow in [false, true] {
        let case = root.path().join(format!("overflow-{overflow}"));
        fs::create_dir(&case).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "blocked_log_fixture_child",
                "--nocapture",
            ])
            .env("LD_PRELOAD", &library)
            .env("CRISPAWN_TEST_LOG_GATE", case.join("gate"))
            .env("CRISPAWN_TEST_LOG_ROOT", &case)
            .env(
                "CRISPAWN_TEST_LOG_OVERFLOW",
                if overflow { "1" } else { "0" },
            )
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                panic!("blocked-log regression exceeded its independent test deadline")
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(status.success(), "overflow={overflow}");
    }
}
#[test]
#[ignore = "isolated LD_PRELOAD child only"]
fn blocked_log_fixture_child() {
    let root = std::path::PathBuf::from(std::env::var_os("CRISPAWN_TEST_LOG_ROOT").unwrap());
    let gate = root.join("gate");
    let overflow = std::env::var("CRISPAWN_TEST_LOG_OVERFLOW").unwrap() == "1";
    let diag = Diagnostics::new(
        &root.join("ram"),
        if overflow { 64 << 10 } else { 1 << 20 },
        "a".repeat(32),
    )
    .unwrap();
    diag.record("startup", json!({"startup":true}));
    diag.begin_persistent(&root.join("continuous"), 0).unwrap();
    diag.phase("COPYING", "copying with a blocked destination log write");
    fs::write(gate.with_extension("enabled"), b"enable").unwrap();
    diag.record("trigger", json!({"block_only_destination_log":true}));
    let wait = Instant::now();
    while !gate.with_extension("entered").exists() {
        assert!(wait.elapsed() < Duration::from_secs(2));
        std::thread::sleep(Duration::from_millis(5));
    }
    let started = Instant::now();
    for index in 0..400 {
        diag.record(
            "while_blocked",
            json!({"index":index,"payload":"x".repeat(if overflow{1000}else{32})}),
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    // Exporting RAM runs independently of the blocked continuous mirror.
    diag.export(&root.join("ram-export")).unwrap();
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(records(&root.join("ram-export"))
        .iter()
        .any(|r| r["value"]["index"] == 399));
    // A real ordinary-file backup still completes while that log write waits.
    let source = root.join("source");
    fs::write(&source, vec![0x6a; 4 << 20]).unwrap();
    let plan = crispawn_core::plan::Plan {
        format_version: 3,
        segmented_storage: true,
        range_bytes: 4 << 20,
        read_bytes: 1 << 20,
        container_bytes: 64 << 20,
        workers: 2,
        regions: 2,
        codec: crispawn_core::types::Codec::Zstd,
        compression_level: 3,
        memory_limit_bytes: 1 << 30,
        ..Default::default()
    };
    let archive = root.join("archive");
    let result = crispawn_engine::backup::backup(
        &source,
        &archive,
        plan,
        &root,
        &crispawn_core::types::NoEvents,
        &crispawn_engine::NoHooks,
        &std::sync::atomic::AtomicBool::new(false),
    )
    .unwrap();
    assert!(result.dataset_committed && archive.join("COMMITTED").is_file());
    assert!(!gate.with_extension("release").exists());
    // Cross the old ten-second deadline while producers and RAM remain alive.
    while started.elapsed() < Duration::from_secs(11) {
        diag.record(
            "still_alive",
            json!({"elapsed_ms":started.elapsed().as_millis()}),
        );
        diag.check().unwrap();
        std::thread::sleep(Duration::from_millis(100));
    }
    fs::write(gate.with_extension("release"), b"release").unwrap();
    diag.flush().unwrap();
    diag.check().unwrap();
    diag.close_persistent().unwrap();
    let saved = records(&root.join("continuous"));
    let info: serde_json::Value = io::read_json(&root.join("continuous/log-info.json")).unwrap();
    assert_eq!(info["closed"], true);
    if overflow {
        assert!(info["dropped_records"].as_u64().unwrap() > 0);
        assert!(saved.iter().any(|r| r["kind"] == "log_gap"));
    } else {
        assert_eq!(info["dropped_records"], 0);
        assert_eq!(
            saved
                .iter()
                .filter(|r| r["kind"] == "while_blocked")
                .count(),
            400
        );
    }
    diag.stop();
}
