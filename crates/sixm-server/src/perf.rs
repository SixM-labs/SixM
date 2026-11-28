//! Frame timing for the `perf` console command and per-resource live
//! monitoring (`resmon`).
//!
//! The loop records the duration of every frame in a fixed ring buffer of the
//! last [`FRAME_HISTORY`] frames, and each resource's tick duration and event
//! counts in a fixed [`RESMON_WINDOW`]-frame ring buffer. Recording is a store
//! and integer updates with zero heap allocations; statistics are computed only
//! when `perf`, `resmon`, or `/resmon.json` asks for them.

use std::cell::Cell;
use std::time::Duration;

/// Frames kept for the statistics.
pub(crate) const FRAME_HISTORY: usize = 1200;
/// Frames kept for per-resource live monitoring (`resmon`).
pub(crate) const RESMON_WINDOW: usize = 60;

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

/// Snapshot of one running resource's recent performance and memory telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResourceResmonStats {
    pub(crate) samples: usize,
    pub(crate) avg_tick_micros: u64,
    pub(crate) max_tick_micros: u32,
    pub(crate) last_tick_micros: u32,
    pub(crate) window_events: u64,
    pub(crate) total_events: u64,
    pub(crate) heap_bytes: usize,
}

/// Lock-free, zero-allocation fixed ring buffer of per-resource tick durations
/// and event handler invocations over the last [`RESMON_WINDOW`] frames.
#[derive(Debug)]
pub(crate) struct ResourceMonitor {
    tick_micros: [Cell<u32>; RESMON_WINDOW],
    event_counts: [Cell<u32>; RESMON_WINDOW],
    head: Cell<usize>,
    filled: Cell<usize>,
    pending_events: Cell<u32>,
    total_events: Cell<u64>,
}

impl Default for ResourceMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceMonitor {
    /// Creates an empty monitor with all ring-buffer slots zeroed.
    pub(crate) const fn new() -> Self {
        Self {
            tick_micros: [const { Cell::new(0) }; RESMON_WINDOW],
            event_counts: [const { Cell::new(0) }; RESMON_WINDOW],
            head: Cell::new(0),
            filled: Cell::new(0),
            pending_events: Cell::new(0),
            total_events: Cell::new(0),
        }
    }

    /// Accumulates `count` event handler invocations with saturating arithmetic.
    pub(crate) fn add_events(&self, count: u64) {
        if count == 0 {
            return;
        }
        let add_u32 = u32::try_from(count).unwrap_or(u32::MAX);
        self.pending_events
            .set(self.pending_events.get().saturating_add(add_u32));
        self.total_events
            .set(self.total_events.get().saturating_add(count));
    }

    /// Records one frame tick duration and flushes pending events into the ring buffer.
    pub(crate) fn record_frame(&self, tick: Duration) {
        let micros = u32::try_from(tick.as_micros()).unwrap_or(u32::MAX);
        let slot = self.head.get();
        self.tick_micros[slot].set(micros);
        self.event_counts[slot].set(self.pending_events.replace(0));
        self.head.set((slot + 1) % RESMON_WINDOW);
        self.filled.set((self.filled.get() + 1).min(RESMON_WINDOW));
    }

    /// Computes summary statistics across the recorded window plus `heap_bytes`.
    pub(crate) fn stats(&self, heap_bytes: usize) -> ResourceResmonStats {
        let samples = self.filled.get();
        let mut sum_micros: u64 = 0;
        let mut max_tick_micros: u32 = 0;
        let mut window_events: u64 = u64::from(self.pending_events.get());
        for i in 0..samples {
            let tick = self.tick_micros[i].get();
            sum_micros = sum_micros.saturating_add(u64::from(tick));
            max_tick_micros = max_tick_micros.max(tick);
            window_events = window_events.saturating_add(u64::from(self.event_counts[i].get()));
        }
        let avg_tick_micros = if samples == 0 {
            0
        } else {
            sum_micros / u64::try_from(samples).unwrap_or(u64::MAX)
        };
        let last_tick_micros = if samples == 0 {
            0
        } else {
            let last_slot = (self.head.get() + RESMON_WINDOW - 1) % RESMON_WINDOW;
            self.tick_micros[last_slot].get()
        };
        ResourceResmonStats {
            samples,
            avg_tick_micros,
            max_tick_micros,
            last_tick_micros,
            window_events,
            total_events: self.total_events.get(),
            heap_bytes,
        }
    }
}

/// Formats microseconds as milliseconds with two decimals, rounded to the
/// nearest hundredth.
pub(crate) fn format_millis(micros: u64) -> String {
    let hundredths = micros.saturating_add(5) / 10;
    format!("{}.{:02}", hundredths / 100, hundredths % 100)
}

/// Formats a byte count in KiB with one decimal, rounded.
pub(crate) fn format_kib(bytes: usize) -> String {
    let tenths = (u128::try_from(bytes).unwrap_or(0) * 10 + 512) / 1024;
    format!("{}.{}", tenths / 10, tenths % 10)
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

    use super::{
        FRAME_HISTORY, FrameHistory, RESMON_WINDOW, ResourceMonitor, format_kib, format_millis,
        per_second,
    };

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

    #[test]
    fn resource_monitor_rings_wrap_and_flush_events() {
        let mon = ResourceMonitor::new();
        assert_eq!(mon.stats(512).samples, 0);
        assert_eq!(mon.stats(512).avg_tick_micros, 0);

        mon.add_events(3);
        assert_eq!(mon.stats(1024).window_events, 3);
        assert_eq!(mon.stats(1024).total_events, 3);

        for i in 1..=(RESMON_WINDOW as u64 + 10) {
            mon.add_events(1);
            mon.record_frame(Duration::from_micros(i * 100));
        }

        let stats = mon.stats(1536);
        assert_eq!(stats.samples, RESMON_WINDOW);
        // Frames 11..=70 remained: avg = 40.5 * 100 = 4050 us, max = 7000 us.
        assert_eq!(stats.avg_tick_micros, 4050);
        assert_eq!(stats.max_tick_micros, 7000);
        assert_eq!(stats.last_tick_micros, 7000);
        assert_eq!(stats.window_events, RESMON_WINDOW as u64);
        assert_eq!(stats.total_events, 3 + RESMON_WINDOW as u64 + 10);
        assert_eq!(stats.heap_bytes, 1536);
        assert_eq!(format_kib(1536), "1.5");
    }
}
