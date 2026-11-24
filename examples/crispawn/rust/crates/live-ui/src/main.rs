// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
#![forbid(unsafe_code)]
mod assets;
mod splash;
mod view;
use crispawn_core::{
    live::{Command, CommandAction, Status},
    presentation::{Interaction, Key, Page},
};
use crispawn_ui::{InputKey, Ui};
use std::{
    error::Error,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
type Result<T> = std::result::Result<T, Box<dyn Error>>;
fn asset_directory() -> PathBuf {
    std::env::var_os("CRISPAWN_UI_ASSET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(assets::DIRECTORY))
}
fn request(root: &Path, action: CommandAction) -> Result<()> {
    let mut random = [0; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    let id = crispawn_core::types::hex(&random);
    let temporary = root.join(format!(".command-{id}"));
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    serde_json::to_writer(
        &mut f,
        &Command {
            schema_version: 1,
            id: id.clone(),
            action,
        },
    )?;
    f.sync_all()?;
    fs::hard_link(
        &temporary,
        root.join("commands").join(format!("cmd-{id}.json")),
    )?;
    fs::remove_file(temporary)?;
    Ok(())
}
fn input(key: InputKey) -> Key {
    match key {
        InputKey::Character(c) => Key::Character(c),
        InputKey::Escape => Key::Escape,
        InputKey::PageUp => Key::PageUp,
        InputKey::PageDown => Key::PageDown,
        InputKey::End => Key::End,
    }
}
fn read_logs(root: &Path) -> Vec<String> {
    // The backend owns this RAM directory. Never open the backup disk from UI.
    let mut paths: Vec<_> = fs::read_dir(root.join("logs"))
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|v| v.to_str()).is_some_and(|v| {
                (v.starts_with("events-") && v.ends_with(".jsonl"))
                    || v == "supervisor-failure.jsonl"
            })
        })
        .collect();
    paths.sort();
    let mut lines = vec![];
    for path in paths
        .into_iter()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        if let Ok(file) = fs::File::open(path) {
            let mut text = String::new();
            if file.take(4 << 20).read_to_string(&mut text).is_ok() {
                for line in text.lines() {
                    lines.push(
                        line.chars()
                            .map(|c| if c.is_control() { ' ' } else { c })
                            .collect(),
                    );
                }
            }
        }
    }
    if lines.len() > 512 {
        lines.drain(..lines.len() - 512);
    }
    lines
}
fn live(root: &Path, boot: bool) -> Result<()> {
    let ui = if boot {
        Ui::display_only()?
    } else {
        Ui::open()?
    };
    let splash = splash::Splash::new(&ui, Path::new(splash::LOGO))?;
    let mut view = None;
    let mut interaction = Interaction::default();
    let mut logs = vec![];
    let start = Instant::now();
    let mut last_update = Instant::now() - Duration::from_secs(1);
    let mut last_logs = Instant::now() - Duration::from_secs(1);
    let mut s = Status::new(String::new());
    println!("CRISPAWN_SPLASH_READY background=FFFFFF aspect=contain");
    while !ui.should_quit() && !root.join("ui-stop").exists() {
        if last_update.elapsed() >= Duration::from_millis(100) {
            if let Ok(file) = fs::File::open(root.join("status.json")) {
                let mut bytes = vec![];
                file.take((2 << 20) + 1).read_to_end(&mut bytes)?;
                if bytes.len() <= 2 << 20 {
                    if let Ok(status) = serde_json::from_slice::<Status>(&bytes) {
                        if status.schema_version == 1 && !status.job_id.is_empty() {
                            s = status;
                            if !matches!(
                                s.phase.as_str(),
                                "STARTING" | "DISCOVERING" | "MOUNTING_NTFS"
                            ) {
                                if view.is_none() {
                                    let next = view::View::with_assets(&ui, &asset_directory())?;
                                    splash.hide();
                                    view = Some(next);
                                    println!("PB_UI_READY data=real source=controller design=CRISPAWN-UI-20261121-v1 language=Rust font=NotoSansJP");
                                }
                                interaction.synchronize(&s);
                            }
                        }
                    }
                }
            }
            if interaction.logs_open && last_logs.elapsed() >= Duration::from_millis(400) {
                logs = read_logs(root);
                last_logs = Instant::now();
            }
            if let Some(view) = &view {
                view.update_state(&s, &interaction, &logs)?;
            }
            if !s.job_id.is_empty() {
                let data = serde_json::to_vec(
                    &serde_json::json!({"schema_version":1,"job_id":s.job_id,"pid":std::process::id(),"prestarted":boot,"presentation_ready":view.is_some(),"monotonic_ms":start.elapsed().as_millis().to_string(),"observed_heartbeat":s.heartbeat,"observed_monotonic_ms":s.monotonic_ms}),
                )?;
                let temporary = root.join(".ui-next");
                fs::write(&temporary, data)?;
                fs::rename(temporary, root.join("ui.json"))?;
            }
            last_update = Instant::now();
        }
        if let Some(view) = &view {
            while let Some(key) = ui.next_input()? {
                if let Some(action) = interaction.key(input(key), &s) {
                    request(root, action)?;
                }
                view.update_state(&s, &interaction, &logs)?;
            }
            view.animate(start.elapsed().as_millis() as u64);
        }
        std::thread::sleep(Duration::from_millis(u64::from(ui.step())));
    }
    drop(ui);
    let _ = fs::remove_file(root.join("ui.json"));
    Ok(())
}
fn write_rgb(ui: &Ui, width: u32, height: u32, path: &Path) -> Result<()> {
    let data = ui.render_rgb()?;
    let mut f = OpenOptions::new().write(true).create_new(true).open(path)?;
    write!(f, "P6\n{width} {height}\n255\n")?;
    f.write_all(&data)?;
    Ok(())
}
fn render_splash(font: &str, logo: &Path, width: u32, height: u32, path: &Path) -> Result<()> {
    let ui = Ui::memory(width, height, font)?;
    let _splash = splash::Splash::new(&ui, logo)?;
    write_rgb(&ui, width, height, path)
}
fn render_state(
    font: &str,
    assets: &Path,
    status: &Status,
    logs: &[String],
    width: u32,
    height: u32,
    path: &Path,
    keys: &str,
) -> Result<()> {
    let ui = Ui::memory(width, height, font)?;
    let view = view::View::with_assets(&ui, assets)?;
    let mut interaction = Interaction::default();
    let mut emitted = vec![];
    view.update_state(status, &interaction, logs)?;
    for key in keys.chars() {
        ui.send_input(InputKey::Character(key))?;
        let received = ui.next_input()?.ok_or("input dispatch missing")?;
        if let Some(action) = interaction.key(input(received), status) {
            emitted.push(action);
        }
        view.update_state(status, &interaction, logs)?;
    }
    write_rgb(&ui, width, height, path)?;
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path.with_extension("actions.json"))?;
    serde_json::to_writer_pretty(&mut f, &emitted)?;
    f.write_all(b"\n")?;
    Ok(())
}
fn render(font: &str, path: &Path) -> Result<()> {
    let ui = Ui::memory(1280, 720, font)?;
    let view = view::View::with_assets(&ui, &asset_directory())?;
    let mut s = Status::new("render-test".into());
    s.phase = "MENU".into();
    s.countdown_seconds = Some(30);
    view.update(&s)?;
    write_rgb(&ui, 1280, 720, path)?;
    // Exercise actual LVGL key press/release, not just the pure Rust reducer.
    let mut interaction = Interaction::default();
    ui.send_input(InputKey::Character('b'))?;
    assert_eq!(
        interaction.key(input(ui.next_input()?.unwrap()), &s),
        Some(CommandAction::CopyMode)
    );
    s.session.page = Page::Running;
    s.can_cancel = true;
    s.can_pause = true;
    ui.send_input(InputKey::Character('C'))?;
    assert!(interaction
        .key(input(ui.next_input()?.unwrap()), &s)
        .is_none());
    assert!(interaction.cancel_open);
    ui.send_input(InputKey::Escape)?;
    assert!(interaction
        .key(input(ui.next_input()?.unwrap()), &s)
        .is_none());
    assert!(!interaction.cancel_open);
    ui.send_input(InputKey::Character('c'))?;
    interaction.key(input(ui.next_input()?.unwrap()), &s);
    ui.send_input(InputKey::Character('y'))?;
    assert_eq!(
        interaction.key(input(ui.next_input()?.unwrap()), &s),
        Some(CommandAction::Cancel)
    );
    ui.send_input(InputKey::Character('i'))?;
    interaction.key(input(ui.next_input()?.unwrap()), &s);
    assert!(interaction.logs_open);
    ui.send_input(InputKey::PageUp)?;
    interaction.key(input(ui.next_input()?.unwrap()), &s);
    assert_eq!(interaction.log_page, 1);
    ui.send_input(InputKey::PageDown)?;
    interaction.key(input(ui.next_input()?.unwrap()), &s);
    assert_eq!(interaction.log_page, 0);
    ui.send_input(InputKey::End)?;
    interaction.key(input(ui.next_input()?.unwrap()), &s);
    ui.send_input(InputKey::Escape)?;
    interaction.key(input(ui.next_input()?.unwrap()), &s);
    assert!(!interaction.logs_open);
    ui.send_key(crispawn_ui::Key::Next)?;
    ui.send_key(crispawn_ui::Key::Enter)?;
    assert!(ui.next_input()?.is_none());
    println!("PB_UI_SHORTCUTS_OK keys=B,R,P,C,I,Esc,PgUp,PgDn,End enter_required=false physical_keyboard=false modal_confirmation=true");
    Ok(())
}
fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let result=match args.as_slice(){
        [root]=>live(Path::new(root),false),[mode,root] if mode=="--boot"=>live(Path::new(root),true),
        [mode,font,logo,w,h,path] if mode=="--render-splash"=>w.parse().and_then(|width|h.parse().map(|height|(width,height))).map_err(|e|->Box<dyn Error>{Box::new(e)}).and_then(|(w,h)|render_splash(font,Path::new(logo),w,h,Path::new(path))),
        [mode,font,path] if mode=="--render"=>render(font,Path::new(path)),
        [mode,font,assets,status,w,h,path,keys] if mode=="--render-state"=>(||->Result<()> {
            let bytes=fs::read(status)?;if bytes.len()>2<<20{return Err("render status exceeds bound".into())}
            let state=serde_json::from_slice(&bytes)?;render_state(font,Path::new(assets),&state,&[],w.parse()?,h.parse()?,Path::new(path),keys)
        })(),
        _=>Err("crispawn-gui RUNTIME_DIRECTORY | --render FONT NEW_PPM | --render-state FONT ASSETS STATUS WIDTH HEIGHT NEW_PPM KEYS".into()),
    };
    if let Err(e) = result {
        eprintln!("PB_UI_FAILED {e}");
        std::process::exit(1);
    }
}
