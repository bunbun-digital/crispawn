// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! Explicit whole-device adapter. Discovery is read-only; no partition edits.
use crate::{Error, Result};
use crispawn_core::{error::require, types::BlockIdentity};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub name: String,
    pub path: PathBuf,
    pub number: String,
    pub whole_name: String,
    pub partition: bool,
    pub usb: bool,
    pub removable: bool,
    pub size_bytes: u64,
    pub identity: Option<BlockIdentity>,
}
fn small(path: &Path) -> Option<String> {
    let s = fs::read_to_string(path).ok()?;
    if s.len() > 1024 {
        return None;
    }
    let s = s.trim();
    if s.is_empty() || s.chars().any(char::is_control) {
        return None;
    }
    Some(s.into())
}
fn stable(path: &Path) -> Option<String> {
    for name in ["wwid", "device/wwid"] {
        if let Some(v) = small(&path.join(name)) {
            return Some(format!("wwid:{v}"));
        }
    }
    for name in ["serial", "device/serial"] {
        if let Some(v) = small(&path.join(name)) {
            return Some(format!(
                "serial:{v}:ns:{}",
                small(&path.join("nsid")).unwrap_or_default()
            ));
        }
    }
    let mut p = path.parent();
    for _ in 0..3 {
        if let Some(parent) = p {
            if let Some(v) = small(&parent.join("serial")) {
                return Some(format!(
                    "serial:{v}:ns:{}",
                    small(&path.join("nsid")).unwrap_or_default()
                ));
            }
            p = parent.parent();
        }
    }
    None
}
pub fn scan(sys: &Path, dev: &Path) -> Result<Vec<Device>> {
    let mut devices = vec![];
    for entry in fs::read_dir(sys.join("class/block"))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let p = entry.path().canonicalize()?;
        let partition = p.join("partition").exists();
        let whole_name = if partition {
            p.parent()
                .and_then(Path::file_name)
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        } else {
            name.clone()
        };
        let number = small(&p.join("dev"))
            .ok_or_else(|| Error::new("INVALID_SYSFS", "missing block device number"))?;
        let sectors: u64 = small(&p.join("size"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let size_bytes = sectors
            .checked_mul(512)
            .ok_or_else(|| Error::new("INVALID_SYSFS", "block size overflow"))?;
        let usb = p
            .components()
            .any(|c| c.as_os_str().to_string_lossy().starts_with("usb"));
        let removable = small(&p.join("removable")).as_deref() == Some("1");
        let logical = small(&p.join("queue/logical_block_size"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(512);
        let physical = small(&p.join("queue/physical_block_size"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(logical);
        let identity = stable(&p).map(|stable_id| BlockIdentity {
            stable_id,
            logical_sector_bytes: logical,
            physical_sector_bytes: physical,
        });
        devices.push(Device {
            name: name.clone(),
            path: dev.join(name),
            number,
            whole_name,
            partition,
            usb,
            removable,
            size_bytes,
            identity,
        });
    }
    devices.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(devices)
}
pub fn select_source<'a>(
    devices: &'a [Device],
    excluded_whole: &str,
    stable_id: Option<&str>,
) -> Result<&'a Device> {
    let candidates: Vec<_> = candidates(devices, excluded_whole)
        .into_iter()
        .filter(|d| {
            stable_id
                .map(|id| d.identity.as_ref().unwrap().stable_id == id)
                .unwrap_or(true)
        })
        .collect();
    require(
        candidates.len() == 1,
        "AMBIGUOUS_SOURCE",
        format!("expected one internal source, found {}", candidates.len()),
    )?;
    Ok(candidates[0])
}
/// Read-only inventory, shared by single-source compatibility and session UI.
pub fn candidates<'a>(devices: &'a [Device], excluded_whole: &str) -> Vec<&'a Device> {
    devices
        .iter()
        .filter(|d| {
            !d.partition
                && !d.usb
                && !d.removable
                && d.name != excluded_whole
                && d.size_bytes > 0
                && d.identity.is_some()
                && !["loop", "ram", "zram", "dm-", "md", "sr", "fd"]
                    .iter()
                    .any(|p| d.name.starts_with(p))
        })
        .collect()
}
pub fn require_unused(
    source: &Device,
    devices: &[Device],
    sys: &Path,
    mountinfo: &str,
    swaps: &str,
) -> Result<()> {
    for d in devices.iter().filter(|d| d.whole_name == source.name) {
        require(
            !mountinfo
                .lines()
                .any(|l| l.split_whitespace().nth(2) == Some(d.number.as_str())),
            "SOURCE_IN_USE",
            "source or source partition is mounted",
        )?;
        if let Ok(mut entries) = fs::read_dir(sys.join("class/block").join(&d.name).join("holders"))
        {
            require(
                entries.next().is_none(),
                "SOURCE_IN_USE",
                "source has a device-mapper/RAID holder",
            )?;
        }
        require(
            !swaps
                .lines()
                .skip(1)
                .any(|l| l.split_whitespace().next() == d.path.to_str()),
            "SOURCE_IN_USE",
            "source is active swap",
        )?;
    }
    Ok(())
}
pub fn open(path: &Path, write: bool, exclusive: bool) -> Result<File> {
    let f = OpenOptions::new()
        .read(true)
        .write(write)
        .custom_flags(
            libc::O_NOFOLLOW | libc::O_NONBLOCK | if exclusive { libc::O_EXCL } else { 0 },
        )
        .open(path)?;
    require(
        f.metadata()?.file_type().is_block_device(),
        "BLOCK_DEVICE_REQUIRED",
        "explicit whole block device required",
    )?;
    let m = f.metadata()?;
    let sys = PathBuf::from(format!(
        "/sys/dev/block/{}:{}",
        libc::major(m.rdev()),
        libc::minor(m.rdev())
    ));
    require(
        !sys.join("partition").exists(),
        "WHOLE_DISK_REQUIRED",
        "partition sources are not accepted",
    )?;
    Ok(f)
}
pub fn identity(file: &File) -> Result<crispawn_core::types::SourceIdentity> {
    let m = file.metadata()?;
    let p = PathBuf::from(format!(
        "/sys/dev/block/{}:{}",
        libc::major(m.rdev()),
        libc::minor(m.rdev())
    ))
    .canonicalize()?;
    let mut bytes = 0u64;
    let mut logical = 0u32;
    let mut physical = 0u32;
    // SAFETY: owned block FD, kernel writes values of the specified ioctl widths.
    unsafe {
        if libc::ioctl(file.as_raw_fd(), 0x80081272u32 as _, &mut bytes) != 0
            || libc::ioctl(file.as_raw_fd(), 0x1268u32 as _, &mut logical) != 0
            || libc::ioctl(file.as_raw_fd(), 0x127bu32 as _, &mut physical) != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    require(
        bytes > 0
            && [512, 4096].contains(&logical)
            && physical >= logical
            && physical.is_power_of_two(),
        "UNSUPPORTED_DEVICE",
        "unsupported disk size or sector geometry",
    )?;
    let stable_id = stable(&p)
        .ok_or_else(|| Error::new("DEVICE_ID_UNAVAILABLE", "source has no stable serial/WWID"))?;
    Ok(crispawn_core::types::SourceIdentity {
        device: 0,
        inode: 0,
        bytes,
        modified_seconds: "0".into(),
        modified_nanos: "0".into(),
        changed_seconds: "0".into(),
        changed_nanos: "0".into(),
        block_device: Some(BlockIdentity {
            stable_id,
            logical_sector_bytes: logical,
            physical_sector_bytes: physical,
        }),
    })
}
pub fn bytes(file: &File) -> Result<u64> {
    if !file.metadata()?.file_type().is_block_device() {
        return Ok(file.metadata()?.len());
    }
    let mut bytes = 0u64;
    // SAFETY: BLKGETSIZE64 writes exactly one u64 to valid storage.
    if unsafe { libc::ioctl(file.as_raw_fd(), 0x80081272u32 as _, &mut bytes) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(bytes)
}
