// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-digital. Licensed under Apache-2.0.
//! Optional temporary I/O diagnostics. Workers update a fixed RAM mapping;
//! a separate process samples it and /proc without depending on destination I/O.
//! Durations are inclusive wall times, not CPU time or guaranteed media latency.
use crate::{Error, Result};
use serde_json::{json, Value};
use std::{
    cell::Cell,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{fs::OpenOptionsExt, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    ptr::NonNull,
    sync::{
        atomic::{AtomicU32, AtomicU64, Ordering},
        Mutex, OnceLock,
    },
    time::{Duration, Instant},
};
const MAGIC: u64 = 0x4350494f54524331;
const SLOTS: usize = 256;
const OPS: usize = 23;
const TRACE_CAP: u64 = 8 << 20;
fn encoded(value: &Value) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|e| Error::new("JSON_ERROR", e.to_string()))
}
static MAP: OnceLock<Mapping> = OnceLock::new();
static OWNER: OnceLock<Mutex<Option<Child>>> = OnceLock::new();
static RATE: OnceLock<Mutex<Rate>> = OnceLock::new();
static DEVICE_INFO: OnceLock<Mutex<Option<(String, Value)>>> = OnceLock::new();
struct Registration(Cell<Option<usize>>);
thread_local! { static SLOT: Registration = const { Registration(Cell::new(None)) }; }
impl Drop for Registration {
    fn drop(&mut self) {
        if let (Some(index), Some(map)) = (self.0.get(), MAP.get()) {
            let slot = &map.shared().slots[index];
            slot.version.fetch_add(1, Ordering::AcqRel);
            slot.operation.store(0, Ordering::Relaxed);
            slot.started_ns.store(0, Ordering::Relaxed);
            slot.bytes.store(0, Ordering::Relaxed);
            slot.version.fetch_add(1, Ordering::Release);
            slot.tid.store(0, Ordering::Release);
        }
    }
}

#[repr(usize)]
#[derive(Clone, Copy)]
pub(crate) enum Op {
    Read = 1,
    Hash,
    Encode,
    Directory,
    Create,
    DataWrite,
    Readback,
    DescriptorWrite,
    Rename,
    LogWrite,
    FileSync,
    FilesystemSync,
    FreeSpace,
    Write,
    TraceExport,
    QueueWait,
    ReceiptWait,
    SourceRead,
    SourceCache,
    /// RAM-backed runtime files (/run, /dev/shm), kept apart from NTFS work.
    RuntimeCreate,
    RuntimeRename,
    RuntimeSync,
}
fn name(index: usize) -> &'static str {
    match index {
        1 => "file_read",
        2 => "blake3",
        3 => "compression",
        4 => "directory",
        5 => "file_create",
        6 => "backup_data_write",
        7 => "stored_data_readback",
        8 => "descriptor_write",
        9 => "rename",
        10 => "log_append",
        11 => "file_sync",
        12 => "filesystem_sync",
        13 => "free_space_query",
        14 => "other_write",
        15 => "trace_export",
        16 => "writer_queue_wait",
        17 => "writer_receipt_wait",
        18 => "source_read",
        19 => "source_cache_release",
        20 => "runtime_file_create",
        21 => "runtime_rename",
        22 => "runtime_file_sync",
        _ => "idle",
    }
}
#[repr(C)]
struct Slot {
    version: AtomicU64,
    tid: AtomicU32,
    operation: AtomicU32,
    started_ns: AtomicU64,
    bytes: AtomicU64,
}
#[repr(C)]
struct Metric {
    calls: AtomicU64,
    wall_ns: AtomicU64,
    max_ns: AtomicU64,
    bytes: AtomicU64,
}
#[repr(C)]
struct Shared {
    magic: AtomicU64,
    pid: AtomicU32,
    overflow: AtomicU32,
    slots: [Slot; SLOTS],
    metrics: [Metric; OPS],
}
struct Mapping {
    pointer: NonNull<Shared>,
    root: PathBuf,
}
// SAFETY: the mapping lives until process exit and every shared field is atomic.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}
impl Mapping {
    fn open(root: &Path, create: bool, pid: u32) -> Result<Self> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(create)
            .custom_flags(libc::O_NOFOLLOW)
            .mode(0o600);
        if create {
            options.create_new(true);
        }
        let file = options.open(root.join("operations.bin"))?;
        let bytes = std::mem::size_of::<Shared>();
        if create {
            file.set_len(bytes as u64)?;
        }
        if !file.metadata()?.is_file() || file.metadata()?.len() != bytes as u64 {
            return Err(Error::new(
                "IO_OBSERVER_LAYOUT",
                "invalid RAM trace mapping",
            ));
        }
        // SAFETY: map exactly the validated regular file. Writers use atomics;
        // the observer maps read-only and only performs atomic loads.
        let pointer = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                bytes,
                libc::PROT_READ | if create { libc::PROT_WRITE } else { 0 },
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if pointer == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error().into());
        }
        let mapping = Self {
            pointer: NonNull::new(pointer.cast()).unwrap(),
            root: root.into(),
        };
        if create {
            let empty = || Metric {
                calls: AtomicU64::new(0),
                wall_ns: AtomicU64::new(0),
                max_ns: AtomicU64::new(0),
                bytes: AtomicU64::new(0),
            };
            let value = Shared {
                magic: AtomicU64::new(0),
                pid: AtomicU32::new(pid),
                overflow: AtomicU32::new(0),
                slots: std::array::from_fn(|_| Slot {
                    version: AtomicU64::new(0),
                    tid: AtomicU32::new(0),
                    operation: AtomicU32::new(0),
                    started_ns: AtomicU64::new(0),
                    bytes: AtomicU64::new(0),
                }),
                metrics: std::array::from_fn(|_| empty()),
            };
            // SAFETY: this new private file is initialized before publication.
            unsafe {
                mapping.pointer.as_ptr().write(value);
            }
            mapping.shared().magic.store(MAGIC, Ordering::Release);
        }
        if mapping.shared().magic.load(Ordering::Acquire) != MAGIC
            || mapping.shared().pid.load(Ordering::Relaxed) != pid
        {
            return Err(Error::new(
                "IO_OBSERVER_LAYOUT",
                "RAM trace PID or version mismatch",
            ));
        }
        Ok(mapping)
    }
    fn shared(&self) -> &Shared {
        // SAFETY: mmap owns a complete initialized Shared; all access is atomic.
        unsafe { self.pointer.as_ref() }
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: this owns the mapping; references cannot outlive self.
        unsafe {
            libc::munmap(self.pointer.as_ptr().cast(), std::mem::size_of::<Shared>());
        }
    }
}
fn monotonic_ns() -> u64 {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes to an owned timespec.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut value) } != 0 {
        return 0;
    }
    (value.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(value.tv_nsec as u64)
}
pub(crate) struct Span {
    index: usize,
    operation: usize,
    started: u64,
    bytes: u64,
    previous: (u32, u64, u64),
    _same_thread: std::marker::PhantomData<*mut ()>,
}
pub(crate) fn span(operation: Op, bytes: u64) -> Option<Span> {
    enter(operation, bytes, false)
}
pub(crate) fn span_if_idle(operation: Op, bytes: u64) -> Option<Span> {
    enter(operation, bytes, true)
}
pub(crate) fn elapsed(operation: Op, elapsed: Duration, bytes: u64) {
    if let Some(map) = MAP.get() {
        let ns = elapsed.as_nanos().min(u128::from(u64::MAX)) as u64;
        let metric = &map.shared().metrics[operation as usize];
        metric.calls.fetch_add(1, Ordering::Relaxed);
        metric.wall_ns.fetch_add(ns, Ordering::Relaxed);
        metric.max_ns.fetch_max(ns, Ordering::Relaxed);
        metric.bytes.fetch_add(bytes, Ordering::Relaxed);
    }
}
fn enter(operation: Op, bytes: u64, idle_only: bool) -> Option<Span> {
    let map = MAP.get()?;
    let shared = map.shared();
    let index = SLOT.with(|cached| {
        if let Some(i) = cached.0.get() {
            return Some(i);
        }
        // SAFETY: gettid has no arguments and identifies only this thread.
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as u32;
        let found = shared.slots.iter().position(|slot| {
            slot.tid
                .compare_exchange(0, tid, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        });
        if found.is_none() {
            shared.overflow.fetch_add(1, Ordering::Relaxed);
        }
        cached.0.set(found);
        found
    })?;
    let slot = &shared.slots[index];
    let previous = (
        slot.operation.load(Ordering::Relaxed),
        slot.started_ns.load(Ordering::Relaxed),
        slot.bytes.load(Ordering::Relaxed),
    );
    if idle_only && previous.0 != 0 {
        return None;
    }
    let started = monotonic_ns();
    slot.version.fetch_add(1, Ordering::AcqRel);
    slot.started_ns.store(started, Ordering::Relaxed);
    slot.bytes.store(bytes, Ordering::Relaxed);
    slot.operation.store(operation as u32, Ordering::Relaxed);
    slot.version.fetch_add(1, Ordering::Release);
    Some(Span {
        index,
        operation: operation as usize,
        started,
        bytes,
        previous,
        _same_thread: std::marker::PhantomData,
    })
}
impl Drop for Span {
    fn drop(&mut self) {
        let shared = MAP.get().unwrap().shared();
        let elapsed = monotonic_ns().saturating_sub(self.started);
        let metric = &shared.metrics[self.operation];
        metric.calls.fetch_add(1, Ordering::Relaxed);
        metric.wall_ns.fetch_add(elapsed, Ordering::Relaxed);
        metric.max_ns.fetch_max(elapsed, Ordering::Relaxed);
        metric.bytes.fetch_add(self.bytes, Ordering::Relaxed);
        let slot = &shared.slots[self.index];
        slot.version.fetch_add(1, Ordering::AcqRel);
        slot.operation.store(self.previous.0, Ordering::Relaxed);
        slot.started_ns.store(self.previous.1, Ordering::Relaxed);
        slot.bytes.store(self.previous.2, Ordering::Relaxed);
        slot.version.fetch_add(1, Ordering::Release);
    }
}
pub(crate) fn start(runtime: &Path) -> Result<()> {
    let root = runtime.join("io-observer");
    fs::create_dir(&root)?;
    let pid = std::process::id();
    let mapping = Mapping::open(&root, true, pid)?;
    MAP.set(mapping)
        .map_err(|_| Error::new("IO_OBSERVER_ACTIVE", "trace already active"))?;
    let child = Command::new(std::env::current_exe()?)
        .arg("observe-io")
        .arg(&root)
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    OWNER
        .set(Mutex::new(Some(child)))
        .map_err(|_| Error::new("IO_OBSERVER_ACTIVE", "observer already active"))?;
    Ok(())
}
pub(crate) fn stop() {
    let Some(map) = MAP.get() else { return };
    let _ = fs::write(map.root.join("stop"), b"stop\n");
    if let Some(owner) = OWNER.get() {
        let mut owner = owner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut child) = owner.take() {
            let until = Instant::now() + Duration::from_secs(3);
            while Instant::now() < until {
                if child.try_wait().ok().flatten().is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = child.kill();
            let _ = child.try_wait();
        }
    }
}
pub(crate) fn export(target: &Path) -> Result<()> {
    let Some(map) = MAP.get() else { return Ok(()) };
    let _span = span(Op::TraceExport, 0);
    let output = target.join("io-observer");
    fs::create_dir(&output)?;
    for item in fs::read_dir(&map.root)? {
        let item = item?;
        let name = item.file_name();
        let text = name.to_string_lossy();
        if (text.starts_with("samples-") && text.ends_with(".jsonl"))
            || text == "latest.json"
            || text == "finished.json"
        {
            let input = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(item.path())?;
            if !input.metadata()?.is_file() || input.metadata()?.len() > TRACE_CAP {
                continue;
            }
            let length = input.metadata()?.len() as usize;
            let mut bytes = vec![0; length];
            crate::io::read_exact_at(&input, &mut bytes, 0, 65536)?;
            if text.ends_with(".jsonl") {
                let end = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
                bytes.truncate(end);
            }
            let mut output_file = crate::io::create_new(&output.join(name))?;
            crate::io::write_all(&mut output_file, &bytes)?;
        }
    }
    Ok(())
}
pub(crate) fn export_final(target: &Path) -> Result<()> {
    let Some(map) = MAP.get() else { return Ok(()) };
    for (source, name) in [
        ("latest.json", "final.json"),
        ("finished.json", "finished.json"),
    ] {
        let source = map.root.join(source);
        if source.is_file() {
            let input = File::open(source)?;
            let mut output = crate::io::create_new(&target.join("io-observer").join(name))?;
            crate::io::copy_file(&input, &mut output)?;
        }
    }
    Ok(())
}
fn device_info(number: &str) -> Value {
    let mut cache = DEVICE_INFO
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some((key, value)) = &*cache {
        if key == number {
            return value.clone();
        }
    }
    let path = PathBuf::from(format!("/sys/dev/block/{number}"));
    let canonical = fs::canonicalize(&path).ok();
    let mut attributes = serde_json::Map::new();
    if let Some(path) = &canonical {
        for parent in path.ancestors().take(16) {
            for name in [
                "idVendor",
                "idProduct",
                "manufacturer",
                "product",
                "speed",
                "bInterfaceProtocol",
            ] {
                if !attributes.contains_key(name) && parent.join(name).is_file() {
                    attributes.insert(name.into(), text(parent.join(name), 256));
                }
            }
            if parent.join("queue").is_dir() {
                for name in [
                    "scheduler",
                    "nr_requests",
                    "max_sectors_kb",
                    "logical_block_size",
                    "physical_block_size",
                ] {
                    attributes.insert(
                        format!("queue_{name}"),
                        text(parent.join("queue").join(name), 256),
                    );
                }
            }
        }
    }
    let value = json!({"sysfs_path":canonical,"attributes":attributes});
    *cache = Some((number.into(), value.clone()));
    value
}
/// Page-cache writeback counters (pages) that explain balance_dirty_pages waits.
fn vmstat() -> Value {
    const KEYS: [&str; 6] = [
        "nr_dirty",
        "nr_writeback",
        "nr_dirty_threshold",
        "nr_dirty_background_threshold",
        "nr_written",
        "nr_dirtied",
    ];
    let mut out = serde_json::Map::new();
    if let Value::String(s) = text("/proc/vmstat", 16384) {
        for line in s.lines() {
            let mut fields = line.split_whitespace();
            if let (Some(key), Some(value)) = (fields.next(), fields.next()) {
                if KEYS.contains(&key) {
                    if let Ok(v) = value.parse::<u64>() {
                        out.insert(key.into(), json!(v));
                    }
                }
            }
        }
    }
    Value::Object(out)
}
/// Dirty page-cache ceiling for the backup destination device only.
pub(crate) const DESTINATION_DIRTY_BYTES: u64 = 256 << 20;
/// Bound the destination's dirty page cache so that a slow USB device does
/// not accumulate minutes of unwritten data. Throughput remains limited by
/// the device; the final sync, failure log export and unmount then complete
/// within a bounded time. Best effort: failures are reported, never fatal.
pub(crate) fn limit_destination_writeback(number: &str) -> Value {
    if !valid_number(number) {
        return json!({"applied":false,"error":"invalid device number"});
    }
    let Some(bdi) = bdi_dir(number) else {
        return json!({"applied":false,"error":"bdi not found"});
    };
    let previous = bdi_info(number);
    let mut errors = vec![];
    for (name, value) in [
        ("max_bytes", DESTINATION_DIRTY_BYTES.to_string()),
        ("strict_limit", "1".to_string()),
    ] {
        if let Err(error) = fs::write(bdi.join(name), &value) {
            errors.push(json!({"setting":name,"errno":error.raw_os_error()}));
        }
    }
    json!({"applied":errors.is_empty(),"requested_max_bytes":DESTINATION_DIRTY_BYTES,
        "previous":previous,"current":bdi_info(number),"errors":errors})
}
fn bdi_dir(number: &str) -> Option<std::path::PathBuf> {
    fs::read_link(format!("/sys/dev/block/{number}"))
        .ok()
        .and_then(|link| {
            let path = std::path::Path::new("/sys/dev/block").join(link);
            // A partition's parent directory is the whole disk with the bdi.
            let parent = if path.join("partition").exists() {
                path.parent()?.to_path_buf()
            } else {
                path
            };
            Some(parent.join("bdi"))
        })
}
/// Per-device writeback settings (read-only observation).
fn bdi_info(number: &str) -> Value {
    let Some(bdi) = bdi_dir(number) else {
        return Value::Null;
    };
    json!({"max_ratio":text(bdi.join("max_ratio"),64),"min_ratio":text(bdi.join("min_ratio"),64),
        "max_bytes":text(bdi.join("max_bytes"),64),"strict_limit":text(bdi.join("strict_limit"),64),
        "read_ahead_kb":text(bdi.join("read_ahead_kb"),64)})
}
fn text(path: impl AsRef<Path>, cap: u64) -> Value {
    match File::open(path).and_then(|file| {
        let mut s = String::new();
        file.take(cap).read_to_string(&mut s)?;
        Ok(s)
    }) {
        Ok(s) => json!(s),
        Err(e) => json!({"unavailable":e.raw_os_error()}),
    }
}
fn sample(map: &Mapping, pid: u32, stacks: bool) -> Value {
    // SAFETY: sysconf only reads the process clock-tick configuration.
    let clock_ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    let now = monotonic_ns();
    let shared = map.shared();
    let mut active = vec![];
    for slot in &shared.slots {
        let v = slot.version.load(Ordering::Acquire);
        if v % 2 != 0 {
            continue;
        }
        let op = slot.operation.load(Ordering::Relaxed);
        if op == 0 {
            continue;
        }
        let tid = slot.tid.load(Ordering::Relaxed);
        let started = slot.started_ns.load(Ordering::Relaxed);
        let bytes = slot.bytes.load(Ordering::Relaxed);
        if slot.version.load(Ordering::Acquire) != v {
            continue;
        }
        active.push((now.saturating_sub(started), tid, op, bytes));
    }
    active.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    let active:Vec<_>=active.into_iter().take(16).enumerate().map(|(i,(ns,tid,op,bytes))| {
        let root=PathBuf::from(format!("/proc/{pid}/task/{tid}"));
        let mut value=json!({"tid":tid,"operation":name(op as usize),"elapsed_ms":ns/1_000_000,
            "requested_bytes":bytes,"comm":text(root.join("comm"),128),"state":text(root.join("stat"),512),
            "wchan":text(root.join("wchan"),256),"syscall":text(root.join("syscall"),512)});
        if stacks && ns>=5_000_000_000 && i<2 {value["kernel_stack"]=text(root.join("stack"),4096);}
        value
    }).collect();
    let metrics:Vec<_>=shared.metrics.iter().enumerate().filter_map(|(i,m)| {
        let calls=m.calls.load(Ordering::Relaxed);
        (calls>0).then(||json!({"operation":name(i),"calls":calls,"inclusive_wall_ms":m.wall_ns.load(Ordering::Relaxed)/1_000_000,
            "max_wall_ms":m.max_ns.load(Ordering::Relaxed)/1_000_000,"requested_bytes":m.bytes.load(Ordering::Relaxed)}))
    }).collect();
    let device = fs::read_to_string(map.root.join("device-number"))
        .ok()
        .filter(|s| valid_number(s));
    let block = device.as_ref().map(|number| {
        json!({"number":number,"stat":text(format!("/sys/dev/block/{number}/stat"),2048),
        "inflight":text(format!("/sys/dev/block/{number}/inflight"),128)})
    });
    json!({"kind":"crispawn.io_sample","schema_version":1,"worker_pid":pid,
        "monotonic_ms":now/1_000_000,"unix_ms":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis().to_string(),
        "active":active,"operations":metrics,"slot_overflow":shared.overflow.load(Ordering::Relaxed),
        "process_io":text(format!("/proc/{pid}/io"),2048),"process_stat":text(format!("/proc/{pid}/stat"),2048),
        "clock_ticks_per_second":clock_ticks,"logical_cpus":std::thread::available_parallelism().map(|n|n.get()).unwrap_or(0),
        "memory":crate::memory::sample(),"io_pressure":text("/proc/pressure/io",1024),
        "dirty_limits":{"bytes":text("/proc/sys/vm/dirty_bytes",64),"ratio":text("/proc/sys/vm/dirty_ratio",64),
            "background_bytes":text("/proc/sys/vm/dirty_background_bytes",64),"background_ratio":text("/proc/sys/vm/dirty_background_ratio",64)},
        "vmstat":vmstat(),"bdi":device.as_deref().map(bdi_info),
        "destination":block,"device_info":device.as_deref().map(device_info),"observer_pid":std::process::id(),
        "note":"inclusive elapsed times may overlap; block completion is not a durability guarantee"})
}
/// Internal diagnostic child. Reads only the worker's RAM mapping and kernel
/// counters; writes a bounded RAM ring, never the destination filesystem.
pub fn observe(root: &Path, pid: u32) -> Result<()> {
    let map = Mapping::open(root, false, pid)?;
    let mut sequence = 0u64;
    let mut used = 0u64;
    let mut current: Option<File> = None;
    let mut names = std::collections::VecDeque::new();
    let mut count = 0u64;
    loop {
        let value = sample(&map, pid, count % 5 == 0);
        let mut bytes = encoded(&value)?;
        bytes.push(b'\n');
        if current.is_none() || used + bytes.len() as u64 > TRACE_CAP / 4 {
            if names.len() == 4 {
                fs::remove_file(names.pop_front().unwrap())?;
            }
            let path = root.join(format!("samples-{sequence:06}.jsonl"));
            sequence += 1;
            current = Some(
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)?,
            );
            names.push_back(path);
            used = 0;
        }
        current.as_mut().unwrap().write_all(&bytes)?;
        used += bytes.len() as u64;
        let latest = root.join(".latest.json");
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&latest)?;
        output.write_all(&bytes)?;
        drop(output);
        fs::rename(latest, root.join("latest.json"))?;
        if root.join("stop").exists() || !Path::new(&format!("/proc/{pid}")).exists() {
            fs::write(
                root.join("finished.json"),
                encoded(
                    &json!({"samples":count+1,"worker_pid":pid,"bounded_ram_bytes":TRACE_CAP}),
                )?,
            )?;
            return Ok(());
        }
        count += 1;
        std::thread::sleep(Duration::from_secs(1));
    }
}
fn valid_number(s: &str) -> bool {
    s.split_once(':').is_some_and(|(a, b)| {
        !a.is_empty() && !b.is_empty() && a.parse::<u32>().is_ok() && b.parse::<u32>().is_ok()
    })
}
#[derive(Default)]
struct Rate {
    number: Option<String>,
    previous: Option<(Instant, u64)>,
    bps: Option<f64>,
}
pub(crate) fn destination(number: &str) {
    if !valid_number(number) {
        return;
    }
    let mut rate = RATE
        .get_or_init(|| Mutex::new(Rate::default()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    rate.number = Some(number.into());
    rate.previous = None;
    rate.bps = None;
    if let Some(map) = MAP.get() {
        let _ = fs::write(map.root.join("device-number"), number);
    }
}
pub(crate) fn destination_bps() -> Option<f64> {
    let mut rate = RATE.get()?.lock().unwrap_or_else(|e| e.into_inner());
    if rate
        .previous
        .is_some_and(|(t, _)| t.elapsed() < Duration::from_secs(1))
    {
        return rate.bps;
    }
    let counters =
        fs::read_to_string(format!("/sys/dev/block/{}/stat", rate.number.as_ref()?)).ok()?;
    let written: u64 = counters.split_whitespace().nth(6)?.parse().ok()?;
    let now = Instant::now();
    if let Some((time, old)) = rate.previous.replace((now, written)) {
        if written > old {
            let value = (written - old) as f64 * 512.0 / now.duration_since(time).as_secs_f64();
            rate.bps = Some(rate.bps.map_or(value, |old| old * 0.75 + value * 0.25));
        }
    }
    rate.bps
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mapping_rejects_wrong_pid_and_records_active_and_finished_operations() {
        let root = tempfile::tempdir().unwrap();
        let pid = std::process::id();
        let map = Mapping::open(root.path(), true, pid).unwrap();
        assert!(Mapping::open(root.path(), false, pid + 1).is_err());
        assert!(MAP.set(map).is_ok());
        let observer = Mapping::open(root.path(), false, pid).unwrap();
        let operation = span(Op::LogWrite, 512).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let active = sample(&observer, pid, false);
        assert!(active["active"]
            .as_array()
            .unwrap()
            .iter()
            .any(
                |row| row["operation"] == "log_append" && row["elapsed_ms"].as_u64().unwrap() >= 20
            ));
        drop(operation);
        let finished = sample(&observer, pid, false);
        assert!(finished["operations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["operation"] == "log_append" && row["calls"].as_u64().unwrap() > 0));
        assert!(encoded(&finished).unwrap().len() < 32 << 10);
        assert!(finished["dirty_limits"].is_object());
        assert!(!valid_number("8:2/../../"));
    }
    #[test]
    fn destination_writeback_limit_reports_unknown_devices_without_failing() {
        let value = limit_destination_writeback("4095:1048575");
        assert_eq!(value["applied"], false);
        assert_eq!(limit_destination_writeback("../x")["applied"], false);
        assert!(vmstat().is_object());
    }
}
