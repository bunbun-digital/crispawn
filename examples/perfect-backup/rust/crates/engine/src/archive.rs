// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
use crate::{codec, io, Error, Result};
use perfect_backup_core::{
    error::require,
    format::{self, Record, FILE_HEADER, FOOTER, INDEX_ROW, RECORD_HEADER},
    plan::{Plan, MAX_FILES},
    types::{self, FileEntry, Metadata, RangeEntry},
};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{BufRead, BufReader, Read},
    path::Path,
};

pub struct Index {
    file: tempfile::NamedTempFile,
    seen: Vec<u8>,
    pub count: u64,
    pub present: u64,
}
impl Index {
    pub fn new(count: u64, scratch: &Path) -> Result<Self> {
        require(
            count > 0 && count <= perfect_backup_core::plan::MAX_RANGES,
            "LIMIT_EXCEEDED",
            "range index",
        )?;
        let file = tempfile::Builder::new()
            .prefix(".pbk-index-")
            .tempfile_in(scratch)?;
        file.as_file().set_len(count * INDEX_ROW as u64)?;
        Ok(Self {
            file,
            seen: vec![0; ((count + 7) / 8) as usize],
            count,
            present: 0,
        })
    }
    pub fn contains(&self, id: u64) -> bool {
        id < self.count && self.seen[id as usize / 8] & (1 << (id % 8)) != 0
    }
    pub fn put(&mut self, record: &Record, offset: u64) -> Result<()> {
        require(
            record.range_id < self.count && !self.contains(record.range_id),
            "DUPLICATE_RANGE",
            "duplicate or invalid range id",
        )?;
        let mut row = [0; INDEX_ROW];
        row[..RECORD_HEADER].copy_from_slice(&record.encode());
        row[RECORD_HEADER..].copy_from_slice(&offset.to_le_bytes());
        io::write_all_at(
            self.file.as_file(),
            &row,
            record.range_id * INDEX_ROW as u64,
        )?;
        self.seen[record.range_id as usize / 8] |= 1 << (record.range_id % 8);
        self.present += 1;
        Ok(())
    }
    pub fn get(&self, id: u64) -> Result<(Record, u64)> {
        require(self.contains(id), "MISSING_RANGE", "range index has a gap")?;
        let mut row = [0; INDEX_ROW];
        io::read_exact_at(
            self.file.as_file(),
            &mut row,
            id * INDEX_ROW as u64,
            INDEX_ROW as u64,
        )?;
        Ok((
            Record::decode(&row[..RECORD_HEADER])?,
            u64::from_le_bytes(row[RECORD_HEADER..].try_into().unwrap()),
        ))
    }
}
pub fn load(root: &Path) -> Result<(Metadata, Plan, [u8; 16])> {
    let m: Metadata = io::read_json(&root.join("metadata.json"))?;
    let p: Plan = io::read_json(&root.join("plan.json"))?;
    require(
        m.format_version == 1
            && p.format_version == 1
            && m.hash_algorithm == "sha256"
            && match &m.source.block_device {
                None => {
                    m.source_kind == "regular-file-fixture"
                        && m.logical_sector_bytes == 512
                        && m.physical_sector_bytes == 512
                }
                Some(b) => {
                    m.source_kind == "linux-whole-block-device"
                        && !b.stable_id.is_empty()
                        && [512, 4096].contains(&b.logical_sector_bytes)
                        && b.physical_sector_bytes >= b.logical_sector_bytes
                        && b.physical_sector_bytes.is_power_of_two()
                        && m.logical_sector_bytes == b.logical_sector_bytes
                        && m.physical_sector_bytes == b.physical_sector_bytes
                }
            },
        "INVALID_METADATA",
        "unsupported format/source/hash/sector adapter",
    )?;
    p.validate(m.source.bytes)?;
    let id = types::unhex::<16>(&m.backup_id)?;
    Ok((m, p, id))
}
pub fn record_from_entry(
    e: &RangeEntry,
    id: [u8; 16],
    plan: &Plan,
    source_bytes: u64,
) -> Result<Record> {
    require(
        e.record_length_bytes >= RECORD_HEADER as u64,
        "INVALID_INDEX",
        "short record",
    )?;
    let level = if matches!(e.codec, types::Codec::Raw | types::Codec::Zero) {
        0
    } else {
        plan.compression_level
    };
    require(
        e.codec_params_id == e.codec.params(level),
        "INVALID_INDEX",
        "codec parameter id",
    )?;
    let r = Record {
        backup_id: id,
        file_id: e.file_id,
        range_id: e.range_id,
        source_offset: e.source_offset_bytes,
        raw_bytes: e.source_length_bytes,
        payload_bytes: e.record_length_bytes - RECORD_HEADER as u64,
        codec: e.codec,
        level,
        raw_hash: types::unhex(&e.raw_sha256)?,
    };
    r.validate(id, e.file_id, plan, source_bytes)?;
    Ok(r)
}

pub struct Scan {
    pub end: u64,
    pub count: u64,
    pub raw: u64,
    pub hash: Sha256,
}
pub fn scan_prefix(
    file: &File,
    id: [u8; 16],
    file_id: u64,
    end: u64,
    meta: &Metadata,
    plan: &Plan,
    mut visit: impl FnMut(&Record, u64, &[u8]) -> Result<()>,
) -> Result<Scan> {
    require(
        end >= FILE_HEADER as u64
            && end <= plan.container_bytes - FOOTER as u64
            && file.metadata()?.len() >= end,
        "INVALID_CONTAINER",
        "prefix length",
    )?;
    let mut head = [0; FILE_HEADER];
    io::read_exact_at(file, &mut head, 0, FILE_HEADER as u64)?;
    format::check_file_header(&head, id, file_id)?;
    let mut h = Sha256::new();
    h.update(head);
    let (mut pos, mut count, mut total) = (FILE_HEADER as u64, 0, 0);
    while pos < end {
        require(
            end - pos >= RECORD_HEADER as u64,
            "INVALID_CONTAINER",
            "partial record header",
        )?;
        let mut b = [0; RECORD_HEADER];
        io::read_exact_at(file, &mut b, pos, RECORD_HEADER as u64)?;
        let r = Record::decode(&b)?;
        r.validate(id, file_id, plan, meta.source.bytes)?;
        require(
            r.payload_bytes <= end - pos - RECORD_HEADER as u64,
            "INVALID_CONTAINER",
            "payload exceeds prefix",
        )?;
        let mut payload = vec![0; r.payload_bytes as usize];
        io::read_exact_at(
            file,
            &mut payload,
            pos + RECORD_HEADER as u64,
            plan.read_bytes,
        )?;
        let raw = codec::decode(r.codec, &payload, r.raw_bytes as usize)?;
        require(
            types::hash(&raw) == r.raw_hash,
            "RAW_HASH_MISMATCH",
            "decoded range checksum",
        )?;
        visit(&r, pos, &raw)?;
        h.update(b);
        h.update(&payload);
        pos += RECORD_HEADER as u64 + r.payload_bytes;
        count += 1;
        total += r.raw_bytes;
    }
    Ok(Scan {
        end: pos,
        count,
        raw: total,
        hash: h,
    })
}
pub fn scan_sealed(
    root: &Path,
    e: &FileEntry,
    id: [u8; 16],
    meta: &Metadata,
    plan: &Plan,
    visit: impl FnMut(&Record, u64, &[u8]) -> Result<()>,
) -> Result<()> {
    require(
        e.file_id < MAX_FILES && e.relative_path == types::data_name(e.file_id, false),
        "INVALID_PATH",
        "container path must match generated file id",
    )?;
    require(
        e.stored_bytes >= (FILE_HEADER + FOOTER) as u64 && e.stored_bytes <= plan.container_bytes,
        "INVALID_CONTAINER",
        "stored length",
    )?;
    types::unhex::<32>(&e.stored_sha256)?;
    let file = io::open_regular(&root.join(&e.relative_path), false)?;
    require(
        file.metadata()?.len() == e.stored_bytes,
        "STORED_LENGTH_MISMATCH",
        "container length",
    )?;
    let end = e.stored_bytes - FOOTER as u64;
    let mut scan = scan_prefix(&file, id, e.file_id, end, meta, plan, visit)?;
    require(
        scan.count == e.range_count && scan.raw == e.raw_bytes_total && scan.count > 0,
        "INVALID_CONTAINER",
        "file range totals",
    )?;
    let mut b = [0; FOOTER];
    io::read_exact_at(&file, &mut b, end, FOOTER as u64)?;
    format::check_footer(&b, id, e.file_id, scan.count, scan.raw, end)?;
    scan.hash.update(b);
    require(
        types::hex(&scan.hash.finalize()) == e.stored_sha256,
        "STORED_HASH_MISMATCH",
        "container checksum",
    )
}

pub fn bounded_line(reader: &mut impl BufRead, limit: usize) -> Result<Option<Vec<u8>>> {
    let mut b = Vec::new();
    (&mut *reader)
        .take(limit as u64 + 1)
        .read_until(b'\n', &mut b)?;
    require(
        b.len() <= limit,
        "LIMIT_EXCEEDED",
        "index/journal line too long",
    )?;
    Ok(if b.is_empty() { None } else { Some(b) })
}
pub struct CsvRows<T> {
    reader: BufReader<io::Retrying<File>>,
    headers: csv::StringRecord,
    marker: std::marker::PhantomData<T>,
}
impl<T: serde::de::DeserializeOwned> CsvRows<T> {
    pub fn open(path: &Path, columns: &[&str]) -> Result<Self> {
        let mut reader = BufReader::new(io::Retrying(io::open_regular(path, false)?));
        let line = bounded_line(&mut reader, 2048)?
            .ok_or_else(|| Error::new("INVALID_CSV", "missing header"))?;
        let headers = parse_csv_line(&line)?;
        require(
            headers.iter().eq(columns.iter().copied()),
            "INVALID_CSV",
            "unexpected columns/order",
        )?;
        Ok(Self {
            reader,
            headers,
            marker: std::marker::PhantomData,
        })
    }
    pub fn next_row(&mut self) -> Result<Option<T>> {
        match bounded_line(&mut self.reader, 2048)? {
            None => Ok(None),
            Some(line) => Ok(Some(
                parse_csv_line(&line)?
                    .deserialize(Some(&self.headers))
                    .map_err(|e| Error::new("INVALID_CSV", e.to_string()))?,
            )),
        }
    }
}
fn parse_csv_line(line: &[u8]) -> Result<csv::StringRecord> {
    let mut r = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(false)
        .from_reader(line);
    let mut iter = r.records();
    let row = iter
        .next()
        .ok_or_else(|| Error::new("INVALID_CSV", "empty row"))?
        .map_err(|e| Error::new("INVALID_CSV", e.to_string()))?;
    require(
        iter.next().is_none(),
        "INVALID_CSV",
        "multiple records per physical line",
    )?;
    Ok(row)
}
pub fn write_indices(root: &Path, files: &[FileEntry], index: &Index) -> Result<()> {
    require(
        index.present == index.count,
        "MISSING_RANGE",
        "cannot commit incomplete coverage",
    )?;
    for name in ["files.csv", "ranges.csv"] {
        let mut temp = tempfile::Builder::new()
            .prefix(".pbk-csv-")
            .tempfile_in(root)?;
        {
            let mut w = csv::WriterBuilder::new()
                .terminator(csv::Terminator::CRLF)
                .has_headers(false)
                .from_writer(io::Retrying(&mut temp));
            if name == "files.csv" {
                w.write_record(types::FILE_COLUMNS).map_err(csv_error)?;
                for e in files {
                    w.serialize(e).map_err(csv_error)?;
                }
            } else {
                w.write_record(types::RANGE_COLUMNS).map_err(csv_error)?;
                for i in 0..index.count {
                    let (r, o) = index.get(i)?;
                    w.serialize(r.entry(o)).map_err(csv_error)?;
                }
            }
            w.flush()?;
        }
        io::sync_file(temp.as_file())?;
        temp.persist(root.join(name))
            .map_err(|e| Error::from(e.error))?;
        io::sync_dir(root)?;
    }
    Ok(())
}
fn csv_error(e: csv::Error) -> Error {
    Error::new("CSV_ERROR", e.to_string())
}
