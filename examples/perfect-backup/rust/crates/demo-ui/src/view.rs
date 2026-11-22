// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: LicenseRef-Bunbun-Proprietary
#![forbid(unsafe_code)]
use bunbun_ui::{
    Align::*,
    Font, Kind,
    Length::{Percent as Pc, Pixels as Px},
    Result,
    Selector::*,
    Style::*,
    Ui, Widget,
};
use perfect_backup_demo_model::Snapshot;

pub const PAUSE: u32 = 1;
pub const RESET: u32 = 2;
pub struct View<'ui> {
    bar: Widget<'ui>,
    percent: Widget<'ui>,
    state: Widget<'ui>,
    copied: Widget<'ui>,
    elapsed: Widget<'ui>,
    remaining: Widget<'ui>,
    speed: Widget<'ui>,
    pause: Widget<'ui>,
    pause_caption: Widget<'ui>,
}
fn text<'a>(
    ui: &'a Ui,
    parent: Widget<'a>,
    content: &str,
    font: &Font<'a>,
    color: u32,
) -> Result<Widget<'a>> {
    let label = ui.child(parent, Kind::Label)?;
    label.font(font)?;
    label.style(Text(color), Main)?;
    label.text(content)?;
    Ok(label)
}
fn plain<'a>(
    ui: &'a Ui,
    parent: Widget<'a>,
    width: bunbun_ui::Length,
    height: u16,
    column: bool,
) -> Result<Widget<'a>> {
    let o = ui.child(parent, Kind::Container)?;
    o.plain();
    o.size(width, Px(height))?;
    o.flex(column);
    Ok(o)
}
fn card<'a>(ui: &'a Ui, parent: Widget<'a>, height: u16) -> Result<Widget<'a>> {
    let o = ui.child(parent, Kind::Container)?;
    o.size(Pc(100), Px(height))?;
    for s in [
        Background(0xf7f9fc),
        Border(0xd9e2ec),
        Radius(14),
        Padding(20),
        RowGap(10),
    ] {
        o.style(s, Main)?;
    }
    Ok(o)
}
fn control<'a>(
    ui: &'a Ui,
    parent: Widget<'a>,
    caption: &str,
    action: u32,
    key: char,
    bg: u32,
    fg: u32,
) -> Result<(Widget<'a>, Widget<'a>)> {
    let o = ui.child(parent, Kind::Button)?;
    o.size(Pc(49), Px(58))?;
    for s in [Background(bg), Text(fg), Radius(10)] {
        o.style(s, Main)?;
    }
    o.style(Background(0xe4eaf1), Disabled)?;
    o.style(Text(0x748398), Disabled)?;
    o.style(Outline(3), Focused)?;
    o.style(OutlineColor(0x0b6e62), Focused)?;
    o.on_click(action)?;
    o.shortcut(key)?;
    let label = ui.child(o, Kind::Label)?;
    label.text(caption)?;
    label.center();
    Ok((o, label))
}
impl<'a> View<'a> {
    pub fn create(ui: &'a Ui) -> Result<Self> {
        ui.direct_keys()?;
        let body = ui.font(24)?;
        let small = ui.font(20)?;
        let heading = ui.font(32)?;
        let number = ui.font(48)?;
        ui.theme(0x16796e, 0x52667e, &body)?;
        let screen = ui.root()?;
        screen.font(&body)?;
        screen.flex(true);
        for s in [Background(0xffffff), Padding(28), RowGap(14)] {
            screen.style(s, Main)?;
        }
        let header = plain(ui, screen, Pc(100), 80, false)?;
        header.align(SpaceBetween, Start, Start);
        let titles = plain(ui, header, Pc(83), 80, true)?;
        text(ui, titles, "perfect backup", &heading, 0x172b4d)?;
        text(
            ui,
            titles,
            "ディスク全体をイメージとして保存",
            &small,
            0x5c6f86,
        )?;
        let badge = text(ui, header, " デモ ", &small, 0x126b5f)?;
        for s in [Background(0xe1f4ed), Opacity(255), Radius(6), Padding(6)] {
            badge.style(s, Main)?;
        }

        let devices = card(ui, screen, 116)?;
        devices.flex(false);
        devices.align(SpaceBetween, Start, Start);
        let source = plain(ui, devices, Pc(48), 76, true)?;
        text(ui, source, "バックアップ元（サンプル）", &small, 0x5c6f86)?;
        text(ui, source, "内蔵SSD / 512 GiB", &body, 0x1f3047)?;
        let dest = plain(ui, devices, Pc(48), 76, true)?;
        text(ui, dest, "出力ファイル（サンプル）", &small, 0x5c6f86)?;
        text(ui, dest, "ssd-backup_2026.img", &body, 0x1f3047)?;

        let progress = card(ui, screen, 278)?;
        progress.flex(true);
        let top = plain(ui, progress, Pc(100), 76, false)?;
        top.align(SpaceBetween, Center, Center);
        let state = text(ui, top, "バックアップ中", &body, 0x126b5f)?;
        let percent = text(ui, top, "0%", &number, 0x172b4d)?;
        percent.width(180);
        percent.style(TextRight, Main)?;
        let bar = ui.child(progress, Kind::Bar)?;
        bar.size(Pc(100), Px(18))?;
        bar.bar(1000, 0)?;
        bar.style(Background(0xe0e7ef), Main)?;
        bar.style(Background(0x168577), Indicator)?;
        bar.style(Radius(9), Main)?;
        bar.style(Radius(9), Indicator)?;
        let copied = text(ui, progress, "", &small, 0x52667e)?;
        let metrics = plain(ui, progress, Pc(100), 72, false)?;
        metrics.align(SpaceBetween, Start, Start);
        let col = plain(ui, metrics, Pc(30), 72, true)?;
        text(ui, col, "経過時間", &small, 0x5c6f86)?;
        let elapsed = text(ui, col, "", &body, 0x1f3047)?;
        let col = plain(ui, metrics, Pc(30), 72, true)?;
        text(ui, col, "残り時間", &small, 0x5c6f86)?;
        let remaining = text(ui, col, "", &body, 0x1f3047)?;
        let col = plain(ui, metrics, Pc(30), 72, true)?;
        text(ui, col, "転送速度", &small, 0x5c6f86)?;
        let speed = text(ui, col, "", &body, 0x1f3047)?;

        let actions = plain(ui, screen, Pc(100), 66, false)?;
        actions.align(SpaceBetween, Center, Center);
        let (pause, pause_caption) =
            control(ui, actions, "[P] 一時停止", PAUSE, 'p', 0x16796e, 0xffffff)?;
        control(ui, actions, "[R] 最初から", RESET, 'r', 0xe8eef5, 0x253a55)?;
        text(
            ui,
            screen,
            "P: 一時停止 / 再開　R: 最初から（Enter不要）",
            &small,
            0x5c6f86,
        )?;
        text(
            ui,
            screen,
            "表示用デモです。バックアップデータは作成しません。",
            &small,
            0x5c6f86,
        )?;
        Ok(Self {
            bar,
            percent,
            state,
            copied,
            elapsed,
            remaining,
            speed,
            pause,
            pause_caption,
        })
    }
    pub fn update(&self, s: &Snapshot) -> Result<()> {
        self.bar.bar(1000, s.permille as i32)?;
        self.percent.text(&format!("{}%", s.permille / 10))?;
        self.state.text(s.status)?;
        self.pause_caption
            .text(&format!("[P] {}", s.pause_caption))?;
        self.pause.enabled(s.pause_enabled);
        self.copied.text(&s.copied)?;
        self.elapsed.text(&s.elapsed)?;
        self.remaining.text(&s.remaining)?;
        self.speed.text(&s.speed)?;
        Ok(())
    }
}
