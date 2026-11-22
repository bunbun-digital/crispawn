// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! Bounded sealed-file verification; the coordinator alone publishes progress.
use crate::{
    control::ExecutionControl,
    parallel_hash::CpuPool,
    v2::{self, FileEntry},
    Error, Hooks, Reporter, Result,
};
use perfect_backup_core::{error::require, plan::Plan, types::Metadata};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::Scope,
    time::Duration,
};

pub(crate) struct Inline {
    tasks: Option<mpsc::SyncSender<FileEntry>>,
    results: mpsc::Receiver<(FileEntry, perfect_backup_core::types::SourceIdentity)>,
    read: Arc<AtomicU64>,
    service_nanos: Arc<AtomicU64>,
    service_bytes: Arc<AtomicU64>,
    failed: Arc<Mutex<Option<Error>>>,
    stop: Arc<AtomicBool>,
    expected: BTreeMap<u64, FileEntry>,
    completed: BTreeMap<u64, (FileEntry, perfect_backup_core::types::SourceIdentity)>,
    phase: &'static str,
}
impl Inline {
    pub fn new<'scope, 'env>(
        scope: &'scope Scope<'scope, 'env>,
        root: &'scope Path,
        meta: &'scope Metadata,
        plan: &'scope Plan,
        pool: &'scope CpuPool,
        control: &'scope dyn ExecutionControl,
        hooks: &'scope dyn Hooks,
    ) -> Self {
        Self::with_target(scope, root, meta, plan, pool, control, hooks, None)
    }
    pub fn with_target<'scope, 'env>(
        scope: &'scope Scope<'scope, 'env>,
        root: &'scope Path,
        meta: &'scope Metadata,
        plan: &'scope Plan,
        pool: &'scope CpuPool,
        control: &'scope dyn ExecutionControl,
        hooks: &'scope dyn Hooks,
        target: Option<&'scope std::fs::File>,
    ) -> Self {
        let workers = control.verification_workers().clamp(1, 32);
        let (tasks, rx) = mpsc::sync_channel::<FileEntry>(workers as usize * 2);
        let rx = Arc::new(Mutex::new(rx));
        let (tx, results) = mpsc::channel();
        let read = Arc::new(AtomicU64::new(0));
        let service_nanos = Arc::new(AtomicU64::new(0));
        let service_bytes = Arc::new(AtomicU64::new(0));
        let failed = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        for index in 0..workers {
            let nanos = service_nanos.clone();
            let bytes = service_bytes.clone();
            let (rx, tx, read, failed, stop) = (
                rx.clone(),
                tx.clone(),
                read.clone(),
                failed.clone(),
                stop.clone(),
            );
            scope.spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    while index >= control.verification_workers().clamp(1, workers)
                        && !stop.load(Ordering::Acquire)
                    {
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    let task = rx.lock().unwrap_or_else(|e| e.into_inner()).recv();
                    let Ok(entry) = task else {
                        break;
                    };
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                        || -> Result<perfect_backup_core::types::SourceIdentity> {
                            control.check_cancel()?;
                            if target.is_none() {
                                hooks.at("verify_read")?;
                            }
                            let path = root.join(&entry.relative_path);
                            let before =
                                crate::io::identity(&crate::io::open_regular(&path, false)?)?;
                            let id = perfect_backup_core::types::unhex::<16>(&meta.backup_id)?;
                            let started = std::time::Instant::now();
                            let mut paused = false;
                            let mut progress = |raw: &[u8]| -> Result<()> {
                                control.check_cancel()?;
                                if stop.load(Ordering::Acquire) {
                                    return Err(Error::new(
                                        "CANCELLED",
                                        "verification stopped after failure",
                                    ));
                                }
                                read.fetch_add(raw.len() as u64, Ordering::Relaxed);
                                while control.pause_requested() {
                                    paused = true;
                                    control.check_cancel()?;
                                    if stop.load(Ordering::Acquire) {
                                        return Err(Error::new(
                                            "CANCELLED",
                                            "verification stopped while paused",
                                        ));
                                    }
                                    std::thread::sleep(Duration::from_millis(25));
                                }
                                Ok(())
                            };
                            if let Some(target) = target {
                                let mut hash = blake3::Hasher::new();
                                let mut offset = 0;
                                while offset < entry.source_length_bytes {
                                    control.check_cancel()?;
                                    let length =
                                        plan.range_bytes.min(entry.source_length_bytes - offset);
                                    hooks.span(
                                        "restore_verify_read",
                                        entry.source_offset_bytes + offset,
                                        length,
                                    )?;
                                    let mut raw = vec![0; length as usize];
                                    crate::io::read_exact_at(
                                        target,
                                        &mut raw,
                                        entry.source_offset_bytes + offset,
                                        plan.read_bytes,
                                    )?;
                                    pool.update(&mut hash, &raw);
                                    progress(&raw)?;
                                    offset += length;
                                }
                                require(
                                    hash.finalize().to_hex().as_str() == entry.raw_blake3,
                                    "RESTORE_HASH_MISMATCH",
                                    "restored contiguous source span BLAKE3 mismatch",
                                )?;
                            } else {
                                v2::scan_file(root, &entry, meta, plan, id, pool, |_, raw| {
                                    progress(raw)
                                })?;
                            }
                            if !paused {
                                nanos.fetch_add(
                                    started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
                                    Ordering::Relaxed,
                                );
                                bytes.fetch_add(entry.stored_bytes, Ordering::Relaxed);
                            }
                            require(
                                crate::io::identity(&crate::io::open_regular(&path, false)?)?
                                    == before,
                                "FILE_CHANGED",
                                "sealed file changed during verification",
                            )?;
                            Ok(before)
                        },
                    ))
                    .unwrap_or_else(|_| {
                        Err(Error::new(
                            "ENGINE_PANICKED",
                            "sealed-file verification worker panicked",
                        ))
                    });
                    match result {
                        Ok(identity) => {
                            if tx.send((entry, identity)).is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            crate::supervisor::failure(&error);
                            let mut failure = failed.lock().unwrap_or_else(|e| e.into_inner());
                            if failure.is_none() {
                                *failure = Some(error);
                            }
                            stop.store(true, Ordering::Release);
                            break;
                        }
                    }
                }
            });
        }
        Self {
            tasks: Some(tasks),
            results,
            read,
            service_nanos,
            service_bytes,
            failed,
            stop,
            expected: BTreeMap::new(),
            completed: BTreeMap::new(),
            phase: if target.is_some() {
                "RESTORING"
            } else {
                "COPYING"
            },
        }
    }
    pub fn service(&self) -> (u64, f64) {
        (
            self.service_bytes.load(Ordering::Relaxed),
            self.service_nanos.load(Ordering::Relaxed) as f64 * 1e-9,
        )
    }
    pub fn poll(&mut self, report: &mut Reporter<'_>) -> Result<()> {
        let before = (report.verified, report.verification_read);
        if let Some(error) = self
            .failed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            return Err(Error::new(error.code, error.detail.clone()));
        }
        while let Ok((entry, identity)) = self.results.try_recv() {
            require(
                self.expected.get(&entry.file_id) == Some(&entry)
                    && !self.completed.contains_key(&entry.file_id),
                "INVALID_STATE",
                "verification result does not match sealed file",
            )?;
            report.verified += entry.source_length_bytes;
            self.completed.insert(entry.file_id, (entry, identity));
        }
        report.verification_read = self.read.load(Ordering::Relaxed);
        if before != (report.verified, report.verification_read) {
            report.emit(self.phase, "CONCURRENT_FILE_VERIFICATION");
        }
        Ok(())
    }
    pub fn submit(
        &mut self,
        mut entry: FileEntry,
        report: &mut Reporter<'_>,
        control: &dyn ExecutionControl,
    ) -> Result<()> {
        require(
            self.expected.insert(entry.file_id, entry.clone()).is_none(),
            "INVALID_STATE",
            "file verification queued twice",
        )?;
        loop {
            control.check_cancel()?;
            self.poll(report)?;
            match self.tasks.as_ref().unwrap().try_send(entry) {
                Ok(()) => return Ok(()),
                Err(mpsc::TrySendError::Full(value)) => {
                    entry = value;
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(_) => {
                    return Err(Error::new(
                        "ENGINE_EXITED",
                        "verification queue disconnected",
                    ))
                }
            }
        }
    }
    pub fn finish(
        &mut self,
        report: &mut Reporter<'_>,
        control: &dyn ExecutionControl,
    ) -> Result<()> {
        self.tasks.take();
        while self.completed.len() != self.expected.len() {
            report.gate(control, self.phase)?;
            self.poll(report)?;
            std::thread::sleep(Duration::from_millis(25));
        }
        self.poll(report)
    }
    pub fn matches(&self, root: &Path, files: &[FileEntry]) -> Result<bool> {
        if files.len() != self.completed.len() {
            return Ok(false);
        }
        for entry in files {
            let Some((actual, identity)) = self.completed.get(&entry.file_id) else {
                return Ok(false);
            };
            if actual != entry {
                return Ok(false);
            }
            require(
                crate::io::identity(&crate::io::open_regular(
                    &root.join(&entry.relative_path),
                    false,
                )?)? == *identity,
                "FILE_CHANGED",
                "sealed file changed after concurrent verification",
            )?;
        }
        Ok(true)
    }
    pub fn close(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.tasks.take();
    }
}
