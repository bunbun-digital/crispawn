// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! Shared GUI/terminal interaction rules. No disk access or display operations.
use crate::{
    live::{CommandAction, Status},
    types::decimal,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    #[default]
    Ja,
    En,
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Backup,
    Restore,
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Page {
    #[default]
    CopySelection,
    RestoreSelection,
    RestoreMapping,
    RestoreConfirmation,
    Running,
    Complete,
    Failed,
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskState {
    #[default]
    Waiting,
    Working,
    Done,
    Failed,
    Unavailable,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiskView {
    pub stable_id: String,
    pub name: String,
    #[serde(with = "decimal")]
    pub bytes: u64,
    pub selected: bool,
    pub state: DiskState,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedDisk {
    pub archive: String,
    pub name: String,
    pub stable_id: String,
    #[serde(with = "decimal")]
    pub bytes: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedSet {
    pub id: String,
    pub datetime: String,
    pub disks: Vec<SavedDisk>,
}
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionView {
    pub page: Page,
    pub mode: Mode,
    pub prepared: bool,
    pub preparation_error: Option<String>,
    pub disks: Vec<DiskView>,
    pub backups: Vec<SavedSet>,
    pub selected_backup: Option<usize>,
    pub selected_saved_disk: Option<usize>,
    pub selected_target: Option<usize>,
    pub current_disk: Option<usize>,
    pub completed_disks: u32,
    #[serde(with = "decimal")]
    pub total_bytes: u64,
    #[serde(with = "decimal")]
    pub previous_durable_bytes: u64,
    #[serde(with = "decimal")]
    pub previous_verified_bytes: u64,
    pub previous_elapsed_seconds: f64,
    #[serde(with = "decimal")]
    pub destination_free_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Character(char),
    Escape,
    PageUp,
    PageDown,
    End,
}
#[derive(Debug, Default)]
pub struct Interaction {
    pub logs_open: bool,
    pub log_page: usize,
    pub list_page: usize,
    pub cancel_open: bool,
    modal_backup_id: Option<String>,
}
impl Interaction {
    pub fn synchronize(&mut self, status: &Status) {
        if !status.can_cancel
            || self.modal_backup_id.as_deref()
                != status.snapshot.as_ref().map(|s| s.backup_id.as_str())
        {
            self.cancel_open = false;
        }
    }
    /// C opens a confirmation; only Y produces a cancellation request.
    /// Log/page actions never execute or pause a data operation.
    pub fn key(&mut self, key: Key, status: &Status) -> Option<CommandAction> {
        self.synchronize(status);
        let key = match key {
            Key::Character(c) => Key::Character(c.to_ascii_lowercase()),
            k => k,
        };
        if self.cancel_open {
            match key {
                Key::Escape | Key::Character('c') => self.cancel_open = false,
                Key::Character('y') if status.can_cancel => {
                    self.cancel_open = false;
                    return Some(CommandAction::Cancel);
                }
                _ => {}
            }
            return None;
        }
        if key == Key::Character('i') {
            self.logs_open = !self.logs_open;
            self.log_page = 0;
            return None;
        }
        if self.logs_open {
            match key {
                Key::Escape => self.logs_open = false,
                Key::PageUp => self.log_page = self.log_page.saturating_add(1),
                Key::PageDown => self.log_page = self.log_page.saturating_sub(1),
                Key::End => self.log_page = 0,
                _ => {}
            }
            return None;
        }
        if key == Key::Character('c') && status.can_cancel {
            self.cancel_open = true;
            self.modal_backup_id = status.snapshot.as_ref().map(|s| s.backup_id.clone());
            return None;
        }
        if key == Key::Character('p') {
            if status.can_resume {
                return Some(CommandAction::Resume);
            }
            if status.can_pause {
                return Some(CommandAction::Pause);
            }
        }
        let session = &status.session;
        if matches!(session.page, Page::Running | Page::Complete | Page::Failed) {
            return None;
        }
        match key {
            Key::Character('b') => {
                self.list_page = 0;
                Some(CommandAction::CopyMode)
            }
            Key::Character('r') => {
                self.list_page = 0;
                Some(CommandAction::RestoreMode)
            }
            Key::Character('s') if session.page == Page::CopySelection => {
                Some(CommandAction::StartCopy)
            }
            Key::Character('n')
                if matches!(session.page, Page::RestoreSelection | Page::RestoreMapping) =>
            {
                self.list_page = 0;
                Some(CommandAction::Next)
            }
            Key::Character('y') if session.page == Page::RestoreConfirmation => {
                Some(CommandAction::StartRestore)
            }
            Key::Escape if session.mode == Mode::Restore => {
                self.list_page = 0;
                Some(CommandAction::Back)
            }
            Key::PageUp => {
                self.list_page = self.list_page.saturating_sub(1);
                None
            }
            Key::PageDown => {
                let count = match session.page {
                    Page::CopySelection => session.disks.len(),
                    Page::RestoreSelection => session.backups.len(),
                    Page::RestoreMapping => session.disks.len().max(
                        session
                            .selected_backup
                            .and_then(|i| session.backups.get(i))
                            .map(|b| b.disks.len())
                            .unwrap_or(0),
                    ),
                    _ => 0,
                };
                self.list_page = self
                    .list_page
                    .saturating_add(1)
                    .min(count.saturating_sub(1) / 3);
                None
            }
            Key::Character(c @ '1'..='9') => {
                let index = self
                    .list_page
                    .saturating_mul(3)
                    .saturating_add((c as usize) - ('1' as usize));
                if c > '3' || index > u32::MAX as usize {
                    return None;
                }
                let index = index as u32;
                match session.page {
                    Page::CopySelection => Some(CommandAction::SelectSource { index }),
                    Page::RestoreSelection => Some(CommandAction::SelectBackup { index }),
                    Page::RestoreMapping => Some(CommandAction::SelectSavedDisk { index }),
                    _ => None,
                }
            }
            Key::Character(c @ ('a' | 'd' | 'f')) if session.page == Page::RestoreMapping => {
                let offset = match c {
                    'a' => 0,
                    'd' => 1,
                    _ => 2,
                };
                let index = self.list_page.saturating_mul(3).saturating_add(offset);
                u32::try_from(index)
                    .ok()
                    .map(|index| CommandAction::SelectTarget { index })
            }
            _ => None,
        }
    }
}

/// Decimal GB with one digit and exact decimal bytes. No floating-point rounding.
pub fn capacity(bytes: u64) -> String {
    let whole = bytes / 1_000_000_000;
    let tenth = (bytes % 1_000_000_000) / 100_000_000;
    format!("{whole}.{tenth} GB")
}
pub fn grouped(bytes: u64) -> String {
    let digits = bytes.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',')
        }
        out.push(c);
    }
    out
}
pub fn exact(bytes: u64) -> String {
    format!("{} bytes", grouped(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn running() -> Status {
        let mut s = Status::new("test".into());
        s.phase = "COPYING".into();
        s.session.page = Page::Running;
        s.can_cancel = true;
        s.can_pause = true;
        s
    }
    #[test]
    fn cancellation_requires_confirmation_and_escape_keeps_work_running() {
        let s = running();
        let mut ui = Interaction::default();
        assert!(ui.key(Key::Character('c'), &s).is_none());
        assert!(ui.cancel_open);
        assert!(ui.key(Key::Character('p'), &s).is_none());
        assert!(ui.key(Key::Escape, &s).is_none());
        assert!(!ui.cancel_open);
        assert!(ui.key(Key::Character('y'), &s).is_none());
        ui.key(Key::Character('c'), &s);
        assert_eq!(ui.key(Key::Character('y'), &s), Some(CommandAction::Cancel));
    }
    #[test]
    fn completed_operation_cannot_accept_a_stale_confirmation() {
        let mut s = running();
        let mut ui = Interaction::default();
        ui.key(Key::Character('c'), &s);
        s.can_cancel = false;
        s.session.page = Page::Complete;
        assert!(ui.key(Key::Character('y'), &s).is_none());
        assert!(!ui.cancel_open);
    }
    #[test]
    fn logs_capture_keys_and_do_not_start_or_stop_work() {
        let s = running();
        let mut ui = Interaction::default();
        ui.key(Key::Character('i'), &s);
        for key in ['b', 'r', 's', 'n', 'y', 'p', 'c', '1'] {
            assert!(ui.key(Key::Character(key), &s).is_none());
        }
        ui.key(Key::PageUp, &s);
        assert_eq!(ui.log_page, 1);
        ui.key(Key::End, &s);
        assert_eq!(ui.log_page, 0);
        ui.key(Key::Escape, &s);
        assert!(!ui.logs_open);
    }
    #[test]
    fn numeric_choices_are_contextual_and_running_modes_are_locked() {
        let mut s = Status::new("test".into());
        let mut ui = Interaction::default();
        assert_eq!(
            ui.key(Key::Character('2'), &s),
            Some(CommandAction::SelectSource { index: 1 })
        );
        s.session.page = Page::RestoreMapping;
        assert_eq!(
            ui.key(Key::Character('2'), &s),
            Some(CommandAction::SelectSavedDisk { index: 1 })
        );
        assert_eq!(
            ui.key(Key::Character('d'), &s),
            Some(CommandAction::SelectTarget { index: 1 })
        );
        s.session.page = Page::Running;
        assert!(ui.key(Key::Character('r'), &s).is_none());
    }
    #[test]
    fn capacity_format_preserves_exact_large_integers() {
        assert_eq!(capacity(1_234_567_890), "1.2 GB");
        assert_eq!(exact(u64::MAX), "18,446,744,073,709,551,615 bytes");
    }
}
