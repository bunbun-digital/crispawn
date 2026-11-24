// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-digital. Licensed under Apache-2.0.
//! One independently bounded write attempt, even when the normal log collector is stuck.
use crate::{Error, Result};
use crispawn_core::{error::require, presentation::Language};
use serde::{Deserialize, Serialize};
use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const LIMIT: usize = 16 << 10;
const TIMEOUT: Duration = Duration::from_secs(5);

fn kernel_tail() -> serde_json::Value {
    let result = (|| -> std::io::Result<String> {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open("/dev/kmsg")?;
        let mut buffer = [0; 8192];
        let mut tail = Vec::new();
        // Bounded untrusted evidence, never interpreted as commands.
        for _ in 0..4096 {
            match file.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    tail.extend_from_slice(&buffer[..count]);
                    if tail.len() > 1024 {
                        tail.drain(..tail.len() - 1024);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e)
                    if e.kind() == std::io::ErrorKind::Interrupted
                        || e.raw_os_error() == Some(libc::EPIPE) =>
                {
                    continue
                }
                Err(e) => return Err(e),
            }
        }
        Ok(String::from_utf8_lossy(&tail).into_owned())
    })();
    match result {
        Ok(text) => {
            serde_json::json!({"available":true,"source":"/dev/kmsg","tail_bytes_cap":1024,"text":text})
        }
        Err(e) => serde_json::json!({"available":false,"source":"/dev/kmsg","error":e.to_string()}),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Target {
    pub job_id: String,
    pub device_number: String,
}
impl Target {
    pub fn validate(&self) -> Result<(u32, u32)> {
        require(
            self.job_id.len() == 32 && self.job_id.bytes().all(|b| b.is_ascii_hexdigit()),
            "INVALID_FAILURE_TARGET",
            "invalid job identifier",
        )?;
        let (major, minor) = self
            .device_number
            .split_once(':')
            .ok_or_else(|| Error::new("INVALID_FAILURE_TARGET", "invalid device number"))?;
        let parse = |s: &str| -> Result<u32> {
            require(
                !s.is_empty() && s.len() <= 10 && s.bytes().all(|b| b.is_ascii_digit()),
                "INVALID_FAILURE_TARGET",
                "invalid device number",
            )?;
            s.parse()
                .map_err(|_| Error::new("INVALID_FAILURE_TARGET", "invalid device number"))
        };
        Ok((parse(major)?, parse(minor)?))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    target: Target,
    record: serde_json::Value,
}

fn directory(parent: &File, name: &str, device: u64, create: bool) -> Result<File> {
    let name =
        CString::new(name).map_err(|_| Error::new("UNSAFE_DESTINATION", "NUL in directory"))?;
    // SAFETY: every component is fixed or generated locally; parent is an owned directory FD.
    if create && unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EEXIST) {
            return Err(error.into());
        }
    }
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: openat returned a new owned descriptor.
    let file = unsafe { File::from_raw_fd(fd) };
    require(
        file.metadata()?.dev() == device,
        "UNSAFE_DESTINATION",
        "failure log left destination filesystem",
    )?;
    Ok(file)
}

fn persist(root: &File, request: &Request) -> Result<String> {
    request.target.validate()?;
    let bytes = serde_json::to_vec_pretty(&request.record)
        .map_err(|e| Error::new("JSON_ERROR", e.to_string()))?;
    require(
        bytes.len() <= LIMIT,
        "LIMIT_EXCEEDED",
        "failure record exceeds cap",
    )?;
    let device = root.metadata()?.dev();
    let product = directory(root, "crispawn", device, true)?;
    let logs = directory(&product, "logs", device, true)?;
    let suffix = crate::io::new_id()?;
    let name = format!(
        "{}-failure-{}",
        request.target.job_id,
        &crispawn_core::types::hex(&suffix)[..8]
    );
    // Exclusive directory creation: never reuse or overwrite an existing session.
    let cname = CString::new(name.clone()).unwrap();
    if unsafe { libc::mkdirat(logs.as_raw_fd(), cname.as_ptr(), 0o700) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let output = directory(&logs, &name, device, false)?;
    let fd = unsafe {
        libc::openat(
            output.as_raw_fd(),
            c"failure.json".as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    for dir in [&output, &logs, &product, root] {
        dir.sync_all()?;
    }
    Ok(format!("crispawn/logs/{name}/failure.json"))
}

pub(crate) fn child() -> Result<()> {
    let mut bytes = vec![];
    std::io::stdin()
        .take((LIMIT + 1) as u64)
        .read_to_end(&mut bytes)?;
    require(
        bytes.len() <= LIMIT,
        "LIMIT_EXCEEDED",
        "failure request exceeds cap",
    )?;
    let mut request: Request =
        serde_json::from_slice(&bytes).map_err(|e| Error::new("JSON_ERROR", e.to_string()))?;
    let result = (|| {
        let (major, minor) = request.target.validate()?;
        let path = Path::new(crate::live::RUNTIME).join("destination");
        let mounts = fs::read_to_string("/proc/self/mountinfo")?;
        let owned = mounts.lines().any(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            fields.get(2) == Some(&request.target.device_number.as_str())
                && fields.get(4) == path.to_str().as_ref()
                && fields
                    .get(5)
                    .is_some_and(|v| v.split(',').any(|p| p == "rw"))
                && line
                    .split_once(" - ")
                    .is_some_and(|(_, rest)| rest.split_whitespace().next() == Some("ntfs3"))
        });
        require(
            owned,
            "MOUNT_IDENTITY_MISMATCH",
            "failure logging requires the existing owned writable NTFS mount",
        )?;
        let root = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        require(
            root.metadata()?.dev() == libc::makedev(major, minor),
            "MOUNT_IDENTITY_MISMATCH",
            "destination device changed",
        )?;
        if let Some(record) = request.record.as_object_mut() {
            record.insert("kernel_diagnostics".into(), kernel_tail());
            if let Ok(status) = crate::io::read_json::<crispawn_core::live::Status>(
                &Path::new(crate::live::RUNTIME).join("status.json"),
            ) {
                if status.job_id == request.target.job_id {
                    record.insert(
                        "storage".into(),
                        serde_json::to_value(&status.storage).unwrap_or(serde_json::Value::Null),
                    );
                    record.insert(
                        "progress".into(),
                        serde_json::to_value(status.snapshot)
                            .map_err(|e| Error::new("JSON_ERROR", e.to_string()))?,
                    );
                }
            }
        }
        if serde_json::to_vec(&request.record)
            .map_err(|e| Error::new("JSON_ERROR", e.to_string()))?
            .len()
            > LIMIT
        {
            if let Some(record) = request.record.as_object_mut() {
                record.insert("kernel_diagnostics".into(),serde_json::json!({"available":false,"error":"kernel tail omitted to preserve bounded failure reason"}));
            }
        }
        persist(&root, &request)
    })();
    let response = match result {
        Ok(path) => serde_json::json!({"status":"persisted","path":path,"synced":true}),
        Err(error) => {
            serde_json::json!({"status":"failed","code":error.code,"detail":error.detail.chars().take(512).collect::<String>(),"errno":error.raw_os_error()})
        }
    };
    // stdout is a private RAM file, never the failed device or a terminal.
    serde_json::to_writer(std::io::stdout(), &response)
        .map_err(|e| Error::new("JSON_ERROR", e.to_string()))?;
    Ok(())
}

fn wait(child: &mut Child, timeout: Duration) -> Result<bool> {
    let end = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return Ok(true);
        }
        if Instant::now() >= end {
            let _ = child.kill();
            // A task stuck in kernel I/O can ignore SIGKILL until the syscall returns.
            // Never wait()/join() after this deadline.
            let _ = child.try_wait();
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub(crate) fn attempt(
    target: Option<&Target>,
    error: &Error,
    worker: u32,
    language: Language,
) -> serde_json::Value {
    let Some(target) = target else {
        return serde_json::json!({"status":"not_available","detail":"logging disabled or no owned NTFS destination was registered"});
    };
    let run = || -> Result<serde_json::Value> {
        target.validate()?;
        let request = Request {
            target: target.clone(),
            record: serde_json::json!({
                "kind":"crispawn.failure","schema_version":1,"status":if error.code=="CANCELLED" {"cancelled"} else {"failed"},"code":error.code,
                "detail":error.detail.chars().take(1024).collect::<String>(),
                "user_message":language.error_detail(&error.to_string()).chars().take(1024).collect::<String>(),"worker_pid":worker,
                "detected_by":"supervisor","dataset_committed_by_supervisor":false,
                "unix_ms":SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis().to_string()
            }),
        };
        let mut input = tempfile::Builder::new()
            .prefix("crispawn-failure-request-")
            .tempfile_in("/run")?;
        let output = tempfile::Builder::new()
            .prefix("crispawn-failure-result-")
            .tempfile_in("/run")?;
        serde_json::to_writer(&mut input, &request)
            .map_err(|e| Error::new("JSON_ERROR", e.to_string()))?;
        input.as_file_mut().seek(SeekFrom::Start(0))?;
        let mut child = Command::new(std::env::current_exe()?)
            .arg("persist-failure")
            .env_remove("CRISPAWN_SUPERVISOR_FD")
            .stdin(Stdio::from(input.as_file().try_clone()?))
            .stdout(Stdio::from(output.reopen()?))
            .stderr(Stdio::null())
            .spawn()?;
        if !wait(&mut child, TIMEOUT)? {
            return Ok(
                serde_json::json!({"status":"timed_out","timeout_seconds":TIMEOUT.as_secs(),"durability":"unknown"}),
            );
        }
        let mut bytes = vec![];
        output.reopen()?.take(4097).read_to_end(&mut bytes)?;
        require(
            bytes.len() <= 4096,
            "LIMIT_EXCEEDED",
            "failure helper result cap",
        )?;
        serde_json::from_slice(&bytes).map_err(|e| Error::new("FAILURE_LOG_FAILED", e.to_string()))
    };
    run().unwrap_or_else(|e| serde_json::json!({"status":"failed","code":e.code,"detail":e.detail.chars().take(512).collect::<String>()}))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> Request {
        Request {
            target: Target {
                job_id: "a".repeat(32),
                device_number: "8:2".into(),
            },
            record: serde_json::json!({"code":"IO_STALLED","status":"failed","user_message":"読み書きが一定時間進まなかったため停止しました"}),
        }
    }
    #[test]
    fn saves_a_separate_failure_record_without_overwriting_regular_logs() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("crispawn/logs/existing")).unwrap();
        fs::write(
            dir.path()
                .join("crispawn/logs/existing/events-000000.jsonl"),
            b"existing log",
        )
        .unwrap();
        let root = File::open(dir.path()).unwrap();
        let path = persist(&root, &request()).unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.path().join(path)).unwrap()).unwrap();
        assert_eq!(value["code"], "IO_STALLED");
        assert_eq!(
            fs::read(
                dir.path()
                    .join("crispawn/logs/existing/events-000000.jsonl")
            )
            .unwrap(),
            b"existing log"
        );
    }
    #[test]
    fn refuses_symlink_escape_and_invalid_identity() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(other.path(), dir.path().join("crispawn")).unwrap();
        assert!(persist(&File::open(dir.path()).unwrap(), &request()).is_err());
        assert_eq!(fs::read_dir(other.path()).unwrap().count(), 0);
        let mut req = request();
        req.target.job_id = "../escape".into();
        assert_eq!(
            persist(&File::open(dir.path()).unwrap(), &req)
                .unwrap_err()
                .code,
            "INVALID_FAILURE_TARGET"
        );
        for number in ["../8:2", "8:-2", "8:2:3", "4294967296:2"] {
            req.target.device_number = number.into();
            req.target.job_id = "a".repeat(32);
            assert!(req.target.validate().is_err());
        }
    }
    #[test]
    fn missing_destination_does_not_spawn_or_write() {
        assert_eq!(
            attempt(None, &Error::new("IO_STALLED", "test"), 1, Language::Ja)["status"],
            "not_available"
        );
    }
    #[test]
    fn unresponsive_save_helper_cannot_block_shutdown() {
        let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let start = Instant::now();
        assert!(!wait(&mut child, Duration::from_millis(100)).unwrap());
        assert!(start.elapsed() < Duration::from_secs(2));
        let end = Instant::now() + Duration::from_secs(1);
        while child.try_wait().unwrap().is_none() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
