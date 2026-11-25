// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-digital. Licensed under Apache-2.0.
//! Bounded, asynchronous destination mirror. A slow destination never owns the
//! RAM collector or producer queue. Retain recent records on overflow and report
//! the gap explicitly; only actual I/O errors fail the copy.
use super::{Persistent, MAX_RECORD};
use crate::{Error, Result};
use crispawn_core::live::Status;
use std::{
    collections::VecDeque,
    ffi::OsString,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Condvar, Mutex,
    },
    time::Duration,
};

type Ack = mpsc::Sender<Result<()>>;
enum Message {
    Record(OsString, Vec<u8>),
    Flush(Ack),
    Close(Status, Ack),
}
struct State {
    messages: VecDeque<Message>,
    bytes: usize,
    records: usize,
    controls: usize,
    gap: u64,
    stop: bool,
    open_failed: bool,
}
struct Queue {
    state: Mutex<State>,
    changed: Condvar,
    capacity: usize,
    dropped: Arc<AtomicU64>,
}
pub(super) struct Mirror(Arc<Queue>);
impl Mirror {
    pub(super) fn start(
        target: PathBuf,
        initial: Vec<(OsString, Vec<u8>)>,
        interval_ms: u32,
        cap: u64,
        dropped: Arc<AtomicU64>,
        failure: Arc<Mutex<Option<String>>>,
        deferred: Arc<AtomicBool>,
        ready: Ack,
    ) -> Result<Self> {
        let queue = Arc::new(Queue {
            state: Mutex::new(State {
                messages: VecDeque::new(),
                bytes: 0,
                records: 0,
                controls: 0,
                gap: 0,
                stop: false,
                open_failed: false,
            }),
            changed: Condvar::new(),
            capacity: cap as usize,
            dropped,
        });
        let worker = queue.clone();
        std::thread::Builder::new()
            .name("cp-log-ntfs".into())
            .spawn(move || {
                crate::supervisor::observer_thread();
                let mut filename = initial.last().map(|(name, _)| name.clone());
                let mut persistent = match Persistent::open(
                    &target,
                    &initial,
                    interval_ms,
                    cap,
                    worker.dropped.load(Ordering::Relaxed),
                    deferred,
                ) {
                    Ok(log) => {
                        let _ = ready.send(Ok(()));
                        Some(log)
                    }
                    Err(error) => {
                        let mut state = worker.state.lock().unwrap_or_else(|e| e.into_inner());
                        state.open_failed = true;
                        state.stop = true;
                        drop(state);
                        let _ = ready.send(Err(error));
                        None
                    }
                };
                loop {
                    let (message, gap, stopped) = {
                        let mut state = worker.state.lock().unwrap_or_else(|e| e.into_inner());
                        if state.messages.is_empty() && !state.stop {
                            state = worker
                                .changed
                                .wait_timeout(state, Duration::from_millis(100))
                                .unwrap_or_else(|e| e.into_inner())
                                .0;
                        }
                        let message = state.messages.pop_front();
                        if let Some(Message::Record(_, bytes)) = &message {
                            state.bytes -= bytes.capacity() + 64;
                            state.records -= 1;
                        } else if message.is_some() {
                            state.controls -= 1;
                        }
                        (message, std::mem::take(&mut state.gap), state.stop)
                    };
                    let result: Result<bool> = (|| {
                        if let Some(Message::Record(name, _)) = &message {
                            if filename.as_ref() != Some(name) {
                                if let Some(log) = persistent.as_mut() {
                                    log.rotate(name)?;
                                }
                                filename = Some(name.clone());
                            }
                        }
                        if let Some(log) = persistent.as_mut() {
                            if gap > 0 {
                                let mut record = serde_json::to_vec(&serde_json::json!({
                                "kind":"log_gap","schema_version":1,
                                "value":{"omitted_records":gap,"reason":"destination_log_backlog"}
                            })).map_err(|e| Error::new("JSON_ERROR", e.to_string()))?;
                                record.push(b'\n');
                                log.write(&record)?;
                            }
                        }
                        match message {
                            Some(Message::Record(name, bytes)) => {
                                if let Some(log) = persistent.as_mut() {
                                    if filename.as_ref() != Some(&name) {
                                        log.rotate(&name)?;
                                        filename = Some(name);
                                    }
                                    log.write(&bytes)?;
                                }
                            }
                            Some(Message::Flush(ack)) => {
                                let result = match persistent.as_mut() {
                                    Some(log) => log.sync(),
                                    None => Err(Error::new(
                                        "DIAGNOSTIC_LOG_FAILED",
                                        "destination log mirror failed",
                                    )),
                                };
                                let _ = ack.send(result);
                            }
                            Some(Message::Close(status, ack)) => {
                                let result = match persistent.take() {
                                    Some(log) => log.close(
                                        cap,
                                        worker.dropped.load(Ordering::Relaxed),
                                        Some(&status),
                                    ),
                                    None => Err(Error::new(
                                        "DIAGNOSTIC_LOG_FAILED",
                                        "destination log mirror failed",
                                    )),
                                };
                                let _ = ack.send(result);
                                return Ok(true);
                            }
                            None if stopped => {
                                if let Some(log) = persistent.take() {
                                    log.close(cap, worker.dropped.load(Ordering::Relaxed), None)?;
                                }
                                return Ok(true);
                            }
                            None => {
                                if let Some(log) = persistent.as_mut() {
                                    log.sync_if_due()?;
                                }
                            }
                        }
                        Ok(false)
                    })();
                    match result {
                        Ok(true) => break,
                        Ok(false) => {}
                        Err(error) => {
                            *failure.lock().unwrap_or_else(|e| e.into_inner()) =
                                Some(error.to_string());
                            crate::supervisor::failure(&error);
                            persistent.take();
                        }
                    }
                }
            })?;
        Ok(Self(queue))
    }
    pub(super) fn record(&self, filename: OsString, bytes: Vec<u8>) {
        let charge = bytes.capacity() + 64;
        let mut s = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.stop {
            return;
        }
        while s.bytes + charge > self.0.capacity || s.records >= 8192 {
            let Some(index) = s
                .messages
                .iter()
                .position(|m| matches!(m, Message::Record(..)))
            else {
                break;
            };
            if let Some(Message::Record(_, old)) = s.messages.remove(index) {
                s.bytes -= old.capacity() + 64;
                s.records -= 1;
                s.gap += 1;
                self.0.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        if bytes.len() <= MAX_RECORD && charge <= self.0.capacity {
            s.bytes += charge;
            s.records += 1;
            s.messages.push_back(Message::Record(filename, bytes));
        } else {
            s.gap += 1;
            self.0.dropped.fetch_add(1, Ordering::Relaxed);
        }
        self.0.changed.notify_one();
    }
    pub(super) fn open_failed(&self) -> bool {
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .open_failed
    }
    fn control(&self, message: Message) -> Result<()> {
        let mut s = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.stop || s.controls >= 32 {
            return Err(Error::new(
                "DIAGNOSTIC_LOG_CONTROL_BUSY",
                "destination log controls are full or closed",
            ));
        }
        s.controls += 1;
        s.messages.push_back(message);
        self.0.changed.notify_one();
        Ok(())
    }
    pub(super) fn flush(&self, ack: Ack) -> Result<()> {
        self.control(Message::Flush(ack))
    }
    pub(super) fn close(&self, status: Status, ack: Ack) -> Result<()> {
        self.control(Message::Close(status, ack))
    }
}
impl Drop for Mirror {
    fn drop(&mut self) {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner()).stop = true;
        self.0.changed.notify_one();
    }
}
