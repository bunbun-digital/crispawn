// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! Shared disk/session selection and countdown. No GUI, filesystem or device I/O.
use crate::{
    error::require,
    live::CommandAction,
    presentation::{DiskState, DiskView, Mode, Page, SavedSet, SessionView},
    Error, Result,
};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Start {
    Backup {
        disks: Vec<usize>,
    },
    Restore {
        backup: usize,
        saved_disk: usize,
        target: usize,
    },
}
pub struct Session {
    pub view: SessionView,
    prepared: Vec<Option<std::result::Result<(), String>>>,
    global_error: Option<String>,
    prerequisites_ready: bool,
    deadline_ms: u64,
    last_ms: u64,
    automatic: bool,
    requested: bool,
    started: bool,
}
impl Session {
    pub fn new(
        mut disks: Vec<DiskView>,
        backups: Vec<SavedSet>,
        preset: &[String],
        now_ms: u64,
        menu_seconds: u32,
    ) -> Result<Self> {
        let mut ids = BTreeSet::new();
        require(
            disks.len() <= 64 && backups.len() <= 1024,
            "CATALOG_LIMIT",
            "disk/history catalog exceeds its bound",
        )?;
        for disk in &mut disks {
            require(
                !disk.stable_id.is_empty() && ids.insert(disk.stable_id.clone()),
                "INVALID_CATALOG",
                "empty or duplicate disk identity",
            )?;
            disk.selected = disk.state != DiskState::Unavailable
                && (preset.is_empty() || preset.contains(&disk.stable_id));
        }
        require(
            preset.iter().all(|id| ids.contains(id)),
            "SOURCE_NOT_FOUND",
            "configured disk identity is absent",
        )?;
        require(menu_seconds <= 300, "INVALID_CONFIG", "menu timeout")?;
        let deadline_ms = now_ms
            .checked_add(u64::from(menu_seconds) * 1000)
            .ok_or_else(|| Error::new("CLOCK_RANGE", "menu deadline overflow"))?;
        let count = disks.len();
        let mut value = Self {
            view: SessionView {
                disks,
                backups,
                ..SessionView::default()
            },
            prepared: vec![None; count],
            global_error: None,
            prerequisites_ready: true,
            deadline_ms,
            last_ms: now_ms,
            automatic: true,
            requested: false,
            started: false,
        };
        value.recompute()?;
        require(
            value.view.disks.iter().any(|disk| disk.selected),
            "SOURCE_NOT_FOUND",
            "no eligible configured disk",
        )?;
        Ok(value)
    }
    fn recompute(&mut self) -> Result<()> {
        let selected: Vec<_> = self
            .view
            .disks
            .iter()
            .enumerate()
            .filter(|(_, d)| d.selected)
            .map(|(i, _)| i)
            .collect();
        self.view.total_bytes = selected
            .iter()
            .try_fold(0u64, |sum, &i| sum.checked_add(self.view.disks[i].bytes))
            .ok_or_else(|| Error::new("SIZE_OVERFLOW", "selected disk capacities exceed u64"))?;
        self.view.prepared = !selected.is_empty()
            && self.prerequisites_ready
            && selected.iter().all(|&i| self.prepared[i].is_some())
            && self.global_error.is_none();
        self.view.preparation_error = self.global_error.clone().or_else(|| {
            selected.iter().find_map(|&i| {
                self.prepared[i]
                    .as_ref()
                    .and_then(|r| r.as_ref().err())
                    .cloned()
            })
        });
        if self.view.preparation_error.is_some() {
            self.view.prepared = false;
        }
        Ok(())
    }
    pub fn prepared(
        &mut self,
        index: usize,
        result: std::result::Result<(), String>,
    ) -> Result<()> {
        let item = self
            .prepared
            .get_mut(index)
            .ok_or_else(|| Error::new("INVALID_SELECTION", "preparation index"))?;
        *item = Some(result);
        self.recompute()
    }
    pub fn global_preparation_error(&mut self, message: String) -> Result<()> {
        self.global_error = Some(message);
        self.recompute()
    }
    /// Global asynchronous prerequisites (e.g. configured networking) gate both
    /// automatic and manual start without restarting the visible countdown.
    pub fn prerequisites_ready(&mut self, ready: bool) -> Result<()> {
        self.prerequisites_ready = ready;
        self.recompute()
    }
    pub fn countdown(&self, now_ms: u64) -> Option<u32> {
        (self.automatic && !self.started && self.view.mode == Mode::Backup).then(|| {
            self.deadline_ms
                .saturating_sub(now_ms)
                .div_ceil(1000)
                .min(300) as u32
        })
    }
    fn choice<T>(items: &[T], index: u32) -> Result<usize> {
        let i = index as usize;
        require(
            i < items.len(),
            "INVALID_SELECTION",
            "choice outside current catalog",
        )?;
        Ok(i)
    }
    fn saved(&self) -> Result<&crate::presentation::SavedDisk> {
        self.view
            .selected_backup
            .and_then(|i| self.view.backups.get(i))
            .and_then(|s| self.view.selected_saved_disk.and_then(|i| s.disks.get(i)))
            .ok_or_else(|| Error::new("INVALID_SELECTION", "saved disk must be selected"))
    }
    pub fn action(&mut self, action: CommandAction) -> Result<Option<Start>> {
        require(
            !self.started,
            "CONTROL_UNAVAILABLE",
            "session already started",
        )?;
        match action {
            CommandAction::CopyMode | CommandAction::Backup => {
                self.automatic = false;
                self.requested = false;
                self.view.mode = Mode::Backup;
                self.view.page = Page::CopySelection;
                self.recompute()?;
                if action == CommandAction::Backup {
                    self.requested = true;
                }
            }
            CommandAction::RestoreMode | CommandAction::Restore => {
                self.automatic = false;
                self.requested = false;
                self.view.mode = Mode::Restore;
                self.view.page = Page::RestoreSelection;
            }
            CommandAction::MenuInteraction => self.automatic = false,
            CommandAction::SelectSource { index } => {
                require(
                    self.view.page == Page::CopySelection,
                    "CONTROL_UNAVAILABLE",
                    "not choosing copy disks",
                )?;
                let i = Self::choice(&self.view.disks, index)?;
                require(
                    self.view.disks[i].state != DiskState::Unavailable,
                    "DISK_UNAVAILABLE",
                    "disk is in use or excluded",
                )?;
                self.view.disks[i].selected = !self.view.disks[i].selected;
                self.automatic = false;
                self.requested = false;
                self.recompute()?;
            }
            CommandAction::StartCopy => {
                require(
                    self.view.page == Page::CopySelection
                        && self.view.disks.iter().any(|d| d.selected),
                    "INVALID_SELECTION",
                    "select at least one copy disk",
                )?;
                self.requested = true;
                self.automatic = false;
            }
            CommandAction::SelectBackup { index } => {
                require(
                    self.view.page == Page::RestoreSelection,
                    "CONTROL_UNAVAILABLE",
                    "not choosing saved copy",
                )?;
                self.view.selected_backup = Some(Self::choice(&self.view.backups, index)?);
                self.view.selected_saved_disk = None;
                self.view.selected_target = None;
            }
            CommandAction::SelectSavedDisk { index } => {
                require(
                    self.view.page == Page::RestoreMapping,
                    "CONTROL_UNAVAILABLE",
                    "not mapping restore disks",
                )?;
                let backup = self
                    .view
                    .selected_backup
                    .and_then(|i| self.view.backups.get(i))
                    .ok_or_else(|| Error::new("INVALID_SELECTION", "choose a saved copy"))?;
                self.view.selected_saved_disk = Some(Self::choice(&backup.disks, index)?);
                if let Some(target) = self.view.selected_target {
                    if self.view.disks[target].bytes < self.saved()?.bytes {
                        self.view.selected_target = None;
                    }
                }
            }
            CommandAction::SelectTarget { index } => {
                require(
                    self.view.page == Page::RestoreMapping,
                    "CONTROL_UNAVAILABLE",
                    "not choosing restore target",
                )?;
                let i = Self::choice(&self.view.disks, index)?;
                require(
                    self.view.disks[i].state != DiskState::Unavailable,
                    "DISK_UNAVAILABLE",
                    "restore target is unavailable",
                )?;
                require(
                    self.view.disks[i].bytes >= self.saved()?.bytes,
                    "TARGET_TOO_SMALL",
                    "restore target is too small",
                )?;
                self.view.selected_target = Some(i);
            }
            CommandAction::Next => match self.view.page {
                Page::RestoreSelection => {
                    require(
                        self.view.selected_backup.is_some(),
                        "INVALID_SELECTION",
                        "choose a saved copy",
                    )?;
                    self.view.page = Page::RestoreMapping;
                }
                Page::RestoreMapping => {
                    self.saved()?;
                    require(
                        self.view.selected_target.is_some(),
                        "INVALID_SELECTION",
                        "choose a restore target",
                    )?;
                    self.view.page = Page::RestoreConfirmation;
                }
                _ => return Err(Error::new("CONTROL_UNAVAILABLE", "no next selection step")),
            },
            CommandAction::Back => {
                self.view.page = match self.view.page {
                    Page::RestoreConfirmation => Page::RestoreMapping,
                    Page::RestoreMapping => Page::RestoreSelection,
                    Page::RestoreSelection => {
                        self.view.mode = Mode::Backup;
                        Page::CopySelection
                    }
                    _ => {
                        return Err(Error::new(
                            "CONTROL_UNAVAILABLE",
                            "no previous selection step",
                        ))
                    }
                }
            }
            CommandAction::StartRestore => {
                require(
                    self.prerequisites_ready,
                    "PREPARATION_PENDING",
                    "session prerequisites are not ready",
                )?;
                require(
                    self.view.page == Page::RestoreConfirmation,
                    "RESTORE_CONFIRMATION_REQUIRED",
                    "restore requires its final confirmation",
                )?;
                let saved = self.saved()?;
                let target = self
                    .view
                    .selected_target
                    .ok_or_else(|| Error::new("INVALID_SELECTION", "restore target"))?;
                require(
                    self.view.disks[target].bytes >= saved.bytes
                        && self.view.disks[target].state != DiskState::Unavailable,
                    "TARGET_TOO_SMALL",
                    "restore target is unavailable or too small",
                )?;
                let choice = Start::Restore {
                    backup: self.view.selected_backup.unwrap(),
                    saved_disk: self.view.selected_saved_disk.unwrap(),
                    target,
                };
                self.view.total_bytes = saved.bytes;
                for (i, disk) in self.view.disks.iter_mut().enumerate() {
                    disk.selected = i == target;
                }
                self.started = true;
                self.view.page = Page::Running;
                self.view.current_disk = Some(target);
                return Ok(Some(choice));
            }
            _ => {
                return Err(Error::new(
                    "CONTROL_UNAVAILABLE",
                    "command is not a session selection",
                ))
            }
        }
        Ok(None)
    }
    pub fn tick(&mut self, now_ms: u64) -> Result<Option<Start>> {
        require(
            now_ms >= self.last_ms,
            "CLOCK_REVERSED",
            "session clock reversed",
        )?;
        self.last_ms = now_ms;
        if self.started
            || self.view.mode != Mode::Backup
            || !(self.requested || (self.automatic && now_ms >= self.deadline_ms))
        {
            return Ok(None);
        }
        if let Some(error) = &self.view.preparation_error {
            return Err(Error::new("PREPARATION_FAILED", error.clone()));
        }
        if !self.view.prepared {
            return Ok(None);
        }
        let disks: Vec<_> = self
            .view
            .disks
            .iter()
            .enumerate()
            .filter(|(_, d)| d.selected)
            .map(|(i, _)| i)
            .collect();
        require(
            !disks.is_empty(),
            "SOURCE_NOT_FOUND",
            "no selected source disk",
        )?;
        self.started = true;
        self.view.page = Page::Running;
        self.view.current_disk = disks.first().copied();
        Ok(Some(Start::Backup { disks }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::{SavedDisk, SavedSet};
    fn new() -> Session {
        let disks = (0..3)
            .map(|i| DiskView {
                stable_id: format!("disk-{i}"),
                name: format!("Disk {i}"),
                bytes: 1024 * (i + 1),
                selected: false,
                state: DiskState::Waiting,
            })
            .collect();
        let backups = vec![SavedSet {
            id: "saved".into(),
            datetime: "2026-11-22 03:00 UTC".into(),
            disks: vec![SavedDisk {
                archive: "backups/a".into(),
                name: "original".into(),
                stable_id: "old-disk".into(),
                bytes: 2048,
            }],
        }];
        Session::new(disks, backups, &[], 0, 30).unwrap()
    }
    #[test]
    fn countdown_and_preparation_overlap_and_both_are_required() {
        let mut s = new();
        assert!(s.view.disks.iter().all(|d| d.selected));
        assert!(s.tick(30_000).unwrap().is_none());
        for i in 0..3 {
            s.prepared(i, Ok(())).unwrap();
        }
        assert_eq!(
            s.tick(30_001).unwrap(),
            Some(Start::Backup {
                disks: vec![0, 1, 2]
            })
        );
        assert!(s.tick(31_000).unwrap().is_none());
    }
    #[test]
    fn prepared_before_deadline_does_not_start_early() {
        let mut s = new();
        for i in 0..3 {
            s.prepared(i, Ok(())).unwrap();
        }
        assert!(s.tick(29_999).unwrap().is_none());
        assert_eq!(s.countdown(29_999), Some(1));
        assert!(s.tick(30_000).unwrap().is_some());
    }
    #[test]
    fn asynchronous_prerequisite_waits_without_restarting_the_countdown() {
        let mut s = new();
        s.prerequisites_ready(false).unwrap();
        for i in 0..3 {
            s.prepared(i, Ok(())).unwrap();
        }
        assert!(!s.view.prepared);
        assert_eq!(s.countdown(30_000), Some(0));
        assert!(s.tick(40_000).unwrap().is_none());
        s.prerequisites_ready(true).unwrap();
        assert!(s.tick(40_001).unwrap().is_some());
    }
    #[test]
    fn manual_selection_stops_automatic_start_and_waits_for_preparation() {
        let mut s = new();
        s.action(CommandAction::SelectSource { index: 1 }).unwrap();
        assert_eq!(s.countdown(0), None);
        assert!(s.tick(40_000).unwrap().is_none());
        s.action(CommandAction::StartCopy).unwrap();
        assert!(s.tick(40_001).unwrap().is_none());
        s.prepared(0, Ok(())).unwrap();
        s.prepared(2, Ok(())).unwrap();
        assert_eq!(
            s.tick(40_002).unwrap(),
            Some(Start::Backup { disks: vec![0, 2] })
        );
    }
    #[test]
    fn restore_requires_explicit_mapping_and_final_confirmation() {
        let mut s = new();
        s.action(CommandAction::RestoreMode).unwrap();
        assert!(s.tick(100_000).unwrap().is_none());
        assert!(s.action(CommandAction::StartRestore).is_err());
        s.action(CommandAction::SelectBackup { index: 0 }).unwrap();
        s.action(CommandAction::Next).unwrap();
        s.action(CommandAction::SelectSavedDisk { index: 0 })
            .unwrap();
        assert!(s.action(CommandAction::SelectTarget { index: 0 }).is_err());
        s.action(CommandAction::SelectTarget { index: 1 }).unwrap();
        assert!(s.action(CommandAction::StartRestore).is_err());
        s.action(CommandAction::Next).unwrap();
        assert_eq!(
            s.action(CommandAction::StartRestore).unwrap(),
            Some(Start::Restore {
                backup: 0,
                saved_disk: 0,
                target: 1
            })
        );
    }
    #[test]
    fn invalid_choice_and_late_mode_change_never_change_target() {
        let mut s = new();
        assert!(s
            .action(CommandAction::SelectSource { index: 100 })
            .is_err());
        for i in 0..3 {
            s.prepared(i, Ok(())).unwrap();
        }
        s.tick(30_000).unwrap();
        assert!(s.action(CommandAction::RestoreMode).is_err());
        assert_eq!(s.view.mode, Mode::Backup);
    }
    #[test]
    fn failed_preparation_is_reported_and_can_be_deselected_before_start() {
        let mut s = new();
        s.prepared(0, Err("IO_ERROR".into())).unwrap();
        assert!(s.tick(30_000).is_err());
        s.action(CommandAction::SelectSource { index: 0 }).unwrap();
        s.prepared(1, Ok(())).unwrap();
        s.prepared(2, Ok(())).unwrap();
        s.action(CommandAction::StartCopy).unwrap();
        assert_eq!(
            s.tick(30_001).unwrap(),
            Some(Start::Backup { disks: vec![1, 2] })
        );
    }
}
