// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-digital. Licensed under Apache-2.0.
//! Physical part files are independent of the PBK compression-frame boundaries.
//! Acknowledgement requires fsync plus encoded-byte BLAKE3 readback, not decode.
use crate::{io, parallel_hash::CpuPool, Error, Hooks, NoHooks, Result};
use crispawn_core::{
    error::require,
    live::StorageStatus,
    plan::Plan,
    types::{self, EventSink, SourceIdentity},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

const FIRST: u64 = 1 << 20;
const MIN: u64 = 64 << 10;
const MAX: u64 = 16 << 20;
const MAX_PARTS: usize = 32768;
const MAX_DESCRIPTOR: u64 = 8 << 20;
const GOAL: f64 = 5.0;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Part {
    path: String,
    stream_offset: u64,
    bytes: u64,
    used: u64,
    blake3: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    kind: String,
    schema_version: u32,
    backup_id: String,
    file_id: u64,
    bytes: u64,
    parts: Vec<Part>,
}
impl Manifest {
    fn validate(&self) -> Result<()> {
        require(
            self.kind == "crispawn.segmented_file" && self.schema_version == 1,
            "UNKNOWN_VERSION",
            "segmented file descriptor",
        )?;
        types::unhex::<16>(&self.backup_id)?;
        require(
            self.file_id < crispawn_core::plan::MAX_FILES && self.parts.len() <= MAX_PARTS,
            "LIMIT_EXCEEDED",
            "storage part count or file ID",
        )?;
        let prefix = format!("segments/{:08x}/", self.file_id);
        let mut names = std::collections::BTreeSet::new();
        let mut length = 0u64;
        for p in &self.parts {
            let suffix = p
                .path
                .strip_prefix(&prefix)
                .and_then(|s| s.strip_suffix(".bin"))
                .ok_or_else(|| Error::new("INVALID_STORAGE_PART", "part path"))?;
            require(
                suffix.len() == 32
                    && suffix.bytes().all(|c| c.is_ascii_hexdigit())
                    && names.insert(&p.path)
                    && p.stream_offset == length
                    && p.bytes > 0
                    && p.bytes <= MAX
                    && p.used > 0
                    && p.used <= p.bytes,
                "INVALID_STORAGE_PART",
                "path, duplicate or part length",
            )?;
            types::unhex::<32>(&p.blake3)?;
            length = length
                .checked_add(p.used)
                .ok_or_else(|| Error::new("LIMIT_EXCEEDED", "part lengths"))?;
        }
        require(
            length == self.bytes && length <= crispawn_core::plan::MAX_BACKUP_FILE_BYTES,
            "INVALID_STORAGE_PART",
            "logical stream length",
        )
    }
}
struct State {
    status: StorageStatus,
    phase_since: Instant,
    buffered: u64,
    input: u64,
    baseline: u64,
    proofs: BTreeMap<PathBuf, (String, SourceIdentity)>,
    files: BTreeMap<u64, (SourceIdentity, String)>,
}
pub(crate) struct Shared {
    pub pool: Arc<CpuPool>,
    id: String,
    state: Mutex<State>,
    pub source_read: AtomicU64,
    pub queue_bytes: AtomicU64,
    pub cancelled: AtomicBool,
    done: AtomicBool,
    hooks: Arc<dyn Hooks>,
    capacity: u64,
    started: Instant,
}
pub(crate) struct StopMonitor(pub Arc<Shared>);
impl Drop for StopMonitor {
    fn drop(&mut self) {
        self.0.done.store(true, Ordering::Release);
    }
}
impl Shared {
    pub fn new(id: &str, workers: u32) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            pool: Arc::new(CpuPool::new(workers)?),
            id: id.into(),
            source_read: AtomicU64::new(0),
            queue_bytes: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
            done: AtomicBool::new(false),
            hooks: Arc::new(NoHooks),
            capacity: MAX,
            started: Instant::now(),
            state: Mutex::new(State {
                status: StorageStatus {
                    phase: "CALIBRATING".into(),
                    written_blocks: 0,
                    confirmed_blocks: 0,
                    waiting_blocks: 0,
                    queued_encoded_bytes: 0,
                    current_file_bytes: 0,
                    baseline_bytes: FIRST,
                    write_workers: 1,
                    written_bytes: 0,
                    confirmed_bytes: 0,
                    confirmed_source_bytes: 0,
                    phase_elapsed_ms: 0,
                    read_mb_per_second: 0.0,
                    write_mb_per_second: 0.0,
                    calibrated: false,
                    elapsed_seconds: 0,
                    calibration_bytes: 0,
                    calibration_elapsed_ms: 0,
                    last_write_elapsed_ms: 0,
                    last_check_elapsed_ms: 0,
                },
                phase_since: Instant::now(),
                buffered: 0,
                input: 0,
                baseline: FIRST,
                proofs: BTreeMap::new(),
                files: BTreeMap::new(),
            }),
        }))
    }
    pub fn new_for_plan(id: &str, plan: &Plan) -> Result<Arc<Self>> {
        let mut shared = Self::new(id, plan.workers)?;
        Arc::get_mut(&mut shared).unwrap().capacity = plan.storage_buffer_bytes();
        {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.baseline = FIRST.min(shared.capacity);
            state.status.baseline_bytes = state.baseline;
        }
        Ok(shared)
    }
    fn check(&self) -> Result<()> {
        require(
            !self.cancelled.load(Ordering::Acquire),
            "CANCELLED",
            "storage write cancelled",
        )
    }
    fn target(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .baseline
    }
    fn stage(&self, phase: &str, size: u64) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.status.phase = phase.into();
        s.status.current_file_bytes = size;
        s.phase_since = Instant::now();
    }
    pub fn input(&self, bytes: u64) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).input = bytes;
    }
    pub fn credit_source(&self, bytes: u64) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.status.confirmed_source_bytes = s.status.confirmed_source_bytes.saturating_add(bytes);
    }
    fn buffered(&self, added: u64, removed: u64) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.buffered = s.buffered.saturating_add(added).saturating_sub(removed);
        s.input = s.input.saturating_sub(added);
    }
    fn proof(&self, path: &Path, hash: &str, identity: SourceIdentity, new_write: bool) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !s.proofs.contains_key(path) {
            s.status.confirmed_bytes += identity.bytes;
            s.status.confirmed_blocks += 1;
            if !new_write {
                s.status.written_bytes += identity.bytes;
                s.status.written_blocks += 1;
            }
        }
        s.proofs.insert(path.into(), (hash.into(), identity));
    }
    fn calibrated_size(bytes: u64, seconds: f64) -> u64 {
        ((bytes as f64 * (GOAL * 0.8) / seconds.max(0.001)) as u64).clamp(MIN, MAX) / 4096 * 4096
    }
    fn calibrate(&self, bytes: u64, seconds: f64) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !s.status.calibrated {
            s.baseline = Self::calibrated_size(bytes, seconds).min(self.capacity);
            s.status.baseline_bytes = s.baseline;
            s.status.calibrated = true;
            s.status.calibration_bytes = bytes;
            s.status.calibration_elapsed_ms = (seconds * 1000.0).round() as u64;
        }
    }
    pub fn snapshot(&self) -> StorageStatus {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut v = s.status.clone();
        v.elapsed_seconds = self.started.elapsed().as_secs();
        v.phase_elapsed_ms = s
            .phase_since
            .elapsed()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        v.queued_encoded_bytes = self
            .queue_bytes
            .load(Ordering::Relaxed)
            .saturating_add(s.buffered)
            .saturating_add(s.input);
        v.waiting_blocks = v.queued_encoded_bytes.div_ceil(s.baseline);
        v
    }
    pub fn monitor(&self, events: &dyn EventSink, control: &dyn crate::control::ExecutionControl) {
        let mut previous = (Instant::now(), 0u64, 0u64);
        let mut last = Instant::now() - Duration::from_secs(1);
        while !self.done.load(Ordering::Acquire) {
            self.cancelled.store(control.cancelled(), Ordering::Release);
            if last.elapsed() >= Duration::from_millis(250) {
                let mut v = self.snapshot();
                let dt = previous.0.elapsed().as_secs_f64();
                if dt >= 1.0 {
                    let read = self.source_read.load(Ordering::Relaxed);
                    v.read_mb_per_second = read.saturating_sub(previous.1) as f64 / dt / 1e6;
                    v.write_mb_per_second =
                        v.written_bytes.saturating_sub(previous.2) as f64 / dt / 1e6;
                    {
                        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                        s.status.read_mb_per_second = v.read_mb_per_second;
                        s.status.write_mb_per_second = v.write_mb_per_second;
                    }
                    previous = (Instant::now(), read, v.written_bytes);
                }
                events.storage_progress(&self.id, &v);
                last = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        events.storage_progress(&self.id, &self.snapshot());
    }
    pub fn finish_file(&self, path: &Path, file_id: u64, expected: &str) -> Result<()> {
        let reader = DataFile::read(path, true)?;
        reader.check_descriptor(expected)?;
        let m = reader.manifest.as_ref().unwrap();
        require(
            m.backup_id == self.id && m.file_id == file_id,
            "FILE_ID_MISMATCH",
            "storage receipt descriptor",
        )?;
        let mut digest = blake3::Hasher::new();
        let (cached, proofs) = {
            let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (
                s.files.get(&file_id).cloned(),
                m.parts
                    .iter()
                    .map(|p| s.proofs.get(&reader.parent().join(&p.path)).cloned())
                    .collect::<Vec<_>>(),
            )
        };
        for (p, proof) in m.parts.iter().zip(proofs) {
            let path = reader.part_path(p)?;
            let identity = io::identity(&io::open_regular(&path, false)?)?;
            if cached.is_none() {
                let proof = proof.ok_or_else(|| {
                    Error::new("MISSING_VERIFICATION", "storage part not checked")
                })?;
                require(
                    proof.0 == p.blake3 && proof.1 == identity,
                    "FILE_CHANGED",
                    "checked part changed",
                )?;
            }
            digest.update(&io::json(&(&p.path, &p.blake3, &identity))?);
        }
        let identity = io::identity(reader.descriptor())?;
        let identity_hash = digest.finalize().to_hex().to_string();
        if let Some(old) = cached {
            require(
                old == (identity.clone(), identity_hash.clone()),
                "FILE_CHANGED",
                "checked stream or parts changed",
            )?;
        }
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        for p in &m.parts {
            s.proofs.remove(&reader.parent().join(&p.path));
        }
        s.files.insert(file_id, (identity, identity_hash));
        Ok(())
    }
    pub fn matches(&self, root: &Path, entries: &[crate::v2::FileEntry]) -> Result<bool> {
        for e in entries {
            require(
                self.state
                    .lock()
                    .unwrap_or_else(|v| v.into_inner())
                    .files
                    .contains_key(&e.file_id),
                "MISSING_VERIFICATION",
                "logical stream has no checked receipt",
            )?;
            self.finish_file(
                &root.join(&e.relative_path),
                e.file_id,
                e.storage_descriptor_blake3.as_ref().unwrap(),
            )?;
        }
        Ok(true)
    }
}

pub(crate) struct DataFile {
    path: PathBuf,
    file: File,
    manifest: Option<Manifest>,
    buffer: Vec<u8>,
    buffer_source_bytes: u64,
    position: u64,
    shared: Option<Arc<Shared>>,
    last_flush: Instant,
    dirty: bool,
}
pub(crate) struct FileMetadata(u64);
impl FileMetadata {
    pub fn len(&self) -> u64 {
        self.0
    }
}
impl DataFile {
    fn parent(&self) -> &Path {
        self.path.parent().unwrap()
    }
    fn part_path(&self, p: &Part) -> Result<PathBuf> {
        for parent in [
            self.parent().join("segments"),
            self.parent().join(format!(
                "segments/{:08x}",
                self.manifest.as_ref().unwrap().file_id
            )),
        ] {
            require(
                fs::symlink_metadata(parent)?.is_dir(),
                "INVALID_STORAGE_PART",
                "part parent is a symlink or not a directory",
            )?;
        }
        Ok(self.parent().join(&p.path))
    }
    pub fn descriptor(&self) -> &File {
        &self.file
    }
    pub fn descriptor_blake3(&self) -> Result<Option<String>> {
        if let Some(m) = &self.manifest {
            Ok(Some(types::hex(blake3::hash(&io::json(m)?).as_bytes())))
        } else {
            Ok(None)
        }
    }
    pub fn check_descriptor(&self, expected: &str) -> Result<()> {
        let before = io::identity(&self.file)?;
        require(
            before.bytes <= MAX_DESCRIPTOR,
            "LIMIT_EXCEEDED",
            "descriptor hash read",
        )?;
        let mut bytes = vec![0; before.bytes as usize];
        io::read_exact_at(&self.file, &mut bytes, 0, 1 << 20)?;
        require(
            blake3::hash(&bytes).to_hex().as_str() == expected,
            "STORED_HASH_MISMATCH",
            "storage descriptor BLAKE3",
        )?;
        require(
            before == io::identity(&self.file)?,
            "FILE_CHANGED",
            "descriptor changed during hash read",
        )
    }
    pub fn read(path: &Path, segmented: bool) -> Result<Self> {
        let file = io::open_regular(path, false)?;
        let manifest = if segmented {
            require(
                file.metadata()?.len() <= MAX_DESCRIPTOR,
                "LIMIT_EXCEEDED",
                "storage descriptor too large",
            )?;
            let m: Manifest = io::read_json(path)?;
            m.validate()?;
            Some(m)
        } else {
            None
        };
        Ok(Self {
            path: path.into(),
            file,
            manifest,
            buffer: vec![],
            buffer_source_bytes: 0,
            position: 0,
            shared: None,
            last_flush: Instant::now(),
            dirty: false,
        })
    }
    pub fn open_owned(path: &Path, segmented: bool, shared: Arc<Shared>) -> Result<Self> {
        let mut v = Self::read(path, segmented)?;
        v.file = crate::v2::open_owned(path)?;
        if segmented {
            v.verify_parts(Some(&shared), &std::sync::atomic::AtomicBool::new(false))?;
        }
        v.position = v.metadata()?.len();
        v.shared = Some(shared);
        Ok(v)
    }
    pub fn create(
        path: &Path,
        plan: &Plan,
        backup_id: &str,
        id: u64,
        shared: Arc<Shared>,
    ) -> Result<Self> {
        let file = io::create_new(path)?;
        let manifest = if plan.segmented_storage {
            Some(Manifest {
                kind: "crispawn.segmented_file".into(),
                schema_version: 1,
                backup_id: backup_id.into(),
                file_id: id,
                bytes: 0,
                parts: vec![],
            })
        } else {
            None
        };
        let mut v = Self {
            path: path.into(),
            file,
            manifest,
            buffer: vec![],
            buffer_source_bytes: 0,
            position: 0,
            shared: Some(shared),
            last_flush: Instant::now(),
            dirty: plan.segmented_storage,
        };
        if plan.segmented_storage {
            v.persist()?;
        }
        Ok(v)
    }
    pub fn metadata(&self) -> Result<FileMetadata> {
        Ok(FileMetadata(match &self.manifest {
            Some(m) => m.bytes + self.buffer.len() as u64,
            None => self.file.metadata()?.len(),
        }))
    }
    pub fn due(&self) -> bool {
        self.manifest.is_some()
            && !self.buffer.is_empty()
            && self.last_flush.elapsed().as_secs_f64() >= GOAL
    }
    fn persist(&mut self) -> Result<()> {
        if self.dirty {
            let m = self.manifest.as_ref().unwrap();
            m.validate()?;
            let bytes = io::json(m)?;
            require(
                bytes.len() as u64 <= MAX_DESCRIPTOR,
                "LIMIT_EXCEEDED",
                "storage descriptor",
            )?;
            io::atomic_write(
                self.parent(),
                self.path.file_name().unwrap().to_str().unwrap(),
                &bytes,
            )?;
            self.file = crate::v2::open_owned(&self.path)?;
            self.dirty = false;
        }
        Ok(())
    }
    fn flush_part(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let shared = self.shared.as_ref().unwrap().clone();
        shared.check()?;
        let started = Instant::now();
        let bytes = std::mem::take(&mut self.buffer);
        let size = bytes.len() as u64;
        let result = (|| -> Result<()> {
            let expected = types::hex(&shared.pool.hash(&bytes));
            let m = self.manifest.as_ref().unwrap();
            require(
                m.parts.len() < MAX_PARTS,
                "LIMIT_EXCEEDED",
                "too many storage parts",
            )?;
            let directory = self.parent().join(format!("segments/{:08x}", m.file_id));
            let base = self.parent().join("segments");
            for p in [&base, &directory] {
                match fs::create_dir(p) {
                    Ok(()) => io::sync_dir(p.parent().unwrap())?,
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        require(
                            fs::symlink_metadata(p)?.is_dir(),
                            "INVALID_STORAGE_PART",
                            "part directory is not an ordinary directory",
                        )?;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            let relative = format!(
                "segments/{:08x}/{}.bin",
                m.file_id,
                types::hex(&io::new_id()?)
            );
            let path = self.parent().join(&relative);
            shared.stage("WRITING", size);
            let write_started = Instant::now();
            shared.hooks.at("before_segment_write")?;
            crate::destination_space::check(self.parent())?;
            let mut file = io::create_new(&path)?;
            io::write_all(&mut file, &bytes)?;
            shared.stage("SYNCING", size);
            io::sync_file(&file)?;
            io::sync_dir(&directory)?;
            {
                let mut s = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                s.status.written_bytes += size;
                s.status.written_blocks += 1;
            }
            shared.hooks.at("after_segment_sync")?;
            shared
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .status
                .last_write_elapsed_ms = write_started.elapsed().as_millis() as u64;
            shared.stage("CHECKING", size);
            let check_started = Instant::now();
            verify_part(&file, &expected, &shared.pool, || shared.check())?;
            shared
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .status
                .last_check_elapsed_ms = check_started.elapsed().as_millis() as u64;
            shared.proof(&path, &expected, io::identity(&file)?, true);
            let m = self.manifest.as_mut().unwrap();
            m.parts.push(Part {
                path: relative,
                stream_offset: m.bytes,
                bytes: size,
                used: size,
                blake3: expected,
            });
            m.bytes += size;
            self.dirty = true;
            shared.stage("SAVING_METADATA", size);
            self.persist()?;
            // Only a synced, checked part with a persisted mapping earns credit.
            shared.credit_source(self.buffer_source_bytes);
            self.buffer_source_bytes = 0;
            shared.calibrate(size, started.elapsed().as_secs_f64());
            shared.buffered(0, size);
            self.last_flush = Instant::now();
            shared.stage("READY", 0);
            Ok(())
        })();
        if result.is_err() {
            self.buffer = bytes;
        } else {
            self.buffer = bytes;
            self.buffer.clear();
        }
        result
    }
    pub fn sync(&mut self) -> Result<()> {
        if self.manifest.is_some() {
            self.flush_part()?;
            self.persist()
        } else {
            io::sync_file(&self.file)
        }
    }
    pub fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        self.write_with_source(bytes, 0)
    }
    fn write_with_source(&mut self, mut bytes: &[u8], raw_bytes: u64) -> Result<()> {
        if self.manifest.is_none() {
            return io::write_all(&mut self.file, bytes);
        }
        require(
            self.position == self.metadata()?.len(),
            "INVALID_STATE",
            "segmented writes must append",
        )?;
        let shared = self.shared.as_ref().unwrap().clone();
        let total = bytes.len() as u64;
        let mut processed = 0u64;
        while !bytes.is_empty() {
            shared.check()?;
            if self.due() {
                self.flush_part()?;
            }
            let target = shared.target() as usize;
            let n = bytes.len().min(target.saturating_sub(self.buffer.len()));
            if n == 0 {
                self.flush_part()?;
                continue;
            }
            self.buffer.extend_from_slice(&bytes[..n]);
            let end = processed + n as u64;
            self.buffer_source_bytes += source_equivalent(raw_bytes, end, total)
                - source_equivalent(raw_bytes, processed, total);
            processed = end;
            self.position += n as u64;
            shared.buffered(n as u64, 0);
            bytes = &bytes[n..];
            if self.buffer.len() >= target {
                self.flush_part()?;
            }
        }
        Ok(())
    }
    pub fn set_len(&mut self, len: u64) -> Result<()> {
        if let Some(m) = &mut self.manifest {
            require(
                len <= m.bytes,
                "INVALID_STATE",
                "segmented truncate cannot extend",
            )?;
            let mut position = 0;
            m.parts.retain_mut(|p| {
                if position >= len {
                    return false;
                }
                p.used = p.used.min(len - position);
                position += p.used;
                true
            });
            m.bytes = len;
            self.buffer.clear();
            self.buffer_source_bytes = 0;
            self.position = len;
            self.dirty = true;
            self.persist()
        } else {
            self.file.set_len(len)?;
            Ok(())
        }
    }
    pub fn seek(&mut self, from: SeekFrom) -> Result<u64> {
        if self.manifest.is_none() {
            return Ok(self.file.seek(from)?);
        }
        let pos = match from {
            SeekFrom::Start(v) => v,
            SeekFrom::End(v) => (self.metadata()?.len() as i128 + v as i128)
                .try_into()
                .map_err(|_| Error::new("INVALID_STATE", "negative seek"))?,
            SeekFrom::Current(v) => (self.position as i128 + v as i128)
                .try_into()
                .map_err(|_| Error::new("INVALID_STATE", "negative seek"))?,
        };
        self.position = pos;
        Ok(pos)
    }
    pub fn verify_parts(
        &self,
        shared: Option<&Arc<Shared>>,
        control: &dyn crate::control::ExecutionControl,
    ) -> Result<()> {
        if let Some(s) = shared {
            return self.verify_parts_with_pool(shared, control, &s.pool);
        }
        let pool = CpuPool::new(1)?;
        self.verify_parts_with_pool(shared, control, &pool)
    }
    pub fn verify_parts_with_pool(
        &self,
        shared: Option<&Arc<Shared>>,
        control: &dyn crate::control::ExecutionControl,
        pool: &CpuPool,
    ) -> Result<()> {
        let m = self
            .manifest
            .as_ref()
            .ok_or_else(|| Error::new("INVALID_STATE", "not segmented storage"))?;
        for p in &m.parts {
            control.check_cancel()?;
            let path = self.part_path(p)?;
            let file = io::open_regular(&path, false)?;
            require(
                file.metadata()?.len() == p.bytes,
                "TRUNCATED_RECORD",
                "storage part length",
            )?;
            verify_part(&file, &p.blake3, pool, || control.check_cancel())?;
            if let Some(s) = shared {
                s.proof(&path, &p.blake3, io::identity(&file)?, false);
            }
        }
        Ok(())
    }
    pub fn member_paths(&self) -> Result<Vec<PathBuf>> {
        self.manifest
            .as_ref()
            .map(|m| m.parts.iter().map(|p| self.part_path(p)).collect())
            .unwrap_or_else(|| Ok(vec![]))
    }
}
fn verify_part(
    file: &File,
    expected: &str,
    pool: &CpuPool,
    mut gate: impl FnMut() -> Result<()>,
) -> Result<()> {
    let before = io::identity(file)?;
    let mut hash = blake3::Hasher::new();
    let mut offset = 0;
    let mut buffer = vec![0; 1 << 20];
    while offset < before.bytes {
        gate()?;
        let n = (before.bytes - offset).min(buffer.len() as u64) as usize;
        io::read_exact_at(file, &mut buffer[..n], offset, 1 << 20)?;
        pool.update(&mut hash, &buffer[..n]);
        offset += n as u64;
    }
    require(
        hash.finalize().to_hex().as_str() == expected,
        "STORED_HASH_MISMATCH",
        "saved compressed bytes differ from the pre-write BLAKE3",
    )?;
    require(
        before == io::identity(file)?,
        "FILE_CHANGED",
        "storage part changed during readback",
    )
}
impl io::Reader for DataFile {
    fn read_at(&self, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
        let Some(m) = &self.manifest else {
            return io::Reader::read_at(&self.file, buffer, offset);
        };
        let mut operation = || -> Result<usize> {
            if offset >= m.bytes {
                return Ok(0);
            }
            let index = m
                .parts
                .partition_point(|p| p.stream_offset + p.used <= offset);
            if let Some(p) = m.parts.get(index) {
                let file = io::open_regular(&self.part_path(p)?, false)?;
                require(
                    file.metadata()?.len() == p.bytes,
                    "TRUNCATED_RECORD",
                    "storage part length",
                )?;
                let n = (p.used - (offset - p.stream_offset)).min(buffer.len() as u64) as usize;
                io::read_exact_at(&file, &mut buffer[..n], offset - p.stream_offset, 1 << 20)?;
                return Ok(n);
            }
            Err(Error::new("MISSING_DATA", "segmented stream gap"))
        };
        operation().map_err(|e| std::io::Error::other(e.to_string()))
    }
}
pub(crate) fn write_all(file: &mut DataFile, bytes: &[u8]) -> Result<()> {
    file.write_all(bytes)
}
fn source_equivalent(raw: u64, processed: u64, encoded: u64) -> u64 {
    (u128::from(raw) * u128::from(processed) / u128::from(encoded)) as u64
}
/// Headers/footers do not earn source progress. A zero frame has no payload,
/// so its checked frame header represents the original zero-filled range.
pub(crate) fn write_frame(
    file: &mut DataFile,
    frame: &crate::v2_format::Frame,
    version: u32,
    payload: &[u8],
) -> Result<()> {
    file.write_with_source(
        &frame.encode(version),
        if payload.is_empty() {
            frame.raw_bytes
        } else {
            0
        },
    )?;
    file.write_with_source(payload, frame.raw_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn first_file_calibration_is_bounded_and_accounts_for_sync_and_readback() {
        assert_eq!(Shared::calibrated_size(1 << 20, 4.0), 1 << 20);
        assert_eq!(Shared::calibrated_size(1 << 20, 1000.0), MIN);
        assert_eq!(Shared::calibrated_size(1 << 20, 0.001), MAX);
    }
    #[test]
    fn checked_parts_credit_proportional_source_bytes_and_keep_rounding_remainders() {
        assert_eq!(
            source_equivalent(100_000_000, 1_000_000, 10_000_000),
            10_000_000
        );
        assert_eq!(source_equivalent(u64::MAX, 1, u64::MAX), 1);
        let (_t, s, mut f) = fixture();
        // Two whole parts are checked, but the frame's last 13 bytes remain in RAM.
        let encoded = vec![7; (2 << 20) + 13];
        let raw = 100_000_007;
        f.write_with_source(&encoded, raw).unwrap();
        let partial = s.snapshot();
        assert_eq!(partial.confirmed_bytes, 2 << 20);
        assert_eq!(
            partial.confirmed_source_bytes,
            source_equivalent(raw, 2 << 20, encoded.len() as u64)
        );
        f.sync().unwrap();
        assert_eq!(s.snapshot().confirmed_source_bytes, raw);
        // The next frame has a different compression ratio; never use a global ratio.
        f.write_with_source(&vec![3; 700_003], 2_000_009).unwrap();
        assert_eq!(s.snapshot().confirmed_source_bytes, raw);
        f.sync().unwrap();
        assert_eq!(s.snapshot().confirmed_source_bytes, raw + 2_000_009);
        // Container metadata earns no source-byte credit.
        f.write_all(&[0; 77]).unwrap();
        f.sync().unwrap();
        assert_eq!(s.snapshot().confirmed_source_bytes, raw + 2_000_009);
    }
    #[test]
    fn zero_frame_earns_original_bytes_only_after_its_header_is_checked() {
        let (_t, s, mut f) = fixture();
        write_frame(
            &mut f,
            &crate::v2_format::Frame {
                codec: types::Codec::Zero,
                level: 0,
                raw_bytes: 100_000_000,
                payload_bytes: 0,
            },
            3,
            &[],
        )
        .unwrap();
        assert_eq!(s.snapshot().confirmed_source_bytes, 0);
        f.sync().unwrap();
        assert_eq!(s.snapshot().confirmed_source_bytes, 100_000_000);
    }
    #[test]
    fn elapsed_time_does_not_depend_on_completed_writes() {
        let mut s = Shared::new(&"1".repeat(32), 1).unwrap();
        Arc::get_mut(&mut s).unwrap().started = Instant::now() - Duration::from_secs(125);
        let status = s.snapshot();
        assert_eq!(status.elapsed_seconds, 125);
        assert_eq!(status.written_bytes, 0);
        assert_eq!(status.confirmed_bytes, 0);
    }
    fn fixture() -> (tempfile::TempDir, Arc<Shared>, DataFile) {
        let t = tempfile::tempdir().unwrap();
        let s = Shared::new(&"1".repeat(32), 2).unwrap();
        s.calibrate(1 << 20, 4.0);
        let plan = Plan {
            format_version: 3,
            segmented_storage: true,
            ..Plan::default()
        };
        let f = DataFile::create(
            &t.path().join("00000000.pbk.partial"),
            &plan,
            &"1".repeat(32),
            0,
            s.clone(),
        )
        .unwrap();
        (t, s, f)
    }
    #[test]
    fn arbitrary_encoded_bytes_split_without_decoding_and_read_as_one_stream() {
        let (t, s, mut f) = fixture();
        let bytes = (0..(3 << 20) + 31)
            .map(|i| (i % 251) as u8)
            .collect::<Vec<_>>();
        f.write_all(&bytes).unwrap();
        f.sync().unwrap();
        assert_eq!(s.snapshot().confirmed_blocks, 4);
        let m = f.manifest.as_ref().unwrap();
        assert_eq!(m.parts.len(), 4);
        assert!(m.parts.iter().all(|p| p.bytes <= 1 << 20));
        let expected = f.descriptor_blake3().unwrap().unwrap();
        let r = DataFile::read(&t.path().join("00000000.pbk.partial"), true).unwrap();
        r.check_descriptor(&expected).unwrap();
        let mut actual = vec![0; bytes.len()];
        io::read_exact_at(&r, &mut actual, 0, 333333).unwrap();
        assert_eq!(actual, bytes);
    }
    #[test]
    fn truncate_keeps_original_part_files_and_restarts_at_an_interior_byte_offset() {
        let (t, s, mut f) = fixture();
        let bytes = vec![7; 2 << 20];
        f.write_all(&bytes).unwrap();
        f.sync().unwrap();
        let names = f
            .manifest
            .as_ref()
            .unwrap()
            .parts
            .iter()
            .map(|p| t.path().join(&p.path))
            .collect::<Vec<_>>();
        f.set_len(12345).unwrap();
        f.seek(SeekFrom::Start(12345)).unwrap();
        f.write_all(&[9; 1000]).unwrap();
        f.sync().unwrap();
        assert!(names.iter().all(|p| p.exists()));
        let r = DataFile::read(&f.path, true).unwrap();
        r.verify_parts(Some(&s), &AtomicBool::new(false)).unwrap();
        let mut result = vec![0; 13345];
        io::read_exact_at(&r, &mut result, 0, 8192).unwrap();
        assert_eq!(&result[..12345], &bytes[..12345]);
        assert_eq!(&result[12345..], &[9; 1000]);
    }
    #[test]
    fn waiting_block_count_uses_encoded_pending_bytes_and_not_source_read_bytes() {
        let (_t, s, mut f) = fixture();
        s.source_read.store(1 << 40, Ordering::Relaxed);
        s.queue_bytes.store(2 << 20, Ordering::Relaxed);
        f.write_all(&[1; 1000]).unwrap();
        assert_eq!(s.snapshot().waiting_blocks, 3);
        assert_eq!(s.snapshot().queued_encoded_bytes, (2 << 20) + 1000);
    }
    #[test]
    fn part_hash_error_does_not_acknowledge_the_part_or_commit_its_descriptor() {
        struct Corrupt(PathBuf);
        impl Hooks for Corrupt {
            fn at(&self, p: &'static str) -> Result<()> {
                if p == "after_segment_sync" {
                    let directory = self.0.join("segments/00000000");
                    let file = fs::read_dir(directory)?.next().unwrap()?.path();
                    use std::os::unix::fs::FileExt;
                    let f = io::open_regular(&file, true)?;
                    f.write_at(&[99], 0)?;
                    f.sync_all()?;
                }
                Ok(())
            }
        }
        let t = tempfile::tempdir().unwrap();
        let mut s = Shared::new(&"1".repeat(32), 1).unwrap();
        Arc::get_mut(&mut s).unwrap().hooks = Arc::new(Corrupt(t.path().into()));
        s.calibrate(1 << 20, 4.0);
        let plan = Plan {
            format_version: 3,
            segmented_storage: true,
            ..Plan::default()
        };
        let mut f = DataFile::create(
            &t.path().join("00000000.pbk.partial"),
            &plan,
            &"1".repeat(32),
            0,
            s.clone(),
        )
        .unwrap();
        assert_eq!(
            f.write_with_source(&vec![7; 1 << 20], 100_000_000)
                .unwrap_err()
                .code,
            "STORED_HASH_MISMATCH"
        );
        assert_eq!(s.snapshot().confirmed_blocks, 0);
        assert_eq!(s.snapshot().confirmed_source_bytes, 0);
        assert!(DataFile::read(&f.path, true)
            .unwrap()
            .manifest
            .unwrap()
            .parts
            .is_empty());
    }
    #[test]
    fn storage_observer_keeps_reporting_wait_time_while_writer_is_held() {
        use std::sync::{atomic::AtomicUsize, Condvar};
        struct Hold(Arc<(Mutex<bool>, Condvar)>);
        impl Hooks for Hold {
            fn at(&self, p: &'static str) -> Result<()> {
                if p == "before_segment_write" {
                    let (lock, wake) = &*self.0;
                    let done = lock.lock().unwrap();
                    let (done, timeout) = wake
                        .wait_timeout_while(done, Duration::from_secs(2), |v| !*v)
                        .unwrap();
                    require(
                        *done && !timeout.timed_out(),
                        "TEST_DEADLINE",
                        "writer test release",
                    )?;
                }
                Ok(())
            }
        }
        struct Observe(AtomicUsize);
        impl EventSink for Observe {
            fn emit(&self, _: crispawn_core::types::Snapshot) {}
            fn storage_progress(&self, _: &str, s: &StorageStatus) {
                if s.phase == "WRITING" && s.phase_elapsed_ms >= 100 {
                    assert_eq!(s.written_bytes, 0);
                    assert_eq!(s.confirmed_bytes, 0);
                    self.0.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        let t = tempfile::tempdir().unwrap();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let mut shared = Shared::new(&"1".repeat(32), 1).unwrap();
        Arc::get_mut(&mut shared).unwrap().hooks = Arc::new(Hold(release.clone()));
        let plan = Plan {
            format_version: 3,
            segmented_storage: true,
            ..Plan::default()
        };
        let mut file = DataFile::create(
            &t.path().join("00000000.pbk.partial"),
            &plan,
            &"1".repeat(32),
            0,
            shared.clone(),
        )
        .unwrap();
        let events = Observe(AtomicUsize::new(0));
        let control = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let _stop = StopMonitor(shared.clone());
            scope.spawn(|| shared.monitor(&events, &control));
            let worker = scope.spawn(|| {
                file.write_all(&[9; 1000])?;
                file.sync()
            });
            let deadline = Instant::now() + Duration::from_secs(1);
            while events.0.load(Ordering::Relaxed) < 2 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(25));
            }
            *release.0.lock().unwrap() = true;
            release.1.notify_all();
            worker.join().unwrap().unwrap();
            assert!(events.0.load(Ordering::Relaxed) >= 2);
            let status = shared.snapshot();
            assert!(status.calibrated);
            assert_eq!(status.calibration_bytes, 1000);
            assert!(status.calibration_elapsed_ms >= 250);
            assert!(status.last_write_elapsed_ms >= 250);
            assert_eq!(status.baseline_bytes, MIN);
        });
    }
}
