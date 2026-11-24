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
        atomic::{AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
const MAX_RECORD: usize = 32 * 1024;
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
        let path = root.to_path_buf();
        std::thread::spawn(move || {
            crate::supervisor::observer_thread();
            let outcome = write_loop(&path, capacity, receiver, &drop_count, &fail);
            if let Err(e) = outcome {
                *fail.lock().unwrap_or_else(|e| e.into_inner()) = Some(e.to_string());
            }
        });
        Ok(Arc::new(Self {
            status: Mutex::new(Status::new(id)),
            start: Instant::now(),
            probe_interval_ms: AtomicU64::new(250),
            last_progress: AtomicU64::new(0),
            dropped,
            sender,
            failure,
            emergency: Mutex::new(std::collections::VecDeque::new()),
        }))
    }
    pub fn ms(&self) -> u64 {
        self.start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
    }
    pub fn phase(&self, phase: &str, detail: &str) {
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
            // Healthy synchronization retains all records. A stuck collector
            // has a deadline and a separate recent in-memory failure tail.
            if bytes.len() > MAX_RECORD || self.enqueue(Message::Record(bytes)).is_err() {
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
                        let error = Error::new(
                            "DIAGNOSTIC_LOG_STALLED",
                            "log queue did not drain within 10 seconds",
                        );
                        *self.failure.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(error.to_string());
                        crate::supervisor::failure(&error);
                        return Err(error);
                    }
                    message = value;
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }
    fn acknowledgement(&self, rx: mpsc::Receiver<Result<()>>) -> Result<()> {
        rx.recv_timeout(Duration::from_secs(30)).map_err(|_| {
            Error::new(
                "DIAGNOSTIC_LOG_STALLED",
                "log operation acknowledgement deadline exceeded",
            )
        })??;
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
    /// A zero interval syncs every record; a positive interval batches durability.
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
}
impl Persistent {
    fn open(
        target: &Path,
        files: &std::collections::VecDeque<PathBuf>,
        interval_ms: u32,
        cap: u64,
        dropped: u64,
    ) -> Result<Self> {
        fs::create_dir(target)?;
        let mut log = Self {
            root: target.into(),
            current: None,
            interval: Duration::from_millis(u64::from(interval_ms)),
            last_sync: Instant::now(),
            dirty: false,
        };
        for path in files {
            let src = io::open_regular(path, false)?;
            let mut dst = io::create_new(&target.join(path.file_name().unwrap()))?;
            io::copy_file(&src, &mut dst)?;
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
        self.sync()?;
        self.current = Some(io::create_new(&self.root.join(name))?);
        io::sync_dir(&self.root)
    }
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let file = self
            .current
            .as_mut()
            .ok_or_else(|| Error::new("DIAGNOSTIC_LOG_FAILED", "missing persistent segment"))?;
        io::write_all(file, bytes)?;
        self.dirty = true;
        self.sync_if_due()
    }
    fn sync(&mut self) -> Result<()> {
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
        {
            let mut s = self.status.lock().unwrap_or_else(|e| e.into_inner());
            match &event {
                JobEvent::State { phase } => s.phase = phase.clone(),
                JobEvent::PlanSelected {
                    plan,
                    verify_workers,
                    eta_seconds,
                    total_bytes,
                    ..
                } => {
                    s.selected_plan = Some(plan.clone());
                    s.verify_workers = *verify_workers;
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
        if matches!(event, JobEvent::ReadAhead { .. }) {
            return;
        }
        self.record(
            "job_event",
            serde_json::to_value(event).unwrap_or(Value::Null),
        );
    }
}
fn mirror(
    persistent: &mut Option<Persistent>,
    failure: &Mutex<Option<String>>,
    action: impl FnOnce(&mut Persistent) -> Result<()>,
) {
    if let Some(log) = persistent.as_mut() {
        if let Err(error) = action(log) {
            *failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(error.to_string());
            crate::supervisor::failure(&error);
            // Keep collecting to RAM after the NTFS mirror has failed.
            persistent.take();
        }
    }
}
fn write_loop(
    root: &Path,
    cap: u64,
    receiver: mpsc::Receiver<Message>,
    dropped: &AtomicU64,
    failure: &Mutex<Option<String>>,
) -> Result<()> {
    let per_file = cap / 4;
    let mut sequence = 0u64;
    let mut used = 0u64;
    let mut files = std::collections::VecDeque::new();
    let mut current: Option<File> = None;
    let mut persistent: Option<Persistent> = None;
    loop {
        let message = match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(message) => message,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                mirror(&mut persistent, failure, Persistent::sync_if_due);
                continue;
            }
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
                        let old: PathBuf = files.pop_front().unwrap();
                        fs::remove_file(old)?;
                    }
                    let path = root.join(format!("events-{sequence:06}.jsonl"));
                    sequence += 1;
                    current = Some(io::create_new(&path)?);
                    mirror(&mut persistent, failure, |log| {
                        log.rotate(path.file_name().unwrap())
                    });
                    files.push_back(path);
                    used = 0;
                }
                io::write_all(current.as_mut().unwrap(), &bytes)?;
                used += bytes.len() as u64;
                mirror(&mut persistent, failure, |log| log.write(&bytes));
            }
            Message::Persist(target, interval_ms, ack) => {
                let result = (|| {
                    require(
                        persistent.is_none(),
                        "PERSISTENT_LOG_ALREADY_OPEN",
                        "one NTFS log per session",
                    )?;
                    if let Some(file) = &current {
                        io::sync_file(file)?;
                    }
                    persistent = Some(Persistent::open(
                        &target,
                        &files,
                        interval_ms,
                        cap,
                        dropped.load(Ordering::Relaxed),
                    )?);
                    Ok(())
                })();
                let _ = ack.send(result);
            }
            Message::Flush(ack) => {
                let result = (|| {
                    if let Some(file) = &current {
                        io::sync_file(file)?;
                    }
                    if let Some(log) = &mut persistent {
                        log.sync()?;
                    }
                    Ok(())
                })();
                let _ = ack.send(result);
            }
            Message::ClosePersistent(status, ack) => {
                let result = match persistent.take() {
                    Some(log) => log.close(cap, dropped.load(Ordering::Relaxed), Some(&status)),
                    None => Ok(()),
                };
                let _ = ack.send(result);
            }
            Message::Export(target, ack) => {
                let result = (|| -> Result<()> {
                    if let Some(f) = &current {
                        io::sync_file(f)?;
                    }
                    fs::create_dir(&target)?;
                    for path in &files {
                        let src = io::open_regular(path, false)?;
                        let mut dst = io::create_new(&target.join(path.file_name().unwrap()))?;
                        io::copy_file(&src, &mut dst)?;
                        io::sync_file(&dst)?;
                    }
                    io::atomic_write(
                        &target,
                        "log-info.json",
                        &io::json(
                            &json!({"schema_version":1,"cap_bytes":cap.to_string(),"dropped_records":dropped.load(Ordering::Relaxed)}),
                        )?,
                    )?;
                    io::sync_dir(&target)?;
                    io::sync_dir(target.parent().unwrap())?;
                    Ok(())
                })();
                let _ = ack.send(result);
            }
            Message::Stop => {
                if let Some(f) = current {
                    io::sync_file(&f)?;
                }
                if let Some(log) = persistent {
                    log.close(cap, dropped.load(Ordering::Relaxed), None)?;
                }
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
