// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! Independent CLI process supervisor. Never waits indefinitely for a worker,
//! collector, filesystem sync, or child-process shutdown.
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    io::{Seek, SeekFrom, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{net::UnixDatagram, process::CommandExt},
    },
    process::{Child, Command},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        OnceLock,
    },
    time::{Duration, Instant},
};
const SOCKET_ENV: &str = "CRISPAWN_SUPERVISOR_FD";
static SOCKET: OnceLock<Option<UnixDatagram>> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();
static LAST_PROGRESS: AtomicU64 = AtomicU64::new(0);
static INTERRUPTED: AtomicBool = AtomicBool::new(false);
thread_local! {
    // Status and diagnostic writes do not prove that disk work is progressing.
    static OBSERVER_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
pub fn observer_thread() {
    OBSERVER_THREAD.with(|observer| observer.set(true));
}
pub(crate) fn managed() -> bool {
    SOCKET.get().is_some_and(|socket| socket.is_some())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    pub no_progress_seconds: u32,
    pub failure_grace_seconds: u32,
    pub shutdown_grace_seconds: u32,
    pub transient_retries: u32,
    pub retry_delay_ms: u32,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            no_progress_seconds: 180,
            failure_grace_seconds: 5,
            shutdown_grace_seconds: 20,
            transient_retries: 3,
            retry_delay_ms: 100,
        }
    }
}
impl Policy {
    pub fn validate(&self) -> Result<()> {
        crispawn_core::error::require(
            (1..=3600).contains(&self.no_progress_seconds)
                && (1..=120).contains(&self.failure_grace_seconds)
                && (1..=120).contains(&self.shutdown_grace_seconds)
                && self.transient_retries <= 8
                && (1..=2000).contains(&self.retry_delay_ms),
            "INVALID_CONFIG",
            "I/O stall timeout 1..3600s; failure/shutdown grace 1..120s",
        )
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Cui,
    Live,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Notice {
    Progress,
    Active,
    Idle,
    ShutdownRequested,
    CancelRequested,
    FailureStorageClosed,
    FailureLogTarget {
        target: Option<crate::failure_persistence::Target>,
        enabled: bool,
    },
    LiveFailurePolicy {
        poweroff: bool,
        delay_seconds: u32,
        language: crispawn_core::presentation::Language,
    },
    FailurePresented,
    Retry {
        errno: i32,
        attempt: u32,
        delay_ms: u64,
    },
    Policy {
        policy: Policy,
    },
    Failure {
        code: String,
        detail: String,
    },
}
fn send(value: Notice) {
    if let Some(Some(socket)) = SOCKET.get() {
        let critical = !matches!(
            value,
            Notice::Progress | Notice::Active | Notice::Idle | Notice::Retry { .. }
        );
        if let Ok(bytes) = serde_json::to_vec(&value) {
            if bytes.len() <= 8192 {
                let until = Instant::now() + Duration::from_millis(100);
                loop {
                    match socket.send(&bytes) {
                        Ok(_) => break,
                        Err(error)
                            if critical
                                && error.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < until =>
                        {
                            std::thread::sleep(Duration::from_millis(5))
                        }
                        Err(_) => break,
                    }
                }
            }
        }
    }
}
pub fn progress() {
    if OBSERVER_THREAD.with(std::cell::Cell::get) || SOCKET.get().is_none() {
        return;
    }
    let now = START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1;
    let previous = LAST_PROGRESS.load(Ordering::Relaxed);
    if now.saturating_sub(previous) >= 100
        && LAST_PROGRESS
            .compare_exchange(previous, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        send(Notice::Progress);
    }
}
pub fn phase(phase: &str) {
    if phase == "POWEROFF_REQUESTED" {
        send(Notice::ShutdownRequested);
    } else if matches!(phase, "MENU" | "PAUSED") {
        send(Notice::Idle);
    } else {
        send(Notice::Active);
    }
}
pub fn configure(policy: &Policy) {
    crate::io::configure_retries(policy.transient_retries, u64::from(policy.retry_delay_ms));
    send(Notice::Policy {
        policy: policy.clone(),
    });
}
pub fn retry(errno: i32, attempt: u32, delay_ms: u64) {
    send(Notice::Retry {
        errno,
        attempt,
        delay_ms,
    });
}
pub fn failure(error: &Error) {
    send(Notice::Failure {
        code: error.code.into(),
        detail: error.detail.chars().take(1024).collect(),
    });
}
pub fn cancel_requested() {
    send(Notice::CancelRequested);
}
pub fn failure_storage_closed() {
    send(Notice::FailureStorageClosed);
}
pub(crate) fn failure_log_target(job_id: &str, device_number: &str, enabled: bool) {
    send(Notice::FailureLogTarget {
        target: Some(crate::failure_persistence::Target {
            job_id: job_id.into(),
            device_number: device_number.into(),
        }),
        enabled,
    });
}

#[doc(hidden)]
pub fn persist_failure_child() -> Result<()> {
    crate::failure_persistence::child()
}
pub fn live_failure_policy(
    poweroff: bool,
    delay_seconds: u32,
    language: crispawn_core::presentation::Language,
) {
    send(Notice::LiveFailurePolicy {
        poweroff,
        delay_seconds: delay_seconds.min(60),
        language,
    });
}
pub fn failure_presented() {
    send(Notice::FailurePresented);
}

/// Reserve RAM-backed space before disk work. A full/disconnected backup disk
/// cannot prevent overwriting this emergency log. It does not survive poweroff.
struct Emergency {
    file: Option<tempfile::NamedTempFile>,
}
impl Emergency {
    fn new(mode: Mode) -> Result<Self> {
        let directory = if mode == Mode::Live {
            "/run"
        } else {
            "/dev/shm"
        };
        let mut file = tempfile::Builder::new()
            .prefix("crispawn-failure-")
            .suffix(".json")
            .tempfile_in(directory)?;
        file.write_all(&vec![b' '; 16384])?;
        Ok(Self { file: Some(file) })
    }
    fn retain(&mut self, error: &Error, worker: u32) -> Result<std::path::PathBuf> {
        let bytes = crate::io::json(&serde_json::json!({
            "schema_version":1,"status":"failed","code":error.code,
            "detail":error.detail,"worker_pid":worker,"storage":"reserved_ram",
            "dataset_committed_by_supervisor":false
        }))?;
        let file = self.file.as_mut().unwrap();
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&bytes)?;
        file.as_file().set_len(bytes.len() as u64)?;
        let (_, path) = self
            .file
            .take()
            .unwrap()
            .keep()
            .map_err(|e| Error::from(e.error))?;
        Ok(path)
    }
}
extern "C" fn interrupt(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}
fn signal(child: &Child, number: libc::c_int) {
    // SAFETY: this group was created for this child only. No PID comes from
    // configuration, an archive, a log, or a shared queue.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), number);
    }
}
fn emergency_console(text: &str) {
    // O_NONBLOCK does not bound writes to a regular file. stderr can itself
    // point at the failed backup disk, so isolate this last output attempt too.
    let pointer = text.as_ptr();
    let length = text.len().min(4096);
    // SAFETY: after fork the child uses only raw, async-signal-safe calls and
    // the inherited immutable buffer, then _exit (no Rust locks/destructors).
    unsafe {
        let pid = libc::fork();
        if pid < 0 {
            return;
        }
        if pid == 0 {
            let flags = libc::fcntl(2, libc::F_GETFL);
            if flags >= 0 {
                libc::fcntl(2, libc::F_SETFL, flags | libc::O_NONBLOCK);
                let mut p = libc::pollfd {
                    fd: 2,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                if libc::poll(&mut p, 1, 0) > 0 && p.revents & libc::POLLOUT != 0 {
                    libc::write(2, pointer.cast(), length);
                }
                libc::fcntl(2, libc::F_SETFL, flags);
            }
            libc::_exit(0);
        }
        let until = Instant::now() + Duration::from_millis(20);
        let mut status = 0;
        while Instant::now() < until {
            if libc::waitpid(pid, &mut status, libc::WNOHANG) == pid {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        libc::kill(pid, libc::SIGKILL);
        libc::waitpid(pid, &mut status, libc::WNOHANG);
    }
}
struct TerminalRestore(Option<libc::termios>);
impl TerminalRestore {
    fn new() -> Self {
        let mut value = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: tcgetattr initializes this owned termios on success.
        if unsafe { libc::tcgetattr(0, value.as_mut_ptr()) } == 0 {
            Self(Some(unsafe { value.assume_init() }))
        } else {
            Self(None)
        }
    }
}
impl Drop for TerminalRestore {
    fn drop(&mut self) {
        if let Some(value) = &self.0 {
            // SAFETY: restore the same input terminal after a force-killed
            // child could not run its own terminal guard destructor.
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, value);
            }
        }
    }
}
fn live_poweroff(policy: &Policy, already_requested: bool, storage_closed: bool) -> Result<()> {
    // entry() has already validated the LiveOS identity and RAM root.
    // BusyBox init eventually kills service processes before a global sync.
    // Never hand a still-stuck filesystem to that sequence: it could kill this
    // supervisor's deadline first. Only clean storage uses normal init shutdown.
    let mut poweroff = if already_requested || !storage_closed {
        None
    } else {
        Command::new("/sbin/poweroff").spawn().ok()
    };
    let until = Instant::now()
        + if already_requested || !storage_closed {
            Duration::ZERO
        } else {
            Duration::from_secs(u64::from(policy.shutdown_grace_seconds))
        };
    while Instant::now() < until {
        if let Some(process) = poweroff.as_mut() {
            let _ = process.try_wait();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    emergency_console(
        "CRISPAWN_FAILURE normal shutdown deadline exceeded; requesting emergency poweroff\n",
    );
    // SAFETY: reachable only in the guarded LiveOS parent. Do not globally sync
    // here: a stuck filesystem is the reason the normal shutdown may fail.
    if unsafe { libc::reboot(libc::RB_POWER_OFF) } != 0 {
        return Err(Error::new(
            "POWEROFF_FAILED",
            std::io::Error::last_os_error().to_string(),
        ));
    }
    Ok(())
}

fn publish_failure_view(
    root: &std::path::Path,
    error: &Error,
    language: crispawn_core::presentation::Language,
    remaining: Option<u32>,
) -> Result<()> {
    let mut status = crate::io::read_json::<crispawn_core::live::Status>(&root.join("status.json"))
        .unwrap_or_else(|_| crispawn_core::live::Status::new("0".repeat(32)));
    status.phase = if error.code == "CANCELLED" {
        "CANCELLED"
    } else {
        "FAILED"
    }
    .into();
    status.detail = error.to_string();
    status.language = language;
    status.session.page = crispawn_core::presentation::Page::Failed;
    status.finalization_complete = false;
    status.poweroff_requested = false;
    status.can_pause = false;
    status.can_resume = false;
    status.can_cancel = false;
    status.countdown_seconds = None;
    status.shutdown_countdown_seconds = remaining;
    status.heartbeat = status.heartbeat.saturating_add(1);
    crate::io::atomic_write(root, "status.json", &crate::io::json(&status)?)
}

fn retain_failure_for_view(root: &std::path::Path, error: &Error) {
    let record = serde_json::json!({"kind":"supervisor_failure","schema_version":1,
        "value":{"code":error.code,"detail":error.detail,"detected_by":"supervisor"}});
    if let Ok(bytes) = crate::io::json(&record) {
        // A private RAM file is independent of the failed NTFS mirror/collector.
        let _ = crate::io::atomic_write(&root.join("logs"), "supervisor-failure.jsonl", &bytes);
    }
}

fn ensure_failure_gui(root: &std::path::Path) -> Option<Child> {
    if !crate::io::read_json::<crate::live::Config>(&root.join("live-config.json"))
        .is_ok_and(|config| config.gui)
    {
        return None;
    }
    let executable = std::path::Path::new("/usr/bin/crispawn-gui");
    let existing = crate::io::read_json::<serde_json::Value>(&root.join("ui.json"))
        .ok()
        .and_then(|v| v.get("pid").and_then(serde_json::Value::as_u64));
    if existing
        .filter(|pid| *pid > 1 && *pid <= i32::MAX as u64)
        .is_some_and(|pid| {
            std::fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|path| path == executable)
        })
    {
        return None; // The independent early splash/GUI remains alive.
    }
    if !executable.is_file() {
        return None;
    }
    let _ = std::fs::remove_file(root.join("ui-stop"));
    Command::new(executable)
        .arg(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .ok()
}

#[derive(Default)]
struct DestinationProgress {
    written_sectors: Option<u64>,
}
impl DestinationProgress {
    fn observe(&mut self, written: Option<u64>) -> bool {
        let Some(written) = written else {
            return false;
        };
        let previous = self.written_sectors.replace(written);
        if let Some(previous) = previous {
            // A replacement/reset device is not proof of completed backup I/O.
            if written < previous {
                self.written_sectors = Some(previous);
            }
            written > previous
        } else {
            false
        }
    }
    fn sample(&mut self, target: &crate::failure_persistence::Target) -> bool {
        if target.validate().is_err() {
            return false;
        }
        let text =
            std::fs::read_to_string(format!("/sys/dev/block/{}/stat", target.device_number)).ok();
        let written = text
            .as_deref()
            .and_then(|s| s.split_whitespace().nth(6))
            .and_then(|s| s.parse().ok());
        self.observe(written)
    }
}

fn supervise(mut command: Command, mode: Mode, mut policy: Policy) -> Result<i32> {
    policy.validate()?;
    let _terminal = TerminalRestore::new();
    let mut emergency = Emergency::new(mode)?;
    let (socket, child_socket) = UnixDatagram::pair()?;
    socket.set_nonblocking(true)?;
    let fd = child_socket.as_raw_fd();
    command.env(SOCKET_ENV, fd.to_string()).process_group(0);
    // SAFETY: pre_exec performs only async-signal-safe fcntl on an owned socket
    // kept alive until spawn. Exec inherits this one descriptor.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    drop(child_socket);
    // SAFETY: the single-threaded parent's handlers only store an atomic.
    unsafe {
        libc::signal(libc::SIGINT, interrupt as *const () as usize);
        libc::signal(libc::SIGTERM, interrupt as *const () as usize);
    }
    let mut active = true;
    let mut progress = Instant::now();
    let mut failure: Option<Error> = None;
    let mut deadline = None;
    let mut buffer = [0u8; 8192];
    let mut cancelled = false;
    let mut normal_shutdown = false;
    let mut storage_closed = false;
    let mut failure_poweroff = true;
    let mut failure_delay = 5;
    let mut failure_language = crispawn_core::presentation::Language::Ja;
    let mut failure_log_target: Option<crate::failure_persistence::Target> = None;
    let mut destination_target: Option<crate::failure_persistence::Target> = None;
    let mut destination_progress = DestinationProgress::default();
    let mut destination_sample_at = Instant::now();
    let mut presented_at: Option<Instant> = None;
    let mut observed_exit: Option<(i32, String, Instant)> = None;
    let code = loop {
        // Bound draining: a busy child cannot starve exit/deadline checks.
        for _ in 0..512 {
            let length = match socket.recv(&mut buffer) {
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => break,
                Err(e) => {
                    failure = Some(e.into());
                    signal(&child, libc::SIGKILL);
                    deadline = Some(Instant::now());
                    break;
                }
            };
            match serde_json::from_slice::<Notice>(&buffer[..length]) {
                Ok(Notice::Progress) => progress = Instant::now(),
                Ok(Notice::Active) => {
                    active = true;
                    progress = Instant::now();
                }
                Ok(Notice::Idle) => {
                    active = false;
                    progress = Instant::now();
                }
                Ok(Notice::ShutdownRequested) if mode == Mode::Live => {
                    normal_shutdown = true;
                    storage_closed = true;
                    deadline = Some(
                        Instant::now()
                            + Duration::from_secs(u64::from(policy.shutdown_grace_seconds)),
                    );
                }
                Ok(Notice::Policy { policy: next }) if next.validate().is_ok() => policy = next,
                Ok(Notice::FailureStorageClosed) if mode == Mode::Live => storage_closed = true,
                Ok(Notice::FailureLogTarget { target, enabled }) if mode == Mode::Live => {
                    let target = target.filter(|target| target.validate().is_ok());
                    failure_log_target = if enabled { target.clone() } else { None };
                    destination_target = target;
                    destination_progress = DestinationProgress::default();
                }
                Ok(Notice::LiveFailurePolicy {
                    poweroff,
                    delay_seconds,
                    language,
                }) if mode == Mode::Live && delay_seconds <= 60 => {
                    failure_poweroff = poweroff;
                    failure_delay = delay_seconds;
                    failure_language = language;
                }
                Ok(Notice::FailurePresented) if mode == Mode::Live && storage_closed => {
                    active = false;
                    presented_at = Some(Instant::now());
                    deadline = if failure_poweroff {
                        Some(
                            Instant::now()
                                + Duration::from_secs(u64::from(
                                    failure_delay + policy.shutdown_grace_seconds + 1,
                                )),
                        )
                    } else {
                        None
                    };
                }
                Ok(Notice::CancelRequested) if deadline.is_none() && !normal_shutdown => {
                    cancelled = true;
                    deadline = Some(
                        Instant::now()
                            + Duration::from_secs(u64::from(policy.failure_grace_seconds)),
                    );
                }
                Ok(Notice::Retry {
                    errno,
                    attempt,
                    delay_ms,
                }) => {
                    emergency_console(&format!(
                        "CRISPAWN_IO_RETRY errno={errno} attempt={attempt} delay_ms={delay_ms}\n"
                    ));
                }
                Ok(Notice::Failure { code, detail }) if failure.is_none() => {
                    failure = Some(if code == "CANCELLED" {
                        Error::new("CANCELLED", detail)
                    } else {
                        Error::new("WORKER_FAILED", format!("{code}: {detail}"))
                    });
                    deadline = Some(
                        Instant::now()
                            + Duration::from_secs(u64::from(policy.failure_grace_seconds)),
                    );
                }
                _ => {}
            }
        }
        if INTERRUPTED.swap(false, Ordering::Relaxed) && deadline.is_none() && !normal_shutdown {
            cancelled = true;
            signal(&child, libc::SIGTERM);
            deadline =
                Some(Instant::now() + Duration::from_secs(u64::from(policy.failure_grace_seconds)));
        }
        if destination_sample_at.elapsed() >= Duration::from_millis(250) {
            destination_sample_at = Instant::now();
            if active
                && failure.is_none()
                && destination_target
                    .as_ref()
                    .is_some_and(|target| destination_progress.sample(target))
            {
                // A long fsync/write can still be making real block-device
                // progress. RAM/status/log heartbeats alone never extend this.
                progress = Instant::now();
            }
        }
        if active
            && progress.elapsed() >= Duration::from_secs(u64::from(policy.no_progress_seconds))
            && failure.is_none()
        {
            failure = Some(Error::new(
                "IO_STALLED",
                format!(
                    "no completed I/O/progress for {} seconds",
                    policy.no_progress_seconds
                ),
            ));
            signal(&child, libc::SIGTERM);
            deadline =
                Some(Instant::now() + Duration::from_secs(u64::from(policy.failure_grace_seconds)));
        }
        if observed_exit.is_none() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    observed_exit = Some((
                        status.code().unwrap_or(1),
                        status.to_string(),
                        Instant::now(),
                    ))
                }
                Ok(None) => {}
                Err(error) => {
                    failure = Some(error.into());
                    signal(&child, libc::SIGKILL);
                    break 1;
                }
            }
        }
        if let Some((code, description, at)) = &observed_exit {
            // Stay alive as the last shutdown backstop even after init stops
            // the worker. A stuck system-wide sync must not remove the timer.
            if (!normal_shutdown || failure.is_some()) && at.elapsed() >= Duration::from_millis(50)
            {
                if *code == 0 && failure.is_none() {
                    cancelled = false;
                }
                if *code != 0 && failure.is_none() {
                    failure = Some(Error::new("WORKER_FAILED", description.clone()));
                }
                break if cancelled { 130 } else { *code };
            }
        }
        if deadline.is_some_and(|end| Instant::now() >= end) {
            if normal_shutdown && failure.is_none() {
                failure = Some(Error::new(
                    "SHUTDOWN_STALLED",
                    "normal LiveOS shutdown deadline exceeded",
                ));
            }
            signal(&child, libc::SIGKILL);
            // Never wait()/join(): SIGKILL itself may await kernel I/O.
            break if cancelled { 130 } else { 1 };
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    if cancelled && failure.is_none() {
        failure = Some(Error::new(
            "CANCELLED",
            "cancelled with bounded worker shutdown",
        ));
    }
    if let Some(error) = failure {
        signal(&child, libc::SIGKILL);
        let path = emergency
            .retain(&error, child.id())
            .map(|p| p.display().to_string())
            .unwrap_or_else(|e| format!("RAM_LOG_FAILED:{e}"));
        emergency_console(&format!("CRISPAWN_FAILURE {error}; emergency_log={path}\n"));
        if mode == Mode::Live {
            let root = std::path::Path::new(crate::live::RUNTIME);
            retain_failure_for_view(root, &error);
            let _ = publish_failure_view(root, &error, failure_language, None);
            let _failure_gui = ensure_failure_gui(root);
            // The collector and worker may both be stuck on the failed volume.
            // An independent helper attempts USB persistence within a hard deadline.
            let saved = crate::failure_persistence::attempt(
                failure_log_target.as_ref(),
                &error,
                child.id(),
                failure_language,
            );
            if let Ok(bytes) = crate::io::json(&saved) {
                let _ = crate::io::atomic_write(
                    std::path::Path::new(crate::live::RUNTIME),
                    "failure-persistence.json",
                    &bytes,
                );
            }
            emergency_console(&format!("CRISPAWN_FAILURE_PERSISTENCE {saved}\n"));
            emergency_console(&format!(
                "{}\n",
                failure_language.error_detail(&error.to_string())
            ));
            if !failure_poweroff {
                emergency_console(&format!(
                    "{}\n",
                    failure_language.text(crispawn_core::locale::Text::FailureStopped)
                ));
                // This is the explicit configured stopped state, not a worker
                // waiting for failed disk I/O. Never resume disk work here.
                loop {
                    let _ = publish_failure_view(root, &error, failure_language, None);
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
            let until = presented_at.unwrap_or_else(Instant::now)
                + Duration::from_secs(u64::from(failure_delay));
            while Instant::now() < until {
                let remaining = until
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .div_ceil(1000) as u32;
                let _ = publish_failure_view(root, &error, failure_language, Some(remaining));
                std::thread::sleep(Duration::from_millis(100));
            }
            live_poweroff(&policy, normal_shutdown, storage_closed)?;
        }
        return Ok(if code == 0 { 1 } else { code });
    }
    Ok(code)
}

/// Invoked by the real CLI only. No external command endpoint is installed.
pub fn entry(mode: Mode) -> Result<Option<i32>> {
    if let Ok(value) = std::env::var(SOCKET_ENV) {
        let fd = value
            .parse::<i32>()
            .map_err(|_| Error::new("INVALID_SUPERVISOR", "invalid inherited socket"))?;
        let mut kind = 0i32;
        let mut size = std::mem::size_of::<i32>() as libc::socklen_t;
        // SAFETY: inspect the inherited descriptor before taking ownership.
        let valid = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_TYPE,
                (&mut kind as *mut i32).cast(),
                &mut size,
            )
        } == 0;
        if !valid || kind != libc::SOCK_DGRAM {
            return Err(Error::new(
                "INVALID_SUPERVISOR",
                "inherited descriptor is not a datagram socket",
            ));
        }
        // SAFETY: this descriptor was inherited exclusively for this worker.
        let socket = unsafe { UnixDatagram::from_raw_fd(fd) };
        // SAFETY: future frontend/helper execs must not inherit the worker IPC.
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        socket.set_nonblocking(true)?;
        let _ = SOCKET.set(Some(socket));
        send(Notice::Active);
        return Ok(None);
    }
    if mode == Mode::Live {
        crate::live::guard()?;
    }
    let mut command = Command::new(std::env::current_exe()?);
    command.args(std::env::args_os().skip(1));
    supervise(command, mode, Policy::default()).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn supervisor_failure_is_visible_in_ram_without_the_regular_collector() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("logs")).unwrap();
        let error = Error::new("IO_STALLED", "no completed I/O/progress for 180 seconds");
        retain_failure_for_view(root.path(), &error);
        publish_failure_view(
            root.path(),
            &error,
            crispawn_core::presentation::Language::Ja,
            Some(5),
        )
        .unwrap();
        let status: crispawn_core::live::Status =
            crate::io::read_json(&root.path().join("status.json")).unwrap();
        assert_eq!(status.phase, "FAILED");
        assert!(status.detail.contains("IO_STALLED"));
        assert!(!status.finalization_complete && !status.can_cancel);
        assert_eq!(status.shutdown_countdown_seconds, Some(5));
        assert!(
            std::fs::read_to_string(root.path().join("logs/supervisor-failure.jsonl"))
                .unwrap()
                .contains("IO_STALLED")
        );
        publish_failure_view(
            root.path(),
            &error,
            crispawn_core::presentation::Language::Ja,
            None,
        )
        .unwrap();
        let status: crispawn_core::live::Status =
            crate::io::read_json(&root.path().join("status.json")).unwrap();
        assert_eq!(status.shutdown_countdown_seconds, None);
        publish_failure_view(
            root.path(),
            &Error::new("CANCELLED", "operation cancelled"),
            crispawn_core::presentation::Language::Ja,
            None,
        )
        .unwrap();
        let cancelled: crispawn_core::live::Status =
            crate::io::read_json(&root.path().join("status.json")).unwrap();
        assert_eq!(cancelled.phase, "CANCELLED");
    }
    #[test]
    fn completed_destination_writes_count_but_heartbeats_and_counter_resets_do_not() {
        let mut progress = DestinationProgress::default();
        assert!(!progress.observe(None));
        assert!(!progress.observe(Some(100)));
        assert!(!progress.observe(Some(100)));
        assert!(progress.observe(Some(120)));
        assert!(!progress.observe(None));
        assert!(!progress.observe(Some(2)));
        assert!(!progress.observe(Some(3)));
        assert!(progress.observe(Some(121)));
    }
    #[test]
    fn status_file_heartbeat_does_not_count_as_disk_progress() {
        const CHILD: &str = "CRISPAWN_TEST_OBSERVER_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let (sender, receiver) = UnixDatagram::pair().unwrap();
            sender.set_nonblocking(true).unwrap();
            receiver.set_nonblocking(true).unwrap();
            assert!(SOCKET.set(Some(sender)).is_ok());
            std::thread::spawn(|| {
                observer_thread();
                let file = tempfile::tempfile_in("/dev/shm").unwrap();
                for _ in 0..3 {
                    crate::io::write_all(&mut &file, b"status heartbeat").unwrap();
                    crate::io::sync_file(&file).unwrap();
                    std::thread::sleep(Duration::from_millis(110));
                }
            })
            .join()
            .unwrap();
            let mut message = [0; 8192];
            assert_eq!(
                receiver.recv(&mut message).unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
            let mut data = tempfile::tempfile_in("/dev/shm").unwrap();
            crate::io::write_all(&mut data, b"actual copy progress").unwrap();
            // Progress messages are deliberately limited to ten per second.
            std::thread::sleep(Duration::from_millis(110));
            crate::io::sync_file(&data).unwrap();
            let length = receiver.recv(&mut message).unwrap();
            assert!(matches!(
                serde_json::from_slice::<Notice>(&message[..length]).unwrap(),
                Notice::Progress
            ));
            return;
        }
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "supervisor::tests::status_file_heartbeat_does_not_count_as_disk_progress",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(status.success());
    }
    #[test]
    fn unresponsive_child_has_bounded_failure_without_waiting_for_it() {
        let mut command = Command::new("/bin/sleep");
        command.arg("30");
        let begin = Instant::now();
        let policy = Policy {
            no_progress_seconds: 1,
            failure_grace_seconds: 1,
            shutdown_grace_seconds: 1,
            ..Policy::default()
        };
        assert_ne!(supervise(command, Mode::Cui, policy).unwrap(), 0);
        assert!(begin.elapsed() < Duration::from_secs(5));
    }
    #[test]
    fn successful_child_is_not_reported_as_failure() {
        assert_eq!(
            supervise(Command::new("/bin/true"), Mode::Cui, Policy::default()).unwrap(),
            0
        );
    }
}
