// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! Optional terminal view over the exact same RAM status and typed controls as GUI.
use crate::{diagnostics, io, terminal, Result};
use crispawn_core::{
    live::{Command, Status},
    locale::Text as T,
    presentation::{self, Interaction, Key, Page},
};
use std::{
    io::{Read, Write},
    path::Path,
    time::{Duration, Instant},
};

pub fn render(s: &Status, input: &Interaction, logs: &str) -> Result<String> {
    let l = s.language;
    let v = &s.session;
    let mut text = String::from("\x1b[2J\x1b[HCRISPAWN\n[B] ");
    text.push_str(l.text(T::Copy));
    text.push_str("  [R] ");
    text.push_str(l.text(T::Restore));
    text.push('\n');
    if input.cancel_open {
        text.push_str(&format!(
            "\n{}\n[Y] {}  [Esc] {}\n",
            l.text(T::AbortTitle),
            l.text(T::Abort),
            l.text(T::Continue)
        ));
        return Ok(text);
    }
    if input.logs_open {
        text.push_str(l.text(T::LogsTitle));
        text.push('\n');
        text.push_str(logs);
        text.push_str("\n[I / Esc] ");
        text.push_str(l.text(T::LogsBack));
        return Ok(text);
    }
    let first = input.list_page * 3;
    if s.phase == "FAILED" || s.phase == "CANCELLED" {
        text.push_str(l.text(T::Failure));
        text.push('\n');
        text.push_str(&l.error_detail(&s.detail));
        text.push('\n');
        text.push_str(&if let Some(seconds) = s.shutdown_countdown_seconds {
            l.message(T::Shutdown, &[("seconds", &seconds.to_string())])?
        } else {
            l.text(T::FailureStopped).into()
        });
        text.push_str(&format!("\n[I] {}\n", l.text(T::Details)));
        return Ok(text);
    }
    match v.page {
        Page::CopySelection => {
            text.push_str(l.text(T::ChangeSources));
            text.push('\n');
            for (i, d) in v.disks.iter().enumerate().skip(first).take(3) {
                text.push_str(&format!(
                    "[{}] {} {} · {} ({})\n",
                    i - first + 1,
                    if d.selected { "[x]" } else { "[ ]" },
                    d.name,
                    presentation::capacity(d.bytes),
                    presentation::exact(d.bytes)
                ));
            }
            text.push_str(&format!(
                "\n{}\n[S] {}\n",
                l.text(if v.prepared { T::Ready } else { T::Preparing }),
                l.text(T::Start)
            ));
        }
        Page::RestoreSelection => {
            text.push_str(l.text(T::RestoreSource));
            text.push('\n');
            for (i, b) in v.backups.iter().enumerate().skip(first).take(3) {
                text.push_str(&format!(
                    "[{}] {} {}\n",
                    i - first + 1,
                    if v.selected_backup == Some(i) {
                        "(*)"
                    } else {
                        "( )"
                    },
                    b.datetime
                ));
            }
            text.push_str(&format!("[N] {}\n", l.text(T::Next)));
        }
        Page::RestoreMapping => {
            text.push_str(l.text(T::SourceDisk));
            text.push('\n');
            if let Some(b) = v.selected_backup.and_then(|i| v.backups.get(i)) {
                for (i, d) in b.disks.iter().enumerate().skip(first).take(3) {
                    text.push_str(&format!(
                        "[{}] {} {}\n",
                        i - first + 1,
                        if v.selected_saved_disk == Some(i) {
                            "(*)"
                        } else {
                            "( )"
                        },
                        d.name
                    ));
                }
            }
            text.push_str(l.text(T::TargetDisk));
            text.push('\n');
            for (i, d) in v.disks.iter().enumerate().skip(first).take(3) {
                text.push_str(&format!(
                    "[{}] {} {} · {}\n",
                    ['A', 'D', 'F'][i - first],
                    if v.selected_target == Some(i) {
                        "(*)"
                    } else {
                        "( )"
                    },
                    d.name,
                    presentation::capacity(d.bytes)
                ));
            }
            text.push_str(&format!("[N] {}  [Esc]\n", l.text(T::Next)));
        }
        Page::RestoreConfirmation => {
            text.push_str(l.text(T::OverwriteWarning));
            text.push('\n');
            if let Some(d) = v.selected_target.and_then(|i| v.disks.get(i)) {
                text.push_str(&d.name);
                text.push('\n');
            }
            text.push_str(&format!("[Y] {}  [Esc]\n", l.text(T::RestoreStart)));
        }
        _ => {
            text.push_str(&format!("{}\n", s.phase));
            if let Some(storage) = &s.storage {
                text.push_str(&format!(
                    "{} {:02}:{:02}:{:02}\n",
                    l.text(T::Elapsed),
                    storage.elapsed_seconds / 3600,
                    storage.elapsed_seconds / 60 % 60,
                    storage.elapsed_seconds % 60
                ));
                text.push_str(&l.storage_summary(storage));
                text.push('\n');
            }
            if let Some(p) = &s.snapshot {
                for (key, n) in [
                    (T::Read, p.read_raw_bytes),
                    (
                        T::Durable,
                        if v.mode == crispawn_core::presentation::Mode::Backup {
                            s.storage
                                .as_ref()
                                .map(|v| v.written_source_bytes)
                                .unwrap_or(p.durable_raw_bytes)
                        } else {
                            p.durable_raw_bytes
                        },
                    ),
                    (
                        T::Confirmed,
                        if v.mode == crispawn_core::presentation::Mode::Backup {
                            s.storage
                                .as_ref()
                                .map(|v| v.confirmed_source_bytes)
                                .unwrap_or(p.verified_raw_bytes)
                        } else {
                            p.verified_raw_bytes
                        },
                    ),
                ] {
                    text.push_str(&format!(
                        "{}: {} ({}) / {} ({})\n",
                        l.text(key),
                        presentation::capacity(n),
                        presentation::exact(n),
                        presentation::capacity(p.total_bytes),
                        presentation::exact(p.total_bytes)
                    ));
                }
            }
            if let Some(p) = &s.selected_plan {
                text.push_str(&format!(
                    "{}: {} · {} {} · {}: {} · {}: {}\n",
                    l.text(T::ReadBlock),
                    presentation::exact(p.read_bytes),
                    p.codec.name(),
                    p.compression_level,
                    l.text(T::Workers),
                    p.workers,
                    l.text(T::VerifyWorkers),
                    s.verify_workers
                ));
            }
            if let Some(seconds) = s.shutdown_countdown_seconds {
                text.push_str(&l.message(T::Shutdown, &[("seconds", &seconds.to_string())])?);
                text.push('\n');
            }
            text.push_str("[P] ");
            text.push_str(l.text(T::Pause));
            text.push_str("  [C] ");
            text.push_str(l.text(T::Cancel));
            text.push('\n');
        }
    }
    if let Some(e) = &v.preparation_error {
        text.push_str(&l.error_detail(e));
        text.push('\n');
    }
    text.push_str(&format!(
        "\n{}: {} · {}: NTFS\n[I] {} · PgUp / PgDn\n",
        l.text(T::Destination),
        l.text(T::UsbDisk),
        l.text(T::Filesystem),
        l.text(T::Details)
    ));
    Ok(text)
}
pub fn run(root: &Path) -> Result<()> {
    let _terminal = terminal::Terminal::open(true)?;
    let mut keys = terminal::Keys::default();
    let mut input = Interaction::default();
    let mut s = Status::new(String::new());
    let mut last = Instant::now() - Duration::from_secs(1);
    while !root.join("ui-stop").exists() {
        if last.elapsed() >= Duration::from_millis(250) {
            if let Ok(file) = std::fs::File::open(root.join("status.json")) {
                let mut b = Vec::new();
                file.take((2 << 20) + 1).read_to_end(&mut b)?;
                if b.len() <= 2 << 20 {
                    if let Ok(value) = serde_json::from_slice(&b) {
                        s = value;
                    }
                }
            }
            input.synchronize(&s);
            let logs = if input.logs_open {
                let tail = diagnostics::tail(&root.join("logs"), 65536)?;
                let lines = tail.lines().collect::<Vec<_>>();
                let end = lines
                    .len()
                    .saturating_sub(input.log_page.saturating_mul(18));
                let begin = end.saturating_sub(18);
                lines[begin..end].join("\n")
            } else {
                String::new()
            };
            print!("{}", render(&s, &input, &logs)?);
            std::io::stdout().flush()?;
            last = Instant::now();
        }
        let key = match keys.read()? {
            terminal::Input::Key(c) => Key::Character(c),
            terminal::Input::Command(key) => key,
            terminal::Input::End => break,
            terminal::Input::Timeout => continue,
        };
        if let Some(action) = input.key(key, &s) {
            let id = crispawn_core::types::hex(&io::new_id()?);
            io::atomic_write(
                &root.join("commands"),
                &format!("cmd-{id}.json"),
                &io::json(&Command {
                    schema_version: 1,
                    id,
                    action,
                })?,
            )?;
        }
        last = Instant::now() - Duration::from_secs(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_console_shows_japanese_reason_and_the_configured_countdown_or_stopped_state() {
        let mut s = Status::new("test".into());
        s.phase = "FAILED".into();
        s.detail = "DESTINATION_LOW_SPACE: raw English detail".into();
        s.shutdown_countdown_seconds = Some(7);
        let text = render(&s, &Interaction::default(), "").unwrap();
        assert!(text.contains("空き容量が1GB"));
        assert!(text.contains("7秒"));
        assert!(!text.contains("raw English"));
        s.shutdown_countdown_seconds = None;
        assert!(render(&s, &Interaction::default(), "")
            .unwrap()
            .contains("自動では切れません"));
    }
}
