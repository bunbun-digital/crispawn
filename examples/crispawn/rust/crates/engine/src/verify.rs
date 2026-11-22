// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
use crate::{
    archive::{self, CsvRows, Index},
    control::ExecutionControl,
    io, Error, Reporter, Result,
};
use crispawn_core::{
    error::require,
    plan::{Plan, MAX_FILES},
    types::{self, EventSink, FileEntry, Metadata, RangeEntry},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

const INDEX_FILES: [&str; 4] = ["files.csv", "metadata.json", "plan.json", "ranges.csv"];
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Commit {
    pub format_version: u32,
    pub backup_id: String,
    pub hashes: BTreeMap<String, String>,
    pub verification_kind: String,
}
#[derive(Debug, Serialize)]
pub struct Report {
    pub operation: String,
    pub backup_id: String,
    pub source_bytes: String,
    pub range_count: String,
    pub file_count: String,
    pub verification_kind: String,
    pub dataset_committed: bool,
    pub restored_ranges_verified: bool,
}
pub struct Verified {
    pub metadata: Metadata,
    pub plan: Plan,
    pub id: [u8; 16],
    pub files: Vec<FileEntry>,
    pub index: Index,
}
impl Verified {
    pub fn report(&self) -> Report {
        Report {
            operation: "verify".into(),
            backup_id: self.metadata.backup_id.clone(),
            source_bytes: self.metadata.source.bytes.to_string(),
            range_count: self.index.count.to_string(),
            file_count: self.files.len().to_string(),
            verification_kind: "buffered_reopen".into(),
            dataset_committed: true,
            restored_ranges_verified: false,
        }
    }
}

pub(crate) fn hashes(root: &Path) -> Result<BTreeMap<String, String>> {
    let mut hashes = BTreeMap::new();
    for name in INDEX_FILES {
        hashes.insert(
            name.to_owned(),
            io::hash_file(&io::open_regular(&root.join(name), false)?)?,
        );
    }
    Ok(hashes)
}
fn checksum_text(hashes: &BTreeMap<String, String>) -> String {
    hashes
        .iter()
        .map(|(name, hash)| format!("{hash}  {name}\n"))
        .collect()
}
pub(crate) fn write_checksums(root: &Path) -> Result<()> {
    io::atomic_write(
        root,
        "checksums.sha256",
        checksum_text(&hashes(root)?).as_bytes(),
    )
}
pub(crate) fn write_commit(root: &Path, id: String) -> Result<()> {
    let mut hashes = hashes(root)?;
    hashes.insert(
        "checksums.sha256".into(),
        io::hash_file(&io::open_regular(&root.join("checksums.sha256"), false)?)?,
    );
    io::atomic_write(
        root,
        "COMMITTED",
        &io::json(&Commit {
            format_version: 1,
            backup_id: id,
            hashes,
            verification_kind: "buffered_reopen".into(),
        })?,
    )
}
pub(crate) fn inspect(
    root: &Path,
    scratch: &Path,
    require_commit: bool,
    report: &mut Reporter<'_>,
    control: &dyn ExecutionControl,
) -> Result<Verified> {
    control.check_cancel()?;
    let (meta, plan, id) = archive::load(root)?;
    for (name, limit) in [
        ("files.csv", MAX_FILES * 2048 + 2048),
        (
            "ranges.csv",
            plan.range_count(meta.source.bytes) * 2048 + 2048,
        ),
    ] {
        require(
            io::open_regular(&root.join(name), false)?.metadata()?.len() <= limit,
            "LIMIT_EXCEEDED",
            "CSV file exceeds row/line bounds",
        )?;
    }
    let mut current = hashes(root)?;
    require(
        io::read_small(&root.join("checksums.sha256"), 4096)? == checksum_text(&current).as_bytes(),
        "INDEX_HASH_MISMATCH",
        "metadata/index checksum list",
    )?;
    if require_commit {
        let commit: Commit = io::read_json(&root.join("COMMITTED"))?;
        current.insert(
            "checksums.sha256".into(),
            io::hash_file(&io::open_regular(&root.join("checksums.sha256"), false)?)?,
        );
        require(
            commit.format_version == 1
                && commit.backup_id == meta.backup_id
                && commit.hashes == current
                && commit.verification_kind == "buffered_reopen",
            "COMMIT_MISMATCH",
            "commit identity/checksums/verification kind",
        )?;
    }
    let mut index = Index::new(plan.range_count(meta.source.bytes), scratch)?;
    let mut files = vec![];
    let mut rows = CsvRows::<FileEntry>::open(&root.join("files.csv"), &types::FILE_COLUMNS)?;
    report.verified = 0;
    report.stored = 0;
    report.emit("VERIFYING", "VERIFY_STARTED");
    while let Some(e) = rows.next_row()? {
        require(
            (files.len() as u64) < MAX_FILES && e.file_id == files.len() as u64,
            "INVALID_INDEX",
            "file count/order/duplicate id",
        )?;
        archive::scan_sealed(root, &e, id, &meta, &plan, |r, offset, _| {
            report.gate(control, "VERIFYING")?;
            index.put(r, offset)?;
            report.verified += r.raw_bytes;
            if index.present % 64 == 0 {
                report.emit("VERIFYING", "RANGE_VERIFIED");
            }
            Ok(())
        })?;
        report.stored += e.stored_bytes;
        files.push(e);
    }
    require(
        index.present == index.count,
        "MISSING_RANGE",
        "source coverage incomplete",
    )?;
    let mut ranges = CsvRows::<RangeEntry>::open(&root.join("ranges.csv"), &types::RANGE_COLUMNS)?;
    for range_id in 0..index.count {
        let e = ranges
            .next_row()?
            .ok_or_else(|| Error::new("MISSING_RANGE", "ranges.csv is incomplete"))?;
        let (r, offset) = index.get(range_id)?;
        require(
            e == r.entry(offset),
            "INDEX_RECORD_MISMATCH",
            "CSV and container disagree, or source order is invalid",
        )?;
    }
    require(
        ranges.next_row()?.is_none(),
        "INVALID_INDEX",
        "extra range rows",
    )?;
    let actual = fs::read_dir(root.join("data"))?
        .map(|entry| entry.map(|e| e.file_name()))
        .collect::<std::io::Result<BTreeSet<_>>>()?;
    let expected = files
        .iter()
        .map(|e| Path::new(&e.relative_path).file_name().unwrap().to_owned())
        .collect::<BTreeSet<_>>();
    require(
        actual == expected,
        "UNREFERENCED_DATA",
        "unexpected or partial files in data directory; preserved",
    )?;
    report.emit("VERIFYING", "ALL_RANGES_VERIFIED");
    Ok(Verified {
        metadata: meta,
        plan,
        id,
        files,
        index,
    })
}
pub fn verify(root: &Path, scratch: &Path, events: &dyn EventSink) -> Result<Report> {
    verify_controlled(
        root,
        scratch,
        events,
        &std::sync::atomic::AtomicBool::new(false),
    )
}
pub fn verify_controlled(
    root: &Path,
    scratch: &Path,
    events: &dyn EventSink,
    control: &dyn ExecutionControl,
) -> Result<Report> {
    let root = io::canonical_archive(root)?;
    if crate::v2::is_v2(&root)? {
        return crate::v2::verify_controlled(&root, events, control);
    }
    let _lock = io::Lock::acquire(&root, false)?;
    let (meta, _, _) = archive::load(&root)?;
    let mut report = Reporter::new(events, meta.backup_id, meta.source.bytes);
    report.durable = meta.source.bytes;
    let v = inspect(&root, scratch, true, &mut report, control)?;
    report.emit("COMPLETE", "BACKUP_VERIFIED");
    Ok(v.report())
}
