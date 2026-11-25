// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! Bounded RAM log and optional continuous NTFS mirror. Producers never do disk I/O.
use crate::{
    io,
    job::{JobEvent, JobEvents},
    Error, Result,
};
use crispawn_core::{error::require, live::Status};
use serde_json::{json, Value};
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
const MAX_RECORD: usize = 32 * 1024;
#[path = "diagnostic_mirror.rs"]
mod destination_mirror;
enum Message {
    Record(Vec<u8>),
    Export(PathBuf, mpsc::Sender<Result<()>>),
    Persist(PathBuf, u32, mpsc::Sender<Result<()>>),
    Flush(mpsc::Sender<Result<()>>),
    ClosePersistent(Status, mpsc::Sender<Result<()>>),
    Stop,
}
pub struct Diagnostics {
    pub status: Mutex<Status>,
    pub start: Instant,
    pub probe_interval_ms: AtomicU64,
    last_progress: AtomicU64,
    dropped: Arc<AtomicU64>,
    sender: mpsc::SyncSender<Message>,
    failure: Arc<Mutex<Option<String>>>,
    emergency: Mutex<std::collections::VecDeque<Vec<u8>>>,
    defer_persistent_sync: Arc<AtomicBool>,
}
impl Diagnostics {
    pub fn new(root: &Path, capacity: u64, id: String) -> Result<Arc<Self>> {
        require(
            (64 * 1024..=32 * 1024 * 1024).contains(&capacity),
            "INVALID_CONFIG",
            "RAM log cap 64 KiB..32 MiB",
        )?;
        fs::create_dir(root)?;
        let (sender, receiver) = mpsc::sync_channel(128);
        let failure = Arc::new(Mutex::new(None));
        let dropped = Arc::new(AtomicU64::new(0));
        let fail = failure.clone();
        let drop_count = dropped.clone();
        let defer_persistent_sync = Arc::new(AtomicBool::new(false));
        let deferred = defer_persistent_sync.clone();
        let path = root.to_path_buf();
        std::thread::Builder::new()
            .name("cp-log-ram".into())
            .spawn(move || {
                crate::supervisor::observer_thread();
                let outcome = write_loop(&path, capacity, receiver, &drop_count, &fail, &deferred);
                if let Err(e) = outcome {
                    *fail.lock().unwrap_or_else(|e| e.into_inner()) = Some(e.to_string());
                }
            })?;
        Ok(Arc::new(Self {
            status: Mutex::new(Status::new(id)),
            start: Instant::now(),
            probe_interval_ms: AtomicU64::new(250),
            last_progress: AtomicU64::new(0),
            dropped,
            sender,
            failure,
            emergency: Mutex::new(std::collections::VecDeque::new()),
            defer_persistent_sync,
        }))
    }
    pub fn ms(&self) -> u64 {
        self.start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
    }
    pub fn phase(&self, phase: &str, detail: &str) {
        if phase == "COPYING" {
            self.defer_persistent_sync.store(true, Ordering::Release);
        }
        crate::supervisor::phase(phase);
        {
            let mut s = self.status.lock().unwrap_or_else(|e| e.into_inner());
            s.phase = phase.into();
            s.detail = detail.into();
            s.countdown_seconds = None;
            s.can_pause = false;
            s.can_resume = false;
            s.can_cancel = false;
        }
        self.record("phase", json!({"phase":phase,"detail":detail}));
    }
    pub fn record(&self, kind: &str, value: Value) {
        let record = json!({"schema_version":1,"monotonic_ms":self.ms(),
            "unix_ms":SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis().to_string(),
            "kind":kind,"value":value});
        if let Ok(mut bytes) = serde_json::to_vec(&record) {
            bytes.push(b'\n');
            {
                let mut emergency = self.emergency.lock().unwrap_or_else(|e| e.into_inner());
                emergency.push_back(bytes.clone());
                while emergency.iter().map(Vec::len).sum::<usize>() > 65536 {
                    emergency.pop_front();
                }
            }
            // Logging backpressure is not evidence that disk work has failed.
            // The bounded emergency tail is updated even if the RAM queue is busy.
            if bytes.len() > MAX_RECORD || self.enqueue_record(bytes).is_err() {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    pub fn emergency_tail(&self) -> Vec<u8> {
        self.emergency
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .flatten()
            .copied()
            .collect()
    }
    fn enqueue_record(&self, bytes: Vec<u8>) -> Result<()> {
        let until = Instant::now() + Duration::from_millis(50);
        let mut message = Message::Record(bytes);
        loop {
            match self.sender.try_send(message) {
                Ok(()) => return Ok(()),
                Err(mpsc::TrySendError::Full(value)) if Instant::now() < until => {
                    message = value;
                    std::thread::yield_now();
                }
                _ => {
                    return Err(Error::new(
                        "DIAGNOSTIC_RAM_BUSY",
                        "bounded RAM log collector is busy",
                    ))
                }
            }
        }
    }
    fn enqueue(&self, mut message: Message) -> Result<()> {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            match self.sender.try_send(message) {
                Ok(()) => return Ok(()),
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    return Err(Error::new("DIAGNOSTIC_LOG_FAILED", "log collector ended"))
                }
                Err(mpsc::TrySendError::Full(value)) => {
                    if Instant::now() >= until {
                        return Err(Error::new(
                            "DIAGNOSTIC_LOG_CONTROL_BUSY",
                            "RAM log control queue did not drain within 10 seconds",
                        ));
                    }
                    message = value;
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }
    fn acknowledgement(&self, rx: mpsc::Receiver<Result<()>>) -> Result<()> {
        let started = Instant::now();
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(result) => {
                    result?;
                    break;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(Error::new(
                        "DIAGNOSTIC_LOG_FAILED",
                        "log operation ended without acknowledgement",
                    ))
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.check()?;
                    // Live/CLI supervision already watches actual completed
                    // destination I/O. A fixed log timeout must not preempt it.
                    if !crate::supervisor::managed() && started.elapsed() >= Duration::from_secs(30)
                    {
                        return Err(Error::new(
                            "DIAGNOSTIC_LOG_STALLED",
                            "unsupervised log operation deadline exceeded",
                        ));
                    }
                }
            }
        }
        // A preceding queued record may have failed its mirror after the
        // caller's initial check, even though the subsequent RAM flush worked.
        self.check()
    }
    pub fn snapshot(&self) -> Status {
        let mut s = self.status.lock().unwrap_or_else(|e| e.into_inner());
        s.monotonic_ms = self.ms();
        s.progress_age_ms = s
            .monotonic_ms
            .saturating_sub(self.last_progress.load(Ordering::Relaxed));
        s.log_dropped = self.dropped.load(Ordering::Relaxed);
        // One shared ETA covers every selected disk and concurrent checking.
        // Reporter time excludes acknowledged pauses; no view estimates disk work.
        if s.session.total_bytes > 0 && matches!(s.phase.as_str(), "COPYING" | "RESTORING") {
            if let Some(p) = &s.snapshot {
                let total = 2 * u128::from(s.session.total_bytes);
                let done = u128::from(s.session.previous_durable_bytes)
                    + u128::from(s.session.previous_verified_bytes)
                    + u128::from(p.durable_raw_bytes)
                    + u128::from(p.verification_read_raw_bytes.max(p.verified_raw_bytes));
                let elapsed = s.session.previous_elapsed_seconds + p.elapsed_seconds;
                if done > 0 && elapsed.is_finite() && elapsed > 0.0 {
                    s.eta_seconds = Some(
                        ((total.saturating_sub(done) as f64 * elapsed / done as f64)
                            .ceil()
                            .min(u64::MAX as f64) as u64)
                            .max(1),
                    );
                }
            }
        }
        if self
            .failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
        {
            s.logs_persisted = false;
        }
        s.clone()
    }
    pub fn check(&self) -> Result<()> {
        if let Some(e) = &*self.failure.lock().unwrap_or_else(|e| e.into_inner()) {
            return Err(Error::new("DIAGNOSTIC_LOG_FAILED", e.clone()));
        }
        Ok(())
    }
    /// Backfill startup RAM segments and mirror all subsequent records automatically.
    /// Outside productive copying, zero syncs every record and positive intervals
    /// batch durability. Copy-time records are appended without per-record sync.
    pub fn begin_persistent(&self, target: &Path, sync_interval_ms: u32) -> Result<()> {
        require(
            sync_interval_ms <= 60_000,
            "INVALID_CONFIG",
            "log sync interval <= 60000 ms",
        )?;
        self.check()?;
        let (tx, rx) = mpsc::channel();
        self.enqueue(Message::Persist(target.to_path_buf(), sync_interval_ms, tx))?;
        self.acknowledgement(rx)?;
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .logs_persisted = true;
        Ok(())
    }
    pub fn flush(&self) -> Result<()> {
        self.check()?;
        let (tx, rx) = mpsc::channel();
        self.enqueue(Message::Flush(tx))?;
        self.acknowledgement(rx)
    }
    /// Drain and close all destination handles before NTFS unmount.
    pub fn close_persistent(&self) -> Result<()> {
        self.check()?;
        let (tx, rx) = mpsc::channel();
        self.enqueue(Message::ClosePersistent(self.snapshot(), tx))?;
        self.acknowledgement(rx)
    }
    /// Cleanup remains possible after a mirror failure, while its acknowledgement
    /// still has a deadline. Never turn this cleanup into a successful backup.
    pub fn close_after_failure(&self) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.enqueue(Message::ClosePersistent(self.snapshot(), tx))?;
        rx.recv_timeout(Duration::from_secs(30))
            .map_err(|_| Error::new("DIAGNOSTIC_LOG_STALLED", "failure close deadline exceeded"))?
    }
    pub fn export(&self, target: &Path) -> Result<()> {
        self.check()?;
        let (tx, rx) = mpsc::channel();
        self.enqueue(Message::Export(target.to_path_buf(), tx))?;
        self.acknowledgement(rx)?;
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .logs_persisted = true;
        Ok(())
    }
    pub fn stop(&self) {
        let _ = self.enqueue(Message::Stop);
    }
}
struct Persistent {
    root: PathBuf,
    current: Option<File>,
    interval: Duration,
    last_sync: Instant,
    dirty: bool,
    deferred: Arc<AtomicBool>,
    filesystem: File,
}
impl Persistent {
    fn open(
        target: &Path,
        files: &[(std::ffi::OsString, Vec<u8>)],
        interval_ms: u32,
        cap: u64,
        dropped: u64,
        deferred: Arc<AtomicBool>,
    ) -> Result<Self> {
        fs::create_dir(target)?;
        let mut log = Self {
            root: target.into(),
            current: None,
            interval: Duration::from_millis(u64::from(interval_ms)),
            last_sync: Instant::now(),
            dirty: false,
            deferred,
            filesystem: File::open(target)?,
        };
        for (name, bytes) in files {
            let mut dst = io::create_new(&target.join(name))?;
            io::write_all(&mut dst, bytes)?;
            io::sync_file(&dst)?;
            log.current = Some(dst);
        }
        log.info(cap, dropped, false)?;
        io::sync_dir(target.parent().unwrap())?;
        Ok(log)
    }
    fn info(&self, cap: u64, dropped: u64, closed: bool) -> Result<()> {
        io::atomic_write(
            &self.root,
            "log-info.json",
            &io::json(&json!({
                "schema_version":1,"mode":"continuous","cap_bytes":cap.to_string(),
                "ram_cap_bytes":cap.to_string(),"ntfs_history_bounded":false,
                "sync_interval_ms":self.interval.as_millis() as u64,
                "closed":closed,"dropped_records":dropped
            }))?,
        )
    }
    fn rotate(&mut self, name: &std::ffi::OsStr) -> Result<()> {
        let _deferred = io::DeferredSync::new(self.deferred.load(Ordering::Acquire));
        self.sync()?;
        self.current = Some(io::create_new(&self.root.join(name))?);
        io::sync_dir(&self.root)
    }
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let _trace = crate::io_trace::span(crate::io_trace::Op::LogWrite, bytes.len() as u64);
        let file = self
            .current
            .as_mut()
            .ok_or_else(|| Error::new("DIAGNOSTIC_LOG_FAILED", "missing persistent segment"))?;
        io::write_all(file, bytes)?;
        self.dirty = true;
        self.sync_if_due()
    }
    fn sync(&mut self) -> Result<()> {
        if self.deferred.load(Ordering::Acquire) {
            return Ok(());
        }
        if let Some(file) = &self.current {
            io::sync_file(file)?;
        }
        self.dirty = false;
        self.last_sync = Instant::now();
        Ok(())
    }
    fn sync_if_due(&mut self) -> Result<()> {
        if self.dirty && self.last_sync.elapsed() >= self.interval {
            self.sync()?;
        }
        Ok(())
    }
    fn close(mut self, cap: u64, dropped: u64, status: Option<&Status>) -> Result<()> {
        if self.deferred.swap(false, Ordering::AcqRel) {
            // The ordered Close message follows all queued copy records. Flush
            // every rotated segment, using an fd retained since opening the log.
            io::sync_filesystem(&self.filesystem)?;
        }
        self.sync()?;
        self.current.take();
        if let Some(status) = status {
            io::atomic_write(&self.root, "probe-final.json", &io::json(status)?)?;
        }
        self.info(cap, dropped, true)?;
        io::sync_dir(&self.root)
    }
}
impl JobEvents for Diagnostics {
    fn emit(&self, event: JobEvent) {
        // Keep appending logs to NTFS during copying, without submitting another
        // device-wide flush for every progress record. Final close still syncs.
        match &event {
            JobEvent::Progress { phase, .. } if phase == "COPYING" => {
                self.defer_persistent_sync.store(true, Ordering::Release)
            }
            _ => {}
        }
        let mut persist_storage = false;
        {
            let mut s = self.status.lock().unwrap_or_else(|e| e.into_inner());
            match &event {
                JobEvent::StorageProgress { backup_id, status } => {
                    if s.snapshot
                        .as_ref()
                        .is_some_and(|v| v.backup_id == *backup_id)
                    {
                        let memory = s.memory.get_or_insert_with(Default::default);
                        memory.encoded_queue_bytes = status.prefetch_encoded_bytes;
                        memory.encoded_queue_limit_bytes = status.prefetch_limit_bytes;
                        persist_storage = s
                            .storage
                            .as_ref()
                            .map(|old| {
                                old.written_blocks != status.written_blocks
                                    || old.confirmed_blocks != status.confirmed_blocks
                                    || old.calibrated != status.calibrated
                                    || (old.phase == status.phase
                                        && old.phase_elapsed_ms / 5000
                                            != status.phase_elapsed_ms / 5000)
                            })
                            .unwrap_or(true);
                        if s.storage
                            .as_ref()
                            .map(|old| (old.written_bytes, old.confirmed_bytes))
                            != Some((status.written_bytes, status.confirmed_bytes))
                        {
                            self.last_progress.store(self.ms(), Ordering::Relaxed);
                        }
                        s.storage = Some(status.clone());
                    }
                }
                JobEvent::State { phase } => s.phase = phase.clone(),
                JobEvent::PlanSelected {
                    plan,
                    verify_workers,
                    eta_seconds,
                    total_bytes,
                    ..
                } => {
                    s.selected_plan = Some(plan.clone());
                    s.verify_workers = if !s.backup_verification_enabled
                        && s.session.mode == crispawn_core::presentation::Mode::Backup
                    {
                        0
                    } else {
                        *verify_workers
                    };
                    if s.snapshot.is_none() {
                        s.eta_seconds = *eta_seconds;
                    }
                    s.source_bytes = total_bytes.parse().unwrap_or(0);
                    s.detail.clear();
                }
                JobEvent::Progress {
                    snapshot,
                    percent,
                    eta_seconds,
                    phase,
                } => {
                    if s.snapshot.as_ref().map(|old| {
                        (
                            old.read_raw_bytes,
                            old.durable_raw_bytes,
                            old.verified_raw_bytes,
                            old.verification_read_raw_bytes,
                        )
                    }) != Some((
                        snapshot.read_raw_bytes,
                        snapshot.durable_raw_bytes,
                        snapshot.verified_raw_bytes,
                        snapshot.verification_read_raw_bytes,
                    )) {
                        self.last_progress.store(self.ms(), Ordering::Relaxed);
                    }
                    if s.snapshot
                        .as_ref()
                        .is_some_and(|old| old.backup_id != snapshot.backup_id)
                    {
                        s.storage = None;
                    }
                    let mut current = snapshot.clone();
                    if let Some(previous) = s
                        .snapshot
                        .as_ref()
                        .filter(|old| old.backup_id == current.backup_id)
                    {
                        current.read_raw_bytes =
                            current.read_raw_bytes.max(previous.read_raw_bytes);
                    }
                    s.snapshot = Some(current);
                    s.percent = *percent;
                    s.eta_seconds = *eta_seconds;
                    s.phase = phase.clone();
                    s.detail.clear();
                }
                JobEvent::ReadAhead {
                    backup_id,
                    read_raw_bytes,
                    ..
                } => {
                    if let Some(snapshot) = s
                        .snapshot
                        .as_mut()
                        .filter(|old| old.backup_id == *backup_id)
                    {
                        if let Ok(read) = read_raw_bytes.parse::<u64>() {
                            if read <= snapshot.total_bytes && read > snapshot.read_raw_bytes {
                                snapshot.read_raw_bytes = read;
                                self.last_progress.store(self.ms(), Ordering::Relaxed);
                            }
                        }
                    }
                }
                JobEvent::ProbeTrial {
                    candidate_id,
                    index,
                    total,
                    ..
                } => {
                    s.phase = "PROBING".into();
                    s.detail = format!("{index}/{total} {candidate_id}");
                }
                JobEvent::Finished { success, code, .. } => {
                    s.phase = if *success { "JOB_COMPLETE" } else { "FAILED" }.into();
                    s.detail = code.clone();
                }
                JobEvent::ControlRejected { code, detail } => {
                    s.detail = format!("{code}: {detail}")
                }
            }
            if s.phase != "MENU" {
                s.countdown_seconds = None;
            }
            s.can_pause = matches!(
                s.phase.as_str(),
                "COPYING" | "VERIFYING" | "RESTORING" | "VERIFYING_RESTORE"
            );
            s.can_resume = s.phase == "PAUSED";
            s.can_cancel = matches!(
                s.phase.as_str(),
                "PREPARING"
                    | "PROBING"
                    | "COPYING"
                    | "VERIFYING"
                    | "PAUSE_REQUESTED"
                    | "PAUSED"
                    | "RESUME_REQUESTED"
                    | "RESTORING"
                    | "VERIFYING_RESTORE"
            );
        }
        // Read-ahead is live RAM telemetry, not a new durable checkpoint. Do
        // not add one USB fsync per sample while diagnosing a slow writer.
        if matches!(event, JobEvent::ReadAhead { .. })
            || (matches!(event, JobEvent::StorageProgress { .. }) && !persist_storage)
        {
            return;
        }
        self.record(
            "job_event",
            serde_json::to_value(event).unwrap_or(Value::Null),
        );
    }
}
fn ram_snapshot(
    files: &std::collections::VecDeque<PathBuf>,
) -> Result<Vec<(std::ffi::OsString, Vec<u8>)>> {
    files
        .iter()
        .map(|path| {
            let file = io::open_regular(path, false)?;
            let length = file.metadata()?.len();
            require(length <= 8 << 20, "LIMIT_EXCEEDED", "RAM log segment")?;
            let mut bytes = vec![0; length as usize];
            io::read_exact_at(&file, &mut bytes, 0, 65536)?;
            Ok((path.file_name().unwrap().to_owned(), bytes))
        })
        .collect()
}
fn export_snapshot(
    target: &Path,
    files: Vec<(std::ffi::OsString, Vec<u8>)>,
    cap: u64,
    dropped: u64,
) -> Result<()> {
    fs::create_dir(target)?;
    for (name, bytes) in files {
        let mut dst = io::create_new(&target.join(name))?;
        io::write_all(&mut dst, &bytes)?;
        io::sync_file(&dst)?;
    }
    io::atomic_write(
        target,
        "log-info.json",
        &io::json(&json!({
            "schema_version":1,"cap_bytes":cap.to_string(),"dropped_records":dropped
        }))?,
    )?;
    io::sync_dir(target)?;
    io::sync_dir(target.parent().unwrap())
}
fn write_loop(
    root: &Path,
    cap: u64,
    receiver: mpsc::Receiver<Message>,
    dropped: &Arc<AtomicU64>,
    failure: &Arc<Mutex<Option<String>>>,
    deferred: &Arc<AtomicBool>,
) -> Result<()> {
    let per_file = cap / 4;
    let mut sequence = 0u64;
    let mut used = 0u64;
    let mut files = std::collections::VecDeque::new();
    let mut current: Option<File> = None;
    let mut persistent: Option<destination_mirror::Mirror> = None;
    loop {
        let message = match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(message) => message,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => Message::Stop,
        };
        match message {
            Message::Record(bytes) => {
                if bytes.len() as u64 > per_file {
                    dropped.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                if current.is_none() || used + bytes.len() as u64 > per_file {
                    if let Some(f) = current.take() {
                        io::sync_file(&f)?;
                    }
                    if files.len() == 4 {
                        fs::remove_file(files.pop_front().unwrap())?;
                    }
                    let path = root.join(format!("events-{sequence:06}.jsonl"));
                    sequence += 1;
                    current = Some(io::create_new(&path)?);
                    files.push_back(path);
                    used = 0;
                }
                io::write_all(current.as_mut().unwrap(), &bytes)?;
                used += bytes.len() as u64;
                if let Some(mirror) = &persistent {
                    mirror.record(files.back().unwrap().file_name().unwrap().to_owned(), bytes);
                }
            }
            Message::Persist(target, interval_ms, ack) => {
                if persistent
                    .as_ref()
                    .is_some_and(|mirror| mirror.open_failed())
                {
                    persistent.take();
                }
                let result = (|| {
                    require(
                        persistent.is_none(),
                        "PERSISTENT_LOG_ALREADY_OPEN",
                        "one NTFS log per session",
                    )?;
                    let initial = ram_snapshot(&files)?;
                    persistent = Some(destination_mirror::Mirror::start(
                        target,
                        initial,
                        interval_ms,
                        cap,
                        dropped.clone(),
                        failure.clone(),
                        deferred.clone(),
                        ack.clone(),
                    )?);
                    Ok(())
                })();
                if let Err(error) = result {
                    let _ = ack.send(Err(error));
                }
            }
            Message::Flush(ack) => {
                let result = (|| {
                    if let Some(file) = &current {
                        io::sync_file(file)?;
                    }
                    if let Some(mirror) = &persistent {
                        mirror.flush(ack.clone())?;
                    } else {
                        let _ = ack.send(Ok(()));
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    let _ = ack.send(Err(error));
                }
            }
            Message::ClosePersistent(status, ack) => {
                if let Some(mirror) = persistent.take() {
                    if let Err(error) = mirror.close(status, ack.clone()) {
                        let _ = ack.send(Err(error));
                    }
                } else {
                    let _ = ack.send(Ok(()));
                }
            }
            Message::Export(target, ack) => {
                let result = (|| {
                    let files = ram_snapshot(&files)?;
                    let lost = dropped.load(Ordering::Relaxed);
                    let reply = ack.clone();
                    std::thread::Builder::new()
                        .name("cp-log-export".into())
                        .spawn(move || {
                            crate::supervisor::observer_thread();
                            let _ = reply.send(export_snapshot(&target, files, cap, lost));
                        })?;
                    Ok(())
                })();
                if let Err(error) = result {
                    let _ = ack.send(Err(error));
                }
            }
            Message::Stop => {
                if let Some(f) = current {
                    io::sync_file(&f)?;
                }
                drop(persistent);
                return Ok(());
            }
        }
    }
}
/// Snapshot a bounded tail for tty2; no log contents are ever interpreted as commands.
pub fn tail(root: &Path, limit: usize) -> Result<String> {
    let mut files = fs::read_dir(root)?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    files.retain(|path| {
        path.file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("events-"))
    });
    files.sort();
    let Some(path) = files.last() else {
        return Ok(String::new());
    };
    let f = io::open_regular(path, false)?;
    let length = f.metadata()?.len();
    let n = length.min(limit as u64) as usize;
    let mut bytes = vec![0; n];
    io::read_exact_at(&f, &mut bytes, length - n as u64, 65536)?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(text
        .lines()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n"))
}
