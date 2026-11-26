//! Inbound event rate limiting for one player.
//!
//! A token bucket admits events at a sustained rate with a short burst. Events
//! beyond the bucket are dropped. A player whose traffic stays above ten times
//! the sustained rate for five consecutive seconds is considered to be
//! flooding and gets kicked.

use std::time::{Duration, Instant};

const EVENTS_PER_SECOND: f64 = 100.0;
const BURST: f64 = 200.0;
const WINDOW: Duration = Duration::from_secs(1);
/// Events per window above which a window counts as hot: ten times the rate.
const FLOOD_LIMIT: u64 = 1000;
const FLOOD_WINDOWS: u32 = 5;
const DROP_REPORT_INTERVAL: Duration = Duration::from_secs(1);

/// Outcome of admitting one event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Deliver the event.
    Accept,
    /// Discard the event but keep the connection.
    Drop,
    /// Kick the player.
    Flood,
}

/// Rate limiter for a single connection.
#[derive(Debug)]
pub(crate) struct RateLimiter {
    tokens: f64,
    refilled_at: Instant,
    window_start: Instant,
    window_count: u64,
    hot_windows: u32,
    dropped: u64,
    reported_at: Instant,
}

impl RateLimiter {
    /// Creates a limiter with a full bucket.
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            tokens: BURST,
            refilled_at: now,
            window_start: now,
            window_count: 0,
            hot_windows: 0,
            dropped: 0,
            reported_at: now,
        }
    }

    /// Decides whether an event arriving at `now` is delivered.
    pub(crate) fn admit(&mut self, now: Instant) -> Verdict {
        self.roll_window(now);
        self.window_count += 1;
        if self.window_count > FLOOD_LIMIT && self.hot_windows + 1 >= FLOOD_WINDOWS {
            return Verdict::Flood;
        }
        self.refill(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Verdict::Accept
        } else {
            self.dropped += 1;
            Verdict::Drop
        }
    }

    /// Returns how many events were dropped since the last report, at most once
    /// per second, so that a flood produces a bounded amount of logging.
    pub(crate) fn take_drop_report(&mut self, now: Instant) -> Option<u64> {
        if self.dropped == 0 || now.duration_since(self.reported_at) < DROP_REPORT_INTERVAL {
            return None;
        }
        self.reported_at = now;
        Some(std::mem::take(&mut self.dropped))
    }

    fn roll_window(&mut self, now: Instant) {
        let elapsed = now.duration_since(self.window_start);
        if elapsed < WINDOW {
            return;
        }
        let hot = self.window_count > FLOOD_LIMIT && elapsed < WINDOW * 2;
        self.hot_windows = if hot { self.hot_windows + 1 } else { 0 };
        self.window_start = now;
        self.window_count = 0;
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.duration_since(self.refilled_at).as_secs_f64();
        self.tokens = (self.tokens + elapsed * EVENTS_PER_SECOND).min(BURST);
        self.refilled_at = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Admits `count` events spread over one second starting at `base`.
    fn hot_second(limiter: &mut RateLimiter, base: Instant) -> Vec<Verdict> {
        (0..=FLOOD_LIMIT)
            .map(|step| limiter.admit(base + Duration::from_micros(step * 900)))
            .collect()
    }

    #[test]
    fn burst_is_admitted_then_events_are_dropped() {
        let start = Instant::now();
        let mut limiter = RateLimiter::new(start);
        let admitted = (0..250)
            .filter(|_| limiter.admit(start) == Verdict::Accept)
            .count();
        assert_eq!(admitted, 200);
    }

    #[test]
    fn bucket_refills_at_the_sustained_rate() {
        let start = Instant::now();
        let mut limiter = RateLimiter::new(start);
        for _ in 0..200 {
            limiter.admit(start);
        }
        let later = start + Duration::from_millis(500);
        let admitted = (0..100)
            .filter(|_| limiter.admit(later) == Verdict::Accept)
            .count();
        assert_eq!(admitted, 50);
    }

    #[test]
    fn flood_is_detected_in_the_fifth_consecutive_hot_second() {
        let start = Instant::now();
        let mut limiter = RateLimiter::new(start);
        for second in 0..4 {
            let verdicts = hot_second(&mut limiter, start + WINDOW * second);
            assert!(!verdicts.contains(&Verdict::Flood));
        }
        let verdicts = hot_second(&mut limiter, start + WINDOW * 4);
        assert_eq!(
            verdicts.iter().position(|v| *v == Verdict::Flood),
            Some(1000)
        );
    }

    #[test]
    fn a_quiet_second_resets_the_flood_count() {
        let start = Instant::now();
        let mut limiter = RateLimiter::new(start);
        for second in 0..4 {
            hot_second(&mut limiter, start + WINDOW * second);
        }
        assert_ne!(limiter.admit(start + WINDOW * 5), Verdict::Flood);
        for second in 6..10 {
            let verdicts = hot_second(&mut limiter, start + WINDOW * second);
            assert!(!verdicts.contains(&Verdict::Flood));
        }
    }
}
