// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
use std::{fmt, io};

#[derive(Debug)]
pub struct Error {
    pub code: &'static str,
    pub detail: String,
    io_errno: Option<i32>,
}
impl Error {
    pub fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
            io_errno: None,
        }
    }
    pub fn raw_os_error(&self) -> Option<i32> {
        self.io_errno
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}
impl std::error::Error for Error {}
impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        // Linux errno classifications remain available on the workspace MSRV;
        // newer ErrorKind storage variants are deliberately not required.
        #[cfg(target_os = "linux")]
        let code = match e.raw_os_error() {
            Some(28 | 122) => "DESTINATION_FULL",     // ENOSPC / EDQUOT
            Some(30) => "READ_ONLY_DESTINATION",      // EROFS
            Some(19 | 6) => "DEVICE_UNAVAILABLE",     // ENODEV / ENXIO
            Some(27) => "FILE_TOO_LARGE",             // EFBIG
            Some(24 | 23) => "FILE_DESCRIPTOR_LIMIT", // EMFILE / ENFILE
            Some(110) => "IO_TIMEOUT",                // ETIMEDOUT
            Some(12) => "MEMORY_BUDGET",              // ENOMEM
            _ => "IO_ERROR",
        };
        #[cfg(not(target_os = "linux"))]
        let code = "IO_ERROR";
        #[cfg(target_os = "linux")]
        let errno = e.raw_os_error().or_else(|| match e.kind() {
            io::ErrorKind::Interrupted => Some(4),
            io::ErrorKind::WouldBlock => Some(11),
            _ => None,
        });
        #[cfg(not(target_os = "linux"))]
        let errno = e.raw_os_error();
        Self {
            code,
            detail: e.to_string(),
            io_errno: errno,
        }
    }
}
pub type Result<T> = std::result::Result<T, Error>;
pub fn require(condition: bool, code: &'static str, detail: impl Into<String>) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(Error::new(code, detail))
    }
}
