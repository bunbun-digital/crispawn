// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! Host research probe using exactly the engine's codec and range hash paths.
//! Does not write archives, open block devices, or select a production plan.
use perfect_backup_core::{
    error::require,
    format::Record,
    plan::Plan,
    types::{hash, Codec},
    Error, Result,
};
use perfect_backup_engine::{codec, io};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{path::Path, time::Instant};

#[derive(Serialize)]
struct Timing {
    wall_seconds: f64,
    cpu_seconds: f64,
}
fn cpu_seconds() -> Result<f64> {
    let mut time = std::mem::MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: writable timespec, supported Linux process CPU clock.
    if unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, time.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let time = unsafe { time.assume_init() };
    Ok(time.tv_sec as f64 + time.tv_nsec as f64 * 1e-9)
}
fn measure<T>(operation: impl FnOnce() -> Result<T>) -> Result<(T, Timing)> {
    let cpu = cpu_seconds()?;
    let wall = Instant::now();
    let value = operation()?;
    Ok((
        value,
        Timing {
            wall_seconds: wall.elapsed().as_secs_f64(),
            cpu_seconds: cpu_seconds()? - cpu,
        },
    ))
}
#[derive(Serialize)]
struct RangeMeasurement {
    range_id: String,
    source_offset_bytes: String,
    source_bytes: String,
    chosen_codec: Codec,
    payload_bytes: String,
    record_bytes: String,
    allocate_and_buffered_read: Timing,
    source_hash: Timing,
    encode: Timing,
    record_and_stored_hash: Timing,
    decode_and_raw_hash: Timing,
}
fn run() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    require(
        args.len() == 4,
        "ARGUMENT_ERROR",
        "codec_probe INPUT_FILE raw|zstd|xz|gzip LEVEL RANGE_BYTES",
    )?;
    let selected = match args[1].as_str() {
        "raw" => Codec::Raw,
        "zstd" => Codec::Zstd,
        "xz" => Codec::Xz,
        "gzip" => Codec::Gzip,
        _ => return Err(Error::new("ARGUMENT_ERROR", "codec")),
    };
    let level = args[2]
        .parse()
        .map_err(|_| Error::new("ARGUMENT_ERROR", "level"))?;
    let chunk = perfect_backup_core::types::parse_decimal(&args[3])?;
    let file = io::open_regular(Path::new(&args[0]), false)?;
    io::lock_file(&file, false)?;
    let identity = io::identity(&file)?;
    let plan = Plan {
        codec: selected,
        compression_level: level,
        range_bytes: chunk,
        read_bytes: chunk.min(1 << 20),
        workers: 1,
        ..Plan::default()
    };
    plan.validate(identity.bytes)?;
    let mut ranges = vec![];
    // Input and output are capped to host research fixture sizes, not disk sizes.
    require(
        identity.bytes <= 512 << 20,
        "LIMIT_EXCEEDED",
        "probe accepts at most 512 MiB per invocation",
    )?;
    require(
        plan.range_count(identity.bytes) <= 4096,
        "LIMIT_EXCEEDED",
        "probe report is limited to 4096 ranges",
    )?;
    for id in 0..plan.range_count(identity.bytes) {
        let (offset, length) = plan.range(identity.bytes, id)?;
        let (raw, read) = measure(|| {
            let mut raw = vec![0; length as usize];
            io::read_exact_at(&file, &mut raw, offset, plan.read_bytes)?;
            Ok(raw)
        })?;
        let (raw_hash, source_hash) = measure(|| Ok(hash(&raw)))?;
        let (encoded, encode) = measure(|| codec::encode(raw, selected, level))?;
        let record = Record {
            backup_id: [0; 16],
            file_id: 0,
            range_id: id,
            source_offset: offset,
            raw_bytes: length,
            payload_bytes: encoded.payload.len() as u64,
            codec: encoded.codec,
            level: encoded.level,
            raw_hash,
        };
        let (_, stored_hash) = measure(|| {
            let header = record.encode();
            let mut h = Sha256::new();
            h.update(header);
            h.update(&encoded.payload);
            std::hint::black_box(h.finalize());
            Ok(())
        })?;
        let (_, decode) = measure(|| {
            let raw = codec::decode(encoded.codec, &encoded.payload, length as usize)?;
            require(hash(&raw) == raw_hash, "RAW_HASH_MISMATCH", "probe decode")
        })?;
        ranges.push(RangeMeasurement {
            range_id: id.to_string(),
            source_offset_bytes: offset.to_string(),
            source_bytes: length.to_string(),
            chosen_codec: encoded.codec,
            payload_bytes: encoded.payload.len().to_string(),
            record_bytes: (160 + encoded.payload.len()).to_string(),
            allocate_and_buffered_read: read,
            source_hash,
            encode,
            record_and_stored_hash: stored_hash,
            decode_and_raw_hash: decode,
        });
    }
    io::unchanged(&file, &identity)?;
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: writable rusage for the current Linux process. ru_maxrss is KiB here.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let rss = unsafe { usage.assume_init() }.ru_maxrss;
    // getrusage may include a high-water mark inherited through exec. VmHWM
    // describes the current address space; keep both rather than subtracting peaks.
    let status = std::fs::read_to_string("/proc/self/status")?;
    let hwm = status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| Error::new("RSS_MEASUREMENT_UNAVAILABLE", "/proc/self/status VmHWM"))?;
    let result = serde_json::json!({"schema_version":2,"kind":"codec_cpu_probe","source_kind":"regular-file-fixture","physical_device_benchmark":false,"source_bytes":identity.bytes.to_string(),"range_bytes":chunk.to_string(),"codec":selected,"level":level,"max_rss_kib":hwm,"rss_metric":"proc_self_status_VmHWM_approximate","rusage_max_rss_kib":rss,"ranges":ranges});
    println!(
        "{}",
        serde_json::to_string(&result).map_err(|e| Error::new("JSON_ERROR", e.to_string()))?
    );
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
