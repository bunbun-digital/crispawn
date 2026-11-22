// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! Live adapter for the display-independent session reducer. The view never opens a disk.
use super::*;
use crate::job_config::JobConfig;
use perfect_backup_core::{
    presentation::{DiskState, DiskView, Mode, Page, SavedDisk, SavedSet},
    session::{Session, Start},
    types::Metadata,
};

fn archive_path(mount: &Path, relative_name: &str) -> Result<PathBuf> {
    relative(relative_name)?;
    let mut path = mount.to_path_buf();
    for part in Path::new(relative_name).components() {
        path.push(part);
        require(
            !fs::symlink_metadata(&path)?.file_type().is_symlink(),
            "UNSAFE_PATH",
            "archive path contains a symlink",
        )?;
    }
    require(path.is_dir(), "UNSAFE_PATH", "archive must be a directory")?;
    Ok(path)
}
fn saved_disk(mount: &Path, name: &str, label: &str) -> Result<SavedDisk> {
    let path = archive_path(mount, name)?;
    // Catalog reads indices, not bulk payload. Restore independently validates the archive.
    require(
        path.join("COMMITTED").is_file(),
        "UNCOMMITTED_ARCHIVE",
        "history contains only committed copies",
    )?;
    let meta: Metadata = io::read_json(&path.join("metadata.json"))?;
    let stable_id = meta
        .source
        .block_device
        .as_ref()
        .map(|d| d.stable_id.clone())
        .unwrap_or_default();
    Ok(SavedDisk {
        archive: name.into(),
        name: label.into(),
        stable_id,
        bytes: meta.source.bytes,
    })
}
fn timestamp(seconds: u64) -> String {
    let seconds = seconds.min(i64::MAX as u64) as libc::time_t;
    let mut value = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: gmtime_r writes one tm for a valid time_t, with no shared static buffer.
    if unsafe { libc::gmtime_r(&seconds, value.as_mut_ptr()) }.is_null() {
        return seconds.to_string();
    }
    let t = unsafe { value.assume_init() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
        t.tm_year + 1900,
        t.tm_mon + 1,
        t.tm_mday,
        t.tm_hour,
        t.tm_min,
        t.tm_sec
    )
}
fn read_saved_set(path: &Path) -> Result<SavedSet> {
    let mut set: SavedSet = io::read_json(path)?;
    for disk in &mut set.disks {
        relative(&disk.archive)?;
        // Builder moves the containing directory, preserving the session JSON.
        // Normalize only the exact old top-level component, in memory.
        if let Some(suffix) = disk.archive.strip_prefix("perfect-backup/") {
            disk.archive = format!("crispawn/{suffix}");
        }
    }
    Ok(set)
}
fn catalog(mount: &Path, config: &Config, diag: &Diagnostics) -> Result<Vec<SavedSet>> {
    let sessions = owned_directory(mount, "crispawn/sessions")?;
    let mut result = Vec::new();
    let mut known = std::collections::BTreeSet::new();
    for file in fs::read_dir(sessions)?.take(1025) {
        let file = file?;
        require(
            result.len() < 1024,
            "CATALOG_LIMIT",
            "too many saved sessions",
        )?;
        if !file.file_type()?.is_file()
            || file.path().extension().and_then(|v| v.to_str()) != Some("json")
        {
            continue;
        }
        let found = (|| -> Result<SavedSet> {
            let set = read_saved_set(&file.path())?;
            require(
                !set.disks.is_empty() && set.disks.len() <= 64,
                "INVALID_CATALOG",
                "invalid saved disk count",
            )?;
            for disk in &set.disks {
                let actual = saved_disk(mount, &disk.archive, &disk.name)?;
                require(
                    actual.bytes == disk.bytes && actual.stable_id == disk.stable_id,
                    "INVALID_CATALOG",
                    "saved session disagrees with disk metadata",
                )?;
            }
            Ok(set)
        })();
        match found {
            Ok(set) => {
                known.extend(set.disks.iter().map(|d| d.archive.clone()));
                result.push(set);
            }
            Err(e) => diag.record(
                "history_rejected",
                json!({"file":file.file_name(),"code":e.code}),
            ),
        }
    }
    // Earlier single-disk archives remain selectable after upgrading the UI.
    let archives = owned_directory(mount, &config.backup_directory)?;
    for entry in fs::read_dir(archives)?.take(2049) {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        require(
            result.len() < 1024,
            "CATALOG_LIMIT",
            "too many saved copies",
        )?;
        let relative_name = format!(
            "{}/{}",
            config.backup_directory,
            entry.file_name().to_string_lossy()
        );
        if known.contains(&relative_name) {
            continue;
        }
        if let Ok(disk) = saved_disk(mount, &relative_name, &entry.file_name().to_string_lossy()) {
            let meta: Metadata = io::read_json(&entry.path().join("metadata.json"))?;
            result.push(SavedSet {
                id: meta.backup_id,
                datetime: timestamp(meta.created_unix_seconds.parse().unwrap_or(0)),
                disks: vec![disk],
            });
        }
    }
    result.sort_by(|a, b| b.datetime.cmp(&a.datetime).then_with(|| a.id.cmp(&b.id)));
    require(
        io::json(&result)?.len() <= 512 * 1024,
        "CATALOG_LIMIT",
        "history exceeds RAM display budget",
    )?;
    Ok(result)
}
fn publish(diag: &Diagnostics, session: &Session, mount: &Mounted) -> Result<()> {
    let free = io::available_bytes(&mount.path)?;
    let mut s = diag.status.lock().unwrap_or_else(|e| e.into_inner());
    s.session = session.view.clone();
    s.session.destination_free_bytes = free;
    s.countdown_seconds = session.countdown(diag.ms());
    s.can_restore = !s.session.backups.is_empty();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use perfect_backup_core::{plan::Plan, types::NoEvents};
    use std::os::unix::fs::symlink;

    fn session(archive: &str, bytes: u64) -> SavedSet {
        SavedSet {
            id: "preserved-session-id".into(),
            datetime: "2026-11-22 01:02:03 UTC".into(),
            disks: vec![SavedDisk {
                archive: archive.into(),
                name: "Preserved source".into(),
                stable_id: String::new(),
                bytes,
            }],
        }
    }

    #[test]
    fn moved_session_is_selected_once_and_restores_without_rewriting_existing_data() {
        let temp = tempfile::tempdir().unwrap();
        let mount = temp.path().join("usb");
        let old = mount.join("perfect-backup");
        fs::create_dir_all(old.join("backups")).unwrap();
        fs::create_dir(old.join("sessions")).unwrap();
        let source = temp.path().join("source");
        let data = b"preserved backup payload\n".repeat(1024);
        fs::write(&source, &data).unwrap();
        crate::backup::backup(
            &source,
            &old.join("backups/existing"),
            Plan {
                range_bytes: 4096,
                read_bytes: 512,
                container_bytes: 16384,
                workers: 1,
                regions: 1,
                checkpoint_records: 1,
                ..Plan::current()
            },
            temp.path(),
            &NoEvents,
            &crate::NoHooks,
            &AtomicBool::new(false),
        )
        .unwrap();
        let original_json = io::json(&session(
            "perfect-backup/backups/existing",
            data.len() as u64,
        ))
        .unwrap();
        fs::write(old.join("sessions/existing.json"), &original_json).unwrap();
        // Reproduce Builder's move; LiveOS must only interpret the saved path.
        fs::rename(&old, mount.join("crispawn")).unwrap();
        let diag = Diagnostics::new(&temp.path().join("ram"), 64 << 10, "test".into()).unwrap();
        let history = catalog(&mount, &Config::default(), &diag).unwrap();
        diag.stop();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, "preserved-session-id");
        assert_eq!(history[0].disks[0].archive, "crispawn/backups/existing");
        let archive = archive_path(&mount, &history[0].disks[0].archive).unwrap();
        let restored = temp.path().join("restored");
        crate::restore::restore(
            &archive,
            &restored,
            false,
            temp.path(),
            &NoEvents,
            &crate::NoHooks,
        )
        .unwrap();
        assert_eq!(fs::read(restored).unwrap(), data);
        assert_eq!(
            fs::read(mount.join("crispawn/sessions/existing.json")).unwrap(),
            original_json
        );
        assert!(!old.exists());
    }

    #[test]
    fn session_path_remap_is_prefix_only_and_keeps_path_guards() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("session.json");
        for (input, expected) in [
            ("perfect-backup/backups/job", "crispawn/backups/job"),
            ("crispawn/backups/job", "crispawn/backups/job"),
            ("perfect-backup-other/job", "perfect-backup-other/job"),
            ("custom/perfect-backup/job", "custom/perfect-backup/job"),
        ] {
            let original = io::json(&session(input, 512)).unwrap();
            fs::write(&file, &original).unwrap();
            assert_eq!(read_saved_set(&file).unwrap().disks[0].archive, expected);
            assert_eq!(fs::read(&file).unwrap(), original);
        }
        for input in ["perfect-backup/../outside", "/perfect-backup/backups/job"] {
            fs::write(&file, io::json(&session(input, 512)).unwrap()).unwrap();
            assert_eq!(read_saved_set(&file).unwrap_err().code, "INVALID_CONFIG");
        }
        fs::create_dir(temp.path().join("crispawn")).unwrap();
        symlink(temp.path(), temp.path().join("crispawn/backups")).unwrap();
        fs::write(
            &file,
            io::json(&session("perfect-backup/backups/job", 512)).unwrap(),
        )
        .unwrap();
        let set = read_saved_set(&file).unwrap();
        assert_eq!(
            archive_path(temp.path(), &set.disks[0].archive)
                .unwrap_err()
                .code,
            "UNSAFE_PATH"
        );
    }
}
pub(super) fn execute(
    root: &Path,
    diag: &Arc<Diagnostics>,
    mount: &Mounted,
    config: &Settings,
    devices: &[Device],
    frontends: &mut Frontends,
    network: &mut dyn crate::connection::Readiness,
) -> Result<()> {
    let excluded_boot = diag.snapshot().excluded_disk;
    let mut sources = source_candidates(devices, &excluded_boot, &mount.device.whole_name)
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    require(
        !sources.is_empty() && sources.len() <= 64,
        "SOURCE_NOT_FOUND",
        "no eligible internal whole disk",
    )?;
    sources.sort_by(|a, b| {
        a.identity
            .as_ref()
            .unwrap()
            .stable_id
            .cmp(&b.identity.as_ref().unwrap().stable_id)
    });
    if config.version == 2 {
        require(
            sources.windows(2).all(|pair| {
                pair[0].identity.as_ref().unwrap().stable_id
                    != pair[1].identity.as_ref().unwrap().stable_id
            }),
            "SOURCE_ID_NOT_UNIQUE",
            "eligible source stable ID is not unique",
        )?;
    }
    let mountinfo = fs::read_to_string("/proc/self/mountinfo")?;
    let swaps = fs::read_to_string("/proc/swaps")?;
    let disks = sources
        .iter()
        .map(|d| DiskView {
            stable_id: d.identity.as_ref().unwrap().stable_id.clone(),
            name: format!(
                "{} ({})",
                fs::read_to_string(
                    Path::new("/sys/class/block")
                        .join(&d.name)
                        .join("device/model")
                )
                .unwrap_or_else(|_| "Disk".into())
                .trim(),
                d.path.display()
            ),
            bytes: d.size_bytes,
            selected: true,
            state: if device::require_unused(d, devices, Path::new("/sys"), &mountinfo, &swaps)
                .is_ok()
            {
                DiskState::Waiting
            } else {
                DiskState::Unavailable
            },
        })
        .collect();
    let mut preset = config.source_ids.clone();
    if let Some(id) = &config.source_id {
        preset.push(id.clone());
    }
    let mut history = catalog(&mount.path, config, diag)?;
    if let Some(r) = &config.restore {
        if !history
            .iter()
            .any(|s| s.disks.iter().any(|d| d.archive == r.archive))
        {
            let disk = saved_disk(&mount.path, &r.archive, "Configured copy")?;
            history.push(SavedSet {
                id: "configured".into(),
                datetime: "Configured copy".into(),
                disks: vec![disk],
            });
        }
    }
    let mut session = Session::new(
        disks,
        history,
        &preset,
        diag.ms(),
        if config.gui { config.menu_seconds } else { 0 },
    )?;
    config.initialize_session(&mut session)?;
    session.prerequisites_ready(false)?;
    {
        let mut status = diag.status.lock().unwrap_or_else(|e| e.into_inner());
        status.language = config.language;
    }
    diag.phase(
        "MENU",
        "preparing disks while the configurable start timer runs",
    );
    // The visible menu must not exempt a stuck preflight I/O from supervision.
    crate::supervisor::phase("PREPARING");
    publish(diag, &session, mount)?;
    let archives = owned_directory(&mount.path, &config.backup_directory)?;
    let (tx, rx) = mpsc::channel();
    // Preparation reads a few bounded spans; no productive data is discarded for tuning.
    let prepare = {
        let sources = sources.clone();
        let devices = devices.to_vec();
        let tuning = config.tuning.clone();
        let parent = archives.clone();
        std::thread::spawn(move || {
            for (i, d) in sources.iter().enumerate() {
                let result = (|| -> Result<(perfect_backup_core::plan::Plan, u32)> {
                    device::require_unused(
                        d,
                        &devices,
                        Path::new("/sys"),
                        &fs::read_to_string("/proc/self/mountinfo")?,
                        &fs::read_to_string("/proc/swaps")?,
                    )?;
                    let source = device::open(&d.path, false, true)?;
                    let identity = io::identity(&source)?;
                    require(
                        identity.block_device.as_ref() == d.identity.as_ref()
                            && identity.bytes == d.size_bytes,
                        "DEVICE_CHANGED",
                        "prepared disk identity changed",
                    )?;
                    io::source_preflight(&source, identity.bytes)?;
                    io::destination_preflight(&parent)?;
                    let cpus = std::thread::available_parallelism()
                        .map(|v| v.get() as u32)
                        .unwrap_or(1);
                    let (plan, v) = if tuning.mode == crate::job_config::TuningMode::Auto {
                        crate::profiles::select(
                            &tuning.plan,
                            tuning.profile,
                            cpus,
                            crate::profiles::available_memory(),
                            identity.bytes,
                        )?
                    } else {
                        (tuning.plan.clone(), tuning.verify_workers.unwrap_or(1))
                    };
                    io::descriptor_preflight(&plan, identity.bytes, v)?;
                    Ok((plan, v))
                })();
                if tx.send((i, result)).is_err() {
                    break;
                }
            }
        })
    };
    if config.operation == InitialOperation::Restore {
        let r = config.restore.as_ref().ok_or_else(|| {
            Error::new(
                "RESTORE_NOT_CONFIGURED",
                "explicit restore configuration required",
            )
        })?;
        session.action(CommandAction::RestoreMode)?;
        let (b, d) = session
            .view
            .backups
            .iter()
            .enumerate()
            .find_map(|(b, s)| {
                s.disks
                    .iter()
                    .position(|d| d.archive == r.archive)
                    .map(|d| (b, d))
            })
            .ok_or_else(|| Error::new("RESTORE_NOT_CONFIGURED", "configured archive missing"))?;
        let target = session
            .view
            .disks
            .iter()
            .position(|d| d.stable_id == r.target_id)
            .ok_or_else(|| Error::new("RESTORE_TARGET_MISMATCH", "configured target missing"))?;
        session.action(CommandAction::SelectBackup { index: b as u32 })?;
        session.action(CommandAction::Next)?;
        session.action(CommandAction::SelectSavedDisk { index: d as u32 })?;
        session.action(CommandAction::SelectTarget {
            index: target as u32,
        })?;
        session.action(CommandAction::Next)?;
    }
    let mut preparation_finished = false;
    let mut prepared_count = 0usize;
    let mut network_reported = false;
    let start = loop {
        let ready = network.poll()?;
        session.prerequisites_ready(ready)?;
        if ready && !network_reported {
            diag.record("network_ready", json!({"mode":config.network.mode()}));
            network_reported = true;
        }
        let mut disconnected = false;
        loop {
            let (index, result) = match rx.try_recv() {
                Ok(value) => value,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            };
            prepared_count += 1;
            match result {
                Ok((plan, v)) => {
                    session.prepared(index, Ok(()))?;
                    let mut s = diag.status.lock().unwrap_or_else(|e| e.into_inner());
                    if s.selected_plan.is_none() {
                        s.selected_plan = Some(plan);
                        s.verify_workers = v;
                    }
                }
                Err(e) => {
                    diag.record(
                        "preparation_failed",
                        json!({"disk":index,"code":e.code,"detail":e.detail}),
                    );
                    session.prepared(index, Err(e.to_string()))?;
                }
            }
        }
        if !preparation_finished && disconnected {
            preparation_finished = true;
            if prepared_count != sources.len() {
                session.global_preparation_error(
                    "ENGINE_EXITED: preparation ended without every disk result".into(),
                )?;
            }
            crate::supervisor::phase("MENU");
        }
        let mut requested = None;
        for request in requests(root)? {
            if let Some(action) = handle(request, None, diag, mount)? {
                match session.action(action) {
                    Ok(Some(start)) => requested = Some(start),
                    Ok(None) => {}
                    Err(e) => {
                        diag.record("control_rejected", json!({"code":e.code,"detail":e.detail}));
                        diag.status.lock().unwrap_or_else(|e| e.into_inner()).detail =
                            e.to_string();
                    }
                }
            }
        }
        // An explicit headless restore configuration is a pre-authorized choice.
        if config.operation == InitialOperation::Restore
            && !config.gui
            && ready
            && prepare.is_finished()
            && requested.is_none()
        {
            requested = session.action(CommandAction::StartRestore)?;
        }
        publish(diag, &session, mount)?;
        if let Some(start) = requested.or(session.tick(diag.ms())?) {
            break start;
        }
        if let Some(child) = &mut frontends.gui {
            require(
                child.try_wait()?.is_none(),
                "GUI_EXITED",
                "GUI exited at selection",
            )?;
        }
        if let Some(child) = &mut frontends.console {
            require(
                child.try_wait()?.is_none(),
                "CONSOLE_EXITED",
                "console exited at selection",
            )?;
        }
        diag.check()?;
        std::thread::sleep(Duration::from_millis(50));
    };
    diag.phase(
        "PREPARING",
        "selection locked; validating the complete selected session",
    );
    publish(diag, &session, mount)?;
    prepare
        .join()
        .map_err(|_| Error::new("ENGINE_PANICKED", "preparation worker panicked"))?;
    let id = diag.snapshot().job_id;
    let reports = owned_directory(&mount.path, "crispawn/reports")?;
    let work = owned_directory(&mount.path, "crispawn/work")?;
    let (indices, restore) = match start {
        Start::Backup { disks } => (disks, None),
        Start::Restore {
            backup,
            saved_disk,
            target,
        } => (
            vec![target],
            Some(session.view.backups[backup].disks[saved_disk].clone()),
        ),
    };
    let mut saved = SavedSet {
        id: id.clone(),
        datetime: timestamp(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        ),
        disks: vec![],
    };
    for (number, index) in indices.iter().copied().enumerate() {
        let source = &sources[index];
        // Repeat stable identity / in-use checks immediately before each actual job.
        device::require_unused(
            source,
            devices,
            Path::new("/sys"),
            &fs::read_to_string("/proc/self/mountinfo")?,
            &fs::read_to_string("/proc/swaps")?,
        )?;
        let opened = device::open(&source.path, false, true)?;
        let actual = io::identity(&opened)?;
        require(
            actual.block_device.as_ref() == source.identity.as_ref()
                && actual.bytes == source.size_bytes,
            "DEVICE_CHANGED",
            "chosen disk changed before opening",
        )?;
        drop(opened);
        let job_id = format!("{id}-{number:02}");
        let (archive, operation) = if let Some(disk) = &restore {
            diag.record("restore_serial_comparison",json!({"result":if disk.stable_id.is_empty(){"unknown"}else if disk.stable_id == source.identity.as_ref().unwrap().stable_id{"match"}else{"mismatch"},"saved":disk.stable_id,"target":source.identity.as_ref().unwrap().stable_id}));
            (
                archive_path(&mount.path, &disk.archive)?,
                Operation::Restore,
            )
        } else {
            (archives.join(&job_id), Operation::Backup)
        };
        session.view.page = Page::Running;
        session.view.current_disk = Some(index);
        session.view.disks[index].state = DiskState::Working;
        {
            let mut s = diag.status.lock().unwrap_or_else(|e| e.into_inner());
            s.source_disk = source.path.to_string_lossy().into_owned();
            s.source_id = source.identity.as_ref().unwrap().stable_id.clone();
            s.source_bytes = restore
                .as_ref()
                .map(|d| d.bytes)
                .unwrap_or(source.size_bytes);
            s.snapshot = None;
            s.percent = 0;
            s.eta_seconds = None;
        }
        publish(diag, &session, mount)?;
        let scratch = work.join(&job_id);
        fs::create_dir(&scratch)?;
        let job = JobConfig {
            schema_version: 1,
            source_file: None,
            source_device: Some(source.path.clone()),
            archive: archive.clone(),
            scratch_dir: scratch.clone(),
            restore_target_file: None,
            restore_target_device: (operation == Operation::Restore).then(|| source.path.clone()),
            report_file: Some(reports.join(format!("{job_id}.json"))),
            tuning: config.tuning.clone(),
        };
        let controller = Arc::new(Controller::new(diag.clone() as Arc<dyn JobEvents>)?);
        let (tx, rx) = mpsc::channel();
        let worker = {
            let controller = controller.clone();
            std::thread::spawn(move || {
                let _ = tx.send(controller.run(&job, operation));
            })
        };
        let result = loop {
            match rx.try_recv() {
                Ok(r) => break r,
                Err(mpsc::TryRecvError::Disconnected) => {
                    break Err(Error::new(
                        "ENGINE_EXITED",
                        "job worker ended without a result",
                    ))
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            match requests(root) {
                Ok(values) => {
                    for request in values {
                        match handle(request, Some(&controller), diag, mount) {
                            Ok(Some(_)) => diag.record("control_rejected",json!({"code":"CONTROL_UNAVAILABLE","detail":"disk/mode selection is locked during a job"})),
                            Ok(None) => {},
                            Err(e) => diag.record("control_error",json!({"code":e.code,"detail":e.detail})),
                        }
                    }
                }
                Err(e) => diag.record("control_error", json!({"code":e.code,"detail":e.detail})),
            }
            if diag.check().is_err() || network.poll().is_err() {
                controller.cancel();
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        worker
            .join()
            .map_err(|_| Error::new("ENGINE_PANICKED", "job worker panicked"))?;
        fs::remove_dir(&scratch)
            .map_err(|e| Error::new("SCRATCH_CLEANUP_FAILED", e.to_string()))?;
        network.poll()?;
        result?;
        session.view.disks[index].state = DiskState::Done;
        session.view.completed_disks += 1;
        let bytes = restore
            .as_ref()
            .map(|d| d.bytes)
            .unwrap_or(source.size_bytes);
        session.view.previous_durable_bytes += bytes;
        session.view.previous_verified_bytes += bytes;
        session.view.previous_elapsed_seconds += diag
            .snapshot()
            .snapshot
            .as_ref()
            .map(|s| s.elapsed_seconds)
            .unwrap_or(0.0);
        if restore.is_none() {
            saved.disks.push(SavedDisk {
                archive: format!("{}/{}", config.backup_directory, job_id),
                name: session.view.disks[index].name.clone(),
                stable_id: source.identity.as_ref().unwrap().stable_id.clone(),
                bytes,
            });
        }
        if number + 1 < indices.len() {
            diag.status
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .snapshot = None;
        }
    }
    if restore.is_none() {
        let parent = owned_directory(&mount.path, "crispawn/sessions")?;
        io::atomic_write(&parent, &format!("{id}.json"), &io::json(&saved)?)?;
    }
    // Snapshot still describes the last disk; previous counters exclude that disk.
    let last_bytes = restore
        .as_ref()
        .map(|d| d.bytes)
        .unwrap_or_else(|| sources[*indices.last().unwrap()].size_bytes);
    session.view.previous_durable_bytes -= last_bytes;
    session.view.previous_verified_bytes -= last_bytes;
    session.view.previous_elapsed_seconds -= diag
        .snapshot()
        .snapshot
        .as_ref()
        .map(|s| s.elapsed_seconds)
        .unwrap_or(0.0);
    session.view.page = Page::Running;
    session.view.mode = if restore.is_some() {
        Mode::Restore
    } else {
        Mode::Backup
    };
    publish(diag, &session, mount)?;
    diag.record(
        "session_complete",
        json!({"disks":indices.len(),"saved_session":saved.id}),
    );
    Ok(())
}
