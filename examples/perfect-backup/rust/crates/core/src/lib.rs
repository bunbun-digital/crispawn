// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! GUI-independent backup contracts. No filesystem, rendering, or device access.
pub mod candidates;
pub mod error;
pub mod format;
pub mod live;
pub mod locale;
pub mod plan;
pub mod presentation;
pub mod session;
pub mod types;
pub mod workflow;
pub use error::{Error, Result};
