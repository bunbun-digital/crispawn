// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! Cooperative controls. A request is not an acknowledgement of quiescence.
use crate::{Error, Result};
use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    Condvar, Mutex,
};
use std::time::Duration;

pub trait ExecutionControl: Send + Sync {
    fn cancelled(&self) -> bool;
    fn verification_workers(&self) -> u32 {
        1
    }
    fn configure_verification_workers(&self, _workers: u32) {}
    fn pause_requested(&self) -> bool {
        false
    }
    fn wait_for_resume(&self) -> Result<()> {
        self.check_cancel()
    }
    fn check_cancel(&self) -> Result<()> {
        if self.cancelled() {
            Err(Error::new("CANCELLED", "operation cancelled"))
        } else {
            Ok(())
        }
    }
}
impl ExecutionControl for AtomicBool {
    fn cancelled(&self) -> bool {
        self.load(Ordering::Relaxed)
    }
}
/// Mark active I/O and park at a pause boundary without claiming it is running.
pub(crate) struct Activity<'a> {
    counter: &'a AtomicU64,
    counted: bool,
}
impl<'a> Activity<'a> {
    pub fn new(counter: &'a AtomicU64) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self {
            counter,
            counted: true,
        }
    }
    pub fn wait(&mut self, control: &dyn ExecutionControl, stop: &AtomicBool) -> Result<bool> {
        let mut waited = false;
        loop {
            control.check_cancel()?;
            if stop.load(Ordering::Acquire) {
                return Err(Error::new("CANCELLED", "activity stopped"));
            }
            if !self.counted {
                self.counter.fetch_add(1, Ordering::AcqRel);
                self.counted = true;
            }
            if !control.pause_requested() {
                return Ok(waited);
            }
            self.counter.fetch_sub(1, Ordering::AcqRel);
            self.counted = false;
            waited = true;
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}
impl Drop for Activity<'_> {
    fn drop(&mut self) {
        if self.counted {
            self.counter.fetch_sub(1, Ordering::AcqRel);
        }
    }
}
pub struct RunControl {
    cancel: AtomicBool,
    pause: Mutex<bool>,
    wake: Condvar,
    verification_workers: AtomicU32,
}
impl Default for RunControl {
    fn default() -> Self {
        Self::new()
    }
}
impl RunControl {
    pub const fn new() -> Self {
        Self {
            cancel: AtomicBool::new(false),
            pause: Mutex::new(false),
            wake: Condvar::new(),
            verification_workers: AtomicU32::new(1),
        }
    }
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.wake.notify_all();
    }
    pub fn set_verification_workers(&self, workers: u32) {
        self.verification_workers
            .store(workers.clamp(1, 32), Ordering::Relaxed);
    }
    pub fn pause(&self) {
        *self.pause.lock().unwrap_or_else(|e| e.into_inner()) = true;
    }
    pub fn resume(&self) {
        *self.pause.lock().unwrap_or_else(|e| e.into_inner()) = false;
        self.wake.notify_all();
    }
}
impl ExecutionControl for RunControl {
    fn configure_verification_workers(&self, workers: u32) {
        self.set_verification_workers(workers);
    }
    fn verification_workers(&self) -> u32 {
        self.verification_workers.load(Ordering::Relaxed)
    }
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
    fn pause_requested(&self) -> bool {
        *self.pause.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn wait_for_resume(&self) -> Result<()> {
        let mut paused = self.pause.lock().unwrap_or_else(|e| e.into_inner());
        while *paused && !self.cancelled() {
            paused = self
                .wake
                .wait_timeout(paused, Duration::from_millis(100))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        self.check_cancel()
    }
}
