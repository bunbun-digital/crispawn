// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! Bounded, chained progress journal. Prefix hashes are temporary resume state;
//! the committed backup exposes only each file's final original-data BLAKE3.
use crate::{io, v2::FileEntry, v2_format, Error, Result};
use perfect_backup_core::{
    error::require,
    plan::{Plan, MAX_FILES, MAX_RANGES},
    types::{self, decimal, Metadata},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::Path,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prefix {
    #[serde(with = "decimal")]
    pub file_id: u64,
    #[serde(with = "decimal")]
    pub raw_bytes: u64,
    #[serde(with = "decimal")]
    pub stored_end: u64,
    #[serde(with = "decimal")]
    pub frames: u64,
    pub raw_blake3: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Change {
    Open {
        file_id: u64,
        region: u32,
        source_offset_bytes: String,
    },
    Checkpoint {
        prefix: Prefix,
    },
    Seal {
        entry: FileEntry,
    },
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    seq: u64,
    previous: String,
    change: Change,
    checksum: String,
}
#[derive(Clone)]
pub struct Partial {
    pub region: usize,
    pub source_offset: u64,
    pub prefix: Prefix,
}
pub struct State {
    pub sealed: Vec<FileEntry>,
    pub partial: BTreeMap<u64, Partial>,
    pub cursors: Vec<u64>,
    pub ends: Vec<u64>,
    pub used: BTreeSet<u64>,
}
impl State {
    pub fn new(meta: &Metadata, plan: &Plan) -> Self {
        let count = plan.range_count(meta.source.bytes);
        let regions = u64::from(plan.regions).min(count);
        let cursors = (0..regions)
            .map(|r| (count * r / regions) * plan.range_bytes)
            .collect();
        let ends = (0..regions)
            .map(|r| ((count * (r + 1) / regions) * plan.range_bytes).min(meta.source.bytes))
            .collect();
        Self {
            sealed: vec![],
            partial: BTreeMap::new(),
            cursors,
            ends,
            used: BTreeSet::new(),
        }
    }
    pub fn apply(&mut self, change: &Change, meta: &Metadata, plan: &Plan) -> Result<()> {
        match change {
            Change::Open {
                file_id,
                region,
                source_offset_bytes,
            } => {
                let r = *region as usize;
                let offset = types::parse_decimal(source_offset_bytes)?;
                require(
                    *file_id < MAX_FILES
                        && self.used.len() < (MAX_FILES as usize)
                        && !self.used.contains(file_id)
                        && r < self.cursors.len()
                        && offset == self.cursors[r]
                        && offset < self.ends[r]
                        && !self.partial.values().any(|p| p.region == r),
                    "INVALID_JOURNAL",
                    "v2 open identity, region or source position",
                )?;
                self.used.insert(*file_id);
                self.partial.insert(
                    *file_id,
                    Partial {
                        region: r,
                        source_offset: offset,
                        prefix: Prefix {
                            file_id: *file_id,
                            raw_bytes: 0,
                            stored_end: v2_format::FILE_HEADER as u64,
                            frames: 0,
                            raw_blake3: blake3::hash(b"").to_hex().to_string(),
                        },
                    },
                );
            }
            Change::Checkpoint { prefix } => {
                let p = self
                    .partial
                    .get_mut(&prefix.file_id)
                    .ok_or_else(|| Error::new("INVALID_JOURNAL", "unknown v2 checkpoint file"))?;
                types::unhex::<32>(&prefix.raw_blake3)?;
                require(
                    prefix.raw_bytes >= p.prefix.raw_bytes
                        && prefix.raw_bytes <= self.ends[p.region] - p.source_offset
                        && (prefix.raw_bytes % plan.range_bytes == 0
                            || p.source_offset + prefix.raw_bytes == meta.source.bytes)
                        && prefix.frames == plan.range_count(prefix.raw_bytes)
                        && prefix.stored_end >= p.prefix.stored_end
                        && prefix.stored_end <= plan.container_bytes - v2_format::FOOTER as u64
                        && prefix.stored_end
                            >= v2_format::FILE_HEADER as u64
                                + prefix.frames * v2_format::FRAME_HEADER as u64,
                    "INVALID_JOURNAL",
                    "v2 checkpoint extent, count or monotonicity",
                )?;
                if prefix.raw_bytes == p.prefix.raw_bytes {
                    require(
                        prefix.stored_end == p.prefix.stored_end
                            && prefix.raw_blake3 == p.prefix.raw_blake3,
                        "INVALID_JOURNAL",
                        "v2 checkpoint changed without new source data",
                    )?;
                }
                p.prefix = prefix.clone();
            }
            Change::Seal { entry } => {
                crate::v2::check_entry(entry, meta, plan)?;
                let p = self
                    .partial
                    .get(&entry.file_id)
                    .ok_or_else(|| Error::new("INVALID_JOURNAL", "unknown sealed v2 file"))?;
                require(
                    entry.source_offset_bytes == p.source_offset
                        && entry.source_length_bytes == p.prefix.raw_bytes
                        && entry.frame_count == p.prefix.frames
                        && entry.stored_bytes == p.prefix.stored_end + v2_format::FOOTER as u64
                        && entry.raw_blake3 == p.prefix.raw_blake3,
                    "INVALID_JOURNAL",
                    "sealed v2 file disagrees with checkpoint",
                )?;
                self.cursors[p.region] = p.source_offset + p.prefix.raw_bytes;
                self.partial.remove(&entry.file_id);
                self.sealed.push(entry.clone());
            }
        }
        Ok(())
    }
}

fn checksum(seq: u64, previous: &str, change: &Change) -> Result<String> {
    let bytes = serde_json::to_vec(&(seq, previous, change))
        .map_err(|e| Error::new("JSON_ERROR", e.to_string()))?;
    Ok(types::hex(&types::hash(&bytes)))
}
pub struct Replay {
    pub state: State,
    pub end: u64,
    pub seq: u64,
    pub previous: String,
}
pub fn replay(root: &Path, meta: &Metadata, plan: &Plan) -> Result<Replay> {
    let file = io::open_regular(&root.join("journal.jsonl"), false)?;
    let mut reader = BufReader::new(io::Retrying(file));
    let mut state = State::new(meta, plan);
    let mut previous = types::hex(&[0; 32]);
    let mut seq = 0;
    let mut end = 0;
    loop {
        let mut line = Vec::new();
        let count = Read::by_ref(&mut reader)
            .take(4097)
            .read_until(b'\n', &mut line)?;
        if count == 0 {
            break;
        }
        require(
            count <= 4096,
            "INVALID_JOURNAL",
            "v2 journal line exceeds bound",
        )?;
        if !line.ends_with(b"\n") {
            break;
        }
        require(
            seq < MAX_RANGES * 2 + MAX_FILES * 4,
            "LIMIT_EXCEEDED",
            "v2 journal event count",
        )?;
        let record: Envelope = serde_json::from_slice(&line)
            .map_err(|e| Error::new("INVALID_JOURNAL", e.to_string()))?;
        require(
            record.seq == seq
                && record.previous == previous
                && record.checksum == checksum(seq, &previous, &record.change)?,
            "JOURNAL_CHECKSUM_MISMATCH",
            "v2 journal chain or sequence",
        )?;
        state.apply(&record.change, meta, plan)?;
        previous = record.checksum;
        seq += 1;
        end += count as u64;
    }
    Ok(Replay {
        state,
        end,
        seq,
        previous,
    })
}
pub struct Journal {
    file: File,
    pub state: State,
    seq: u64,
    previous: String,
}
impl Journal {
    /// Drop temporary checkpoint digests once every file is sealed. The final
    /// journal repeats only the same one canonical digest stored for each file.
    pub fn compact(&self, root: &Path, meta: &Metadata, plan: &Plan) -> Result<()> {
        require(
            self.state.partial.is_empty(),
            "INVALID_STATE",
            "cannot compact open v2 files",
        )?;
        let mut temp = tempfile::Builder::new()
            .prefix(".pbk-v2-journal-")
            .tempfile_in(root)?;
        let mut entries = self.state.sealed.clone();
        entries.sort_by_key(|e| e.source_offset_bytes);
        let mut state = State::new(meta, plan);
        let mut seq = 0;
        let mut previous = types::hex(&[0; 32]);
        for entry in entries {
            let region = state
                .cursors
                .iter()
                .enumerate()
                .position(|(r, c)| *c == entry.source_offset_bytes && *c < state.ends[r])
                .ok_or_else(|| Error::new("INVALID_JOURNAL", "sealed v2 region coverage"))?;
            for change in [
                Change::Open {
                    file_id: entry.file_id,
                    region: region as u32,
                    source_offset_bytes: entry.source_offset_bytes.to_string(),
                },
                Change::Checkpoint {
                    prefix: Prefix {
                        file_id: entry.file_id,
                        raw_bytes: entry.source_length_bytes,
                        stored_end: entry.stored_bytes - v2_format::FOOTER as u64,
                        frames: entry.frame_count,
                        raw_blake3: entry.raw_blake3.clone(),
                    },
                },
                Change::Seal { entry },
            ] {
                state.apply(&change, meta, plan)?;
                let digest = checksum(seq, &previous, &change)?;
                io::write_all(
                    &mut temp,
                    &io::json(&Envelope {
                        seq,
                        previous,
                        change,
                        checksum: digest.clone(),
                    })?,
                )?;
                previous = digest;
                seq += 1;
            }
        }
        io::sync_file(temp.as_file())?;
        temp.persist(root.join("journal.jsonl"))
            .map_err(|e| Error::from(e.error))?;
        io::sync_dir(root)
    }
    pub fn create(root: &Path, meta: &Metadata, plan: &Plan) -> Result<Self> {
        let file = io::create_new(&root.join("journal.jsonl"))?;
        io::sync_file(&file)?;
        Ok(Self {
            file,
            state: State::new(meta, plan),
            seq: 0,
            previous: types::hex(&[0; 32]),
        })
    }
    pub fn resume(root: &Path, replay: Replay) -> Result<Self> {
        let mut file = crate::v2::open_owned(&root.join("journal.jsonl"))?;
        file.set_len(replay.end)?;
        file.seek(SeekFrom::End(0))?;
        Ok(Self {
            file,
            state: replay.state,
            seq: replay.seq,
            previous: replay.previous,
        })
    }
    pub fn append(
        &mut self,
        change: Change,
        meta: &Metadata,
        plan: &Plan,
        hooks: &dyn crate::Hooks,
    ) -> Result<()> {
        self.state.apply(&change, meta, plan)?;
        let digest = checksum(self.seq, &self.previous, &change)?;
        let entry = Envelope {
            seq: self.seq,
            previous: self.previous.clone(),
            change,
            checksum: digest.clone(),
        };
        let bytes = io::json(&entry)?;
        require(
            bytes.len() <= 4096,
            "LIMIT_EXCEEDED",
            "v2 journal event size",
        )?;
        hooks.at("before_journal_write")?;
        io::write_all(&mut self.file, &bytes)?;
        hooks.at("after_journal_write")?;
        io::sync_file(&self.file)?;
        hooks.at("after_journal_sync")?;
        self.seq += 1;
        self.previous = digest;
        Ok(())
    }
}
