// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
use crate::{
    error::require,
    types::{hash, hex, Codec, RangeEntry, VERSION},
    Result,
};
pub const FILE_HEADER: usize = 96;
pub const RECORD_HEADER: usize = 160;
pub const FOOTER: usize = 96;
pub const INDEX_ROW: usize = 168;
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
fn check(b: &[u8], len: usize, magic: &[u8; 8]) -> Result<()> {
    require(b.len() == len, "INVALID_HEADER", "header length")?;
    require(
        &b[..8] == magic && get32(b, 8) == VERSION && get32(b, 12) == len as u32,
        "INVALID_HEADER",
        "magic/version/length",
    )?;
    require(
        hash(&b[..len - 32]) == b[len - 32..],
        "HEADER_HASH_MISMATCH",
        "header checksum",
    )
}
pub fn file_header(id: [u8; 16], file_id: u64) -> [u8; FILE_HEADER] {
    let mut b = [0; FILE_HEADER];
    b[..8].copy_from_slice(b"PBKFILE1");
    put32(&mut b, 8, VERSION);
    put32(&mut b, 12, FILE_HEADER as u32);
    b[16..32].copy_from_slice(&id);
    put64(&mut b, 32, file_id);
    let h = hash(&b[..64]);
    b[64..].copy_from_slice(&h);
    b
}
pub fn check_file_header(b: &[u8], id: [u8; 16], file_id: u64) -> Result<()> {
    check(b, FILE_HEADER, b"PBKFILE1")?;
    require(
        b == file_header(id, file_id),
        "FILE_ID_MISMATCH",
        "file header identity/reserved fields",
    )
}
pub fn footer(id: [u8; 16], file_id: u64, count: u64, raw: u64, end: u64) -> [u8; FOOTER] {
    let mut b = [0; FOOTER];
    b[..8].copy_from_slice(b"PBKEND01");
    put32(&mut b, 8, VERSION);
    put32(&mut b, 12, FOOTER as u32);
    b[16..32].copy_from_slice(&id);
    put64(&mut b, 32, file_id);
    put64(&mut b, 40, count);
    put64(&mut b, 48, raw);
    put64(&mut b, 56, end);
    let h = hash(&b[..64]);
    b[64..].copy_from_slice(&h);
    b
}
pub fn check_footer(
    b: &[u8],
    id: [u8; 16],
    file_id: u64,
    count: u64,
    raw: u64,
    end: u64,
) -> Result<()> {
    check(b, FOOTER, b"PBKEND01")?;
    require(
        b == footer(id, file_id, count, raw, end),
        "FOOTER_MISMATCH",
        "footer totals/identity",
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub backup_id: [u8; 16],
    pub file_id: u64,
    pub range_id: u64,
    pub source_offset: u64,
    pub raw_bytes: u64,
    pub payload_bytes: u64,
    pub codec: Codec,
    pub level: i32,
    pub raw_hash: [u8; 32],
}
impl Record {
    pub fn encode(&self) -> [u8; RECORD_HEADER] {
        let mut b = [0; RECORD_HEADER];
        b[..8].copy_from_slice(b"PBKREC01");
        put32(&mut b, 8, VERSION);
        put32(&mut b, 12, RECORD_HEADER as u32);
        b[16..32].copy_from_slice(&self.backup_id);
        for (p, n) in [
            (32, self.file_id),
            (40, self.range_id),
            (48, self.source_offset),
            (56, self.raw_bytes),
            (64, self.payload_bytes),
        ] {
            put64(&mut b, p, n);
        }
        put32(&mut b, 72, self.codec.id());
        b[76..80].copy_from_slice(&self.level.to_le_bytes());
        b[80..112].copy_from_slice(&self.raw_hash);
        let h = hash(&b[..128]);
        b[128..].copy_from_slice(&h);
        b
    }
    pub fn decode(b: &[u8]) -> Result<Self> {
        check(b, RECORD_HEADER, b"PBKREC01")?;
        require(
            b[112..128].iter().all(|v| *v == 0),
            "UNKNOWN_FLAGS",
            "record reserved bytes",
        )?;
        Ok(Self {
            backup_id: b[16..32].try_into().unwrap(),
            file_id: get64(b, 32),
            range_id: get64(b, 40),
            source_offset: get64(b, 48),
            raw_bytes: get64(b, 56),
            payload_bytes: get64(b, 64),
            codec: Codec::from_id(get32(b, 72))?,
            level: i32::from_le_bytes(b[76..80].try_into().unwrap()),
            raw_hash: b[80..112].try_into().unwrap(),
        })
    }
    pub fn entry(&self, offset: u64) -> RangeEntry {
        RangeEntry {
            range_id: self.range_id,
            source_offset_bytes: self.source_offset,
            source_length_bytes: self.raw_bytes,
            raw_sha256: hex(&self.raw_hash),
            file_id: self.file_id,
            record_offset_bytes: offset,
            record_length_bytes: RECORD_HEADER as u64 + self.payload_bytes,
            codec: self.codec,
            codec_params_id: self.codec.params(self.level),
        }
    }
    pub fn validate(
        &self,
        id: [u8; 16],
        file_id: u64,
        plan: &crate::plan::Plan,
        source_bytes: u64,
    ) -> Result<()> {
        let (offset, len) = plan.range(source_bytes, self.range_id)?;
        require(
            self.backup_id == id
                && self.file_id == file_id
                && self.source_offset == offset
                && self.raw_bytes == len,
            "RECORD_MISMATCH",
            "identity or source range",
        )?;
        let payload_ok = match self.codec {
            Codec::Raw => self.payload_bytes == len && self.level == 0,
            Codec::Zero => self.payload_bytes == 0 && self.level == 0,
            _ => {
                self.codec == plan.codec
                    && self.level == plan.compression_level
                    && self.payload_bytes > 0
                    && self.payload_bytes < len
            }
        };
        require(
            payload_ok,
            "INVALID_RECORD",
            "codec parameters or payload length",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detects_header_damage_and_reserved_fields() {
        let id = [1; 16];
        let mut b = file_header(id, 2);
        assert!(check_file_header(&b, id, 2).is_ok());
        b[42] = 1;
        assert!(check_file_header(&b, id, 2).is_err());
        let r = Record {
            backup_id: id,
            file_id: 0,
            range_id: 0,
            source_offset: 0,
            raw_bytes: 512,
            payload_bytes: 512,
            codec: Codec::Raw,
            level: 0,
            raw_hash: hash(&[0; 512]),
        };
        assert_eq!(Record::decode(&r.encode()).unwrap(), r);
        assert!(Record::decode(&r.encode()[..159]).is_err());
    }
    #[test]
    fn fixed_wire_bytes_match_independent_python_fixture() {
        let id = std::array::from_fn(|i| i as u8);
        let r = Record {
            backup_id: id,
            file_id: 0,
            range_id: 0,
            source_offset: 0,
            raw_bytes: 3,
            payload_bytes: 3,
            codec: Codec::Raw,
            level: 0,
            raw_hash: hash(b"abc"),
        };
        let mut bytes = file_header(id, 0).to_vec();
        bytes.extend(r.encode());
        bytes.extend(b"abc");
        bytes.extend(footer(id, 0, 1, 3, 259));
        assert_eq!(
            hex(&bytes),
            include_str!("../tests/fixtures/v1-raw-abc.hex").trim()
        );
    }
}
