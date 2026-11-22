// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! V2 framing: each file covers one contiguous original span and has one BLAKE3.
use crate::Result;
use perfect_backup_core::{error::require, plan::Plan, types::Codec};

pub const FILE_HEADER: usize = 64;
pub const FRAME_HEADER: usize = 32;
pub const FOOTER: usize = 112;

fn put32(b: &mut [u8], p: usize, n: u32) {
    b[p..p + 4].copy_from_slice(&n.to_le_bytes());
}
fn put64(b: &mut [u8], p: usize, n: u64) {
    b[p..p + 8].copy_from_slice(&n.to_le_bytes());
}
fn get32(b: &[u8], p: usize) -> u32 {
    u32::from_le_bytes(b[p..p + 4].try_into().unwrap())
}
fn get64(b: &[u8], p: usize) -> u64 {
    u64::from_le_bytes(b[p..p + 8].try_into().unwrap())
}

pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320u32 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}
fn finish(b: &mut [u8]) {
    let end = b.len() - 4;
    let crc = crc32(&b[..end]);
    put32(b, end, crc);
}
fn check(b: &[u8], size: usize, magic: &[u8; 8]) -> Result<()> {
    require(b.len() == size, "INVALID_HEADER", "v2 header length")?;
    require(&b[..8] == magic, "INVALID_HEADER", "v2 magic")?;
    require(
        get32(b, size - 4) == crc32(&b[..size - 4]),
        "HEADER_CHECKSUM_MISMATCH",
        "v2 header CRC32",
    )
}
pub fn file_header(version: u32, id: [u8; 16], file: u64, offset: u64) -> [u8; FILE_HEADER] {
    let mut b = [0; FILE_HEADER];
    b[..8].copy_from_slice(if version == 3 {
        b"PBKFILE3"
    } else {
        b"PBKFILE2"
    });
    put32(&mut b, 8, version);
    put32(&mut b, 12, FILE_HEADER as u32);
    b[16..32].copy_from_slice(&id);
    put64(&mut b, 32, file);
    put64(&mut b, 40, offset);
    finish(&mut b);
    b
}
pub fn check_file_header(
    version: u32,
    b: &[u8],
    id: [u8; 16],
    file: u64,
    offset: u64,
) -> Result<()> {
    check(
        b,
        FILE_HEADER,
        if version == 3 {
            b"PBKFILE3"
        } else {
            b"PBKFILE2"
        },
    )?;
    require(
        b == file_header(version, id, file, offset),
        "FILE_ID_MISMATCH",
        "v2 file identity, source offset or reserved bytes",
    )
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub codec: Codec,
    pub level: i32,
    pub raw_bytes: u64,
    pub payload_bytes: u32,
}
impl Frame {
    pub fn encode(&self, version: u32) -> [u8; FRAME_HEADER] {
        let mut b = [0; FRAME_HEADER];
        b[..8].copy_from_slice(if version == 3 {
            b"PBKFRM03"
        } else {
            b"PBKFRM02"
        });
        put32(&mut b, 8, self.codec.id());
        put32(&mut b, 12, self.level as u32);
        put64(&mut b, 16, self.raw_bytes);
        put32(&mut b, 24, self.payload_bytes);
        finish(&mut b);
        b
    }
    pub fn decode(b: &[u8], plan: &Plan) -> Result<Self> {
        check(
            b,
            FRAME_HEADER,
            if plan.format_version == 3 {
                b"PBKFRM03"
            } else {
                b"PBKFRM02"
            },
        )?;
        let value = Self {
            codec: Codec::from_id(get32(b, 8))?,
            level: get32(b, 12) as i32,
            raw_bytes: get64(b, 16),
            payload_bytes: get32(b, 24),
        };
        require(
            value.raw_bytes > 0 && value.raw_bytes <= plan.range_bytes,
            "INVALID_RECORD",
            "v2 decoded frame size exceeds the bounded range",
        )?;
        let valid = match value.codec {
            Codec::Raw => value.level == 0 && u64::from(value.payload_bytes) == value.raw_bytes,
            Codec::Zero => value.level == 0 && value.payload_bytes == 0,
            codec => {
                (if plan.format_version == 3 {
                    allowed(codec, value.level)
                } else {
                    codec == plan.codec && value.level == plan.compression_level
                }) && value.payload_bytes > 0
                    && u64::from(value.payload_bytes) < value.raw_bytes
            }
        };
        require(valid, "INVALID_RECORD", "v2 codec, level or payload length")?;
        Ok(value)
    }
}

pub fn footer(
    version: u32,
    id: [u8; 16],
    file: u64,
    offset: u64,
    length: u64,
    frames: u64,
    hash: [u8; 32],
) -> [u8; FOOTER] {
    let mut b = [0; FOOTER];
    b[..8].copy_from_slice(if version == 3 {
        b"PBKEND03"
    } else {
        b"PBKEND02"
    });
    put32(&mut b, 8, version);
    put32(&mut b, 12, FOOTER as u32);
    b[16..32].copy_from_slice(&id);
    put64(&mut b, 32, file);
    put64(&mut b, 40, offset);
    put64(&mut b, 48, length);
    put64(&mut b, 56, frames);
    b[64..96].copy_from_slice(&hash);
    finish(&mut b);
    b
}
pub fn check_footer(
    version: u32,
    b: &[u8],
    id: [u8; 16],
    file: u64,
    offset: u64,
    length: u64,
    frames: u64,
    hash: [u8; 32],
) -> Result<()> {
    check(
        b,
        FOOTER,
        if version == 3 {
            b"PBKEND03"
        } else {
            b"PBKEND02"
        },
    )?;
    require(
        b == footer(version, id, file, offset, length, frames, hash),
        "FOOTER_MISMATCH",
        "v2 footer identity, coverage, frame count or BLAKE3",
    )
}

/// Closed, versioned codec/level vocabulary. Unknown combinations fail before decode.
pub fn allowed(codec: Codec, level: i32) -> bool {
    match codec {
        Codec::Raw | Codec::Zero => level == 0,
        Codec::Zstd => matches!(level, 1 | 3 | 6 | 9),
        Codec::Xz => matches!(level, 0 | 3),
        Codec::Gzip => matches!(level, 1 | 6 | 9),
    }
}
pub fn profile_name(codec: Codec, level: i32) -> String {
    let name = match codec {
        Codec::Raw => "raw",
        Codec::Zero => "zero",
        Codec::Zstd => "zstd",
        Codec::Xz => "xz",
        Codec::Gzip => "gzip",
    };
    format!("{name}:{level}")
}
pub fn valid_profiles(value: &str) -> bool {
    let mut previous = "";
    if value.is_empty() || value.len() > 256 {
        return false;
    }
    for item in value.split(';') {
        if item <= previous {
            return false;
        }
        let Some((name, level)) = item.split_once(':') else {
            return false;
        };
        let codec = match name {
            "raw" => Codec::Raw,
            "zero" => Codec::Zero,
            "zstd" => Codec::Zstd,
            "xz" => Codec::Xz,
            "gzip" => Codec::Gzip,
            _ => return false,
        };
        let Ok(level) = level.parse::<i32>() else {
            return false;
        };
        if !allowed(codec, level) || profile_name(codec, level) != item {
            return false;
        }
        previous = item;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crc_and_headers_reject_corruption_before_lengths_are_used() {
        assert_eq!(crc32(b"123456789"), 0xcbf43926);
        let plan = Plan {
            format_version: 2,
            ..Plan::default()
        };
        let frame = Frame {
            codec: Codec::Zero,
            level: 0,
            raw_bytes: 8192,
            payload_bytes: 0,
        };
        assert_eq!(
            Frame::decode(&frame.encode(2), &plan).unwrap().raw_bytes,
            8192
        );
        let mut bad = frame.encode(2);
        bad[20] ^= 1;
        assert!(Frame::decode(&bad, &plan).is_err());
        let mut header = file_header(2, [1; 16], 2, 1u64 << 40);
        assert!(check_file_header(2, &header, [1; 16], 2, 1u64 << 40).is_ok());
        header[48] = 1;
        finish(&mut header);
        assert!(check_file_header(2, &header, [1; 16], 2, 1u64 << 40).is_err());
    }
}
