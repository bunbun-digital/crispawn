// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
use perfect_backup_core::{
    candidates::CandidateGrid,
    plan::Plan,
    types::{parse_decimal, Codec, EventSink, Snapshot},
    Error, Result,
};
use perfect_backup_engine::{backup, restore, verify, NoHooks};
use std::{
    collections::BTreeMap,
    io::Write,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc,
    },
    time::Duration,
};

static CANCEL: AtomicBool = AtomicBool::new(false);
extern "C" fn cancel_signal(_: libc::c_int) {
    CANCEL.store(true, Ordering::Relaxed);
}
struct Output {
    tx: mpsc::SyncSender<serde_json::Value>,
    dropped: Arc<AtomicU64>,
}
impl EventSink for Output {
    fn emit(&self, snapshot: Snapshot) {
        if let Ok(mut value) = serde_json::to_value(snapshot) {
            value
                .as_object_mut()
                .unwrap()
                .insert("type".into(), "snapshot".into());
            self.send(value);
        }
    }
}
impl Output {
    fn send(&self, value: serde_json::Value) {
        if self.tx.try_send(value).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("candidates") {
        // This is a read-only data response, not a running backup's diagnostics.
        // Write synchronously so a large catalogue cannot be dropped at process exit.
        let result=candidate_catalog(&args[1..]).and_then(|value|{
            let stdout=std::io::stdout();let mut out=stdout.lock();
            serde_json::to_writer(&mut out,&serde_json::json!({"schema_version":1,"type":"result","success":true,"result":value})).map_err(|e|Error::new("OUTPUT_ERROR",e.to_string()))?;
            out.write_all(b"\n")?;out.flush()?;Ok(())
        });
        if let Err(e) = result {
            eprintln!("{e}");
            std::process::exit(1);
        }
        return;
    }
    match perfect_backup_engine::supervisor::entry(perfect_backup_engine::supervisor::Mode::Cui) {
        Ok(Some(code)) => std::process::exit(code),
        Ok(None) => {}
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
    let (tx, rx) = mpsc::sync_channel::<serde_json::Value>(64);
    let (done_tx, done_rx) = mpsc::channel();
    let dropped = Arc::new(AtomicU64::new(0));
    let output = Output {
        tx,
        dropped: Arc::clone(&dropped),
    };
    // A slow/disconnected observer cannot hold an engine lock or block archive I/O.
    std::thread::spawn(move || {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        for value in rx {
            if serde_json::to_writer(&mut out, &value).is_err()
                || out.write_all(b"\n").is_err()
                || out.flush().is_err()
            {
                break;
            }
        }
        let _ = done_tx.send(());
    });
    let result = run(args, &output);
    let code = match result {
        Ok(value) => {
            output.send(serde_json::json!({"schema_version":1,"type":"result","success":true,"result":value,"dropped_events":dropped.load(Ordering::Relaxed)}));
            0
        }
        Err(e) => {
            perfect_backup_engine::supervisor::failure(&e);
            output.send(serde_json::json!({"schema_version":1,"type":"result","success":false,"code":e.code,"detail":e.detail,"dropped_events":dropped.load(Ordering::Relaxed)}));
            eprintln!("{}: {}", e.code, e.detail);
            if e.code == "CANCELLED" {
                130
            } else {
                1
            }
        }
    };
    drop(output);
    let _ = done_rx.recv_timeout(Duration::from_millis(200));
    std::process::exit(code);
}
fn run(args: Vec<String>, events: &dyn EventSink) -> Result<serde_json::Value> {
    if args.is_empty() || args[0] == "--help" || args[0] == "help" {
        eprintln!("perfect-backup-engine (regular-file development adapter)\n  candidates --source-bytes N [--memory-limit-bytes N]\n  backup --source-file FILE --archive NEW_DIR [options]\n  resume --source-file FILE --archive DIR\n  verify --archive DIR\n  restore --archive DIR --target-file NEW_FILE [--overwrite-file]\nOptions: --codec raw|zstd|xz|gzip --level N --range-bytes N --container-bytes N\n  --read-bytes N --workers N --regions N --checkpoint-records N --memory-limit-bytes N\n  --scratch-dir EXISTING_DIR --no-gui\nOffsets/byte counts are unsigned decimal. Existing archive directories are never overwritten.\nBlock devices and automatic disk selection are not enabled.");
        return Ok(serde_json::json!({"help":true}));
    }
    let command = &args[0];
    let mut options = BTreeMap::<String, String>::new();
    let mut overwrite = false;
    let mut i = 1;
    while i < args.len() {
        let key = &args[i];
        if key == "--no-gui" {
            i += 1;
            continue;
        }
        if key == "--overwrite-file" {
            if overwrite {
                return Err(Error::new("ARGUMENT_ERROR", "duplicate --overwrite-file"));
            }
            overwrite = true;
            i += 1;
            continue;
        }
        if !key.starts_with("--") || i + 1 == args.len() {
            return Err(Error::new("ARGUMENT_ERROR", "expected --option value"));
        }
        if options.insert(key.clone(), args[i + 1].clone()).is_some() {
            return Err(Error::new("ARGUMENT_ERROR", "duplicate option"));
        }
        i += 2;
    }
    let archive = PathBuf::from(take(&mut options, "--archive")?);
    let scratch = options
        .remove("--scratch-dir")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    if !scratch.is_dir() {
        return Err(Error::new("ARGUMENT_ERROR", "scratch directory must exist"));
    }
    let result = match command.as_str() {
        "backup" => {
            if overwrite {
                return Err(Error::new(
                    "ARGUMENT_ERROR",
                    "--overwrite-file is only valid for restore",
                ));
            }
            let source = PathBuf::from(take(&mut options, "--source-file")?);
            let mut plan = Plan::current();
            if let Some(codec) = options.remove("--codec") {
                plan.codec = match codec.as_str() {
                    "raw" => Codec::Raw,
                    "zstd" => Codec::Zstd,
                    "xz" => Codec::Xz,
                    "gzip" => Codec::Gzip,
                    _ => return Err(Error::new("ARGUMENT_ERROR", "unknown codec")),
                };
            }
            plan.compression_level = match plan.codec {
                Codec::Raw => 0,
                Codec::Gzip => 6,
                _ => 3,
            };
            if let Some(level) = options.remove("--level") {
                plan.compression_level = level
                    .parse()
                    .map_err(|_| Error::new("ARGUMENT_ERROR", "invalid compression level"))?;
            }
            for (key, field) in [
                ("--range-bytes", &mut plan.range_bytes),
                ("--container-bytes", &mut plan.container_bytes),
                ("--read-bytes", &mut plan.read_bytes),
                ("--memory-limit-bytes", &mut plan.memory_limit_bytes),
            ] {
                if let Some(v) = options.remove(key) {
                    *field = parse_decimal(&v)?;
                }
            }
            for (key, field) in [
                ("--workers", &mut plan.workers),
                ("--regions", &mut plan.regions),
                ("--checkpoint-records", &mut plan.checkpoint_records),
            ] {
                if let Some(v) = options.remove(key) {
                    *field = u32::try_from(parse_decimal(&v)?)
                        .map_err(|_| Error::new("ARGUMENT_ERROR", "u32 overflow"))?;
                }
            }
            no_unknown(&options)?;
            install_signals();
            backup::backup(&source, &archive, plan, &scratch, events, &NoHooks, &CANCEL)?
        }
        "resume" => {
            if overwrite {
                return Err(Error::new(
                    "ARGUMENT_ERROR",
                    "--overwrite-file is only valid for restore",
                ));
            }
            let source = PathBuf::from(take(&mut options, "--source-file")?);
            no_unknown(&options)?;
            install_signals();
            backup::resume(&source, &archive, &scratch, events, &NoHooks, &CANCEL)?
        }
        "verify" => {
            if overwrite {
                return Err(Error::new(
                    "ARGUMENT_ERROR",
                    "--overwrite-file is only valid for restore",
                ));
            }
            no_unknown(&options)?;
            verify::verify(&archive, &scratch, events)?
        }
        "restore" => {
            let target = PathBuf::from(take(&mut options, "--target-file")?);
            no_unknown(&options)?;
            restore::restore(&archive, &target, overwrite, &scratch, events, &NoHooks)?
        }
        _ => return Err(Error::new("ARGUMENT_ERROR", "unknown command")),
    };
    serde_json::to_value(result).map_err(|e| Error::new("JSON_ERROR", e.to_string()))
}
fn candidate_catalog(args: &[String]) -> Result<serde_json::Value> {
    if args.len() % 2 != 0 {
        return Err(Error::new("ARGUMENT_ERROR", "expected --option value"));
    }
    let mut options = BTreeMap::new();
    for pair in args.chunks_exact(2) {
        if options.insert(pair[0].clone(), pair[1].clone()).is_some() {
            return Err(Error::new("ARGUMENT_ERROR", "duplicate option"));
        }
    }
    let source_bytes = parse_decimal(&take(&mut options, "--source-bytes")?)?;
    let mut template = Plan::current();
    if let Some(memory) = options.remove("--memory-limit-bytes") {
        template.memory_limit_bytes = parse_decimal(&memory)?;
    }
    no_unknown(&options)?;
    let cases = CandidateGrid::default().evaluate(&template, source_bytes)?;
    let eligible = cases.iter().filter(|case| case.is_eligible()).count();
    Ok(
        serde_json::json!({"operation":"candidates","source_bytes":source_bytes.to_string(),"memory_limit_bytes":template.memory_limit_bytes.to_string(),"eligible_count":eligible,"rejected_count":cases.len()-eligible,"has_feasible_candidates":eligible>0,"candidates":cases}),
    )
}
fn take(options: &mut BTreeMap<String, String>, key: &str) -> Result<String> {
    options
        .remove(key)
        .ok_or_else(|| Error::new("ARGUMENT_ERROR", format!("missing {key}")))
}
fn no_unknown(options: &BTreeMap<String, String>) -> Result<()> {
    if options.is_empty() {
        Ok(())
    } else {
        Err(Error::new(
            "ARGUMENT_ERROR",
            format!("unknown option {}", options.keys().next().unwrap()),
        ))
    }
}
fn install_signals() {
    // SAFETY: static handler only sets a lock-free atomic; no allocation, locks or I/O.
    unsafe {
        libc::signal(libc::SIGINT, cancel_signal as *const () as usize);
        libc::signal(libc::SIGTERM, cancel_signal as *const () as usize);
    }
}
