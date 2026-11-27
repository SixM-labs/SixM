//! Frame timing for the `perf` console command.
//!
//! The loop records the duration of every frame in a fixed ring buffer of the
//! last [`FRAME_HISTORY`] frames. Recording is a store and two integer updates;
//! the percentiles are computed only when `perf` asks for them.

use std::time::Duration;

/// Frames kept for the statistics.
pub(crate) const FRAME_HISTORY: usize = 1200;

/// The durations of the most recent frames, in microseconds.
#[derive(Debug)]
pub(crate) struct FrameHistory {
    micros: [u32; FRAME_HISTORY],
    /// Index of the slot the next frame is written to.
    next: usize,
    /// Frames recorded, at most [`FRAME_HISTORY`].
    count: usize,
}

impl Default for FrameHistory {
    fn default() -> Self {
        Self {
            micros: [0; FRAME_HISTORY],
            next: 0,
            count: 0,
        }
    }
}

/// Statistics of the recorded frames, all in microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FrameSummary {
    /// Frames the statistics cover.
    pub(crate) count: usize,
    /// Mean duration, rounded down.
    pub(crate) average: u64,
    /// Median duration.
    pub(crate) p50: u64,
    /// 99th percentile duration.
    pub(crate) p99: u64,
    /// Longest duration.
    pub(crate) max: u64,
}

impl FrameHistory {
    /// Records one frame that took `elapsed`. Durations beyond about 71 minutes
    /// are stored as the largest value a `u32` holds.
    pub(crate) fn record(&mut self, elapsed: Duration) {
        self.micros[self.next] = u32::try_from(elapsed.as_micros()).unwrap_or(u32::MAX);
        self.next = (self.next + 1) % FRAME_HISTORY;
        self.count = (self.count + 1).min(FRAME_HISTORY);
    }

    /// The statistics of the recorded frames, or `None` before the first frame.
    ///
    /// While fewer than [`FRAME_HISTORY`] frames are recorded they fill the
    /// slots from the start, so the recorded frames are `micros[..count]` in
    /// both states. The percentiles use the nearest-rank method on a sorted copy.
    pub(crate) fn summary(&self) -> Option<FrameSummary> {
        if self.count == 0 {
            return None;
        }
        let mut sorted = self.micros[..self.count].to_vec();
        sorted.sort_unstable();
        let total: u64 = sorted.iter().map(|&micros| u64::from(micros)).sum();
        let count = sorted.len();
        Some(FrameSummary {
            count,
            average: total / u64::try_from(count).unwrap_or(u64::MAX),
            p50: u64::from(nearest_rank(&sorted, 50)),
            p99: u64::from(nearest_rank(&sorted, 99)),
            max: u64::from(sorted[count - 1]),
        })
    }
}

/// The value at `percent` of a sorted, non-empty slice, by nearest rank.
fn nearest_rank(sorted: &[u32], percent: usize) -> u32 {
    let rank = (sorted.len() * percent).div_ceil(100).max(1);
    sorted[rank - 1]
}

/// Formats microseconds as milliseconds with two decimals, rounded to the
/// nearest hundredth.
pub(crate) fn format_millis(micros: u64) -> String {
    let hundredths = micros.saturating_add(5) / 10;
    format!("{}.{:02}", hundredths / 100, hundredths % 100)
}

/// `amount` per second over `millis` milliseconds, with one decimal, rounded.
/// `scale` divides `amount` first, so 1024 gives KiB. Uses integer arithmetic
/// only.
pub(crate) fn per_second(amount: u64, scale: u64, millis: u128) -> String {
    if millis == 0 {
        return "0.0".to_owned();
    }
    let divisor = u128::from(scale) * millis;
    let tenths = (u128::from(amount) * 10_000 + divisor / 2) / divisor;
    format!("{}.{}", tenths / 10, tenths % 10)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{FRAME_HISTORY, FrameHistory, format_millis, per_second};

    fn history_of(micros: impl Iterator<Item = u64>) -> FrameHistory {
        let mut history = FrameHistory::default();
        for value in micros {
            history.record(Duration::from_micros(value));
        }
        history
    }

    #[test]
    fn empty_history_has_no_summary() {
        assert_eq!(FrameHistory::default().summary(), None);
    }

    #[test]
    fn partial_history_uses_only_recorded_frames() {
        let summary = history_of([5, 1, 9].into_iter()).summary().unwrap();
        assert_eq!(summary.count, 3);
        assert_eq!(summary.average, 5);
        assert_eq!(summary.p50, 5);
        assert_eq!(summary.p99, 9);
        assert_eq!(summary.max, 9);
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let summary = history_of(1..=100).summary().unwrap();
        assert_eq!(summary.count, 100);
        assert_eq!(summary.average, 50);
        assert_eq!(summary.p50, 50);
        assert_eq!(summary.p99, 99);
        assert_eq!(summary.max, 100);
    }

    #[test]
    fn wrap_around_keeps_only_the_last_frames() {
        let total = FRAME_HISTORY as u64 + 100;
        let summary = history_of(1..=total).summary().unwrap();
        // The frames 101 to 1300 remain.
        assert_eq!(summary.count, FRAME_HISTORY);
        assert_eq!(summary.average, 700);
        assert_eq!(summary.p50, 700);
        assert_eq!(summary.p99, 1288);
        assert_eq!(summary.max, 1300);
    }

    #[test]
    fn huge_durations_saturate() {
        let mut history = FrameHistory::default();
        history.record(Duration::from_secs(100_000));
        assert_eq!(history.summary().unwrap().max, u64::from(u32::MAX));
    }

    #[test]
    fn milliseconds_round_to_two_decimals() {
        assert_eq!(format_millis(0), "0.00");
        assert_eq!(format_millis(1_500), "1.50");
        assert_eq!(format_millis(12_345), "12.35");
        assert_eq!(format_millis(49_996), "50.00");
    }

    #[test]
    fn rates_have_one_decimal() {
        assert_eq!(per_second(2048, 1024, 1_000), "2.0");
        assert_eq!(per_second(50, 1, 1_000), "50.0");
        assert_eq!(per_second(1536, 1024, 2_000), "0.8");
        assert_eq!(per_second(10, 1, 0), "0.0");
    }
}
