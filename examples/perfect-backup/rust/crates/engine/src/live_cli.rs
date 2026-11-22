// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.is_empty() {
        match perfect_backup_engine::supervisor::entry(
            perfect_backup_engine::supervisor::Mode::Live,
        ) {
            Ok(Some(code)) => std::process::exit(code),
            Ok(None) => {}
            Err(error) => {
                eprintln!("PB_LIVE_FAILED {error}");
                std::process::exit(1);
            }
        }
    }
    let result = match args.as_slice() {
        [] => perfect_backup_engine::live::run(),
        [mode, path] if mode == "watch" => {
            perfect_backup_engine::live::watch(std::path::Path::new(path))
        }
        [mode, path] if mode == "console" => {
            perfect_backup_engine::console::run(std::path::Path::new(path))
        }
        [mode, path] if mode == "validate-config" => perfect_backup_engine::live_config::read(std::path::Path::new(path)).map(|_| ()),
        [mode, path] if mode == "gui-enabled" => perfect_backup_engine::live_config::read(std::path::Path::new(path))
        .and_then(|config| {
            println!("{}", config.gui);
            Ok(())
        }),
        [mode, path] if mode == "describe-config" => perfect_backup_engine::live_config::read(std::path::Path::new(path)).and_then(|config| {
            let value = serde_json::json!({"schema_version":1,"runtime_version":config.version,"startup":config.startup,"ui_mode":config.interface,"volume":config.volume,"network_mode":config.network.mode(),"engine":config.config,"disk_or_network_io":false});
            println!("{value}");
            Ok(())
        }),
        [mode] if mode == "runtime-capabilities" => {
            println!("{}", serde_json::json!({"live_config_versions":[1,2],"capabilities":perfect_backup_engine::live_config::Settings::capability_names(),"release_signature_checked":false,"hardware_acceptance_verified":false}));
            Ok(())
        },
        _ => Err(perfect_backup_engine::Error::new(
            "ARGUMENT_ERROR",
            "perfect-backup-live [watch/console RUNTIME_DIRECTORY | validate-config/gui-enabled/describe-config FILE | runtime-capabilities]",
        )),
    };
    if let Err(e) = result {
        perfect_backup_engine::supervisor::failure(&e);
        eprintln!("PB_LIVE_FAILED {e}");
        std::process::exit(1);
    }
}
