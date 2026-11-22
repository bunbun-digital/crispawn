// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! Bounded, deterministic candidate generation. Does not benchmark or select a winner.
use crate::{error::require, plan::Plan, types::Codec, Error, Result};
use serde::Serialize;
use std::collections::BTreeSet;

pub const MAX_CANDIDATES: usize = 4096;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CodecSetting {
    pub codec: Codec,
    pub level: i32,
}
pub fn all_supported_codec_settings() -> Vec<CodecSetting> {
    let mut settings = vec![CodecSetting {
        codec: Codec::Raw,
        level: 0,
    }];
    for (codec, first, last) in [(Codec::Zstd, 1, 9), (Codec::Xz, 0, 6), (Codec::Gzip, 0, 9)] {
        settings.extend((first..=last).map(|level| CodecSetting { codec, level }));
    }
    settings
}
#[derive(Debug, Clone)]
pub struct CandidateGrid {
    pub read_bytes: Vec<u64>,
    pub range_bytes: Vec<u64>,
    pub workers: Vec<u32>,
    pub codecs: Vec<CodecSetting>,
}
impl Default for CandidateGrid {
    fn default() -> Self {
        Self {
            read_bytes: vec![256 << 10, 1 << 20, 4 << 20],
            range_bytes: vec![8 << 20, 16 << 20],
            workers: vec![1, 2, 4, 8],
            codecs: vec![
                CodecSetting {
                    codec: Codec::Raw,
                    level: 0,
                },
                CodecSetting {
                    codec: Codec::Zstd,
                    level: 1,
                },
                CodecSetting {
                    codec: Codec::Zstd,
                    level: 3,
                },
                CodecSetting {
                    codec: Codec::Zstd,
                    level: 6,
                },
                CodecSetting {
                    codec: Codec::Xz,
                    level: 0,
                },
                CodecSetting {
                    codec: Codec::Xz,
                    level: 1,
                },
                CodecSetting {
                    codec: Codec::Xz,
                    level: 3,
                },
                CodecSetting {
                    codec: Codec::Xz,
                    level: 6,
                },
                CodecSetting {
                    codec: Codec::Gzip,
                    level: 1,
                },
                CodecSetting {
                    codec: Codec::Gzip,
                    level: 6,
                },
            ],
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Eligibility {
    Eligible,
    Rejected { code: &'static str, detail: String },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Candidate {
    pub id: String,
    pub plan: Plan,
    pub eligibility: Eligibility,
}
impl Candidate {
    pub fn is_eligible(&self) -> bool {
        matches!(self.eligibility, Eligibility::Eligible)
    }
}
impl CandidateGrid {
    pub fn evaluate(&self, template: &Plan, source_bytes: u64) -> Result<Vec<Candidate>> {
        require(
            source_bytes > 0,
            "EMPTY_SOURCE",
            "candidate generation requires nonzero source size",
        )?;
        let lengths = [
            self.read_bytes.len(),
            self.range_bytes.len(),
            self.workers.len(),
            self.codecs.len(),
        ];
        require(
            lengths.iter().all(|n| *n > 0),
            "EMPTY_CANDIDATE_DIMENSION",
            "all candidate axes must be nonempty",
        )?;
        let count = lengths
            .into_iter()
            .try_fold(1usize, |total, n| total.checked_mul(n))
            .ok_or_else(|| Error::new("CANDIDATE_LIMIT", "candidate cardinality overflow"))?;
        require(
            count <= MAX_CANDIDATES,
            "CANDIDATE_LIMIT",
            "at most 4096 candidates per grid",
        )?;
        require(
            unique(self.read_bytes.iter().copied())
                && unique(self.range_bytes.iter().copied())
                && unique(self.workers.iter().copied())
                && unique(self.codecs.iter().map(|s| (s.codec.id(), s.level))),
            "DUPLICATE_CANDIDATE_DIMENSION",
            "duplicate axis value",
        )?;
        let mut cases = Vec::with_capacity(count);
        for &read_bytes in &self.read_bytes {
            for &range_bytes in &self.range_bytes {
                for &workers in &self.workers {
                    for &setting in &self.codecs {
                        let mut plan = template.clone();
                        plan.read_bytes = read_bytes;
                        plan.range_bytes = range_bytes;
                        plan.workers = workers;
                        plan.codec = setting.codec;
                        plan.compression_level = setting.level;
                        let eligibility = match plan.validate(source_bytes) {
                            Ok(()) => Eligibility::Eligible,
                            Err(error) => Eligibility::Rejected {
                                code: error.code,
                                detail: error.detail,
                            },
                        };
                        cases.push(Candidate {
                            id: format!(
                                "b{read_bytes}-c{range_bytes}-p{workers}-{}-{}",
                                setting.codec.name(),
                                setting.level
                            ),
                            plan,
                            eligibility,
                        });
                    }
                }
            }
        }
        Ok(cases)
    }
}
fn unique<T: Ord>(values: impl Iterator<Item = T>) -> bool {
    let mut seen = BTreeSet::new();
    values.into_iter().all(|value| seen.insert(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_grid_has_all_240_distinct_combinations_and_explicit_rejections() {
        let cases = CandidateGrid::default()
            .evaluate(&Plan::default(), 32 * 1024 * 1024 + 513)
            .unwrap();
        assert_eq!(cases.len(), 240);
        assert_eq!(cases.iter().filter(|c| c.is_eligible()).count(), 144);
        assert_eq!(
            cases.iter().map(|c| &c.id).collect::<BTreeSet<_>>().len(),
            240
        );
        for case in &cases {
            assert!([256 << 10, 1 << 20, 4 << 20].contains(&case.plan.read_bytes));
            assert!([8 << 20, 16 << 20].contains(&case.plan.range_bytes));
            assert!([1, 2, 4, 8].contains(&case.plan.workers));
            if let Eligibility::Rejected { code, .. } = case.eligibility {
                assert_eq!(code, "MEMORY_BUDGET");
            }
        }
        let larger = Plan {
            memory_limit_bytes: 2 << 30,
            ..Plan::default()
        };
        let all = CandidateGrid::default()
            .evaluate(&larger, 32 * 1024 * 1024 + 513)
            .unwrap();
        assert_eq!(all.len(), 240);
        assert!(all.iter().all(Candidate::is_eligible));
    }
    #[test]
    fn custom_grid_preserves_non_axis_settings() {
        let grid = CandidateGrid {
            read_bytes: vec![512, 777],
            range_bytes: vec![4096],
            workers: vec![1],
            codecs: vec![
                CodecSetting {
                    codec: Codec::Raw,
                    level: 0,
                },
                CodecSetting {
                    codec: Codec::Zstd,
                    level: 3,
                },
            ],
        };
        let template = Plan {
            container_bytes: 65536,
            checkpoint_records: 2,
            regions: 3,
            ..Plan::default()
        };
        let cases = grid.evaluate(&template, 65536).unwrap();
        assert_eq!(cases.len(), 4);
        assert!(cases.iter().all(Candidate::is_eligible));
        for case in cases {
            assert_eq!(case.plan.container_bytes, 65536);
            assert_eq!(case.plan.checkpoint_records, 2);
            assert_eq!(case.plan.regions, 3);
        }
    }
    #[test]
    fn empty_duplicate_and_excessive_grids_fail_before_expansion() {
        let mut grid = CandidateGrid::default();
        grid.read_bytes.clear();
        assert_eq!(
            grid.evaluate(&Plan::default(), 1).unwrap_err().code,
            "EMPTY_CANDIDATE_DIMENSION"
        );
        grid = CandidateGrid::default();
        grid.workers.push(1);
        assert_eq!(
            grid.evaluate(&Plan::default(), 1).unwrap_err().code,
            "DUPLICATE_CANDIDATE_DIMENSION"
        );
        grid = CandidateGrid {
            read_bytes: (1..=65).collect(),
            range_bytes: (1..=65).collect(),
            workers: vec![1],
            codecs: vec![CodecSetting {
                codec: Codec::Raw,
                level: 0,
            }],
        };
        assert_eq!(
            grid.evaluate(&Plan::default(), 1).unwrap_err().code,
            "CANDIDATE_LIMIT"
        );
        assert_eq!(
            CandidateGrid::default()
                .evaluate(&Plan::default(), 0)
                .unwrap_err()
                .code,
            "EMPTY_SOURCE"
        );
    }
    #[test]
    fn invalid_settings_remain_visible_and_never_become_raw_fallback_candidates() {
        let grid = CandidateGrid {
            read_bytes: vec![0, 512, 8192],
            range_bytes: vec![4096],
            workers: vec![1],
            codecs: vec![
                CodecSetting {
                    codec: Codec::Zero,
                    level: 0,
                },
                CodecSetting {
                    codec: Codec::Xz,
                    level: 7,
                },
            ],
        };
        let cases = grid.evaluate(&Plan::default(), 10000).unwrap();
        assert_eq!(cases.len(), 6);
        assert!(cases.iter().all(|c| matches!(
            c.eligibility,
            Eligibility::Rejected {
                code: "INVALID_PLAN",
                ..
            }
        )));
    }
    #[test]
    fn supported_codec_level_catalog_matches_v1_limits() {
        let all = all_supported_codec_settings();
        assert_eq!(all.len(), 27);
        assert_eq!(
            all.iter()
                .map(|s| (s.codec.id(), s.level))
                .collect::<BTreeSet<_>>()
                .len(),
            27
        );
        for setting in all {
            let p = Plan {
                codec: setting.codec,
                compression_level: setting.level,
                workers: 1,
                ..Plan::default()
            };
            p.validate(1).unwrap();
            assert_ne!(setting.codec, Codec::Zero);
        }
    }
}
