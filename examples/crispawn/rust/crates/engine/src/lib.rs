// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! Headless engine. The initial adapter accepts regular-file fixtures only.
pub mod adaptive;
pub mod archive;
pub mod backup;
pub mod codec;
pub mod connection;
pub mod console;
pub mod control;
mod destination_space;
pub mod device;
pub mod diagnostics;
mod failure_persistence;
mod inline_verify;
pub mod io;
pub mod job;
pub mod job_config;
pub mod journal;
pub mod live;
pub mod live_config;
pub mod network_config;
mod ntfs_diagnostics;
pub mod online;
pub mod parallel_hash;
pub mod presentation;
pub mod probe;
pub mod profiles;
pub mod restore;
mod storage;
mod storage_worker;
pub mod supervisor;
pub mod terminal;
mod uefi_diagnostics;
pub mod v2;
mod v2_backup;
pub mod v2_format;
mod v2_journal;
mod v2_restore;
pub mod verify;
pub use crispawn_core::{Error, Result};

/// Trusted, in-process test seam. Never reads commands or failpoints from archives.
pub trait Hooks: Send + Sync {
    fn at(&self, _point: &'static str) -> Result<()> {
        Ok(())
    }
    fn span(&self, point: &'static str, _offset: u64, _length: u64) -> Result<()> {
        self.at(point)
    }
}
pub struct NoHooks;
impl Hooks for NoHooks {}

pub(crate) struct Reporter<'a> {
    events: &'a dyn crispawn_core::types::EventSink,
    id: String,
    seq: u64,
    start: std::time::Instant,
    pub total: u64,
    pub read: u64,
    pub durable: u64,
    pub verified: u64,
    pub verification_read: u64,
    pub stored: u64,
    pub verification_kind: &'static str,
    paused: std::time::Duration,
}
impl<'a> Reporter<'a> {
    pub fn new(events: &'a dyn crispawn_core::types::EventSink, id: String, total: u64) -> Self {
        Self {
            events,
            id,
            seq: 0,
            start: std::time::Instant::now(),
            total,
            read: 0,
            durable: 0,
            verified: 0,
            verification_read: 0,
            stored: 0,
            verification_kind: "buffered_reopen",
            paused: std::time::Duration::ZERO,
        }
    }
    pub fn emit(&mut self, phase: &str, code: &str) {
        supervisor::phase(phase);
        self.events.emit(crispawn_core::types::Snapshot {
            schema_version: 1,
            backup_id: self.id.clone(),
            seq: self.seq,
            phase: phase.into(),
            code: code.into(),
            total_bytes: self.total,
            read_raw_bytes: self.read,
            durable_raw_bytes: self.durable,
            verified_raw_bytes: self.verified,
            verification_read_raw_bytes: self.verification_read,
            stored_bytes: self.stored,
            elapsed_seconds: self
                .start
                .elapsed()
                .saturating_sub(self.paused)
                .as_secs_f64(),
            eta_seconds: None,
            verification_kind: self.verification_kind.into(),
        });
        self.seq += 1;
    }
    /// Call only after all copy tasks have drained, or at a serial range boundary.
    pub fn gate(&mut self, control: &dyn control::ExecutionControl, phase: &str) -> Result<()> {
        control.check_cancel()?;
        if control.pause_requested() {
            self.emit("PAUSED", "PAUSE_ACK");
            let started = std::time::Instant::now();
            let result = control.wait_for_resume();
            self.paused += started.elapsed();
            result?;
            self.emit(phase, "RESUME_ACK");
        }
        control.check_cancel()
    }
}
