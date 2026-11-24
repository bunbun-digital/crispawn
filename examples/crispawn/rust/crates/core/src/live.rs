// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! Versioned observer/control data. No filesystem, rendering or disk operations.
use crate::{plan::Plan, types::Snapshot};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecureBootObservation {
    pub enabled: Option<bool>,
    pub setup_mode: Option<bool>,
    pub shim_validation_disabled: bool,
    pub lockdown: String,
}
/// Presentation metadata, independent of any transport implementation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DestinationKind {
    UsbDisk,
    Smb,
    GoogleDrive,
    OneDrive,
    Memory,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Destination {
    pub kind: DestinationKind,
    pub filesystem: Option<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageStatus {
    pub phase: String,
    pub written_blocks: u64,
    pub confirmed_blocks: u64,
    pub waiting_blocks: u64,
    pub queued_encoded_bytes: u64,
    pub current_file_bytes: u64,
    pub baseline_bytes: u64,
    pub write_workers: u32,
    #[serde(default)]
    pub active_write_workers: u32,
    pub written_bytes: u64,
    pub confirmed_bytes: u64,
    /// Source-byte equivalent of checked compressed payloads, for progress only.
    #[serde(default)]
    pub confirmed_source_bytes: u64,
    #[serde(default)]
    pub written_source_bytes: u64,
    pub phase_elapsed_ms: u64,
    pub read_mb_per_second: f64,
    pub write_mb_per_second: f64,
    pub calibrated: bool,
    pub elapsed_seconds: u64,
    pub calibration_bytes: u64,
    pub calibration_elapsed_ms: u64,
    pub last_write_elapsed_ms: u64,
    pub last_check_elapsed_ms: u64,
}
/// KiB from one /proc/meminfo sample; byte limits describe application RAM.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryStatus {
    pub total_kib: u64,
    pub available_kib: u64,
    pub cached_kib: u64,
    pub dirty_kib: u64,
    pub writeback_kib: u64,
    pub anonymous_kib: u64,
    pub processing_limit_bytes: u64,
    pub encoded_queue_bytes: u64,
    pub encoded_queue_limit_bytes: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Status {
    pub schema_version: u32,
    pub job_id: String,
    pub phase: String,
    pub detail: String,
    pub heartbeat: u64,
    pub monotonic_ms: u64,
    pub progress_age_ms: u64,
    pub rss_kib: u64,
    pub memory_available_kib: u64,
    #[serde(default)]
    pub memory: Option<MemoryStatus>,
    #[serde(default = "enabled")]
    pub backup_verification_enabled: bool,
    pub log_dropped: u64,
    #[serde(default)]
    pub io_retries: u64,
    pub boot_uuid: String,
    #[serde(default)]
    pub data_uuid: String,
    #[serde(default)]
    pub boot_partition: String,
    #[serde(default)]
    pub destination_partition: String,
    #[serde(default)]
    pub backup_destination: Option<Destination>,
    #[serde(default)]
    pub log_destination: Option<Destination>,
    #[serde(default)]
    pub secure_boot: Option<SecureBootObservation>,
    pub excluded_disk: String,
    pub source_disk: String,
    pub source_id: String,
    #[serde(default, with = "crate::types::decimal")]
    pub source_bytes: u64,
    pub ntfs_mounted: bool,
    pub logs_persisted: bool,
    pub poweroff_requested: bool,
    pub gui_ready: bool,
    pub active_vt: u32,
    pub countdown_seconds: Option<u32>,
    #[serde(default)]
    pub finalization_complete: bool,
    #[serde(default)]
    pub shutdown_countdown_seconds: Option<u32>,
    pub selected_plan: Option<Plan>,
    #[serde(default = "one_verifier")]
    pub verify_workers: u32,
    pub snapshot: Option<Snapshot>,
    #[serde(default)]
    pub storage: Option<StorageStatus>,
    pub percent: u32,
    pub eta_seconds: Option<u64>,
    pub can_pause: bool,
    pub can_resume: bool,
    pub can_cancel: bool,
    pub can_restore: bool,
    pub last_command: String,
    #[serde(default)]
    pub language: crate::presentation::Language,
    #[serde(default)]
    pub session: crate::presentation::SessionView,
    #[serde(default)]
    pub catalog_omitted: bool,
}
impl Status {
    pub fn new(job_id: String) -> Self {
        Self {
            schema_version: 1,
            job_id,
            phase: "STARTING".into(),
            detail: String::new(),
            heartbeat: 0,
            monotonic_ms: 0,
            progress_age_ms: 0,
            rss_kib: 0,
            memory_available_kib: 0,
            memory: None,
            backup_verification_enabled: true,
            log_dropped: 0,
            io_retries: 0,
            boot_uuid: String::new(),
            data_uuid: String::new(),
            boot_partition: String::new(),
            destination_partition: String::new(),
            backup_destination: None,
            log_destination: None,
            secure_boot: None,
            excluded_disk: String::new(),
            source_disk: String::new(),
            source_id: String::new(),
            source_bytes: 0,
            ntfs_mounted: false,
            logs_persisted: false,
            poweroff_requested: false,
            gui_ready: false,
            active_vt: 0,
            countdown_seconds: None,
            finalization_complete: false,
            shutdown_countdown_seconds: None,
            selected_plan: None,
            verify_workers: 1,
            snapshot: None,
            storage: None,
            percent: 0,
            eta_seconds: None,
            can_pause: false,
            can_resume: false,
            can_cancel: false,
            can_restore: false,
            last_command: String::new(),
            language: crate::presentation::Language::default(),
            session: crate::presentation::SessionView::default(),
            catalog_omitted: false,
        }
    }
    /// UART is deliberately bounded; full choices remain in the RAM status file.
    pub fn probe_projection(&self) -> Self {
        let mut value = self.clone();
        value.catalog_omitted = !value.session.backups.is_empty() || value.session.disks.len() > 8;
        value.session.backups.clear();
        value.session.disks.truncate(8);
        for disk in &mut value.session.disks {
            disk.name = disk.name.chars().take(80).collect();
        }
        value
    }
}
fn one_verifier() -> u32 {
    1
}
fn enabled() -> bool {
    true
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandAction {
    MenuInteraction,
    Backup,
    Restore,
    Pause,
    Resume,
    Cancel,
    SaveLogs,
    ShowLogs,
    CopyMode,
    RestoreMode,
    StartCopy,
    StartRestore,
    Next,
    Back,
    SelectSource { index: u32 },
    SelectBackup { index: u32 },
    SelectSavedDisk { index: u32 },
    SelectTarget { index: u32 },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub schema_version: u32,
    pub id: String,
    pub action: CommandAction,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn older_status_defaults_presentation_metadata_without_changing_engine_state() {
        let mut value = serde_json::to_value(Status::new("old-run".into())).unwrap();
        for key in [
            "backup_destination",
            "log_destination",
            "finalization_complete",
            "shutdown_countdown_seconds",
            "memory",
            "backup_verification_enabled",
        ] {
            value.as_object_mut().unwrap().remove(key);
        }
        let old: Status = serde_json::from_value(value).unwrap();
        assert!(old.backup_destination.is_none());
        assert!(!old.finalization_complete);
        assert!(old.memory.is_none());
        assert!(old.backup_verification_enabled);
        assert_eq!(old.job_id, "old-run");
        assert_eq!(old.phase, "STARTING");
    }
    #[test]
    fn memory_logs_do_not_claim_ntfs_and_destination_types_roundtrip() {
        for kind in [
            DestinationKind::UsbDisk,
            DestinationKind::Smb,
            DestinationKind::GoogleDrive,
            DestinationKind::OneDrive,
            DestinationKind::Memory,
        ] {
            let destination = Destination {
                filesystem: if kind == DestinationKind::UsbDisk {
                    Some("NTFS".into())
                } else {
                    None
                },
                kind: kind.clone(),
            };
            let decoded: Destination =
                serde_json::from_slice(&serde_json::to_vec(&destination).unwrap()).unwrap();
            assert_eq!(decoded.kind, kind);
            if kind == DestinationKind::Memory {
                assert!(decoded.filesystem.is_none());
            }
        }
    }
}
