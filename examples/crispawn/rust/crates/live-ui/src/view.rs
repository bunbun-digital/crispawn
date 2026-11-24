// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
#![forbid(unsafe_code)]
//! Approved 16:9 presentation. All choices and disk work belong to the controller.
use crispawn_core::{
    live::Status,
    locale::Text as T,
    presentation::{self as model, DiskState, Interaction, Language, Mode, Page},
};
use crispawn_ui::{
    Kind,
    Selector::{Disabled, Main},
    Style::*,
    Ui, Widget,
};
use std::{error::Error, path::Path};
type Result<T> = std::result::Result<T, Box<dyn Error>>;
const INK: u32 = 0x102744;
const MUTED: u32 = 0x53667a;
const BLUE: u32 = 0x008db8;
const GREEN: u32 = 0x147665;
const LINE: u32 = 0xdfe7ec;
const TRACK: u32 = 0xe8f0f3;
const DANGER: u32 = 0xa14343;
struct Button<'a> {
    object: Widget<'a>,
    caption: Widget<'a>,
}
impl Button<'_> {
    fn text(&self, text: &str, enabled: bool, primary: bool) -> Result<()> {
        self.caption.text(text)?;
        self.object.enabled(enabled);
        self.object
            .style(Background(if primary { INK } else { 0xffffff }), Main)?;
        self.object
            .style(Background(if primary { INK } else { 0xf4f6f8 }), Disabled)?;
        self.caption.style(
            Text(if primary {
                0xffffff
            } else if !enabled {
                MUTED
            } else {
                INK
            }),
            Main,
        )?;
        Ok(())
    }
}
struct Draw<'a> {
    ui: &'a Ui,
    scale: f64,
}
impl<'a> Draw<'a> {
    fn px(&self, n: u16) -> u16 {
        ((f64::from(n) * self.scale).round() as u16).max(u16::from(n > 0))
    }
    fn box_(
        &self,
        parent: Widget<'a>,
        x: u16,
        y: u16,
        w: u16,
        h: u16,
        color: Option<u32>,
    ) -> Result<Widget<'a>> {
        let o = self.ui.child(parent, Kind::Container)?;
        o.plain();
        o.bounds(self.px(x), self.px(y), self.px(w), self.px(h))?;
        if let Some(color) = color {
            o.style(Background(color), Main)?;
            o.style(Opacity(255), Main)?;
        }
        Ok(o)
    }
    fn label(
        &self,
        parent: Widget<'a>,
        x: u16,
        y: u16,
        w: u16,
        h: u16,
        size: u16,
        color: u32,
    ) -> Result<Widget<'a>> {
        let o = self.ui.child(parent, Kind::Label)?;
        o.bounds(self.px(x), self.px(y), self.px(w), self.px(h))?;
        o.font(&self.ui.font(u32::from(self.px(size).max(10)))?)?;
        o.style(Text(color), Main)?;
        Ok(o)
    }
    fn button(&self, parent: Widget<'a>, x: u16, y: u16, w: u16, h: u16) -> Result<Button<'a>> {
        let object = self.ui.child(parent, Kind::Button)?;
        object.plain();
        object.bounds(self.px(x), self.px(y), self.px(w), self.px(h))?;
        for s in [
            Radius(self.px(7)),
            Border(LINE),
            BorderWidth(1),
            Opacity(255),
        ] {
            object.style(s, Main)?;
        }
        object.style(Background(0xf4f6f8), Disabled)?;
        let caption = self.label(object, 0, 0, w, h, 14, INK)?;
        caption.content_height();
        caption.style(TextCenter, Main)?;
        caption.center();
        Ok(Button { object, caption })
    }
    fn bar(
        &self,
        parent: Widget<'a>,
        x: u16,
        y: u16,
        w: u16,
        h: u16,
        color: u32,
    ) -> Result<Widget<'a>> {
        let bar = self.ui.child(parent, Kind::Bar)?;
        bar.plain();
        bar.bounds(self.px(x), self.px(y), self.px(w), self.px(h))?;
        for s in [Background(TRACK), Opacity(255), Radius(self.px(5))] {
            bar.style(s, Main)?;
        }
        for s in [Background(color), Opacity(255), Radius(self.px(5))] {
            bar.style(s, crispawn_ui::Selector::Indicator)?;
        }
        bar.bar(1000, 0)?;
        Ok(bar)
    }
    fn image(
        &self,
        parent: Widget<'a>,
        x: u16,
        y: u16,
        w: u16,
        h: u16,
        path: &Path,
    ) -> Result<Widget<'a>> {
        let box_ = self.box_(parent, x, y, w, h, None)?;
        let image = self.ui.child(box_, Kind::Image)?;
        let (iw, ih, rgba) = crate::assets::rgba(path)?;
        image.image_rgba(iw, ih, &rgba)?;
        image.image_fit(self.px(w), self.px(h))?;
        Ok(image)
    }
}
struct Choice<'a> {
    background: Widget<'a>,
    name: Widget<'a>,
    meta: Widget<'a>,
    size: Widget<'a>,
}
pub struct View<'a> {
    root: Widget<'a>,
    scale: f64,
    crystals: Vec<Widget<'a>>,
    copy: Button<'a>,
    restore: Button<'a>,
    mode_hint: Widget<'a>,
    steps: Vec<Widget<'a>>,
    title: Widget<'a>,
    subtitle: Widget<'a>,
    progress: Widget<'a>,
    selection: Widget<'a>,
    finish: Widget<'a>,
    logs: Widget<'a>,
    percent: Widget<'a>,
    eta: Widget<'a>,
    elapsed: Widget<'a>,
    overall: Widget<'a>,
    total: Widget<'a>,
    overall_bar: Widget<'a>,
    read: Widget<'a>,
    saved: Widget<'a>,
    saved_exact: Widget<'a>,
    saved_bar: Widget<'a>,
    confirmed: Widget<'a>,
    confirmed_exact: Widget<'a>,
    confirmed_bar: Widget<'a>,
    queue: Widget<'a>,
    settings: Widget<'a>,
    settings_fields: Vec<Widget<'a>>,
    destination: Widget<'a>,
    tail: Widget<'a>,
    choices: Vec<Choice<'a>>,
    choice_heading: Widget<'a>,
    choice_target_heading: Widget<'a>,
    choice_hint: Widget<'a>,
    choice_summary: Widget<'a>,
    choice_side: Widget<'a>,
    choice_divider: Widget<'a>,
    finish_title: Widget<'a>,
    finish_body: Widget<'a>,
    log_summary: Widget<'a>,
    log_text: Widget<'a>,
    log_hint: Widget<'a>,
    primary: Button<'a>,
    pause: Button<'a>,
    cancel: Button<'a>,
    details: Button<'a>,
    overlay: Widget<'a>,
    dialog_title: Widget<'a>,
    dialog_body: Widget<'a>,
    keep: Button<'a>,
    stop: Button<'a>,
}
fn clipped(text: &str, n: usize) -> String {
    let mut chars = text.chars().map(|c| if c.is_control() { ' ' } else { c });
    let out: String = chars.by_ref().take(n).collect();
    if chars.next().is_some() {
        out + "…"
    } else {
        out
    }
}
fn ratio(n: u64, d: u64) -> i32 {
    if d == 0 {
        0
    } else {
        ((u128::from(n.min(d)) * 1000) / u128::from(d)) as i32
    }
}
fn percentage(n: u64, d: u64) -> u32 {
    ratio(n, d) as u32 / 10
}
impl<'a> View<'a> {
    pub fn animate(&self, milliseconds: u64) {
        for (n, frame) in self.crystals.iter().enumerate() {
            frame.hidden(n != (milliseconds / 180 % 10) as usize);
        }
    }
    pub fn with_assets(ui: &'a Ui, assets: &Path) -> Result<Self> {
        let (w, h) = ui.extent();
        if w < 960 || h < 540 {
            return Err("UI requires at least 960 by 540 pixels".into());
        }
        ui.command_input()?;
        let scale = (w as f64 / 1280.0).min(h as f64 / 720.0);
        let d = Draw { ui, scale };
        let screen = ui.root()?;
        screen.plain();
        screen.style(Background(0xffffff), Main)?;
        screen.style(Opacity(255), Main)?;
        ui.theme(BLUE, INK, &ui.font(u32::from(d.px(16)))?)?;
        let root = ui.child(screen, Kind::Container)?;
        root.plain();
        root.bounds(
            ((w - u32::from(d.px(1280))) / 2) as u16,
            ((h - u32::from(d.px(720))) / 2) as u16,
            d.px(1280),
            d.px(720),
        )?;
        root.style(Background(0xffffff), Main)?;
        root.style(Opacity(255), Main)?;
        let mut crystals = vec![];
        for n in 0..10 {
            let image = d.image(
                root,
                22,
                10,
                64,
                70,
                &assets.join(format!("crystal-{n:02}.rgba")),
            )?;
            image.hidden(n != 0);
            crystals.push(image);
        }
        d.image(root, 94, 22, 360, 48, &assets.join("wordmark.rgba"))?;
        let copy = d.button(root, 1006, 14, 125, 46)?;
        let restore = d.button(root, 1131, 14, 127, 46)?;
        let mode_hint = d.label(root, 826, 65, 432, 23, 12, MUTED)?;
        mode_hint.style(TextRight, Main)?;
        let mut steps = Vec::new();
        for n in 0..4 {
            steps.push(d.label(root, 22 + n * 309, 87, 309, 24, 14, MUTED)?);
        }
        d.box_(root, 22, 110, 1236, 1, Some(LINE))?;
        let title = d.label(root, 22, 120, 1236, 32, 24, INK)?;
        let subtitle = d.label(root, 22, 153, 1236, 23, 14, MUTED)?;
        let progress = d.box_(root, 22, 176, 1236, 400, None)?;
        let percent = d.label(progress, 0, 6, 340, 60, 40, INK)?;
        let overall = d.label(progress, 0, 0, 600, 20, 14, MUTED)?;
        let eta = d.label(progress, 668, 8, 300, 57, 19, INK)?;
        eta.style(TextRight, Main)?;
        let elapsed = d.label(progress, 360, 8, 280, 57, 19, INK)?;
        elapsed.style(TextRight, Main)?;
        let overall_bar = d.bar(progress, 0, 66, 968, 8, BLUE)?;
        let total = d.label(progress, 0, 81, 968, 46, 12, MUTED)?;
        let read = d.label(progress, 0, 123, 968, 23, 13, MUTED)?;
        let saved = d.label(progress, 0, 147, 968, 24, 14, INK)?;
        let saved_bar = d.bar(progress, 0, 176, 968, 5, BLUE)?;
        let saved_exact = d.label(progress, 0, 185, 968, 25, 12, INK)?;
        let confirmed = d.label(progress, 0, 219, 968, 24, 14, INK)?;
        let confirmed_bar = d.bar(progress, 0, 248, 968, 5, GREEN)?;
        let confirmed_exact = d.label(progress, 0, 257, 968, 25, 12, INK)?;
        let tail = d.label(progress, 0, 297, 968, 63, 12, INK)?;
        d.box_(progress, 990, 0, 1, 325, Some(LINE))?;
        let queue = d.label(progress, 1007, 4, 229, 111, 12, MUTED)?;
        let mut settings_fields = Vec::new();
        for n in 0..4 {
            settings_fields.push(d.label(
                progress,
                1007 + (n % 2) * 115,
                115 + (n / 2) * 46,
                114,
                45,
                12,
                INK,
            )?);
        }
        let settings = d.label(progress, 1007, 213, 229, 33, 12, MUTED)?;
        let destination = d.label(progress, 1007, 259, 229, 111, 13, INK)?;
        let selection = d.box_(root, 22, 177, 1236, 400, None)?;
        let choice_heading = d.label(selection, 0, 0, 968, 23, 14, INK)?;
        let choice_target_heading = d.label(selection, 622, 0, 614, 23, 14, INK)?;
        let choice_hint = d.label(selection, 0, 260, 1236, 50, 14, MUTED)?;
        let choice_summary = d.label(selection, 0, 189, 968, 57, 14, INK)?;
        let choice_side = d.label(selection, 1007, 0, 229, 245, 14, MUTED)?;
        let choice_divider = d.box_(selection, 990, 0, 1, 245, Some(LINE))?;
        let mut choices = vec![];
        for index in 0..6 {
            let column = index / 3;
            let index = index % 3;
            let x = if column == 0 { 0 } else { 622 };
            let b = d.box_(
                selection,
                x,
                25 + index as u16 * 54,
                if column == 0 { 968 } else { 614 },
                52,
                Some(0xf1f7f9),
            )?;
            let name = d.label(b, 8, 5, if column == 0 { 745 } else { 430 }, 24, 16, INK)?;
            let meta = d.label(b, 8, 30, if column == 0 { 745 } else { 430 }, 18, 12, MUTED)?;
            let size = d.label(
                b,
                if column == 0 { 756 } else { 435 },
                6,
                if column == 0 { 204 } else { 169 },
                43,
                12,
                MUTED,
            )?;
            size.style(TextRight, Main)?;
            choices.push(Choice {
                background: b,
                name,
                meta,
                size,
            });
        }
        let finish = d.box_(root, 22, 187, 1236, 370, None)?;
        let finish_title = d.label(finish, 0, 8, 1236, 56, 30, GREEN)?;
        let finish_body = d.label(finish, 0, 85, 1236, 200, 21, INK)?;
        let logs = d.box_(root, 22, 118, 1236, 535, None)?;
        let log_summary = d.label(logs, 0, 0, 1236, 91, 16, INK)?;
        let log_hint = d.label(logs, 0, 94, 1236, 25, 12, MUTED)?;
        let log_box = d.box_(logs, 0, 122, 1236, 411, Some(INK))?;
        log_box.style(Radius(d.px(8)), Main)?;
        let log_text = d.label(log_box, 14, 12, 1208, 389, 12, 0xe2eef5)?;
        let primary = d.button(root, 22, 670, 286, 35)?;
        let pause = d.button(root, 22, 670, 150, 35)?;
        let cancel = d.button(root, 182, 670, 120, 35)?;
        cancel.object.style(Border(0xe5cfcf), Main)?;
        let details = d.button(root, 1072, 670, 186, 35)?;
        details.object.style(Border(BLUE), Main)?;
        details.object.style(BorderWidth(2), Main)?;
        let overlay = d.box_(root, 0, 0, 1280, 720, Some(INK))?;
        overlay.style(Opacity(52), Main)?;
        let dialog = d.box_(overlay, 400, 225, 480, 260, Some(0xffffff))?;
        dialog.style(Radius(d.px(12)), Main)?;
        dialog.style(Border(LINE), Main)?;
        dialog.style(BorderWidth(1), Main)?;
        let dialog_title = d.label(dialog, 26, 24, 428, 35, 22, INK)?;
        let dialog_body = d.label(dialog, 26, 72, 428, 87, 16, MUTED)?;
        let keep = d.button(dialog, 26, 194, 226, 42)?;
        keep.object.style(Outline(3), Main)?;
        keep.object.style(OutlineColor(BLUE), Main)?;
        let stop = d.button(dialog, 268, 194, 186, 42)?;
        stop.object.style(Border(DANGER), Main)?;
        overlay.hidden(true);
        Ok(Self {
            root,
            scale,
            crystals,
            copy,
            restore,
            mode_hint,
            steps,
            title,
            subtitle,
            progress,
            selection,
            finish,
            logs,
            percent,
            eta,
            elapsed,
            overall,
            total,
            overall_bar,
            read,
            saved,
            saved_exact,
            saved_bar,
            confirmed,
            confirmed_exact,
            confirmed_bar,
            queue,
            settings,
            settings_fields,
            destination,
            tail,
            choices,
            choice_heading,
            choice_target_heading,
            choice_hint,
            choice_summary,
            choice_side,
            choice_divider,
            finish_title,
            finish_body,
            log_summary,
            log_text,
            log_hint,
            primary,
            pause,
            cancel,
            details,
            overlay,
            dialog_title,
            dialog_body,
            keep,
            stop,
        })
    }
    pub fn update(&self, status: &Status) -> Result<()> {
        self.update_state(status, &Interaction::default(), &[])
    }
    pub fn update_state(&self, s: &Status, input: &Interaction, logs: &[String]) -> Result<()> {
        let l = s.language;
        let v = &s.session;
        let mode = v.mode;
        let page = if matches!(s.phase.as_str(), "FAILED" | "CANCELLED") {
            Page::Failed
        } else if matches!(
            s.phase.as_str(),
            "COPYING"
                | "RESTORING"
                | "VERIFYING_RESTORE"
                | "PAUSED"
                | "PAUSE_REQUESTED"
                | "RESUME_REQUESTED"
                | "FINALIZING"
                | "JOB_COMPLETE"
                | "SAVING_LOGS"
                | "UNMOUNTING_NTFS"
        ) {
            Page::Running
        } else if s.finalization_complete {
            Page::Complete
        } else {
            v.page
        };
        let locked = matches!(page, Page::Running | Page::Complete | Page::Failed);
        self.root.hidden(false);
        for (n, frame) in self.crystals.iter().enumerate() {
            frame.hidden(n != (s.monotonic_ms / 180 % 10) as usize);
        }
        let mode_text = |m: Mode| {
            format!(
                "{} [{}]\n{}",
                l.text(if m == Mode::Backup {
                    T::Copy
                } else {
                    T::Restore
                }),
                if m == Mode::Backup { 'B' } else { 'R' },
                l.text(if m == mode {
                    T::Selected
                } else if locked {
                    T::Locked
                } else {
                    T::Switch
                })
            )
        };
        self.copy
            .text(&mode_text(Mode::Backup), !locked, mode == Mode::Backup)?;
        self.restore
            .text(&mode_text(Mode::Restore), !locked, mode == Mode::Restore)?;
        self.mode_hint.text(if locked {
            l.text(T::ModeLocked)
        } else {
            "[B] / [R]"
        })?;
        let labels: &[T] = if mode == Mode::Backup {
            &[T::SelectSources, T::Copying, T::Finish]
        } else {
            &[T::RestoreSource, T::TargetDisk, T::Restoring, T::Finish]
        };
        let current_step = match page {
            Page::CopySelection | Page::RestoreSelection => 0,
            Page::RestoreMapping | Page::RestoreConfirmation => 1,
            Page::Complete => labels.len() - 1,
            _ => labels.len() - 2,
        };
        for (n, step) in self.steps.iter().enumerate() {
            step.hidden(input.logs_open || n >= labels.len());
            if let Some(key) = labels.get(n) {
                let x = 22 + n as u16 * (1236 / labels.len() as u16);
                let px = |v: u16| (f64::from(v) * self.scale).round() as u16;
                step.bounds(px(x), px(87), px(1236 / labels.len() as u16), px(24))?;
                step.text(&format!(
                    "{} {}",
                    if n < current_step {
                        "✓"
                    } else if n == current_step {
                        "◉"
                    } else {
                        "○"
                    },
                    l.text(*key)
                ))?;
                step.style(
                    Text(if n < current_step {
                        GREEN
                    } else if n == current_step {
                        BLUE
                    } else {
                        MUTED
                    }),
                    Main,
                )?;
            }
        }
        let is_progress = page == Page::Running;
        let is_finish = matches!(page, Page::Complete | Page::Failed);
        self.progress.hidden(!is_progress || input.logs_open);
        self.selection
            .hidden(is_progress || is_finish || input.logs_open);
        self.finish.hidden(!is_finish || input.logs_open);
        self.logs.hidden(!input.logs_open);
        self.title.hidden(input.logs_open);
        self.subtitle.hidden(input.logs_open);
        let snap = s.snapshot.as_ref();
        let read = snap.map(|x| x.read_raw_bytes).unwrap_or(0);
        let durable = snap.map(|x| x.durable_raw_bytes).unwrap_or(0);
        let verified = if mode == Mode::Backup {
            s.storage.as_ref().map(|v| v.confirmed_source_bytes)
        } else {
            None
        }
        .unwrap_or_else(|| snap.map(|x| x.verified_raw_bytes).unwrap_or(0));
        let current_total = snap.map(|x| x.total_bytes).unwrap_or(s.source_bytes);
        let total = v.total_bytes.max(current_total);
        let done = v.previous_durable_bytes.saturating_add(durable).min(total);
        let checked = v
            .previous_verified_bytes
            .saturating_add(verified)
            .min(total);
        let aggregate = if total == 0 {
            0
        } else {
            ((u128::from(done) + u128::from(checked)) * 1000 / (2 * u128::from(total))) as i32
        };
        self.overall.text(l.text(T::Overall))?;
        self.percent.text(&format!("{} %", aggregate / 10))?;
        self.overall_bar.bar(1000, aggregate)?;
        let eta = s
            .eta_seconds
            .map(|n| format!("{}:{:02}:{:02}", n / 3600, n / 60 % 60, n % 60))
            .unwrap_or_else(|| l.text(T::Estimating).into());
        self.eta.text(&format!("{}\n{eta}", l.text(T::Eta)))?;
        let seconds = if mode == Mode::Backup {
            s.storage.as_ref().map(|v| v.elapsed_seconds)
        } else {
            None
        }
        .unwrap_or_else(|| snap.map(|v| v.elapsed_seconds.max(0.0) as u64).unwrap_or(0));
        self.elapsed.text(&format!(
            "{}\n{:02}:{:02}:{:02}",
            l.text(T::Elapsed),
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        ))?;
        let selected = v.disks.iter().filter(|d| d.selected).count().max(1);
        self.total.text(&format!(
            "{}\n{}: {} · {}",
            l.message(
                T::DoneCount,
                &[
                    ("done", &v.completed_disks.to_string()),
                    ("total", &selected.to_string())
                ]
            )?,
            l.text(T::Total),
            model::capacity(total),
            model::exact(total)
        ))?;
        self.read.text(&format!(
            "{} {} ({})",
            l.text(T::Read),
            model::capacity(read),
            model::exact(read)
        ))?;
        self.saved.text(&format!(
            "{}    {}% · {} / {}",
            l.text(if mode == Mode::Backup {
                T::Durable
            } else {
                T::Written
            }),
            percentage(durable, current_total),
            model::capacity(durable),
            model::capacity(current_total)
        ))?;
        self.saved_exact.text(&format!(
            "{} / {} {}",
            model::exact(durable),
            l.text(T::Total),
            model::exact(current_total)
        ))?;
        self.saved_bar.bar(1000, ratio(durable, current_total))?;
        self.confirmed.text(&format!(
            "{}    {}% · {} / {}",
            l.text(T::Confirmed),
            percentage(verified, current_total),
            model::capacity(verified),
            model::capacity(current_total)
        ))?;
        self.confirmed_exact.text(&format!(
            "{} / {} {}",
            model::exact(verified),
            l.text(T::Total),
            model::exact(current_total)
        ))?;
        self.confirmed_bar
            .bar(1000, ratio(verified, current_total))?;
        let current = v
            .current_disk
            .and_then(|i| v.disks.get(i))
            .map(|d| d.name.as_str())
            .unwrap_or(&s.source_disk);
        self.title.text(&match page {
            Page::CopySelection => l.text(T::CopyTitle).into(),
            Page::RestoreSelection => l.text(T::RestoreSource).into(),
            Page::RestoreMapping => l.text(T::RestoreMapping).into(),
            Page::RestoreConfirmation => l.text(T::RestoreReview).into(),
            Page::Running => l.message(
                T::CurrentDisk,
                &[
                    ("name", &clipped(current, 40)),
                    (
                        "operation",
                        l.text(if mode == Mode::Backup {
                            T::Copy
                        } else {
                            T::Restore
                        }),
                    ),
                ],
            )?,
            Page::Complete => l.text(T::Finish).into(),
            Page::Failed => l.text(T::Failure).into(),
        })?;
        self.subtitle.text(if page == Page::CopySelection {
            l.text(T::ChangeSources)
        } else if matches!(page, Page::RestoreMapping | Page::RestoreConfirmation) {
            l.text(T::OverwriteWarning)
        } else {
            ""
        })?;
        let mut q = String::new();
        let selected_disks = v
            .disks
            .iter()
            .enumerate()
            .filter(|(_, d)| d.selected)
            .collect::<Vec<_>>();
        let current_position = selected_disks
            .iter()
            .position(|(i, _)| Some(*i) == v.current_disk)
            .unwrap_or(0);
        for &(i, d) in selected_disks
            .iter()
            .skip(current_position.saturating_sub(2))
            .take(3)
        {
            let (icon, key) = match d.state {
                DiskState::Done => ("✓", T::Done),
                DiskState::Working => ("◉", T::Working),
                DiskState::Unavailable | DiskState::Failed => ("!", T::Unavailable),
                _ => ("○", T::Waiting),
            };
            q.push_str(&format!(
                "{icon} [{}] {}\n   {} · {}\n",
                i + 1,
                clipped(&d.name, 24),
                model::capacity(d.bytes),
                l.text(key)
            ));
            if q.lines().count() >= 6 {
                break;
            }
        }
        self.queue.text(&q)?;
        let settings = if let Some(p) = &s.selected_plan {
            format!(
                "{} / {}\n{} / {}\n{}   {}\n{}   {}\n{} {} MiB\n{} BLAKE3",
                l.text(T::ReadBlock),
                l.text(T::Compression),
                format_mib(p.read_bytes),
                format!("{} {}", p.codec.name(), p.compression_level),
                l.text(T::Workers),
                p.workers,
                l.text(T::VerifyWorkers),
                s.verify_workers,
                l.text(T::Frame),
                p.range_bytes / (1 << 20),
                l.text(T::Hash)
            )
        } else {
            l.text(T::Preparing).into()
        };
        for (n, field) in self.settings_fields.iter().enumerate() {
            field.hidden(s.selected_plan.is_none());
            if let Some(p) = &s.selected_plan {
                let (key, value) = match n {
                    0 => (T::ReadBlock, format_mib(p.read_bytes)),
                    1 => (
                        T::Compression,
                        format!("{} {}", p.codec.name(), p.compression_level),
                    ),
                    2 => (T::Workers, p.workers.to_string()),
                    _ => (T::VerifyWorkers, s.verify_workers.to_string()),
                };
                field.text(&format!("{}\n{value}", l.text(key)))?;
            }
        }
        self.settings.text(
            &s.selected_plan
                .as_ref()
                .map(|p| {
                    format!(
                        "{} {} MiB · {} BLAKE3",
                        l.text(T::Frame),
                        p.range_bytes / (1 << 20),
                        l.text(T::Hash)
                    )
                })
                .unwrap_or_else(|| l.text(T::Preparing).into()),
        )?;
        let destination = format!(
            "{}: {}\n{}: NTFS\n{} {}",
            l.text(T::Destination),
            l.text(T::UsbDisk),
            l.text(T::Filesystem),
            l.text(T::Free),
            model::capacity(v.destination_free_bytes)
        );
        self.destination.text(&destination)?;
        if mode == Mode::Backup {
            if let Some(st) = &s.storage {
                self.tail.text(&l.storage_summary(st))?;
            } else {
                self.tail
                    .text(if durable == current_total && verified < current_total {
                        l.text(T::CopyTail)
                    } else {
                        ""
                    })?;
            }
        } else {
            self.tail
                .text(if durable == current_total && verified < current_total {
                    l.text(T::RestoreTail)
                } else {
                    ""
                })?;
        }
        self.choices(v, s, input, l, &destination)?;
        self.finish_title.text(l.text(if page == Page::Failed {
            T::Failure
        } else {
            T::AllFinished
        }))?;
        self.finish_body.text(&if page == Page::Failed {
            let reason = l.error_detail(&s.detail);
            let next = if let Some(seconds) = s.shutdown_countdown_seconds {
                l.message(T::Shutdown, &[("seconds", &seconds.to_string())])?
            } else {
                l.text(T::FailureStopped).into()
            };
            format!("{reason}\n{next}")
        } else if let Some(seconds) = s.shutdown_countdown_seconds {
            l.message(T::Shutdown, &[("seconds", &seconds.to_string())])?
        } else if s.finalization_complete {
            l.text(T::Done).into()
        } else {
            l.text(T::Logging).into()
        })?;
        self.log_summary.text(&format!(
            "{}\n{} {} / {} ({})\n{}",
            l.text(T::LogsTitle),
            l.text(T::Read),
            model::capacity(read),
            model::capacity(current_total),
            model::exact(read),
            settings.replace('\n', " · ")
        ))?;
        self.log_hint.text(l.text(T::LogsControls))?;
        let rows = 18usize;
        let end = logs
            .len()
            .saturating_sub(input.log_page.saturating_mul(rows));
        let begin = end.saturating_sub(rows);
        self.log_text.text(
            &logs[begin..end]
                .iter()
                .map(|s| clipped(&s.replace('\0', "�"), 154))
                .collect::<Vec<_>>()
                .join("\n"),
        )?;
        self.pause.object.hidden(!is_progress || input.logs_open);
        self.pause.text(
            &format!(
                "{} [P]",
                l.text(if s.can_resume { T::Resume } else { T::Pause })
            ),
            s.can_pause || s.can_resume,
            false,
        )?;
        self.cancel.object.hidden(!is_progress || input.logs_open);
        self.cancel
            .text(&format!("{} [C]", l.text(T::Cancel)), s.can_cancel, false)?;
        self.cancel.caption.style(Text(DANGER), Main)?;
        self.primary
            .object
            .hidden(is_progress || is_finish || input.logs_open);
        let (caption, key, enabled) = match page {
            Page::CopySelection => (T::Start, 'S', v.disks.iter().any(|d| d.selected)),
            Page::RestoreSelection => (T::Next, 'N', v.selected_backup.is_some()),
            Page::RestoreMapping => (
                T::Next,
                'N',
                v.selected_saved_disk.is_some() && v.selected_target.is_some(),
            ),
            Page::RestoreConfirmation => (T::RestoreStart, 'Y', v.selected_target.is_some()),
            _ => (T::Start, 'S', false),
        };
        self.primary
            .text(&format!("{} [{key}]", l.text(caption)), enabled, true)?;
        self.details.text(
            &format!(
                "{} [{}]",
                l.text(if input.logs_open {
                    T::LogsBack
                } else {
                    T::Details
                }),
                if input.logs_open { "I / Esc" } else { "I" }
            ),
            true,
            false,
        )?;
        self.overlay.hidden(!input.cancel_open);
        self.dialog_title.text(l.text(T::AbortTitle))?;
        self.dialog_body.text(l.text(if mode == Mode::Backup {
            T::AbortCopyWarning
        } else {
            T::AbortRestoreWarning
        }))?;
        self.keep
            .text(&format!("{} [Esc]", l.text(T::Continue)), true, false)?;
        self.stop
            .text(&format!("{} [Y]", l.text(T::Abort)), true, false)?;
        Ok(())
    }
    fn choices(
        &self,
        v: &model::SessionView,
        s: &Status,
        input: &Interaction,
        l: Language,
        destination: &str,
    ) -> Result<()> {
        let first = input.list_page.saturating_mul(3);
        let review = v.page == Page::RestoreConfirmation;
        let mapping = v.page == Page::RestoreMapping || review;
        let px = |n: u16| ((f64::from(n) * self.scale).round() as u16).max(u16::from(n > 0));
        self.choice_side.hidden(mapping);
        self.choice_divider.hidden(mapping);
        self.choice_target_heading.hidden(!mapping);
        self.choice_target_heading.text(l.text(T::TargetDisk))?;
        for (n, row) in self.choices.iter().enumerate() {
            row.background.hidden(true);
            if n >= 3 {
                continue;
            }
            let index = first + n;
            let width = if mapping { 600 } else { 968 };
            row.background
                .bounds(0, px(25 + n as u16 * 54), px(width), px(52))?;
            row.name
                .bounds(px(8), px(5), px(if mapping { 430 } else { 745 }), px(24))?;
            row.meta
                .bounds(px(8), px(30), px(if mapping { 430 } else { 745 }), px(18))?;
            row.size.bounds(
                px(if mapping { 438 } else { 756 }),
                px(6),
                px(if mapping { 154 } else { 204 }),
                px(43),
            )?;
            let item = match v.page {
                Page::CopySelection => v.disks.get(index).map(|d| {
                    (
                        format!(
                            "{}  {} [{}]",
                            if d.selected { "[✓]" } else { "[ ]" },
                            clipped(&d.name, 42),
                            n + 1
                        ),
                        clipped(&d.stable_id, 88),
                        d.bytes,
                    )
                }),
                Page::RestoreSelection => v.backups.get(index).map(|b| {
                    (
                        format!(
                            "{}  {} [{}]",
                            if v.selected_backup == Some(index) {
                                "◉"
                            } else {
                                "○"
                            },
                            b.datetime,
                            n + 1
                        ),
                        format!("{} {}", b.disks.len(), l.text(T::SourceDisk)),
                        b.disks
                            .iter()
                            .fold(0u64, |sum, d| sum.saturating_add(d.bytes)),
                    )
                }),
                Page::RestoreMapping => v
                    .selected_backup
                    .and_then(|i| v.backups.get(i))
                    .and_then(|b| b.disks.get(index))
                    .map(|d| {
                        (
                            format!(
                                "{}  {} [{}]",
                                if v.selected_saved_disk == Some(index) {
                                    "◉"
                                } else {
                                    "○"
                                },
                                clipped(&d.name, 32),
                                n + 1
                            ),
                            clipped(&d.stable_id, 60),
                            d.bytes,
                        )
                    }),
                _ => None,
            };
            if let Some((name, meta, bytes)) = item {
                row.background.hidden(false);
                row.name.text(&name)?;
                row.meta.text(&meta)?;
                row.size.text(&format!(
                    "{}\n{}",
                    model::capacity(bytes),
                    model::exact(bytes)
                ))?;
            }
        }
        if v.page == Page::RestoreMapping {
            for (n, row) in self.choices.iter().skip(3).enumerate() {
                let index = first + n;
                if let Some(d) = v.disks.get(index) {
                    row.background.hidden(false);
                    row.name.text(&format!(
                        "{} {} [{}]",
                        if v.selected_target == Some(index) {
                            "◉"
                        } else {
                            "○"
                        },
                        clipped(&d.name, 26),
                        ['A', 'D', 'F'][n]
                    ))?;
                    row.meta.text(&clipped(&d.stable_id, 44))?;
                    row.size.text(&format!(
                        "{}\n{}",
                        model::capacity(d.bytes),
                        model::exact(d.bytes)
                    ))?;
                }
            }
        }
        if review {
            if let Some(d) = v
                .selected_backup
                .and_then(|i| v.backups.get(i))
                .and_then(|b| v.selected_saved_disk.and_then(|i| b.disks.get(i)))
            {
                let row = &self.choices[0];
                row.background.hidden(false);
                row.name.text(&clipped(&d.name, 32))?;
                row.meta.text(&clipped(&d.stable_id, 60))?;
                row.size.text(&format!(
                    "{}\n{}",
                    model::capacity(d.bytes),
                    model::exact(d.bytes)
                ))?;
            }
            if let Some(d) = v.selected_target.and_then(|i| v.disks.get(i)) {
                let row = &self.choices[3];
                row.background.hidden(false);
                row.name.text(&clipped(&d.name, 32))?;
                row.meta.text(&clipped(&d.stable_id, 60))?;
                row.size.text(&format!(
                    "{}\n{}",
                    model::capacity(d.bytes),
                    model::exact(d.bytes)
                ))?;
            }
        }
        self.choice_heading
            .text(l.text(if v.page == Page::CopySelection {
                T::SelectSources
            } else if mapping {
                T::SourceDisk
            } else {
                T::RestoreSource
            }))?;
        self.choice_hint
            .text(l.text(if v.page == Page::RestoreMapping {
                T::TargetKeys
            } else {
                T::PageKeys
            }))?;
        self.choice_hint.hidden(match v.page {
            Page::CopySelection => v.disks.len() <= 3,
            Page::RestoreSelection => v.backups.len() <= 3,
            Page::RestoreMapping => false,
            _ => true,
        });
        let summary = if v.page == Page::CopySelection {
            l.message(
                T::SelectedCount,
                &[
                    (
                        "count",
                        &v.disks.iter().filter(|d| d.selected).count().to_string(),
                    ),
                    ("capacity", &model::capacity(v.total_bytes)),
                ],
            )?
        } else if review {
            format!(
                "{}\n{}",
                l.text(T::UsedBackup),
                v.selected_backup
                    .and_then(|i| v.backups.get(i))
                    .map(|b| b.datetime.as_str())
                    .unwrap_or("")
            )
        } else if v.page == Page::RestoreSelection && v.backups.is_empty() {
            l.text(T::NoBackups).into()
        } else {
            String::new()
        };
        self.choice_summary.text(&summary)?;
        self.choice_summary.bounds(
            0,
            px(if review { 100 } else { 189 }),
            px(if mapping { 1236 } else { 968 }),
            px(if review { 100 } else { 57 }),
        )?;
        let state = if let Some(error) = &v.preparation_error {
            format!(
                "{}\n{}",
                l.text(T::PreparationFailed),
                l.error_detail(error)
            )
        } else {
            let preparation = l.text(if v.prepared { T::Ready } else { T::Preparing });
            if let Some(seconds) = s.countdown_seconds {
                format!(
                    "{}\n\n{preparation}",
                    l.message(T::Countdown, &[("seconds", &seconds.to_string())])?
                )
            } else {
                preparation.into()
            }
        };
        self.choice_side
            .text(&format!("{state}\n\n{destination}"))?;
        Ok(())
    }
}
fn format_mib(bytes: u64) -> String {
    if bytes % (1 << 20) == 0 {
        format!("{} MiB", bytes / (1 << 20))
    } else {
        format!("{} KiB", bytes / (1 << 10))
    }
}
