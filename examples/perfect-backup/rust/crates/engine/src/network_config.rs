// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 BunBun Digital Technology Co.,Ltd.
//! Wire compatibility for the community edition; no connection implementation.
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum Network {
    None {},
    Auto {
        wifi_profiles: Vec<serde_json::Value>,
    },
    Ethernet {
        ipv4: serde_json::Value,
    },
    Wifi {
        country: String,
        ipv4: serde_json::Value,
        wifi_profile: serde_json::Value,
    },
}
impl Network {
    pub fn mode(&self) -> &'static str {
        match self {
            Self::None {} => "none",
            Self::Auto { .. } => "auto",
            Self::Ethernet { .. } => "ethernet",
            Self::Wifi { .. } => "wifi",
        }
    }
    pub fn validate(&self) -> Result<()> {
        if matches!(self, Self::None {}) {
            Ok(())
        } else {
            Err(Error::new(
                "UNSUPPORTED_CAPABILITY",
                "community edition has no network support",
            ))
        }
    }
}
