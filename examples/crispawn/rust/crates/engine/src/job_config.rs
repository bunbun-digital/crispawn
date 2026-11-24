// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
use crate::{io, Error, Result};
use crispawn_core::{error::require, plan::Plan};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TuningMode {
    Auto,
    /// Explicit legacy sampled experiments; never the normal unattended default.
    Probe,
    Fixed,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Tuning {
    pub mode: TuningMode,
    pub plan: Plan,
    pub budget_seconds: u32,
    pub max_trials: u32,
    pub profile: crate::profiles::Profile,
    pub verify_workers: Option<u32>,
    pub verify_after_write: bool,
    pub io_policy: crate::supervisor::Policy,
}
impl Default for Tuning {
    fn default() -> Self {
        Self {
            mode: TuningMode::Auto,
            plan: Plan {
                memory_limit_bytes: 4 << 30,
                ..Plan::current()
            },
            budget_seconds: 30,
            max_trials: 12,
            profile: crate::profiles::Profile::default(),
            verify_workers: None,
            verify_after_write: true,
            io_policy: crate::supervisor::Policy::default(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobConfig {
    pub schema_version: u32,
    pub source_file: Option<PathBuf>,
    pub source_device: Option<PathBuf>,
    pub archive: PathBuf,
    pub scratch_dir: PathBuf,
    pub restore_target_file: Option<PathBuf>,
    pub restore_target_device: Option<PathBuf>,
    pub report_file: Option<PathBuf>,
    #[serde(default)]
    pub tuning: Tuning,
}
impl JobConfig {
    /// Relative paths are relative to the configuration file, never the process cwd.
    pub fn load(path: &Path) -> Result<Self> {
        let base = path.canonicalize()?.parent().unwrap().to_path_buf();
        let mut config: Self = io::read_json(path)?;
        for path in [
            &mut config.source_file,
            &mut config.source_device,
            &mut config.restore_target_file,
            &mut config.restore_target_device,
            &mut config.report_file,
        ]
        .into_iter()
        .flatten()
        {
            resolve(path, &base)?;
        }
        resolve(&mut config.archive, &base)?;
        resolve(&mut config.scratch_dir, &base)?;
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&self) -> Result<()> {
        self.tuning.io_policy.validate()?;
        require(
            !(self.source_file.is_some() && self.source_device.is_some())
                && !(self.restore_target_file.is_some() && self.restore_target_device.is_some()),
            "INVALID_CONFIG",
            "choose exactly one adapter per source/target",
        )?;
        require(
            self.schema_version == 1,
            "UNKNOWN_VERSION",
            "job configuration",
        )?;
        require(
            (1..=120).contains(&self.tuning.budget_seconds)
                && (1..=16).contains(&self.tuning.max_trials)
                && self
                    .tuning
                    .verify_workers
                    .is_none_or(|v| (1..=32).contains(&v)),
            "INVALID_CONFIG",
            "probe budget 1..120 seconds, max_trials 1..16",
        )?;
        require(
            self.scratch_dir.is_dir(),
            "INVALID_CONFIG",
            "scratch_dir must exist",
        )?;
        require(
            parent(&self.archive).is_dir(),
            "INVALID_CONFIG",
            "archive parent must exist",
        )?;
        require(
            self.archive.file_name().is_some(),
            "INVALID_CONFIG",
            "archive must have a directory name",
        )?;
        if let Some(report) = &self.report_file {
            let archive_path = parent(&self.archive)
                .canonicalize()?
                .join(self.archive.file_name().unwrap());
            let report_parent = parent(report).canonicalize()?;
            let report_path = report_parent.join(
                report
                    .file_name()
                    .ok_or_else(|| Error::new("INVALID_CONFIG", "report must have a file name"))?,
            );
            require(
                report_path != archive_path && !report_parent.starts_with(archive_path),
                "INVALID_CONFIG",
                "job report must be outside the archive",
            )?;
        }
        Ok(())
    }
    pub fn source(&self) -> Result<&Path> {
        self.source_file
            .as_deref()
            .or(self.source_device.as_deref())
            .ok_or_else(|| {
                Error::new(
                    "INVALID_CONFIG",
                    "source_file or source_device is required for backup/resume",
                )
            })
    }
}
fn resolve(path: &mut PathBuf, base: &Path) -> Result<()> {
    require(!path.as_os_str().is_empty(), "INVALID_CONFIG", "empty path")?;
    if path.is_relative() {
        *path = base.join(&path);
    }
    Ok(())
}
pub(crate) fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}
