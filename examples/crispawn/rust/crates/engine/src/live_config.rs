// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! Runtime wire format -> execution settings. No disk discovery, network or UI I/O.
//! Builder projects and credentials are deliberately not accepted as runtime files.
use crate::{io, live::Config, Error, Result};
use crispawn_core::{error::require, live::CommandAction, session::Session};
use serde::{de, Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::BTreeSet,
    fmt,
    ops::{Deref, DerefMut},
    path::Path,
};

pub const LIMIT: u64 = 65_536;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Interface {
    Gui,
    Console,
    Headless,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum Startup {
    AutoCopy { delay_seconds: u32 },
    Menu {},
    RestoreSelect {},
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum Volume {
    BootDiskData {},
    Uuid { uuid: String },
}

/// Only this resolved value reaches the existing controller. Legacy serialization
/// remains the recovery-cache format; the v2-only startup/UI/volume stay explicit.
#[derive(Debug, Clone)]
pub struct Settings {
    pub config: Config,
    pub version: u32,
    pub startup: Startup,
    pub interface: Interface,
    pub volume: Volume,
    pub network: crate::network_config::Network,
}
impl Deref for Settings {
    type Target = Config;
    fn deref(&self) -> &Config {
        &self.config
    }
}
impl DerefMut for Settings {
    fn deref_mut(&mut self) -> &mut Config {
        &mut self.config
    }
}
impl Settings {
    pub fn initialize_session(&self, session: &mut Session) -> Result<()> {
        match self.startup {
            Startup::AutoCopy { .. } => {}
            Startup::Menu {} => {
                session.action(CommandAction::MenuInteraction)?;
            }
            Startup::RestoreSelect {} => {
                session.action(CommandAction::RestoreMode)?;
            }
        }
        Ok(())
    }
    pub fn capability_names() -> Vec<String> {
        let mut names = vec![
            "sources.all_internal",
            "sources.selected_ids",
            "destination.usb_disk",
            "destination.usb_disk.uuid",
            "startup.auto_copy",
            "startup.menu",
            "startup.restore_select",
            "completion.poweroff",
            "completion.stay",
            "failure.poweroff",
            "failure.stay",
            "ui.gui",
            "ui.console",
            "ui.headless",
            "ui.lang.ja",
            "ui.lang.en",
            "logging.always",
            "logging.on_error",
            "logging.never",
            "processing.auto",
            "processing.verify_after_write",
            "processing.profile.balanced",
            "processing.profile.long_matches",
            "processing.profile.high_compression",
            "processing.profile.incompressible",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        names.sort();
        names
    }
}

// Original v2 groups are required. The additive failure policy is optional;
// older configurations retain automatic error poweroff. Every object is closed.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Runtime {
    kind: String,
    schema_version: u32,
    sources: Sources,
    destination: Destination,
    startup: Startup,
    completion: Completion,
    #[serde(default)]
    failure: Failure,
    ui: Ui,
    logging: Logging,
    processing: Processing,
    network: crate::network_config::Network,
    diagnostics: Diagnostics,
}
#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
enum Sources {
    AllInternal {},
    SelectedIds { ids: Vec<String> },
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Destination {
    UsbDisk {
        filesystem: String,
        volume: Volume,
        backup_directory: String,
    },
    Smb {
        server: String,
        share: String,
        backup_directory: String,
        authentication: Value,
    },
    S3 {
        endpoint: String,
        bucket: String,
        region: String,
        prefix: String,
        credential_ref: String,
    },
}
#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Completion {
    Poweroff { delay_seconds: u32 },
    Stay {},
}
#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Failure {
    Poweroff {},
    Stay {},
}
impl Default for Failure {
    fn default() -> Self {
        Self::Poweroff {}
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ui {
    mode: Interface,
    language: crispawn_core::presentation::Language,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Logging {
    policy: crate::live::LogPolicy,
    sync_interval_ms: u32,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Processing {
    mode: String,
    initial_profile: crate::profiles::Profile,
    #[serde(default = "enabled")]
    verify_after_write: bool,
    io_policy: IoPolicy,
}
fn enabled() -> bool {
    true
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IoPolicy {
    no_progress_seconds: u32,
    failure_grace_seconds: u32,
    shutdown_grace_seconds: u32,
    transient_retries: u32,
    retry_delay_ms: u32,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Diagnostics {
    probe_interval_ms: u32,
}

fn invalid(path: &'static str) -> Error {
    Error::new("INVALID_CONFIG", path)
}
fn check(condition: bool, path: &'static str) -> Result<()> {
    require(condition, "INVALID_CONFIG", path)
}

/// The subset with executable backends is explicit. Future SMB/S3/network
/// settings always fail before any connection, mount or copy is attempted.
impl Runtime {
    fn resolve(self) -> Result<Settings> {
        check(
            self.kind == "crispawn.live" && self.schema_version == 2,
            "kind/schema_version",
        )?;
        let ids = match self.sources {
            Sources::AllInternal {} => vec![],
            Sources::SelectedIds { ids } => {
                let mut seen = BTreeSet::new();
                check((1..=64).contains(&ids.len()), "sources.ids")?;
                for id in &ids {
                    stable_id(id)?;
                    check(seen.insert(id), "sources.ids")?;
                }
                ids
            }
        };
        let (volume, directory) = match self.destination {
            Destination::UsbDisk {
                filesystem,
                volume,
                backup_directory,
            } => {
                check(filesystem == "ntfs", "destination.filesystem")?;
                portable_path(&backup_directory)?;
                if let Volume::Uuid { uuid } = &volume {
                    check(
                        uuid.len() == 16
                            && uuid
                                .bytes()
                                .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b)),
                        "destination.volume.uuid",
                    )?;
                }
                (volume, backup_directory)
            }
            Destination::Smb {
                server,
                share,
                backup_directory,
                authentication,
            } => {
                // Do not retain values in errors, especially authentication data.
                drop((server, share, backup_directory, authentication));
                return Err(Error::new("UNSUPPORTED_CAPABILITY", "destination.smb"));
            }
            Destination::S3 {
                endpoint,
                bucket,
                region,
                prefix,
                credential_ref,
            } => {
                drop((endpoint, bucket, region, prefix, credential_ref));
                return Err(Error::new("UNSUPPORTED_CAPABILITY", "destination.s3"));
            }
        };
        self.network.validate()?;
        let delay = match self.startup {
            Startup::AutoCopy { delay_seconds } => {
                check(delay_seconds <= 300, "startup.delay_seconds")?;
                delay_seconds
            }
            _ => 0,
        };
        match self.ui.mode {
            Interface::Gui => {}
            Interface::Console => check(
                !matches!(self.startup, Startup::AutoCopy { .. }) || delay == 0,
                "startup.delay_seconds",
            )?,
            Interface::Headless => check(
                matches!(self.startup, Startup::AutoCopy { delay_seconds: 0 }),
                "startup.mode",
            )?,
        }
        let (poweroff, completion_seconds) = match self.completion {
            Completion::Poweroff { delay_seconds } => {
                check(delay_seconds <= 60, "completion.delay_seconds")?;
                (true, delay_seconds)
            }
            Completion::Stay {} => (false, Config::default().completion_seconds),
        };
        check(
            self.logging.sync_interval_ms <= 60_000,
            "logging.sync_interval_ms",
        )?;
        check(
            (100..=5000).contains(&self.diagnostics.probe_interval_ms),
            "diagnostics.probe_interval_ms",
        )?;
        check(self.processing.mode == "auto", "processing.mode")?;
        let p = self.processing.io_policy;
        let policy = crate::supervisor::Policy {
            no_progress_seconds: p.no_progress_seconds,
            failure_grace_seconds: p.failure_grace_seconds,
            shutdown_grace_seconds: p.shutdown_grace_seconds,
            transient_retries: p.transient_retries,
            retry_delay_ms: p.retry_delay_ms,
        };
        policy
            .validate()
            .map_err(|_| invalid("processing.io_policy"))?;
        let mut config = Config {
            source_ids: ids,
            backup_directory: directory,
            language: self.ui.language,
            gui: self.ui.mode == Interface::Gui,
            menu_seconds: delay,
            poweroff_on_success: poweroff,
            poweroff_on_failure: matches!(self.failure, Failure::Poweroff {}),
            completion_seconds,
            log_policy: self.logging.policy,
            log_sync_interval_ms: self.logging.sync_interval_ms,
            probe_interval_ms: self.diagnostics.probe_interval_ms,
            ..Config::default()
        };
        config.tuning.mode = crate::job_config::TuningMode::Auto;
        config.tuning.profile = self.processing.initial_profile;
        config.tuning.verify_after_write = self.processing.verify_after_write;
        config.tuning.io_policy = policy;
        config.validate().map_err(|_| invalid("runtime"))?;
        Ok(Settings {
            config,
            version: 2,
            startup: self.startup,
            interface: self.ui.mode,
            volume,
            network: self.network,
        })
    }
}

pub fn stable_id(value: &str) -> Result<()> {
    check(
        !value.is_empty()
            && value.len() <= 512
            && value.trim() == value
            && !value.chars().any(char::is_control),
        "sources.ids",
    )?;
    if let Some(wwid) = value.strip_prefix("wwid:") {
        check(!wwid.is_empty() && wwid.trim() == wwid, "sources.ids")
    } else if let Some(serial) = value.strip_prefix("serial:") {
        let (serial, ns) = serial
            .rsplit_once(":ns:")
            .ok_or_else(|| invalid("sources.ids"))?;
        check(
            !serial.is_empty()
                && serial.trim() == serial
                && (ns.is_empty()
                    || (ns.bytes().all(|b| b.is_ascii_digit())
                        && (ns.len() == 1 || !ns.starts_with('0')))),
            "sources.ids",
        )
    } else {
        Err(invalid("sources.ids"))
    }
}

pub fn portable_path(value: &str) -> Result<()> {
    check(
        !value.is_empty()
            && value.len() <= 512
            && !value
                .chars()
                .any(|c| c.is_control() || "\\<>:\"|?*".contains(c)),
        "destination.backup_directory",
    )?;
    for part in value.split('/') {
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        check(
            !part.is_empty()
                && part.len() <= 255
                && part != "."
                && part != ".."
                && !part.ends_with([' ', '.'])
                && !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                && !((stem.starts_with("COM") || stem.starts_with("LPT"))
                    && stem.len() == 4
                    && (b'1'..=b'9').contains(&stem.as_bytes()[3])),
            "destination.backup_directory",
        )?;
    }
    Ok(())
}

/// Bounded raw JSON validation retains duplicate keys and numeric token types.
/// serde's error text can contain input values: never forward it to diagnostics.
struct Seed<'a> {
    depth: usize,
    nodes: &'a mut usize,
}
impl<'de> de::DeserializeSeed<'de> for Seed<'_> {
    type Value = Value;
    fn deserialize<D: de::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<Value, D::Error> {
        *self.nodes += 1;
        if self.depth > 32 || *self.nodes > 32_768 {
            return Err(de::Error::custom("limit"));
        }
        deserializer.deserialize_any(self)
    }
}
impl<'de> de::Visitor<'de> for Seed<'_> {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("bounded configuration")
    }
    fn visit_bool<E: de::Error>(self, v: bool) -> std::result::Result<Value, E> {
        Ok(Value::Bool(v))
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Value, E> {
        Ok(v.into())
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Value, E> {
        Ok(v.into())
    }
    fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Value, E> {
        if v.contains('\0') {
            return Err(E::custom("NUL"));
        }
        Ok(v.into())
    }
    fn visit_unit<E: de::Error>(self) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_seq<A: de::SeqAccess<'de>>(self, mut a: A) -> std::result::Result<Value, A::Error> {
        let mut values = vec![];
        while let Some(v) = a.next_element_seed(Seed {
            depth: self.depth + 1,
            nodes: self.nodes,
        })? {
            values.push(v);
        }
        Ok(Value::Array(values))
    }
    fn visit_map<A: de::MapAccess<'de>>(self, mut a: A) -> std::result::Result<Value, A::Error> {
        let mut values = Map::new();
        while let Some(k) = a.next_key::<String>()? {
            *self.nodes += 1;
            if *self.nodes > 32_768 || k.contains('\0') || values.contains_key(&k) {
                return Err(de::Error::custom("duplicate/key/limit"));
            }
            let v = a.next_value_seed(Seed {
                depth: self.depth + 1,
                nodes: self.nodes,
            })?;
            values.insert(k, v);
        }
        Ok(Value::Object(values))
    }
}

pub fn parse(bytes: &[u8]) -> Result<Settings> {
    let value = parse_json(bytes)?;
    resolve(value)
}

pub fn parse_json(bytes: &[u8]) -> Result<Value> {
    require(
        !bytes.is_empty() && bytes.len() <= LIMIT as usize,
        "CONFIG_SIZE",
        "runtime",
    )?;
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    let mut nodes = 0;
    let value = de::DeserializeSeed::deserialize(
        Seed {
            depth: 0,
            nodes: &mut nodes,
        },
        &mut decoder,
    )
    .map_err(|_| Error::new("CONFIG_JSON_INVALID", "runtime"))?;
    decoder
        .end()
        .map_err(|_| Error::new("CONFIG_JSON_INVALID", "runtime"))?;
    Ok(value)
}

fn resolve(value: Value) -> Result<Settings> {
    let object = value.as_object().ok_or_else(|| invalid("runtime"))?;
    if object.contains_key("kind") {
        require(
            object.get("kind").and_then(Value::as_str) == Some("crispawn.live"),
            "CONFIG_KIND",
            "kind",
        )?;
        require(
            object.get("schema_version").and_then(Value::as_u64) == Some(2),
            "UNKNOWN_VERSION",
            "schema_version",
        )?;
        let runtime: Runtime = serde_json::from_value(value).map_err(|_| invalid("runtime"))?;
        runtime.resolve()
    } else {
        if let Some(version) = object.get("schema_version") {
            require(
                version.as_u64() == Some(1),
                "UNKNOWN_VERSION",
                "schema_version",
            )?;
        }
        // Legacy defaults, nullable optional fields, sampled/fixed tuning and
        // explicit unattended restore remain supported by their original DTO.
        let config: Config = serde_json::from_value(value).map_err(|_| invalid("runtime"))?;
        config
            .validate()
            .map_err(|e| Error::new(e.code, "runtime"))?;
        let startup = Startup::AutoCopy {
            delay_seconds: if config.gui { config.menu_seconds } else { 0 },
        };
        let interface = if config.gui {
            Interface::Gui
        } else {
            Interface::Console
        };
        Ok(Settings {
            config,
            version: 1,
            startup,
            interface,
            volume: Volume::BootDiskData {},
            network: crate::network_config::Network::None {},
        })
    }
}

pub fn read(path: &Path) -> Result<Settings> {
    read_file(path, false)
}
/// Host-side validate/describe must also accept ordinary editable input files;
/// the boot-time entry explicitly applies the private-file policy.
pub fn read_private(path: &Path) -> Result<Settings> {
    read_file(path, true)
}
fn read_file(path: &Path, private: bool) -> Result<Settings> {
    parse(&read_bytes(path, private, false)?)
}

pub fn read_bytes(path: &Path, private: bool, secret: bool) -> Result<Vec<u8>> {
    use std::os::unix::fs::MetadataExt;
    let file = io::open_regular(path, false).map_err(|e| Error::new(e.code, "runtime file"))?;
    let before = file.metadata()?;
    require(
        before.nlink() == 1
            && (!private
                || (before.uid() == unsafe { libc::geteuid() }
                    && before.mode() & if secret { 0o077 } else { 0o022 } == 0)),
        "UNSAFE_CONFIG_FILE",
        "runtime file ownership/mode/links",
    )?;
    require(
        before.len() > 0 && before.len() <= LIMIT,
        "CONFIG_SIZE",
        "runtime",
    )?;
    let mut bytes = vec![0; before.len() as usize];
    io::read_exact_at(&file, &mut bytes, 0, LIMIT)?;
    let after = file.metadata()?;
    require(
        before.len() == after.len()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec(),
        "FILE_CHANGED",
        "runtime",
    )?;
    Ok(bytes)
}
