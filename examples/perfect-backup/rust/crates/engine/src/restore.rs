// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
use crate::{archive, control::ExecutionControl, io, verify, Hooks, Reporter, Result};
use perfect_backup_core::{
    error::require,
    types::{self, EventSink},
};
use std::path::Path;

/// Explicit regular-file adapter. Never truncates an existing target or opens devices.
pub fn restore(
    root: &Path,
    target: &Path,
    overwrite: bool,
    scratch: &Path,
    events: &dyn EventSink,
    hooks: &dyn Hooks,
) -> Result<verify::Report> {
    restore_controlled(
        root,
        target,
        overwrite,
        scratch,
        events,
        hooks,
        &std::sync::atomic::AtomicBool::new(false),
    )
}
pub fn restore_controlled(
    root: &Path,
    target: &Path,
    overwrite: bool,
    scratch: &Path,
    events: &dyn EventSink,
    hooks: &dyn Hooks,
    control: &dyn ExecutionControl,
) -> Result<verify::Report> {
    restore_inner(
        root, target, overwrite, scratch, events, hooks, control, None,
    )
}
pub fn restore_device_checked(
    root: &Path,
    target: &Path,
    scratch: &Path,
    events: &dyn EventSink,
    hooks: &dyn Hooks,
    control: &dyn ExecutionControl,
    expected: &types::SourceIdentity,
) -> Result<verify::Report> {
    require(
        expected.block_device.is_some(),
        "BLOCK_DEVICE_REQUIRED",
        "explicit restore device identity",
    )?;
    restore_inner(
        root,
        target,
        true,
        scratch,
        events,
        hooks,
        control,
        Some(expected),
    )
}
fn restore_inner(
    root: &Path,
    target: &Path,
    overwrite: bool,
    scratch: &Path,
    events: &dyn EventSink,
    hooks: &dyn Hooks,
    control: &dyn ExecutionControl,
    block: Option<&types::SourceIdentity>,
) -> Result<verify::Report> {
    let root = io::canonical_archive(root)?;
    let _lock = io::Lock::acquire(&root, false)?;
    if crate::v2::is_v2(&root)? {
        return crate::v2_restore::restore(&root, target, overwrite, events, hooks, control, block);
    }
    let (meta, _, _) = archive::load(&root)?;
    let mut report = Reporter::new(events, meta.backup_id.clone(), meta.source.bytes);
    let v = verify::inspect(&root, scratch, true, &mut report, control)?;
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    require(
        !parent.canonicalize()?.starts_with(&root),
        "TARGET_IS_ARCHIVE",
        "restore target cannot be inside the archive",
    )?;
    let file = if overwrite {
        let f = if let Some(expected) = block {
            let f = crate::device::open(target, true, true)?;
            io::unchanged(&f, expected)?;
            require(
                expected.block_device.as_ref().unwrap().logical_sector_bytes
                    == meta.logical_sector_bytes,
                "SECTOR_SIZE_MISMATCH",
                "restore logical sector size must match the archived disk",
            )?;
            f
        } else {
            io::open_regular(target, true)?
        };
        io::lock_file(&f, true)?;
        require(
            crate::device::bytes(&f)? >= meta.source.bytes,
            "TARGET_TOO_SMALL",
            "target is smaller than source",
        )?;
        for name in [
            "metadata.json",
            "plan.json",
            "files.csv",
            "ranges.csv",
            "checksums.sha256",
            "COMMITTED",
            "journal.jsonl",
            ".lock",
        ]
        .into_iter()
        .map(str::to_owned)
        .chain(v.files.iter().map(|e| e.relative_path.clone()))
        {
            require(
                !io::same_file(&f, &io::open_regular(&root.join(name), false)?)?,
                "TARGET_IS_ARCHIVE",
                "target is a hard link to an archive member",
            )?;
        }
        f
    } else {
        let f = io::create_new(target)?;
        io::lock_file(&f, true)?;
        f.set_len(meta.source.bytes)?;
        f
    };
    report.verified = 0;
    report.durable = 0;
    report.emit("RESTORING", "RESTORE_STARTED");
    for e in &v.files {
        archive::scan_sealed(&root, e, v.id, &v.metadata, &v.plan, |r, _, raw| {
            report.gate(control, "RESTORING")?;
            hooks.at("restore_write")?;
            io::write_all_at(&file, raw, r.source_offset)?;
            report.read += r.raw_bytes;
            report.emit("RESTORING", "RANGE_WRITTEN");
            Ok(())
        })?;
    }
    hooks.at("restore_sync")?;
    io::sync_file(&file)?;
    io::sync_dir(&parent.canonicalize()?)?;
    report.durable = meta.source.bytes;
    report.emit("VERIFYING_RESTORE", "RESTORE_SYNCED");
    for id in 0..v.index.count {
        report.gate(control, "VERIFYING_RESTORE")?;
        let (r, _) = v.index.get(id)?;
        let mut raw = vec![0; r.raw_bytes as usize];
        hooks.at("restore_verify_read")?;
        io::read_exact_at(&file, &mut raw, r.source_offset, v.plan.read_bytes)?;
        require(
            types::hash(&raw) == r.raw_hash,
            "RESTORE_HASH_MISMATCH",
            "restored range checksum",
        )?;
        report.verified += r.raw_bytes;
        if id % 64 == 0 {
            report.emit("VERIFYING_RESTORE", "RESTORED_RANGE_VERIFIED");
        }
    }
    require(
        crate::device::bytes(&file)? >= meta.source.bytes,
        "TARGET_CHANGED",
        "target size changed",
    )?;
    report.emit("COMPLETE", "RESTORE_VERIFIED");
    let mut result = v.report();
    result.operation = "restore".into();
    result.restored_ranges_verified = true;
    Ok(result)
}
