// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! LiveOS-only orchestration: stable source exclusion, NTFS ownership and safe shutdown.
use crate::{
    device::{self, Device},
    diagnostics::{self, Diagnostics},
    io,
    job::{Controller, JobEvents, Operation},
    job_config::Tuning,
    live_config::{self, Interface, Settings, Volume},
    Error, Result,
};
use crispawn_core::{
    error::require,
    live::{
        Command as Request, CommandAction, Destination, DestinationKind, SecureBootObservation,
        Status,
    },
    workflow::Action,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{BufRead, Read},
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{FileTypeExt, OpenOptionsExt},
        },
    },
    path::{Component, Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};
pub const RUNTIME: &str = "/run/crispawn";
pub const BUILTIN_CONFIG: &str = "/etc/perfect-backup/live.json";
pub const LOG_CAP: u64 = 16 << 20;
/// The media-layout v2 filename, relative to the mounted USB boot volume.
pub const USB_BOOT_ISO: &str = "crispawn.iso";

#[doc(hidden)]
pub fn inspect_ntfs_child(length: u64, retries: u32, delay_ms: u64) -> Result<()> {
    crate::ntfs_diagnostics::child(length, retries, delay_ms)
}

pub fn verify_usb_boot_iso(boot_root: &Path) -> Result<PathBuf> {
    let path = boot_root.join(USB_BOOT_ISO);
    let file = io::open_regular(&path, false).map_err(|error| {
        if error.raw_os_error() == Some(libc::ENOENT) {
            Error::new(
                "BOOT_ISO_NOT_FOUND",
                format!("boot ISO /{USB_BOOT_ISO} not found at {}", path.display()),
            )
        } else {
            error
        }
    })?;
    let mut magic = [0; 5];
    io::read_exact_at(&file, &mut magic, 32769, 5)?;
    require(
        &magic == b"CD001",
        "BOOT_VOLUME_MISMATCH",
        format!(
            "selected boot volume has no ISO signature at {}",
            path.display()
        ),
    )?;
    Ok(path)
}

#[path = "live_session.rs"]
mod session_runner;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreConfig {
    pub archive: String,
    pub target_id: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogPolicy {
    Always,
    OnError,
    Never,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InitialOperation {
    Backup,
    Restore,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    pub operation: InitialOperation,
    pub source_id: Option<String>,
    pub source_ids: Vec<String>,
    pub language: crispawn_core::presentation::Language,
    pub backup_directory: String,
    pub restore: Option<RestoreConfig>,
    pub tuning: Tuning,
    pub gui: bool,
    pub menu_seconds: u32,
    pub completion_seconds: u32,
    pub poweroff_on_success: bool,
    pub poweroff_on_failure: bool,
    pub log_policy: LogPolicy,
    pub log_sync_interval_ms: u32,
    pub probe_interval_ms: u32,
    pub io_observer: bool,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: 1,
            operation: InitialOperation::Backup,
            source_id: None,
            source_ids: vec![],
            language: Default::default(),
            backup_directory: "crispawn/backups".into(),
            restore: None,
            tuning: Tuning::default(),
            gui: true,
            menu_seconds: 30,
            completion_seconds: 5,
            poweroff_on_success: true,
            poweroff_on_failure: true,
            log_policy: LogPolicy::Always,
            log_sync_interval_ms: 0,
            probe_interval_ms: 250,
            io_observer: true,
        }
    }
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        self.tuning.io_policy.validate()?;
        require(
            self.schema_version == 1
                && self.menu_seconds <= 300
                && self.completion_seconds <= 60
                && self.log_sync_interval_ms <= 60_000
                && (100..=5000).contains(&self.probe_interval_ms),
            "INVALID_CONFIG",
            "LiveOS configuration limits",
        )?;
        relative(&self.backup_directory)?;
        require(
            self.source_id.is_none() || self.source_ids.is_empty(),
            "INVALID_CONFIG",
            "source_id and source_ids are mutually exclusive",
        )?;
        require(
            self.source_ids.len() <= 64,
            "INVALID_CONFIG",
            "at most 64 preset sources",
        )?;
        require(
            self.operation != InitialOperation::Restore || (!self.gui && self.restore.is_some()),
            "RESTORE_NOT_CONFIGURED",
            "unattended restore requires GUI disabled, archive and explicit target ID",
        )?;
        if let Some(r) = &self.restore {
            relative(&r.archive)?;
            require(
                !r.target_id.is_empty(),
                "INVALID_CONFIG",
                "restore target ID required",
            )?;
        }
        Ok(())
    }
}
/// An external boot-volume file overrides the ISO default. An invalid override
/// fails explicitly; it is never silently replaced by another configuration.
pub fn load_config(boot_root: &Path, builtin: &Path) -> Result<Settings> {
    let external = boot_root.join("crispawn-config.json");
    let path = match fs::symlink_metadata(&external) {
        Ok(_) => external,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => builtin.to_path_buf(),
        Err(e) => return Err(e.into()),
    };
    live_config::read_private(&path)
}
pub fn relative(value: &str) -> Result<()> {
    require(
        !value.is_empty()
            && value.len() <= 512
            && Path::new(value)
                .components()
                .all(|c| matches!(c, Component::Normal(_))),
        "INVALID_CONFIG",
        "path must contain only relative normal components",
    )
}
pub fn guard() -> Result<()> {
    require(
        unsafe { libc::geteuid() } == 0,
        "LIVEOS_REQUIRED",
        "LiveOS requires root",
    )?;
    require(
        io::read_small(Path::new("/etc/perfect-backup/live-root"), 128)?
            == b"perfect-backup-liveos-v1\n",
        "LIVEOS_REQUIRED",
        "missing LiveOS image identity",
    )?;
    let cmd = fs::read_to_string("/proc/cmdline")?;
    require(
        cmd.split_whitespace().any(|s| s == "pb.live=1"),
        "LIVEOS_REQUIRED",
        "missing LiveOS boot flag",
    )?;
    let mounts = fs::read_to_string("/proc/mounts")?;
    require(
        mounts.lines().any(|l| {
            let v: Vec<_> = l.split_whitespace().collect();
            v.len() > 2 && v[1] == "/" && matches!(v[2], "rootfs" | "tmpfs" | "ramfs")
        }),
        "LIVEOS_REQUIRED",
        "root must be an in-memory filesystem",
    )?;
    require(
        fs::read_to_string("/proc/swaps")?.lines().count() <= 1,
        "SWAP_ACTIVE",
        "swap must be disabled in LiveOS",
    )?;
    Ok(())
}
pub fn ntfs_uuid(bytes: &[u8]) -> Result<String> {
    require(
        bytes.len() >= 512 && &bytes[3..11] == b"NTFS    " && bytes[510..512] == [0x55, 0xaa],
        "NOT_NTFS",
        "missing NTFS boot-sector signature",
    )?;
    Ok(format!(
        "{:016X}",
        u64::from_le_bytes(bytes[72..80].try_into().unwrap())
    ))
}
pub fn boot_uuid_from_cmdline(args: &str) -> Result<String> {
    boot_ids(args).map(|v| v.0)
}
/// An optical boot identifies the ISO volume, independently of the writable
/// destination. Never reinterpret an ambiguous mixture as a USB boot.
pub fn optical_label_from_cmdline(args: &str) -> Result<Option<String>> {
    let labels: Vec<_> = args
        .split_whitespace()
        .filter_map(|s| s.strip_prefix("pb.optical_label="))
        .collect();
    if labels.is_empty() {
        return Ok(None);
    }
    require(
        labels.len() == 1
            && labels[0].len() == 29
            && labels[0].starts_with("CRISPAWN_")
            && labels[0][9..]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
            && !args
                .split_whitespace()
                .any(|s| s.starts_with("pb.boot_uuid=") || s.starts_with("pb.data_uuid=")),
        "INVALID_OPTICAL_BOOT",
        "one ISO volume label and no USB boot UUIDs are required",
    )?;
    Ok(Some(labels[0].into()))
}
pub fn optical_volume_label(bytes: &[u8]) -> Result<String> {
    require(
        bytes.len() >= 2048 && &bytes[..7] == b"\x01CD001\x01",
        "NOT_ISO9660",
        "missing primary ISO9660 volume descriptor",
    )?;
    let label = std::str::from_utf8(&bytes[40..72])
        .map_err(|_| Error::new("NOT_ISO9660", "non-ASCII ISO label"))?
        .trim_end_matches(' ');
    require(
        label.is_ascii() && !label.is_empty() && !label.bytes().any(|b| b.is_ascii_control()),
        "NOT_ISO9660",
        "invalid ISO volume label",
    )?;
    Ok(label.into())
}
fn find_optical_volume(devices: &[Device], label: &str) -> Result<Device> {
    let mut found = vec![];
    for d in devices
        .iter()
        .filter(|d| !d.partition && d.name.starts_with("sr") && d.size_bytes > 0)
    {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&d.path)?;
        require(
            file.metadata()?.file_type().is_block_device(),
            "BLOCK_DEVICE_REQUIRED",
            "optical boot volume",
        )?;
        let mut descriptor = [0; 2048];
        io::read_exact_at(&file, &mut descriptor, 32768, 2048)?;
        if optical_volume_label(&descriptor).ok().as_deref() == Some(label) {
            found.push(d.clone());
        }
    }
    require(
        !found.is_empty(),
        "BOOT_VOLUME_NOT_FOUND",
        "configured optical ISO is absent",
    )?;
    require(
        found.len() == 1,
        "BOOT_VOLUME_AMBIGUOUS",
        "more than one matching optical ISO",
    )?;
    Ok(found.remove(0))
}
pub fn boot_ids(args: &str) -> Result<(String, Option<String>)> {
    let found: Vec<_> = args
        .split_whitespace()
        .filter_map(|s| s.strip_prefix("pb.boot_uuid="))
        .collect();
    let data: Vec<_> = args
        .split_whitespace()
        .filter_map(|s| s.strip_prefix("pb.data_uuid="))
        .collect();
    let valid_ntfs = |s: &str| s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit());
    let valid_fat = |s: &str| {
        s.len() == 9
            && s.as_bytes()[4] == b'-'
            && s.bytes()
                .enumerate()
                .all(|(i, b)| i == 4 || b.is_ascii_hexdigit())
    };
    require(
        found.len() == 1
            && ((data.is_empty() && valid_ntfs(found[0]))
                || (data.len() == 1 && valid_fat(found[0]) && valid_ntfs(data[0]))),
        "BOOT_UUID_REQUIRED",
        "GRUB must pass one NTFS UUID, or a FAT boot UUID and separate NTFS data UUID",
    )?;
    Ok((
        found[0].to_ascii_uppercase(),
        data.first().map(|s| s.to_ascii_uppercase()),
    ))
}
pub fn fat_uuid(bytes: &[u8]) -> Result<String> {
    require(
        bytes.len() >= 512 && &bytes[82..90] == b"FAT32   " && bytes[510..512] == [0x55, 0xaa],
        "NOT_FAT32",
        "missing FAT32 boot-sector signature",
    )?;
    let value = u32::from_le_bytes(bytes[67..71].try_into().unwrap());
    Ok(format!("{:04X}-{:04X}", value >> 16, value & 0xffff))
}
fn find_volume(devices: &[Device], uuid: &str, fat: bool) -> Result<Device> {
    let mut found = vec![];
    for d in devices.iter().filter(|d| d.partition) {
        let f = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&d.path)?;
        require(
            f.metadata()?.file_type().is_block_device(),
            "BLOCK_DEVICE_REQUIRED",
            "boot volume",
        )?;
        let mut b = [0; 512];
        io::read_exact_at(&f, &mut b, 0, 512)?;
        let observed = if fat { fat_uuid(&b) } else { ntfs_uuid(&b) };
        if observed.ok().as_deref() == Some(uuid) {
            found.push(d.clone());
        }
    }
    require(
        !found.is_empty(),
        "BOOT_VOLUME_NOT_FOUND",
        "no matching volume",
    )?;
    require(
        found.len() == 1,
        "BOOT_VOLUME_AMBIGUOUS",
        format!("found {} matching volumes for {uuid}", found.len()),
    )?;
    Ok(found.remove(0))
}
pub fn require_same_media(boot: &Device, data: &Device) -> Result<()> {
    require(
        boot.partition
            && data.partition
            && boot.number != data.number
            && boot.whole_name == data.whole_name,
        "BOOT_DATA_MEDIA_MISMATCH",
        "boot and data must be distinct partitions of the same whole disk",
    )
}
/// Check the selected volume without weakening boot-media exclusion. An explicit
/// UUID selects an actual USB partition, never an arbitrary internal disk.
pub fn require_destination(settings: &Settings, boot: &Device, data: &Device) -> Result<()> {
    require(
        data.partition,
        "DESTINATION_NOT_PARTITION",
        "NTFS destination must be a partition",
    )?;
    match settings.volume {
        Volume::BootDiskData {} => require(
            boot.whole_name == data.whole_name,
            "BOOT_DATA_MEDIA_MISMATCH",
            "boot/data whole disk changed",
        ),
        Volume::Uuid { .. } => require(
            data.usb,
            "DESTINATION_NOT_USB",
            "explicit NTFS destination is not USB attached",
        ),
    }
}
pub fn destination_error(error: Error) -> Error {
    match error.code {
        "BOOT_VOLUME_NOT_FOUND" => {
            Error::new("DESTINATION_NOT_FOUND", "configured NTFS volume is absent")
        }
        "BOOT_VOLUME_AMBIGUOUS" => Error::new(
            "DESTINATION_VOLUME_AMBIGUOUS",
            "configured NTFS volume is not unique",
        ),
        // Preserve the errno and original failure classification for retry and
        // diagnostics. An I/O failure is not evidence of an ambiguous UUID.
        _ => error,
    }
}
pub fn source_candidates<'a>(
    devices: &'a [Device],
    boot_whole: &str,
    destination_whole: &str,
) -> Vec<&'a Device> {
    device::candidates(devices, destination_whole)
        .into_iter()
        .filter(|d| d.whole_name != boot_whole)
        .collect()
}
pub fn observe_secure_boot(sys: &Path) -> SecureBootObservation {
    let efivars = sys.join("firmware/efi/efivars");
    let read = |name: &str, guid: &str| {
        fs::read(efivars.join(format!("{name}-{guid}")))
            .ok()
            .and_then(|b| b.get(4).copied())
            .filter(|b| *b <= 1)
            .map(|b| b == 1)
    };
    SecureBootObservation {
        enabled: read("SecureBoot", "8be4df61-93ca-11d2-aa0d-00e098032b8c"),
        setup_mode: read("SetupMode", "8be4df61-93ca-11d2-aa0d-00e098032b8c"),
        shim_validation_disabled: read("MokSBStateRT", "605dab50-e046-4300-abb6-3dd810dd8b23")
            == Some(true),
        lockdown: fs::read_to_string(sys.join("kernel/security/lockdown"))
            .unwrap_or_default()
            .trim()
            .into(),
    }
}
pub fn require_secure_boot(s: &SecureBootObservation) -> Result<()> {
    require(
        s.enabled == Some(true)
            && s.setup_mode == Some(false)
            && !s.shim_validation_disabled
            && (s.lockdown.contains("[integrity]") || s.lockdown.contains("[confidentiality]")),
        "SECURE_BOOT_NOT_ENFORCED",
        "Secure Boot, user mode, shim validation and kernel lockdown must all be enforced",
    )
}
pub fn recovery_log_policy(root: &Path, destination: &Path) -> LogPolicy {
    live_config::read(&root.join("live-config.json"))
        .or_else(|_| live_config::read(&destination.join("crispawn-config.json")))
        .map(|c| c.log_policy)
        .unwrap_or(LogPolicy::Always)
}
struct Mounted {
    device: Device,
    path: PathBuf,
    filesystem: &'static str,
}
impl Mounted {
    fn mount(device: Device, path: PathBuf) -> Result<Self> {
        Self::mount_kind(device, path, "ntfs3", false)
    }
    fn mount_boot(device: Device, path: PathBuf, fat: bool) -> Result<Self> {
        Self::mount_kind(device, path, if fat { "vfat" } else { "ntfs3" }, true)
    }
    fn mount_kind(
        device: Device,
        path: PathBuf,
        filesystem: &'static str,
        read_only: bool,
    ) -> Result<Self> {
        let info = fs::read_to_string("/proc/self/mountinfo")?;
        require(
            !info
                .lines()
                .any(|l| l.split_whitespace().nth(2) == Some(device.number.as_str())),
            "DESTINATION_ALREADY_MOUNTED",
            "boot NTFS is already mounted",
        )?;
        fs::create_dir(&path)?;
        let dev = CString::new(device.path.as_os_str().as_bytes())
            .map_err(|_| Error::new("INVALID_PATH", "NUL"))?;
        let dst = CString::new(path.as_os_str().as_bytes()).unwrap();
        let fs_name = CString::new(filesystem).unwrap();
        let ntfs_options = CString::new(crate::ntfs_diagnostics::MOUNT_OPTIONS).unwrap();
        let options = if filesystem == "iso9660" {
            c""
        } else if filesystem == "vfat" {
            c"uid=0,gid=0,umask=077,iocharset=utf8"
        } else {
            ntfs_options.as_c_str()
        };
        // SAFETY: validated block device, owned empty mountpoint, fixed filesystem/options.
        if unsafe {
            libc::mount(
                dev.as_ptr(),
                dst.as_ptr(),
                fs_name.as_ptr(),
                libc::MS_NODEV
                    | libc::MS_NOSUID
                    | libc::MS_NOEXEC
                    | if read_only { libc::MS_RDONLY } else { 0 },
                options.as_ptr().cast(),
            )
        } != 0
        {
            let mut error = Error::from(std::io::Error::last_os_error());
            error.code = if filesystem == "iso9660" {
                "BOOT_MOUNT_FAILED"
            } else {
                "NTFS_MOUNT_FAILED"
            };
            return Err(error);
        }
        let mounted = Self {
            device,
            path,
            filesystem,
        };
        mounted.check()?;
        Ok(mounted)
    }
    fn check(&self) -> Result<()> {
        let info = fs::read_to_string("/proc/self/mountinfo")?;
        require(
            info.lines().any(|l| {
                let f: Vec<_> = l.split_whitespace().collect();
                f.get(2) == Some(&self.device.number.as_str())
                    && f.get(4) == self.path.to_str().as_ref()
                    && l.split_once(" - ")
                        .map(|(_, r)| r.split_whitespace().next() == Some(self.filesystem))
                        .unwrap_or(false)
            }),
            "MOUNT_IDENTITY_MISMATCH",
            "destination mount source/type changed",
        )
    }
    fn sync(&self) -> Result<()> {
        self.check()?;
        let f = File::open(&self.path)?;
        // SAFETY: valid mount FD; syncfs errors are propagated.
        if unsafe { libc::syncfs(f.as_raw_fd()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
    /// If NTFS is unavailable, retain one bounded diagnostic on our verified
    /// FAT boot partition. Never mount the failed NTFS, touch EFI/ISO files or
    /// use this fallback when logging is disabled or the boot medium is RO.
    fn save_mount_diagnostic(&self, bytes: &[u8], job_id: &str) -> Result<PathBuf> {
        require(
            self.filesystem == "vfat" && bytes.len() <= 32 << 10,
            "BOOT_DIAGNOSTICS_UNAVAILABLE",
            "a writable FAT boot partition is required",
        )?;
        require(
            job_id.len() == 32 && job_id.bytes().all(|b| b.is_ascii_hexdigit()),
            "INVALID_PATH",
            "invalid diagnostics job identifier",
        )?;
        self.check()?;
        verify_usb_boot_iso(&self.path)?;
        let target = CString::new(self.path.as_os_str().as_bytes()).unwrap();
        let remount = |read_only: bool| -> Result<()> {
            // SAFETY: exact owned, identity-checked FAT mount; no force option.
            if unsafe {
                libc::mount(
                    std::ptr::null(),
                    target.as_ptr(),
                    std::ptr::null(),
                    libc::MS_REMOUNT
                        | libc::MS_NODEV
                        | libc::MS_NOSUID
                        | libc::MS_NOEXEC
                        | if read_only { libc::MS_RDONLY } else { 0 },
                    std::ptr::null(),
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(())
        };
        remount(false)?;
        let saved = (|| {
            let folder = owned_directory(&self.path, "crispawn/diagnostics")?;
            let path = folder.join(format!("ntfs-mount-{job_id}.json"));
            let mut file = io::create_new(&path)?;
            io::write_all(&mut file, bytes)?;
            io::sync_file(&file)?;
            io::sync_dir(&folder)?;
            self.sync()?;
            Ok(path)
        })();
        // Also attempted after a failed create/write; return RO before shutdown.
        let closed = remount(true);
        match (saved, closed) {
            (_, Err(error)) => Err(error),
            (saved, Ok(())) => saved,
        }
    }
    fn unmount(&self) -> Result<()> {
        self.sync()?;
        let dst = CString::new(self.path.as_os_str().as_bytes()).unwrap();
        // SAFETY: only the exact owned mountpoint; no force or lazy unmount.
        if unsafe { libc::umount2(dst.as_ptr(), 0) } != 0 {
            return Err(Error::new(
                "NTFS_UNMOUNT_FAILED",
                std::io::Error::last_os_error().to_string(),
            ));
        }
        require(
            !fs::read_to_string("/proc/self/mountinfo")?
                .lines()
                .any(|l| l.split_whitespace().nth(4) == self.path.to_str()),
            "NTFS_UNMOUNT_FAILED",
            "mountpoint is still mounted",
        )
    }
}
pub fn owned_directory(root: &Path, name: &str) -> Result<PathBuf> {
    relative(name)?;
    let device = std::os::unix::fs::MetadataExt::dev(&fs::metadata(root)?);
    let mut path = root.to_path_buf();
    for component in Path::new(name).components() {
        path.push(component.as_os_str());
        if !path.try_exists()? {
            fs::create_dir(&path)?;
            io::sync_dir(path.parent().unwrap())?;
        }
        let m = fs::symlink_metadata(&path)?;
        require(
            m.is_dir()
                && !m.file_type().is_symlink()
                && std::os::unix::fs::MetadataExt::dev(&m) == device,
            "UNSAFE_DESTINATION",
            "directory escaped destination filesystem",
        )?;
    }
    Ok(path)
}
fn metric(path: &str, key: &str) -> u64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with(key))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|n| n.parse().ok())
        })
        .unwrap_or(0)
}
fn active_vt() -> u32 {
    #[repr(C)]
    struct Vt {
        active: u16,
        signal: u16,
        state: u16,
    }
    let Ok(f) = File::open("/dev/tty0") else {
        return 0;
    };
    let mut vt = Vt {
        active: 0,
        signal: 0,
        state: 0,
    };
    if unsafe { libc::ioctl(f.as_raw_fd(), 0x5603, &mut vt) } == 0 {
        u32::from(vt.active)
    } else {
        0
    }
}
pub fn ui_fresh(value: &serde_json::Value, now_ms: u64) -> bool {
    value.get("schema_version").and_then(|v| v.as_u64()) == Some(1)
        && value
            .get("observed_monotonic_ms")
            .and_then(|v| v.as_u64())
            .map(|seen| seen <= now_ms && now_ms - seen <= 5000)
            .unwrap_or(false)
}
fn show_logs() -> Result<()> {
    let f = File::open("/dev/tty0")?;
    if unsafe { libc::ioctl(f.as_raw_fd(), 0x5606, 2) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
pub fn watch(root: &Path) -> Result<()> {
    loop {
        if let Ok(s) = io::read_json::<Status>(&root.join("status.json")) {
            let snap = s.snapshot.as_ref();
            println!(
                "\x1b[2J\x1b[HCRISPAWN - diagnostic console (Alt+F1: GUI)\n\
phase={} heartbeat={} age={}ms rss={}KiB vt={}\nsource={} excluded={}\n\
progress={}% read={} durable={} total={} verified={} verification_read={}\nlogs_dropped={} ntfs_mounted={}\n{}",
                s.phase,
                s.heartbeat,
                s.progress_age_ms,
                s.rss_kib,
                s.active_vt,
                s.source_disk,
                s.excluded_disk,
                s.percent,
                snap.map(|s| s.read_raw_bytes).unwrap_or(0),
                snap.map(|s| s.durable_raw_bytes).unwrap_or(0),
                snap.map(|s| s.total_bytes).unwrap_or(0),
                snap.map(|s| s.verified_raw_bytes).unwrap_or(0),
                snap.map(|s| s.verification_read_raw_bytes).unwrap_or(0),
                s.log_dropped,
                s.ntfs_mounted,
                diagnostics::tail(&root.join("logs"), 4096)
                    .unwrap_or_default()
                    .chars()
                    .map(|c| if c.is_ascii() && !c.is_control() {
                        c
                    } else if c == '\n' {
                        '\n'
                    } else {
                        '?'
                    })
                    .collect::<String>()
            );
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}
struct Frontends {
    gui: Option<Child>,
    viewer: Option<Child>,
    early_gui: bool,
    console: Option<Child>,
}
impl Frontends {
    fn stop(&mut self) {
        if self.early_gui {
            // No PID supplied by a status file is ever signalled. The display
            // observes a sentinel in this controller's private runtime.
            let _ = fs::write(Path::new(RUNTIME).join("ui-stop"), b"stop\n");
            let until = Instant::now() + Duration::from_secs(3);
            while Path::new(RUNTIME).join("ui.json").exists() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(20));
            }
            self.early_gui = false;
        }
        for slot in [&mut self.gui, &mut self.viewer, &mut self.console] {
            let Some(mut child) = slot.take() else {
                continue;
            };
            if child.try_wait().ok().flatten().is_none() {
                unsafe {
                    libc::kill(child.id() as i32, libc::SIGTERM);
                }
            }
            let until = Instant::now() + Duration::from_secs(3);
            while Instant::now() < until {
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}
fn start_frontends(interface: Interface, diag: &Arc<Diagnostics>) -> Result<Frontends> {
    let gui = interface == Interface::Gui;
    let early_expected = Path::new("/run/crispawn-display/pid").is_file();
    let mut early_gui = false;
    if early_expected {
        let until = Instant::now() + Duration::from_secs(3);
        while Instant::now() < until {
            if let Ok(value) =
                io::read_json::<serde_json::Value>(&Path::new(RUNTIME).join("ui.json"))
            {
                if ui_fresh(&value, diag.ms())
                    && value["job_id"] == diag.snapshot().job_id
                    && value["prestarted"] == true
                {
                    early_gui = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if let Ok(text) = fs::read_to_string("/run/crispawn-display/startup.log") {
            diag.record(
                "early_display",
                json!({"text":text.chars().take(16384).collect::<String>(),"adopted":early_gui}),
            );
        }
        if !gui {
            fs::write(Path::new(RUNTIME).join("ui-stop"), b"disabled\n")?;
            early_gui = false;
        }
    }
    let viewer = OpenOptions::new()
        .write(true)
        .open("/dev/tty2")
        .ok()
        .and_then(|f| {
            Command::new("/usr/bin/crispawn-live")
                .args(["watch", RUNTIME])
                .stdin(Stdio::null())
                .stdout(Stdio::from(f))
                .stderr(Stdio::null())
                .spawn()
                .ok()
        });
    let child = if gui && !early_gui {
        let mut child = Command::new("/usr/bin/crispawn-gui")
            .arg(RUNTIME)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        for pipe in [
            child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
            child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
        ]
        .into_iter()
        .flatten()
        {
            let diag = diag.clone();
            std::thread::spawn(move || {
                let mut reader = std::io::BufReader::new(pipe);
                loop {
                    let mut line = vec![];
                    match Read::take(&mut reader, 8192).read_until(b'\n', &mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => diag
                            .record("gui_output", json!({"text":String::from_utf8_lossy(&line)})),
                    }
                }
            });
        }
        Some(child)
    } else {
        None
    };
    let console = if interface == Interface::Console {
        let tty = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty1")?;
        Some(
            Command::new("/usr/bin/crispawn-live")
                .args(["console", RUNTIME])
                .stdin(Stdio::from(tty.try_clone()?))
                .stdout(Stdio::from(tty.try_clone()?))
                .stderr(Stdio::from(tty))
                .spawn()?,
        )
    } else {
        None
    };
    Ok(Frontends {
        gui: child,
        viewer,
        early_gui,
        console,
    })
}
fn requests(root: &Path) -> Result<Vec<Request>> {
    let mut paths = fs::read_dir(root.join("commands"))?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    paths.sort();
    require(
        paths.len() <= 128,
        "CONTROL_OVERFLOW",
        "too many pending commands",
    )?;
    let mut found = vec![];
    for path in paths {
        let name = path.file_name().unwrap().to_string_lossy();
        require(
            name.starts_with("cmd-") && name.ends_with(".json"),
            "INVALID_COMMAND",
            "unexpected control file",
        )?;
        let request: Request = io::read_json(&path)?;
        require(
            request.schema_version == 1
                && request.id.len() == 32
                && request.id.bytes().all(|b| b.is_ascii_hexdigit())
                && name == format!("cmd-{}.json", request.id),
            "INVALID_COMMAND",
            "command identity/version",
        )?;
        fs::remove_file(path)?;
        found.push(request);
    }
    Ok(found)
}
fn export(diag: &Diagnostics, mount: &Mounted, kind: &str) -> Result<PathBuf> {
    mount.check()?;
    let parent = owned_directory(&mount.path, "crispawn/logs")?;
    let id = diag.snapshot().job_id;
    let target = parent.join(format!(
        "{id}-{kind}-{}",
        io::new_id()?
            .iter()
            .take(4)
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    ));
    diag.export(&target)?;
    if let Err(error) = crate::io_trace::export(&target) {
        diag.record(
            "io_observer_export_failed",
            json!({"error":error.to_string()}),
        );
    }
    io::atomic_write(&target, "probe-final.json", &io::json(&diag.snapshot())?)?;
    mount.sync()?;
    Ok(target)
}
fn handle(
    request: Request,
    controller: Option<&Controller>,
    diag: &Diagnostics,
    mount: &Mounted,
) -> Result<Option<CommandAction>> {
    diag.status
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .last_command = request.id;
    match request.action {
        CommandAction::SaveLogs => {
            export(diag, mount, "manual")?;
        }
        CommandAction::ShowLogs => show_logs()?,
        CommandAction::Pause | CommandAction::Resume | CommandAction::Cancel => {
            if let Some(c) = controller {
                let action = match request.action {
                    CommandAction::Pause => Action::Pause,
                    CommandAction::Resume => Action::Resume,
                    _ => Action::Cancel,
                };
                if let Err(e) = c.request(action) {
                    diag.record("control_rejected", json!({"code":e.code,"detail":e.detail}));
                }
            }
        }
        action => return Ok(Some(action)),
    }
    Ok(None)
}
// A diagnostic UART can disappear or reject writes. In release builds a
// println! panic would abort the entire worker, so this stream is fallible.
fn diagnostic_output(receiver: mpsc::Receiver<String>, mut output: impl std::io::Write) {
    for line in receiver {
        if writeln!(output, "{line}").is_err() {
            break;
        }
    }
}

pub fn run() -> Result<()> {
    guard()?;
    let root = Path::new(RUNTIME);
    fs::create_dir(root)?;
    fs::create_dir(root.join("commands"))?;
    let id = crispawn_core::types::hex(&io::new_id()?);
    let diag = Diagnostics::new(&root.join("logs"), LOG_CAP, id)?;
    let stop = Arc::new(AtomicBool::new(false));
    let (serial_tx, serial_rx) = mpsc::sync_channel::<String>(16);
    std::thread::spawn(move || {
        diagnostic_output(serial_rx, std::io::stdout().lock());
    });
    let monitor = {
        let diag = diag.clone();
        let stop = stop.clone();
        let tx = serial_tx.clone();
        std::thread::spawn(move || {
            crate::supervisor::observer_thread();
            let mut count = 0u64;
            let mut retries_seen = 0u64;
            while !stop.load(Ordering::Relaxed) {
                {
                    let mut s = diag.status.lock().unwrap_or_else(|e| e.into_inner());
                    s.heartbeat += 1;
                    s.io_retries = io::retry_count();
                    s.rss_kib = metric("/proc/self/status", "VmRSS:");
                    let mut memory = crate::memory::sample();
                    if let Some(old) = &s.memory {
                        memory.processing_limit_bytes = old.processing_limit_bytes;
                        memory.encoded_queue_bytes = old.encoded_queue_bytes;
                        memory.encoded_queue_limit_bytes = old.encoded_queue_limit_bytes;
                    }
                    s.memory_available_kib = memory.available_kib;
                    s.memory = Some(memory);
                    s.active_vt = active_vt();
                    s.gui_ready = io::read_json::<serde_json::Value>(&root.join("ui.json"))
                        .map(|value| ui_fresh(&value, diag.ms()))
                        .unwrap_or(false);
                }
                let status = diag.snapshot();
                if status.io_retries != retries_seen {
                    diag.record(
                        "io_retries",
                        json!({"total":status.io_retries,"previous":retries_seen}),
                    );
                    retries_seen = status.io_retries;
                }
                if let Ok(bytes) = io::json(&status) {
                    let _ = io::atomic_write(root, "status.json", &bytes);
                }
                if count % 4 == 0 {
                    diag.record(
                        "memory_sample",
                        json!({"rss_kib":status.rss_kib,"memory":status.memory}),
                    );
                    let _ = tx.try_send(format!(
                        "PB_PROBE {}",
                        serde_json::to_string(&status.probe_projection()).unwrap()
                    ));
                }
                if count % 20 == 0 {
                    if let Ok(sample) =
                        io::read_json::<serde_json::Value>(&root.join("io-observer/latest.json"))
                    {
                        diag.record("io_observer", sample);
                    }
                }
                count += 1;
                std::thread::sleep(Duration::from_millis(
                    diag.probe_interval_ms.load(Ordering::Relaxed),
                ));
            }
        })
    };
    let result = run_inner(root, &diag, &serial_tx);
    if let Err(e) = &result {
        let failure_config =
            io::read_json::<Config>(&root.join("live-config.json")).unwrap_or_default();
        let _ = serial_tx.try_send(format!(
            "CRISPAWN_ERROR {}",
            failure_config.language.error_detail(&e.to_string())
        ));
        crate::supervisor::failure(e);
        // Independent of the NTFS collector, which may be the failed device.
        if let Ok(mut file) = io::create_new(&root.join("failure-ram.jsonl")) {
            let _ = std::io::Write::write_all(&mut file, &diag.emergency_tail());
        }
        diag.phase(
            if e.code == "CANCELLED" {
                "CANCELLED"
            } else {
                "FAILED"
            },
            &format!("{}: {}", e.code, e.detail),
        );
        {
            let mut s = diag.status.lock().unwrap_or_else(|e| e.into_inner());
            s.session.page = crispawn_core::presentation::Page::Failed;
            s.can_pause = false;
            s.can_resume = false;
            s.can_cancel = false;
            s.finalization_complete = false;
            s.shutdown_countdown_seconds = failure_config
                .poweroff_on_failure
                .then_some(failure_config.completion_seconds);
        }
        let _ = io::atomic_write(
            root,
            "status.json",
            &io::json(&diag.snapshot()).unwrap_or_default(),
        );
        let _ = serial_tx.try_send(format!(
            "PB_FAILURE {}",
            json!({"code":e.code,"detail":e.detail})
        ));
        let _ = serial_tx.try_send(format!(
            "PB_PROBE {}",
            serde_json::to_string(&diag.snapshot().probe_projection()).unwrap()
        ));
        let owned_mount = io::read_json::<Device>(&root.join("owned-mount.json"))
            .ok()
            .map(|device| Mounted {
                device,
                path: root.join("destination"),
                filesystem: "ntfs3",
            })
            .filter(|m| m.check().is_ok());
        let mut storage_closed = owned_mount.is_none();
        if let Some(mount) = &owned_mount {
            let policy = recovery_log_policy(root, &mount.path);
            if policy != LogPolicy::Never {
                if let Err(error) = export(&diag, mount, "failure") {
                    diag.record("log_export_failed", json!({"error":error.to_string()}));
                }
            }
            if diag.close_after_failure().is_ok() && mount.unmount().is_ok() {
                storage_closed = true;
            }
        }
        if storage_closed {
            diag.status
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .ntfs_mounted = false;
            crate::supervisor::failure_storage_closed();
            crate::supervisor::failure_presented();
        }
        crate::supervisor::phase("MENU");
        if failure_config.poweroff_on_failure {
            let until =
                Instant::now() + Duration::from_secs(u64::from(failure_config.completion_seconds));
            while Instant::now() < until {
                diag.status
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .shutdown_countdown_seconds = Some(
                    until
                        .saturating_duration_since(Instant::now())
                        .as_millis()
                        .div_ceil(1000) as u32,
                );
                std::thread::sleep(Duration::from_millis(50));
            }
            guard()?;
            diag.status
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .poweroff_requested = true;
            let _ = serial_tx.try_send(format!(
                "PB_PROBE {}",
                serde_json::to_string(&diag.snapshot().probe_projection()).unwrap()
            ));
            crate::supervisor::phase("POWEROFF_REQUESTED");
            require(
                Command::new("/sbin/poweroff").status()?.success(),
                "POWEROFF_FAILED",
                "init rejected failure shutdown",
            )?;
        }
        // Both configured outcomes leave the failed job stopped. The monitor
        // and view remain alive; only the poweroff policy requests shutdown.
        loop {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    stop.store(true, Ordering::Relaxed);
    let _ = monitor.join();
    diag.stop();
    result
}

#[cfg(test)]
mod serial_diagnostic_tests {
    use super::diagnostic_output;
    use std::{io, sync::mpsc};

    #[test]
    fn unavailable_diagnostic_output_does_not_panic() {
        struct FailedUart;
        impl io::Write for FailedUart {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from_raw_os_error(libc::EIO))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (tx, rx) = mpsc::channel();
        tx.send("PB_PROBE diagnostic only".into()).unwrap();
        drop(tx);
        diagnostic_output(rx, FailedUart);
    }

    #[test]
    fn available_diagnostic_output_retains_complete_lines() {
        let (tx, rx) = mpsc::channel();
        tx.send("PB_PROBE first".into()).unwrap();
        tx.send("PB_PROBE second".into()).unwrap();
        drop(tx);
        let mut output = Vec::new();
        diagnostic_output(rx, &mut output);
        assert_eq!(output, b"PB_PROBE first\nPB_PROBE second\n");
    }
}
fn run_inner(
    root: &Path,
    diag: &Arc<Diagnostics>,
    serial: &mpsc::SyncSender<String>,
) -> Result<()> {
    let cmdline = fs::read_to_string("/proc/cmdline")?;
    let optical = optical_label_from_cmdline(&cmdline)?;
    let security = observe_secure_boot(Path::new("/sys"));
    diag.record("secure_boot", json!(&security));
    diag.status
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .secure_boot = Some(security.clone());
    // Optical media also boots with Secure Boot disabled or legacy BIOS. If
    // firmware enables it, enforce the same complete chain as the USB route.
    if optical.is_some() && Path::new("/sys/firmware/efi").is_dir() {
        require(
            security.enabled.is_some() && security.setup_mode.is_some(),
            "SECURE_BOOT_NOT_ENFORCED",
            "UEFI security state could not be read",
        )?;
    }
    if (optical.is_some() && security.enabled == Some(true))
        || cmdline
            .split_whitespace()
            .any(|s| s == "pb.require_secure_boot=1")
    {
        require_secure_boot(&security)?;
    }
    let (uuid, data_uuid) = match &optical {
        Some(label) => (label.clone(), None),
        None => boot_ids(&cmdline)?,
    };
    diag.phase(
        "DISCOVERING",
        "locating the boot ISO and backup data volume",
    );
    let until = Instant::now() + Duration::from_secs(30);
    let (devices, boot, data) = loop {
        let devices = device::scan(Path::new("/sys"), Path::new("/dev"))?;
        let found = (|| {
            let boot = if optical.is_some() {
                find_optical_volume(&devices, &uuid)?
            } else {
                find_volume(&devices, &uuid, data_uuid.is_some())?
            };
            let data = match &data_uuid {
                Some(id) => {
                    let data = find_volume(&devices, id, false)?;
                    require_same_media(&boot, &data)?;
                    data
                }
                None => boot.clone(),
            };
            Ok::<_, Error>((boot, data))
        })();
        match found {
            Ok((boot, data)) => break (devices, boot, data),
            Err(e) if Instant::now() < until => {
                diag.record("waiting_for_boot_volume", json!({"code":e.code}));
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Err(e),
        }
    };
    {
        let mut s = diag.status.lock().unwrap_or_else(|e| e.into_inner());
        s.boot_uuid = uuid.clone();
        s.data_uuid = data_uuid.clone().unwrap_or_else(|| s.boot_uuid.clone());
        s.excluded_disk = boot.whole_name.clone();
        s.boot_partition = boot.path.to_string_lossy().into_owned();
        s.destination_partition = data.path.to_string_lossy().into_owned();
    }
    // Read the authoritative override from a read-only boot mount BEFORE any
    // destination writes. Invalid v2/unsupported backends never fall back to v1.
    let mut boot_mount = Some(if optical.is_some() {
        Mounted::mount_kind(boot.clone(), root.join("boot"), "iso9660", true)?
    } else {
        Mounted::mount_boot(boot.clone(), root.join("boot"), data_uuid.is_some())?
    });
    let boot_root = &boot_mount.as_ref().unwrap().path;
    let mut config = load_config(boot_root, Path::new(BUILTIN_CONFIG))?;
    // Apply retry/timeout and language before probing the configured destination.
    crate::supervisor::configure(&config.tuning.io_policy);
    if config.io_observer {
        match crate::io_trace::start(root) {
            Ok(()) => diag.record(
                "io_observer_started",
                json!({"independent_process":true,"ram_cap_bytes":8 << 20}),
            ),
            Err(error) => diag.record(
                "io_observer_unavailable",
                json!({"error":error.to_string()}),
            ),
        }
    }
    crate::supervisor::live_failure_policy(
        config.poweroff_on_failure,
        config.completion_seconds,
        config.language,
    );
    io::atomic_write(root, "live-config.json", &io::json(&config.config)?)?;
    diag.status
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .language = config.language;
    if optical.is_none() {
        let path = verify_usb_boot_iso(boot_root)?;
        diag.record(
            "usb_boot_iso_verified",
            json!({"path":path,"boot_iso_path":"/crispawn.iso"}),
        );
    } else {
        require(
            matches!(config.volume, Volume::Uuid { .. }),
            "OPTICAL_DESTINATION_UUID_REQUIRED",
            "CD/DVD boot requires an explicit USB NTFS destination UUID",
        )?;
        diag.record(
            "optical_boot",
            json!({"volume_label":uuid,"device":boot.path,"read_only":true}),
        );
    }
    let devices = if optical.is_some() {
        // USB enumeration can finish after the optical drive. Wait only for an
        // absent UUID, never retry an ambiguity, I/O error or mismatched device.
        loop {
            let refreshed = device::scan(Path::new("/sys"), Path::new("/dev"))?;
            let Volume::Uuid { uuid } = &config.volume else {
                unreachable!()
            };
            match find_volume(&refreshed, uuid, false) {
                Ok(_) => break refreshed,
                Err(e) if e.code == "BOOT_VOLUME_NOT_FOUND" && Instant::now() < until => {
                    std::thread::sleep(Duration::from_millis(200))
                }
                Err(e) => return Err(destination_error(e)),
            }
        }
    } else {
        devices
    };
    let data = match &config.volume {
        Volume::BootDiskData {} => data,
        Volume::Uuid { uuid } => find_volume(&devices, uuid, false).map_err(destination_error)?,
    };
    require_destination(&config, &boot, &data)?;
    // A single NTFS boot/data partition must not be mounted twice. FAT ESP and
    // separate boot disks remain read-only until finalization.
    if boot.number == data.number {
        boot_mount.take().unwrap().unmount()?;
    }
    io::atomic_write(root, "live-config.json", &io::json(&config.config)?)?;
    io::atomic_write(
        root,
        "runtime-settings.json",
        &io::json(&json!({
            "runtime_version":config.version,"startup":config.startup,"ui_mode":config.interface,"volume":config.volume,
        }))?,
    )?;
    diag.phase("MOUNTING_NTFS", "mounting the configured NTFS data volume");
    let mut attempt = crate::ntfs_diagnostics::Attempt::begin(
        &data,
        &root.join("destination"),
        &config.tuning.io_policy,
    );
    let mounted = attempt.preflight().and_then(|()| {
        attempt.report.mount_attempted = true;
        Mounted::mount(data, root.join("destination"))
    });
    let mount = match mounted {
        Ok(mount) => {
            let (report, _) = attempt.finish(None);
            diag.record(
                "ntfs_mount_diagnostics",
                serde_json::to_value(report).unwrap(),
            );
            mount
        }
        Err(error) => {
            let (mut report, error) = attempt.finish(Some(error));
            report.user_message = Some(
                config
                    .language
                    .error_detail(&error.as_ref().unwrap().to_string()),
            );
            let bytes = io::json(&report)?;
            let saved = io::atomic_write(root, "ntfs-mount-diagnostics.json", &bytes);
            diag.record("ntfs_mount_failure", serde_json::to_value(report).unwrap());
            if let Err(save_error) = saved {
                diag.record(
                    "ntfs_diagnostics_save_failed",
                    json!({"error":save_error.to_string()}),
                );
            }
            if config.log_policy != LogPolicy::Never {
                if let Some(boot) = boot_mount.as_ref().filter(|b| b.filesystem == "vfat") {
                    match boot.save_mount_diagnostic(&bytes, &diag.snapshot().job_id) {
                        Ok(path) => diag.record("ntfs_diagnostics_persisted", json!({"path":path,"medium":"boot_esp","ntfs_written":false})),
                        Err(error) => diag.record("ntfs_diagnostics_persist_failed", json!({"error":error.to_string(),"ram_path":"/run/crispawn/ntfs-mount-diagnostics.json"})),
                    }
                }
            }
            return Err(error.unwrap());
        }
    };
    diag.status
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .ntfs_mounted = true;
    io::atomic_write(root, "owned-mount.json", &io::json(&mount.device)?)?;
    crate::supervisor::failure_log_target(
        &diag.snapshot().job_id,
        &mount.device.number,
        config.log_policy != LogPolicy::Never,
    );
    {
        let mut s = diag.status.lock().unwrap_or_else(|e| e.into_inner());
        s.destination_partition = mount.device.path.to_string_lossy().into_owned();
        if let Volume::Uuid { uuid } = &config.volume {
            s.data_uuid = uuid.clone();
        }
    }
    {
        let mut s = diag.status.lock().unwrap_or_else(|e| e.into_inner());
        s.backup_destination = Some(Destination {
            kind: DestinationKind::UsbDisk,
            filesystem: Some("NTFS".into()),
        });
        s.log_destination = Some(if config.log_policy == LogPolicy::Always {
            Destination {
                kind: DestinationKind::UsbDisk,
                filesystem: Some("NTFS".into()),
            }
        } else {
            Destination {
                kind: DestinationKind::Memory,
                filesystem: None,
            }
        });
    }
    if config.log_policy == LogPolicy::Always {
        let parent = owned_directory(&mount.path, "crispawn/logs")?;
        let target = parent.join(format!(
            "{}-live-{}",
            diag.snapshot().job_id,
            &crispawn_core::types::hex(&io::new_id()?)[..8]
        ));
        diag.begin_persistent(&target, config.log_sync_interval_ms)?;
        diag.record(
            "continuous_logging",
            json!({"enabled":true,"sync_interval_ms":config.log_sync_interval_ms}),
        );
    }
    crate::io_trace::destination(&mount.device.number);
    diag.record(
        "destination_writeback_limit",
        crate::io_trace::limit_destination_writeback(&mount.device.number),
    );
    diag.probe_interval_ms
        .store(u64::from(config.probe_interval_ms), Ordering::Relaxed);
    diag.status
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .can_restore = config.restore.is_some();
    let available = metric("/proc/meminfo", "MemAvailable:").saturating_mul(1024);
    config.tuning.plan.memory_limit_bytes = crate::profiles::live_memory_budget(
        config.tuning.plan.memory_limit_bytes,
        available,
        config.version == 2 && config.tuning.mode == crate::job_config::TuningMode::Auto,
    );
    diag.status
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .memory
        .get_or_insert_with(Default::default)
        .processing_limit_bytes = config.tuning.plan.memory_limit_bytes;
    diag.record("memory_budget", json!({"available_bytes":available.to_string(),
        "processing_limit_bytes":config.tuning.plan.memory_limit_bytes.to_string(),
        "os_headroom_bytes":available.saturating_sub(config.tuning.plan.memory_limit_bytes).to_string()}));
    diag.status
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .backup_verification_enabled = config.tuning.verify_after_write;
    diag.record(
        "device_discovery",
        json!({"devices":devices,"excluded_whole_disk":mount.device.whole_name}),
    );
    diag.record("runtime_configured", json!({"runtime_version":config.version,"startup":config.startup,"ui_mode":config.interface,"log_policy":config.log_policy,"volume":config.volume,
        "io_policy":config.tuning.io_policy,"verify_after_write":config.tuning.verify_after_write,
        "failure":{"poweroff":config.poweroff_on_failure,"delay_seconds":config.completion_seconds}}));
    let mut frontends = start_frontends(config.interface, diag)?;
    let mut network = crate::connection::Local;
    diag.record("network_configured", json!({"mode":config.network.mode()}));
    let result = (|| -> Result<()> {
        session_runner::execute(
            root,
            diag,
            &mount,
            &config,
            &devices,
            &mut frontends,
            &mut network,
        )?;
        diag.check()?;
        diag.phase(
            "COMPLETE",
            "backup verified; preparing a normal guest shutdown",
        );
        // Finalization can finish between periodic probes. Retain the verified
        // completion boundary independently of the UI countdown's duration.
        let _ = serial.send(format!(
            "PB_PROBE {}",
            serde_json::to_string(&diag.snapshot().probe_projection()).unwrap()
        ));
        diag.phase("SAVING_LOGS", "persisting bounded RAM diagnostics on NTFS");
        diag.record("finalization",json!({"engine_verified":true,"normal_poweroff_planned":config.poweroff_on_success,"unmount_requested":true}));
        let complete_log = if config.log_policy == LogPolicy::Always {
            Some(export(diag, &mount, "complete")?)
        } else {
            None
        };
        mount.sync()?;
        diag.phase(
            "UNMOUNTING_NTFS",
            "all backup, report and requested log writes are synced",
        );
        diag.close_persistent()?;
        crate::io_trace::stop();
        if let Some(target) = complete_log {
            if let Err(error) = crate::io_trace::export_final(&target) {
                diag.record(
                    "io_observer_final_export_failed",
                    json!({"error":error.to_string()}),
                );
            }
        }
        mount.unmount()?;
        if let Some(boot) = &boot_mount {
            boot.unmount()?;
        }
        diag.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ntfs_mounted = false;
        diag.phase(
            "UNMOUNTED",
            "NTFS data and boot volumes were cleanly unmounted",
        );
        let _ = serial.try_send(format!(
            "PB_PROBE {}",
            serde_json::to_string(&diag.snapshot().probe_projection()).unwrap()
        ));
        {
            let mut s = diag.status.lock().unwrap_or_else(|e| e.into_inner());
            s.finalization_complete = true;
            s.session.page = crispawn_core::presentation::Page::Complete;
        }
        if config.poweroff_on_success {
            {
                let mut s = diag.status.lock().unwrap_or_else(|e| e.into_inner());
                s.can_pause = false;
                s.can_resume = false;
                s.can_cancel = false;
            }
            diag.phase(
                "SHUTDOWN_COUNTDOWN",
                "all processing and filesystem finalization complete",
            );
            let until = Instant::now() + Duration::from_secs(u64::from(config.completion_seconds));
            while Instant::now() < until {
                diag.status
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .shutdown_countdown_seconds = Some(
                    until
                        .saturating_duration_since(Instant::now())
                        .as_millis()
                        .div_ceil(1000) as u32,
                );
                std::thread::sleep(Duration::from_millis(50));
            }
            frontends.stop();
            let _ = fs::remove_file(root.join("ui.json"));
            guard()?;
            diag.status
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .poweroff_requested = true;
            diag.phase(
                "POWEROFF_REQUESTED",
                "requesting BusyBox init's normal shutdown",
            );
            let _ = serial.try_send(format!(
                "PB_PROBE {}",
                serde_json::to_string(&diag.snapshot().probe_projection()).unwrap()
            ));
            std::thread::sleep(Duration::from_millis(500));
            require(
                Command::new("/sbin/poweroff").status()?.success(),
                "POWEROFF_FAILED",
                "init rejected normal poweroff",
            )?;
            // Init normally terminates this service. Do not turn a request into a false completion.
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        if config.version == 2 && !config.poweroff_on_success {
            diag.phase(
                "COMPLETE",
                "all processing complete; waiting without shutting down",
            );
            crate::supervisor::phase("MENU");
            loop {
                // Completed media are already unmounted. Keep the presentation
                // alive without accepting commands that could restart a job.
                diag.check()?;
                if let Some(child) = &mut frontends.gui {
                    require(
                        child.try_wait()?.is_none(),
                        "GUI_EXITED",
                        "GUI exited after completion",
                    )?;
                }
                if let Some(child) = &mut frontends.console {
                    require(
                        child.try_wait()?.is_none(),
                        "CONSOLE_EXITED",
                        "console exited after completion",
                    )?;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        Ok(())
    })();
    if result.is_err() {
        diag.record(
            "finalization_error",
            json!({"error":result.as_ref().err().unwrap().to_string()}),
        );
    }
    if result.is_ok() {
        frontends.stop();
    }
    result
}
