// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! Shared GUI/CUI lifecycle contract. Effects are requests, never disk or power operations.
use crate::{error::require, plan::Plan, Error, Result};

pub const MENU_COUNTDOWN_MS: u64 = 30_000;
pub const COMPLETION_MESSAGE_MS: u64 = 5_000;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Startup {
    GuiCountdown,
    CuiUnattended,
    InteractiveMenu,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Copy,
    Verify,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Menu,
    Preparing,
    Probing,
    StartingCopy,
    Working(Stage),
    PauseRequested(Stage),
    Paused(Stage),
    ResumeRequested(Stage),
    CancelRequested,
    Finalizing,
    Cleaning,
    CompleteMessage,
    PoweroffRequested,
    Cancelled,
    Failed,
    RestoreEntry,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    PrepareConfiguredBackup,
    RunProbe,
    PresentSelectedPlan,
    StartCopy,
    StartVerify,
    PauseEngine,
    ResumeEngine,
    CancelEngine,
    CommitDataset,
    CleanOwnedMounts,
    RequestGuestPoweroff,
    OpenRestoreWorkflow,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Estimate {
    pub seconds: u64,
    pub lower_seconds: u64,
    pub upper_seconds: u64,
}
impl Estimate {
    fn validate(self) -> Result<()> {
        require(
            self.lower_seconds <= self.seconds && self.seconds <= self.upper_seconds,
            "INVALID_ETA",
            "estimate bounds",
        )
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    MenuInteraction,
    StartBackup,
    SelectRestore,
    Pause,
    Resume,
    Cancel,
}
pub enum Backend {
    SourceReady {
        total_bytes: u64,
    },
    PlanSelected {
        plan: Plan,
        eta: Option<Estimate>,
    },
    CopyStarted,
    CopyProgress {
        read_bytes: u64,
        durable_bytes: u64,
        eta: Option<Estimate>,
    },
    CopyFinished,
    VerifyProgress {
        verified_bytes: u64,
        eta: Option<Estimate>,
    },
    VerifyFinished,
    VerifySkipped,
    Paused,
    Resumed,
    Cancelled,
    DatasetCommitted,
    CleanupFinished,
    Failed {
        code: String,
    },
}
pub struct Workflow {
    startup: Startup,
    phase: Phase,
    now: u64,
    menu_deadline: Option<u64>,
    off_deadline: Option<u64>,
    total: u64,
    read: u64,
    durable: u64,
    verified: u64,
    eta: Option<Estimate>,
    plan: Option<Plan>,
    copy_finished: bool,
    verify_finished: bool,
    committed: bool,
    error: Option<String>,
}
impl Workflow {
    pub fn new(startup: Startup, now_ms: u64) -> Result<Self> {
        let deadline = if startup == Startup::GuiCountdown {
            Some(
                now_ms
                    .checked_add(MENU_COUNTDOWN_MS)
                    .ok_or_else(|| Error::new("CLOCK_OVERFLOW", "menu deadline"))?,
            )
        } else {
            None
        };
        Ok(Self {
            startup,
            phase: Phase::Menu,
            now: now_ms,
            menu_deadline: deadline,
            off_deadline: None,
            total: 0,
            read: 0,
            durable: 0,
            verified: 0,
            eta: None,
            plan: None,
            copy_finished: false,
            verify_finished: false,
            committed: false,
            error: None,
        })
    }
    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn total_bytes(&self) -> u64 {
        self.total
    }
    /// Distinct source bytes read for COPY only; excludes probe/retry/verification reads.
    pub fn read_bytes(&self) -> u64 {
        self.read
    }
    pub fn durable_bytes(&self) -> u64 {
        self.durable
    }
    pub fn verified_bytes(&self) -> u64 {
        self.verified
    }
    pub fn selected_plan(&self) -> Option<&Plan> {
        self.plan.as_ref()
    }
    pub fn eta(&self) -> Option<Estimate> {
        if matches!(self.phase, Phase::StartingCopy | Phase::Working(_)) {
            self.eta
        } else {
            None
        }
    }
    pub fn dataset_committed(&self) -> bool {
        self.committed
    }
    pub fn error_code(&self) -> Option<&str> {
        self.error.as_deref()
    }
    pub fn countdown_seconds(&self) -> Option<u64> {
        if self.phase == Phase::Menu {
            self.menu_deadline
                .map(|d| d.saturating_sub(self.now).div_ceil(1000))
        } else {
            None
        }
    }
    pub fn copy_percent(&self) -> Option<u32> {
        if self.total == 0 {
            None
        } else {
            Some((u128::from(self.durable) * 100 / u128::from(self.total)) as u32)
        }
    }
    pub fn can_pause(&self) -> bool {
        matches!(self.phase, Phase::Working(_))
    }
    pub fn can_resume(&self) -> bool {
        matches!(self.phase, Phase::Paused(_))
    }
    pub fn can_cancel(&self) -> bool {
        matches!(
            self.phase,
            Phase::Preparing
                | Phase::Probing
                | Phase::StartingCopy
                | Phase::Working(_)
                | Phase::PauseRequested(_)
                | Phase::Paused(_)
                | Phase::ResumeRequested(_)
        )
    }
    fn time(&mut self, now_ms: u64) -> Result<()> {
        require(
            now_ms >= self.now,
            "CLOCK_REVERSED",
            "use a monotonic clock",
        )?;
        self.now = now_ms;
        Ok(())
    }
    pub fn tick(&mut self, now_ms: u64) -> Result<Vec<Effect>> {
        self.time(now_ms)?;
        if self.phase == Phase::Menu
            && (self.startup == Startup::CuiUnattended
                || self.menu_deadline.map(|d| now_ms >= d).unwrap_or(false))
        {
            return self.begin_backup();
        }
        if self.phase == Phase::CompleteMessage
            && self.off_deadline.map(|d| now_ms >= d).unwrap_or(false)
        {
            self.phase = Phase::PoweroffRequested;
            return Ok(vec![Effect::RequestGuestPoweroff]);
        }
        Ok(vec![])
    }
    fn begin_backup(&mut self) -> Result<Vec<Effect>> {
        require(
            self.phase == Phase::Menu,
            "INVALID_TRANSITION",
            "backup start requires menu",
        )?;
        self.menu_deadline = None;
        self.phase = Phase::Preparing;
        Ok(vec![Effect::PrepareConfiguredBackup])
    }
    pub fn action(&mut self, action: Action, now_ms: u64) -> Result<Vec<Effect>> {
        self.time(now_ms)?;
        match action {
            Action::MenuInteraction => {
                require(
                    self.phase == Phase::Menu,
                    "INVALID_TRANSITION",
                    "menu input",
                )?;
                self.menu_deadline = None;
                if self.startup == Startup::CuiUnattended {
                    self.startup = Startup::InteractiveMenu;
                }
                Ok(vec![])
            }
            Action::StartBackup => self.begin_backup(),
            Action::SelectRestore => {
                require(
                    self.phase == Phase::Menu,
                    "INVALID_TRANSITION",
                    "restore selection",
                )?;
                self.menu_deadline = None;
                self.phase = Phase::RestoreEntry;
                Ok(vec![Effect::OpenRestoreWorkflow])
            }
            Action::Pause => {
                require(self.can_pause(), "CONTROL_UNAVAILABLE", "pause")?;
                if let Phase::Working(stage) = self.phase {
                    self.phase = Phase::PauseRequested(stage);
                }
                Ok(vec![Effect::PauseEngine])
            }
            Action::Resume => {
                require(self.can_resume(), "CONTROL_UNAVAILABLE", "resume")?;
                if let Phase::Paused(stage) = self.phase {
                    self.phase = Phase::ResumeRequested(stage);
                }
                Ok(vec![Effect::ResumeEngine])
            }
            Action::Cancel => {
                require(self.can_cancel(), "CONTROL_UNAVAILABLE", "cancel")?;
                self.phase = Phase::CancelRequested;
                self.eta = None;
                Ok(vec![Effect::CancelEngine])
            }
        }
    }
    fn after_work(&mut self, stage: Stage) -> Vec<Effect> {
        match stage {
            Stage::Copy => {
                self.phase = Phase::Working(Stage::Verify);
                vec![Effect::StartVerify]
            }
            Stage::Verify => {
                self.phase = Phase::Finalizing;
                self.eta = None;
                vec![Effect::CommitDataset]
            }
        }
    }
    pub fn backend(&mut self, event: Backend, now_ms: u64) -> Result<Vec<Effect>> {
        self.time(now_ms)?;
        match event {
            Backend::SourceReady { total_bytes } => {
                require(
                    self.phase == Phase::Preparing && total_bytes > 0,
                    "INVALID_TRANSITION",
                    "source preflight",
                )?;
                self.total = total_bytes;
                self.phase = Phase::Probing;
                Ok(vec![Effect::RunProbe])
            }
            Backend::PlanSelected { plan, eta } => {
                require(
                    self.phase == Phase::Probing,
                    "INVALID_TRANSITION",
                    "plan selection",
                )?;
                plan.validate(self.total)?;
                if let Some(e) = eta {
                    e.validate()?;
                }
                self.plan = Some(plan);
                self.eta = eta;
                self.phase = Phase::StartingCopy;
                Ok(vec![Effect::PresentSelectedPlan, Effect::StartCopy])
            }
            Backend::CopyStarted => {
                require(
                    self.phase == Phase::StartingCopy,
                    "INVALID_TRANSITION",
                    "copy start acknowledgement",
                )?;
                self.phase = Phase::Working(Stage::Copy);
                Ok(vec![])
            }
            Backend::CopyProgress {
                read_bytes,
                durable_bytes,
                eta,
            } => {
                require(
                    matches!(
                        self.phase,
                        Phase::Working(Stage::Copy)
                            | Phase::PauseRequested(Stage::Copy)
                            | Phase::CancelRequested
                    ),
                    "INVALID_TRANSITION",
                    "copy progress",
                )?;
                require(
                    read_bytes >= self.read
                        && durable_bytes >= self.durable
                        && durable_bytes <= read_bytes
                        && read_bytes <= self.total,
                    "INVALID_PROGRESS",
                    "copy byte counters",
                )?;
                if let Some(e) = eta {
                    e.validate()?;
                }
                self.read = read_bytes;
                self.durable = durable_bytes;
                self.eta = eta;
                Ok(vec![])
            }
            Backend::CopyFinished => {
                require(
                    self.read == self.total && self.durable == self.total && self.total > 0,
                    "INVALID_PROGRESS",
                    "copy must be durable before verification",
                )?;
                require(
                    matches!(
                        self.phase,
                        Phase::Working(Stage::Copy)
                            | Phase::PauseRequested(Stage::Copy)
                            | Phase::Paused(Stage::Copy)
                            | Phase::CancelRequested
                    ),
                    "INVALID_TRANSITION",
                    "copy finish",
                )?;
                self.copy_finished = true;
                if self.phase == Phase::Working(Stage::Copy) {
                    Ok(self.after_work(Stage::Copy))
                } else {
                    Ok(vec![])
                }
            }
            Backend::VerifyProgress {
                verified_bytes,
                eta,
            } => {
                require(
                    matches!(
                        self.phase,
                        Phase::Working(Stage::Verify)
                            | Phase::PauseRequested(Stage::Verify)
                            | Phase::CancelRequested
                    ),
                    "INVALID_TRANSITION",
                    "verify progress",
                )?;
                require(
                    verified_bytes >= self.verified && verified_bytes <= self.total,
                    "INVALID_PROGRESS",
                    "verified byte counters",
                )?;
                if let Some(e) = eta {
                    e.validate()?;
                }
                self.verified = verified_bytes;
                self.eta = eta;
                Ok(vec![])
            }
            Backend::VerifySkipped => {
                require(
                    self.phase == Phase::Working(Stage::Verify)
                        && self.copy_finished
                        && self.verified == 0,
                    "INVALID_TRANSITION",
                    "skip only the completed copy's verification stage",
                )?;
                self.verify_finished = true;
                Ok(self.after_work(Stage::Verify))
            }
            Backend::VerifyFinished => {
                require(
                    self.verified == self.total && self.total > 0,
                    "INVALID_PROGRESS",
                    "all ranges must be verified",
                )?;
                require(
                    matches!(
                        self.phase,
                        Phase::Working(Stage::Verify)
                            | Phase::PauseRequested(Stage::Verify)
                            | Phase::Paused(Stage::Verify)
                            | Phase::CancelRequested
                    ),
                    "INVALID_TRANSITION",
                    "verify finish",
                )?;
                self.verify_finished = true;
                if self.phase == Phase::Working(Stage::Verify) {
                    Ok(self.after_work(Stage::Verify))
                } else {
                    Ok(vec![])
                }
            }
            Backend::Paused => {
                if let Phase::PauseRequested(stage) = self.phase {
                    self.phase = Phase::Paused(stage);
                    Ok(vec![])
                } else {
                    Err(Error::new("INVALID_TRANSITION", "pause acknowledgement"))
                }
            }
            Backend::Resumed => {
                if let Phase::ResumeRequested(stage) = self.phase {
                    self.eta = None;
                    let finished = match stage {
                        Stage::Copy => self.copy_finished,
                        Stage::Verify => self.verify_finished,
                    };
                    if finished {
                        Ok(self.after_work(stage))
                    } else {
                        self.phase = Phase::Working(stage);
                        Ok(vec![])
                    }
                } else {
                    Err(Error::new("INVALID_TRANSITION", "resume acknowledgement"))
                }
            }
            Backend::Cancelled => {
                require(
                    self.phase == Phase::CancelRequested,
                    "INVALID_TRANSITION",
                    "cancel acknowledgement",
                )?;
                self.phase = Phase::Cancelled;
                self.eta = None;
                Ok(vec![])
            }
            Backend::DatasetCommitted => {
                require(
                    self.phase == Phase::Finalizing,
                    "INVALID_TRANSITION",
                    "commit acknowledgement",
                )?;
                self.committed = true;
                self.phase = Phase::Cleaning;
                Ok(vec![Effect::CleanOwnedMounts])
            }
            Backend::CleanupFinished => {
                require(
                    self.phase == Phase::Cleaning && self.committed,
                    "INVALID_TRANSITION",
                    "cleanup acknowledgement",
                )?;
                let deadline = now_ms
                    .checked_add(COMPLETION_MESSAGE_MS)
                    .ok_or_else(|| Error::new("CLOCK_OVERFLOW", "completion message deadline"))?;
                self.phase = Phase::CompleteMessage;
                self.eta = None;
                self.off_deadline = Some(deadline);
                Ok(vec![])
            }
            Backend::Failed { code } => {
                self.phase = Phase::Failed;
                self.error = Some(code);
                self.eta = None;
                self.off_deadline = None;
                Ok(vec![])
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn copying(startup: Startup) -> Workflow {
        let mut f = Workflow::new(startup, 0).unwrap();
        f.action(Action::StartBackup, 0).unwrap();
        f.backend(Backend::SourceReady { total_bytes: 1024 }, 0)
            .unwrap();
        f.backend(
            Backend::PlanSelected {
                plan: Plan::default(),
                eta: Some(Estimate {
                    seconds: 60,
                    lower_seconds: 40,
                    upper_seconds: 90,
                }),
            },
            0,
        )
        .unwrap();
        f.backend(Backend::CopyStarted, 0).unwrap();
        f
    }
    fn finish_copy(f: &mut Workflow) {
        f.backend(
            Backend::CopyProgress {
                read_bytes: 1024,
                durable_bytes: 1024,
                eta: None,
            },
            0,
        )
        .unwrap();
        assert_eq!(
            f.backend(Backend::CopyFinished, 0).unwrap(),
            vec![Effect::StartVerify]
        );
    }
    #[test]
    fn gui_countdown_only_starts_backup_once() {
        let mut f = Workflow::new(Startup::GuiCountdown, 0).unwrap();
        assert_eq!(f.countdown_seconds(), Some(30));
        assert!(f.tick(29_999).unwrap().is_empty());
        assert_eq!(f.countdown_seconds(), Some(1));
        assert_eq!(
            f.tick(30_000).unwrap(),
            vec![Effect::PrepareConfiguredBackup]
        );
        assert!(f.tick(60_000).unwrap().is_empty());
    }
    #[test]
    fn menu_input_cancels_auto_start_and_restore_never_auto_starts() {
        let mut f = Workflow::new(Startup::GuiCountdown, 0).unwrap();
        f.action(Action::MenuInteraction, 1000).unwrap();
        assert_eq!(f.countdown_seconds(), None);
        assert!(f.tick(99_000).unwrap().is_empty());
        assert_eq!(
            f.action(Action::SelectRestore, 99_000).unwrap(),
            vec![Effect::OpenRestoreWorkflow]
        );
        assert!(f.tick(199_000).unwrap().is_empty());
    }
    #[test]
    fn cui_has_no_countdown_and_can_start_without_input() {
        let mut f = Workflow::new(Startup::CuiUnattended, 0).unwrap();
        assert_eq!(f.countdown_seconds(), None);
        assert_eq!(f.tick(0).unwrap(), vec![Effect::PrepareConfiguredBackup]);
        let mut manual = Workflow::new(Startup::InteractiveMenu, 0).unwrap();
        assert!(manual.tick(99_000).unwrap().is_empty());
    }
    #[test]
    fn no_interaction_path_is_identical_after_menu_for_gui_and_cui() {
        for mode in [Startup::GuiCountdown, Startup::CuiUnattended] {
            let mut f = copying(mode);
            finish_copy(&mut f);
            assert_eq!(f.copy_percent(), Some(100));
            assert!(!f.dataset_committed());
            assert!(f.tick(0).unwrap().is_empty());
            f.backend(
                Backend::VerifyProgress {
                    verified_bytes: 1024,
                    eta: None,
                },
                0,
            )
            .unwrap();
            assert_eq!(
                f.backend(Backend::VerifyFinished, 0).unwrap(),
                vec![Effect::CommitDataset]
            );
            assert!(!f.can_cancel());
            assert_eq!(
                f.backend(Backend::DatasetCommitted, 0).unwrap(),
                vec![Effect::CleanOwnedMounts]
            );
            assert!(f.tick(0).unwrap().is_empty());
            f.backend(Backend::CleanupFinished, 0).unwrap();
            assert_eq!(f.phase(), Phase::CompleteMessage);
            assert!(f.tick(4999).unwrap().is_empty());
            assert_eq!(f.tick(5000).unwrap(), vec![Effect::RequestGuestPoweroff]);
            assert!(f.tick(6000).unwrap().is_empty());
        }
    }
    #[test]
    fn pause_and_resume_wait_for_acknowledgement() {
        let mut f = copying(Startup::GuiCountdown);
        assert_eq!(
            f.action(Action::Pause, 0).unwrap(),
            vec![Effect::PauseEngine]
        );
        assert_eq!(f.phase(), Phase::PauseRequested(Stage::Copy));
        assert!(f.eta().is_none());
        f.backend(Backend::Paused, 0).unwrap();
        assert!(f.can_resume());
        assert_eq!(
            f.action(Action::Resume, 0).unwrap(),
            vec![Effect::ResumeEngine]
        );
        assert_eq!(f.phase(), Phase::ResumeRequested(Stage::Copy));
        f.backend(Backend::Resumed, 0).unwrap();
        assert_eq!(f.phase(), Phase::Working(Stage::Copy));
    }
    #[test]
    fn pause_at_copy_boundary_does_not_lose_request_or_skip_verification() {
        let mut f = copying(Startup::GuiCountdown);
        f.action(Action::Pause, 0).unwrap();
        f.backend(
            Backend::CopyProgress {
                read_bytes: 1024,
                durable_bytes: 1024,
                eta: None,
            },
            0,
        )
        .unwrap();
        assert!(f.backend(Backend::CopyFinished, 0).unwrap().is_empty());
        f.backend(Backend::Paused, 0).unwrap();
        f.action(Action::Resume, 0).unwrap();
        assert_eq!(
            f.backend(Backend::Resumed, 0).unwrap(),
            vec![Effect::StartVerify]
        );
    }
    #[test]
    fn cancel_is_not_success_and_does_not_power_off() {
        let mut f = copying(Startup::CuiUnattended);
        assert_eq!(
            f.action(Action::Cancel, 0).unwrap(),
            vec![Effect::CancelEngine]
        );
        assert_eq!(f.phase(), Phase::CancelRequested);
        f.backend(Backend::Cancelled, 0).unwrap();
        assert_eq!(f.phase(), Phase::Cancelled);
        assert!(f.tick(1_000_000).unwrap().is_empty());
    }
    #[test]
    fn explicit_skip_finishes_copy_without_inventing_verified_bytes() {
        let mut f = copying(Startup::CuiUnattended);
        assert!(f.backend(Backend::VerifySkipped, 0).is_err());
        finish_copy(&mut f);
        assert_eq!(
            f.backend(Backend::VerifySkipped, 0).unwrap(),
            vec![Effect::CommitDataset]
        );
        assert_eq!(f.verified, 0);
        assert_eq!(f.phase(), Phase::Finalizing);
    }
    #[test]
    fn cleanup_error_keeps_committed_dataset_but_never_requests_poweroff() {
        let mut f = copying(Startup::GuiCountdown);
        finish_copy(&mut f);
        f.backend(
            Backend::VerifyProgress {
                verified_bytes: 1024,
                eta: None,
            },
            0,
        )
        .unwrap();
        f.backend(Backend::VerifyFinished, 0).unwrap();
        f.backend(Backend::DatasetCommitted, 0).unwrap();
        f.backend(
            Backend::Failed {
                code: "UNMOUNT_FAILED".into(),
            },
            0,
        )
        .unwrap();
        assert!(f.dataset_committed());
        assert_eq!(f.error_code(), Some("UNMOUNT_FAILED"));
        assert!(f.tick(99_999).unwrap().is_empty());
    }
    #[test]
    fn invalid_progress_and_reversed_clock_are_rejected() {
        let mut f = copying(Startup::GuiCountdown);
        assert!(f
            .backend(
                Backend::CopyProgress {
                    read_bytes: 10,
                    durable_bytes: 11,
                    eta: None
                },
                0
            )
            .is_err());
        assert!(f.backend(Backend::CopyFinished, 0).is_err());
        f.tick(10).unwrap();
        assert!(f.tick(9).is_err());
    }
    #[test]
    fn entire_successful_run_needs_no_user_action_on_either_surface() {
        for mode in [Startup::GuiCountdown, Startup::CuiUnattended] {
            let mut f = Workflow::new(mode, 0).unwrap();
            let now = if mode == Startup::GuiCountdown {
                30_000
            } else {
                0
            };
            assert_eq!(f.tick(now).unwrap(), vec![Effect::PrepareConfiguredBackup]);
            f.backend(Backend::SourceReady { total_bytes: 1024 }, now)
                .unwrap();
            f.backend(
                Backend::PlanSelected {
                    plan: Plan::default(),
                    eta: None,
                },
                now,
            )
            .unwrap();
            f.backend(Backend::CopyStarted, now).unwrap();
            f.backend(
                Backend::CopyProgress {
                    read_bytes: 1024,
                    durable_bytes: 1024,
                    eta: None,
                },
                now,
            )
            .unwrap();
            f.backend(Backend::CopyFinished, now).unwrap();
            f.backend(
                Backend::VerifyProgress {
                    verified_bytes: 1024,
                    eta: None,
                },
                now,
            )
            .unwrap();
            f.backend(Backend::VerifyFinished, now).unwrap();
            f.backend(Backend::DatasetCommitted, now).unwrap();
            f.backend(Backend::CleanupFinished, now).unwrap();
            assert_eq!(
                f.tick(now + 5000).unwrap(),
                vec![Effect::RequestGuestPoweroff]
            );
        }
    }
}
