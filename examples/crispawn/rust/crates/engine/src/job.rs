// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! GUI-independent controller. Only typed in-process results advance completion.
use crate::{
    archive, backup,
    control::{ExecutionControl, RunControl},
    io,
    job_config::{parent, JobConfig, TuningMode},
    probe::{self, ProbeReport},
    restore, verify, Error, NoHooks, Result,
};
use crispawn_core::{
    error::require,
    plan::Plan,
    types::{EventSink, Snapshot},
    workflow::{Action, Backend, Phase, Stage, Startup, Workflow},
};
use serde::Serialize;
use std::{
    io::{Seek, SeekFrom},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Backup,
    Resume,
    Restore,
    Verify,
}
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JobEvent {
    State {
        phase: String,
    },
    ProbeTrial {
        index: usize,
        total: usize,
        candidate_id: String,
        elapsed_seconds: Option<f64>,
    },
    PlanSelected {
        plan: Plan,
        verify_workers: u32,
        total_bytes: String,
        eta_seconds: Option<u64>,
        selection: String,
    },
    Progress {
        snapshot: Snapshot,
        percent: u32,
        eta_seconds: Option<u64>,
        phase: String,
    },
    ReadAhead {
        backup_id: String,
        read_raw_bytes: String,
        queued_encoded_bytes: String,
        queue_budget_bytes: String,
    },
    StorageProgress {
        backup_id: String,
        status: crispawn_core::live::StorageStatus,
    },
    Finished {
        success: bool,
        code: String,
        report_file: PathBuf,
        dataset_committed: bool,
    },
    ControlRejected {
        code: String,
        detail: String,
    },
}
pub trait JobEvents: Send + Sync {
    fn emit(&self, event: JobEvent);
}
pub struct NoJobEvents;
impl JobEvents for NoJobEvents {
    fn emit(&self, _: JobEvent) {}
}
#[derive(Debug, Serialize)]
pub struct JobReport {
    pub schema_version: u32,
    pub job_id: String,
    pub operation: Operation,
    pub status: String,
    pub report_file: PathBuf,
    pub selected_plan: Option<Plan>,
    pub verify_workers: u32,
    pub probe: Option<ProbeReport>,
    pub engine: Option<verify::Report>,
    pub error_code: Option<String>,
    pub error_detail: Option<String>,
    pub elapsed_seconds: f64,
    pub guest_poweroff_requested: bool,
    pub io_retries: u64,
}
struct State {
    flow: Workflow,
    operation: Option<Operation>,
    active: bool,
    phase: String,
    fault: Option<Error>,
    initial_eta: Option<u64>,
    verify_estimate: f64,
    work_phase: Option<String>,
    stage_started: f64,
    stage_baseline: u64,
}
pub struct Controller {
    state: Mutex<State>,
    pub control: RunControl,
    start: Instant,
    events: Arc<dyn JobEvents>,
}
impl Controller {
    pub fn new(events: Arc<dyn JobEvents>) -> Result<Self> {
        Ok(Self {
            state: Mutex::new(State {
                flow: Workflow::new(Startup::InteractiveMenu, 0)?,
                operation: None,
                active: false,
                phase: "MENU".into(),
                fault: None,
                initial_eta: None,
                verify_estimate: 0.0,
                work_phase: None,
                stage_started: 0.0,
                stage_baseline: 0,
            }),
            control: RunControl::new(),
            start: Instant::now(),
            events,
        })
    }
    fn ms(&self) -> u64 {
        self.start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
    }
    pub fn request(&self, action: Action) -> Result<()> {
        let phase = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            require(s.active, "CONTROL_UNAVAILABLE", "job is not running")?;
            require(
                matches!(action, Action::Pause | Action::Resume | Action::Cancel),
                "CONTROL_UNAVAILABLE",
                "job control",
            )?;
            if matches!(s.operation, Some(Operation::Backup | Operation::Resume)) {
                s.flow.action(action, self.ms())?;
            } else {
                let allowed = match action {
                    Action::Pause => matches!(
                        s.phase.as_str(),
                        "VERIFYING" | "RESTORING" | "VERIFYING_RESTORE"
                    ),
                    Action::Resume => s.phase == "PAUSED",
                    Action::Cancel => true,
                    _ => false,
                };
                require(
                    allowed,
                    "CONTROL_UNAVAILABLE",
                    "control unavailable in this phase",
                )?;
            }
            match action {
                Action::Pause => {
                    self.control.pause();
                    s.phase = "PAUSE_REQUESTED".into();
                }
                Action::Resume => {
                    self.control.resume();
                    s.phase = "RESUME_REQUESTED".into();
                }
                Action::Cancel => {
                    self.control.cancel();
                    s.phase = "CANCEL_REQUESTED".into();
                }
                _ => unreachable!(),
            }
            s.phase.clone()
        };
        crate::supervisor::phase(&phase);
        if phase == "CANCEL_REQUESTED" {
            crate::supervisor::cancel_requested();
        }
        self.events.emit(JobEvent::State { phase });
        Ok(())
    }
    /// Cancellation also works while preflight is just starting.
    pub fn cancel(&self) {
        self.control.cancel();
        let _ = self.request(Action::Cancel);
    }
    fn state_event(&self, phase: &str) {
        crate::supervisor::phase(phase);
        self.state.lock().unwrap_or_else(|e| e.into_inner()).phase = phase.into();
        self.events.emit(JobEvent::State {
            phase: phase.into(),
        });
    }
    pub fn run(&self, config: &JobConfig, operation: Operation) -> Result<JobReport> {
        config.validate()?;
        crate::supervisor::configure(&config.tuning.io_policy);
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            require(
                s.operation.is_none(),
                "JOB_ALREADY_STARTED",
                "a controller executes one job",
            )?;
            s.operation = Some(operation);
        }
        let job_id = crispawn_core::types::hex(&io::new_id()?);
        let report_path = config.report_file.clone().unwrap_or_else(|| {
            let name = config
                .archive
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            parent(&config.archive).join(format!("{name}.job-{job_id}.json"))
        });
        // Reserve with O_EXCL/O_NOFOLLOW. A repeated invocation preserves old reports.
        let mut report_file = io::create_new(&report_path)?;
        io::sync_dir(parent(&report_path))?;
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.active = true;
            if matches!(operation, Operation::Backup | Operation::Resume) {
                s.flow.action(Action::StartBackup, self.ms())?;
            } else if operation == Operation::Restore {
                s.flow.action(Action::SelectRestore, self.ms())?;
            }
        }
        let mut report = JobReport {
            schema_version: 1,
            job_id,
            operation,
            status: "running".into(),
            report_file: report_path.clone(),
            selected_plan: None,
            verify_workers: 1,
            probe: None,
            engine: None,
            error_code: None,
            error_detail: None,
            elapsed_seconds: 0.0,
            guest_poweroff_requested: false,
            io_retries: 0,
        };
        // Reserve a small report before the bulk data. Later ENOSPC must not
        // prevent a failure report from replacing its own allocated blocks.
        let initial = io::json(&report)?;
        io::write_all(&mut report_file, &initial)?;
        io::write_all(
            &mut report_file,
            &vec![b' '; 65536usize.saturating_sub(initial.len())],
        )?;
        io::sync_file(&report_file)?;
        let retry_start = io::retry_count();
        self.state_event("PREPARING");
        let result = self.execute(config, operation, &mut report);
        self.state.lock().unwrap_or_else(|e| e.into_inner()).active = false;
        if let Err(e) = &result {
            crate::supervisor::failure(e);
            report.status = if e.code == "CANCELLED" {
                "cancelled"
            } else {
                "failed"
            }
            .into();
            report.error_code = Some(e.code.into());
            report.error_detail = Some(e.detail.clone());
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if e.code == "CANCELLED" && s.flow.can_cancel() {
                s.flow.action(Action::Cancel, self.ms())?;
            }
            if e.code == "CANCELLED" && s.flow.phase() == Phase::CancelRequested {
                s.flow.backend(Backend::Cancelled, self.ms())?;
            } else {
                s.flow.backend(
                    Backend::Failed {
                        code: e.code.into(),
                    },
                    self.ms(),
                )?;
            }
        } else {
            report.status = "complete".into();
        }
        report.elapsed_seconds = self.start.elapsed().as_secs_f64();
        report.io_retries = io::retry_count().saturating_sub(retry_start);
        let persisted = (|| -> Result<()> {
            require(
                io::same_file(&report_file, &io::open_regular(&report_path, false)?)?,
                "REPORT_CHANGED",
                "reserved report path changed",
            )?;
            let bytes = io::json(&report)?;
            report_file.seek(SeekFrom::Start(0))?;
            io::write_all(&mut report_file, &bytes)?;
            report_file.set_len(bytes.len() as u64)?;
            io::sync_file(&report_file)?;
            io::sync_dir(parent(&report_path))
        })();
        self.events.emit(JobEvent::Finished {
            success: result.is_ok() && persisted.is_ok(),
            code: report.error_code.clone().unwrap_or_else(|| {
                if persisted.is_ok() {
                    "COMPLETE"
                } else {
                    "REPORT_WRITE_FAILED"
                }
                .into()
            }),
            report_file: report_path,
            dataset_committed: report
                .engine
                .as_ref()
                .map(|r| r.dataset_committed)
                .unwrap_or(false),
        });
        if let Err(error) = &persisted {
            crate::supervisor::failure(error);
        }
        // Preserve the primary backup failure if the error report also fails.
        result?;
        persisted?;
        Ok(report)
    }
    fn execute(
        &self,
        config: &JobConfig,
        operation: Operation,
        report: &mut JobReport,
    ) -> Result<()> {
        self.control.check_cancel()?;
        let mut current_config = config.clone();
        current_config.tuning.plan.format_version = 2;
        let config = &current_config;
        if matches!(operation, Operation::Backup | Operation::Resume) {
            let path = config.source()?;
            let source = if config.source_device.is_some() {
                crate::device::open(path, false, false)?
            } else {
                io::open_regular(path, false)?
            };
            io::lock_file(&source, false)?;
            let identity = io::identity(&source)?;
            require(identity.bytes > 0, "EMPTY_SOURCE", "source is empty")?;
            io::source_preflight(&source, identity.bytes)?;
            io::unchanged(&source, &identity)?;
            if operation == Operation::Backup {
                require(
                    !config.archive.try_exists()?,
                    "ARCHIVE_EXISTS",
                    "use resume explicitly for an incomplete archive",
                )?;
                crate::destination_space::check(parent(&config.archive))?;
                io::destination_preflight(parent(&config.archive))?;
            }
            {
                let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                s.flow.backend(
                    Backend::SourceReady {
                        total_bytes: identity.bytes,
                    },
                    self.ms(),
                )?;
            }
            self.state_event("PROBING");
            let mut selected_verify = config.tuning.verify_workers.unwrap_or(1);
            let (plan, selection, eta, verify_estimate) = if operation == Operation::Resume {
                let root = io::canonical_archive(&config.archive)?;
                let _lock = io::Lock::acquire(&root, false)?;
                let (meta, plan, _) = if crate::v2::is_v2(&root)? {
                    crate::v2::load(&root)?
                } else {
                    archive::load(&root)?
                };
                io::unchanged(&source, &meta.source)?;
                if config.tuning.verify_workers.is_none() {
                    selected_verify = std::thread::available_parallelism()
                        .map(|n| (n.get() as u32).div_ceil(2).clamp(1, 16))
                        .unwrap_or(1);
                    let available =
                        crate::profiles::available_memory().min(plan.memory_limit_bytes);
                    while selected_verify > 1
                        && crate::profiles::memory_bound(&plan, selected_verify) > available
                    {
                        selected_verify = selected_verify.div_ceil(2);
                    }
                }
                (plan, "recorded plan (resume)", None, 0.0)
            } else if config.tuning.mode == TuningMode::Fixed {
                config.tuning.plan.validate(identity.bytes)?;
                (
                    config.tuning.plan.clone(),
                    "configured fixed plan",
                    None,
                    0.0,
                )
            } else if config.tuning.mode == TuningMode::Auto {
                let cpus = std::thread::available_parallelism()
                    .map(|v| v.get() as u32)
                    .unwrap_or(1);
                let (mut plan, verify) = crate::profiles::select(
                    &config.tuning.plan,
                    config.tuning.profile,
                    cpus,
                    crate::profiles::available_memory(),
                    identity.bytes,
                )?;
                selected_verify = config.tuning.verify_workers.unwrap_or(verify);
                crate::online::reserve(&mut plan, cpus, selected_verify)?;
                require(
                    crate::profiles::memory_bound(&plan, selected_verify)
                        <= plan.memory_limit_bytes,
                    "MEMORY_BUDGET",
                    "configured verification workers exceed starting RAM budget",
                )?;
                (
                    plan,
                    "measured starting profile; no discarded trial copy",
                    None,
                    0.0,
                )
            } else {
                let probe = probe::run(
                    config,
                    &source,
                    &identity,
                    &self.control,
                    &|index, total, candidate, trial| {
                        self.events.emit(JobEvent::ProbeTrial {
                            index: index + 1,
                            total,
                            candidate_id: candidate.id.clone(),
                            elapsed_seconds: trial.map(|t| t.elapsed_seconds),
                        });
                    },
                )?;
                let eta = probe.eta(identity.bytes);
                let verify_seconds = probe.verify_estimate(identity.bytes);
                let plan = probe.selected_plan.clone();
                report.probe = Some(probe);
                (plan, "sampled pipeline measurements", eta, verify_seconds)
            };
            self.control.check_cancel()?;
            io::unchanged(&source, &identity)?;
            require(
                crate::profiles::memory_bound(&plan, selected_verify)
                    <= plan
                        .memory_limit_bytes
                        .min(crate::profiles::available_memory()),
                "MEMORY_BUDGET",
                "copy plus concurrent verification exceeds current available RAM",
            )?;
            io::descriptor_preflight(&plan, identity.bytes, selected_verify)?;
            {
                let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                s.flow.backend(
                    Backend::PlanSelected {
                        plan: plan.clone(),
                        eta,
                    },
                    self.ms(),
                )?;
                s.initial_eta = eta.map(|e| e.seconds);
                s.verify_estimate = verify_estimate;
            }
            report.selected_plan = Some(plan.clone());
            require(
                config.tuning.verify_after_write || plan.segmented_storage,
                "INVALID_CONFIG",
                "skip verification is supported only for segmented copies",
            )?;
            self.control
                .set_backup_verification(config.tuning.verify_after_write);
            report.verify_workers = if config.tuning.verify_after_write {
                selected_verify
            } else {
                0
            };
            self.control.set_verification_workers(selected_verify);
            self.events.emit(JobEvent::PlanSelected {
                plan: plan.clone(),
                verify_workers: selected_verify,
                total_bytes: identity.bytes.to_string(),
                eta_seconds: eta.map(|e| e.seconds),
                selection: selection.into(),
            });
            let result = if operation == Operation::Backup {
                if config.source_device.is_some() {
                    backup::backup_device_checked(
                        path,
                        &config.archive,
                        plan,
                        &config.scratch_dir,
                        self,
                        &NoHooks,
                        &self.control,
                        &identity,
                    )?
                } else {
                    backup::backup_checked(
                        path,
                        &config.archive,
                        plan,
                        &config.scratch_dir,
                        self,
                        &NoHooks,
                        &self.control,
                        Some(&identity),
                    )?
                }
            } else {
                if config.source_device.is_some() {
                    backup::resume_device(
                        path,
                        &config.archive,
                        &config.scratch_dir,
                        self,
                        &NoHooks,
                        &self.control,
                        &identity,
                    )?
                } else {
                    backup::resume(
                        path,
                        &config.archive,
                        &config.scratch_dir,
                        self,
                        &NoHooks,
                        &self.control,
                    )?
                }
            };
            require(
                result.dataset_committed,
                "UNCOMMITTED_RESULT",
                "backup did not commit",
            )?;
            report.engine = Some(result);
            self.finish_backup(identity.bytes)?;
        } else {
            if operation == Operation::Restore {
                let root = io::canonical_archive(&config.archive)?;
                let (meta, plan, _) = if crate::v2::is_v2(&root)? {
                    crate::v2::load(&root)?
                } else {
                    archive::load(&root)?
                };
                let cpus = std::thread::available_parallelism()
                    .map(|v| v.get() as u32)
                    .unwrap_or(1);
                let available = crate::profiles::available_memory();
                let mut verifiers = config
                    .tuning
                    .verify_workers
                    .unwrap_or(cpus.div_ceil(2).clamp(1, 16));
                let bound =
                    |v: u32| (192u64 << 20) + u64::from(v) * ((128 << 20) + 2 * plan.range_bytes);
                while verifiers > 1 && bound(verifiers) > available {
                    verifiers -= 1;
                }
                require(
                    bound(verifiers) <= available,
                    "MEMORY_BUDGET",
                    "restore plus concurrent readback exceeds available RAM",
                )?;
                self.control.set_verification_workers(verifiers);
                report.verify_workers = verifiers;
                report.selected_plan = Some(plan.clone());
                let mut display_plan = plan;
                display_plan.workers = 1; // Current archive decode/writer is serial; checking is separate.
                self.events.emit(JobEvent::PlanSelected {
                    plan: display_plan,
                    verify_workers: verifiers,
                    total_bytes: meta.source.bytes.to_string(),
                    eta_seconds: None,
                    selection: "recorded archive codecs; concurrent target readback".into(),
                });
            }
            report.engine = Some(if operation == Operation::Restore {
                if let Some(target) = &config.restore_target_device {
                    let file = crate::device::open(target, false, false)?;
                    let expected = io::identity(&file)?;
                    drop(file);
                    restore::restore_device_checked(
                        &config.archive,
                        target,
                        &config.scratch_dir,
                        self,
                        &NoHooks,
                        &self.control,
                        &expected,
                    )?
                } else {
                    let target = config.restore_target_file.as_deref().ok_or_else(|| {
                        Error::new("INVALID_CONFIG", "restore_target_file is required")
                    })?;
                    restore::restore_controlled(
                        &config.archive,
                        target,
                        false,
                        &config.scratch_dir,
                        self,
                        &NoHooks,
                        &self.control,
                    )?
                }
            } else {
                verify::verify_controlled(
                    &config.archive,
                    &config.scratch_dir,
                    self,
                    &self.control,
                )?
            });
        }
        if let Some(e) = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .fault
            .take()
        {
            return Err(e);
        }
        Ok(())
    }
    fn finish_backup(&self, total: u64) -> Result<()> {
        // Completion wins a cancel that arrives after COMMITTED. No destructive rollback.
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = s.fault.take() {
            return Err(e);
        }
        // The engine's last snapshot is FINALIZING; copy/verify facts are typed here.
        if matches!(s.flow.phase(), Phase::Working(Stage::Copy)) {
            s.flow.backend(
                Backend::CopyProgress {
                    read_bytes: total,
                    durable_bytes: total,
                    eta: None,
                },
                self.ms(),
            )?;
            s.flow.backend(Backend::CopyFinished, self.ms())?;
        }
        // A late pause request may race the last engine gate. Work is now quiescent.
        if matches!(s.flow.phase(), Phase::PauseRequested(_)) {
            s.flow.backend(Backend::Paused, self.ms())?;
            drop(s);
            self.state_event("PAUSED");
            self.control.wait_for_resume()?;
            s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(s.flow.phase(), Phase::ResumeRequested(_)) {
                s.flow.backend(Backend::Resumed, self.ms())?;
            }
        }
        if s.flow.phase() == Phase::Working(Stage::Verify) {
            if !self.control.verify_after_write() {
                s.flow.backend(Backend::VerifySkipped, self.ms())?;
            } else {
                s.flow.backend(
                    Backend::VerifyProgress {
                        verified_bytes: total,
                        eta: None,
                    },
                    self.ms(),
                )?;
                s.flow.backend(Backend::VerifyFinished, self.ms())?;
            }
        }
        require(
            s.flow.phase() == Phase::Finalizing,
            "INVALID_TRANSITION",
            "verified backup completion phase",
        )?;
        s.flow.backend(Backend::DatasetCommitted, self.ms())?;
        // Regular files have no mounts owned by this job; all engine handles are closed.
        s.flow.backend(Backend::CleanupFinished, self.ms())?;
        Ok(())
    }
    fn snapshot(&self, snapshot: Snapshot) -> Result<JobEvent> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let backup = matches!(s.operation, Some(Operation::Backup | Operation::Resume));
        if backup {
            if snapshot.phase == "COPYING" && s.flow.phase() == Phase::StartingCopy {
                s.flow.backend(Backend::CopyStarted, self.ms())?;
            }
            if snapshot.code == "RESUME_ACK" && matches!(s.flow.phase(), Phase::ResumeRequested(_))
            {
                s.flow.backend(Backend::Resumed, self.ms())?;
            }
            if matches!(
                s.flow.phase(),
                Phase::Working(Stage::Copy)
                    | Phase::PauseRequested(Stage::Copy)
                    | Phase::CancelRequested
            ) && matches!(snapshot.phase.as_str(), "COPYING" | "VERIFYING" | "PAUSED")
            {
                s.flow.backend(
                    Backend::CopyProgress {
                        read_bytes: snapshot.read_raw_bytes,
                        durable_bytes: snapshot.durable_raw_bytes,
                        eta: None,
                    },
                    self.ms(),
                )?;
            }
            if snapshot.phase == "VERIFYING"
                && matches!(
                    s.flow.phase(),
                    Phase::Working(Stage::Copy) | Phase::PauseRequested(Stage::Copy)
                )
            {
                s.flow.backend(Backend::CopyFinished, self.ms())?;
            }
            if snapshot.phase == "VERIFYING"
                && matches!(
                    s.flow.phase(),
                    Phase::Working(Stage::Verify) | Phase::PauseRequested(Stage::Verify)
                )
            {
                s.flow.backend(
                    Backend::VerifyProgress {
                        verified_bytes: snapshot.verified_raw_bytes,
                        eta: None,
                    },
                    self.ms(),
                )?;
                if snapshot.code == "ALL_RANGES_VERIFIED" {
                    s.flow.backend(Backend::VerifyFinished, self.ms())?;
                }
            }
            if snapshot.code == "PAUSE_ACK" && matches!(s.flow.phase(), Phase::PauseRequested(_)) {
                s.flow.backend(Backend::Paused, self.ms())?;
            }
        }
        let phase = if snapshot.phase == "COMPLETE" {
            "FINALIZING".to_owned()
        } else if snapshot.phase != "PAUSED" && self.control.pause_requested() {
            "PAUSE_REQUESTED".to_owned()
        } else {
            snapshot.phase.clone()
        };
        s.phase = phase.clone();
        let work_phase = snapshot.phase.as_str();
        let verifying = work_phase.starts_with("VERIFYING");
        let restoring = work_phase == "RESTORING";
        if matches!(
            work_phase,
            "COPYING" | "VERIFYING" | "RESTORING" | "VERIFYING_RESTORE"
        ) && s.work_phase.as_deref() != Some(work_phase)
        {
            s.work_phase = Some(work_phase.into());
            s.stage_started = snapshot.elapsed_seconds;
            s.stage_baseline = if work_phase == "COPYING" {
                snapshot.durable_raw_bytes
            } else {
                0
            };
        }
        let done = if verifying {
            snapshot
                .verification_read_raw_bytes
                .max(snapshot.verified_raw_bytes)
        } else if restoring {
            snapshot.read_raw_bytes
        } else {
            snapshot.durable_raw_bytes
        };
        let elapsed = (snapshot.elapsed_seconds - s.stage_started).max(0.0);
        let eta = if matches!(
            phase.as_str(),
            "PAUSED" | "PAUSE_REQUESTED" | "CANCELLED" | "FINALIZING"
        ) {
            None
        } else {
            probe::remaining(
                elapsed,
                snapshot.total_bytes,
                done,
                s.stage_baseline,
                if verifying || restoring {
                    0.0
                } else {
                    s.verify_estimate
                },
            )
            .map(|e| e.seconds)
            .or_else(|| {
                if verifying {
                    probe::estimate(s.verify_estimate)
                        .map(|e| e.seconds)
                        .filter(|s| *s > 0)
                } else {
                    s.initial_eta
                }
            })
        };
        let percent = if snapshot.total_bytes == 0 {
            0
        } else {
            (u128::from(done) * 100 / u128::from(snapshot.total_bytes)).min(100) as u32
        };
        Ok(JobEvent::Progress {
            snapshot,
            percent,
            eta_seconds: eta,
            phase,
        })
    }
}
impl EventSink for Controller {
    fn storage_progress(&self, backup_id: &str, status: &crispawn_core::live::StorageStatus) {
        self.events.emit(JobEvent::StorageProgress {
            backup_id: backup_id.into(),
            status: status.clone(),
        });
    }
    fn prefetch(&self, backup_id: &str, read: u64, queued: u64, budget: u64) {
        self.events.emit(JobEvent::ReadAhead {
            backup_id: backup_id.into(),
            read_raw_bytes: read.to_string(),
            queued_encoded_bytes: queued.to_string(),
            queue_budget_bytes: budget.to_string(),
        });
    }
    fn settings(&self, plan: &Plan, verify_workers: u32, total: u64, reason: &str) {
        self.events.emit(JobEvent::PlanSelected {
            plan: plan.clone(),
            verify_workers,
            total_bytes: total.to_string(),
            eta_seconds: None,
            selection: reason.into(),
        });
    }
    fn emit(&self, snapshot: Snapshot) {
        match self.snapshot(snapshot) {
            Ok(event) => self.events.emit(event),
            Err(error) => {
                self.state.lock().unwrap_or_else(|e| e.into_inner()).fault = Some(error);
                self.control.cancel();
            }
        }
    }
}
