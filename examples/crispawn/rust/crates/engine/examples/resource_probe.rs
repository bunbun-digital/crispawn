// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! Native concurrent-pipeline evaluator. Private temporary records, not a new
//! product backup format. Uses the engine's actual codecs and parallel BLAKE3.
#[cfg(test)]
use crispawn_core::plan::Plan;
use crispawn_core::types::Codec;
use crispawn_engine::{
    adaptive::{self, Choice, Cost, Measurement, Resources},
    codec,
    parallel_hash::CpuPool,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::io::AsRawFd,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    time::Instant,
};

type AnyResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
const MIB: u64 = 1 << 20;
#[path = "support/online.rs"]
mod online;
#[derive(Deserialize)]
struct Config {
    mode: String,
    source: Option<PathBuf>,
    destination: PathBuf,
    report: PathBuf,
    resources: Option<Resources>,
    costs: Option<PathBuf>,
    #[serde(default = "default_trials")]
    max_trials: usize,
    #[serde(default = "default_bytes")]
    trial_bytes: u64,
    #[serde(default = "default_validate")]
    validate_bytes: u64,
    #[serde(default)]
    start_gate: Option<PathBuf>,
    #[serde(default)]
    fixed_reference: Option<Choice>,
}
fn default_trials() -> usize {
    20
}
fn default_bytes() -> u64 {
    256 * MIB
}
fn default_validate() -> u64 {
    2048 * MIB
}
fn cpu_seconds() -> f64 {
    let mut t = std::mem::MaybeUninit::<libc::timespec>::uninit();
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, t.as_mut_ptr()) },
        0
    );
    let t = unsafe { t.assume_init() };
    t.tv_sec as f64 + t.tv_nsec as f64 * 1e-9
}
fn event(value: serde_json::Value) {
    println!("{value}");
    std::io::stdout().flush().unwrap();
}
fn save(path: &Path, value: &impl Serialize) -> AnyResult<()> {
    let mut f = OpenOptions::new().write(true).create_new(true).open(path)?;
    serde_json::to_writer_pretty(&mut f, value)?;
    f.write_all(b"\n")?;
    f.sync_all()?;
    Ok(())
}
fn drop_cache(f: &File, offset: u64, len: u64) -> AnyResult<()> {
    let status = unsafe {
        libc::posix_fadvise(
            f.as_raw_fd(),
            offset as i64,
            len as i64,
            libc::POSIX_FADV_DONTNEED,
        )
    };
    if status != 0 {
        return Err(std::io::Error::from_raw_os_error(status).into());
    }
    Ok(())
}
fn read_at(f: &File, buf: &mut [u8], offset: u64, bs: u64) -> AnyResult<()> {
    crispawn_engine::io::read_exact_at(f, buf, offset, bs)?;
    Ok(())
}
fn random_fill(buf: &mut [u8], s: &mut u64) {
    for block in buf.chunks_mut(8) {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        let b = s.to_le_bytes();
        block.copy_from_slice(&b[..block.len()]);
    }
}

fn generate(dir: &Path) -> AnyResult<serde_json::Value> {
    let mut seed = 0x4f87_b2aa_091c_75d3u64;
    let mut dictionary = vec![0; 1 << 20];
    random_fill(&mut dictionary, &mut seed);
    let mut repeat = vec![0; 8 << 20];
    random_fill(&mut repeat, &mut seed);
    let mut block = vec![0; MIB as usize];
    let mut reports = vec![];
    for name in ["mixed", "ciphertext", "long-repeat"] {
        let path = dir.join(format!("{name}.img"));
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        for index in 0..4096u64 {
            match name {
                "ciphertext" => random_fill(&mut block, &mut seed),
                "long-repeat" => block.copy_from_slice(
                    &repeat[((index % 8) * MIB) as usize..((index % 8 + 1) * MIB) as usize],
                ),
                _ => match index % 2048 {
                    0..=1023 => random_fill(&mut block, &mut seed),
                    1024..=1535 => {
                        for cell in block.chunks_mut(64) {
                            seed ^= seed << 13;
                            seed ^= seed >> 7;
                            seed ^= seed << 17;
                            let at = ((seed as usize) % 16384) * 64;
                            cell.copy_from_slice(&dictionary[at..at + 64]);
                        }
                    }
                    _ => block.fill(0),
                },
            }
            out.write_all(&block)?;
        }
        out.sync_all()?;
        drop_cache(&out, 0, 0)?;
        reports.push(json!({"name":name,"path":path,"bytes":4096*MIB,"probe_half_bytes":2048*MIB,"held_out_half_bytes":2048*MIB}));
        event(json!({"event":"fixture_ready","name":name}));
    }
    Ok(
        json!({"schema_version":1,"seed":"4f87b2aa091c75d3","fixtures":reports,"sparse_source_files":false}),
    )
}

fn calibrate(source: &Path, destination: &Path) -> AnyResult<serde_json::Value> {
    let input = File::open(source)?;
    let bytes = 512 * MIB;
    let mut buf = vec![0; MIB as usize];
    let mut read = Vec::<f64>::new();
    for _ in 0..3 {
        drop_cache(&input, 2048 * MIB, bytes)?;
        let t = Instant::now();
        for offset in (0..bytes).step_by(MIB as usize) {
            read_at(&input, &mut buf, 2048 * MIB + offset, MIB)?;
        }
        read.push(bytes as f64 / t.elapsed().as_secs_f64());
    }
    let path = destination.join("io-calibration.data");
    let mut out = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)?;
    let mut seed = 67u64;
    random_fill(&mut buf, &mut seed);
    let t = Instant::now();
    for _ in 0..512 {
        out.write_all(&buf)?;
    }
    out.sync_all()?;
    let write = bytes as f64 / t.elapsed().as_secs_f64();
    drop_cache(&out, 0, bytes)?;
    let t = Instant::now();
    for offset in (0..bytes).step_by(MIB as usize) {
        read_at(&out, &mut buf, offset, MIB)?;
    }
    let readback = bytes as f64 / t.elapsed().as_secs_f64();
    drop(out);
    fs::remove_file(&path)?;
    read.sort_by(f64::total_cmp);
    Ok(
        json!({"schema_version":1,"bytes_per_pass":bytes,"source_bps":read[1],"source_repetitions":read,"destination_write_bps":write,"destination_read_bps":readback,"cache_policy":"per-file POSIX_FADV_DONTNEED; no host-wide cache drops","includes_write_fsync":true}),
    )
}

fn costs(source: &Path, cpus: u32) -> AnyResult<Vec<Cost>> {
    let f = File::open(source)?;
    let pool = CpuPool::new(cpus)?;
    let mut costs = vec![];
    for frame in [8 * MIB, 16 * MIB] {
        for (codec, level) in [
            (Codec::Raw, 0),
            (Codec::Zstd, 1),
            (Codec::Zstd, 3),
            (Codec::Zstd, 6),
            (Codec::Zstd, 9),
            (Codec::Xz, 0),
            (Codec::Xz, 3),
            (Codec::Xz, 6),
            (Codec::Gzip, 1),
        ] {
            let mut raw_bytes = 0;
            let mut stored = 0;
            let mut enc = 0.0;
            let mut dec = 0.0;
            for quarter in 0..4u64 {
                let offset = (quarter * 512 + 256) * MIB;
                let mut raw = vec![0; frame as usize];
                read_at(&f, &mut raw, offset, MIB)?;
                let begin = cpu_seconds();
                let expected = pool.hash(&raw);
                let encoded = codec::encode(raw, codec, level)?;
                enc += cpu_seconds() - begin;
                let begin = cpu_seconds();
                let decoded = codec::decode(encoded.codec, &encoded.payload, frame as usize)?;
                if pool.hash(&decoded) != expected {
                    return Err("CPU probe round trip mismatch".into());
                }
                dec += cpu_seconds() - begin;
                raw_bytes += frame;
                stored += encoded.payload.len() as u64 + 128;
            }
            let c = Cost {
                codec,
                level,
                frame_bytes: frame,
                stored_ratio: stored as f64 / raw_bytes as f64,
                encode_cpu_per_byte: enc / raw_bytes as f64,
                verify_cpu_per_byte: dec / raw_bytes as f64,
            };
            event(json!({"event":"codec_cost","cost":c}));
            costs.push(c);
        }
    }
    Ok(costs)
}

#[derive(Clone)]
struct Record {
    offset: u64,
    payload: usize,
    raw: usize,
    codec: Codec,
    hash: [u8; 32],
    header: [u8; 128],
}
struct Output {
    file: File,
    end: u64,
    pending: Vec<Record>,
    sync_seconds: f64,
}
fn verify_record(file: &File, r: &Record, pool: &CpuPool, read_bytes: u64) -> AnyResult<u64> {
    let mut header = [0; 128];
    read_at(file, &mut header, r.offset, read_bytes)?;
    if header != r.header {
        return Err("readback header mismatch".into());
    }
    let mut payload = vec![0; r.payload];
    read_at(file, &mut payload, r.offset + 128, read_bytes)?;
    let decoded = codec::decode(r.codec, &payload, r.raw)?;
    if pool.hash(&decoded) != r.hash {
        return Err("readback BLAKE3 mismatch".into());
    }
    Ok(r.raw as u64)
}

fn read_ranges(half: u64, bytes: u64, frame: u64, heldout: bool) -> AnyResult<Vec<(u64, u64)>> {
    // Independently check coverage before starting concurrent reads.
    if frame == 0 || bytes == 0 || bytes > half || !bytes.is_multiple_of(frame) {
        return Err("evaluation requires whole frames fitting one source half".into());
    }
    let count = bytes / frame;
    let bands = 8.min(count);
    if !count.is_multiple_of(bands) {
        return Err("uneven evaluation bands".into());
    }
    let span = if heldout { bytes } else { half };
    let band_bytes = span / frame / bands * frame;
    let base = if heldout { half } else { 0 };
    let ranges: Vec<_> = (0..count)
        .map(|i| (base + (i % bands) * band_bytes + (i / bands) * frame, frame))
        .collect();
    let mut sorted = ranges.clone();
    sorted.sort_unstable();
    let mut end = base;
    for &(offset, len) in &sorted {
        if offset < end || offset + len > base + span || (heldout && offset != end) {
            return Err("source range overlap, gap or out of bounds".into());
        }
        end = offset + len;
    }
    if heldout && end != base + bytes {
        return Err("incomplete held-out coverage".into());
    }
    Ok(ranges)
}

fn pipeline(
    source: &Path,
    destination: &Path,
    choice: &Choice,
    bytes: u64,
    heldout: bool,
) -> AnyResult<Measurement> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    event(json!({"event":"pipeline_start","sequence":sequence,"id":choice.id,"heldout":heldout}));
    let path = destination.join(format!(
        "trial-{}-{}.data",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    let source = File::open(source)?;
    let identity = crispawn_engine::io::identity(&source)?;
    drop_cache(&source, 0, 0)?;
    let half = identity.bytes / 2;
    let ranges = read_ranges(half, bytes, choice.plan.range_bytes, heldout)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)?;
    let verify_file = File::open(&path)?;
    let output = Mutex::new(Output {
        file,
        end: 0,
        pending: vec![],
        sync_seconds: 0.0,
    });
    let pool = CpuPool::new(choice.plan.workers)?;
    let count = ranges.len();
    let next = AtomicUsize::new(0);
    let verified = AtomicU64::new(0);
    let max_pending = AtomicU64::new(0);
    let overlap = AtomicU64::new(0);
    let verify_active = AtomicU64::new(0);
    let verify_peak = AtomicU64::new(0);
    let read_stage_ns = AtomicU64::new(0);
    let writer_lock_ns = AtomicU64::new(0);
    let verification_queue_ns = AtomicU64::new(0);
    let (tx, rx) = mpsc::sync_channel::<Record>((choice.verify_workers as usize * 8).max(1));
    let rx = Arc::new(Mutex::new(rx));
    let errors = Mutex::new(Vec::<String>::new());
    let start = Instant::now();
    let cpu = cpu_seconds();
    std::thread::scope(|scope| {
        for _ in 0..choice.verify_workers {
            let rx = rx.clone();
            let vf = &verify_file;
            let pool = &pool;
            let verified = &verified;
            let errors = &errors;
            let active = &verify_active;
            let peak = &verify_peak;
            scope.spawn(move || loop {
                let record = rx.lock().unwrap().recv();
                let Ok(r) = record else { break };
                let n = active.fetch_add(1, Ordering::Relaxed) + 1;
                peak.fetch_max(n, Ordering::Relaxed);
                match verify_record(vf, &r, pool, choice.plan.read_bytes) {
                    Ok(n) => {
                        verified.fetch_add(n, Ordering::Relaxed);
                    }
                    Err(e) => {
                        errors.lock().unwrap().push(e.to_string());
                    }
                }
                active.fetch_sub(1, Ordering::Relaxed);
            });
        }
        std::thread::scope(|writers| {
            for _ in 0..choice.plan.workers {
                let tx = tx.clone();
                let next = &next;
                let source = &source;
                let output = &output;
                let pool = &pool;
                let errors = &errors;
                let max_pending = &max_pending;
                let ranges = &ranges;
                let read_stage_ns = &read_stage_ns;
                let writer_lock_ns = &writer_lock_ns;
                let verification_queue_ns = &verification_queue_ns;
                writers.spawn(move || {
                    let result = (|| -> AnyResult<()> {
                        loop {
                            let index = next.fetch_add(1, Ordering::Relaxed);
                            if index >= count {
                                break;
                            }
                            let (offset, len) = ranges[index];
                            let mut raw = vec![0; len as usize];
                            let read_start = Instant::now();
                            read_at(source, &mut raw, offset, choice.plan.read_bytes)?;
                            read_stage_ns.fetch_add(
                                read_start.elapsed().as_nanos() as u64,
                                Ordering::Relaxed,
                            );
                            let hash = pool.hash(&raw);
                            let encoded = codec::encode(
                                raw,
                                choice.plan.codec,
                                choice.plan.compression_level,
                            )?;
                            let mut header = [0u8; 128];
                            header[..8].copy_from_slice(b"PBSTUDY1");
                            header[8..16].copy_from_slice(&offset.to_le_bytes());
                            header[16..24].copy_from_slice(&len.to_le_bytes());
                            header[24..32]
                                .copy_from_slice(&(encoded.payload.len() as u64).to_le_bytes());
                            header[32..64].copy_from_slice(&hash);
                            header[64..68].copy_from_slice(&encoded.codec.id().to_le_bytes());
                            let batch = {
                                let waiting = Instant::now();
                                let mut out = output.lock().unwrap();
                                writer_lock_ns.fetch_add(
                                    waiting.elapsed().as_nanos() as u64,
                                    Ordering::Relaxed,
                                );
                                let offset = out.end;
                                out.file.write_all(&header)?;
                                out.file.write_all(&encoded.payload)?;
                                out.end += 128 + encoded.payload.len() as u64;
                                out.pending.push(Record {
                                    offset,
                                    payload: encoded.payload.len(),
                                    raw: len as usize,
                                    codec: encoded.codec,
                                    hash,
                                    header,
                                });
                                max_pending.fetch_max(out.pending.len() as u64, Ordering::Relaxed);
                                if out.pending.len() >= 8 {
                                    let syncing = Instant::now();
                                    out.file.sync_data()?;
                                    out.sync_seconds += syncing.elapsed().as_secs_f64();
                                    drop_cache(&out.file, 0, out.end)?;
                                    Some(std::mem::take(&mut out.pending))
                                } else {
                                    None
                                }
                            };
                            if let Some(batch) = batch {
                                let waiting = Instant::now();
                                for record in batch {
                                    tx.send(record).map_err(|_| "verification queue closed")?;
                                }
                                verification_queue_ns.fetch_add(
                                    waiting.elapsed().as_nanos() as u64,
                                    Ordering::Relaxed,
                                );
                            }
                        }
                        Ok(())
                    })();
                    if let Err(e) = result {
                        errors.lock().unwrap().push(e.to_string());
                    }
                });
            }
        });
        overlap.store(verified.load(Ordering::Relaxed), Ordering::Relaxed);
        let pending = {
            let mut out = output.lock().unwrap();
            let syncing = Instant::now();
            out.file.sync_data().unwrap();
            out.sync_seconds += syncing.elapsed().as_secs_f64();
            drop_cache(&out.file, 0, out.end).unwrap();
            std::mem::take(&mut out.pending)
        };
        if !pending.is_empty() {
            for record in pending {
                tx.send(record).unwrap();
            }
        }
        drop(tx);
    });
    let seconds = start.elapsed().as_secs_f64();
    let cpu_seconds = cpu_seconds() - cpu;
    let stored = output.lock().unwrap().end;
    let sync_seconds = output.lock().unwrap().sync_seconds;
    let actual = verified.load(Ordering::Relaxed);
    if !errors.lock().unwrap().is_empty() {
        return Err(errors.lock().unwrap().join("; ").into());
    }
    if actual != bytes {
        return Err(format!("coverage mismatch: {actual} / {bytes}").into());
    }
    crispawn_engine::io::unchanged(&source, &identity)?;
    drop(output);
    drop(verify_file);
    fs::remove_file(&path)?;
    let result = Measurement {
        choice: choice.clone(),
        seconds,
        cpu_seconds,
        stored_bytes: stored,
        verified_bytes: actual,
    };
    event(
        json!({"event":"pipeline_finished","sequence":sequence,"heldout":heldout,"measurement":result,"max_pending_records":max_pending.load(Ordering::Relaxed),"verified_during_copy_bytes":overlap.load(Ordering::Relaxed),"peak_active_verifiers":verify_peak.load(Ordering::Relaxed),"writer_sync_seconds":sync_seconds,"read_stage_aggregate_seconds":read_stage_ns.load(Ordering::Relaxed) as f64 / 1e9,"writer_lock_aggregate_seconds":writer_lock_ns.load(Ordering::Relaxed) as f64 / 1e9,"verification_queue_aggregate_seconds":verification_queue_ns.load(Ordering::Relaxed) as f64 / 1e9}),
    );
    Ok(result)
}

fn aggregate_runs(choice: &Choice, measurements: &[Measurement]) -> Measurement {
    let mut rows: Vec<_> = measurements
        .iter()
        .filter(|m| m.choice.id == choice.id)
        .collect();
    rows.sort_by(|a, b| a.seconds.total_cmp(&b.seconds));
    assert!(!rows.is_empty());
    assert!(rows
        .iter()
        .all(|m| m.stored_bytes == rows[0].stored_bytes
            && m.verified_bytes == rows[0].verified_bytes));
    if rows.len() % 2 == 1 {
        return (*rows[rows.len() / 2]).clone();
    }
    Measurement {
        choice: choice.clone(),
        seconds: rows.iter().map(|m| m.seconds).sum::<f64>() / rows.len() as f64,
        cpu_seconds: rows.iter().map(|m| m.cpu_seconds).sum::<f64>() / rows.len() as f64,
        stored_bytes: rows[0].stored_bytes,
        verified_bytes: rows[0].verified_bytes,
    }
}

fn confirm_three(
    source: &Path,
    destination: &Path,
    choices: &[Choice],
    bytes: u64,
    runs: &mut Vec<Measurement>,
) -> AnyResult<()> {
    for round in 0..3 {
        let mut order = choices.to_vec();
        if round == 1 {
            order.reverse();
        }
        for choice in order {
            if runs.iter().filter(|m| m.choice.id == choice.id).count() < 3 {
                runs.push(pipeline(source, destination, &choice, bytes, false)?);
            }
        }
    }
    Ok(())
}

fn study(c: &Config) -> AnyResult<serde_json::Value> {
    let source = c.source.as_ref().ok_or("source required")?;
    let limits = c.resources.as_ref().ok_or("resources required")?;
    event(json!({"event":"device_probe_start"}));
    let device_probe = calibrate(source, &c.destination)?;
    let mut measured_resources = limits.clone();
    // A configured bandwidth ceiling is not an observed device speed. Measure
    // under the current CPU/RAM/I/O limits, including sync and readback.
    measured_resources.source_bps = device_probe["source_bps"]
        .as_f64()
        .ok_or("source probe")?
        .min(limits.source_bps);
    measured_resources.destination_write_bps = device_probe["destination_write_bps"]
        .as_f64()
        .ok_or("write probe")?
        .min(limits.destination_write_bps);
    measured_resources.destination_read_bps = device_probe["destination_read_bps"]
        .as_f64()
        .ok_or("readback probe")?
        .min(limits.destination_read_bps);
    let r = &measured_resources;
    event(json!({"event":"device_probe_finished","resources":r,"measurement":device_probe}));
    let costs: Vec<Cost> =
        serde_json::from_slice(&fs::read(c.costs.as_ref().ok_or("costs required")?)?)?;
    if c.max_trials < 12 {
        return Err("local search requires at least twelve distinct candidates".into());
    }
    let choices = adaptive::propose(r, &costs, c.max_trials.min(10))?;
    let legacy_cost = costs
        .iter()
        .find(|x| x.codec == Codec::Zstd && x.level == 3 && x.frame_bytes == 8 * MIB)
        .ok_or("legacy codec cost")?;
    let legacy =
        Choice::new(r, legacy_cost, MIB, 2.min(r.cpus), 1).ok_or("legacy memory budget")?;
    let mut known: std::collections::BTreeSet<String> =
        choices.iter().map(|x| x.id.clone()).collect();
    let mut trials = vec![];
    for choice in choices {
        event(json!({"event":"trial_start","id":choice.id}));
        trials.push(pipeline(
            source,
            &c.destination,
            &choice,
            c.trial_bytes,
            false,
        )?);
    }
    // A single short run exaggerates startup/cache/scheduler noise. Re-race
    // the leading three on a larger training sample, in reversed order too.
    let mut finalists = trials.iter().collect::<Vec<_>>();
    finalists.sort_by(|a, b| a.seconds.total_cmp(&b.seconds));
    let mut finalists: Vec<_> = finalists
        .into_iter()
        .take(3)
        .map(|m| m.choice.clone())
        .collect();
    if !finalists.iter().any(|x| x.id == legacy.id) {
        finalists.push(legacy.clone());
    }
    if let Some(dense) = trials.iter().find(|m| {
        m.choice.plan.codec == Codec::Zstd
            && m.choice.plan.compression_level == 9
            && m.choice.plan.workers == r.cpus
            && m.choice.verify_workers == r.cpus.div_ceil(2)
    }) {
        if !finalists.iter().any(|x| x.id == dense.choice.id) {
            finalists.push(dense.choice.clone());
        }
    }
    let raw_ratio = costs
        .iter()
        .filter(|x| x.codec == Codec::Raw)
        .map(|x| x.stored_ratio)
        .fold(f64::INFINITY, f64::min);
    let smallest_ratio = costs
        .iter()
        .map(|x| x.stored_ratio)
        .fold(f64::INFINITY, f64::min);
    if smallest_ratio >= raw_ratio * 0.995 {
        if let Some(raw) = trials
            .iter()
            .filter(|m| m.choice.plan.codec == Codec::Raw)
            .min_by(|a, b| a.seconds.total_cmp(&b.seconds))
        {
            if !finalists.iter().any(|x| x.id == raw.choice.id) {
                finalists.push(raw.choice.clone());
            }
        }
    }
    let refinement_bytes = (c.trial_bytes * 4).min(c.validate_bytes);
    let mut refinement = vec![];
    for round in 0..2 {
        let mut order = finalists.clone();
        if round == 1 {
            order.reverse();
        }
        for choice in order {
            refinement.push(pipeline(
                source,
                &c.destination,
                &choice,
                refinement_bytes,
                false,
            )?);
        }
    }
    for choice in &finalists {
        let times: Vec<_> = refinement
            .iter()
            .filter(|m| m.choice.id == choice.id)
            .map(|m| m.seconds)
            .collect();
        if times.iter().copied().fold(f64::NEG_INFINITY, f64::max)
            > times.iter().copied().fold(f64::INFINITY, f64::min) * 1.15
        {
            event(json!({"event":"unstable_refinement_repeated","id":choice.id,"seconds":times}));
            refinement.push(pipeline(
                source,
                &c.destination,
                choice,
                refinement_bytes,
                false,
            )?);
        }
    }
    let refined: Vec<Measurement> = finalists
        .iter()
        .map(|choice| aggregate_runs(choice, &refinement))
        .collect();
    let incumbent = refined[adaptive::select(&refined, refinement_bytes)?].clone();
    event(json!({"event":"local_search_start","incumbent":incumbent.choice}));
    let mut local_trials = vec![];
    for choice in adaptive::neighborhood(r, &costs, &incumbent)? {
        if refinement.iter().any(|m| m.choice.id == choice.id) {
            continue;
        }
        // Reserve one new candidate for a matched Raw control after the race.
        if !known.contains(&choice.id) && known.len() >= c.max_trials - 1 {
            continue;
        }
        known.insert(choice.id.clone());
        let m = pipeline(source, &c.destination, &choice, refinement_bytes, false)?;
        local_trials.push(m.clone());
        refinement.push(m);
    }
    let mut unique = vec![];
    for m in &refinement {
        if !unique.iter().any(|x: &Choice| x.id == m.choice.id) {
            unique.push(m.choice.clone());
        }
    }
    let mut ranked: Vec<_> = unique
        .iter()
        .map(|choice| aggregate_runs(choice, &refinement))
        .collect();
    ranked.sort_by(|a, b| a.seconds.total_cmp(&b.seconds));
    let mut confirmed: Vec<Choice> = ranked.iter().take(3).map(|m| m.choice.clone()).collect();
    for choice in [&incumbent.choice, &legacy] {
        if !confirmed.iter().any(|x| x.id == choice.id) {
            confirmed.push(choice.clone());
        }
    }
    confirm_three(
        source,
        &c.destination,
        &confirmed,
        refinement_bytes,
        &mut refinement,
    )?;
    let mut refined: Vec<_> = confirmed
        .iter()
        .map(|choice| aggregate_runs(choice, &refinement))
        .collect();
    let mut selected = adaptive::select(&refined, refinement_bytes)?;
    let candidate = &refined[selected];
    if let Some(raw) = costs
        .iter()
        .find(|x| x.codec == Codec::Raw && x.frame_bytes == candidate.choice.plan.range_bytes)
    {
        if candidate.choice.plan.codec != Codec::Raw
            && candidate.stored_bytes as f64 / refinement_bytes as f64 >= raw.stored_ratio * 0.995
        {
            let cp = &candidate.choice;
            let control = Choice::new(
                r,
                raw,
                cp.plan.read_bytes,
                cp.plan.workers,
                cp.verify_workers,
            )
            .ok_or("matched raw control")?;
            if !confirmed.iter().any(|x| x.id == control.id) {
                known.insert(control.id.clone());
                confirmed.push(control);
            }
            confirm_three(
                source,
                &c.destination,
                &confirmed,
                refinement_bytes,
                &mut refinement,
            )?;
            refined = confirmed
                .iter()
                .map(|choice| aggregate_runs(choice, &refinement))
                .collect();
            selected = adaptive::select(&refined, refinement_bytes)?;
        }
    }
    if known.len() > c.max_trials {
        return Err("distinct candidate budget exceeded".into());
    }
    let chosen = refined[selected].choice.clone();
    event(json!({"event":"plan_selected","choice":chosen}));
    let mut ranked = refined.iter().collect::<Vec<_>>();
    ranked.sort_by(|a, b| a.seconds.total_cmp(&b.seconds));
    let challenger = ranked
        .into_iter()
        .find(|m| m.choice.id != chosen.id && m.choice.id != legacy.id)
        .ok_or("challenger missing")?
        .choice
        .clone();
    let mut repeats = vec![];
    for round in 0..3 {
        let mut order = vec![chosen.clone(), legacy.clone(), challenger.clone()];
        let mut seen = std::collections::BTreeSet::new();
        order.retain(|x| seen.insert(x.id.clone()));
        if round == 1 {
            order.reverse();
        } else if round == 2 {
            order.rotate_left(1);
        }
        for choice in order {
            repeats.push(pipeline(
                source,
                &c.destination,
                &choice,
                c.validate_bytes,
                true,
            )?);
        }
    }
    let chosen_full = aggregate_runs(&chosen, &repeats);
    let legacy_full = aggregate_runs(&legacy, &repeats);
    let challenger_full = aggregate_runs(&challenger, &repeats);
    let best = chosen_full
        .seconds
        .min(legacy_full.seconds)
        .min(challenger_full.seconds);
    Ok(
        json!({"schema_version":1,"scope":"experimental concurrent pipeline using production codecs and BLAKE3; not production v2 archive acceptance","baseline_kind":"previous P2/zstd3 parameter policy on the SAME concurrent pipeline, not the old product binary","configured_resource_limits":limits,"device_probe":device_probe,"resources":r,"trial_bytes":c.trial_bytes,"refinement_bytes":refinement_bytes,"refinement_repeats":refinement,"refinement_aggregate":refined,"heldout_bytes":c.validate_bytes,"selection":chosen,"distinct_candidates":known.len(),"local_trials":local_trials,"trials":trials,"selected_full":chosen_full,"legacy_p2_full":legacy_full,"challenger_full":challenger_full,"validation_repeats":repeats,"validation_aggregation":"median of three counterbalanced runs; all outliers preserved","regret_vs_measured_contenders":chosen_full.seconds/best-1.0,"verification":"all persisted records sync+readback+decode+parallel BLAKE3; concurrent with copy","measured_contenders_are_not_a_global_optimality_proof":true}),
    )
}

fn main() -> AnyResult<()> {
    let path = std::env::args()
        .nth(1)
        .ok_or("configuration path required")?;
    let c: Config = serde_json::from_slice(&fs::read(path)?)?;
    if let Some(gate) = &c.start_gate {
        let wait = Instant::now();
        while !gate.is_file() {
            if wait.elapsed().as_secs() > 20 {
                return Err("container start gate timeout".into());
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
    event(
        json!({"event":"environment","available_cpus":std::thread::available_parallelism()?.get(),"configured_resources":c.resources}),
    );
    std::thread::sleep(std::time::Duration::from_millis(250));
    let report = match c.mode.as_str() {
        "generate" => generate(&c.destination)?,
        "io" => calibrate(c.source.as_ref().ok_or("source required")?, &c.destination)?,
        "costs" => serde_json::to_value(costs(
            c.source.as_ref().ok_or("source required")?,
            c.resources.as_ref().ok_or("resources required")?.cpus,
        )?)?,
        "study" => study(&c)?,
        "online-study" => online::compare(&c)?,
        _ => return Err("unknown evaluation mode".into()),
    };
    save(&c.report, &report)?;
    event(json!({"event":"complete","report":c.report}));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn median_run_preserves_matching_cpu_counters_and_all_input_measurements() {
        let choice = Choice {
            id: "median-test".into(),
            plan: Plan::current(),
            verify_workers: 1,
            conservative_memory_bytes: 512 << 20,
            predicted_bps: 1.0,
        };
        let rows: Vec<_> = [(2.0, 1.0), (21.0, 3.0), (2.5, 1.5)]
            .into_iter()
            .map(|(seconds, cpu_seconds)| Measurement {
                choice: choice.clone(),
                seconds,
                cpu_seconds,
                stored_bytes: 100,
                verified_bytes: 200,
            })
            .collect();
        let m = aggregate_runs(&choice, &rows);
        assert_eq!(m.seconds, 2.5);
        assert_eq!(m.cpu_seconds, 1.5);
        assert_eq!(rows[1].seconds, 21.0);
    }
    #[test]
    fn read_ranges_cover_disjoint_halves_without_gaps_or_overlap() {
        for frame in [8 * MIB, 16 * MIB] {
            let train = read_ranges(2048 * MIB, 256 * MIB, frame, false).unwrap();
            let mut validation = read_ranges(2048 * MIB, 2048 * MIB, frame, true).unwrap();
            assert!(train.iter().all(|(o, n)| o + n <= 2048 * MIB));
            validation.sort_unstable();
            for (i, (offset, len)) in validation.iter().enumerate() {
                assert_eq!(*offset, 2048 * MIB + i as u64 * frame);
                assert_eq!(*len, frame);
            }
        }
        assert!(read_ranges(64, 65, 8, true).is_err());
        assert!(read_ranges(64, 63, 8, true).is_err());
    }
    #[test]
    fn concurrent_pipeline_roundtrips_and_preserves_source() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("source");
        let mut bytes = vec![0; 64 << 20];
        random_fill(&mut bytes, &mut 0x914ab02u64);
        fs::write(&p, &bytes).unwrap();
        for (codec, level) in [(Codec::Raw, 0), (Codec::Zstd, 3), (Codec::Xz, 0)] {
            let plan = Plan {
                format_version: 2,
                range_bytes: MIB,
                read_bytes: 256 << 10,
                workers: 2,
                codec,
                compression_level: level,
                memory_limit_bytes: 3 << 30,
                ..Plan::current()
            };
            let c = Choice {
                id: format!("test-{}", codec.name()),
                plan,
                verify_workers: 2,
                conservative_memory_bytes: 512 << 20,
                predicted_bps: 1.0,
            };
            for heldout in [false, true] {
                let r = pipeline(&p, t.path(), &c, 32 << 20, heldout).unwrap();
                assert_eq!(r.verified_bytes, 32 << 20);
                assert!(r.seconds > 0.0);
            }
        }
        assert_eq!(fs::read(p).unwrap(), bytes);
    }
    #[test]
    fn readback_does_not_accept_corrupt_metadata_or_hash() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("payload");
        let raw = vec![0x73; 4096];
        let pool = CpuPool::new(2).unwrap();
        let hash = pool.hash(&raw);
        let encoded = codec::encode(raw, Codec::Zstd, 3).unwrap();
        let header = [0u8; 128];
        let mut f = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(p)
            .unwrap();
        f.write_all(&header).unwrap();
        f.write_all(&encoded.payload).unwrap();
        f.sync_all().unwrap();
        let mut r = Record {
            offset: 0,
            payload: encoded.payload.len(),
            raw: 4096,
            codec: encoded.codec,
            hash,
            header,
        };
        assert_eq!(verify_record(&f, &r, &pool, 512).unwrap(), 4096);
        r.hash[0] ^= 1;
        assert!(verify_record(&f, &r, &pool, 512).is_err());
        r.hash = hash;
        r.header[3] = 1;
        assert!(verify_record(&f, &r, &pool, 512).is_err());
    }
}
