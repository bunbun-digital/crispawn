// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
use std::{error::Error, fs::File, io::Read, path::Path};
pub const DIRECTORY: &str = "/usr/share/crispawn/ui";
pub fn rgba(path: &Path) -> Result<(u32, u32, Vec<u8>), Box<dyn Error>> {
    let mut file = File::open(path)?;
    let mut header = [0; 16];
    file.read_exact(&mut header)?;
    if &header[..8] != b"CRISRGBA" {
        return Err("invalid UI image".into());
    }
    let w = u32::from_le_bytes(header[8..12].try_into()?);
    let h = u32::from_le_bytes(header[12..16].try_into()?);
    if w == 0 || h == 0 || w > 4096 || h > 4096 {
        return Err("UI image is too large".into());
    }
    let length = w as usize * h as usize * 4;
    if file.metadata()?.len() != length as u64 + 16 {
        return Err("invalid UI image length".into());
    }
    let mut data = vec![0; length];
    file.read_exact(&mut data)?;
    Ok((w, h, data))
}
