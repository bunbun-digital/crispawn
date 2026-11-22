// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 BunBun Digital Technology Co.,Ltd.
//! Process readiness is separate from the storage engine. The OSS adapter has
//! no network operations. Other applications can implement this interface.
use crate::Result;
pub trait Readiness {
    fn poll(&mut self) -> Result<bool>;
}
pub struct Local;
impl Readiness for Local {
    fn poll(&mut self) -> Result<bool> {
        Ok(true)
    }
}
