// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
use crate::{error::require, Error, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

pub const VERSION: u32 = 1;
pub fn hash(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub fn unhex<const N: usize>(s: &str) -> Result<[u8; N]> {
    require(
        s.len() == N * 2
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "INVALID_HEX",
        "expected canonical lowercase hex",
    )?;
    let mut out = [0; N];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
            .map_err(|_| Error::new("INVALID_HEX", "hex"))?;
    }
    Ok(out)
}
pub fn parse_decimal(s: &str) -> Result<u64> {
    require(
        !s.is_empty() && (s == "0" || !s.starts_with('0')) && s.bytes().all(|b| b.is_ascii_digit()),
        "INVALID_INTEGER",
        "expected canonical unsigned decimal",
    )?;
    s.parse()
        .map_err(|_| Error::new("INVALID_INTEGER", "u64 overflow"))
}
pub mod decimal {
    use super::*;
    pub fn serialize<S: Serializer>(v: &u64, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<u64, D::Error> {
        let s = String::deserialize(d)?;
        parse_decimal(&s).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    Raw,
    Zero,
    Zstd,
    Xz,
    Gzip,
}
impl Codec {
    pub fn id(self) -> u32 {
        match self {
            Self::Raw => 0,
            Self::Zero => 1,
            Self::Zstd => 2,
            Self::Xz => 3,
            Self::Gzip => 4,
        }
    }
    pub fn from_id(id: u32) -> Result<Self> {
        match id {
            0 => Ok(Self::Raw),
            1 => Ok(Self::Zero),
            2 => Ok(Self::Zstd),
            3 => Ok(Self::Xz),
            4 => Ok(Self::Gzip),
            _ => Err(Error::new("UNKNOWN_CODEC", "record codec")),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Zero => "zero",
            Self::Zstd => "zstd",
            Self::Xz => "xz",
            Self::Gzip => "gzip",
        }
    }
    pub fn params(self, level: i32) -> String {
        format!("{}-{level}-v1", self.name())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RangeEntry {
    #[serde(with = "decimal")]
    pub range_id: u64,
    #[serde(with = "decimal")]
    pub source_offset_bytes: u64,
    #[serde(with = "decimal")]
    pub source_length_bytes: u64,
    pub raw_sha256: String,
    #[serde(with = "decimal")]
    pub file_id: u64,
    #[serde(with = "decimal")]
    pub record_offset_bytes: u64,
    #[serde(with = "decimal")]
    pub record_length_bytes: u64,
    pub codec: Codec,
    pub codec_params_id: String,
}
pub const RANGE_COLUMNS: [&str; 9] = [
    "range_id",
    "source_offset_bytes",
    "source_length_bytes",
    "raw_sha256",
    "file_id",
    "record_offset_bytes",
    "record_length_bytes",
    "codec",
    "codec_params_id",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileEntry {
    #[serde(with = "decimal")]
    pub file_id: u64,
    pub relative_path: String,
    #[serde(with = "decimal")]
    pub stored_bytes: u64,
    pub stored_sha256: String,
    #[serde(with = "decimal")]
    pub raw_bytes_total: u64,
    #[serde(with = "decimal")]
    pub range_count: u64,
}
pub const FILE_COLUMNS: [&str; 6] = [
    "file_id",
    "relative_path",
    "stored_bytes",
    "stored_sha256",
    "raw_bytes_total",
    "range_count",
];
pub fn data_name(id: u64, partial: bool) -> String {
    format!(
        "data/data-{id:06}.pbk{}",
        if partial { ".part" } else { "" }
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockIdentity {
    pub stable_id: String,
    pub logical_sector_bytes: u32,
    pub physical_sector_bytes: u32,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceIdentity {
    #[serde(with = "decimal")]
    pub device: u64,
    #[serde(with = "decimal")]
    pub inode: u64,
    #[serde(with = "decimal")]
    pub bytes: u64,
    pub modified_seconds: String,
    pub modified_nanos: String,
    pub changed_seconds: String,
    pub changed_nanos: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_device: Option<BlockIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    pub format_version: u32,
    pub backup_id: String,
    pub source_kind: String,
    pub source: SourceIdentity,
    pub logical_sector_bytes: u32,
    pub physical_sector_bytes: u32,
    pub hash_algorithm: String,
    pub created_unix_seconds: String,
    pub application_version: String,
    pub source_consistency: String,
    pub boot_exclusion: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub schema_version: u32,
    pub backup_id: String,
    pub seq: u64,
    pub phase: String,
    pub code: String,
    #[serde(with = "decimal")]
    pub total_bytes: u64,
    /// Distinct completed source reads, excluding probe/verification/retry reads.
    #[serde(default, with = "decimal")]
    pub read_raw_bytes: u64,
    #[serde(with = "decimal")]
    pub durable_raw_bytes: u64,
    #[serde(with = "decimal")]
    pub verified_raw_bytes: u64,
    /// Bytes reread for the current verification pass. They become confirmed
    /// only after the complete backup file's expected digest matches.
    #[serde(default, with = "decimal", skip_serializing_if = "is_zero")]
    pub verification_read_raw_bytes: u64,
    #[serde(with = "decimal")]
    pub stored_bytes: u64,
    pub elapsed_seconds: f64,
    pub eta_seconds: Option<f64>,
    pub verification_kind: String,
}
pub trait EventSink: Send + Sync {
    fn emit(&self, snapshot: Snapshot);
    /// Actual completed source reads; queued bytes are encoded payload/storage,
    /// never durable or verified progress.
    fn prefetch(&self, _backup_id: &str, _read: u64, _queued: u64, _budget: u64) {}
    fn storage_progress(&self, _backup_id: &str, _status: &crate::live::StorageStatus) {}
    /// Productive-copy feedback; observers must never affect source coverage.
    fn settings(
        &self,
        _plan: &crate::plan::Plan,
        _verify_workers: u32,
        _total: u64,
        _reason: &str,
    ) {
    }
}
fn is_zero(value: &u64) -> bool {
    *value == 0
}
pub struct NoEvents;
impl EventSink for NoEvents {
    fn emit(&self, _: Snapshot) {}
}
