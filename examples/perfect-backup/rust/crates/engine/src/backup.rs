// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
use crate::{
    archive::{self, Index},
    codec,
    control::ExecutionControl,
    io,
    journal::{self, Change, FileState, Journal, Replay},
    verify, Error, Hooks, Reporter, Result,
};
use perfect_backup_core::{
    error::require,
    format::{self, Record, FILE_HEADER, FOOTER, RECORD_HEADER},
    plan::{Plan, Schedule, MAX_FILES},
    types::{self, EventSink, FileEntry, Metadata, RangeEntry, SourceIdentity},
};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Seek, SeekFrom},
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub fn backup(
    source: &Path,
    root: &Path,
    plan: Plan,
    scratch: &Path,
    events: &dyn EventSink,
    hooks: &dyn Hooks,
    cancel: &dyn ExecutionControl,
) -> Result<verify::Report> {
    backup_checked(source, root, plan, scratch, events, hooks, cancel, None)
}
/// Keep the source identity used by a probe bound to the actual opened source.
pub fn backup_checked(
    source: &Path,
    root: &Path,
    plan: Plan,
    scratch: &Path,
    events: &dyn EventSink,
    hooks: &dyn Hooks,
    cancel: &dyn ExecutionControl,
    expected: Option<&SourceIdentity>,
) -> Result<verify::Report> {
    let source = io::open_regular(source, false)?;
    backup_opened(source, root, plan, scratch, events, hooks, cancel, expected)
}
pub fn backup_device_checked(
    source: &Path,
    root: &Path,
    plan: Plan,
    scratch: &Path,
    events: &dyn EventSink,
    hooks: &dyn Hooks,
    cancel: &dyn ExecutionControl,
    expected: &SourceIdentity,
) -> Result<verify::Report> {
    require(
        expected.block_device.is_some(),
        "BLOCK_DEVICE_REQUIRED",
        "expected block identity",
    )?;
    let source = crate::device::open(source, false, true)?;
    backup_opened(
        source,
        root,
        plan,
        scratch,
        events,
        hooks,
        cancel,
        Some(expected),
    )
}
fn backup_opened(
    source: File,
    root: &Path,
    plan: Plan,
    scratch: &Path,
    events: &dyn EventSink,
    hooks: &dyn Hooks,
    cancel: &dyn ExecutionControl,
    expected: Option<&SourceIdentity>,
) -> Result<verify::Report> {
    if matches!(plan.format_version, 2 | 3) {
        return crate::v2_backup::backup_opened(
            source, root, plan, scratch, events, hooks, cancel, expected,
        );
    }
    cancel.check_cancel()?;
    io::lock_file(&source, false)?;
    if let Some(expected) = expected {
        io::unchanged(&source, expected)?;
    }
    let identity = io::identity(&source)?;
    plan.validate(identity.bytes)?;
    let parent = root
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    crate::destination_space::check(parent)?;
    fs::create_dir(root)?;
    fs::create_dir(root.join("data"))?;
    io::sync_file(&io::create_new(&root.join(".lock"))?)?;
    let _lock = io::Lock::acquire(root, true)?;
    let id = io::new_id()?;
    let meta = Metadata {
        format_version: 1,
        backup_id: types::hex(&id),
        source_kind: if identity.block_device.is_some() {
            "linux-whole-block-device"
        } else {
            "regular-file-fixture"
        }
        .into(),
        logical_sector_bytes: identity
            .block_device
            .as_ref()
            .map(|b| b.logical_sector_bytes)
            .unwrap_or(512),
        physical_sector_bytes: identity
            .block_device
            .as_ref()
            .map(|b| b.physical_sector_bytes)
            .unwrap_or(512),
        source: identity,
        hash_algorithm: "sha256".into(),
        created_unix_seconds: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Error::new("CLOCK_ERROR", "system clock before epoch"))?
            .as_secs()
            .to_string(),
        application_version: env!("CARGO_PKG_VERSION").into(),
        source_consistency: if expected.and_then(|i| i.block_device.as_ref()).is_some() {
            "exclusive-open+unmounted-whole-device"
        } else {
            "caller-quiescent-file+identity+timestamps"
        }
        .into(),
        boot_exclusion: if expected.and_then(|i| i.block_device.as_ref()).is_some() {
            "live-controller-excludes-boot-and-destination-whole-device"
        } else {
            "not-applicable-regular-file-fixture"
        }
        .into(),
    };
    io::atomic_write(root, "metadata.json", &io::json(&meta)?)?;
    io::atomic_write(root, "plan.json", &io::json(&plan)?)?;
    io::sync_file(&io::create_new(&root.join("journal.jsonl"))?)?;
    io::sync_dir(&root.join("data"))?;
    io::sync_dir(root)?;
    io::sync_dir(parent)?;
    run(
        source, root, &meta, &plan, id, scratch, events, hooks, cancel,
    )
}
pub fn resume(
    source: &Path,
    root: &Path,
    scratch: &Path,
    events: &dyn EventSink,
    hooks: &dyn Hooks,
    cancel: &dyn ExecutionControl,
) -> Result<verify::Report> {
    resume_inner(source, root, scratch, events, hooks, cancel, None)
}
pub fn resume_device(
    source: &Path,
    root: &Path,
    scratch: &Path,
    events: &dyn EventSink,
    hooks: &dyn Hooks,
    cancel: &dyn ExecutionControl,
    expected: &SourceIdentity,
) -> Result<verify::Report> {
    require(
        expected.block_device.is_some(),
        "BLOCK_DEVICE_REQUIRED",
        "expected whole-device identity",
    )?;
    resume_inner(source, root, scratch, events, hooks, cancel, Some(expected))
}
fn resume_inner(
    source: &Path,
    root: &Path,
    scratch: &Path,
    events: &dyn EventSink,
    hooks: &dyn Hooks,
    cancel: &dyn ExecutionControl,
    expected: Option<&SourceIdentity>,
) -> Result<verify::Report> {
    let root = io::canonical_archive(root)?;
    let _lock = io::Lock::acquire(&root, true)?;
    require(
        !root.join("COMMITTED").try_exists()?,
        "ALREADY_COMMITTED",
        "verify the complete archive instead of resuming",
    )?;
    if crate::v2::is_v2(&root)? {
        return crate::v2_backup::resume(source, &root, events, hooks, cancel, expected);
    }
    let (meta, plan, id) = archive::load(&root)?;
    require(
        meta.source.block_device.is_some() == expected.is_some(),
        "ADAPTER_MISMATCH",
        "resume must explicitly select the recorded source adapter",
    )?;
    let source = if expected.is_some() {
        crate::device::open(source, false, true)?
    } else {
        io::open_regular(source, false)?
    };
    io::lock_file(&source, false)?;
    if let Some(expected) = expected {
        io::unchanged(&source, expected)?;
    }
    io::unchanged(&source, &meta.source)?;
    run(
        source, &root, &meta, &plan, id, scratch, events, hooks, cancel,
    )
}
fn run(
    source: File,
    root: &Path,
    meta: &Metadata,
    plan: &Plan,
    id: [u8; 16],
    scratch: &Path,
    events: &dyn EventSink,
    hooks: &dyn Hooks,
    cancel: &dyn ExecutionControl,
) -> Result<verify::Report> {
    let replay = journal::replay(root, meta, plan, id, scratch)?;
    let mut reporter = Reporter::new(events, meta.backup_id.clone(), meta.source.bytes);
    reporter.durable = replay.durable_raw;
    reporter.read = replay.durable_raw;
    reporter.emit("PREFLIGHT", "SOURCE_AND_PLAN_VALIDATED");
    let mut writer = Writer::recover(root, meta, plan, id, replay, &source, hooks)?;
    crate::destination_space::check(root)?;
    reporter.durable = writer.durable;
    reporter.emit("COPYING", "COPY_STARTED");
    pipeline(
        &source,
        meta,
        plan,
        &mut writer,
        &mut reporter,
        hooks,
        cancel,
    )?;
    writer.seal()?;
    crate::destination_space::check(root)?;
    io::unchanged(&source, &meta.source)?;
    require(
        writer.index.present == writer.index.count,
        "MISSING_RANGE",
        "copy ended with missing ranges",
    )?;
    let files = writer
        .states
        .iter()
        .map(|s| {
            s.sealed
                .clone()
                .ok_or_else(|| Error::new("UNSEALED_FILE", "container not sealed"))
        })
        .collect::<Result<Vec<_>>>()?;
    hooks.at("index_write")?;
    archive::write_indices(root, &files, &writer.index)?;
    verify::write_checksums(root)?;
    hooks.at("after_indices")?;
    reporter.durable = meta.source.bytes;
    reporter.read = meta.source.bytes;
    let v = verify::inspect(root, scratch, false, &mut reporter, cancel)?;
    io::unchanged(&source, &meta.source)?;
    crate::destination_space::gate(&mut reporter, cancel, root, "VERIFYING")?;
    require(
        !cancel.cancelled(),
        "CANCELLED",
        "cancelled before final commit",
    )?;
    hooks.at("before_commit")?;
    crate::destination_space::check(root)?;
    verify::write_commit(root, meta.backup_id.clone())?;
    reporter.emit("COMPLETE", "BACKUP_COMMITTED");
    let mut result = v.report();
    result.operation = "backup".into();
    Ok(result)
}

struct Active {
    file: File,
    state: FileState,
    hash: Sha256,
}
struct Writer<'a> {
    root: &'a Path,
    id: [u8; 16],
    plan: &'a Plan,
    states: Vec<FileState>,
    active: Option<Active>,
    index: Index,
    journal: Journal,
    pending: Vec<RangeEntry>,
    durable: u64,
    hooks: &'a dyn Hooks,
}
impl<'a> Writer<'a> {
    fn recover(
        root: &'a Path,
        meta: &Metadata,
        plan: &'a Plan,
        id: [u8; 16],
        mut replay: Replay,
        source: &File,
        hooks: &'a dyn Hooks,
    ) -> Result<Self> {
        let mut known = std::collections::BTreeSet::new();
        let mut orphan = false;
        // Validate every committed archive/source range before truncating or renaming anything.
        for s in &replay.files {
            let final_name = types::data_name(s.id, false);
            let part_name = types::data_name(s.id, true);
            let final_exists = root.join(&final_name).try_exists()?;
            let part_exists = root.join(&part_name).try_exists()?;
            require(
                final_exists ^ part_exists,
                "RECOVERY_AMBIGUOUS",
                "expected exactly one final or partial container",
            )?;
            let name = if final_exists { final_name } else { part_name };
            known.insert(Path::new(&name).file_name().unwrap().to_owned());
            let compare = |r: &Record, offset: u64, _: &[u8]| -> Result<()> {
                journal::compare_record(&replay.index, r, offset)?;
                let mut raw = vec![0; r.raw_bytes as usize];
                hooks.at("resume_source_read")?;
                io::read_exact_at(source, &mut raw, r.source_offset, plan.read_bytes)?;
                require(
                    types::hash(&raw) == r.raw_hash,
                    "SOURCE_CHANGED",
                    "committed source range changed",
                )
            };
            if let Some(e) = &s.sealed {
                require(
                    final_exists,
                    "RECOVERY_AMBIGUOUS",
                    "sealed file still has partial name",
                )?;
                archive::scan_sealed(root, e, id, meta, plan, compare)?;
            } else {
                let f = io::open_regular(&root.join(name), false)?;
                let scan = archive::scan_prefix(&f, id, s.id, s.end, meta, plan, compare)?;
                require(
                    scan.count == s.count && scan.raw == s.raw,
                    "JOURNAL_RECORD_MISMATCH",
                    "prefix totals",
                )?;
                if final_exists {
                    require(
                        f.metadata()?.len() == s.end + FOOTER as u64,
                        "INVALID_CONTAINER",
                        "renamed file length",
                    )?;
                    let mut b = [0; FOOTER];
                    io::read_exact_at(&f, &mut b, s.end, FOOTER as u64)?;
                    format::check_footer(&b, id, s.id, s.count, s.raw, s.end)?;
                }
            }
        }
        let next = replay.files.len() as u64;
        let orphan_name = types::data_name(next, true);
        if root.join(&orphan_name).try_exists()? {
            require(
                replay
                    .files
                    .last()
                    .map(|s| s.sealed.is_some())
                    .unwrap_or(true),
                "RECOVERY_AMBIGUOUS",
                "orphan beside open container",
            )?;
            let f = io::open_regular(&root.join(&orphan_name), false)?;
            let mut b = [0; FILE_HEADER];
            io::read_exact_at(&f, &mut b, 0, FILE_HEADER as u64)?;
            format::check_file_header(&b, id, next)?;
            known.insert(Path::new(&orphan_name).file_name().unwrap().to_owned());
            orphan = true;
        }
        let actual = fs::read_dir(root.join("data"))?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<std::io::Result<std::collections::BTreeSet<_>>>()?;
        require(
            actual == known,
            "UNREFERENCED_DATA",
            "unknown data files are preserved; automatic recovery stopped",
        )?;
        io::unchanged(source, &meta.source)?;
        let mut journal = Journal::open(root, meta.backup_id.clone(), &replay)?;
        if let Some(s) = replay.files.last_mut() {
            if s.sealed.is_none() && root.join(types::data_name(s.id, false)).try_exists()? {
                let f = io::open_regular(&root.join(types::data_name(s.id, false)), false)?;
                let e = FileEntry {
                    file_id: s.id,
                    relative_path: types::data_name(s.id, false),
                    stored_bytes: s.end + FOOTER as u64,
                    stored_sha256: io::hash_file(&f)?,
                    raw_bytes_total: s.raw,
                    range_count: s.count,
                };
                io::sync_file(&f)?;
                io::sync_dir(&root.join("data"))?;
                journal.append(Change::Seal { file: e.clone() }, hooks)?;
                s.sealed = Some(e);
            }
        }
        if orphan {
            replay.files.push(FileState::new(next));
        }
        let active = if let Some(s) = replay.files.last().filter(|s| s.sealed.is_none()) {
            let mut file = io::open_regular(&root.join(types::data_name(s.id, true)), true)?;
            file.set_len(s.end)?;
            io::sync_file(&file)?;
            file.seek(SeekFrom::Start(s.end))?;
            let scan = archive::scan_prefix(&file, id, s.id, s.end, meta, plan, |_, _, _| Ok(()))?;
            Some(Active {
                file,
                state: s.clone(),
                hash: scan.hash,
            })
        } else {
            None
        };
        Ok(Self {
            root,
            id,
            plan,
            states: replay.files,
            active,
            index: replay.index,
            journal,
            pending: vec![],
            durable: replay.durable_raw,
            hooks,
        })
    }
    fn start(&mut self) -> Result<()> {
        let id = self.states.len() as u64;
        require(id < MAX_FILES, "LIMIT_EXCEEDED", "too many containers")?;
        let mut file = io::create_new(&self.root.join(types::data_name(id, true)))?;
        let head = format::file_header(self.id, id);
        self.hooks.at("data_write")?;
        io::write_all(&mut file, &head)?;
        let mut hash = Sha256::new();
        hash.update(head);
        let state = FileState::new(id);
        self.states.push(state.clone());
        self.active = Some(Active { file, state, hash });
        Ok(())
    }
    fn append(&mut self, frame: Frame) -> Result<()> {
        self.hooks.at("destination_space_check")?;
        crate::destination_space::check(self.root)?;
        let size = RECORD_HEADER as u64 + frame.encoded.payload.len() as u64;
        if self
            .active
            .as_ref()
            .map(|a| a.state.end + size + FOOTER as u64 > self.plan.container_bytes)
            .unwrap_or(false)
        {
            self.seal()?;
        }
        if self.active.is_none() {
            self.start()?;
        }
        let active = self.active.as_mut().unwrap();
        let r = Record {
            backup_id: self.id,
            file_id: active.state.id,
            range_id: frame.id,
            source_offset: frame.offset,
            raw_bytes: frame.length,
            payload_bytes: frame.encoded.payload.len() as u64,
            codec: frame.encoded.codec,
            level: frame.encoded.level,
            raw_hash: frame.hash,
        };
        let head = r.encode();
        self.hooks.at("data_write")?;
        io::write_all(&mut active.file, &head)?;
        io::write_all(&mut active.file, &frame.encoded.payload)?;
        self.hooks.at("after_data_write")?;
        self.index.put(&r, active.state.end)?;
        self.pending.push(r.entry(active.state.end));
        active.hash.update(head);
        active.hash.update(&frame.encoded.payload);
        active.state.end += size;
        active.state.count += 1;
        active.state.raw += r.raw_bytes;
        if self.pending.len() >= self.plan.checkpoint_records as usize {
            self.checkpoint()?;
        }
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let active = self.active.as_mut().unwrap();
        self.hooks.at("data_sync")?;
        io::sync_file(&active.file)?;
        io::sync_dir(&self.root.join("data"))?;
        self.hooks.at("after_data_sync")?;
        self.journal.append(
            Change::Checkpoint {
                file_id: active.state.id,
                end: active.state.end.to_string(),
                records: self.pending.clone(),
            },
            self.hooks,
        )?;
        self.durable += self
            .pending
            .iter()
            .map(|e| e.source_length_bytes)
            .sum::<u64>();
        self.pending.clear();
        self.states[active.state.id as usize] = active.state.clone();
        self.hooks.at("after_checkpoint")?;
        Ok(())
    }
    fn seal(&mut self) -> Result<()> {
        self.checkpoint()?;
        if let Some(mut active) = self.active.take() {
            require(
                active.state.count > 0,
                "EMPTY_CONTAINER",
                "cannot seal an empty container",
            )?;
            let footer = format::footer(
                self.id,
                active.state.id,
                active.state.count,
                active.state.raw,
                active.state.end,
            );
            self.hooks.at("data_write")?;
            io::write_all(&mut active.file, &footer)?;
            active.hash.update(footer);
            self.hooks.at("data_sync")?;
            io::sync_file(&active.file)?;
            self.hooks.at("after_footer_sync")?;
            io::rename_new(
                &self.root.join(types::data_name(active.state.id, true)),
                &self.root.join(types::data_name(active.state.id, false)),
            )?;
            io::sync_dir(&self.root.join("data"))?;
            self.hooks.at("after_rename")?;
            let e = FileEntry {
                file_id: active.state.id,
                relative_path: types::data_name(active.state.id, false),
                stored_bytes: active.state.end + FOOTER as u64,
                stored_sha256: types::hex(&active.hash.finalize()),
                raw_bytes_total: active.state.raw,
                range_count: active.state.count,
            };
            self.journal
                .append(Change::Seal { file: e.clone() }, self.hooks)?;
            active.state.sealed = Some(e);
            let state_id = active.state.id as usize;
            self.states[state_id] = active.state;
            self.hooks.at("after_seal")?;
        }
        Ok(())
    }
}

struct Frame {
    id: u64,
    offset: u64,
    length: u64,
    hash: [u8; 32],
    encoded: codec::Encoded,
}
fn pipeline(
    source: &File,
    meta: &Metadata,
    plan: &Plan,
    writer: &mut Writer<'_>,
    report: &mut Reporter<'_>,
    hooks: &dyn Hooks,
    cancel: &dyn ExecutionControl,
) -> Result<()> {
    let stop = AtomicBool::new(false);
    let read_bytes = AtomicU64::new(writer.durable);
    std::thread::scope(|scope| -> Result<()> {
        let (task_tx, task_rx) = mpsc::sync_channel::<u64>(plan.workers as usize);
        let task_rx = Arc::new(Mutex::new(task_rx));
        let (result_tx, result_rx) = mpsc::sync_channel::<Result<Frame>>(plan.workers as usize);
        let mut workers = vec![];
        for _ in 0..plan.workers {
            let rx = Arc::clone(&task_rx);
            let tx = result_tx.clone();
            let stop = &stop;
            let read_bytes = &read_bytes;
            workers.push(scope.spawn(move || loop {
                let task = match rx.lock() {
                    Ok(rx) => rx.recv(),
                    Err(_) => break,
                };
                let id = match task {
                    Ok(id) => id,
                    Err(_) => break,
                };
                if stop.load(Ordering::Relaxed) || cancel.cancelled() {
                    break;
                }
                let result = (|| -> Result<Frame> {
                    let (offset, length) = plan.range(meta.source.bytes, id)?;
                    let mut raw = vec![0; length as usize];
                    hooks.at("source_read")?;
                    io::read_exact_at(source, &mut raw, offset, plan.read_bytes)?;
                    read_bytes.fetch_add(length, Ordering::Relaxed);
                    let hash = types::hash(&raw);
                    hooks.at("codec_encode")?;
                    let encoded = codec::encode(raw, plan.codec, plan.compression_level)?;
                    Ok(Frame {
                        id,
                        offset,
                        length,
                        hash,
                        encoded,
                    })
                })();
                let failed = result.is_err();
                if tx.send(result).is_err() || failed {
                    break;
                }
            }));
        }
        drop(result_tx);
        let mut schedule = Schedule::new(plan.range_count(meta.source.bytes), plan.regions);
        let mut next = None;
        let mut exhausted = false;
        let mut inflight = 0;
        let outcome = (|| -> Result<()> {
            let mut space = crate::destination_space::Monitor::new(writer.root)?;
            loop {
                space.poll(writer.root)?;
                if cancel.cancelled() {
                    return Err(Error::new("CANCELLED", "copy cancelled"));
                }
                if cancel.pause_requested() && inflight == 0 {
                    writer.checkpoint()?;
                    report.durable = writer.durable;
                    report.read = read_bytes.load(Ordering::Relaxed);
                    crate::destination_space::gate(report, cancel, writer.root, "COPYING")?;
                }
                while !exhausted
                    && !cancel.pause_requested()
                    && inflight < plan.workers as usize * 2
                {
                    if next.is_none() {
                        next = schedule.find(|id| !writer.index.contains(*id));
                        if next.is_none() {
                            exhausted = true;
                            break;
                        }
                    }
                    match task_tx.try_send(next.unwrap()) {
                        Ok(()) => {
                            inflight += 1;
                            next = None;
                        }
                        Err(mpsc::TrySendError::Full(_)) => break,
                        Err(_) => {
                            return Err(Error::new("WORKER_FAILED", "reader queue disconnected"))
                        }
                    }
                }
                if exhausted && inflight == 0 {
                    return Ok(());
                }
                match result_rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(frame) => {
                        inflight -= 1;
                        writer.append(frame?)?;
                        report.durable = writer.durable;
                        report.read = read_bytes.load(Ordering::Relaxed);
                        report.emit("COPYING", "CHECKPOINT_PROGRESS");
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        let read = read_bytes.load(Ordering::Relaxed);
                        if read != report.read {
                            report.read = read;
                            report.emit("COPYING", "SOURCE_READ_PROGRESS");
                        }
                        continue;
                    }
                    Err(_) => {
                        return Err(Error::new(
                            "WORKER_FAILED",
                            "worker ended without all results",
                        ))
                    }
                }
            }
        })();
        stop.store(true, Ordering::Relaxed);
        drop(task_tx);
        drop(result_rx);
        let mut panicked = false;
        for worker in workers {
            if worker.join().is_err() {
                panicked = true;
            }
        }
        if panicked {
            return Err(Error::new(
                "WORKER_PANICKED",
                "worker panic; archive is incomplete",
            ));
        }
        if outcome.as_ref().err().map(|e| e.code) == Some("CANCELLED") {
            writer.checkpoint()?;
            report.durable = writer.durable;
            report.emit("CANCELLED", "COPY_CANCELLED");
        }
        outcome
    })
}
