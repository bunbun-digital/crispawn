// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
use perfect_backup_core::{
    live::CommandAction,
    presentation::{DiskState, DiskView, Page},
    session::Session,
};
use perfect_backup_engine::{
    job::{Controller, NoJobEvents, Operation},
    job_config::JobConfig,
    live::{self, InitialOperation, LogPolicy},
    live_config::{self, Interface, Settings, Startup, Volume},
};
use serde_json::{json, Value};
use std::{
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    path::PathBuf,
    process::Command,
    sync::Arc,
};

#[test]
fn failure_actions_share_completion_delay_and_reject_unknown_settings() {
    assert!(parse(&baseline()).poweroff_on_failure);
    let mut v = baseline();
    v["completion"] = json!({"action":"poweroff","delay_seconds":17});
    v["failure"] = json!({"action":"stay"});
    let configured = parse(&v);
    assert!(!configured.poweroff_on_failure);
    assert_eq!(configured.completion_seconds, 17);
    v["failure"] = json!({"action":"poweroff"});
    assert!(parse(&v).poweroff_on_failure);
    v["completion"] = json!({"action":"stay"});
    assert_eq!(parse(&v).completion_seconds, 5);
    for value in [
        Value::Null,
        json!({"action":"unknown"}),
        json!({"action":"stay","delay_seconds":1}),
    ] {
        v["failure"] = value;
        rejected(&v);
    }
}

fn example(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../../config/crispawn/examples")
        .join(name);
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}
fn baseline() -> Value {
    example("live-v2-usb.json")
}
fn parse(value: &Value) -> Settings {
    live_config::parse(&serde_json::to_vec(value).unwrap()).unwrap()
}
fn rejected(value: &Value) {
    assert!(
        live_config::parse(&serde_json::to_vec(value).unwrap()).is_err(),
        "unexpected acceptance"
    );
}
fn session(settings: &Settings) -> Session {
    let mut session = Session::new(
        vec![DiskView {
            stable_id: "serial:DEMO:ns:".into(),
            name: "demo".into(),
            bytes: 1 << 20,
            selected: false,
            state: DiskState::Waiting,
        }],
        vec![],
        &settings.source_ids,
        10,
        settings.menu_seconds,
    )
    .unwrap();
    settings.initialize_session(&mut session).unwrap();
    session.prepared(0, Ok(())).unwrap();
    session
}
#[test]
fn v2_usb_defaults_resolve_to_the_existing_v1_golden() {
    let s = parse(&baseline());
    assert_eq!(s.version, 2);
    assert_eq!(s.interface, Interface::Gui);
    assert_eq!(s.startup, Startup::AutoCopy { delay_seconds: 30 });
    assert_eq!(s.volume, Volume::BootDiskData {});
    assert_eq!(
        serde_json::to_value(&s.config).unwrap(),
        serde_json::to_value(parse(&example("live-v1-usb.generated.json")).config).unwrap()
    );
    assert!(matches!(
        s.tuning.mode,
        perfect_backup_engine::job_config::TuningMode::Auto
    ));
}
#[test]
fn each_runtime_setting_is_passed_without_silent_defaults() {
    let mut v = baseline();
    v["sources"] = json!({"mode":"selected_ids","ids":["serial:DEMO:ns:","wwid:DEMO"]});
    v["destination"]["backup_directory"] = json!("user-copies/日本語");
    v["destination"]["volume"] = json!({"mode":"uuid","uuid":"0123456789ABCDEF"});
    v["startup"]["delay_seconds"] = json!(7);
    v["completion"] = json!({"action":"stay"});
    v["ui"]["language"] = json!("en");
    v["logging"] = json!({"policy":"on_error","sync_interval_ms":1234});
    v["processing"]["initial_profile"] = json!("incompressible");
    v["processing"]["io_policy"] = json!({"no_progress_seconds":42,"failure_grace_seconds":7,"shutdown_grace_seconds":21,"transient_retries":5,"retry_delay_ms":123});
    v["diagnostics"]["probe_interval_ms"] = json!(4321);
    let s = parse(&v);
    assert_eq!(s.source_ids, ["serial:DEMO:ns:", "wwid:DEMO"]);
    assert_eq!(s.backup_directory, "user-copies/日本語");
    assert_eq!(
        s.volume,
        Volume::Uuid {
            uuid: "0123456789ABCDEF".into()
        }
    );
    assert_eq!(s.menu_seconds, 7);
    assert!(!s.poweroff_on_success);
    // Success stays on screen; an error poweroff retains a confirmation delay.
    assert_eq!(s.completion_seconds, 5);
    assert_eq!(s.language, perfect_backup_core::presentation::Language::En);
    assert_eq!(s.log_policy, LogPolicy::OnError);
    assert_eq!(s.log_sync_interval_ms, 1234);
    assert_eq!(s.probe_interval_ms, 4321);
    assert_eq!(s.tuning.io_policy.no_progress_seconds, 42);
    assert_eq!(s.tuning.io_policy.failure_grace_seconds, 7);
    assert_eq!(s.tuning.io_policy.shutdown_grace_seconds, 21);
    assert_eq!(s.tuning.io_policy.transient_retries, 5);
    assert_eq!(s.tuning.io_policy.retry_delay_ms, 123);
    assert_eq!(
        serde_json::to_value(s.tuning.profile).unwrap(),
        "incompressible"
    );
}
#[test]
fn all_supported_shared_examples_are_accepted_but_projects_are_not_runtime() {
    for name in [
        "builder-usb.json",
        "builder-iso.json",
        "builder-menu-v2.json",
        "builder-restore-select-v2.json",
    ] {
        let project = example(name);
        parse(&project["runtime"]);
        rejected(&project);
    }
    for name in [
        "credentials.example.json",
        "capabilities-legacy-v1.fixture.json",
        "capabilities-future-v2.fixture.json",
    ] {
        rejected(&example(name));
    }
}
#[test]
fn menu_and_restore_selection_never_start_an_unattended_job() {
    for (name, page) in [
        ("builder-menu-v2.json", Page::CopySelection),
        ("builder-restore-select-v2.json", Page::RestoreSelection),
    ] {
        let s = parse(&example(name)["runtime"]);
        let mut session = session(&s);
        assert_eq!(session.view.page, page);
        assert_eq!(session.countdown(10), None);
        assert!(session.tick(3_600_000).unwrap().is_none());
        if page == Page::CopySelection {
            session.action(CommandAction::StartCopy).unwrap();
            assert!(session.tick(3_600_001).unwrap().is_some());
        } else {
            assert!(session.action(CommandAction::StartRestore).is_err());
        }
    }
}
#[test]
fn auto_copy_uses_the_configured_deadline() {
    let mut v = baseline();
    v["startup"]["delay_seconds"] = json!(7);
    let mut session = session(&parse(&v));
    assert!(session.tick(7009).unwrap().is_none());
    assert_eq!(session.countdown(7009), Some(1));
    assert!(session.tick(7010).unwrap().is_some());
}
#[test]
fn console_and_headless_are_distinct_and_cannot_hide_user_input_waits() {
    let mut v = baseline();
    for mode in ["console", "headless"] {
        v["ui"]["mode"] = json!(mode);
        rejected(&v);
        v["startup"]["delay_seconds"] = json!(0);
        let s = parse(&v);
        assert!(!s.gui);
        assert_eq!(
            s.interface,
            if mode == "console" {
                Interface::Console
            } else {
                Interface::Headless
            }
        );
        assert!(session(&s).tick(10).unwrap().is_some());
        v["startup"] = json!({"mode":"menu"});
        if mode == "console" {
            assert!(session(&parse(&v)).tick(1_000_000).unwrap().is_none());
        } else {
            rejected(&v);
        }
        v["startup"] = json!({"mode":"auto_copy","delay_seconds":30});
    }
}
#[test]
fn no_required_v2_field_or_nested_field_is_defaulted() {
    let original = baseline();
    for key in original.as_object().unwrap().keys() {
        let mut v = original.clone();
        v.as_object_mut().unwrap().remove(key);
        rejected(&v);
    }
    for object in [
        "sources",
        "destination",
        "startup",
        "completion",
        "ui",
        "logging",
        "processing",
        "network",
        "diagnostics",
    ] {
        for key in original[object].as_object().unwrap().keys() {
            let mut v = original.clone();
            v[object].as_object_mut().unwrap().remove(key);
            rejected(&v);
        }
    }
    for key in original["processing"]["io_policy"]
        .as_object()
        .unwrap()
        .keys()
    {
        let mut v = original.clone();
        v["processing"]["io_policy"]
            .as_object_mut()
            .unwrap()
            .remove(key);
        rejected(&v);
    }
}
#[test]
fn unknown_fields_and_null_values_are_rejected_at_each_supported_level() {
    for object in [
        "",
        "sources",
        "destination",
        "startup",
        "completion",
        "ui",
        "logging",
        "processing",
        "network",
        "diagnostics",
    ] {
        let mut v = baseline();
        let target = if object.is_empty() {
            &mut v
        } else {
            &mut v[object]
        };
        target["execute"] = json!("NEVER_EXECUTE_THIS");
        rejected(&v);
        let mut v = baseline();
        if object.is_empty() {
            v["kind"] = Value::Null;
        } else {
            v[object] = Value::Null;
        }
        rejected(&v);
    }
}
#[test]
fn limits_wrong_types_and_forbidden_syntax_fail_without_input_values_in_errors() {
    for (object, key, values) in [
        (
            "startup",
            "delay_seconds",
            vec![json!(-1), json!(301), json!(1.0), json!(true), json!("5")],
        ),
        ("completion", "delay_seconds", vec![json!(-1), json!(61)]),
        ("logging", "sync_interval_ms", vec![json!(-1), json!(60001)]),
        (
            "diagnostics",
            "probe_interval_ms",
            vec![json!(99), json!(5001)],
        ),
    ] {
        for value in values {
            let mut v = baseline();
            v[object][key] = value;
            rejected(&v);
        }
    }
    for (key, value) in [
        ("no_progress_seconds", 3601),
        ("failure_grace_seconds", 121),
        ("shutdown_grace_seconds", 121),
        ("transient_retries", 9),
        ("retry_delay_ms", 2001),
    ] {
        let mut v = baseline();
        v["processing"]["io_policy"][key] = json!(value);
        rejected(&v);
        v["processing"]["io_policy"][key] = json!(-1);
        rejected(&v);
    }
    let bytes = serde_json::to_vec(&baseline()).unwrap();
    let text = String::from_utf8(bytes.clone()).unwrap();
    let mut inputs = vec![
        vec![],
        vec![b' '; 65537],
        b"\xef\xbb\xbf{}".to_vec(),
        b"{\"kind\":\"SECRET_VALUE\"}".to_vec(),
        b"{\"schema_version\":1,\"schema_version\":1}".to_vec(),
        b"{\"menu_seconds\":1e0}".to_vec(),
        b"{\"menu_seconds\":NaN}".to_vec(),
        b"{\"source_id\":\"\\u0000\"}".to_vec(),
        b"{\"source_id\":\"\\ud800\"}".to_vec(),
    ];
    inputs.push(
        text.replacen("\"schema_version\":2", "\"schema_version\":2.0", 1)
            .into_bytes(),
    );
    inputs.push(format!("{}{}", "[".repeat(34), "]".repeat(34)).into_bytes());
    inputs.push([bytes, b" {}".to_vec()].concat());
    for input in inputs {
        let error = live_config::parse(&input).unwrap_err();
        assert!(!error.to_string().contains("SECRET_VALUE"));
    }
}
#[test]
fn source_ids_paths_and_volume_serials_follow_the_portable_contract() {
    for id in [
        "/dev/sda",
        "serial:demo",
        "serial:demo:ns:01",
        "wwid:",
        "wwid: demo",
        "serial:demo:ns:x",
        "serial:demo:ns:\n",
    ] {
        let mut v = baseline();
        v["sources"] = json!({"mode":"selected_ids","ids":[id]});
        rejected(&v);
    }
    for ids in [vec![], vec!["wwid:demo"; 2], vec!["wwid:demo"; 65]] {
        let mut v = baseline();
        v["sources"] = json!({"mode":"selected_ids","ids":ids});
        rejected(&v);
    }
    for path in [
        "",
        "/absolute",
        "a//b",
        "a/../b",
        "a/./b",
        "a/",
        "a\\b",
        "CON",
        "a/nul.txt",
        "com1",
        "a/b.",
        "a/b ",
        "C:/x",
        "a\n",
    ] {
        let mut v = baseline();
        v["destination"]["backup_directory"] = json!(path);
        rejected(&v);
    }
    for uuid in [
        "0123456789abcdef",
        "01234567-89ABCDEF",
        "0123456789ABCDEG",
        "0123456789ABCDEF\n",
    ] {
        let mut v = baseline();
        v["destination"]["volume"] = json!({"mode":"uuid","uuid":uuid});
        rejected(&v);
    }
}
#[test]
fn future_backends_fail_closed_without_credential_read_or_network_operations() {
    for name in ["builder-smb-v2.json", "builder-s3-v2.json"] {
        let error = live_config::parse(&serde_json::to_vec(&example(name)["runtime"]).unwrap())
            .unwrap_err();
        assert_eq!(error.code, "UNSUPPORTED_CAPABILITY");
    }
    let mut v = baseline();
    v["network"] = json!({"mode":"auto","wifi_profiles":[]});
    assert_eq!(
        live_config::parse(&serde_json::to_vec(&v).unwrap())
            .unwrap_err()
            .code,
        "UNSUPPORTED_CAPABILITY"
    );
}
#[test]
fn legacy_defaults_fixed_tuning_and_explicit_restore_are_preserved() {
    let empty = live_config::parse(b"{}").unwrap();
    assert_eq!(empty.version, 1);
    assert_eq!(empty.menu_seconds, 30);
    let fixed = live_config::parse(br#"{"tuning":{"mode":"fixed"},"gui":false}"#).unwrap();
    assert_eq!(fixed.interface, Interface::Console);
    assert!(matches!(
        fixed.tuning.mode,
        perfect_backup_engine::job_config::TuningMode::Fixed
    ));
    let restore=live_config::parse(br#"{"operation":"restore","gui":false,"restore":{"archive":"backups/a","target_id":"legacy-target"}}"#).unwrap();
    assert_eq!(restore.operation, InitialOperation::Restore);
    assert_eq!(restore.restore.as_ref().unwrap().target_id, "legacy-target");
}
#[test]
fn file_ownership_links_and_external_precedence_are_checked() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let builtin = root.join("builtin.json");
    let external = root.join("perfect-backup.json");
    fs::write(&builtin, serde_json::to_vec(&baseline()).unwrap()).unwrap();
    fs::set_permissions(&builtin, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(live::load_config(root, &builtin).unwrap().version, 2);
    fs::write(&external, b"{}").unwrap();
    fs::set_permissions(&external, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(live::load_config(root, &builtin).unwrap().version, 1);
    fs::write(&external, b"invalid").unwrap();
    assert!(live::load_config(root, &builtin).is_err());
    fs::remove_file(&external).unwrap();
    symlink(&builtin, &external).unwrap();
    assert!(live::load_config(root, &builtin).is_err());
    fs::remove_file(&external).unwrap();
    fs::hard_link(&builtin, &external).unwrap();
    assert!(live::load_config(root, &builtin).is_err());
    fs::remove_file(&external).unwrap();
    fs::set_permissions(&builtin, fs::Permissions::from_mode(0o666)).unwrap();
    assert_eq!(
        live_config::read_private(&builtin).unwrap_err().code,
        "UNSAFE_CONFIG_FILE"
    );
}
#[test]
fn production_cli_uses_the_same_reader_without_liveos_or_device_access() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("runtime.json");
    fs::write(&config, serde_json::to_vec(&baseline()).unwrap()).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    let binary = env!("CARGO_BIN_EXE_perfect-backup-live");
    let result = Command::new(binary)
        .args(["describe-config", config.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(result.status.success(), "{:?}", result.stderr);
    let result: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(result["runtime_version"], 2);
    assert_eq!(result["disk_or_network_io"], false);
    let result = Command::new(binary)
        .args(["gui-enabled", config.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(result.stdout, b"true\n");
    fs::write(&config, b"{\"kind\":\"SECRET_NO_LOG\"}").unwrap();
    let result = Command::new(binary)
        .args(["validate-config", config.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(!String::from_utf8_lossy(&result.stderr).contains("SECRET_NO_LOG"));
}
#[test]
fn v2_tuning_drives_a_real_regular_file_copy_and_verified_restore() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let mut v = baseline();
    v["ui"]["mode"] = json!("headless");
    v["startup"]["delay_seconds"] = json!(0);
    v["processing"]["initial_profile"] = json!("incompressible");
    let settings = parse(&v);
    let source = [
        b"CRISPAWN raw disk data\n".repeat(4096),
        vec![0; 131072],
        vec![0x76; 8192],
    ]
    .concat();
    fs::write(root.join("source.img"), &source).unwrap();
    let config = JobConfig {
        schema_version: 1,
        source_file: Some(root.join("source.img")),
        source_device: None,
        archive: root.join("archive"),
        scratch_dir: root.into(),
        restore_target_file: Some(root.join("restored.img")),
        restore_target_device: None,
        report_file: None,
        tuning: settings.tuning.clone(),
    };
    let controller = Controller::new(Arc::new(NoJobEvents)).unwrap();
    let copied = controller.run(&config, Operation::Backup).unwrap();
    assert_eq!(copied.status, "complete");
    let restored = Controller::new(Arc::new(NoJobEvents))
        .unwrap()
        .run(&config, Operation::Restore)
        .unwrap();
    assert_eq!(restored.status, "complete");
    assert_eq!(fs::read(root.join("restored.img")).unwrap(), source);
}

#[test]
fn explicit_usb_destination_never_makes_boot_or_destination_a_source() {
    use perfect_backup_engine::device::Device;
    let disk = |name: &str, usb: bool, partition: bool, whole: &str| Device {
        name: name.into(),
        path: PathBuf::from(format!("/dev/{name}")),
        number: format!("8:{}", if partition { 1 } else { 0 }),
        whole_name: whole.into(),
        partition,
        usb,
        removable: false,
        size_bytes: 1 << 20,
        identity: Some(perfect_backup_core::types::BlockIdentity {
            stable_id: format!("serial:{name}:ns:"),
            logical_sector_bytes: 512,
            physical_sector_bytes: 4096,
        }),
    };
    let boot = disk("sda1", false, true, "sda");
    let usb = disk("sdb1", true, true, "sdb");
    let mut v = baseline();
    v["destination"]["volume"] = json!({"mode":"uuid","uuid":"0123456789ABCDEF"});
    let settings = parse(&v);
    live::require_destination(&settings, &boot, &usb).unwrap();
    let internal = disk("sdc1", false, true, "sdc");
    assert_eq!(
        live::require_destination(&settings, &boot, &internal)
            .unwrap_err()
            .code,
        "DESTINATION_NOT_USB"
    );
    let devices = vec![
        disk("sda", false, false, "sda"),
        disk("sdb", true, false, "sdb"),
        disk("nvme0n1", false, false, "nvme0n1"),
    ];
    let eligible = live::source_candidates(&devices, "sda", "sdb");
    assert_eq!(eligible.len(), 1);
    assert_eq!(eligible[0].name, "nvme0n1");
    assert!(live::require_destination(&parse(&baseline()), &boot, &usb).is_err());
}

#[test]
fn destination_lookup_keeps_io_failures_distinct_from_configuration_errors() {
    use perfect_backup_engine::Error;
    let io = live::destination_error(Error::from(std::io::Error::from_raw_os_error(libc::EIO)));
    assert_eq!(io.raw_os_error(), Some(libc::EIO));
    assert_eq!(
        live::destination_error(Error::new("BOOT_VOLUME_NOT_FOUND", "absent")).code,
        "DESTINATION_NOT_FOUND"
    );
    assert_eq!(
        live::destination_error(Error::new("BOOT_VOLUME_AMBIGUOUS", "duplicate")).code,
        "DESTINATION_VOLUME_AMBIGUOUS"
    );
}
