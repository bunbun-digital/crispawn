// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! Optional local-terminal commands; pipes retain the bounded line protocol.
use std::io::{self, IsTerminal};

pub struct Terminal {
    original: Option<libc::termios>,
}
impl Terminal {
    pub fn open(enabled: bool) -> io::Result<Self> {
        if !enabled || !io::stdin().is_terminal() {
            return Ok(Self { original: None });
        }
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: stdin is a terminal and the pointer covers a writable termios.
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, original.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: tcgetattr completed successfully.
        let original = unsafe { original.assume_init() };
        let mut direct = original;
        direct.c_lflag &= !(libc::ICANON | libc::ECHO);
        direct.c_cc[libc::VMIN] = 1;
        direct.c_cc[libc::VTIME] = 0;
        // Keep ISIG: Ctrl+C remains a cooperative cancellation signal.
        // SAFETY: both terminal descriptor and termios value are valid.
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &direct) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            original: Some(original),
        })
    }
    pub fn direct(&self) -> bool {
        self.original.is_some()
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        if let Some(original) = &self.original {
            // SAFETY: restore the descriptor's saved mode without flushing input.
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, original);
            }
        }
    }
}
#[derive(Default)]
pub struct Keys {
    escape: u8,
    sequence: Vec<u8>,
    command: Option<crispawn_core::presentation::Key>,
}
pub enum Input {
    Key(char),
    Command(crispawn_core::presentation::Key),
    Timeout,
    End,
}
impl Keys {
    fn decode(&mut self, byte: u8) -> Option<char> {
        match self.escape {
            1 => {
                self.escape = if matches!(byte, b'[' | b'O') { 2 } else { 0 };
                self.sequence.clear();
                None
            }
            2 => {
                self.sequence.push(byte);
                if (0x40..=0x7e).contains(&byte) {
                    self.escape = 0;
                    use crispawn_core::presentation::Key;
                    self.command = match self.sequence.as_slice() {
                        b"5~" => Some(Key::PageUp),
                        b"6~" => Some(Key::PageDown),
                        b"F" | b"4~" => Some(Key::End),
                        _ => None,
                    };
                }
                if self.sequence.len() > 16 {
                    self.sequence.clear();
                    self.escape = 0;
                }
                None
            }
            _ if byte == 0x1b => {
                self.escape = 1;
                None
            }
            _ if byte.is_ascii_alphanumeric() => Some(char::from(byte.to_ascii_lowercase())),
            _ => None,
        }
    }
    pub fn read(&mut self) -> io::Result<Input> {
        let mut poll = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll references exactly one initialized pollfd, with a bounded wait.
        let ready = unsafe { libc::poll(&mut poll, 1, 100) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(Input::Timeout);
            }
            return Err(error);
        }
        if ready == 0 {
            let isolated = self.escape == 1;
            self.escape = 0; // An isolated Escape must not swallow the next command.
            if isolated {
                return Ok(Input::Command(crispawn_core::presentation::Key::Escape));
            }
            return Ok(Input::Timeout);
        }
        let mut byte = 0u8;
        // SAFETY: a single byte is writable; no other thread reads stdin.
        match unsafe { libc::read(libc::STDIN_FILENO, (&mut byte as *mut u8).cast(), 1) } {
            0 => Ok(Input::End),
            1 => {
                let key = self.decode(byte).map(Input::Key);
                Ok(key
                    .or_else(|| self.command.take().map(Input::Command))
                    .unwrap_or(Input::Timeout))
            }
            _ => {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    Ok(Input::Timeout)
                } else {
                    Err(error)
                }
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arrow_and_function_key_sequences_never_become_backup_or_cancel_commands() {
        let mut keys = Keys::default();
        for sequence in [
            b"\x1b[B".as_slice(),
            b"\x1b[C",
            b"\x1b[1;5C",
            b"\x1bOP",
            b"\x1bb",
        ] {
            assert!(sequence.iter().all(|&byte| keys.decode(byte).is_none()));
        }
        assert_eq!(keys.decode(b'B'), Some('b'));
        assert_eq!(keys.decode(b'P'), Some('p'));
        assert_eq!(keys.decode(b'\n'), None);
    }
}
