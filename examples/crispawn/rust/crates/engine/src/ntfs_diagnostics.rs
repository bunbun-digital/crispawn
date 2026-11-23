// SPDX-License-Identifier: Apache-2.0
//! Read-only NTFS admission and bounded kernel evidence. Never repairs a volume.
use crate::{device::Device, Error, Result};
use ntfs::{structured_values::NtfsVolumeFlags, Ntfs};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
            process::CommandExt,
        },
    },
    path::Path,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

const READ_BUDGET: u64 = 2 << 20;
const KERNEL_BUDGET: usize = 8 << 10;
pub const MOUNT_OPTIONS: &str = "uid=0,gid=0,umask=077,iocharset=utf8,windows_names";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeState {
    pub version: String,
    pub flags: u16,
    pub dirty: bool,
    pub sector_bytes: u16,
    pub cluster_bytes: u32,
    pub file_record_bytes: u32,
    pub filesystem_bytes: u64,
    pub serial: String,
}
static INSPECTOR_CHILD: AtomicBool = AtomicBool::new(false);

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InspectorReply {
    volume: Option<VolumeState>,
    error: Option<String>,
    io_errno: Option<i32>,
}
fn reply(value: &InspectorReply) -> Result<()> {
    let mut data = serde_json::to_vec(value)
        .map_err(|e| Error::new("NTFS_STATE_UNAVAILABLE", e.to_string()))?;
    data.push(b'\n');
    io::stdout().write_all(&data)?;
    Ok(())
}
fn note_error(error: &io::Error) {
    if INSPECTOR_CHILD.load(Ordering::Relaxed) {
        let _ = reply(&InspectorReply {
            io_errno: error.raw_os_error(),
            error: Some(limit(&error.to_string(), 1024)),
            ..Default::default()
        });
    }
}

// The parser can seek within this partition only; corrupt metadata cannot turn
// diagnosis into a whole-disk scan or an unbounded metadata read.
struct Bounded<R> {
    inner: R,
    length: u64,
    position: u64,
    remaining: u64,
    last_errno: Option<i32>,
}
impl<R: Read> Read for Bounded<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let error = io::Error::other("NTFS diagnosis read budget exceeded");
            note_error(&error);
            return Err(error);
        }
        let length = (buf.len() as u64)
            .min(self.remaining)
            .min(self.length.saturating_sub(self.position)) as usize;
        let n = crate::io::Retrying(&mut self.inner)
            .read(&mut buf[..length])
            .map_err(|error| {
                self.last_errno = error.raw_os_error();
                // Record the actual read errno before calling the third-party
                // parser, which can abort while converting a nested I/O error.
                note_error(&error);
                error
            })?;
        self.position += n as u64;
        self.remaining -= n as u64;
        crate::supervisor::progress();
        Ok(n)
    }
}
impl<R: Seek> Seek for Bounded<R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let next = match from {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::Current(n) => self.position.checked_add_signed(n),
            SeekFrom::End(n) => self.length.checked_add_signed(n),
        }
        .filter(|n| *n <= self.length)
        .ok_or_else(|| io::Error::other("NTFS metadata seek outside partition"))?;
        self.inner.seek(SeekFrom::Start(next))?;
        self.position = next;
        Ok(next)
    }
}

/// This function only receives a read handle. No kernel mount/journal replay.
fn inspect_reader(reader: impl Read + Seek, length: u64) -> Result<VolumeState> {
    let mut reader = Bounded {
        inner: reader,
        length,
        position: 0,
        remaining: READ_BUDGET,
        last_errno: None,
    };
    let parsed = (|| {
        let ntfs = Ntfs::new(&mut reader).map_err(|e| e.to_string())?;
        // Limit the parser's per-record allocation before reading any MFT file.
        if ntfs.file_record_size() > 64 << 10 || ntfs.size() > length {
            return Err("NTFS metadata geometry exceeds partition or diagnosis limits".into());
        }
        let info = ntfs.volume_info(&mut reader).map_err(|e| e.to_string())?;
        Ok(VolumeState {
            version: format!("{}.{}", info.major_version(), info.minor_version()),
            flags: info.flags().bits(),
            dirty: info.flags().contains(NtfsVolumeFlags::IS_DIRTY),
            sector_bytes: ntfs.sector_size(),
            cluster_bytes: ntfs.cluster_size(),
            file_record_bytes: ntfs.file_record_size(),
            filesystem_bytes: ntfs.size(),
            serial: format!("{:016X}", ntfs.serial_number()),
        })
    })();
    parsed.map_err(|e: String| {
        let mut error = reader
            .last_errno
            .map(|errno| Error::from(io::Error::from_raw_os_error(errno)))
            .unwrap_or_else(|| Error::new("NTFS_STATE_UNAVAILABLE", ""));
        error.code = "NTFS_STATE_UNAVAILABLE";
        error.detail = limit(&e, 2048);
        error
    })
}

/// Internal child entry point. It inherits an already opened READ-ONLY FD, not
/// a path or a command supplied by the volume. Parser panics stay in this child.
pub(crate) fn child(length: u64, retries: u32, delay_ms: u64) -> Result<()> {
    crispawn_core::error::require(
        length >= 512 && retries <= 8 && (1..=2000).contains(&delay_ms),
        "ARGUMENT_ERROR",
        "invalid read-only NTFS inspection bounds",
    )?;
    // SAFETY: stdin is inherited from the parent's retained read-only file.
    let input = unsafe { File::from_raw_fd(libc::STDIN_FILENO) };
    let metadata = input.metadata()?;
    // SAFETY: query the owned valid FD; never change its access mode.
    let access = unsafe { libc::fcntl(input.as_raw_fd(), libc::F_GETFL) };
    crispawn_core::error::require(
        access >= 0
            && access & libc::O_ACCMODE == libc::O_RDONLY
            && (metadata.file_type().is_block_device()
                || (metadata.is_file() && metadata.len() == length)),
        "ARGUMENT_ERROR",
        "NTFS inspector requires a read-only image or partition FD",
    )?;
    crate::io::configure_retries(retries, delay_ms);
    INSPECTOR_CHILD.store(true, Ordering::Relaxed);
    match inspect_reader(input, length) {
        Ok(volume) => reply(&InspectorReply {
            volume: Some(volume),
            ..Default::default()
        }),
        Err(e) => reply(&InspectorReply {
            io_errno: e.raw_os_error(),
            error: Some(e.detail),
            ..Default::default()
        }),
    }
}

struct InspectorProcess(Child);
impl std::ops::Deref for InspectorProcess {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}
impl std::ops::DerefMut for InspectorProcess {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}
impl Drop for InspectorProcess {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            for _ in 0..25 {
                if self.0.try_wait().ok().flatten().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn collect_inspector(mut command: Command, input: File, timeout: Duration) -> Result<VolumeState> {
    // SAFETY: only async-signal-safe setrlimit in the forked child. Core dumps
    // are disabled for the inspector alone, never for the host or caller.
    unsafe {
        command.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::setrlimit(libc::RLIMIT_CORE, &limit) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut process = InspectorProcess(
        command
            .stdin(Stdio::from(input))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let mut output = process.stdout.take().unwrap();
    // SAFETY: set nonblocking reads on this owned pipe, preserving existing flags.
    let flags = unsafe { libc::fcntl(output.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(output.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        let error = io::Error::last_os_error();
        let _ = process.kill();
        return Err(error.into());
    }
    let until = Instant::now() + timeout;
    let mut bytes = Vec::new();
    let mut status = None;
    let mut failure = None;
    loop {
        let mut buffer = [0; 4096];
        match output.read(&mut buffer) {
            Ok(n) => {
                bytes.extend_from_slice(&buffer[..n]);
            }
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => {
                failure = Some(e.to_string());
                break;
            }
        }
        if bytes.len() > 16384 {
            failure = Some("NTFS inspector response exceeded limit".into());
            break;
        }
        if let Some(exit) = process.try_wait()? {
            // Pipe is bounded and the child has exited; one final drain suffices.
            let _ = output
                .take((16385 - bytes.len()) as u64)
                .read_to_end(&mut bytes);
            status = Some(exit);
            break;
        }
        if Instant::now() >= until {
            failure = Some("read-only NTFS inspector timed out".into());
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if failure.is_some() {
        let _ = process.kill();
        // Do not wait forever if a defective device leaves a read in kernel D
        // state. The inspector has no write handle and the parent can report.
        for _ in 0..25 {
            if process.try_wait().ok().flatten().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let mut volume = None;
    let mut errno = None;
    if bytes.len() > 16384 {
        failure = Some("NTFS inspector response exceeded limit".into());
    }
    for line in bytes.split(|b| *b == b'\n') {
        if let Ok(message) = serde_json::from_slice::<InspectorReply>(line) {
            errno = message.io_errno.or(errno);
            if let Some(detail) = message.error {
                failure = Some(detail);
            }
            if message.volume.is_some() {
                volume = message.volume;
            }
        }
    }
    if failure.is_none() && errno.is_none() && status.is_some_and(|s| s.success()) {
        if let Some(volume) = volume {
            return Ok(volume);
        }
    }
    let mut error = errno
        .map(|n| Error::from(io::Error::from_raw_os_error(n)))
        .unwrap_or_else(|| Error::new("NTFS_STATE_UNAVAILABLE", ""));
    error.code = "NTFS_STATE_UNAVAILABLE";
    error.detail = limit(
        &format!(
            "{}; inspector_exit={status:?}",
            failure.unwrap_or_else(|| "read-only NTFS inspector exited without a result".into())
        ),
        2048,
    );
    Err(error)
}

fn inspect(device: &Device, policy: &crate::supervisor::Policy) -> Result<VolumeState> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&device.path)?;
    let m = file.metadata()?;
    crispawn_core::error::require(
        m.file_type().is_block_device()
            && format!("{}:{}", libc::major(m.rdev()), libc::minor(m.rdev())) == device.number,
        "DEVICE_CHANGED",
        "NTFS diagnosis block identity changed",
    )?;
    let mut command = Command::new(std::env::current_exe()?);
    command.env_remove("CRISPAWN_SUPERVISOR_FD");
    command.args([
        "inspect-ntfs",
        &device.size_bytes.to_string(),
        &policy.transient_retries.to_string(),
        &policy.retry_delay_ms.to_string(),
    ]);
    collect_inspector(
        command,
        file,
        Duration::from_secs(u64::from(policy.no_progress_seconds.min(30))),
    )
}

fn limit(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].into()
}

#[derive(Debug, Default, Serialize)]
pub struct KernelEvidence {
    pub source: &'static str,
    pub available: bool,
    pub truncated: bool,
    pub error: Option<String>,
    pub messages: Vec<String>,
}
struct KernelCapture {
    file: Option<File>,
    evidence: KernelEvidence,
}
impl KernelCapture {
    fn begin() -> Self {
        let mut evidence = KernelEvidence {
            source: "/dev/kmsg since attempt start",
            ..Default::default()
        };
        let file = (|| -> io::Result<File> {
            let mut file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open("/dev/kmsg")?;
            file.seek(SeekFrom::End(0))?;
            Ok(file)
        })();
        let file = match file {
            Ok(file) => {
                evidence.available = true;
                Some(file)
            }
            Err(e) => {
                evidence.error = Some(e.to_string());
                None
            }
        };
        Self { file, evidence }
    }
    fn finish(mut self, name: &str) -> KernelEvidence {
        if let Some(file) = &mut self.file {
            let mut buffer = [0; 8192];
            let mut used = 0;
            for _ in 0..256 {
                match file.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        let line = String::from_utf8_lossy(&buffer[..n]);
                        if line.contains("ntfs3") || line.contains(name) {
                            let line = limit(&line, 2048);
                            used += serde_json::to_vec(&line)
                                .map(|b| b.len())
                                .unwrap_or(KERNEL_BUDGET);
                            if used > KERNEL_BUDGET {
                                self.evidence.truncated = true;
                                break;
                            }
                            self.evidence.messages.push(line);
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.raw_os_error() == Some(libc::EPIPE) => {
                        self.evidence.truncated = true;
                    }
                    Err(e) => {
                        self.evidence.error = Some(e.to_string());
                        break;
                    }
                }
            }
        }
        self.evidence
    }
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub kind: &'static str,
    pub schema_version: u32,
    pub device: Device,
    pub mountpoint: String,
    pub filesystem: &'static str,
    pub options: &'static str,
    pub force: bool,
    pub read_only_inspection: bool,
    pub kernel_release: String,
    pub uefi_boot_context: serde_json::Value,
    pub ntfs3_registered: bool,
    pub volume: Option<VolumeState>,
    pub inspection_error: Option<String>,
    pub inspection_errno: Option<i32>,
    pub mount_attempted: bool,
    pub errno: Option<i32>,
    pub code: String,
    pub error: Option<String>,
    pub user_message: Option<String>,
    pub kernel: KernelEvidence,
}
pub struct Attempt {
    pub report: Report,
    capture: KernelCapture,
    policy: crate::supervisor::Policy,
}
impl Attempt {
    pub fn begin(device: &Device, mountpoint: &Path, policy: &crate::supervisor::Policy) -> Self {
        Self {
            capture: KernelCapture::begin(),
            policy: policy.clone(),
            report: Report {
                kind: "crispawn.ntfs_mount_diagnostics",
                schema_version: 1,
                device: device.clone(),
                mountpoint: mountpoint.display().to_string(),
                filesystem: "ntfs3",
                options: MOUNT_OPTIONS,
                force: false,
                read_only_inspection: true,
                kernel_release: fs::read_to_string("/proc/sys/kernel/osrelease")
                    .unwrap_or_default()
                    .trim()
                    .into(),
                uefi_boot_context: crate::uefi_diagnostics::observe(Path::new("/sys")),
                ntfs3_registered: fs::read_to_string("/proc/filesystems")
                    .unwrap_or_default()
                    .lines()
                    .any(|l| l.split_whitespace().last() == Some("ntfs3")),
                volume: None,
                inspection_error: None,
                inspection_errno: None,
                mount_attempted: false,
                errno: None,
                code: String::new(),
                error: None,
                user_message: None,
                kernel: KernelEvidence::default(),
            },
        }
    }
    pub fn preflight(&mut self) -> Result<()> {
        match inspect(&self.report.device, &self.policy) {
            Ok(volume) => {
                let dirty = volume.dirty;
                self.report.volume = Some(volume);
                if dirty {
                    return Err(Error::new("NTFS_VOLUME_DIRTY", "read-only $Volume inspection found the NTFS dirty flag; writable mount was not attempted"));
                }
                Ok(())
            }
            Err(e) => {
                self.report.inspection_errno = e.raw_os_error();
                self.report.inspection_error = Some(e.to_string());
                Err(Error::new("NTFS_STATE_UNAVAILABLE", "read-only NTFS state could not be established; writable mount was not attempted"))
            }
        }
    }
    pub fn finish(mut self, error: Option<Error>) -> (Report, Option<Error>) {
        self.report.kernel = self.capture.finish(&self.report.device.name);
        let error = error.map(|mut error| {
            self.report.errno = error.raw_os_error();
            if error.code == "NTFS_MOUNT_FAILED" {
                error.code = classify(&self.report.device.name, &self.report.kernel.messages);
            }
            self.report.code = error.code.into();
            self.report.error = Some(limit(&error.to_string(), 3072));
            error.detail = format!(
                "{}; device={}; diagnostics=/run/crispawn/ntfs-mount-diagnostics.json",
                error.detail,
                self.report.device.path.display()
            );
            error
        });
        if error.is_none() {
            self.report.code = "NTFS_MOUNTED".into();
        }
        (self.report, error)
    }
}

// A raw EINVAL is deliberately NOT interpreted as dirty/corrupt media. Only
// diagnostics naming this exact partition can refine a failed mount's reason.
fn classify(device_name: &str, messages: &[String]) -> &'static str {
    for line in messages {
        if !line.contains(&format!("ntfs3({device_name}):"))
            && !line.contains(&format!("ntfs3: {device_name}:"))
        {
            continue;
        }
        if line.contains("volume is dirty") && line.contains("force") {
            return "NTFS_VOLUME_DIRTY";
        }
        if line.contains("failed to replay log file") {
            return "NTFS_JOURNAL_REPLAY_FAILED";
        }
    }
    "NTFS_MOUNT_FAILED"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn errno_alone_or_other_partition_does_not_diagnose_dirty() {
        assert_eq!(classify("sdb2", &[]), "NTFS_MOUNT_FAILED");
        let messages = vec!["ntfs3: sdc2: volume is dirty and force flag is not set!".into()];
        assert_eq!(classify("sdb2", &messages), "NTFS_MOUNT_FAILED");
    }
    #[test]
    fn exact_partition_diagnostics_distinguish_dirty_and_journal() {
        assert_eq!(
            classify(
                "sdb2",
                &["3,1,2,-;ntfs3: sdb2: volume is dirty and force flag is not set!".into()]
            ),
            "NTFS_VOLUME_DIRTY"
        );
        assert_eq!(
            classify(
                "sdb2",
                &["ntfs3: sdb2: failed to replay log file. Can't mount rw!".into()]
            ),
            "NTFS_JOURNAL_REPLAY_FAILED"
        );
        assert_eq!(
            classify(
                "sdb2",
                &["3,1,2,-;ntfs3(sdb2): volume is dirty and force flag is not set!".into()]
            ),
            "NTFS_VOLUME_DIRTY"
        );
        assert_eq!(
            classify(
                "sdb2",
                &["ntfs3(sdb2): failed to replay log file. Can't mount rw!".into()]
            ),
            "NTFS_JOURNAL_REPLAY_FAILED"
        );
        assert_eq!(
            classify(
                "sdb2",
                &["ntfs3(sdb20): volume is dirty and force flag is not set!".into()]
            ),
            "NTFS_MOUNT_FAILED"
        );
    }
    #[test]
    fn invalid_volume_remains_unknown_instead_of_being_called_clean_or_dirty() {
        let error = inspect_reader(Cursor::new(vec![0; 4096]), 4096).unwrap_err();
        assert_eq!(error.code, "NTFS_STATE_UNAVAILABLE");
    }
    #[test]
    fn metadata_reader_bounds_seeks_and_total_reads() {
        let mut reader = Bounded {
            inner: Cursor::new(vec![1; 32]),
            length: 16,
            position: 0,
            remaining: 4,
            last_errno: None,
        };
        assert!(reader.seek(SeekFrom::Start(17)).is_err());
        assert!(reader.seek(SeekFrom::Current(-1)).is_err());
        assert_eq!(reader.read(&mut [0; 8]).unwrap(), 4);
        assert!(reader.read(&mut [0; 1]).is_err());
        assert_eq!(reader.position, 4);
    }

    #[test]
    fn inspector_fixture() {
        let Ok(mode) = std::env::var("CRISPAWN_TEST_NTFS_INSPECTOR") else {
            return;
        };
        // Break any libtest progress prefix before the private JSON lines.
        io::stdout().write_all(b"\n").unwrap();
        if mode == "timeout" {
            std::thread::sleep(Duration::from_secs(30));
            std::process::exit(0);
        }
        struct Gone(Cursor<Vec<u8>>);
        impl Read for Gone {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::from_raw_os_error(libc::ENXIO))
            }
        }
        impl Seek for Gone {
            fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
                self.0.seek(from)
            }
        }
        INSPECTOR_CHILD.store(true, Ordering::Relaxed);
        let error = inspect_reader(Gone(Cursor::new(vec![0; 4096])), 4096).unwrap_err();
        let _ = reply(&InspectorReply {
            io_errno: error.raw_os_error(),
            error: Some(error.detail),
            ..Default::default()
        });
        std::process::exit(0);
    }

    fn fixture(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "ntfs_diagnostics::tests::inspector_fixture",
                "--nocapture",
            ])
            .env("CRISPAWN_TEST_NTFS_INSPECTOR", mode);
        command
    }
    #[test]
    fn inspection_io_error_or_parser_panic_is_contained_and_retains_errno() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("volume.img");
        fs::write(&path, [0; 4096]).unwrap();
        let error = collect_inspector(
            fixture("io-error"),
            File::open(&path).unwrap(),
            Duration::from_secs(2),
        )
        .unwrap_err();
        assert_eq!(error.code, "NTFS_STATE_UNAVAILABLE");
        assert_eq!(error.raw_os_error(), Some(libc::ENXIO));
    }
    #[test]
    fn inspection_timeout_does_not_hang_the_parent() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("volume.img");
        fs::write(&path, [0; 4096]).unwrap();
        let started = Instant::now();
        let error = collect_inspector(
            fixture("timeout"),
            File::open(&path).unwrap(),
            Duration::from_millis(150),
        )
        .unwrap_err();
        assert_eq!(error.code, "NTFS_STATE_UNAVAILABLE");
        assert!(error.detail.contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn an_interrupted_inspection_read_retries_at_the_same_position() {
        struct Interrupted {
            cursor: Cursor<Vec<u8>>,
            first: bool,
        }
        impl Read for Interrupted {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                if self.first {
                    self.first = false;
                    return Err(io::Error::from_raw_os_error(libc::EINTR));
                }
                self.cursor.read(bytes)
            }
        }
        let mut reader = Bounded {
            inner: Interrupted {
                cursor: Cursor::new(vec![1, 2, 3, 4]),
                first: true,
            },
            length: 4,
            position: 0,
            remaining: 4,
            last_errno: None,
        };
        let mut data = [0; 4];
        assert_eq!(reader.read(&mut data).unwrap(), 4);
        assert_eq!(data, [1, 2, 3, 4]);
        assert_eq!(reader.position, 4);
        assert!(reader.last_errno.is_none());
    }

    #[test]
    #[ignore = "functional test: requires mkntfs and ntfsfix; ordinary temporary image only"]
    fn real_ntfs_dirty_detection_uses_read_only_handles() {
        use std::process::Command;
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("volume.img");
        let image = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        image.set_len(64 << 20).unwrap();
        drop(image);
        assert!(path.symlink_metadata().unwrap().file_type().is_file());
        assert!(Command::new("mkntfs")
            .args(["-F", "-Q", "-L", "CRISPAWN"])
            .arg(&path)
            .output()
            .unwrap()
            .status
            .success());
        let hash = || crate::io::hash_file(&File::open(&path).unwrap()).unwrap();
        let before = hash();
        let clean = inspect_reader(File::open(&path).unwrap(), 64 << 20).unwrap();
        assert!(!clean.dirty);
        assert_eq!(clean.version, "3.1");
        assert_eq!(hash(), before);
        // ntfsfix without --clear-dirty intentionally marks this disposable
        // fixture dirty. It is never used by production code.
        assert!(Command::new("ntfsfix")
            .arg(&path)
            .output()
            .unwrap()
            .status
            .success());
        let before = hash();
        let dirty = inspect_reader(File::open(&path).unwrap(), 64 << 20).unwrap();
        assert!(dirty.dirty);
        assert_eq!(hash(), before);
    }
}
