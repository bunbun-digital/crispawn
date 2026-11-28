// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-digital. Licensed under Apache-2.0.
//! Physical part files are independent of the PBK compression-frame boundaries.
//! Parts are written/checked concurrently through the OS cache. Durability is
//! established by the final filesystem barrier before COMMITTED, not per part.
//!
//! Descriptor schema 2 appends bounded chunks to one large physical part per
//! stream (up to `PART_LIMIT`) instead of creating one file per chunk. Chunk
//! size remains the write/check/progress unit; part size is the file unit.
use crate::{io, parallel_hash::CpuPool, Error, Hooks, NoHooks, Result};
use crispawn_core::{
    error::require,
    live::StorageStatus,
    plan::Plan,
    types::{self, EventSink, SourceIdentity},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::{self, File},
    io::{Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

const FIRST: u64 = 1 << 20;
const MIN: u64 = 64 << 10;
/// Largest appended chunk (write/check unit) and schema-1 part size.
const MAX: u64 = 16 << 20;
/// Largest schema-2 physical part file. Below MAX_BACKUP_FILE_BYTES.
pub(crate) const PART_LIMIT: u64 = 1 << 30;
const MAX_PARTS: usize = 32768;
const MAX_DESCRIPTOR: u64 = 8 << 20;
const GOAL: f64 = 5.0;
/// Buffers below MIN (typically zero-frame headers) wait longer before a
/// time-based flush, so zero-filled regions do not produce tiny appends.
const SMALL_GOAL: f64 = 30.0;
const SCHEMA: u32 = 2;
const MAX_DIAGNOSTICS: usize = 8192;
/// New parts reserve up to their size limit so two streams growing at once
/// do not interleave on the disk; smaller reservations are not worth it.
const PREALLOCATE_MIN: u64 = 64 << 20;
/// Allocation left past the sealed length that still counts as trimmed.
const TRIM_SLACK: u64 = 1 << 20;

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
            self.kind == "crispawn.segmented_file" && matches!(self.schema_version, 1 | 2),
            "UNKNOWN_VERSION",
            "segmented file descriptor",
        )?;
        let part_limit = if self.schema_version == 1 {
            MAX
        } else {
            PART_LIMIT
        };
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
                    && p.bytes <= part_limit
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
    throughput_bps: Option<f64>,
    proofs: BTreeMap<PathBuf, (String, SourceIdentity)>,
    files: BTreeMap<u64, (SourceIdentity, String)>,
    active: BTreeMap<u64, (String, u64)>,
    failure: Option<Error>,
    /// Bounded per-chunk/per-part records drained by `monitor` into the log.
    diagnostics: VecDeque<(&'static str, serde_json::Value)>,
    dropped_diagnostics: u64,
    summary: Summary,
}
fn push_diagnostic(s: &mut State, name: &'static str, value: serde_json::Value) {
    if s.diagnostics.len() >= MAX_DIAGNOSTICS {
        s.dropped_diagnostics += 1;
    } else {
        s.diagnostics.push_back((name, value));
    }
}
/// Whole-copy storage totals, emitted once as `storage_summary`.
#[derive(Default, Serialize)]
struct Summary {
    chunks: u64,
    chunk_bytes: u64,
    parts_sealed: u64,
    part_bytes: u64,
    /// Sealed part counts by size class: <=1KiB, <=1MiB, <=64MiB, <=256MiB, larger.
    part_size_classes: [u64; 5],
    max_chunk_write_ms: u64,
    max_chunk_queue_ms: u64,
    max_part_extents: u32,
    parts_preallocated: u64,
    preallocate_failures: u64,
    preallocated_bytes: u64,
}
pub(crate) struct Shared {
    pub pool: Arc<CpuPool>,
    id: String,
    state: Arc<Mutex<State>>,
    pub source_read: AtomicU64,
    pub queue_bytes: AtomicU64,
    pub cancelled: Arc<AtomicBool>,
    done: AtomicBool,
    hooks: Arc<dyn Hooks>,
    capacity: u64,
    /// Physical part ceiling; PART_LIMIT in production, smaller in tests.
    part_limit: u64,
    started: Instant,
    writers: OnceLock<crate::storage_worker::Pool>,
    next_job: AtomicU64,
    destination: OnceLock<File>,
    verify_after_write: AtomicBool,
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
            cancelled: Arc::new(AtomicBool::new(false)),
            done: AtomicBool::new(false),
            hooks: Arc::new(NoHooks),
            capacity: MAX,
            part_limit: PART_LIMIT,
            started: Instant::now(),
            writers: OnceLock::new(),
            next_job: AtomicU64::new(0),
            destination: OnceLock::new(),
            verify_after_write: AtomicBool::new(true),
            state: Arc::new(Mutex::new(State {
                status: StorageStatus {
                    phase: "CALIBRATING".into(),
                    written_blocks: 0,
                    confirmed_blocks: 0,
                    waiting_blocks: 0,
                    queued_encoded_bytes: 0,
                    prefetch_encoded_bytes: 0,
                    prefetch_limit_bytes: 0,
                    current_file_bytes: 0,
                    baseline_bytes: FIRST,
                    write_workers: crate::storage_worker::WORKERS,
                    active_write_workers: 0,
                    written_bytes: 0,
                    confirmed_bytes: 0,
                    confirmed_source_bytes: 0,
                    written_source_bytes: 0,
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
                throughput_bps: None,
                proofs: BTreeMap::new(),
                files: BTreeMap::new(),
                active: BTreeMap::new(),
                failure: None,
                diagnostics: VecDeque::new(),
                dropped_diagnostics: 0,
                summary: Summary::default(),
            })),
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
        if let Some(e) = &self.state.lock().unwrap_or_else(|e| e.into_inner()).failure {
            return Err(e.clone());
        }
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
        if !s.active.is_empty() {
            return;
        }
        s.status.phase = phase.into();
        s.status.current_file_bytes = size;
        s.phase_since = Instant::now();
    }
    fn submit(&self, key: u64, task: crate::storage_worker::Task) -> Result<()> {
        if self.writers.get().is_none() {
            let _ = self.writers.set(crate::storage_worker::Pool::new()?);
        }
        self.writers
            .get()
            .unwrap()
            .submit(key, task, || self.check())
    }
    fn part_sealed(
        &self,
        relative: &str,
        bytes: u64,
        chunks: u32,
        seconds: f64,
        extents: Option<u32>,
        preallocated: u64,
        allocated: Option<u64>,
    ) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let class = match bytes {
            0..=1024 => 0,
            1025..=1048576 => 1,
            1048577..=67108864 => 2,
            67108865..=268435456 => 3,
            _ => 4,
        };
        s.summary.parts_sealed += 1;
        s.summary.part_bytes += bytes;
        s.summary.part_size_classes[class] += 1;
        s.summary.max_part_extents = s.summary.max_part_extents.max(extents.unwrap_or(0));
        push_diagnostic(
            &mut s,
            "storage_part",
            serde_json::json!({"part":relative,"bytes":bytes,"chunks":chunks,
                "open_seconds":(seconds*1000.0).round()/1000.0,"extents":extents,
                "preallocated":preallocated,"allocated_bytes":allocated}),
        );
    }
    /// Emit and clear collected storage diagnostics.
    pub fn drain_diagnostics(&self, events: &dyn EventSink) {
        let (records, dropped) = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (
                std::mem::take(&mut s.diagnostics),
                std::mem::take(&mut s.dropped_diagnostics),
            )
        };
        if dropped > 0 {
            events.diagnostic(
                "storage_diagnostics_dropped",
                &serde_json::json!({"records":dropped}),
            );
        }
        for (name, value) in records {
            events.diagnostic(name, &value);
        }
    }
    pub fn emit_summary(&self, events: &dyn EventSink) {
        self.drain_diagnostics(events);
        let value = {
            let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            serde_json::to_value(&s.summary).unwrap_or_default()
        };
        events.diagnostic("storage_summary", &value);
    }
    pub fn bind_destination(&self, root: &Path) -> Result<()> {
        // Keep this fd open from BEFORE copying: a newly opened fd at the end
        // could miss earlier filesystem writeback errors already reported.
        if self.destination.get().is_none() {
            let file = File::open(root)?;
            require(
                file.metadata()?.is_dir(),
                "INVALID_STATE",
                "destination is not a directory",
            )?;
            let _ = self.destination.set(file);
        }
        Ok(())
    }
    pub fn final_sync(&self, _root: &Path) -> Result<()> {
        self.check()?;
        self.stage("FINAL_SYNC", 0);
        io::sync_filesystem(
            self.destination
                .get()
                .ok_or_else(|| Error::new("INVALID_STATE", "destination filesystem not bound"))?,
        )?;
        self.stage("READY", 0);
        Ok(())
    }
    pub fn input(&self, bytes: u64) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).input = bytes;
    }
    pub fn prefetch_limit(&self, bytes: u64) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .status
            .prefetch_limit_bytes = bytes;
    }
    pub fn set_write_verification(&self, enabled: bool) {
        self.verify_after_write.store(enabled, Ordering::Relaxed);
    }
    pub fn verifies_writes(&self) -> bool {
        self.verify_after_write.load(Ordering::Relaxed)
    }
    pub fn credit_source(&self, bytes: u64) {
        if !self.verifies_writes() {
            return;
        }
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.status.confirmed_source_bytes = s.status.confirmed_source_bytes.saturating_add(bytes);
    }
    pub fn seed_source(&self, bytes: u64) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.status.written_source_bytes += bytes;
        if self.verifies_writes() {
            s.status.confirmed_source_bytes += bytes;
        }
    }
    fn buffered(&self, added: u64, removed: u64) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.buffered = s.buffered.saturating_add(added).saturating_sub(removed);
        s.input = s.input.saturating_sub(added);
    }
    fn proof(&self, path: &Path, hash: &str, identity: SourceIdentity, new_write: bool) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // New writes were already counted per appended chunk by the writer.
        if !new_write && !s.proofs.contains_key(path) {
            if self.verifies_writes() {
                s.status.confirmed_bytes += identity.bytes;
                s.status.confirmed_blocks += 1;
            }
            s.status.written_bytes += identity.bytes;
            s.status.written_blocks += 1;
        }
        s.proofs.insert(path.into(), (hash.into(), identity));
    }
    fn calibrated_size(bytes: u64, seconds: f64) -> u64 {
        ((bytes as f64 * (GOAL * 0.8) / seconds.max(0.001)) as u64).clamp(MIN, MAX) / 4096 * 4096
    }
    fn calibrate(&self, bytes: u64, seconds: f64) {
        let physical = crate::io_trace::destination_bps()
            .map(|bps| bps / f64::from(crate::storage_worker::WORKERS));
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !s.status.calibrated {
            s.status.calibrated = true;
            s.status.calibration_bytes = bytes;
            s.status.calibration_elapsed_ms = (seconds * 1000.0).round() as u64;
        }
        // Tiny/cached admissions do not measure a USB device's sustained speed.
        // Prefer completed block I/O; fall back to nontrivial part service times
        // for ordinary-file adapters. Never add another destructive trial write.
        if let Some(rate) = physical.or_else(|| {
            (bytes >= FIRST / 4 && seconds >= 0.05).then_some(bytes as f64 / seconds.max(0.001))
        }) {
            let rate = s
                .throughput_bps
                .map_or(rate, |old| (old * 0.75 + rate * 0.25).min(rate * 2.0));
            s.throughput_bps = Some(rate);
            let target = Self::calibrated_size(rate as u64, 1.0).min(self.capacity);
            s.baseline = target.min(s.baseline.saturating_mul(2)).max(MIN);
        }
        s.status.baseline_bytes = s.baseline;
    }
    pub fn snapshot(&self) -> StorageStatus {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut v = s.status.clone();
        v.prefetch_encoded_bytes = self.queue_bytes.load(Ordering::Relaxed);
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
                self.drain_diagnostics(events);
                last = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        events.storage_progress(&self.id, &self.snapshot());
        self.emit_summary(events);
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

#[derive(Clone)]
struct WriteContext {
    pool: Arc<CpuPool>,
    state: Arc<Mutex<State>>,
    cancelled: Arc<AtomicBool>,
    hooks: Arc<dyn Hooks>,
    verify_after_write: bool,
    part_limit: u64,
}
impl WriteContext {
    fn check(&self) -> Result<()> {
        if let Some(e) = &self.state.lock().unwrap_or_else(|e| e.into_inner()).failure {
            return Err(e.clone());
        }
        require(
            !self.cancelled.load(Ordering::Acquire),
            "CANCELLED",
            "storage write cancelled",
        )
    }
    fn phase(&self, job: u64, phase: &str, bytes: u64) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.active.insert(job, (phase.into(), bytes));
        Self::refresh(&mut s);
    }
    fn refresh(s: &mut State) {
        s.status.active_write_workers = s.active.len() as u32;
        s.status.current_file_bytes = s.active.values().map(|v| v.1).max().unwrap_or(0);
        let phase = if s.failure.is_some() {
            "FAILED"
        } else if s.active.values().any(|v| v.0 == "WRITING") {
            "WRITING"
        } else if s.active.is_empty() {
            "READY"
        } else {
            "CHECKING"
        };
        if s.status.phase != phase {
            s.phase_since = Instant::now();
        }
        s.status.phase = phase.into();
    }
    fn store(&self, job: u64, input: ChunkInput) -> Result<ChunkReceipt> {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _deferred = io::DeferredSync::new(true);
            let queued = input.started.elapsed();
            crate::io_trace::elapsed(
                crate::io_trace::Op::QueueWait,
                queued,
                input.bytes.len() as u64,
            );
            self.check()?;
            self.phase(job, "WRITING", input.bytes.len() as u64);
            let started = Instant::now();
            let expected = types::hex(&self.pool.hash(&input.bytes));
            let directory = input.path.parent().unwrap();
            if input.part_offset == 0 {
                for path in [directory.parent().unwrap(), directory] {
                    let _trace = crate::io_trace::span(crate::io_trace::Op::Directory, 0);
                    match fs::symlink_metadata(path) {
                        Ok(meta) => require(
                            meta.is_dir(),
                            "INVALID_STORAGE_PART",
                            "storage directory is not an ordinary directory",
                        )?,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            match fs::create_dir(path) {
                                Ok(()) => {}
                                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                                    require(
                                        fs::symlink_metadata(path)?.is_dir(),
                                        "INVALID_STORAGE_PART",
                                        "storage directory is not an ordinary directory",
                                    )?;
                                }
                                Err(error) => return Err(error.into()),
                            }
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
            }
            self.hooks.at("before_segment_write")?;
            crate::destination_space::check(directory)?;
            let mut preallocate = serde_json::Value::Null;
            if input.part_offset == 0 {
                let created = io::create_new(&input.path)?;
                let reserving = Instant::now();
                let (granted, errno) = preallocate_part(&created, directory, self.part_limit);
                input.reservation.grant(granted);
                preallocate = serde_json::json!({"bytes":granted,"errno":errno,
                    "ms":reserving.elapsed().as_millis() as u64});
                let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if granted > 0 {
                    s.summary.parts_preallocated += 1;
                    s.summary.preallocated_bytes += granted;
                } else if errno.is_some() {
                    s.summary.preallocate_failures += 1;
                }
                drop(s);
                require(
                    input.file.set(created).is_ok(),
                    "INVALID_STATE",
                    "storage part opened twice",
                )?;
            }
            let file = input.file.get().ok_or_else(|| {
                Error::new(
                    "STORAGE_WORKER_FAILED",
                    "earlier append of this part did not open it",
                )
            })?;
            require(
                file.metadata()?.len() == input.part_offset,
                "INVALID_STORAGE_PART",
                "storage part append offset",
            )?;
            {
                let _trace =
                    crate::io_trace::span(crate::io_trace::Op::DataWrite, input.bytes.len() as u64);
                io::write_all_at(file, &input.bytes, input.part_offset)?;
            }
            input.reservation.consume(input.bytes.len() as u64);
            let write_ms = started.elapsed().as_millis() as u64;
            {
                let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                s.status.written_bytes += input.bytes.len() as u64;
                s.status.written_source_bytes += input.source_bytes;
                s.status.written_blocks += 1;
                s.status.last_write_elapsed_ms = write_ms;
            }
            self.hooks.at("after_segment_write")?;
            let check_ms = if self.verify_after_write {
                let _trace =
                    crate::io_trace::span(crate::io_trace::Op::Readback, input.bytes.len() as u64);
                self.phase(job, "CHECKING", input.bytes.len() as u64);
                let checking = Instant::now();
                verify_range(
                    file,
                    input.part_offset,
                    input.bytes.len() as u64,
                    &expected,
                    &self.pool,
                    || self.check(),
                )?;
                let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                s.status.confirmed_bytes += input.bytes.len() as u64;
                s.status.confirmed_blocks += 1;
                checking.elapsed().as_millis() as u64
            } else {
                0
            };
            {
                let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                s.status.last_check_elapsed_ms = check_ms;
                let queue_ms = queued.as_millis() as u64;
                s.summary.chunks += 1;
                s.summary.chunk_bytes += input.bytes.len() as u64;
                s.summary.max_chunk_write_ms = s.summary.max_chunk_write_ms.max(write_ms);
                s.summary.max_chunk_queue_ms = s.summary.max_chunk_queue_ms.max(queue_ms);
                let writer = std::thread::current().name().unwrap_or("").to_string();
                push_diagnostic(
                    &mut s,
                    "storage_chunk",
                    serde_json::json!({"part":input.relative,"offset":input.part_offset,
                        "bytes":input.bytes.len(),"source_bytes":input.source_bytes,
                        "queue_ms":queue_ms,"write_ms":write_ms,"check_ms":check_ms,
                        "writer":writer,"preallocate":preallocate}),
                );
            }
            Ok(ChunkReceipt {
                source_bytes: input.source_bytes,
                service_seconds: (write_ms + check_ms) as f64 / 1000.0,
            })
        }))
        .unwrap_or_else(|_| {
            Err(Error::new(
                "STORAGE_WORKER_FAILED",
                "storage writer panicked",
            ))
        });
        if let Err(error) = &result {
            crate::supervisor::failure(error);
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if s.failure.is_none() {
                s.failure = Some(error.clone());
            }
            self.cancelled.store(true, Ordering::Release);
        }
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.active.remove(&job);
        Self::refresh(&mut s);
        result
    }
}
/// One bounded append into a physical part. Appends of one part run in order.
struct ChunkInput {
    path: PathBuf,
    relative: String,
    part_offset: u64,
    bytes: Vec<u8>,
    source_bytes: u64,
    started: Instant,
    file: Arc<OnceLock<File>>,
    reservation: Arc<Reservation>,
}
/// Space preallocated (FALLOC_FL_KEEP_SIZE) for one open part. Bytes not yet
/// written stay in `destination_space::RESERVED` until the part seals or is
/// dropped.
#[derive(Default)]
struct Reservation {
    remaining: AtomicU64,
    granted: AtomicU64,
}
impl Reservation {
    fn grant(&self, bytes: u64) {
        if bytes > 0 {
            self.granted.store(bytes, Ordering::Release);
            self.remaining.store(bytes, Ordering::Release);
            crate::destination_space::RESERVED.fetch_add(bytes, Ordering::AcqRel);
        }
    }
    fn consume(&self, bytes: u64) {
        let before = self
            .remaining
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |r| {
                Some(r.saturating_sub(bytes))
            })
            .unwrap_or(0);
        crate::destination_space::RESERVED.fetch_sub(before.min(bytes), Ordering::AcqRel);
    }
    /// Stop counting the unwritten rest; returns the original grant.
    fn release(&self) -> u64 {
        let rest = self.remaining.swap(0, Ordering::AcqRel);
        crate::destination_space::RESERVED.fetch_sub(rest, Ordering::AcqRel);
        self.granted.load(Ordering::Acquire)
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.release();
    }
}
/// Reserve contiguous space for a new part without changing its length, so
/// appends to two growing parts do not interleave cluster by cluster. Uses at
/// most a quarter of the space above the destination minimum.
fn preallocate_part(file: &File, directory: &Path, limit: u64) -> (u64, Option<i32>) {
    use std::os::fd::AsRawFd;
    // Memory filesystems gain nothing and may enforce quotas statvfs hides.
    if memory_filesystem(directory) {
        return (0, None);
    }
    let Ok(free) = io::available_bytes(directory) else {
        return (0, None);
    };
    let budget = free.saturating_sub(crate::destination_space::MIN_FREE_BYTES) / 4;
    let size = limit.min(budget) & !((1 << 20) - 1);
    if size < PREALLOCATE_MIN {
        return (0, None);
    }
    // SAFETY: valid open descriptor; KEEP_SIZE leaves the file length unchanged.
    let rc = unsafe {
        libc::fallocate(
            file.as_raw_fd(),
            libc::FALLOC_FL_KEEP_SIZE,
            0,
            size as libc::off_t,
        )
    };
    if rc == 0 {
        (size, None)
    } else {
        (0, std::io::Error::last_os_error().raw_os_error())
    }
}
fn memory_filesystem(directory: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    const TMPFS_MAGIC: i64 = 0x0102_1994;
    const RAMFS_MAGIC: i64 = 0x8584_58f6;
    let Ok(path) = std::ffi::CString::new(directory.as_os_str().as_bytes()) else {
        return true;
    };
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: valid NUL-terminated path and owned writable structure.
    if unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return true;
    }
    // SAFETY: statfs succeeded and initialized the structure.
    let kind = unsafe { stat.assume_init() }.f_type as i64;
    kind == TMPFS_MAGIC || kind == RAMFS_MAGIC
}
fn allocated_bytes(file: &File) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    file.metadata().ok().map(|m| m.blocks() * 512)
}
/// Free preallocated clusters past the sealed length. A same-length truncate
/// does this on most filesystems; otherwise shrink from one byte longer.
fn trim_preallocation(file: &File, bytes: u64) -> Result<Option<u64>> {
    file.set_len(bytes)?;
    if allocated_bytes(file).is_some_and(|a| a > bytes + TRIM_SLACK) {
        file.set_len(bytes + 1)?;
        file.set_len(bytes)?;
    }
    Ok(allocated_bytes(file))
}
struct ChunkReceipt {
    source_bytes: u64,
    service_seconds: f64,
}
/// The physical part currently receiving appends for one logical stream.
struct Current {
    relative: String,
    path: PathBuf,
    file: Arc<OnceLock<File>>,
    stream_offset: u64,
    bytes: u64,
    hash: blake3::Hasher,
    chunks: u32,
    opened: Instant,
    reservation: Arc<Reservation>,
}
enum PendingPart {
    Chunk {
        bytes: u64,
        receiver: Mutex<mpsc::Receiver<Result<ChunkReceipt>>>,
    },
    /// Published only after every earlier chunk of the part has a receipt.
    Seal(
        Box<(
            Part,
            PathBuf,
            Arc<OnceLock<File>>,
            u32,
            Instant,
            Arc<Reservation>,
        )>,
    ),
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
    current: Option<Current>,
    pending: VecDeque<PendingPart>,
    /// Bytes handed to writers but not yet published in the manifest.
    pending_bytes: u64,
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
            let bytes = file.metadata()?.len();
            require(
                bytes <= MAX_DESCRIPTOR,
                "LIMIT_EXCEEDED",
                format!(
                    "storage descriptor too large: path={} bytes={bytes} limit={MAX_DESCRIPTOR}",
                    path.display()
                ),
            )?;
            let m: Manifest = io::read_json_bounded(path, MAX_DESCRIPTOR)?;
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
            current: None,
            dirty: false,
            pending: VecDeque::new(),
            pending_bytes: 0,
        })
    }
    pub fn open_owned(path: &Path, segmented: bool, shared: Arc<Shared>) -> Result<Self> {
        if segmented {
            shared.bind_destination(path.parent().unwrap())?;
        }
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
        if plan.segmented_storage {
            shared.bind_destination(path.parent().unwrap())?;
        }
        let file = io::create_new(path)?;
        let manifest = if plan.segmented_storage {
            Some(Manifest {
                kind: "crispawn.segmented_file".into(),
                schema_version: SCHEMA,
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
            current: None,
            dirty: plan.segmented_storage,
            pending: VecDeque::new(),
            pending_bytes: 0,
        };
        if plan.segmented_storage {
            v.persist()?;
        }
        Ok(v)
    }
    pub fn metadata(&self) -> Result<FileMetadata> {
        Ok(FileMetadata(match &self.manifest {
            Some(m) => m.bytes + self.pending_bytes + self.buffer.len() as u64,
            None => self.file.metadata()?.len(),
        }))
    }
    pub fn due(&self) -> bool {
        let goal = if self.buffer.len() as u64 >= MIN {
            GOAL
        } else {
            SMALL_GOAL
        };
        self.manifest.is_some()
            && !self.buffer.is_empty()
            && self.last_flush.elapsed().as_secs_f64() >= goal
    }
    fn persist(&mut self) -> Result<()> {
        if self.dirty {
            let _trace = crate::io_trace::span(crate::io_trace::Op::DescriptorWrite, 0);
            let _deferred = io::DeferredSync::new(true);
            if let Some(shared) = &self.shared {
                shared.hooks.at("before_descriptor_write")?;
            }
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
    fn pending_chunks(&self) -> usize {
        self.pending
            .iter()
            .filter(|p| matches!(p, PendingPart::Chunk { .. }))
            .count()
    }
    fn receive_part(&mut self, wait: bool) -> Result<bool> {
        let Some(front) = self.pending.front() else {
            return Ok(false);
        };
        let shared = self.shared.as_ref().unwrap().clone();
        let PendingPart::Chunk { bytes, receiver } = front else {
            // Every earlier chunk of this part already has a checked receipt.
            let Some(PendingPart::Seal(sealed)) = self.pending.pop_front() else {
                unreachable!()
            };
            let (part, path, file, chunks, opened, reservation) = *sealed;
            let file = file
                .get()
                .ok_or_else(|| Error::new("INVALID_STATE", "sealed part was never opened"))?;
            // Trim before the identity is taken: truncation updates timestamps.
            let preallocated = reservation.release();
            let allocated = if preallocated > 0 {
                trim_preallocation(file, part.bytes)?
            } else {
                allocated_bytes(file)
            };
            let identity = io::identity(file)?;
            let m = self.manifest.as_mut().unwrap();
            require(
                part.stream_offset == m.bytes && identity.bytes == part.bytes,
                "INVALID_STORAGE_PART",
                "sealed part offset or length",
            )?;
            shared.proof(&path, &part.blake3, identity, true);
            let extents = extent_count(file);
            shared.part_sealed(
                &part.path,
                part.bytes,
                chunks,
                opened.elapsed().as_secs_f64(),
                extents,
                preallocated,
                allocated,
            );
            m.bytes += part.bytes;
            self.pending_bytes -= part.bytes;
            m.parts.push(part);
            self.dirty = true;
            shared.stage("SAVING_METADATA", 0);
            self.persist()?;
            shared.stage("READY", 0);
            return Ok(true);
        };
        let size = *bytes;
        let result = if wait {
            let _trace = crate::io_trace::span(crate::io_trace::Op::ReceiptWait, size);
            loop {
                match receiver
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .recv_timeout(Duration::from_millis(25))
                {
                    Ok(v) => break v,
                    Err(mpsc::RecvTimeoutError::Timeout) => shared.check()?,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Err(Error::new(
                            "STORAGE_WORKER_FAILED",
                            "part completion disconnected",
                        ))
                    }
                }
            }
        } else {
            match receiver
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .try_recv()
            {
                Ok(v) => v,
                Err(mpsc::TryRecvError::Empty) => return Ok(false),
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(Error::new(
                        "STORAGE_WORKER_FAILED",
                        "part completion disconnected",
                    ))
                }
            }
        };
        let completed = result?;
        self.pending.pop_front();
        shared.credit_source(completed.source_bytes);
        shared.buffered(0, size);
        shared.calibrate(size, completed.service_seconds);
        shared.stage("READY", 0);
        Ok(true)
    }
    pub fn poll(&mut self) -> Result<()> {
        while self.receive_part(false)? {}
        if self.due() {
            self.flush_part()?;
        }
        Ok(())
    }
    /// Close the current physical part; it is published after its last append.
    fn seal_current(&mut self) {
        if let Some(c) = self.current.take() {
            let part = Part {
                path: c.relative,
                stream_offset: c.stream_offset,
                bytes: c.bytes,
                used: c.bytes,
                blake3: c.hash.finalize().to_hex().to_string(),
            };
            self.pending.push_back(PendingPart::Seal(Box::new((
                part,
                c.path,
                c.file,
                c.chunks,
                c.opened,
                c.reservation,
            ))));
        }
    }
    fn flush_part(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        while self.receive_part(false)? {}
        // Bound per-stream completion receipts even if its first write is slow.
        while self.pending_chunks()
            >= crate::storage_worker::QUEUED + crate::storage_worker::WORKERS as usize
        {
            self.receive_part(true)?;
        }
        let shared = self.shared.as_ref().unwrap().clone();
        shared.check()?;
        if self
            .current
            .as_ref()
            .is_some_and(|c| c.bytes + self.buffer.len() as u64 > shared.part_limit)
        {
            self.seal_current();
        }
        let m = self.manifest.as_ref().unwrap();
        let sealing = self
            .pending
            .iter()
            .filter(|p| matches!(p, PendingPart::Seal(_)))
            .count();
        if self.current.is_none() {
            require(
                m.parts.len() + sealing + 1 < MAX_PARTS,
                "LIMIT_EXCEEDED",
                "too many storage parts",
            )?;
            let relative = format!(
                "segments/{:08x}/{}.bin",
                m.file_id,
                types::hex(&io::new_id()?)
            );
            self.current = Some(Current {
                path: self.parent().join(&relative),
                relative,
                file: Arc::new(OnceLock::new()),
                stream_offset: m.bytes + self.pending_bytes,
                bytes: 0,
                hash: blake3::Hasher::new(),
                chunks: 0,
                opened: Instant::now(),
                reservation: Arc::default(),
            });
        }
        let key = m.file_id;
        let bytes = std::mem::take(&mut self.buffer);
        let size = bytes.len() as u64;
        let c = self.current.as_mut().unwrap();
        shared.pool.update(&mut c.hash, &bytes);
        let input = ChunkInput {
            path: c.path.clone(),
            relative: c.relative.clone(),
            part_offset: c.bytes,
            bytes,
            source_bytes: self.buffer_source_bytes,
            started: Instant::now(),
            file: c.file.clone(),
            reservation: c.reservation.clone(),
        };
        c.bytes += size;
        c.chunks += 1;
        let context = WriteContext {
            verify_after_write: shared.verifies_writes(),
            pool: shared.pool.clone(),
            state: shared.state.clone(),
            cancelled: shared.cancelled.clone(),
            hooks: shared.hooks.clone(),
            part_limit: shared.part_limit,
        };
        let job = shared.next_job.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        shared.submit(
            key,
            Box::new(move || {
                let _ = sender.send(context.store(job, input));
            }),
        )?;
        self.pending.push_back(PendingPart::Chunk {
            bytes: size,
            receiver: Mutex::new(receiver),
        });
        self.pending_bytes += size;
        self.buffer_source_bytes = 0;
        self.last_flush = Instant::now();
        shared.stage("QUEUED", size);
        Ok(())
    }
    pub fn sync(&mut self) -> Result<()> {
        if self.manifest.is_some() {
            self.flush_part()?;
            self.seal_current();
            while self.receive_part(true)? {}
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
        self.poll()?;
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
        require(
            self.pending.is_empty() && self.current.is_none() && self.buffer.is_empty(),
            "INVALID_STATE",
            "truncate has pending writes",
        )?;
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
    gate: impl FnMut() -> Result<()>,
) -> Result<()> {
    let bytes = io::identity(file)?.bytes;
    verify_range(file, 0, bytes, expected, pool, gate)
}
/// Read back `length` bytes at `offset` and compare their BLAKE3.
fn verify_range(
    file: &File,
    start: u64,
    length: u64,
    expected: &str,
    pool: &CpuPool,
    mut gate: impl FnMut() -> Result<()>,
) -> Result<()> {
    let before = io::identity(file)?;
    require(
        start
            .checked_add(length)
            .is_some_and(|end| end <= before.bytes),
        "TRUNCATED_RECORD",
        "storage part readback range",
    )?;
    let mut hash = blake3::Hasher::new();
    let mut offset = 0;
    let mut buffer = vec![0; (1 << 20).min(length.max(1) as usize)];
    while offset < length {
        gate()?;
        let n = (length - offset).min(buffer.len() as u64) as usize;
        io::read_exact_at(file, &mut buffer[..n], start + offset, 1 << 20)?;
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
/// Number of mapped extents (FIEMAP), for fragmentation diagnostics only.
/// Unsupported filesystems return None; the result never affects the copy.
fn extent_count(file: &File) -> Option<u32> {
    use std::os::fd::AsRawFd;
    #[repr(C)]
    struct Fiemap {
        start: u64,
        length: u64,
        flags: u32,
        mapped: u32,
        count: u32,
        reserved: u32,
    }
    const FS_IOC_FIEMAP: libc::c_ulong = 0xC020_660B;
    let mut map = Fiemap {
        start: 0,
        length: u64::MAX,
        flags: 0,
        mapped: 0,
        count: 0,
        reserved: 0,
    };
    // SAFETY: valid descriptor; with count=0 the kernel only writes the header.
    let result = unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_FIEMAP as _, &mut map) };
    (result == 0).then_some(map.mapped)
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
    fn first_file_calibration_is_bounded_for_cached_write_and_readback() {
        assert_eq!(Shared::calibrated_size(1 << 20, 4.0), 1 << 20);
        assert_eq!(Shared::calibrated_size(1 << 20, 1000.0), MIN);
        assert_eq!(Shared::calibrated_size(1 << 20, 0.001), MAX);
    }
    #[test]
    fn cached_admission_is_conservative_and_later_slow_writes_resize_parts() {
        let shared = Shared::new(&"1".repeat(32), 2).unwrap();
        shared.calibrate(1 << 20, 0.002);
        assert_eq!(shared.snapshot().baseline_bytes, FIRST);
        for _ in 0..4 {
            shared.calibrate(8 << 20, 0.5);
        }
        let fast = shared.snapshot().baseline_bytes;
        assert!(fast > FIRST && fast <= MAX);
        shared.calibrate(8 << 20, 200.0);
        assert!(shared.snapshot().baseline_bytes < fast);
        assert!(shared.snapshot().baseline_bytes >= MIN);
    }
    #[test]
    fn appends_share_one_part_and_publish_the_descriptor_only_when_sealed() {
        use std::sync::atomic::AtomicUsize;
        struct Count(AtomicUsize);
        impl Hooks for Count {
            fn at(&self, point: &'static str) -> Result<()> {
                if point == "before_descriptor_write" {
                    self.0.fetch_add(1, Ordering::Relaxed);
                }
                Ok(())
            }
        }
        let t = tempfile::tempdir().unwrap();
        let mut shared = Shared::new(&"1".repeat(32), 2).unwrap();
        let count = Arc::new(Count(AtomicUsize::new(0)));
        Arc::get_mut(&mut shared).unwrap().hooks = count.clone();
        let plan = Plan {
            format_version: 3,
            segmented_storage: true,
            ..Plan::default()
        };
        let path = t.path().join("00000000.pbk.partial");
        let mut file = DataFile::create(&path, &plan, &"1".repeat(32), 0, shared.clone()).unwrap();
        file.write_all(&vec![0x53; 8 << 20]).unwrap();
        file.sync().unwrap();
        let manifest: Manifest = io::read_json(&path).unwrap();
        assert_eq!(manifest.schema_version, 2);
        assert_eq!(manifest.bytes, 8 << 20);
        assert_eq!(manifest.parts.len(), 1);
        assert_eq!(manifest.parts[0].bytes, 8 << 20);
        assert_eq!(shared.snapshot().written_blocks, 8);
        // One descriptor at creation and one when the part is sealed.
        assert_eq!(count.0.load(Ordering::Relaxed), 2);
        assert_eq!(shared.snapshot().confirmed_bytes, 8 << 20);
        assert_eq!(shared.snapshot().confirmed_blocks, 8);
        let names = fs::read_dir(t.path().join("segments/00000000"))
            .unwrap()
            .count();
        assert_eq!(names, 1);
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
        while f.receive_part(true).unwrap() {}
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
    fn small_appends_finish_verify_and_restore_and_schema_one_parts_stay_readable() {
        use crate::{archive::CsvRows, backup, control::RunControl, restore, v2, verify};
        use crispawn_core::types::{Codec, Metadata, NoEvents};
        let mut state = 0x6a09e667f3bcc908u64;
        let original: Vec<_> = (0..24 << 20)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        for checked in [true, false] {
            let t = tempfile::tempdir().unwrap();
            let source = t.path().join("source");
            fs::write(&source, &original).unwrap();
            let archive = t.path().join("archive");
            let plan = Plan {
                format_version: 3,
                segmented_storage: true,
                regions: 1,
                workers: 2,
                range_bytes: 1 << 20,
                read_bytes: 64 << 10,
                container_bytes: 64 << 20,
                codec: Codec::Zstd,
                compression_level: 3,
                memory_limit_bytes: 1 << 30,
                ..Plan::default()
            };
            let control = RunControl::new();
            control.set_backup_verification(checked);
            assert!(
                backup::backup(
                    &source,
                    &archive,
                    plan.clone(),
                    t.path(),
                    &NoEvents,
                    &NoHooks,
                    &control
                )
                .unwrap()
                .dataset_committed
            );
            let meta: Metadata = io::read_json(&archive.join("metadata.json")).unwrap();
            let mut index =
                CsvRows::<v2::FileEntry>::open(&archive.join("files.csv"), &v2::SEGMENTED_COLUMNS)
                    .unwrap();
            let mut entry = index.next_row().unwrap().unwrap();
            assert!(index.next_row().unwrap().is_none());
            drop(index);
            let path = archive.join(&entry.relative_path);
            let old = DataFile::read(&path, true).unwrap();
            let mut encoded = vec![0; old.metadata().unwrap().len() as usize];
            io::read_exact_at(&old, &mut encoded, 0, 65536).unwrap();
            drop(old);
            fs::rename(&path, t.path().join("original-descriptor")).unwrap();
            fs::remove_file(archive.join("COMMITTED")).unwrap();
            // Keep the real encoded stream. Exercise the production writer and
            // seal receipt with its legitimate 64 KiB lower bound, without
            // spending minutes emulating a slow device or adding a runtime knob.
            let mut shared = Shared::new(&meta.backup_id, 2).unwrap();
            Arc::get_mut(&mut shared).unwrap().capacity = MIN;
            shared.state.lock().unwrap().baseline = MIN;
            shared.verify_after_write.store(checked, Ordering::Relaxed);
            let mut stored =
                DataFile::create(&path, &plan, &meta.backup_id, entry.file_id, shared.clone())
                    .unwrap();
            stored.write_all(&encoded).unwrap();
            stored.sync().unwrap();
            let hash = stored.descriptor_blake3().unwrap().unwrap();
            drop(stored);
            let bytes = fs::metadata(&path).unwrap().len();
            // Many 64 KiB appends no longer create one file or descriptor entry each.
            assert!(bytes < 4096, "descriptor_bytes={bytes}");
            assert!(shared.snapshot().written_blocks > 64);
            assert_eq!(
                DataFile::read(&path, true)
                    .unwrap()
                    .manifest
                    .unwrap()
                    .parts
                    .len(),
                1
            );
            shared.finish_file(&path, entry.file_id, &hash).unwrap();
            entry.storage_descriptor_blake3 = Some(hash);
            v2::write_indices(&archive, &[entry.clone()]).unwrap();
            v2::write_commit_with_verification(&archive, &meta.backup_id, checked).unwrap();
            verify::verify(&archive, t.path(), &NoEvents).unwrap();
            let target = t.path().join("target");
            assert!(
                restore::restore(&archive, &target, false, t.path(), &NoEvents, &NoHooks)
                    .unwrap()
                    .restored_ranges_verified
            );
            assert_eq!(fs::read(&target).unwrap(), original);
            // Archives written before schema 2 used one file per <=16 MiB part
            // and could exceed 64 KiB descriptors. They must still verify/restore.
            let old_dir = t.path().join("schema-one");
            fs::create_dir(&old_dir).unwrap();
            let mut parts = vec![];
            for (n, chunk) in encoded.chunks(64 << 10).enumerate() {
                let relative = format!("segments/{:08x}/{n:032x}.bin", entry.file_id);
                let part = path.parent().unwrap().join(&relative);
                fs::create_dir_all(part.parent().unwrap()).unwrap();
                fs::write(&part, chunk).unwrap();
                parts.push(Part {
                    path: relative,
                    stream_offset: (n * (64 << 10)) as u64,
                    bytes: chunk.len() as u64,
                    used: chunk.len() as u64,
                    blake3: blake3::hash(chunk).to_hex().to_string(),
                });
            }
            let legacy = Manifest {
                kind: "crispawn.segmented_file".into(),
                schema_version: 1,
                backup_id: meta.backup_id.clone(),
                file_id: entry.file_id,
                bytes: encoded.len() as u64,
                parts,
            };
            let legacy_bytes = io::json(&legacy).unwrap();
            assert!(legacy_bytes.len() > 65536);
            fs::write(&path, &legacy_bytes).unwrap();
            entry.storage_descriptor_blake3 =
                Some(blake3::hash(&legacy_bytes).to_hex().to_string());
            v2::write_indices(&archive, &[entry.clone()]).unwrap();
            fs::remove_file(archive.join("COMMITTED")).unwrap();
            v2::write_commit_with_verification(&archive, &meta.backup_id, checked).unwrap();
            verify::verify(&archive, t.path(), &NoEvents).unwrap();
            fs::remove_file(&target).unwrap();
            assert!(
                restore::restore(&archive, &target, false, t.path(), &NoEvents, &NoHooks)
                    .unwrap()
                    .restored_ranges_verified
            );
            assert_eq!(fs::read(&target).unwrap(), original);
            // A larger parsing budget must never bypass descriptor integrity.
            use std::io::Write;
            fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(b" ")
                .unwrap();
            assert_eq!(
                verify::verify(&archive, t.path(), &NoEvents)
                    .unwrap_err()
                    .code,
                "STORED_HASH_MISMATCH"
            );
            let blocked = t.path().join("must-not-create");
            assert!(
                restore::restore(&archive, &blocked, false, t.path(), &NoEvents, &NoHooks).is_err()
            );
            assert!(!blocked.exists());
        }
    }
    #[test]
    fn descriptor_reader_uses_the_existing_eight_mib_format_limit() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("descriptor.pbk");
        let manifest = Manifest {
            kind: "crispawn.segmented_file".into(),
            schema_version: 1,
            backup_id: "1".repeat(32),
            file_id: 0,
            bytes: 512,
            parts: (0..512)
                .map(|n| Part {
                    path: format!("segments/00000000/{n:032x}.bin"),
                    stream_offset: n,
                    bytes: 1,
                    used: 1,
                    blake3: "2".repeat(64),
                })
                .collect(),
        };
        let mut bytes = io::json(&manifest).unwrap();
        assert!(bytes.len() > 65536);
        fs::write(&path, &bytes).unwrap();
        let read = DataFile::read(&path, true).unwrap();
        assert_eq!(read.manifest.unwrap().parts.len(), 512);
        bytes.resize(MAX_DESCRIPTOR as usize, b' ');
        fs::write(&path, &bytes).unwrap();
        assert_eq!(
            DataFile::read(&path, true)
                .unwrap()
                .metadata()
                .unwrap()
                .len(),
            512
        );
        let file = io::open_regular(&path, true).unwrap();
        file.set_len(MAX_DESCRIPTOR + 1).unwrap();
        assert_eq!(
            DataFile::read(&path, true).err().unwrap().code,
            "LIMIT_EXCEEDED"
        );
    }
    #[test]
    fn arbitrary_encoded_bytes_split_without_decoding_and_read_as_one_stream() {
        let (t, s, mut f) = fixture();
        let bytes = (0..(3 << 20) + 31)
            .map(|i| (i % 251) as u8)
            .collect::<Vec<_>>();
        f.write_all(&bytes).unwrap();
        f.sync().unwrap();
        // Four bounded appends (write/check units) into one physical part.
        assert_eq!(s.snapshot().confirmed_blocks, 4);
        let m = f.manifest.as_ref().unwrap();
        assert_eq!(m.parts.len(), 1);
        assert_eq!(m.parts[0].bytes, bytes.len() as u64);
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
                if p == "after_segment_write" {
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
        f.write_with_source(&vec![7; 1 << 20], 100_000_000).unwrap();
        assert_eq!(f.sync().unwrap_err().code, "STORED_HASH_MISMATCH");
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
    fn two_writers_serve_two_streams_while_observer_reports_wait_time() {
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
                    assert_eq!(s.write_workers, 2);
                    assert_eq!(s.active_write_workers, 2);
                    assert!(s.waiting_blocks >= 3);
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
        // Appends to one file are sequential; two streams keep both writers busy.
        let mut files = (0..2u64)
            .map(|id| {
                DataFile::create(
                    &t.path().join(format!("{id:08x}.pbk.partial")),
                    &plan,
                    &"1".repeat(32),
                    id,
                    shared.clone(),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        let events = Observe(AtomicUsize::new(0));
        let control = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let _stop = StopMonitor(shared.clone());
            scope.spawn(|| shared.monitor(&events, &control));
            let worker = scope.spawn(|| {
                for file in files.iter_mut() {
                    file.write_all(&vec![9; 3 << 20])?;
                }
                for file in files.iter_mut() {
                    file.sync()?;
                }
                Ok::<_, Error>(())
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
            assert_eq!(status.calibration_bytes, 1 << 20);
            assert!(status.calibration_elapsed_ms >= 250);
            assert!((MIN..=MAX).contains(&status.baseline_bytes));
        });
    }
    #[test]
    fn zero_header_buffers_wait_longer_before_a_time_flush() {
        let (_t, s, mut f) = fixture();
        write_frame(
            &mut f,
            &crate::v2_format::Frame {
                codec: types::Codec::Zero,
                level: 0,
                raw_bytes: 16 << 20,
                payload_bytes: 0,
            },
            3,
            &[],
        )
        .unwrap();
        f.last_flush = Instant::now() - Duration::from_secs_f64(GOAL + 1.0);
        assert!(
            !f.due(),
            "a 32-byte zero header must not become its own append"
        );
        f.last_flush = Instant::now() - Duration::from_secs_f64(SMALL_GOAL + 1.0);
        assert!(f.due());
        f.buffer.resize(MIN as usize, 1);
        f.last_flush = Instant::now() - Duration::from_secs_f64(GOAL + 1.0);
        assert!(f.due());
        f.sync().unwrap();
        assert_eq!(s.snapshot().written_blocks, 1);
    }
    #[test]
    fn parts_roll_over_at_the_physical_limit_and_read_back_as_one_stream() {
        let t = tempfile::tempdir().unwrap();
        let mut s = Shared::new(&"1".repeat(32), 2).unwrap();
        Arc::get_mut(&mut s).unwrap().part_limit = 3 << 20;
        s.calibrate(1 << 20, 4.0);
        let plan = Plan {
            format_version: 3,
            segmented_storage: true,
            ..Plan::default()
        };
        let path = t.path().join("00000000.pbk.partial");
        let mut f = DataFile::create(&path, &plan, &"1".repeat(32), 0, s.clone()).unwrap();
        let bytes = (0..(8 << 20) + 5)
            .map(|i| (i % 249) as u8)
            .collect::<Vec<_>>();
        f.write_all(&bytes).unwrap();
        f.sync().unwrap();
        let m = f.manifest.as_ref().unwrap();
        assert_eq!(m.parts.len(), 3);
        assert!(m.parts.iter().all(|p| p.bytes <= 3 << 20));
        assert_eq!(
            m.parts.iter().map(|p| p.bytes).sum::<u64>(),
            bytes.len() as u64
        );
        let expected = f.descriptor_blake3().unwrap().unwrap();
        let r = DataFile::read(&path, true).unwrap();
        r.check_descriptor(&expected).unwrap();
        r.verify_parts(Some(&s), &AtomicBool::new(false)).unwrap();
        let mut actual = vec![0; bytes.len()];
        io::read_exact_at(&r, &mut actual, 0, 777_777).unwrap();
        assert_eq!(actual, bytes);
        let summary = s.state.lock().unwrap().summary.parts_sealed;
        assert_eq!(summary, 3);
    }
    #[test]
    fn new_parts_preallocate_without_changing_length_and_trim_when_sealed() {
        // A real disk filesystem (the crate directory), not a small tmpfs.
        let t = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let mut s = Shared::new(&"1".repeat(32), 2).unwrap();
        Arc::get_mut(&mut s).unwrap().part_limit = PREALLOCATE_MIN;
        s.calibrate(1 << 20, 4.0);
        let plan = Plan {
            format_version: 3,
            segmented_storage: true,
            ..Plan::default()
        };
        let path = t.path().join("00000000.pbk.partial");
        let mut f = DataFile::create(&path, &plan, &"1".repeat(32), 0, s.clone()).unwrap();
        let bytes = (0..(3 << 20) + 7)
            .map(|i| (i % 251) as u8)
            .collect::<Vec<_>>();
        f.write_all(&bytes).unwrap();
        f.flush_part().unwrap();
        while f.pending_chunks() > 0 {
            f.receive_part(true).unwrap();
        }
        let part = f.current.as_ref().unwrap().file.get().unwrap();
        assert_eq!(part.metadata().unwrap().len(), bytes.len() as u64);
        let summary = |s: &Shared| {
            let st = s.state.lock().unwrap();
            (
                st.summary.parts_preallocated,
                st.summary.preallocate_failures,
            )
        };
        let (granted, failed) = summary(&s);
        let free = io::available_bytes(t.path()).unwrap();
        if free > crate::destination_space::MIN_FREE_BYTES + 8 * PREALLOCATE_MIN {
            assert_eq!((granted, failed), (1, 0), "ext4/xfs support KEEP_SIZE");
            assert!(allocated_bytes(part).unwrap() >= PREALLOCATE_MIN);
        }
        f.sync().unwrap();
        let records = std::mem::take(&mut s.state.lock().unwrap().diagnostics);
        let sealed = records
            .iter()
            .find(|(n, _)| *n == "storage_part")
            .map(|(_, v)| v.clone())
            .unwrap();
        let length = bytes.len() as u64;
        assert!(sealed["allocated_bytes"].as_u64().unwrap() <= length + TRIM_SLACK);
        assert_eq!(sealed["preallocated"], granted * PREALLOCATE_MIN);
        let r = DataFile::read(&path, true).unwrap();
        r.verify_parts(Some(&s), &AtomicBool::new(false)).unwrap();
        let mut actual = vec![0; bytes.len()];
        io::read_exact_at(&r, &mut actual, 0, 777_777).unwrap();
        assert_eq!(actual, bytes);
    }
    #[test]
    fn schema_two_parts_above_the_old_limit_are_rejected_by_schema_one_rules() {
        let mut m = Manifest {
            kind: "crispawn.segmented_file".into(),
            schema_version: 2,
            backup_id: "1".repeat(32),
            file_id: 0,
            bytes: PART_LIMIT,
            parts: vec![Part {
                path: format!("segments/00000000/{:032x}.bin", 1),
                stream_offset: 0,
                bytes: PART_LIMIT,
                used: PART_LIMIT,
                blake3: "2".repeat(64),
            }],
        };
        m.validate().unwrap();
        m.schema_version = 1;
        assert_eq!(m.validate().unwrap_err().code, "INVALID_STORAGE_PART");
        m.schema_version = 2;
        m.parts[0].bytes = PART_LIMIT + 1;
        m.parts[0].used = PART_LIMIT + 1;
        m.bytes = PART_LIMIT + 1;
        assert_eq!(m.validate().unwrap_err().code, "INVALID_STORAGE_PART");
        m.schema_version = 3;
        assert_eq!(m.validate().unwrap_err().code, "UNKNOWN_VERSION");
    }
}
