// SPDX-License-Identifier: Apache-2.0
//! Observe the current firmware boot entry; never changes EFI variables.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{fs::File, io::Read, path::Path};

const GLOBAL: &str = "8be4df61-93ca-11d2-aa0d-00e098032b8c";
fn read_variable(root: &Path, name: &str) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(root.join(format!("{name}-{GLOBAL}")))
        .ok()?
        .take(65541)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() >= 4 && bytes.len() <= 65540).then(|| bytes[4..].to_vec())
}
fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn load_option(bytes: &[u8]) -> Option<Value> {
    let path_bytes = usize::from(u16_at(bytes, 4)?);
    let mut at = 6;
    let mut description = Vec::new();
    loop {
        let ch = u16_at(bytes, at)?;
        at += 2;
        if ch == 0 {
            break;
        }
        if description.len() >= 128 {
            return None;
        }
        description.push(ch);
    }
    let end = at.checked_add(path_bytes)?;
    let path = bytes.get(at..end)?;
    let mut node = 0;
    let mut files = Vec::new();
    while node < path.len() {
        let header = path.get(node..node + 4)?;
        let length = usize::from(u16::from_le_bytes([header[2], header[3]]));
        if length < 4 {
            return None;
        }
        let data = path.get(node + 4..node.checked_add(length)?)?;
        if header[0] == 4 && header[1] == 4 {
            if data.len() % 2 != 0 {
                return None;
            }
            let text: Vec<_> = data
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .take_while(|ch| *ch != 0)
                .take(256)
                .collect();
            if files.len() < 4 {
                files.push(String::from_utf16_lossy(&text));
            }
        }
        node += length;
    }
    let optional = &bytes[end..];
    Some(json!({
        "description":String::from_utf16_lossy(&description),
        "file_paths":files,"device_path_bytes":path.len(),
        "optional_data_bytes":optional.len(),
        "optional_data_prefix_hex":crispawn_core::types::hex(&optional[..optional.len().min(32)]),
        "optional_data_sha256":crispawn_core::types::hex(&Sha256::digest(optional)),
    }))
}

pub fn observe(sys: &Path) -> Value {
    let root = sys.join("firmware/efi/efivars");
    let current = read_variable(&root, "BootCurrent").and_then(|b| u16_at(&b, 0));
    let Some(index) = current else {
        return json!({"available":false,"reason":"BootCurrent is not exposed by firmware"});
    };
    let name = format!("Boot{index:04X}");
    let entry = read_variable(&root, &name);
    let decoded = entry.as_deref().and_then(load_option);
    json!({"available":decoded.is_some(),"boot_current":name,"entry":decoded,
        "limitation":"Boot#### OptionalData is observed; shim's transient EFI_LOADED_IMAGE LoadOptions and FilePath are not available to Linux"})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn boot_entry_preserves_padded_fj_as_bytes_without_guessing_filename() {
        let mut path = vec![4, 4, 0, 0];
        for ch in "\\EFI\\BOOT\\BOOTX64.EFI\0".encode_utf16() {
            path.extend(ch.to_le_bytes());
        }
        let len = (path.len() as u16).to_le_bytes();
        path[2..4].copy_from_slice(&len);
        path.extend([0x7f, 0xff, 4, 0]);
        let mut entry = vec![1, 0, 0, 0];
        entry.extend((path.len() as u16).to_le_bytes());
        entry.extend([0, 0]);
        entry.extend(path);
        entry.extend(b"FJ\0\0");
        let value = load_option(&entry).unwrap();
        assert_eq!(value["file_paths"][0], "\\EFI\\BOOT\\BOOTX64.EFI");
        assert_eq!(value["optional_data_prefix_hex"], "464a0000");
    }
    #[test]
    fn malformed_firmware_entries_are_bounded_and_rejected() {
        assert!(load_option(&[0; 5]).is_none());
        let mut entry = vec![1, 0, 0, 0, 4, 0, 0, 0];
        entry.extend([4, 4, 0, 0]);
        assert!(load_option(&entry).is_none());
    }
}
