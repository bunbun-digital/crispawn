// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
use crate::{
    codec,
    control::ExecutionControl,
    io,
    parallel_hash::CpuPool,
    v2::{self, FileEntry},
    v2_format as format,
    v2_journal::{Change, Journal, Prefix},
    verify, Error, Hooks, Reporter, Result,
};
use crispawn_core::{
    error::require,
    plan::{Plan, Schedule, MAX_FILES},
    types::{self, EventSink, Metadata, SourceIdentity},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Seek, SeekFrom},
    path::Path,
    sync::mpsc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

struct Ready {
    range: u64,
    raw: Vec<u8>,
    encoded: codec::Encoded,
    observation: crate::online::Observation,
}
struct OpenFile {
    file: File,
    id: u64,
    offset: u64,
    raw: u64,
    frames: u64,
    end: u64,
    durable: u64,
    hash: blake3::Hasher,
    profiles: BTreeSet<String>,
}

fn checkpoint(
    f: &mut OpenFile,
    journal: &mut Journal,
    meta: &Metadata,
    plan: &Plan,
    report: &mut Reporter<'_>,
    hooks: &dyn Hooks,
) -> Result<()> {
    if f.raw == f.durable {
        return Ok(());
    }
    hooks.at("before_data_sync")?;
    io::sync_file(&f.file)?;
    hooks.at("after_data_sync")?;
    journal.append(
        Change::Checkpoint {
            prefix: Prefix {
                file_id: f.id,
                raw_bytes: f.raw,
                stored_end: f.end,
                frames: f.frames,
                raw_blake3: f.hash.finalize().to_hex().to_string(),
            },
        },
        meta,
        plan,
        hooks,
    )?;
    report.durable += f.raw - f.durable;
    f.durable = f.raw;
    report.emit("COPYING", "CHECKPOINT_PROGRESS");
    hooks.at("after_checkpoint")
}
fn new_file(
    root: &Path,
    region: usize,
    offset: u64,
    next_id: &mut u64,
    journal: &mut Journal,
    meta: &Metadata,
    plan: &Plan,
    id: [u8; 16],
    report: &mut Reporter<'_>,
    hooks: &dyn Hooks,
) -> Result<OpenFile> {
    require(
        *next_id < MAX_FILES,
        "LIMIT_EXCEEDED",
        "v2 backup file count",
    )?;
    let file_id = *next_id;
    *next_id += 1;
    let partial = root.join(types::data_name(file_id, true));
    require(
        !partial.try_exists()? && !root.join(types::data_name(file_id, false)).try_exists()?,
        "UNREFERENCED_DATA",
        "v2 output file already exists; preserved",
    )?;
    // Journal ownership first: a crash before creation is recoverable as an empty prefix.
    journal.append(
        Change::Open {
            file_id,
            region: region as u32,
            source_offset_bytes: offset.to_string(),
        },
        meta,
        plan,
        hooks,
    )?;
    let mut file = io::create_new(&partial)?;
    io::write_all(
        &mut file,
        &format::file_header(plan.format_version, id, file_id, offset),
    )?;
    io::sync_file(&file)?;
    io::sync_dir(&root.join("data"))?;
    report.stored += format::FILE_HEADER as u64;
    Ok(OpenFile {
        file,
        id: file_id,
        offset,
        raw: 0,
        frames: 0,
        end: format::FILE_HEADER as u64,
        durable: 0,
        hash: blake3::Hasher::new(),
        profiles: BTreeSet::new(),
    })
}
fn seal(
    mut f: OpenFile,
    root: &Path,
    journal: &mut Journal,
    meta: &Metadata,
    plan: &Plan,
    id: [u8; 16],
    report: &mut Reporter<'_>,
    hooks: &dyn Hooks,
) -> Result<FileEntry> {
    require(
        f.raw > 0,
        "INVALID_STATE",
        "cannot seal empty v2 backup file",
    )?;
    checkpoint(&mut f, journal, meta, plan, report, hooks)?;
    let hash = *f.hash.finalize().as_bytes();
    let entry = FileEntry {
        file_id: f.id,
        relative_path: types::data_name(f.id, false),
        source_offset_bytes: f.offset,
        source_length_bytes: f.raw,
        stored_bytes: f.end + format::FOOTER as u64,
        frame_count: f.frames,
        raw_blake3: types::hex(&hash),
        compression_profiles: (plan.format_version == 3)
            .then(|| f.profiles.iter().cloned().collect::<Vec<_>>().join(";")),
    };
    v2::check_entry(&entry, meta, plan)?;
    hooks.at("before_footer")?;
    io::write_all(
        &mut f.file,
        &format::footer(
            plan.format_version,
            id,
            f.id,
            f.offset,
            f.raw,
            f.frames,
            hash,
        ),
    )?;
    io::sync_file(&f.file)?;
    hooks.at("after_footer_sync")?;
    io::rename_new(
        &root.join(types::data_name(f.id, true)),
        &root.join(&entry.relative_path),
    )?;
    hooks.at("after_data_rename")?;
    io::sync_dir(&root.join("data"))?;
    journal.append(
        Change::Seal {
            entry: entry.clone(),
        },
        meta,
        plan,
        hooks,
    )?;
    report.stored += format::FOOTER as u64;
    hooks.at("after_seal")?;
    Ok(entry)
}

pub(crate) fn backup_opened(
    source: File,
    root: &Path,
    plan: Plan,
    _scratch: &Path,
    events: &dyn EventSink,
    hooks: &dyn Hooks,
    control: &dyn ExecutionControl,
    expected: Option<&SourceIdentity>,
) -> Result<verify::Report> {
    control.check_cancel()?;
    io::lock_file(&source, false)?;
    if let Some(expected) = expected {
        io::unchanged(&source, expected)?;
    }
    let identity = io::identity(&source)?;
    plan.validate(identity.bytes)?;
    require(
        matches!(plan.format_version, 2 | 3),
        "UNKNOWN_VERSION",
        "v2 backup plan",
    )?;
    let parent = root
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    crate::destination_space::check(parent)?;
    fs::create_dir(root)?;
    fs::create_dir(root.join("data"))?;
    io::sync_file(&io::create_new(&root.join(".lock"))?)?;
    let _lock = io::Lock::acquire(root, true)?;
    let id = io::new_id()?;
    let block = identity.block_device.is_some();
    let meta = Metadata {
        format_version: plan.format_version,
        backup_id: types::hex(&id),
        source_kind: if block {
            "linux-whole-block-device"
        } else {
            "regular-file-fixture"
        }
        .into(),
        logical_sector_bytes: identity
            .block_device
            .as_ref()
            .map(|d| d.logical_sector_bytes)
            .unwrap_or(512),
        physical_sector_bytes: identity
            .block_device
            .as_ref()
            .map(|d| d.physical_sector_bytes)
            .unwrap_or(512),
        source: identity,
        hash_algorithm: "blake3".into(),
        created_unix_seconds: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Error::new("CLOCK_ERROR", "clock before epoch"))?
            .as_secs()
            .to_string(),
        application_version: env!("CARGO_PKG_VERSION").into(),
        source_consistency: if block {
            "exclusive-open+unmounted-whole-device"
        } else {
            "caller-quiescent-file+identity+timestamps"
        }
        .into(),
        boot_exclusion: if block {
            "live-controller-excludes-boot-and-destination-whole-device"
        } else {
            "not-applicable-regular-file-fixture"
        }
        .into(),
    };
    io::atomic_write(root, "metadata.json", &io::json(&meta)?)?;
    io::atomic_write(root, "plan.json", &io::json(&plan)?)?;
    let journal = Journal::create(root, &meta, &plan)?;
    io::sync_dir(&root.join("data"))?;
    io::sync_dir(root)?;
    io::sync_dir(parent)?;
    let regions = journal.state.cursors.len();
    run(
        source,
        root,
        &meta,
        &plan,
        id,
        events,
        hooks,
        control,
        journal,
        (0..regions).map(|_| None).collect(),
    )
}

fn region_of(range: u64, count: u64, regions: u64) -> usize {
    (((range + 1) * regions - 1) / count) as usize
}

/// The caller owns the archive lock. Validate every durable byte before any
/// recovery mutation; only journal-owned suffixes may subsequently be removed.
pub(crate) fn resume(
    path: &Path,
    root: &Path,
    events: &dyn EventSink,
    hooks: &dyn Hooks,
    control: &dyn ExecutionControl,
    expected: Option<&SourceIdentity>,
) -> Result<verify::Report> {
    control.check_cancel()?;
    let (meta, plan, id) = v2::load(root)?;
    require(
        meta.source.block_device.is_some() == expected.is_some(),
        "ADAPTER_MISMATCH",
        "v2 resume source adapter",
    )?;
    let source = if expected.is_some() {
        crate::device::open(path, false, true)?
    } else {
        io::open_regular(path, false)?
    };
    io::lock_file(&source, false)?;
    if let Some(expected) = expected {
        io::unchanged(&source, expected)?;
    }
    io::unchanged(&source, &meta.source)?;
    let pool = CpuPool::new(plan.workers)?;
    let replay = crate::v2_journal::replay(root, &meta, &plan)?;
    let journal_file = v2::open_owned(&root.join("journal.jsonl"))?;
    require(
        !io::same_file(&source, &journal_file)?,
        "SOURCE_IS_ARCHIVE",
        "v2 journal aliases source",
    )?;
    let mut known = std::collections::BTreeSet::new();
    for entry in &replay.state.sealed {
        known.insert(
            Path::new(&entry.relative_path)
                .file_name()
                .unwrap()
                .to_owned(),
        );
        v2::scan_file(root, entry, &meta, &plan, id, &pool, |offset, raw| {
            control.check_cancel()?;
            let mut original = vec![0; raw.len()];
            io::read_exact_at(&source, &mut original, offset, plan.read_bytes)?;
            require(
                original == raw,
                "SOURCE_CHANGED",
                "v2 sealed source bytes changed",
            )
        })?;
    }
    struct Recovery {
        partial: crate::v2_journal::Partial,
        file: Option<File>,
        hash: blake3::Hasher,
        profiles: BTreeSet<String>,
        renamed: bool,
    }
    let mut recoveries = vec![];
    for partial in replay.state.partial.values() {
        control.check_cancel()?;
        let p = &partial.prefix;
        let part_name = types::data_name(p.file_id, true);
        let full_name = types::data_name(p.file_id, false);
        let part_exists = fs::symlink_metadata(root.join(&part_name))
            .map(|_| true)
            .or_else(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    Ok(false)
                } else {
                    Err(e)
                }
            })?;
        let full_exists = fs::symlink_metadata(root.join(&full_name))
            .map(|_| true)
            .or_else(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    Ok(false)
                } else {
                    Err(e)
                }
            })?;
        require(
            !(part_exists && full_exists),
            "UNREFERENCED_DATA",
            "both partial and sealed v2 files exist; preserved",
        )?;
        require(
            part_exists || full_exists || p.raw_bytes == 0,
            "MISSING_DATA",
            "v2 checkpoint file is missing",
        )?;
        let mut hash = blake3::Hasher::new();
        let mut profiles = BTreeSet::new();
        let file = if part_exists || full_exists {
            let name = if full_exists { &full_name } else { &part_name };
            known.insert(Path::new(name).file_name().unwrap().to_owned());
            let file = v2::open_owned(&root.join(name))?;
            require(
                !io::same_file(&source, &file)?,
                "SOURCE_IS_ARCHIVE",
                "v2 partial aliases source",
            )?;
            let length = file.metadata()?.len();
            require(
                length <= plan.container_bytes && (p.raw_bytes == 0 || length >= p.stored_end),
                "TRUNCATED_RECORD",
                "v2 durable checkpoint data missing or oversized",
            )?;
            if length >= format::FILE_HEADER as u64 {
                let mut header = [0; format::FILE_HEADER];
                io::read_exact_at(&file, &mut header, 0, plan.read_bytes)?;
                format::check_file_header(
                    plan.format_version,
                    &header,
                    id,
                    p.file_id,
                    partial.source_offset,
                )?;
            } else {
                require(
                    p.raw_bytes == 0 && !full_exists,
                    "TRUNCATED_RECORD",
                    "v2 durable file header",
                )?;
            }
            let mut position = format::FILE_HEADER as u64;
            let mut raw_bytes = 0;
            let mut frames = 0;
            while position < p.stored_end {
                control.check_cancel()?;
                require(
                    p.stored_end - position >= format::FRAME_HEADER as u64,
                    "TRUNCATED_RECORD",
                    "v2 checkpoint frame header",
                )?;
                let mut header = [0; format::FRAME_HEADER];
                io::read_exact_at(&file, &mut header, position, plan.read_bytes)?;
                let frame = format::Frame::decode(&header, &plan)?;
                profiles.insert(format::profile_name(frame.codec, frame.level));
                position += format::FRAME_HEADER as u64;
                require(
                    raw_bytes < p.raw_bytes
                        && frame.raw_bytes == plan.range_bytes.min(p.raw_bytes - raw_bytes)
                        && u64::from(frame.payload_bytes) <= p.stored_end - position,
                    "INVALID_RECORD",
                    "v2 checkpoint frame extent",
                )?;
                let mut payload = vec![0; frame.payload_bytes as usize];
                io::read_exact_at(&file, &mut payload, position, plan.read_bytes)?;
                let raw = codec::decode(frame.codec, &payload, frame.raw_bytes as usize)?;
                let mut original = vec![0; raw.len()];
                io::read_exact_at(
                    &source,
                    &mut original,
                    partial.source_offset + raw_bytes,
                    plan.read_bytes,
                )?;
                require(
                    raw == original,
                    "SOURCE_CHANGED",
                    "v2 checkpoint source bytes changed",
                )?;
                pool.update(&mut hash, &raw);
                raw_bytes += frame.raw_bytes;
                frames += 1;
                position += u64::from(frame.payload_bytes);
            }
            require(
                position == p.stored_end
                    && raw_bytes == p.raw_bytes
                    && frames == p.frames
                    && hash.finalize().to_hex().as_str() == p.raw_blake3,
                "FILE_HASH_MISMATCH",
                "v2 checkpoint prefix BLAKE3",
            )?;
            if full_exists {
                require(
                    p.raw_bytes > 0 && length == p.stored_end + format::FOOTER as u64,
                    "INVALID_FOOTER",
                    "v2 renamed file disagrees with durable checkpoint",
                )?;
                let mut footer = [0; format::FOOTER];
                io::read_exact_at(&file, &mut footer, p.stored_end, plan.read_bytes)?;
                format::check_footer(
                    plan.format_version,
                    &footer,
                    id,
                    p.file_id,
                    partial.source_offset,
                    p.raw_bytes,
                    p.frames,
                    *hash.finalize().as_bytes(),
                )?;
            }
            Some(file)
        } else {
            None
        };
        recoveries.push(Recovery {
            partial: partial.clone(),
            file,
            hash,
            profiles,
            renamed: full_exists,
        });
    }
    let actual = fs::read_dir(root.join("data"))?
        .map(|e| e.map(|x| x.file_name()))
        .collect::<std::io::Result<std::collections::BTreeSet<_>>>()?;
    require(
        actual == known,
        "UNREFERENCED_DATA",
        "unknown v2 data files preserved; recovery stopped",
    )?;
    io::unchanged(&source, &meta.source)?;
    control.check_cancel()?;
    crate::destination_space::check(root)?;
    // All validation above is read-only. From here, repair only owned paths.
    let mut journal = Journal::resume(root, replay)?;
    let mut open: Vec<Option<OpenFile>> = (0..journal.state.cursors.len()).map(|_| None).collect();
    for recovery in recoveries {
        let p = recovery.partial;
        let prefix = &p.prefix;
        if recovery.renamed {
            let entry = FileEntry {
                file_id: prefix.file_id,
                relative_path: types::data_name(prefix.file_id, false),
                source_offset_bytes: p.source_offset,
                source_length_bytes: prefix.raw_bytes,
                stored_bytes: prefix.stored_end + format::FOOTER as u64,
                frame_count: prefix.frames,
                raw_blake3: prefix.raw_blake3.clone(),
                compression_profiles: (plan.format_version == 3).then(|| {
                    recovery
                        .profiles
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(";")
                }),
            };
            journal.append(Change::Seal { entry }, &meta, &plan, hooks)?;
        } else {
            let mut file = match recovery.file {
                Some(f) => f,
                None => io::create_new(&root.join(types::data_name(prefix.file_id, true)))?,
            };
            file.set_len(prefix.stored_end)?;
            if prefix.raw_bytes == 0 {
                file.seek(SeekFrom::Start(0))?;
                io::write_all(
                    &mut file,
                    &format::file_header(plan.format_version, id, prefix.file_id, p.source_offset),
                )?;
            }
            file.seek(SeekFrom::Start(prefix.stored_end))?;
            io::sync_file(&file)?;
            open[p.region] = Some(OpenFile {
                file,
                id: prefix.file_id,
                offset: p.source_offset,
                raw: prefix.raw_bytes,
                frames: prefix.frames,
                end: prefix.stored_end,
                durable: prefix.raw_bytes,
                hash: recovery.hash,
                profiles: recovery.profiles,
            });
        }
    }
    io::sync_dir(&root.join("data"))?;
    run(
        source, root, &meta, &plan, id, events, hooks, control, journal, open,
    )
}

fn run(
    source: File,
    root: &Path,
    meta: &Metadata,
    plan: &Plan,
    id: [u8; 16],
    events: &dyn EventSink,
    hooks: &dyn Hooks,
    control: &dyn ExecutionControl,
    journal: Journal,
    open: Vec<Option<OpenFile>>,
) -> Result<verify::Report> {
    let pool = CpuPool::new(plan.workers)?;
    std::thread::scope(|scope| {
        let mut verification =
            crate::inline_verify::Inline::new(scope, root, meta, plan, &pool, control, hooks);
        let result = run_inner(
            source,
            root,
            meta,
            plan,
            id,
            events,
            hooks,
            control,
            journal,
            open,
            &pool,
            &mut verification,
        );
        if let Err(error) = &result {
            crate::supervisor::failure(error);
        }
        verification.close();
        result
    })
}

fn run_inner(
    source: File,
    root: &Path,
    meta: &Metadata,
    plan: &Plan,
    id: [u8; 16],
    events: &dyn EventSink,
    hooks: &dyn Hooks,
    control: &dyn ExecutionControl,
    mut journal: Journal,
    mut open: Vec<Option<OpenFile>>,
    pool: &CpuPool,
    verification: &mut crate::inline_verify::Inline,
) -> Result<verify::Report> {
    let mut space = crate::destination_space::Monitor::new(root)?;
    let count = plan.range_count(meta.source.bytes);
    let regions = open.len();
    let mut next: Vec<u64> = journal
        .state
        .cursors
        .iter()
        .enumerate()
        .map(|(r, cursor)| {
            let offset = open[r]
                .as_ref()
                .map(|f| f.offset + f.raw)
                .unwrap_or(*cursor);
            offset / plan.range_bytes + u64::from(offset % plan.range_bytes != 0)
        })
        .collect();
    let ends: Vec<u64> = journal
        .state
        .ends
        .iter()
        .map(|end| end / plan.range_bytes + u64::from(end % plan.range_bytes != 0))
        .collect();
    let mut pending: Vec<BTreeMap<u64, Ready>> = (0..regions).map(|_| BTreeMap::new()).collect();
    let mut next_file = journal.state.used.last().map(|v| v + 1).unwrap_or(0);
    let mut report = Reporter::new(events, meta.backup_id.clone(), meta.source.bytes);
    report.durable = journal
        .state
        .sealed
        .iter()
        .map(|f| f.source_length_bytes)
        .sum::<u64>()
        + open
            .iter()
            .filter_map(Option::as_ref)
            .map(|f| f.raw)
            .sum::<u64>();
    report.read = report.durable;
    report.stored = journal
        .state
        .sealed
        .iter()
        .map(|f| f.stored_bytes)
        .sum::<u64>()
        + open
            .iter()
            .filter_map(Option::as_ref)
            .map(|f| f.end)
            .sum::<u64>();
    report.emit("COPYING", "BACKUP_STARTED");
    let cpus = std::thread::available_parallelism()
        .map(|v| v.get() as u32)
        .unwrap_or(1);
    let mut feedback = (plan.format_version == 3)
        .then(|| crate::online::Feedback::new(plan, control.verification_workers(), cpus));
    let read_gate = feedback
        .as_ref()
        .map(|f| std::sync::Arc::new(crate::online::ReadGate::new(f.active.workers, plan.workers)));
    if let Some(f) = &feedback {
        events.settings(
            &f.active,
            f.verifiers,
            meta.source.bytes,
            "measured starting profile; productive-copy adaptation enabled",
        );
    }
    for entry in &journal.state.sealed {
        verification.submit(entry.clone(), &mut report, control)?;
    }
    let mut schedule = if plan.format_version == 3 {
        Schedule::from_ends(count, plan.regions)
    } else {
        Schedule::new(count, plan.regions)
    };
    let (sender, receiver) = mpsc::channel::<(u64, Result<Ready>)>();
    let mut outstanding = 0usize;
    let mut exhausted = false;
    let copied = pool.pool().in_place_scope(|scope| -> Result<()> {
        loop {
            control.check_cancel()?;
            space.poll(root)?;
            verification.poll(&mut report)?;
            while !exhausted
                && outstanding
                    < (feedback
                        .as_ref()
                        .map(|f| f.active.workers)
                        .unwrap_or(plan.workers) as usize)
                        * 2
                && !control.pause_requested()
            {
                let range = match schedule.next() {
                    Some(value) => value,
                    None => {
                        exhausted = true;
                        break;
                    }
                };
                let region = region_of(range, count, regions as u64);
                if range < next[region] {
                    continue;
                }
                let send = sender.clone();
                let source = &source;
                let read_gate = read_gate.clone();
                let lane = region;
                let (profile, read_bytes) = feedback.as_mut().map(|f| f.next(lane)).unwrap_or((
                    crate::online::Profile {
                        codec: plan.codec,
                        level: plan.compression_level,
                    },
                    plan.read_bytes,
                ));
                outstanding += 1;
                scope.spawn(move |_| {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                        || -> Result<Ready> {
                            let _permit = read_gate
                                .as_ref()
                                .map(|gate| gate.acquire(control))
                                .transpose()?;
                            control.check_cancel()?;
                            let (offset, length) = plan.range(meta.source.bytes, range)?;
                            hooks.at("source_read")?;
                            let mut raw = vec![0; length as usize];
                            let read_start = std::time::Instant::now();
                            io::read_exact_at(source, &mut raw, offset, read_bytes)?;
                            let read_seconds = read_start.elapsed().as_secs_f64();
                            hooks.at("codec_encode")?;
                            let encode_start =
                                crate::online::cpu_seconds(libc::CLOCK_THREAD_CPUTIME_ID);
                            let encoded = codec::encode(raw.clone(), profile.codec, profile.level)?;
                            let observation = crate::online::Observation {
                                lane,
                                requested: profile,
                                actual: encoded.codec,
                                raw: length,
                                stored: encoded.payload.len() as u64 + format::FRAME_HEADER as u64,
                                encode_cpu: crate::online::cpu_seconds(
                                    libc::CLOCK_THREAD_CPUTIME_ID,
                                ) - encode_start,
                                read_seconds,
                                read_bytes,
                                writer_seconds: 0.0,
                            };
                            hooks.at("after_codec_encode")?;
                            Ok(Ready {
                                range,
                                raw,
                                encoded,
                                observation,
                            })
                        },
                    ))
                    .unwrap_or_else(|_| {
                        Err(Error::new(
                            "ENGINE_PANICKED",
                            "v2 read/compression worker panicked",
                        ))
                    });
                    // The coordinator bounds outstanding jobs; a worker never blocks
                    // a Rayon CPU on queue backpressure while hashing needs that CPU.
                    let _ = send.send((range, result));
                });
            }
            if outstanding == 0 {
                if control.pause_requested() {
                    for f in open.iter_mut().filter_map(Option::as_mut) {
                        checkpoint(f, &mut journal, meta, plan, &mut report, hooks)?;
                    }
                    crate::destination_space::gate(&mut report, control, root, "COPYING")?;
                    if let Some(feedback) = &mut feedback {
                        feedback.resumed();
                    }
                    continue;
                }
                if exhausted {
                    break;
                }
            }
            match receiver.recv_timeout(Duration::from_millis(50)) {
                Ok((range, result)) => {
                    let ready = result?;
                    let region = region_of(range, count, regions as u64);
                    report.read += ready.raw.len() as u64;
                    require(
                        pending[region].insert(range, ready).is_none(),
                        "DUPLICATE_RANGE",
                        "v2 worker returned duplicate source range",
                    )?;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => return Err(Error::new("ENGINE_EXITED", "v2 worker channel ended early")),
            }
            for region in 0..regions {
                while let Some(mut ready) = pending[region].remove(&next[region]) {
                    require(
                        ready.range == next[region],
                        "INVALID_STATE",
                        "v2 source order",
                    )?;
                    let offset = plan.range(meta.source.bytes, ready.range)?.0;
                    let record_bytes =
                        format::FRAME_HEADER as u64 + ready.encoded.payload.len() as u64;
                    if open[region].as_ref().is_some_and(|f| {
                        f.end + record_bytes + format::FOOTER as u64 > plan.container_bytes
                    }) {
                        let entry = seal(
                            open[region].take().unwrap(),
                            root,
                            &mut journal,
                            meta,
                            plan,
                            id,
                            &mut report,
                            hooks,
                        )?;
                        verification.submit(entry, &mut report, control)?;
                    }
                    if open[region].is_none() {
                        open[region] = Some(new_file(
                            root,
                            region,
                            offset,
                            &mut next_file,
                            &mut journal,
                            meta,
                            plan,
                            id,
                            &mut report,
                            hooks,
                        )?);
                    }
                    let f = open[region].as_mut().unwrap();
                    require(
                        f.offset + f.raw == offset
                            && f.end + record_bytes + format::FOOTER as u64 <= plan.container_bytes,
                        "INVALID_STATE",
                        "v2 noncontiguous file or size overflow",
                    )?;
                    f.profiles.insert(format::profile_name(
                        ready.encoded.codec,
                        ready.encoded.level,
                    ));
                    pool.update(&mut f.hash, &ready.raw);
                    let write_start = std::time::Instant::now();
                    hooks.at("before_data_write")?;
                    hooks.at("destination_space_check")?;
                    crate::destination_space::check(root)?;
                    io::write_all(
                        &mut f.file,
                        &format::Frame {
                            codec: ready.encoded.codec,
                            level: ready.encoded.level,
                            raw_bytes: ready.raw.len() as u64,
                            payload_bytes: ready.encoded.payload.len() as u32,
                        }
                        .encode(plan.format_version),
                    )?;
                    io::write_all(&mut f.file, &ready.encoded.payload)?;
                    f.raw += ready.raw.len() as u64;
                    f.frames += 1;
                    f.end += record_bytes;
                    report.stored += record_bytes;
                    hooks.at("after_data_write")?;
                    if f.frames % u64::from(plan.checkpoint_records) == 0 {
                        checkpoint(f, &mut journal, meta, plan, &mut report, hooks)?;
                    }
                    ready.observation.writer_seconds = write_start.elapsed().as_secs_f64();
                    if let Some(feedback) = &mut feedback {
                        let (bytes, seconds) = verification.service();
                        feedback.verification_service(bytes, seconds);
                        if let Some(reason) = feedback.observe(
                            &ready.observation,
                            report.durable.saturating_sub(report.verified),
                        ) {
                            control.configure_verification_workers(feedback.verifiers);
                            if let Some(gate) = &read_gate {
                                gate.set(feedback.active.workers);
                            }
                            events.settings(
                                &feedback.active,
                                feedback.verifiers,
                                meta.source.bytes,
                                &reason,
                            );
                        }
                    }
                    next[region] += 1;
                    outstanding -= 1;
                    if next[region] == ends[region] {
                        let entry = seal(
                            open[region].take().unwrap(),
                            root,
                            &mut journal,
                            meta,
                            plan,
                            id,
                            &mut report,
                            hooks,
                        )?;
                        verification.submit(entry, &mut report, control)?;
                    }
                }
            }
        }
        Ok(())
    });
    if let Err(error) = copied {
        if error.code == "CANCELLED" {
            for f in open.iter_mut().filter_map(Option::as_mut) {
                checkpoint(f, &mut journal, meta, plan, &mut report, hooks)?;
            }
        }
        return Err(error);
    }
    for f in open.into_iter().flatten() {
        let entry = seal(f, root, &mut journal, meta, plan, id, &mut report, hooks)?;
        verification.submit(entry, &mut report, control)?;
    }
    require(next == ends, "MISSING_RANGE", "v2 pipeline source coverage")?;
    io::unchanged(&source, &meta.source)?;
    hooks.at("before_indices")?;
    v2::write_indices(root, &journal.state.sealed)?;
    hooks.at("after_indices")?;
    verification.finish(&mut report, control)?;
    let verified = v2::manifest(root, false, control)?;
    require(
        verification.matches(root, &verified.files)? && report.verified == meta.source.bytes,
        "MISSING_VERIFICATION",
        "not every indexed sealed file passed concurrent verification",
    )?;
    io::unchanged(&source, &meta.source)?;
    journal.compact(root, meta, plan)?;
    hooks.at("before_commit")?;
    control.check_cancel()?;
    crate::destination_space::check(root)?;
    require(
        verification.matches(root, &verified.files)?,
        "MISSING_VERIFICATION",
        "verified file set changed before commit",
    )?;
    v2::write_commit(root, &meta.backup_id)?;
    hooks.at("after_commit")?;
    report.emit("COMPLETE", "BACKUP_COMMITTED");
    let mut result = verified.report();
    result.operation = "backup".into();
    Ok(result)
}
