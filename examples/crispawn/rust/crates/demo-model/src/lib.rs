// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]

//! Display simulation only. No storage I/O or dependency on a GUI toolkit.

pub const TOTAL_BYTES: u64 = 512 * 1024 * 1024 * 1024;
pub const RATE_BYTES: u64 = 320 * 1024 * 1024;
pub const TIME_SCALE: u64 = 20;
pub const DURATION_MS: u64 = TOTAL_BYTES * 1000 / RATE_BYTES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Running,
    Paused,
    Complete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Progress {
    pub state: State,
    pub copied_bytes: u64,
    pub elapsed_ms: u64,
}

impl Default for Progress {
    fn default() -> Self {
        Self {
            state: State::Running,
            copied_bytes: 0,
            elapsed_ms: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub permille: u32,
    pub copied_bytes: u64,
    pub copied: String,
    pub elapsed: String,
    pub remaining: String,
    pub speed: String,
    pub status: &'static str,
    pub pause_caption: &'static str,
    pub pause_enabled: bool,
}

fn duration(seconds: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

impl Progress {
    pub fn advance(&mut self, wall_ms: u64) {
        if self.state != State::Running {
            return;
        }
        let elapsed = u128::from(self.elapsed_ms) + u128::from(wall_ms) * u128::from(TIME_SCALE);
        self.elapsed_ms = elapsed.min(u128::from(DURATION_MS)) as u64;
        self.copied_bytes = (u128::from(self.elapsed_ms) * u128::from(RATE_BYTES) / 1000)
            .min(u128::from(TOTAL_BYTES)) as u64;
        if self.elapsed_ms == DURATION_MS {
            self.state = State::Complete;
        }
    }

    pub fn toggle_pause(&mut self) {
        self.state = match self.state {
            State::Running => State::Paused,
            State::Paused => State::Running,
            State::Complete => State::Complete,
        };
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn snapshot(&self) -> Snapshot {
        let tenths = u128::from(self.copied_bytes) * 10 / (1024 * 1024 * 1024);
        Snapshot {
            permille: (u128::from(self.copied_bytes) * 1000 / u128::from(TOTAL_BYTES)) as u32,
            copied_bytes: self.copied_bytes,
            copied: format!("コピー済み {}.{} GiB / 512 GiB", tenths / 10, tenths % 10),
            elapsed: duration(self.elapsed_ms / 1000),
            remaining: duration((DURATION_MS - self.elapsed_ms).div_ceil(1000)),
            speed: format!(
                "{} MiB/s",
                if self.state == State::Running { 320 } else { 0 }
            ),
            status: match self.state {
                State::Running => "バックアップ中",
                State::Paused => "一時停止中",
                State::Complete => "完了（デモ）",
            },
            pause_caption: if self.state == State::Paused {
                "再開"
            } else {
                "一時停止"
            },
            pause_enabled: self.state != State::Complete,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_matches_the_existing_demo_at_ten_simulated_minutes() {
        let mut p = Progress::default();
        p.advance(30_000);
        let s = p.snapshot();
        assert_eq!(s.permille, 366);
        assert_eq!(s.copied, "コピー済み 187.5 GiB / 512 GiB");
        assert_eq!(s.elapsed, "00:10:00");
        assert_eq!(s.remaining, "00:17:19");
    }

    #[test]
    fn pause_does_not_count_time_and_resume_continues() {
        let mut p = Progress::default();
        p.advance(1000);
        p.toggle_pause();
        let paused = p.clone();
        p.advance(u64::MAX);
        assert_eq!(p, paused);
        assert_eq!(p.snapshot().speed, "0 MiB/s");
        p.toggle_pause();
        p.advance(1000);
        assert_eq!(p.elapsed_ms, 40_000);
    }

    #[test]
    fn completion_is_bounded_even_for_extreme_elapsed_time() {
        let mut p = Progress::default();
        p.advance(u64::MAX);
        assert_eq!(p.copied_bytes, TOTAL_BYTES);
        assert_eq!(p.snapshot().permille, 1000);
        assert_eq!(p.snapshot().remaining, "00:00:00");
        assert!(!p.snapshot().pause_enabled);
        p.toggle_pause();
        assert_eq!(p.state, State::Complete);
        p.reset();
        assert_eq!(p, Progress::default());
    }

    #[test]
    fn fragmented_ticks_preserve_the_same_state() {
        let mut one = Progress::default();
        one.advance(30_000);
        let mut many = Progress::default();
        for _ in 0..300 {
            many.advance(100);
        }
        assert_eq!(one, many);
    }
}
