// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! Development adapter: bounded sampled pipeline trials, including write/sync/verify.
//! Buffered regular-file measurements do not establish physical SSD/USB throughput.
use crate::{
    backup,
    control::ExecutionControl,
    io,
    job_config::{parent, JobConfig},
    Error, NoHooks, Result,
};
use crispawn_core::{
    candidates::{Candidate, CandidateGrid},
    error::require,
    plan::Plan,
    types::{self, Codec, EventSink, Snapshot, SourceIdentity},
    workflow::Estimate,
};
use serde::Serialize;
use std::{fs::File, io::Write, sync::Mutex, time::Instant};

const WINDOW: u64 = 16 << 20;
#[derive(Debug, Clone, Serialize)]
pub struct Window {
    pub offset_bytes: String,
    pub length_bytes: String,
    pub sha256: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct Trial {
    pub candidate_id: String,
    pub plan: Plan,
    pub elapsed_seconds: f64,
    pub copy_seconds: f64,
    pub verify_seconds: f64,
    pub stored_bytes: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct ProbeReport {
    pub measurement_kind: String,
    pub source: SourceIdentity,
    pub windows: Vec<Window>,
    pub sample_bytes: String,
    pub eligible_candidates: usize,
    pub trials: Vec<Trial>,
    pub unmeasured_candidate_ids: Vec<String>,
    pub selected_candidate_id: String,
    pub selected_plan: Plan,
    pub elapsed_seconds: f64,
    pub budget_seconds: u32,
    pub selection_rule: String,
    pub full_16m_frame_observed: bool,
}
impl ProbeReport {
    pub fn eta(&self, bytes: u64) -> Option<Estimate> {
        let trial = self
            .trials
            .iter()
            .find(|t| t.candidate_id == self.selected_candidate_id)?;
        estimate(
            trial.elapsed_seconds * bytes as f64 / self.sample_bytes.parse::<u64>().ok()? as f64,
        )
    }
    pub fn verify_estimate(&self, bytes: u64) -> f64 {
        self.trials
            .iter()
            .find(|t| t.candidate_id == self.selected_candidate_id)
            .map(|t| {
                t.verify_seconds * bytes as f64
                    / self.sample_bytes.parse::<u64>().unwrap_or(1) as f64
            })
            .unwrap_or(0.0)
    }
}
pub fn estimate(seconds: f64) -> Option<Estimate> {
    if !seconds.is_finite() || seconds < 0.0 || seconds > (u64::MAX / 4) as f64 {
        return None;
    }
    Some(Estimate {
        seconds: seconds.ceil() as u64,
        lower_seconds: (seconds * 0.5).floor() as u64,
        upper_seconds: (seconds * 2.0).ceil() as u64,
    })
}
/// Recovered bytes must not be charged to the new run's measured time.
pub fn remaining(
    elapsed: f64,
    total: u64,
    done: u64,
    baseline: u64,
    reserve: f64,
) -> Option<Estimate> {
    if done <= baseline || elapsed <= 0.0 || done > total || reserve < 0.0 {
        return None;
    }
    estimate(elapsed * (total - done) as f64 / (done - baseline) as f64 + reserve)
}
/// Non-overlapping, evenly spread windows; total memory is one <=16 MiB window.
pub fn window_ranges(bytes: u64) -> Result<Vec<(u64, u64)>> {
    require(bytes > 0, "EMPTY_SOURCE", "probe source")?;
    if bytes <= 3 * WINDOW {
        return Ok(vec![(0, bytes)]);
    }
    let span = bytes - WINDOW;
    Ok(vec![
        (0, WINDOW),
        ((span / 2 / 512) * 512, WINDOW),
        (span, WINDOW),
    ])
}
fn trial_candidates(template: &Plan, bytes: u64) -> Result<Vec<Candidate>> {
    let eligible: Vec<_> = CandidateGrid::default()
        .evaluate(template, bytes)?
        .into_iter()
        .filter(Candidate::is_eligible)
        .collect();
    // Every family gets an early trial. Keep xz6/C16 for long-distance matches.
    // Later trials exercise both C, all B, and increasing worker parallelism.
    let order = [
        (Codec::Raw, 0, 8, 1024, 1),
        (Codec::Zstd, 3, 8, 1024, 2),
        (Codec::Xz, 0, 8, 1024, 1),
        (Codec::Gzip, 1, 8, 1024, 1),
        (Codec::Xz, 6, 16, 1024, 1),
        (Codec::Raw, 0, 8, 256, 1),
        (Codec::Raw, 0, 8, 4096, 1),
        (Codec::Raw, 0, 8, 1024, 2),
        (Codec::Raw, 0, 8, 1024, 4),
        (Codec::Raw, 0, 8, 1024, 8),
        (Codec::Zstd, 3, 16, 1024, 2),
        (Codec::Zstd, 1, 8, 1024, 4),
        (Codec::Gzip, 6, 16, 1024, 1),
        (Codec::Xz, 3, 16, 1024, 1),
        (Codec::Zstd, 6, 16, 4096, 1),
        (Codec::Raw, 0, 16, 1024, 2),
    ];
    let mut ordered = vec![];
    for (codec, level, c, b, p) in order {
        if let Some(candidate) = eligible.iter().find(|v| {
            v.plan.codec == codec
                && v.plan.compression_level == level
                && v.plan.range_bytes == c << 20
                && v.plan.read_bytes == b << 10
                && v.plan.workers == p
        }) {
            ordered.push(candidate.clone());
        }
    }
    require(
        !ordered.is_empty(),
        "NO_FEASIBLE_CANDIDATE",
        "no probe candidate fits the configured limits",
    )?;
    Ok(ordered)
}
/// Near ties (2%) favor a smaller conservative memory bound, then stable order.
pub fn select_trial(trials: &[Trial]) -> Result<usize> {
    require(
        !trials.is_empty()
            && trials
                .iter()
                .all(|t| t.elapsed_seconds.is_finite() && t.elapsed_seconds > 0.0),
        "INVALID_MEASUREMENT",
        "empty/nonpositive/nonfinite trial",
    )?;
    let fastest = trials
        .iter()
        .map(|t| t.elapsed_seconds)
        .fold(f64::INFINITY, f64::min);
    Ok(trials
        .iter()
        .enumerate()
        .filter(|(_, t)| t.elapsed_seconds <= fastest * 1.02)
        .min_by_key(|(i, t)| (t.plan.memory_bound(), *i))
        .unwrap()
        .0)
}
struct TrialEvents {
    verify_at: Mutex<Option<f64>>,
}
impl EventSink for TrialEvents {
    fn emit(&self, s: Snapshot) {
        if s.phase == "VERIFYING" && s.code == "VERIFY_STARTED" {
            *self.verify_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(s.elapsed_seconds);
        }
    }
}
pub fn run(
    config: &JobConfig,
    source: &File,
    identity: &SourceIdentity,
    control: &dyn ExecutionControl,
    on_trial: &dyn Fn(usize, usize, &Candidate, Option<&Trial>),
) -> Result<ProbeReport> {
    let start = Instant::now();
    let mut template = config.tuning.plan.clone();
    template.format_version = 2;
    let eligible = CandidateGrid::default().evaluate(&template, identity.bytes)?;
    let ordered = trial_candidates(&template, identity.bytes)?;
    let temporary = tempfile::Builder::new()
        .prefix(".crispawn-probe-")
        .tempdir_in(parent(&config.archive))?;
    let sample = temporary.path().join("sample.img");
    let mut file = io::create_new(&sample)?;
    let mut windows = vec![];
    let mut sample_bytes = 0;
    for (offset, length) in window_ranges(identity.bytes)? {
        // Small files are copied in bounded chunks as well.
        let mut position = 0;
        while position < length {
            control.check_cancel()?;
            let n = (length - position).min(WINDOW);
            let mut bytes = vec![0; n as usize];
            io::read_exact_at(
                source,
                &mut bytes,
                offset + position,
                config.tuning.plan.read_bytes.max(1),
            )?;
            windows.push(Window {
                offset_bytes: (offset + position).to_string(),
                length_bytes: n.to_string(),
                sha256: types::hex(&types::hash(&bytes)),
            });
            file.write_all(&bytes)?;
            sample_bytes += n;
            position += n;
        }
    }
    file.sync_all()?;
    io::unchanged(source, identity)?;
    let mut trials = vec![];
    for candidate in ordered.iter().take(config.tuning.max_trials as usize) {
        control.check_cancel()?;
        // A trial is never cut in the middle of an encode. Record the soft budget.
        if !trials.is_empty()
            && start.elapsed().as_secs_f64() >= f64::from(config.tuning.budget_seconds)
        {
            break;
        }
        candidate.plan.validate(sample_bytes)?;
        on_trial(
            trials.len(),
            ordered.len().min(config.tuning.max_trials as usize),
            candidate,
            None,
        );
        let trial_dir = tempfile::Builder::new()
            .prefix("trial-")
            .tempdir_in(temporary.path())?;
        let archive = trial_dir.path().join("archive");
        let events = TrialEvents {
            verify_at: Mutex::new(None),
        };
        let before = Instant::now();
        let verified = backup::backup(
            &sample,
            &archive,
            candidate.plan.clone(),
            &config.scratch_dir,
            &events,
            &NoHooks,
            control,
        )?;
        require(
            verified.dataset_committed,
            "PROBE_UNVERIFIED",
            "pipeline probe did not commit verified data",
        )?;
        let seconds = before.elapsed().as_secs_f64();
        let copy_seconds = events
            .verify_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .unwrap_or(seconds)
            .min(seconds);
        let stored: u64 = std::fs::read_dir(archive.join("data"))?
            .try_fold(0u64, |sum, entry| -> std::io::Result<u64> {
                Ok(sum + entry?.metadata()?.len())
            })?;
        let trial = Trial {
            candidate_id: candidate.id.clone(),
            plan: candidate.plan.clone(),
            elapsed_seconds: seconds,
            copy_seconds,
            verify_seconds: (seconds - copy_seconds).max(0.0),
            stored_bytes: stored.to_string(),
        };
        on_trial(
            trials.len(),
            ordered.len().min(config.tuning.max_trials as usize),
            candidate,
            Some(&trial),
        );
        trials.push(trial);
    }
    io::unchanged(source, identity)?;
    let selected = &trials[select_trial(&trials)?];
    let report = ProbeReport {
        measurement_kind: "regular-file-buffered-sampled-pipeline".into(),
        source: identity.clone(), windows, sample_bytes: sample_bytes.to_string(),
        eligible_candidates: eligible.iter().filter(|c| c.is_eligible()).count(),
        unmeasured_candidate_ids: eligible.into_iter().filter(|c| c.is_eligible() && !trials.iter().any(|t| t.candidate_id == c.id)).map(|c| c.id).collect(),
        selected_candidate_id: selected.candidate_id.clone(), selected_plan: selected.plan.clone(),
        trials, elapsed_seconds: start.elapsed().as_secs_f64(), budget_seconds: config.tuning.budget_seconds,
        selection_rule: "minimum measured pipeline seconds; within 2% prefer smaller conservative memory bound; no statistical guarantee".into(),
        full_16m_frame_observed: sample_bytes >= WINDOW,
    };
    // Only this job's temporary tree is removed. Cleanup failure fails the job.
    temporary
        .close()
        .map_err(|e| Error::new("PROBE_CLEANUP_FAILED", e.to_string()))?;
    Ok(report)
}
