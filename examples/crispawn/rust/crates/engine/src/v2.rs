// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! Contiguous source spans, one standard parallel BLAKE3 per backup file.
use crate::{
    archive::CsvRows, codec, control::ExecutionControl, io, parallel_hash::CpuPool,
    v2_format as format, verify, Error, Reporter, Result,
};
use crispawn_core::{
    error::require,
    plan::{Plan, MAX_BACKUP_FILE_BYTES, MAX_FILES},
    types::{self, decimal, EventSink, Metadata},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    os::unix::fs::MetadataExt,
    path::Path,
};

pub(crate) fn open_owned(path: &Path) -> Result<File> {
    let file = io::open_regular(path, true)?;
    require(
        file.metadata()?.nlink() == 1,
        "UNSAFE_PATH",
        "mutable v2 file must not be a hard link",
    )?;
    Ok(file)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileEntry {
    #[serde(with = "decimal")]
    pub file_id: u64,
    pub relative_path: String,
    #[serde(with = "decimal")]
    pub source_offset_bytes: u64,
    #[serde(with = "decimal")]
    pub source_length_bytes: u64,
    #[serde(with = "decimal")]
    pub stored_bytes: u64,
    #[serde(with = "decimal")]
    pub frame_count: u64,
    pub raw_blake3: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression_profiles: Option<String>,
}
pub const FILE_COLUMNS: [&str; 7] = [
    "file_id",
    "relative_path",
    "source_offset_bytes",
    "source_length_bytes",
    "stored_bytes",
    "frame_count",
    "raw_blake3",
];
pub const V3_COLUMNS: [&str; 8] = [
    "file_id",
    "relative_path",
    "source_offset_bytes",
    "source_length_bytes",
    "stored_bytes",
    "frame_count",
    "raw_blake3",
    "compression_profiles",
];
const INDEX_FILES: [&str; 3] = ["files.csv", "metadata.json", "plan.json"];

pub fn is_v2(root: &Path) -> Result<bool> {
    let meta: Metadata = io::read_json(&root.join("metadata.json"))?;
    Ok(matches!(meta.format_version, 2 | 3))
}

pub(crate) fn load(root: &Path) -> Result<(Metadata, Plan, [u8; 16])> {
    let meta: Metadata = io::read_json(&root.join("metadata.json"))?;
    let plan: Plan = io::read_json(&root.join("plan.json"))?;
    require(
        matches!(meta.format_version, 2 | 3)
            && plan.format_version == meta.format_version
            && meta.hash_algorithm == "blake3",
        "INVALID_METADATA",
        "v2 format, plan and hash algorithm",
    )?;
    let valid_source = match &meta.source.block_device {
        None => {
            meta.source_kind == "regular-file-fixture"
                && meta.logical_sector_bytes == 512
                && meta.physical_sector_bytes == 512
        }
        Some(device) => {
            meta.source_kind == "linux-whole-block-device"
                && !device.stable_id.is_empty()
                && [512, 4096].contains(&device.logical_sector_bytes)
                && device.physical_sector_bytes >= device.logical_sector_bytes
                && device.physical_sector_bytes.is_power_of_two()
                && meta.logical_sector_bytes == device.logical_sector_bytes
                && meta.physical_sector_bytes == device.physical_sector_bytes
        }
    };
    require(
        valid_source,
        "INVALID_METADATA",
        "v2 source and sector geometry",
    )?;
    plan.validate(meta.source.bytes)?;
    let id = types::unhex::<16>(&meta.backup_id)?;
    Ok((meta, plan, id))
}

pub(crate) fn check_entry(entry: &FileEntry, meta: &Metadata, plan: &Plan) -> Result<()> {
    require(
        entry.file_id < MAX_FILES && entry.relative_path == types::data_name(entry.file_id, false),
        "UNSAFE_PATH",
        "v2 file ID and canonical relative path",
    )?;
    require(
        entry.source_length_bytes > 0
            && entry.source_offset_bytes < meta.source.bytes
            && entry.source_offset_bytes % plan.range_bytes == 0
            && entry.source_length_bytes <= meta.source.bytes - entry.source_offset_bytes,
        "INVALID_INDEX",
        "v2 contiguous source span",
    )?;
    require(
        entry.stored_bytes >= (format::FILE_HEADER + format::FRAME_HEADER + format::FOOTER) as u64
            && entry.stored_bytes <= plan.container_bytes
            && entry.stored_bytes <= MAX_BACKUP_FILE_BYTES,
        "FILE_SIZE_LIMIT",
        "v2 backup file exceeds its configured limit or 2GB",
    )?;
    let count = plan.range_count(entry.source_length_bytes);
    require(
        entry.frame_count == count && count > 0,
        "INVALID_INDEX",
        "v2 frame count",
    )?;
    types::unhex::<32>(&entry.raw_blake3)?;
    require(
        if plan.format_version == 3 {
            entry
                .compression_profiles
                .as_deref()
                .is_some_and(format::valid_profiles)
        } else {
            entry.compression_profiles.is_none()
        },
        "INVALID_INDEX",
        "codec profile column disagrees with archive version",
    )?;
    Ok(())
}

/// Decode in source order with bounded buffers and verify the original-data BLAKE3.
/// The caller must not regard any individual frame as verified before this returns.
pub(crate) fn scan_file(
    root: &Path,
    entry: &FileEntry,
    meta: &Metadata,
    plan: &Plan,
    id: [u8; 16],
    pool: &CpuPool,
    mut visit: impl FnMut(u64, &[u8]) -> Result<()>,
) -> Result<()> {
    check_entry(entry, meta, plan)?;
    let file = io::open_regular(&root.join(&entry.relative_path), false)?;
    require(
        file.metadata()?.len() == entry.stored_bytes,
        "FILE_CHANGED",
        "v2 stored length",
    )?;
    let mut header = [0; format::FILE_HEADER];
    io::read_exact_at(&file, &mut header, 0, plan.read_bytes)?;
    format::check_file_header(
        plan.format_version,
        &header,
        id,
        entry.file_id,
        entry.source_offset_bytes,
    )?;
    let end = entry.stored_bytes - format::FOOTER as u64;
    let mut position = format::FILE_HEADER as u64;
    let mut raw_length = 0u64;
    let mut count = 0u64;
    let mut hash = blake3::Hasher::new();
    let mut profiles = BTreeSet::new();
    while position < end {
        require(
            end - position >= format::FRAME_HEADER as u64,
            "TRUNCATED_RECORD",
            "v2 frame header",
        )?;
        let mut encoded = [0; format::FRAME_HEADER];
        io::read_exact_at(&file, &mut encoded, position, plan.read_bytes)?;
        let frame = format::Frame::decode(&encoded, plan)?;
        profiles.insert(format::profile_name(frame.codec, frame.level));
        position += format::FRAME_HEADER as u64;
        require(
            u64::from(frame.payload_bytes) <= end - position,
            "TRUNCATED_RECORD",
            "v2 payload",
        )?;
        require(
            raw_length < entry.source_length_bytes
                && frame.raw_bytes == plan.range_bytes.min(entry.source_length_bytes - raw_length),
            "INVALID_RECORD",
            "v2 frame source coverage",
        )?;
        let mut payload = vec![0; frame.payload_bytes as usize];
        io::read_exact_at(&file, &mut payload, position, plan.read_bytes)?;
        let raw = codec::decode(frame.codec, &payload, frame.raw_bytes as usize)?;
        pool.update(&mut hash, &raw);
        visit(entry.source_offset_bytes + raw_length, &raw)?;
        raw_length += frame.raw_bytes;
        position += u64::from(frame.payload_bytes);
        count += 1;
        require(
            count <= entry.frame_count,
            "INVALID_RECORD",
            "v2 excess frames",
        )?;
    }
    require(
        position == end && raw_length == entry.source_length_bytes && count == entry.frame_count,
        "MISSING_RANGE",
        "v2 file coverage or frame count",
    )?;
    if plan.format_version == 3 {
        require(
            entry.compression_profiles.as_deref()
                == Some(profiles.into_iter().collect::<Vec<_>>().join(";").as_str()),
            "CODEC_PROFILE_MISMATCH",
            "index codec set disagrees with frame headers",
        )?;
    }
    let actual = *hash.finalize().as_bytes();
    require(
        actual == types::unhex::<32>(&entry.raw_blake3)?,
        "FILE_HASH_MISMATCH",
        "restored original-data BLAKE3 does not match the backup file",
    )?;
    let mut footer = [0; format::FOOTER];
    io::read_exact_at(&file, &mut footer, end, plan.read_bytes)?;
    format::check_footer(
        plan.format_version,
        &footer,
        id,
        entry.file_id,
        entry.source_offset_bytes,
        raw_length,
        count,
        actual,
    )?;
    require(
        file.metadata()?.len() == entry.stored_bytes,
        "FILE_CHANGED",
        "v2 file grew or shrank",
    )
}

fn hashes(root: &Path) -> Result<BTreeMap<String, String>> {
    INDEX_FILES
        .into_iter()
        .map(|name| {
            Ok((
                name.to_owned(),
                io::hash_file(&io::open_regular(&root.join(name), false)?)?,
            ))
        })
        .collect()
}
fn checksum_text(values: &BTreeMap<String, String>) -> String {
    values
        .iter()
        .map(|(name, hash)| format!("{hash}  {name}\n"))
        .collect()
}
pub(crate) fn write_indices(root: &Path, entries: &[FileEntry]) -> Result<()> {
    let mut entries = entries.to_vec();
    entries.sort_by_key(|entry| entry.source_offset_bytes);
    let mut temp = tempfile::Builder::new()
        .prefix(".pbk-v2-index-")
        .tempfile_in(root)?;
    {
        let mut writer = csv::WriterBuilder::new()
            .has_headers(false)
            .terminator(csv::Terminator::CRLF)
            .from_writer(io::Retrying(&mut temp));
        writer
            .write_record(
                if entries
                    .first()
                    .is_some_and(|e| e.compression_profiles.is_some())
                {
                    V3_COLUMNS.as_slice()
                } else {
                    FILE_COLUMNS.as_slice()
                },
            )
            .map_err(|e| Error::new("CSV_ERROR", e.to_string()))?;
        for entry in &entries {
            writer
                .serialize(entry)
                .map_err(|e| Error::new("CSV_ERROR", e.to_string()))?;
        }
        writer.flush()?;
    }
    io::sync_file(temp.as_file())?;
    temp.persist(root.join("files.csv"))
        .map_err(|e| Error::from(e.error))?;
    io::sync_dir(root)?;
    io::atomic_write(
        root,
        "checksums.sha256",
        checksum_text(&hashes(root)?).as_bytes(),
    )
}
pub(crate) fn write_commit(root: &Path, id: &str) -> Result<()> {
    let mut hashes = hashes(root)?;
    hashes.insert(
        "checksums.sha256".into(),
        io::hash_file(&io::open_regular(&root.join("checksums.sha256"), false)?)?,
    );
    io::atomic_write(
        root,
        "COMMITTED",
        &io::json(&verify::Commit {
            format_version: io::read_json::<Metadata>(&root.join("metadata.json"))?.format_version,
            backup_id: id.into(),
            hashes,
            verification_kind: "buffered_reopen".into(),
        })?,
    )
}

pub(crate) struct Verified {
    pub metadata: Metadata,
    pub plan: Plan,
    pub id: [u8; 16],
    pub files: Vec<FileEntry>,
}
impl Verified {
    pub fn report(&self) -> verify::Report {
        verify::Report {
            operation: "verify".into(),
            backup_id: self.metadata.backup_id.clone(),
            source_bytes: self.metadata.source.bytes.to_string(),
            range_count: self
                .plan
                .range_count(self.metadata.source.bytes)
                .to_string(),
            file_count: self.files.len().to_string(),
            verification_kind: "buffered_reopen".into(),
            dataset_committed: true,
            restored_ranges_verified: false,
        }
    }
}
pub(crate) fn inspect(
    root: &Path,
    require_commit: bool,
    report: &mut Reporter<'_>,
    control: &dyn ExecutionControl,
    pool: &CpuPool,
) -> Result<Verified> {
    let verified = manifest(root, require_commit, control)?;
    let Verified {
        metadata,
        plan,
        id,
        files,
    } = &verified;
    report.verified = 0;
    report.verification_read = 0;
    report.stored = 0;
    report.emit("VERIFYING", "VERIFY_STARTED");
    for entry in files {
        scan_file(root, entry, metadata, plan, *id, pool, |_, raw| {
            report.gate(control, "VERIFYING")?;
            report.verification_read += raw.len() as u64;
            report.emit("VERIFYING", "FILE_HASH_PROGRESS");
            Ok(())
        })?;
        report.verified += entry.source_length_bytes;
        report.stored += entry.stored_bytes;
        report.emit("VERIFYING", "FILE_VERIFIED");
    }
    report.emit("VERIFYING", "ALL_RANGES_VERIFIED");
    Ok(verified)
}

/// Metadata/index/coverage checks only. Backup may use this only after every
/// exact sealed FileEntry has separately passed decode + original-data BLAKE3.
pub(crate) fn manifest(
    root: &Path,
    require_commit: bool,
    control: &dyn ExecutionControl,
) -> Result<Verified> {
    control.check_cancel()?;
    let (metadata, plan, id) = load(root)?;
    let mut expected = hashes(root)?;
    require(
        io::read_small(&root.join("checksums.sha256"), 4096)?
            == checksum_text(&expected).as_bytes(),
        "INDEX_HASH_MISMATCH",
        "v2 metadata checksum list",
    )?;
    if require_commit {
        let commit: verify::Commit = io::read_json(&root.join("COMMITTED"))?;
        expected.insert(
            "checksums.sha256".into(),
            io::hash_file(&io::open_regular(&root.join("checksums.sha256"), false)?)?,
        );
        require(
            commit.format_version == metadata.format_version
                && commit.backup_id == metadata.backup_id
                && commit.hashes == expected
                && commit.verification_kind == "buffered_reopen",
            "COMMIT_MISMATCH",
            "v2 commit identity or metadata",
        )?;
    }
    require(
        io::open_regular(&root.join("files.csv"), false)?
            .metadata()?
            .len()
            <= MAX_FILES * 2048 + 2048,
        "LIMIT_EXCEEDED",
        "v2 files index",
    )?;
    let mut rows = CsvRows::<FileEntry>::open(
        &root.join("files.csv"),
        if plan.format_version == 3 {
            &V3_COLUMNS
        } else {
            &FILE_COLUMNS
        },
    )?;
    let mut files = Vec::new();
    let mut ids = BTreeSet::new();
    let mut covered = 0;
    while let Some(entry) = rows.next_row()? {
        check_entry(&entry, &metadata, &plan)?;
        require(
            (files.len() as u64) < MAX_FILES
                && ids.insert(entry.file_id)
                && entry.source_offset_bytes == covered,
            "INVALID_INDEX",
            "v2 duplicate, gap, overlap or source order",
        )?;
        covered += entry.source_length_bytes;
        files.push(entry);
    }
    require(
        covered == metadata.source.bytes,
        "MISSING_RANGE",
        "v2 source coverage is incomplete",
    )?;
    let actual = fs::read_dir(root.join("data"))?
        .map(|e| e.map(|x| x.file_name()))
        .collect::<std::io::Result<BTreeSet<_>>>()?;
    let expected = files
        .iter()
        .map(|e| Path::new(&e.relative_path).file_name().unwrap().to_owned())
        .collect();
    require(
        actual == expected,
        "UNREFERENCED_DATA",
        "unexpected or partial v2 data files; preserved",
    )?;
    Ok(Verified {
        metadata,
        plan,
        id,
        files,
    })
}

pub(crate) fn verify_controlled(
    root: &Path,
    events: &dyn EventSink,
    control: &dyn ExecutionControl,
) -> Result<verify::Report> {
    let root = io::canonical_archive(root)?;
    let _lock = io::Lock::acquire(&root, false)?;
    let (meta, plan, _) = load(&root)?;
    let pool = CpuPool::new(plan.workers)?;
    let mut report = Reporter::new(events, meta.backup_id, meta.source.bytes);
    report.durable = meta.source.bytes;
    let result = inspect(&root, true, &mut report, control, &pool)?;
    report.emit("COMPLETE", "BACKUP_VERIFIED");
    Ok(result.report())
}
