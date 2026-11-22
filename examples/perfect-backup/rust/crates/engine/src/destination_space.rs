// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! Actual destination free-space guard; compression never needs a raw-size reservation.
use crate::{io, Error, Result};
use std::{
    path::Path,
    time::{Duration, Instant},
};

pub(crate) const MIN_FREE_BYTES: u64 = 1_000_000_000;

fn check_available(free: u64) -> Result<()> {
    if free < MIN_FREE_BYTES {
        return Err(Error::new(
            "DESTINATION_LOW_SPACE",
            format!("destination free_bytes={free}; minimum_free_bytes={MIN_FREE_BYTES}"),
        ));
    }
    Ok(())
}

/// A paused copy still checks destination space and accepts cancellation.
pub(crate) fn gate(
    report: &mut crate::Reporter<'_>,
    inner: &dyn crate::control::ExecutionControl,
    path: &Path,
    phase: &str,
) -> Result<()> {
    struct Paused<'a> {
        inner: &'a dyn crate::control::ExecutionControl,
        path: &'a Path,
    }
    impl crate::control::ExecutionControl for Paused<'_> {
        fn cancelled(&self) -> bool {
            self.inner.cancelled()
        }
        fn check_cancel(&self) -> Result<()> {
            self.inner.check_cancel()
        }
        fn pause_requested(&self) -> bool {
            self.inner.pause_requested()
        }
        fn wait_for_resume(&self) -> Result<()> {
            let mut space = Monitor::new(self.path)?;
            while self.inner.pause_requested() {
                self.inner.check_cancel()?;
                space.poll(self.path)?;
                std::thread::sleep(Duration::from_millis(50));
            }
            self.inner.check_cancel()
        }
    }
    report.gate(&Paused { inner, path }, phase)
}

pub(crate) fn check(path: &Path) -> Result<()> {
    check_available(io::available_bytes(path)?)
}

pub(crate) struct Monitor(Instant);
impl Monitor {
    pub(crate) fn new(path: &Path) -> Result<Self> {
        check(path)?;
        Ok(Self(Instant::now()))
    }
    pub(crate) fn poll(&mut self, path: &Path) -> Result<()> {
        if self.0.elapsed() >= Duration::from_secs(1) {
            check(path)?;
            self.0 = Instant::now();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decimal_gigabyte_boundary_without_source_size_or_ratio_assumptions() {
        for free in [0, MIN_FREE_BYTES - 1] {
            assert_eq!(
                check_available(free).unwrap_err().code,
                "DESTINATION_LOW_SPACE"
            );
        }
        for free in [MIN_FREE_BYTES, MIN_FREE_BYTES + 1, u64::MAX] {
            check_available(free).unwrap();
        }
    }
}
