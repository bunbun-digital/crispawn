// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: LicenseRef-Bunbun-Proprietary
#![forbid(unsafe_code)]
mod view;
use bunbun_ui::Ui;
use perfect_backup_demo_model::{Progress, State, TOTAL_BYTES};
use std::error::Error;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::{Duration, Instant};

fn actions(ui: &Ui, progress: &mut Progress) -> Result<bool, Box<dyn Error>> {
    let mut reset = false;
    while let Some(action) = ui.next_action()? {
        match action {
            view::PAUSE => {
                progress.toggle_pause();
                println!(
                    "PERFECT_BACKUP_DEMO_ACTION pause={}",
                    progress.state == State::Paused
                );
            }
            view::RESET => {
                progress.reset();
                reset = true;
                println!("PERFECT_BACKUP_DEMO_ACTION reset");
            }
            _ => return Err("unknown UI action".into()),
        }
    }
    Ok(reset)
}
fn frame(
    ui: &Ui,
    view: &view::View<'_>,
    model: &Progress,
    output: &Path,
    name: &str,
) -> Result<(), Box<dyn Error>> {
    view.update(&model.snapshot())?;
    let rgb = ui.render_rgb()?;
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output.join(format!("{name}.ppm")))?;
    let mut file = BufWriter::new(file);
    file.write_all(b"P6\n1024 768\n255\n")?;
    file.write_all(&rgb)?;
    file.flush()?;
    println!(
        "PERFECT_BACKUP_RUST_FRAME name={name} percent={} simulated=true backup_io=none",
        model.snapshot().permille / 10
    );
    Ok(())
}
fn render(font: &str, output: &Path) -> Result<(), Box<dyn Error>> {
    std::fs::create_dir(output)?;
    let ui = Ui::memory(1024, 768, font)?;
    let view = view::View::create(&ui)?;
    let mut model = Progress::default();
    frame(&ui, &view, &model, output, "start")?;
    model.advance(30_000);
    frame(&ui, &view, &model, output, "middle")?;
    // Exercise LVGL keypad processing and the same action queue as live mode.
    ui.send_shortcut('p')?;
    actions(&ui, &mut model)?;
    if model.state != State::Paused {
        return Err("LVGL keyboard pause failed".into());
    }
    frame(&ui, &view, &model, output, "paused")?;
    let paused = model.clone();
    model.advance(u64::MAX);
    if model != paused {
        return Err("paused progress advanced".into());
    }
    ui.send_shortcut('P')?;
    actions(&ui, &mut model)?;
    if model.state != State::Running {
        return Err("LVGL keyboard resume failed".into());
    }
    ui.send_shortcut('r')?;
    actions(&ui, &mut model)?;
    if model != Progress::default() {
        return Err("LVGL keyboard reset failed".into());
    }
    frame(&ui, &view, &model, output, "reset")?;
    model.advance(u64::MAX);
    frame(&ui, &view, &model, output, "complete")?;
    ui.send_shortcut('R')?;
    actions(&ui, &mut model)?;
    if model != Progress::default() {
        return Err("LVGL reset after completion failed".into());
    }
    println!("PERFECT_BACKUP_RUST_INPUT_OK injected_lvgl_keypad=true physical_keyboard=false");
    Ok(())
}
fn headless() {
    let mut p = Progress::default();
    p.advance(30_000);
    p.toggle_pause();
    let before = p.clone();
    p.advance(u64::MAX);
    assert_eq!(p, before);
    p.toggle_pause();
    p.advance(u64::MAX);
    assert_eq!(p.state, State::Complete);
    assert_eq!(p.copied_bytes, TOTAL_BYTES);
    p.reset();
    assert_eq!(p, Progress::default());
    println!("PERFECT_BACKUP_MODEL_OK gui=disabled backup_io=none");
}
fn live() -> Result<(), Box<dyn Error>> {
    let ui = Ui::open()?;
    let view = view::View::create(&ui)?;
    let mut model = Progress::default();
    let mut previous = Instant::now();
    let mut last_percent = 101;
    view.update(&model.snapshot())?;
    ui.step();
    println!(
        "PERFECT_BACKUP_UI_READY framework=LVGL version=9.6.0 language=Rust keyboards={} lang=ja",
        ui.keyboards()
    );
    println!("PERFECT_BACKUP_DEMO_READY font=NotoSansJP data=simulated backup_io=none");
    while !ui.should_quit() {
        let now = Instant::now();
        let reset = actions(&ui, &mut model)?;
        if reset {
            previous = now;
            last_percent = 101;
        }
        model.advance(
            now.duration_since(previous)
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
        );
        previous = now;
        view.update(&model.snapshot())?;
        let percent = model.snapshot().permille / 10;
        if percent != last_percent {
            println!("PERFECT_BACKUP_DEMO_PROGRESS percent={percent} copied={} total={TOTAL_BYTES} simulated=true language=Rust",model.copied_bytes);
            last_percent = percent;
        }
        std::thread::sleep(Duration::from_millis(u64::from(ui.step())));
    }
    Ok(())
}
fn execute() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => live(),
        [flag] if flag == "--headless" => {
            headless();
            Ok(())
        }
        [flag, font, output] if flag == "--render" => render(font, Path::new(output)),
        _ => Err("usage: perfect-backup-demo [--headless | --render FONT NEW_DIRECTORY]".into()),
    }
}
fn main() {
    if let Err(e) = execute() {
        eprintln!("PERFECT_BACKUP_DEMO_FAIL {e}");
        std::process::exit(1);
    }
}
