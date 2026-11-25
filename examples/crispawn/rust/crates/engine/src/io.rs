// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
use crate::{Error, Result};
use crispawn_core::{
    error::require,
    types::{hex, SourceIdentity},
};
use sha2::{Digest, Sha256};
use std::{
    cell::Cell,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::fd::AsRawFd,
    os::unix::fs::{FileExt, FileTypeExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
    time::Duration,
};

static TRANSIENT_RETRIES: AtomicU32 = AtomicU32::new(3);
static RETRY_DELAY_MS: AtomicU64 = AtomicU64::new(100);
static RETRIES: AtomicU64 = AtomicU64::new(0);
thread_local! { static DEFER_SYNC: Cell<bool> = const { Cell::new(false) }; }
/// Only the owned segmented-copy thread defers its intermediate sync calls.
/// Restore, failure persistence and other threads retain their normal policy.
pub(crate) struct DeferredSync(bool);
impl DeferredSync {
    pub fn new(enabled: bool) -> Self {
        Self(DEFER_SYNC.with(|v| v.replace(enabled)))
    }
}
impl Drop for DeferredSync {
    fn drop(&mut self) {
        DEFER_SYNC.with(|v| v.set(self.0));
    }
}
pub fn configure_retries(retries: u32, delay_ms: u64) {
    TRANSIENT_RETRIES.store(retries.min(8), Ordering::Relaxed);
    RETRY_DELAY_MS.store(delay_ms.clamp(1, 2000), Ordering::Relaxed);
}
pub fn retry_count() -> u64 {
    RETRIES.load(Ordering::Relaxed)
}

fn retry<T>(sync: bool, mut operation: impl FnMut() -> Result<T>) -> Result<T> {
    let maximum = TRANSIENT_RETRIES.load(Ordering::Relaxed);
    let mut retries = 0;
    let mut interrupted = 0;
    loop {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) if error.raw_os_error() == Some(libc::EINTR) => {
                interrupted += 1;
                if interrupted > 128 {
                    return Err(Error::new(
                        "IO_RETRY_EXHAUSTED",
                        "I/O repeatedly interrupted",
                    ));
                }
            }
            Err(error) => {
                let transient = matches!(
                    error.raw_os_error(),
                    Some(libc::EAGAIN | libc::EBUSY | libc::ETIMEDOUT)
                ) || (!sync && error.raw_os_error() == Some(libc::EIO));
                if !transient || retries >= maximum {
                    return Err(error);
                }
                let delay = RETRY_DELAY_MS
                    .load(Ordering::Relaxed)
                    .saturating_mul(1u64 << retries.min(3))
                    .min(2000);
                retries += 1;
                RETRIES.fetch_add(1, Ordering::Relaxed);
                crate::supervisor::retry(error.raw_os_error().unwrap(), retries, delay);
                std::thread::sleep(Duration::from_millis(delay));
            }
        }
    }
}

pub fn write_all(writer: &mut (impl Write + ?Sized), mut bytes: &[u8]) -> Result<()> {
    let _trace = crate::io_trace::span_if_idle(crate::io_trace::Op::Write, bytes.len() as u64);
    while !bytes.is_empty() {
        let count = retry(false, || writer.write(bytes).map_err(Error::from))?;
        require(
            count > 0 && count <= bytes.len(),
            "SHORT_WRITE",
            "invalid/zero write count",
        )?;
        bytes = &bytes[count..];
        crate::supervisor::progress();
    }
    Ok(())
}
pub fn sync_file(file: &File) -> Result<()> {
    if DEFER_SYNC.with(Cell::get) {
        return Ok(());
    }
    let _trace = crate::io_trace::span(crate::io_trace::Op::FileSync, 0);
    // EIO from writeback can mean previously accepted data was lost. A later
    // successful fsync alone cannot certify recovery; do not hide that error.
    retry(true, || file.sync_all().map_err(Error::from))?;
    crate::supervisor::progress();
    Ok(())
}
/// Flush data and metadata once after all copy workers/receipts are complete.
/// Never retry a writeback EIO into an apparent successful commit.
pub(crate) fn sync_filesystem(file: &File) -> Result<()> {
    let _trace = crate::io_trace::span(crate::io_trace::Op::FilesystemSync, 0);
    retry(true, || {
        // SAFETY: file owns a valid descriptor for the destination filesystem.
        if unsafe { libc::syncfs(file.as_raw_fd()) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error().into())
        }
    })?;
    crate::supervisor::progress();
    Ok(())
}
/// Keep parser/serializer state inside its library while retrying only the
/// underlying syscall that consumed no bytes on an error.
pub struct Retrying<T>(pub T);
fn as_io(error: Error) -> io::Error {
    error
        .raw_os_error()
        .map(io::Error::from_raw_os_error)
        .unwrap_or_else(|| io::Error::other(error.to_string()))
}
impl<T: Read> Read for Retrying<T> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        retry(false, || self.0.read(bytes).map_err(Error::from)).map_err(as_io)
    }
}
impl<T: Write> Write for Retrying<T> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        retry(false, || self.0.write(bytes).map_err(Error::from)).map_err(as_io)
    }
    fn flush(&mut self) -> io::Result<()> {
        retry(true, || self.0.flush().map_err(Error::from)).map_err(as_io)
    }
}

pub trait Reader: Send + Sync {
    fn read_at(&self, buffer: &mut [u8], offset: u64) -> io::Result<usize>;
}
impl Reader for File {
    fn read_at(&self, b: &mut [u8], o: u64) -> io::Result<usize> {
        FileExt::read_at(self, b, o)
    }
}
/// Source ranges are consumed once and already owned in RAM after read_exact_at.
/// Hint that their clean cached pages need not compete with destination cache.
/// This best-effort hint never syncs, changes data or operates on output files.
pub(crate) fn release_source_cache(source: &File, offset: u64, length: u64) {
    let _trace = crate::io_trace::span(crate::io_trace::Op::SourceCache, length);
    if length == 0 || offset > i64::MAX as u64 || length > i64::MAX as u64 {
        return;
    }
    // SAFETY: source owns the descriptor and the positive span fits off_t.
    // Advice may be unsupported or ignored, so it cannot fail the copy.
    let _ = unsafe {
        libc::posix_fadvise(
            source.as_raw_fd(),
            offset as libc::off_t,
            length as libc::off_t,
            libc::POSIX_FADV_DONTNEED,
        )
    };
}
pub fn read_exact_at(
    reader: &dyn Reader,
    mut buffer: &mut [u8],
    mut offset: u64,
    request: u64,
) -> Result<()> {
    let _trace = crate::io_trace::span_if_idle(crate::io_trace::Op::Read, buffer.len() as u64);
    require(
        request > 0 && request <= usize::MAX as u64,
        "INVALID_READ_SIZE",
        "nonzero bounded read request required",
    )?;
    let mut interrupted = 0u32;
    while !buffer.is_empty() {
        let n = buffer.len().min(request as usize);
        match retry(false, || {
            reader
                .read_at(&mut buffer[..n], offset)
                .map_err(Error::from)
        }) {
            Ok(0) => return Err(Error::new("SHORT_READ", "unexpected end of source")),
            Ok(got) => {
                crate::supervisor::progress();
                interrupted = 0;
                require(got <= n, "INVALID_READER", "oversized read")?;
                offset = offset
                    .checked_add(got as u64)
                    .ok_or_else(|| Error::new("OFFSET_OVERFLOW", "read offset"))?;
                buffer = &mut buffer[got..];
            }
            Err(e) if e.raw_os_error() == Some(libc::EINTR) => {
                interrupted += 1;
                require(
                    interrupted <= 128,
                    "IO_RETRY_EXHAUSTED",
                    "source read repeatedly interrupted",
                )?;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
pub trait RestoreTarget: Send + Sync {
    fn write_at(&self, buffer: &[u8], offset: u64) -> io::Result<usize>;
}
impl RestoreTarget for File {
    fn write_at(&self, buffer: &[u8], offset: u64) -> io::Result<usize> {
        FileExt::write_at(self, buffer, offset)
    }
}
pub fn write_all_at(file: &dyn RestoreTarget, mut b: &[u8], mut offset: u64) -> Result<()> {
    let mut interrupted = 0u32;
    while !b.is_empty() {
        match retry(false, || file.write_at(b, offset).map_err(Error::from)) {
            Ok(0) => return Err(Error::new("SHORT_WRITE", "zero-byte write")),
            Ok(n) => {
                crate::supervisor::progress();
                require(n <= b.len(), "INVALID_WRITER", "oversized write result")?;
                interrupted = 0;
                offset = offset
                    .checked_add(n as u64)
                    .ok_or_else(|| Error::new("OFFSET_OVERFLOW", "write offset"))?;
                b = &b[n..];
            }
            Err(e) if e.raw_os_error() == Some(libc::EINTR) => {
                interrupted += 1;
                require(
                    interrupted <= 128,
                    "IO_RETRY_EXHAUSTED",
                    "target write repeatedly interrupted",
                )?;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
pub fn open_regular(path: &Path, write: bool) -> Result<File> {
    let file = retry(false, || {
        OpenOptions::new()
            .read(true)
            .write(write)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(Error::from)
    })?;
    require(
        file.metadata()?.is_file(),
        "REGULAR_FILE_REQUIRED",
        "device, directory, pipe and socket adapters are not enabled",
    )?;
    Ok(file)
}
pub fn identity(file: &File) -> Result<SourceIdentity> {
    let m = retry(false, || file.metadata().map_err(Error::from))?;
    if m.file_type().is_block_device() {
        return crate::device::identity(file);
    }
    Ok(SourceIdentity {
        device: m.dev(),
        inode: m.ino(),
        bytes: m.len(),
        modified_seconds: m.mtime().to_string(),
        modified_nanos: m.mtime_nsec().to_string(),
        changed_seconds: m.ctime().to_string(),
        changed_nanos: m.ctime_nsec().to_string(),
        block_device: None,
    })
}
pub fn unchanged(file: &File, original: &SourceIdentity) -> Result<()> {
    require(
        &identity(file)? == original,
        "SOURCE_CHANGED",
        "source identity, size or timestamps changed",
    )
}
pub fn new_id() -> Result<[u8; 16]> {
    let mut b = [0; 16];
    File::open("/dev/urandom")?.read_exact(&mut b)?;
    b[6] = (b[6] & 15) | 64;
    b[8] = (b[8] & 63) | 128;
    Ok(b)
}
pub fn sync_dir(path: &Path) -> Result<()> {
    sync_file(&retry(false, || File::open(path).map_err(Error::from))?)
}
pub fn create_new(path: &Path) -> Result<File> {
    let _trace = crate::io_trace::span(crate::io_trace::Op::Create, 0);
    Ok(OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?)
}
pub fn read_small(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let f = open_regular(path, false)?;
    require(
        f.metadata()?.len() <= limit,
        "LIMIT_EXCEEDED",
        "metadata file too large",
    )?;
    let length = f.metadata()?.len();
    require(
        length <= limit && length <= usize::MAX as u64,
        "LIMIT_EXCEEDED",
        "metadata grew before bounded read",
    )?;
    let mut bytes = vec![0; length as usize];
    read_exact_at(&f, &mut bytes, 0, 65536)?;
    require(
        f.metadata()?.len() == length && length <= limit,
        "FILE_CHANGED",
        "metadata size changed",
    )?;
    Ok(bytes)
}
pub fn hash_file(file: &File) -> Result<String> {
    let len = file.metadata()?.len();
    let mut offset = 0;
    let mut h = Sha256::new();
    let mut b = vec![0; 64 * 1024];
    while offset < len {
        let n = (len - offset).min(b.len() as u64) as usize;
        read_exact_at(file, &mut b[..n], offset, 64 * 1024)?;
        h.update(&b[..n]);
        offset += n as u64;
    }
    require(
        file.metadata()?.len() == len,
        "FILE_CHANGED",
        "file length changed during hash",
    )?;
    Ok(hex(&h.finalize()))
}
pub fn atomic_write(root: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let mut temp = {
        let _trace = crate::io_trace::span(crate::io_trace::Op::Create, 0);
        tempfile::Builder::new()
            .prefix(".pbk-write-")
            .tempfile_in(root)?
    };
    write_all(&mut temp, bytes)?;
    sync_file(temp.as_file())?;
    {
        let _trace = crate::io_trace::span(crate::io_trace::Op::Rename, 0);
        temp.persist(root.join(name))
            .map_err(|e| Error::from(e.error))?;
    }
    sync_dir(root)
}
/// Atomic seal that never replaces an existing data container.
pub fn rename_new(source: &Path, target: &Path) -> Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let a = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| Error::new("INVALID_PATH", "NUL"))?;
    let b = CString::new(target.as_os_str().as_bytes())
        .map_err(|_| Error::new("INVALID_PATH", "NUL"))?;
    // SAFETY: valid NUL-terminated paths; Linux renameat2 is atomic and bounded to this call.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            a.as_ptr(),
            libc::AT_FDCWD,
            b.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}
pub fn json<T: serde::Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut b = serde_json::to_vec(value).map_err(|e| Error::new("JSON_ERROR", e.to_string()))?;
    b.push(b'\n');
    Ok(b)
}
pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    serde_json::from_slice(&read_small(path, 65536)?)
        .map_err(|e| Error::new("INVALID_JSON", e.to_string()))
}
pub fn same_file(a: &File, b: &File) -> Result<bool> {
    let (a, b) = (a.metadata()?, b.metadata()?);
    Ok(a.dev() == b.dev() && a.ino() == b.ino())
}
pub fn copy_file(source: &File, target: &mut File) -> Result<u64> {
    let identity = identity(source)?;
    let mut offset = 0;
    let mut buffer = vec![0; 65536];
    while offset < identity.bytes {
        let count = (identity.bytes - offset).min(buffer.len() as u64) as usize;
        read_exact_at(source, &mut buffer[..count], offset, 65536)?;
        write_all(target, &buffer[..count])?;
        offset += count as u64;
    }
    unchanged(source, &identity)?;
    Ok(offset)
}
pub fn lock_file(file: &File, exclusive: bool) -> Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: flock only inspects this valid FD; the lock is released on close.
    if unsafe {
        libc::flock(
            file.as_raw_fd(),
            (if exclusive {
                libc::LOCK_EX
            } else {
                libc::LOCK_SH
            }) | libc::LOCK_NB,
        )
    } != 0
    {
        return Err(Error::new(
            "FILE_BUSY",
            io::Error::last_os_error().to_string(),
        ));
    }
    Ok(())
}
pub fn available_bytes(path: &Path) -> Result<u64> {
    let _trace = crate::io_trace::span(crate::io_trace::Op::FreeSpace, 0);
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let p =
        CString::new(path.as_os_str().as_bytes()).map_err(|_| Error::new("INVALID_PATH", "NUL"))?;
    let stat = retry(false, || {
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: valid NUL-terminated path and owned writable structure.
        if unsafe { libc::statvfs(p.as_ptr(), stat.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: successful statvfs initialized this structure.
        Ok(unsafe { stat.assume_init() })
    })?;
    require(
        stat.f_flag & libc::ST_RDONLY == 0,
        "READ_ONLY_DESTINATION",
        "destination filesystem is read-only",
    )?;
    Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
}

/// Exercise the destination before creating an archive. Free-space checks are
/// only observations; the writer must still handle later write/fsync failures.
pub fn destination_preflight(path: &Path) -> Result<()> {
    let original_device = fs::metadata(path)?.dev();
    let scratch = tempfile::Builder::new()
        .prefix(".crispawn-preflight-")
        .tempdir_in(path)?;
    let first = scratch.path().join("probe.partial");
    let final_path = scratch.path().join("probe.saved");
    let mut file = create_new(&first)?;
    let bytes = [0x5au8; 4096];
    write_all(&mut file, &bytes)?;
    sync_file(&file)?;
    rename_new(&first, &final_path)?;
    sync_dir(scratch.path())?;
    let mut read = [0u8; 4096];
    read_exact_at(&open_regular(&final_path, false)?, &mut read, 0, 4096)?;
    require(
        read == bytes,
        "DESTINATION_VERIFY_FAILED",
        "preflight readback differs",
    )?;
    fs::remove_file(&final_path)?;
    scratch.close()?;
    sync_dir(path)?;
    require(
        fs::metadata(path)?.dev() == original_device,
        "DESTINATION_CHANGED",
        "destination mount changed during preflight",
    )
}

/// A small health check, not a compression trial or a claim that every sector
/// is healthy. Later EIO/short reads remain fatal; never substitute zero bytes.
pub fn source_preflight(reader: &dyn Reader, bytes: u64) -> Result<()> {
    require(bytes > 0, "EMPTY_SOURCE", "source health check")?;
    let length = bytes.min(4096) as usize;
    let mut offsets = vec![
        0,
        (bytes.saturating_sub(length as u64) / 2 / 512) * 512,
        bytes - length as u64,
    ];
    offsets.sort_unstable();
    offsets.dedup();
    let _trace = crate::io_trace::span(
        crate::io_trace::Op::SourceRead,
        length as u64 * offsets.len() as u64,
    );
    let mut buffer = vec![0; length];
    for offset in offsets {
        read_exact_at(reader, &mut buffer, offset, 4096).map_err(|error| {
            Error::new(
                error.code,
                format!("source preflight offset {offset}: {}", error.detail),
            )
        })?;
    }
    Ok(())
}

pub fn descriptor_preflight(
    plan: &crispawn_core::plan::Plan,
    source_bytes: u64,
    verifiers: u32,
) -> Result<()> {
    let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    // SAFETY: getrlimit writes the process's own descriptor budget.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let limit = unsafe { limit.assume_init() };
    let current = fs::read_dir("/proc/self/fd")?.count() as u64;
    let required = current
        + u64::from(plan.regions).min(plan.range_count(source_bytes))
        + u64::from(verifiers)
        + 12;
    require(
        required <= limit.rlim_cur,
        "FILE_DESCRIPTOR_LIMIT",
        "insufficient descriptor headroom for active files, verification and logs",
    )
}
pub struct Lock(File);
impl Lock {
    pub fn acquire(root: &Path, exclusive: bool) -> Result<Self> {
        use std::os::fd::AsRawFd;
        let file = open_regular(&root.join(".lock"), false)?;
        // SAFETY: valid owned FD; flock does not outlive this object.
        if unsafe {
            libc::flock(
                file.as_raw_fd(),
                (if exclusive {
                    libc::LOCK_EX
                } else {
                    libc::LOCK_SH
                }) | libc::LOCK_NB,
            )
        } != 0
        {
            return Err(Error::new(
                "ARCHIVE_BUSY",
                io::Error::last_os_error().to_string(),
            ));
        }
        Ok(Self(file))
    }
    pub fn held(&self) -> bool {
        self.0.metadata().is_ok()
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        // A simultaneous fork can briefly inherit the open file description,
        // despite CLOEXEC. End this guard's lease explicitly rather than waiting
        // for every inherited descriptor to close after exec. Lock is not Clone.
        // SAFETY: the guard still owns a valid descriptor; unlock never blocks.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
pub fn canonical_archive(path: &Path) -> Result<PathBuf> {
    let m = fs::symlink_metadata(path)?;
    require(
        m.is_dir() && !m.file_type().is_symlink(),
        "INVALID_ARCHIVE",
        "archive root must be a directory, not a symlink",
    )?;
    let root = path.canonicalize()?;
    let m = fs::symlink_metadata(root.join("data"))?;
    require(
        m.is_dir() && !m.file_type().is_symlink(),
        "INVALID_ARCHIVE",
        "data must be a directory, not a symlink",
    )?;
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[test]
    fn archive_guard_releases_its_lease_even_if_a_fork_inherited_the_descriptor() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(".lock"), b"").unwrap();
        let guard = Lock::acquire(root.path(), true).unwrap();
        let inherited = guard.0.try_clone().unwrap();
        assert!(Lock::acquire(root.path(), true).is_err());
        drop(guard);
        let next = Lock::acquire(root.path(), true).unwrap();
        assert!(next.held() && inherited.metadata().is_ok());
    }
    #[test]
    fn a_second_fsync_cannot_hide_the_first_writeback_eio() {
        let calls = AtomicUsize::new(0);
        let result = retry(true, || -> Result<()> {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(io::Error::from_raw_os_error(libc::EIO).into())
            } else {
                Ok(())
            }
        });
        assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EIO));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    struct Short(AtomicUsize);
    impl Reader for Short {
        fn read_at(&self, b: &mut [u8], offset: u64) -> io::Result<usize> {
            if self.0.fetch_add(1, Ordering::Relaxed) == 0 {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let n = 3.min(b.len());
            for (i, v) in b[..n].iter_mut().enumerate() {
                *v = (offset + i as u64) as u8;
            }
            Ok(n)
        }
    }
    #[test]
    fn short_reads_eintr_and_large_offsets() {
        let mut b = [0; 19];
        read_exact_at(&Short(AtomicUsize::new(0)), &mut b, (1u64 << 32) + 253, 7).unwrap();
        assert_eq!(
            b,
            (0..19)
                .map(|i| (253 + i) as u8)
                .collect::<Vec<_>>()
                .as_slice()
        );
    }
    #[test]
    fn short_writes_and_eintr_keep_offsets_and_zero_write_fails() {
        struct ShortWrite {
            calls: AtomicUsize,
            bytes: std::sync::Mutex<Vec<u8>>,
        }
        impl RestoreTarget for ShortWrite {
            fn write_at(&self, b: &[u8], offset: u64) -> io::Result<usize> {
                if self.calls.fetch_add(1, Ordering::Relaxed) == 0 {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                let n = 3.min(b.len());
                let mut bytes = self.bytes.lock().unwrap();
                let offset = (offset - (1u64 << 32)) as usize;
                bytes.resize(offset + n, 0);
                bytes[offset..offset + n].copy_from_slice(&b[..n]);
                Ok(n)
            }
        }
        let target = ShortWrite {
            calls: AtomicUsize::new(0),
            bytes: std::sync::Mutex::new(vec![]),
        };
        write_all_at(&target, b"0123456789", 1u64 << 32).unwrap();
        assert_eq!(&*target.bytes.lock().unwrap(), b"0123456789");
        struct Zero;
        impl RestoreTarget for Zero {
            fn write_at(&self, _: &[u8], _: u64) -> io::Result<usize> {
                Ok(0)
            }
        }
        assert_eq!(
            write_all_at(&Zero, b"x", 0).unwrap_err().code,
            "SHORT_WRITE"
        );
    }
}
