// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
use crispawn_core::types::BlockIdentity;
use crispawn_engine::{
    device::{self, Device},
    diagnostics::Diagnostics,
    io,
    live::{self, Config, InitialOperation},
};
use serde_json::json;
use std::{
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    path::{Path, PathBuf},
};
fn disk(name: &str, usb: bool, partition: bool, whole: &str) -> Device {
    Device {
        name: name.into(),
        path: PathBuf::from(format!("/dev/{name}")),
        number: if partition { "8:1" } else { "8:0" }.into(),
        whole_name: whole.into(),
        partition,
        usb,
        removable: false,
        size_bytes: 128 << 20,
        identity: Some(BlockIdentity {
            stable_id: format!("serial:{name}"),
            logical_sector_bytes: 512,
            physical_sector_bytes: 4096,
        }),
    }
}
#[test]
fn auto_selection_excludes_boot_usb_partitions_and_ambiguous_internal_disks() {
    let list = vec![
        disk("sda", true, false, "sda"),
        disk("sda1", true, true, "sda"),
        disk("nvme0n1", false, false, "nvme0n1"),
    ];
    assert_eq!(
        device::select_source(&list, "sda", None).unwrap().name,
        "nvme0n1"
    );
    assert!(device::select_source(&list, "nvme0n1", None).is_err());
    let mut many = list;
    many.push(disk("vda", false, false, "vda"));
    assert_eq!(
        device::select_source(&many, "sda", None).unwrap_err().code,
        "AMBIGUOUS_SOURCE"
    );
    assert_eq!(
        device::select_source(&many, "sda", Some("serial:vda"))
            .unwrap()
            .name,
        "vda"
    );
    assert!(device::select_source(&many, "sda", Some("serial:sda")).is_err());
}
#[test]
fn mounted_partition_swap_and_holders_refuse_source_use() {
    let dir = tempfile::tempdir().unwrap();
    let source = disk("sda", false, false, "sda");
    let partition = disk("sda1", false, true, "sda");
    let list = vec![source.clone(), partition];
    assert!(device::require_unused(
        &source,
        &list,
        dir.path(),
        "20 1 8:1 / /data rw - ntfs3 /dev/sda1 rw",
        ""
    )
    .is_err());
    assert!(device::require_unused(
        &source,
        &list,
        dir.path(),
        "",
        "Filename Type Size Used Priority\n/dev/sda1 partition 1 0 -2"
    )
    .is_err());
    fs::create_dir_all(dir.path().join("class/block/sda1/holders")).unwrap();
    fs::write(dir.path().join("class/block/sda1/holders/dm-0"), b"").unwrap();
    assert!(device::require_unused(&source, &list, dir.path(), "", "").is_err());
}
#[test]
fn explicit_block_adapter_refuses_an_ordinary_file_without_modifying_it() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("source");
    fs::write(&file, b"preserve").unwrap();
    assert_eq!(
        device::open(&file, true, true).unwrap_err().code,
        "BLOCK_DEVICE_REQUIRED"
    );
    assert_eq!(fs::read(file).unwrap(), b"preserve");
}
#[test]
fn sysfs_discovery_records_stable_identity_and_usb_ancestry() {
    let dir = tempfile::tempdir().unwrap();
    let sys = dir.path();
    fs::create_dir_all(sys.join("class/block")).unwrap();
    for (name, parent, part) in [
        ("sda", "devices/usb1/storage/sda", false),
        ("sda1", "devices/usb1/storage/sda/sda1", true),
        ("vda", "devices/pci/virtio/vda", false),
    ] {
        let p = sys.join(parent);
        fs::create_dir_all(p.join("queue")).unwrap();
        fs::write(
            p.join("dev"),
            if part {
                "8:1"
            } else if name == "sda" {
                "8:0"
            } else {
                "254:0"
            },
        )
        .unwrap();
        fs::write(p.join("size"), "262144").unwrap();
        fs::write(p.join("queue/logical_block_size"), "512").unwrap();
        fs::write(p.join("queue/physical_block_size"), "4096").unwrap();
        if part {
            fs::write(p.join("partition"), "1").unwrap();
        } else {
            fs::write(p.join("serial"), name).unwrap();
        }
        symlink(&p, sys.join("class/block").join(name)).unwrap();
    }
    let found = device::scan(sys, Path::new("/dev")).unwrap();
    assert!(found.iter().find(|d| d.name == "sda").unwrap().usb);
    assert_eq!(
        found.iter().find(|d| d.name == "sda1").unwrap().whole_name,
        "sda"
    );
    let selected = device::select_source(&found, "sda", None).unwrap();
    assert_eq!(selected.name, "vda");
    assert_eq!(selected.size_bytes, 128 << 20);
    assert!(selected
        .identity
        .as_ref()
        .unwrap()
        .stable_id
        .starts_with("serial:vda"));
}
#[test]
fn ntfs_uuid_and_strict_live_configuration_are_validated() {
    let mut sector = [0u8; 512];
    sector[3..11].copy_from_slice(b"NTFS    ");
    sector[510..512].copy_from_slice(&[0x55, 0xaa]);
    sector[72..80].copy_from_slice(&0x123456789abcdef0u64.to_le_bytes());
    assert_eq!(live::ntfs_uuid(&sector).unwrap(), "123456789ABCDEF0");
    sector[3] = 0;
    assert!(live::ntfs_uuid(&sector).is_err());
    assert!(live::ntfs_uuid(&[0; 20]).is_err());
    let mut c = Config::default();
    c.validate().unwrap();
    c.operation = InitialOperation::Restore;
    assert!(c.validate().is_err());
    assert!(
        serde_json::from_value::<Config>(json!({"arbitrary_command":"never execute"})).is_err()
    );
    for path in ["", "/absolute", "../escape", "good/../../escape"] {
        assert!(live::relative(path).is_err());
    }
}
#[test]
fn ntfs_directory_helpers_reject_symlinks_and_preserve_foreign_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let other = dir.path().join("other");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&other).unwrap();
    fs::write(other.join("keep"), b"keep").unwrap();
    symlink(&other, root.join("escape")).unwrap();
    assert!(live::owned_directory(&root, "escape/nested").is_err());
    assert!(!other.join("nested").exists());
    assert_eq!(fs::read(other.join("keep")).unwrap(), b"keep");
    assert!(live::owned_directory(&root, "backups/run")
        .unwrap()
        .is_dir());
}
#[test]
fn ram_log_rotation_is_bounded_and_export_is_new_only() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ram");
    let diag = Diagnostics::new(&root, 64 << 10, "test".into()).unwrap();
    for i in 0..10000 {
        diag.record("test", json!({"index":i,"payload":"x".repeat(1000)}));
    }
    let out = dir.path().join("export");
    diag.export(&out).unwrap();
    let mut bytes = 0;
    let mut files = 0;
    for entry in fs::read_dir(&out).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name().to_string_lossy().starts_with("events-") {
            let data = fs::read(entry.path()).unwrap();
            bytes += data.len();
            files += 1;
            for line in data.split(|b| *b == b'\n').filter(|s| !s.is_empty()) {
                serde_json::from_slice::<serde_json::Value>(line).unwrap();
            }
        }
    }
    assert!(files <= 4 && bytes <= 64 << 10);
    let marker = out.join("keep");
    fs::write(&marker, b"keep").unwrap();
    assert!(diag.export(&out).is_err());
    assert_eq!(fs::read(marker).unwrap(), b"keep");
    assert!(diag.snapshot().logs_persisted);
    diag.stop();
}
#[test]
fn continuous_logs_backfill_startup_and_keep_full_history_without_export_commands() {
    let dir = tempfile::tempdir().unwrap();
    let ram = dir.path().join("ram");
    let ntfs = dir.path().join("ntfs-session");
    let diag = Diagnostics::new(&ram, 64 << 10, "test".into()).unwrap();
    diag.record("startup", json!({"before_mount":true}));
    diag.begin_persistent(&ntfs, 0).unwrap();
    assert!(diag.snapshot().logs_persisted);
    for batch in 0..20 {
        for item in 0..10 {
            diag.record(
                "during_backup",
                json!({"index":batch*10+item,"payload":"x".repeat(1000)}),
            );
        }
        diag.flush().unwrap();
    }
    let read = |root: &Path| {
        let mut paths = fs::read_dir(root)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("events-")
            })
            .collect::<Vec<_>>();
        paths.sort();
        paths
            .iter()
            .flat_map(|p| {
                fs::read_to_string(p)
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
    };
    let records = read(&ntfs);
    assert_eq!(records.len(), 201);
    assert_eq!(records[0]["kind"], "startup");
    for (index, record) in records[1..].iter().enumerate() {
        assert_eq!(record["value"]["index"], index);
    }
    assert!(read(&ram).len() < 201); // RAM rotates; NTFS retains the whole session.
    assert!(diag
        .begin_persistent(&dir.path().join("duplicate"), 0)
        .is_err());
    assert!(!dir.path().join("duplicate").exists());
    diag.close_persistent().unwrap();
    let info: serde_json::Value = io::read_json(&ntfs.join("log-info.json")).unwrap();
    assert_eq!(info["mode"], "continuous");
    assert_eq!(info["closed"], true);
    assert_eq!(info["sync_interval_ms"], 0);
    assert_eq!(info["dropped_records"], 0);
    assert!(ntfs.join("probe-final.json").is_file());
    diag.record("after_unmount", json!({"ram_only":true}));
    diag.flush().unwrap();
    assert_eq!(read(&ntfs).len(), 201);
    assert_eq!(read(&ram).last().unwrap()["kind"], "after_unmount");
    diag.stop();
}
#[test]
fn continuous_logs_preserve_an_unthrottled_burst_larger_than_the_queue() {
    let dir = tempfile::tempdir().unwrap();
    let ram = dir.path().join("ram");
    let ntfs = dir.path().join("ntfs-session");
    let diag = Diagnostics::new(&ram, 64 << 10, "burst".into()).unwrap();
    diag.begin_persistent(&ntfs, 0).unwrap();
    // No intermediate flush or sleep: the collector must sync each record while
    // the producer emits substantially more records than can wait in memory.
    for index in 0..4096 {
        diag.record("burst", json!({"index":index,"payload":"x".repeat(1000)}));
    }
    diag.close_persistent().unwrap();
    let mut paths = fs::read_dir(&ntfs)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("events-")
        })
        .collect::<Vec<_>>();
    paths.sort();
    let mut count = 0;
    for path in paths {
        for line in fs::read_to_string(path).unwrap().lines() {
            let record: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(record["kind"], "burst");
            assert_eq!(
                record["value"]["index"], count,
                "missing or reordered log record"
            );
            count += 1;
        }
    }
    assert_eq!(count, 4096);
    let info: serde_json::Value = io::read_json(&ntfs.join("log-info.json")).unwrap();
    assert_eq!(info["dropped_records"], 0);
    assert_eq!(info["closed"], true);
    assert_eq!(diag.snapshot().log_dropped, 0);
    let ram_bytes: u64 = fs::read_dir(&ram)
        .unwrap()
        .map(|entry| entry.unwrap().metadata().unwrap().len())
        .sum();
    assert!(ram_bytes <= 64 << 10);
    diag.stop();
}
#[test]
fn failed_log_attachment_preserves_existing_files_and_ram_logging() {
    let dir = tempfile::tempdir().unwrap();
    let existing = dir.path().join("existing");
    fs::create_dir(&existing).unwrap();
    fs::write(existing.join("keep"), b"foreign").unwrap();
    let diag = Diagnostics::new(&dir.path().join("ram"), 64 << 10, "test".into()).unwrap();
    assert!(diag.begin_persistent(&existing, 0).is_err());
    assert!(!diag.snapshot().logs_persisted);
    assert!(diag
        .begin_persistent(&dir.path().join("bad-interval"), 60001)
        .is_err());
    assert!(!dir.path().join("bad-interval").exists());
    diag.record("retained", json!({}));
    diag.export(&dir.path().join("recovery")).unwrap();
    assert_eq!(fs::read(existing.join("keep")).unwrap(), b"foreign");
    diag.stop();
}
#[test]
fn persistent_write_failure_is_reported_without_overwriting_foreign_files() {
    let dir = tempfile::tempdir().unwrap();
    let ntfs = dir.path().join("ntfs");
    let diag = Diagnostics::new(&dir.path().join("ram"), 64 << 10, "test".into()).unwrap();
    diag.begin_persistent(&ntfs, 0).unwrap();
    let foreign = dir.path().join("foreign");
    fs::write(&foreign, b"keep").unwrap();
    symlink(&foreign, ntfs.join("events-000001.jsonl")).unwrap();
    for item in 0..30 {
        diag.record("fill", json!({"index":item,"payload":"x".repeat(1000)}));
    }
    assert!(diag.flush().is_err());
    for _ in 0..100 {
        if diag.check().is_err() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(diag.check().is_err());
    diag.record("failure_retained_in_ram", json!({"failed":true}));
    assert!(String::from_utf8(diag.emergency_tail())
        .unwrap()
        .contains("failure_retained_in_ram"));
    assert_eq!(fs::read(foreign).unwrap(), b"keep");
    diag.stop();
}
#[test]
fn embedded_settings_are_used_unless_an_explicit_external_file_exists() {
    let dir = tempfile::tempdir().unwrap();
    let boot = dir.path().join("boot");
    fs::create_dir(&boot).unwrap();
    let builtin = dir.path().join("embedded.json");
    fs::write(&builtin, serde_json::to_vec(&Config::default()).unwrap()).unwrap();
    fs::set_permissions(&builtin, fs::Permissions::from_mode(0o600)).unwrap();
    let c = live::load_config(&boot, &builtin).unwrap();
    assert_eq!(c.log_policy, live::LogPolicy::Always);
    assert_eq!(c.log_sync_interval_ms, 0);
    let mut memory_only = Config::default();
    memory_only.log_policy = live::LogPolicy::Never;
    memory_only.log_sync_interval_ms = 1000;
    fs::write(
        boot.join("perfect-backup.json"),
        serde_json::to_vec(&memory_only).unwrap(),
    )
    .unwrap();
    fs::set_permissions(
        boot.join("perfect-backup.json"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let c = live::load_config(&boot, &builtin).unwrap();
    assert_eq!(c.log_policy, live::LogPolicy::Never);
    assert_eq!(c.log_sync_interval_ms, 1000);
    fs::write(boot.join("perfect-backup.json"), b"invalid override").unwrap();
    assert!(live::load_config(&boot, &builtin).is_err());
    fs::remove_file(boot.join("perfect-backup.json")).unwrap();
    symlink(&builtin, boot.join("perfect-backup.json")).unwrap();
    assert!(live::load_config(&boot, &builtin).is_err());
    memory_only.log_sync_interval_ms = 60001;
    assert!(memory_only.validate().is_err());
}
#[test]
fn log_cap_and_host_execution_guards_are_enforced() {
    let dir = tempfile::tempdir().unwrap();
    assert!(Diagnostics::new(&dir.path().join("too-small"), 1024, "test".into()).is_err());
    assert!(live::guard().is_err()); // Never invoke mount, device-write, or poweroff on the development host.
    let f = dir.path().join("file");
    fs::write(&f, b"plain").unwrap();
    assert!(io::identity(&io::open_regular(&f, false).unwrap())
        .unwrap()
        .block_device
        .is_none());
}
#[test]
fn leaving_menu_clears_countdown_and_plan_selection_clears_probe_detail() {
    use crispawn_engine::job::{JobEvent, JobEvents};
    let dir = tempfile::tempdir().unwrap();
    let diag = Diagnostics::new(&dir.path().join("ram"), 64 << 10, "test".into()).unwrap();
    {
        let mut status = diag.status.lock().unwrap();
        status.phase = "MENU".into();
        status.countdown_seconds = Some(30);
    }
    diag.emit(JobEvent::State {
        phase: "PREPARING".into(),
    });
    assert!(diag.snapshot().countdown_seconds.is_none());
    diag.emit(JobEvent::ProbeTrial {
        index: 1,
        total: 5,
        candidate_id: "candidate".into(),
        elapsed_seconds: None,
    });
    diag.emit(JobEvent::PlanSelected {
        plan: crispawn_core::plan::Plan::default(),
        verify_workers: 1,
        total_bytes: "134217728".into(),
        eta_seconds: Some(10),
        selection: "measured".into(),
    });
    assert_eq!(diag.snapshot().source_bytes, 134217728);
    assert!(diag.snapshot().detail.is_empty());
    diag.stop();
}
#[test]
fn gui_probe_requires_a_recent_valid_observer_heartbeat() {
    assert!(live::ui_fresh(
        &json!({"schema_version":1,"observed_monotonic_ms":1000}),
        2000
    ));
    assert!(!live::ui_fresh(
        &json!({"schema_version":1,"observed_monotonic_ms":1000}),
        7000
    ));
    assert!(!live::ui_fresh(
        &json!({"schema_version":1,"observed_monotonic_ms":9000}),
        2000
    ));
    assert!(!live::ui_fresh(
        &json!({"schema_version":2,"observed_monotonic_ms":1000}),
        2000
    ));
    assert!(!live::ui_fresh(&json!({"schema_version":1}), 2000));
}
#[test]
fn secure_boot_requirement_rejects_disabled_validation_and_unenforced_kernel() {
    use crispawn_core::live::SecureBootObservation;
    let valid = SecureBootObservation {
        enabled: Some(true),
        setup_mode: Some(false),
        shim_validation_disabled: false,
        lockdown: "none [integrity] confidentiality".into(),
    };
    live::require_secure_boot(&valid).unwrap();
    for s in [
        SecureBootObservation {
            enabled: Some(false),
            ..valid.clone()
        },
        SecureBootObservation {
            enabled: None,
            ..valid.clone()
        },
        SecureBootObservation {
            setup_mode: Some(true),
            ..valid.clone()
        },
        SecureBootObservation {
            setup_mode: None,
            ..valid.clone()
        },
        SecureBootObservation {
            shim_validation_disabled: true,
            ..valid.clone()
        },
        SecureBootObservation {
            lockdown: "[none] integrity confidentiality".into(),
            ..valid.clone()
        },
    ] {
        assert_eq!(
            live::require_secure_boot(&s).unwrap_err().code,
            "SECURE_BOOT_NOT_ENFORCED"
        );
    }
}
#[test]
fn secure_boot_observation_reads_uefi_values_without_modifying_them() {
    let dir = tempfile::tempdir().unwrap();
    let vars = dir.path().join("firmware/efi/efivars");
    fs::create_dir_all(&vars).unwrap();
    fs::create_dir_all(dir.path().join("kernel/security")).unwrap();
    let secure = vars.join("SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c");
    fs::write(&secure, [7, 0, 0, 0, 1]).unwrap();
    fs::write(
        vars.join("SetupMode-8be4df61-93ca-11d2-aa0d-00e098032b8c"),
        [7, 0, 0, 0, 0],
    )
    .unwrap();
    fs::write(
        dir.path().join("kernel/security/lockdown"),
        "none [integrity] confidentiality\n",
    )
    .unwrap();
    let s = live::observe_secure_boot(dir.path());
    live::require_secure_boot(&s).unwrap();
    assert_eq!(fs::read(&secure).unwrap(), [7, 0, 0, 0, 1]);
    fs::write(
        vars.join("MokSBStateRT-605dab50-e046-4300-abb6-3dd810dd8b23"),
        [7, 0, 0, 0, 1],
    )
    .unwrap();
    assert!(live::require_secure_boot(&live::observe_secure_boot(dir.path())).is_err());
}
#[test]
fn ntfs_boot_uuid_must_be_unambiguous_and_explicit() {
    assert_eq!(
        live::boot_uuid_from_cmdline("console=tty0 pb.boot_uuid=abcdef0011223344").unwrap(),
        "ABCDEF0011223344"
    );
    for s in [
        "",
        "pb.boot_uuid=abc",
        "pb.boot_uuid=ABCDEF001122334G",
        "pb.boot_uuid=ABCDEF0011223344 pb.boot_uuid=ABCDEF0011223344",
    ] {
        assert!(live::boot_uuid_from_cmdline(s).is_err());
    }
}

#[test]
fn optical_boot_label_rejects_ambiguity_and_usb_identity_mixtures() {
    let label = "CRISPAWN_0123456789ABCDEF0123";
    assert_eq!(
        live::optical_label_from_cmdline(&format!("quiet pb.optical_label={label}"))
            .unwrap()
            .as_deref(),
        Some(label)
    );
    assert_eq!(
        live::optical_label_from_cmdline("pb.boot_uuid=0123456789ABCDEF").unwrap(),
        None
    );
    for args in [
        format!("pb.optical_label={label} pb.optical_label={label}"),
        format!("pb.optical_label={label} pb.boot_uuid=0123456789ABCDEF"),
        format!("pb.optical_label={label} pb.data_uuid=0123456789ABCDEF"),
        "pb.optical_label=CRISPAWN_0123456789abcdef0123".into(),
        "pb.optical_label=../another-disk".into(),
        "pb.optical_label=".into(),
    ] {
        assert_eq!(
            live::optical_label_from_cmdline(&args).unwrap_err().code,
            "INVALID_OPTICAL_BOOT"
        );
    }
}

#[test]
fn optical_identity_uses_the_primary_descriptor_volume_label() {
    let mut descriptor = [0u8; 2048];
    descriptor[..7].copy_from_slice(b"\x01CD001\x01");
    descriptor[40..72].fill(b' ');
    descriptor[40..69].copy_from_slice(b"CRISPAWN_0123456789ABCDEF0123");
    assert_eq!(
        live::optical_volume_label(&descriptor).unwrap(),
        "CRISPAWN_0123456789ABCDEF0123"
    );
    assert!(live::optical_volume_label(&descriptor[..512]).is_err());
    descriptor[0] = 0; // An El Torito boot record is not a primary descriptor.
    assert!(live::optical_volume_label(&descriptor).is_err());
    descriptor[0] = 1;
    descriptor[40] = 0xff;
    assert!(live::optical_volume_label(&descriptor).is_err());
}
#[test]
fn split_fat_boot_and_ntfs_data_require_valid_ids_and_one_physical_media() {
    assert_eq!(
        live::boot_ids("pb.boot_uuid=abcd-1234 pb.data_uuid=abcdef0011223344").unwrap(),
        ("ABCD-1234".into(), Some("ABCDEF0011223344".into()))
    );
    for args in [
        "pb.boot_uuid=ABCD-1234",
        "pb.boot_uuid=ABCD-1234 pb.data_uuid=short",
        "pb.boot_uuid=ABCDEF0011223344 pb.data_uuid=ABCDEF0011223344",
        "pb.boot_uuid=ABCD-1234 pb.data_uuid=ABCDEF0011223344 pb.data_uuid=ABCDEF0011223344",
    ] {
        assert!(live::boot_ids(args).is_err());
    }
    let mut sector = [0u8; 512];
    sector[82..90].copy_from_slice(b"FAT32   ");
    sector[67..71].copy_from_slice(&0xabcd1234u32.to_le_bytes());
    sector[510..512].copy_from_slice(&[0x55, 0xaa]);
    assert_eq!(live::fat_uuid(&sector).unwrap(), "ABCD-1234");
    assert!(live::ntfs_uuid(&sector).is_err());
    let boot = disk("sda1", false, true, "sda");
    let mut data = disk("sda2", false, true, "sda");
    data.number = "8:2".into();
    live::require_same_media(&boot, &data).unwrap();
    assert!(live::require_same_media(&boot, &boot).is_err());
    data.whole_name = "sdb".into();
    assert!(live::require_same_media(&boot, &data).is_err());
}
#[test]
fn split_boot_configuration_preserves_error_log_policy_in_ram() {
    use crispawn_engine::live::LogPolicy;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ram");
    let data = dir.path().join("ntfs");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&data).unwrap();
    let mut config = Config::default();
    config.log_policy = LogPolicy::Never;
    fs::write(
        root.join("live-config.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    assert_eq!(live::recovery_log_policy(&root, &data), LogPolicy::Never);
    fs::write(
        data.join("perfect-backup.json"),
        serde_json::to_vec(&Config::default()).unwrap(),
    )
    .unwrap();
    assert_eq!(live::recovery_log_policy(&root, &data), LogPolicy::Never);
    fs::remove_file(root.join("live-config.json")).unwrap();
    assert_eq!(live::recovery_log_policy(&root, &data), LogPolicy::Always);
}
