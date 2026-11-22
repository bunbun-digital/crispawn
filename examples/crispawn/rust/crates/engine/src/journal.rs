// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
use crate::{
    archive::{self, Index},
    io, Error, Hooks, Result,
};
use crispawn_core::{
    error::require,
    format::{Record, FILE_HEADER, FOOTER, RECORD_HEADER},
    plan::{Plan, MAX_FILES},
    types::{self, FileEntry, Metadata, RangeEntry},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{BufReader, Seek, SeekFrom},
    path::Path,
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Change {
    Checkpoint {
        file_id: u64,
        end: String,
        records: Vec<RangeEntry>,
    },
    Seal {
        file: FileEntry,
    },
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Body {
    format_version: u32,
    backup_id: String,
    seq: u64,
    previous_digest: String,
    change: Change,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Group {
    body: Body,
    digest: String,
}
#[derive(Clone)]
pub struct FileState {
    pub id: u64,
    pub end: u64,
    pub count: u64,
    pub raw: u64,
    pub sealed: Option<FileEntry>,
}
impl FileState {
    pub fn new(id: u64) -> Self {
        Self {
            id,
            end: FILE_HEADER as u64,
            count: 0,
            raw: 0,
            sealed: None,
        }
    }
}
pub struct Replay {
    pub files: Vec<FileState>,
    pub index: Index,
    pub valid_bytes: u64,
    pub seq: u64,
    pub digest: String,
    pub durable_raw: u64,
}
pub struct Journal {
    file: File,
    id: String,
    seq: u64,
    digest: String,
}
impl Journal {
    pub fn open(root: &Path, id: String, replay: &Replay) -> Result<Self> {
        let mut file = io::open_regular(&root.join("journal.jsonl"), true)?;
        file.set_len(replay.valid_bytes)?;
        file.seek(SeekFrom::End(0))?;
        io::sync_file(&file)?;
        Ok(Self {
            file,
            id,
            seq: replay.seq,
            digest: replay.digest.clone(),
        })
    }
    pub fn append(&mut self, change: Change, hooks: &dyn Hooks) -> Result<()> {
        let body = Body {
            format_version: 1,
            backup_id: self.id.clone(),
            seq: self.seq,
            previous_digest: self.digest.clone(),
            change,
        };
        let digest = types::hex(&types::hash(
            &serde_json::to_vec(&body).map_err(json_error)?,
        ));
        let line = io::json(&Group {
            body,
            digest: digest.clone(),
        })?;
        require(
            line.len() <= 65536,
            "LIMIT_EXCEEDED",
            "journal group too large",
        )?;
        hooks.at("journal_write")?;
        io::write_all(&mut self.file, &line)?;
        hooks.at("after_journal_write")?;
        hooks.at("journal_sync")?;
        io::sync_file(&self.file)?;
        self.seq += 1;
        self.digest = digest;
        Ok(())
    }
}
pub fn replay(
    root: &Path,
    meta: &Metadata,
    plan: &Plan,
    id: [u8; 16],
    scratch: &Path,
) -> Result<Replay> {
    let mut out = Replay {
        files: vec![],
        index: Index::new(plan.range_count(meta.source.bytes), scratch)?,
        valid_bytes: 0,
        seq: 0,
        digest: "0".repeat(64),
        durable_raw: 0,
    };
    let mut reader = BufReader::new(io::Retrying(io::open_regular(
        &root.join("journal.jsonl"),
        false,
    )?));
    while let Some(line) = archive::bounded_line(&mut reader, 65536)? {
        if !line.ends_with(b"\n") {
            break;
        } // Torn final group is never promoted to durable work.
        let group: Group = serde_json::from_slice(&line).map_err(json_error)?;
        require(
            group.body.format_version == 1
                && group.body.backup_id == meta.backup_id
                && group.body.seq == out.seq
                && group.body.previous_digest == out.digest,
            "JOURNAL_CHAIN_MISMATCH",
            "sequence/identity",
        )?;
        require(
            group.digest
                == types::hex(&types::hash(
                    &serde_json::to_vec(&group.body).map_err(json_error)?,
                )),
            "JOURNAL_HASH_MISMATCH",
            "group checksum",
        )?;
        match group.body.change {
            Change::Checkpoint {
                file_id,
                end,
                records,
            } => {
                require(
                    !records.is_empty() && records.len() <= 64 && file_id < MAX_FILES,
                    "INVALID_JOURNAL",
                    "checkpoint bounds",
                )?;
                if file_id == out.files.len() as u64 {
                    require(
                        out.files.last().map(|f| f.sealed.is_some()).unwrap_or(true),
                        "INVALID_JOURNAL",
                        "previous container is not sealed",
                    )?;
                    out.files.push(FileState::new(file_id));
                }
                require(
                    file_id + 1 == out.files.len() as u64,
                    "INVALID_JOURNAL",
                    "checkpoint file order",
                )?;
                let file = out.files.last_mut().unwrap();
                require(
                    file.sealed.is_none(),
                    "INVALID_JOURNAL",
                    "checkpoint after seal",
                )?;
                for e in records {
                    require(
                        e.file_id == file_id && e.record_offset_bytes == file.end,
                        "INVALID_JOURNAL",
                        "record location",
                    )?;
                    let r = archive::record_from_entry(&e, id, plan, meta.source.bytes)?;
                    require(
                        e.record_length_bytes <= plan.container_bytes - FOOTER as u64 - file.end,
                        "INVALID_JOURNAL",
                        "container bound",
                    )?;
                    out.index.put(&r, file.end)?;
                    file.end += RECORD_HEADER as u64 + r.payload_bytes;
                    file.count += 1;
                    file.raw += r.raw_bytes;
                    out.durable_raw += r.raw_bytes;
                }
                require(
                    types::parse_decimal(&end)? == file.end,
                    "INVALID_JOURNAL",
                    "prefix end",
                )?;
            }
            Change::Seal { file: e } => {
                require(
                    e.file_id < MAX_FILES && e.file_id + 1 == out.files.len() as u64,
                    "INVALID_JOURNAL",
                    "seal order",
                )?;
                let f = out.files.last_mut().unwrap();
                require(
                    f.sealed.is_none()
                        && e.relative_path == types::data_name(f.id, false)
                        && e.stored_bytes == f.end + FOOTER as u64
                        && e.range_count == f.count
                        && e.raw_bytes_total == f.raw
                        && f.count > 0,
                    "INVALID_JOURNAL",
                    "seal totals",
                )?;
                types::unhex::<32>(&e.stored_sha256)?;
                f.sealed = Some(e);
            }
        }
        out.valid_bytes += line.len() as u64;
        out.seq += 1;
        out.digest = group.digest;
    }
    Ok(out)
}
pub fn compare_record(index: &Index, r: &Record, offset: u64) -> Result<()> {
    let (expected, pos) = index.get(r.range_id)?;
    require(
        expected == *r && pos == offset,
        "JOURNAL_RECORD_MISMATCH",
        "data differs from committed journal",
    )
}
fn json_error(e: serde_json::Error) -> Error {
    Error::new("INVALID_JOURNAL", e.to_string())
}
