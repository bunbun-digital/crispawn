// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
use std::{collections::BTreeSet, process::Command};
fn catalog(memory: &str) -> serde_json::Value {
    let work = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_perfect-backup-engine"))
        .args([
            "candidates",
            "--source-bytes",
            "33554945",
            "--memory-limit-bytes",
            memory,
        ])
        .current_dir(work.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read_dir(work.path()).unwrap().count(), 0);
    serde_json::from_slice(&out.stdout).unwrap()
}
#[test]
fn cli_lists_every_candidate_and_rejection_without_creating_files() {
    let value = catalog("536870912");
    assert_eq!(value["success"], true);
    let result = &value["result"];
    assert_eq!(result["eligible_count"], 138);
    assert_eq!(result["rejected_count"], 102);
    let cases = result["candidates"].as_array().unwrap();
    assert_eq!(cases.len(), 240);
    assert_eq!(
        cases
            .iter()
            .map(|c| c["id"].as_str().unwrap())
            .collect::<BTreeSet<_>>()
            .len(),
        240
    );
    assert_eq!(
        cases
            .iter()
            .filter(|c| c["eligibility"]["status"] == "eligible")
            .count(),
        138
    );
}
#[test]
fn cli_reclassifies_candidates_for_the_requested_memory_budget() {
    let value = catalog("268435456");
    assert_eq!(value["result"]["eligible_count"], 36);
    assert_eq!(value["result"]["rejected_count"], 204);
}
#[test]
fn cli_allows_all_240_combinations_when_the_memory_budget_fits() {
    let value = catalog("2147483648");
    assert_eq!(value["result"]["eligible_count"], 240);
    assert_eq!(value["result"]["rejected_count"], 0);
}
#[test]
fn cli_rejects_empty_source_duplicate_and_unknown_options() {
    for args in [
        vec!["candidates", "--source-bytes", "0"],
        vec!["candidates", "--source-bytes", "1", "--source-bytes", "2"],
        vec!["candidates", "--source-bytes", "1", "--unknown", "3"],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_perfect-backup-engine"))
            .args(args)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
    }
}
