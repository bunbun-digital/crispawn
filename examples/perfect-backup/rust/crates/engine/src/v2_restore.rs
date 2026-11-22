// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
use crate::{
    control::ExecutionControl, io, parallel_hash::CpuPool, v2, verify, Hooks, Reporter, Result,
};
use perfect_backup_core::{
    error::require,
    types::{self, EventSink},
};
use std::path::Path;

/// The caller owns the shared archive lock. Inspect the complete archive before
/// opening a target for writes; success requires target readback for every sealed span.
pub(crate) fn restore(
    root: &Path,
    target: &Path,
    overwrite: bool,
    events: &dyn EventSink,
    hooks: &dyn Hooks,
    control: &dyn ExecutionControl,
    block: Option<&types::SourceIdentity>,
) -> Result<verify::Report> {
    let (meta, plan, _) = v2::load(root)?;
    let cpus = std::thread::available_parallelism()
        .map(|v| v.get() as u32)
        .unwrap_or(1);
    let pool = CpuPool::new(plan.workers.min(cpus).max(1))?;
    let mut report = Reporter::new(events, meta.backup_id.clone(), meta.source.bytes);
    // Keep the existing non-destructive archive preflight: a corrupt archive
    // must fail before the first target write. This is not target readback and
    // does not advance the visible restore counters.
    let mut preflight = Reporter::new(&types::NoEvents, meta.backup_id.clone(), meta.source.bytes);
    let verified = v2::inspect(root, true, &mut preflight, control, &pool)?;
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    require(
        !parent.canonicalize()?.starts_with(root),
        "TARGET_IS_ARCHIVE",
        "restore target cannot be inside archive",
    )?;
    let file = if overwrite {
        let file = if let Some(expected) = block {
            let file = crate::device::open(target, true, true)?;
            io::unchanged(&file, expected)?;
            require(
                expected.block_device.as_ref().unwrap().logical_sector_bytes
                    == meta.logical_sector_bytes,
                "SECTOR_SIZE_MISMATCH",
                "restore logical sector size must match archived disk",
            )?;
            file
        } else {
            io::open_regular(target, true)?
        };
        io::lock_file(&file, true)?;
        require(
            crate::device::bytes(&file)? >= meta.source.bytes,
            "TARGET_TOO_SMALL",
            "restore target smaller than archived source",
        )?;
        for name in [
            "metadata.json",
            "plan.json",
            "files.csv",
            "checksums.sha256",
            "COMMITTED",
            "journal.jsonl",
            ".lock",
        ]
        .into_iter()
        .map(str::to_owned)
        .chain(verified.files.iter().map(|e| e.relative_path.clone()))
        {
            require(
                !io::same_file(&file, &io::open_regular(&root.join(name), false)?)?,
                "TARGET_IS_ARCHIVE",
                "target aliases archive member",
            )?;
        }
        file
    } else {
        let file = io::create_new(target)?;
        io::lock_file(&file, true)?;
        file.set_len(meta.source.bytes)?;
        file
    };
    report.verified = 0;
    report.verification_read = 0;
    report.durable = 0;
    report.read = 0;
    report.emit("RESTORING", "RESTORE_STARTED");
    // Files have disjoint original spans. Sync each written file before its
    // readback while later files are restored. No independent final full pass.
    std::thread::scope(|scope| -> Result<()> {
        let mut checking = crate::inline_verify::Inline::with_target(
            scope,
            root,
            &meta,
            &plan,
            &pool,
            control,
            hooks,
            Some(&file),
        );
        let result = (|| -> Result<()> {
            for entry in &verified.files {
                checking.poll(&mut report)?;
                v2::scan_file(
                    root,
                    entry,
                    &meta,
                    &plan,
                    verified.id,
                    &pool,
                    |offset, raw| {
                        report.gate(control, "RESTORING")?;
                        checking.poll(&mut report)?;
                        hooks.at("restore_write")?;
                        io::write_all_at(&file, raw, offset)?;
                        report.read += raw.len() as u64;
                        report.emit("RESTORING", "RANGE_WRITTEN");
                        Ok(())
                    },
                )?;
                hooks.at("restore_sync")?;
                io::sync_file(&file)?;
                report.durable += entry.source_length_bytes;
                report.emit("RESTORING", "RESTORED_FILE_SYNCED");
                checking.submit(entry.clone(), &mut report, control)?;
            }
            checking.finish(&mut report, control)?;
            require(
                report.verified == meta.source.bytes,
                "MISSING_VERIFICATION",
                "every restored span must pass readback",
            )?;
            io::sync_file(&file)?;
            io::sync_dir(&parent.canonicalize()?)?;
            Ok(())
        })();
        checking.close();
        result
    })?;
    require(
        crate::device::bytes(&file)? >= meta.source.bytes,
        "TARGET_CHANGED",
        "restore target size changed",
    )?;
    report.emit("COMPLETE", "RESTORE_VERIFIED");
    let mut result = verified.report();
    result.operation = "restore".into();
    result.restored_ranges_verified = true;
    Ok(result)
}
