// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
use perfect_backup_core::{workflow::Action, Error, Result};
use perfect_backup_engine::terminal;
use perfect_backup_engine::{
    job::{Controller, JobEvent, JobEvents, Operation},
    job_config::JobConfig,
    presentation,
};
use std::{
    io::{BufRead, Read, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc,
    },
    time::Duration,
};

static SIGNAL_CANCEL: AtomicBool = AtomicBool::new(false);
extern "C" fn cancel_signal(_: libc::c_int) {
    SIGNAL_CANCEL.store(true, Ordering::Relaxed);
}
struct Output {
    sender: mpsc::SyncSender<JobEvent>,
    dropped: AtomicU64,
    paused: AtomicBool,
}
impl JobEvents for Output {
    fn emit(&self, event: JobEvent) {
        if let JobEvent::State { phase } | JobEvent::Progress { phase, .. } = &event {
            self.paused.store(phase == "PAUSED", Ordering::Release);
        }
        if self.sender.try_send(event).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}
struct Options {
    config: PathBuf,
    operation: Operation,
    menu: bool,
    interactive: bool,
    json: bool,
}
fn parse(args: Vec<String>) -> Result<Options> {
    let mut config = None;
    let mut operation = None;
    let (mut menu, mut interactive, mut json) = (false, false, false);
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config" if config.is_none() => {
                config =
                    Some(PathBuf::from(iter.next().ok_or_else(|| {
                        Error::new("ARGUMENT_ERROR", "missing config file")
                    })?))
            }
            "--operation" if operation.is_none() => {
                operation = Some(match iter.next().as_deref() {
                    Some("backup") => Operation::Backup,
                    Some("resume") => Operation::Resume,
                    Some("restore") => Operation::Restore,
                    Some("verify") => Operation::Verify,
                    _ => {
                        return Err(Error::new(
                            "ARGUMENT_ERROR",
                            "operation must be backup/resume/restore/verify",
                        ))
                    }
                });
            }
            "--menu" if !menu => {
                menu = true;
                interactive = true;
            }
            "--interactive" if !interactive => interactive = true,
            "--json" if !json => json = true,
            "--no-gui" => {}
            _ => {
                return Err(Error::new(
                    "ARGUMENT_ERROR",
                    format!("unknown/duplicate option {arg}"),
                ))
            }
        }
    }
    if menu && operation.is_some() {
        return Err(Error::new(
            "ARGUMENT_ERROR",
            "--menu and --operation are mutually exclusive",
        ));
    }
    Ok(Options {
        config: config.ok_or_else(|| Error::new("ARGUMENT_ERROR", "--config is required"))?,
        operation: operation.unwrap_or(Operation::Backup),
        menu,
        interactive,
        json,
    })
}
fn line(reader: &mut impl BufRead) -> Result<Option<String>> {
    let mut bytes = vec![];
    let count = reader.take(257).read_until(b'\n', &mut bytes)?;
    if count == 0 {
        return Ok(None);
    }
    if bytes.len() > 256 {
        return Err(Error::new(
            "INPUT_TOO_LONG",
            "at most 256 bytes per command",
        ));
    }
    let s = String::from_utf8(bytes)
        .map_err(|_| Error::new("INVALID_INPUT", "commands must be UTF-8"))?;
    Ok(Some(s.trim().to_owned()))
}
fn menu(direct: bool) -> Result<Operation> {
    perfect_backup_engine::supervisor::phase("MENU");
    eprintln!(
        "perfect backup\n  [B] バックアップ\n  [R] リストア\n入力待ち（カウントダウンなし）{}:",
        if direct {
            "・Enter不要"
        } else {
            "・Enterで送信"
        }
    );
    if direct {
        let mut keys = terminal::Keys::default();
        loop {
            if SIGNAL_CANCEL.load(Ordering::Relaxed) {
                return Err(Error::new("CANCELLED", "menu cancelled"));
            }
            match keys.read()? {
                terminal::Input::Key('b' | '1') => return Ok(Operation::Backup),
                terminal::Input::Key('r' | '2') => return Ok(Operation::Restore),
                terminal::Input::End => break,
                _ => {}
            }
        }
        return Err(Error::new(
            "MENU_INPUT_REQUIRED",
            "choose backup or restore explicitly",
        ));
    }
    match line(&mut std::io::stdin().lock())?
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("1" | "backup" | "b") => Ok(Operation::Backup),
        Some("2" | "restore" | "r") => Ok(Operation::Restore),
        _ => Err(Error::new(
            "MENU_INPUT_REQUIRED",
            "choose backup or restore explicitly",
        )),
    }
}
fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.is_empty() || args == ["--help"] {
        println!("perfect backup CUI (explicit configured source/target adapter)\n  perfect-backup --config FILE [--operation backup|resume|restore|verify]\n  perfect-backup --config FILE --menu\nOptions: --interactive (P=pause/resume, C=cancel; terminal keys need no Enter), --json, --no-gui\nPipes retain line commands: pause/resume/cancel + Enter.\nDefault: unattended backup, no countdown or required input. SIGINT/SIGTERM cancel cooperatively.\nRelative paths use the config directory. Files and whole devices use separate explicit configuration fields. No host poweroff.");
        return;
    }
    match perfect_backup_engine::supervisor::entry(perfect_backup_engine::supervisor::Mode::Cui) {
        Ok(Some(code)) => std::process::exit(code),
        Ok(None) => {}
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
    let result = parse(args).and_then(run);
    if let Err(error) = result {
        perfect_backup_engine::supervisor::failure(&error);
        eprintln!("{error}");
        std::process::exit(if error.code == "CANCELLED" { 130 } else { 1 });
    }
}
fn run(mut options: Options) -> Result<()> {
    let config = JobConfig::load(&options.config)?;
    perfect_backup_engine::supervisor::configure(&config.tuning.io_policy);
    // SAFETY: handler only sets a lock-free atomic. Install before entering direct terminal mode.
    unsafe {
        libc::signal(libc::SIGINT, cancel_signal as *const () as usize);
        libc::signal(libc::SIGTERM, cancel_signal as *const () as usize);
    }
    let terminal = terminal::Terminal::open(options.interactive)?;
    let direct = terminal.direct();
    if options.menu {
        options.operation = menu(direct)?;
    }
    let (sender, receiver) = mpsc::sync_channel::<JobEvent>(64);
    let (done_tx, done_rx) = mpsc::channel();
    let output = Arc::new(Output {
        sender,
        dropped: AtomicU64::new(0),
        paused: AtomicBool::new(false),
    });
    let json = options.json;
    std::thread::spawn(move || {
        let mut stdout = std::io::stdout().lock();
        for event in receiver {
            let written = if json {
                let mut value = serde_json::to_value(&event).expect("typed JobEvent serializes");
                value["schema_version"] = 1.into();
                serde_json::to_writer(&mut stdout, &value)
                    .map_err(std::io::Error::other)
                    .and_then(|_| stdout.write_all(b"\n"))
            } else {
                writeln!(stdout, "{}", presentation::text(&event))
            };
            if written.is_err() || stdout.flush().is_err() {
                break;
            }
        }
        let _ = done_tx.send(());
    });
    let controller = Arc::new(Controller::new(output.clone())?);
    // SAFETY: handler only sets a lock-free atomic; no locks, allocations or I/O.
    unsafe {
        libc::signal(libc::SIGINT, cancel_signal as *const () as usize);
        libc::signal(libc::SIGTERM, cancel_signal as *const () as usize);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let watch = {
        let controller = Arc::clone(&controller);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if SIGNAL_CANCEL.swap(false, Ordering::Relaxed) {
                    controller.cancel();
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    };
    let input_done = Arc::new(AtomicBool::new(false));
    let mut input_join = None;
    if options.interactive {
        eprintln!(
            "任意操作: [P] 一時停止／再開、[C] キャンセル（{}）。操作なしで処理は進みます。",
            if direct {
                "Enter不要"
            } else {
                "Enterで送信。pause/resume/cancelも利用可"
            }
        );
        let controller = Arc::clone(&controller);
        let output = Arc::clone(&output);
        let stop = Arc::clone(&stop);
        let input_done = Arc::clone(&input_done);
        input_join = Some(std::thread::spawn(move || {
            let mut reader = std::io::stdin().lock();
            let mut keys = terminal::Keys::default();
            while !stop.load(Ordering::Relaxed) {
                let read = if direct {
                    match keys.read() {
                        Ok(terminal::Input::Key(key)) => Ok(Some(key.to_string())),
                        Ok(terminal::Input::Timeout) => continue,
                        Ok(terminal::Input::Command(_)) => continue,
                        Ok(terminal::Input::End) => Ok(None),
                        Err(e) => Err(e.into()),
                    }
                } else {
                    line(&mut reader)
                };
                let input = match read {
                    Ok(Some(s)) => s,
                    Ok(None) => break,
                    Err(e) => {
                        output.emit(JobEvent::ControlRejected {
                            code: e.code.into(),
                            detail: e.detail,
                        });
                        break;
                    }
                };
                let action = match input.to_ascii_lowercase().as_str() {
                    "p" => Some(if output.paused.load(Ordering::Acquire) {
                        Action::Resume
                    } else {
                        Action::Pause
                    }),
                    "pause" => Some(Action::Pause),
                    "r" | "resume" => Some(Action::Resume),
                    "c" | "cancel" => Some(Action::Cancel),
                    "" => None,
                    _ => {
                        output.emit(JobEvent::ControlRejected {
                            code: "UNKNOWN_CONTROL".into(),
                            detail: "use p/r/c".into(),
                        });
                        None
                    }
                };
                if let Some(action) = action {
                    if action == Action::Cancel {
                        controller.cancel();
                    } else if let Err(e) = controller.request(action) {
                        output.emit(JobEvent::ControlRejected {
                            code: e.code.into(),
                            detail: e.detail,
                        });
                    }
                }
            }
            input_done.store(true, Ordering::Release);
        }));
    }
    let result = controller.run(&config, options.operation);
    stop.store(true, Ordering::Relaxed);
    let _ = watch.join();
    // A terminal reader may be waiting for input. It must not delay job completion.
    if direct || input_done.load(Ordering::Acquire) {
        if let Some(handle) = input_join {
            let _ = handle.join();
        }
    }
    drop(controller);
    // The interactive reader can retain a sender. Use bounded output draining either way.
    let dropped = output.dropped.load(Ordering::Relaxed);
    drop(output);
    let _ = done_rx.recv_timeout(Duration::from_millis(200));
    if dropped > 0 {
        eprintln!("診断イベント省略={dropped}。終了状態はjobレポートで確認できます。");
    }
    result.map(|_| ())
}
