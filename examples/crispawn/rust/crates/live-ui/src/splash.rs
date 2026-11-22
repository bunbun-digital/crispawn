// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-digital. Licensed under Apache-2.0.
#![forbid(unsafe_code)]
use crispawn_ui::{Kind, Length::Percent, Selector::Main, Style, Ui, Widget};
use std::{error::Error, fs::File, io::Read, path::Path};

pub const LOGO: &str = "/usr/share/crispawn/logo.rgba";
pub struct Splash<'a> {
    root: Widget<'a>,
}
impl<'a> Splash<'a> {
    pub fn new(ui: &'a Ui, path: &Path) -> Result<Self, Box<dyn Error>> {
        // The build converts the preserved PNG to a bounded raw RGBA asset.
        // Initramfs XZ compresses it; no image decoder is added to the guest.
        let mut file = File::open(path)?;
        let mut header = [0; 16];
        file.read_exact(&mut header)?;
        if &header[..8] != b"CRISRGBA" {
            return Err("invalid splash image header".into());
        }
        let width = u32::from_le_bytes(header[8..12].try_into()?);
        let height = u32::from_le_bytes(header[12..16].try_into()?);
        if width == 0 || height == 0 || width > 4096 || height > 4096 {
            return Err("logo exceeds display image limits".into());
        }
        let length = width as usize * height as usize * 4;
        if file.metadata()?.len() != 16 + length as u64 {
            return Err("invalid splash payload size".into());
        }
        let mut rgba = vec![0; length];
        file.read_exact(&mut rgba)?;
        let screen = ui.root()?;
        screen.plain();
        screen.style(Style::Background(0xffffff), Main)?;
        screen.style(Style::Opacity(255), Main)?;
        let root = ui.child(screen, Kind::Container)?;
        root.plain();
        root.size(Percent(100), Percent(100))?;
        root.style(Style::Background(0xffffff), Main)?;
        root.style(Style::Opacity(255), Main)?;
        let image = ui.child(root, Kind::Image)?;
        image.image_rgba(width, height, &rgba)?;
        Ok(Self { root })
    }
    pub fn hide(&self) {
        self.root.hidden(true);
    }
}
