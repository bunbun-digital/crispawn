// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.is_empty() {
        match crispawn_engine::supervisor::entry(crispawn_engine::supervisor::Mode::Live) {
            Ok(Some(code)) => std::process::exit(code),
            Ok(None) => {}
            Err(error) => {
                eprintln!("PB_LIVE_FAILED {error}");
                std::process::exit(1);
            }
        }
    }
    let result = match args.as_slice() {
        [mode, root, pid] if mode == "observe-io" => {
            pid.parse::<u32>().map_err(|_| crispawn_engine::Error::new("ARGUMENT_ERROR", "invalid observer PID"))
                .and_then(|pid| crispawn_engine::io_trace::observe(std::path::Path::new(root), pid))
        }
        [mode] if mode == "persist-failure" => crispawn_engine::supervisor::persist_failure_child(),
        [mode, length, retries, delay] if mode == "inspect-ntfs" => {
            match (length.parse(), retries.parse(), delay.parse()) {
                (Ok(length), Ok(retries), Ok(delay)) => crispawn_engine::live::inspect_ntfs_child(length, retries, delay),
                _ => Err(crispawn_engine::Error::new("ARGUMENT_ERROR", "invalid read-only inspector arguments")),
            }
        }
        [] => crispawn_engine::live::run(),
        [mode, path] if mode == "watch" => {
            crispawn_engine::live::watch(std::path::Path::new(path))
        }
        [mode, path] if mode == "console" => {
            crispawn_engine::console::run(std::path::Path::new(path))
        }
        [mode, path] if mode == "validate-config" => crispawn_engine::live_config::read(std::path::Path::new(path)).map(|_| ()),
        [mode, path] if mode == "gui-enabled" => crispawn_engine::live_config::read(std::path::Path::new(path))
        .and_then(|config| {
            println!("{}", config.gui);
            Ok(())
        }),
        [mode, path] if mode == "describe-config" => crispawn_engine::live_config::read(std::path::Path::new(path)).and_then(|config| {
            let value = serde_json::json!({"schema_version":1,"runtime_version":config.version,"startup":config.startup,"ui_mode":config.interface,"volume":config.volume,"network_mode":config.network.mode(),"engine":config.config,"disk_or_network_io":false});
            println!("{value}");
            Ok(())
        }),
        [mode] if mode == "runtime-capabilities" => {
            println!("{}", serde_json::json!({"live_config_versions":[1,2],"capabilities":crispawn_engine::live_config::Settings::capability_names(),"release_signature_checked":false,"hardware_acceptance_verified":false}));
            Ok(())
        },
        _ => Err(crispawn_engine::Error::new(
            "ARGUMENT_ERROR",
            "crispawn-live [watch/console RUNTIME_DIRECTORY | validate-config/gui-enabled/describe-config FILE | runtime-capabilities]",
        )),
    };
    if let Err(e) = result {
        crispawn_engine::supervisor::failure(&e);
        eprintln!("PB_LIVE_FAILED {e}");
        std::process::exit(1);
    }
}
