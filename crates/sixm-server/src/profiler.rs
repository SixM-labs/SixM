//! Frame and per-resource Chrome Trace (`chrome://tracing` / Perfetto) profiler.
//!
//! When inactive, recording costs only a `Cell<bool>` check in the tick loop.

use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};

/// Maximum events buffered in memory for one trace session.
const MAX_TRACE_EVENTS: usize = 250_000;

/// One complete (`ph: "X"`) duration event in a Chrome Trace capture.
#[derive(Debug, Clone)]
struct TraceEvent {
    name: String,
    cat: &'static str,
    ts_us: u64,
    dur_us: u64,
}

/// Active state and captured spans for the `profiler` console command.
#[derive(Debug)]
pub(crate) struct Profiler {
    recording: bool,
    frames_remaining: Option<u32>,
    recorded_frames: u32,
    origin: Instant,
    events: Vec<TraceEvent>,
    truncated: bool,
}

impl Default for Profiler {
    fn default() -> Self {
        Self {
            recording: false,
            frames_remaining: None,
            recorded_frames: 0,
            origin: Instant::now(),
            events: Vec::new(),
            truncated: false,
        }
    }
}

/// Snapshot shown by `profiler status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProfilerStatus {
    pub(crate) recording: bool,
    pub(crate) recorded_frames: u32,
    pub(crate) frames_remaining: Option<u32>,
    pub(crate) event_count: usize,
    pub(crate) truncated: bool,
}

impl Profiler {
    /// Clears any prior capture and begins recording `max_frames` frames (or until stopped).
    pub(crate) fn start(&mut self, max_frames: Option<u32>) {
        self.recording = true;
        self.frames_remaining = max_frames;
        self.recorded_frames = 0;
        self.origin = Instant::now();
        self.events.clear();
        self.truncated = false;
    }

    /// Stops an active recording and returns how many frames were captured.
    pub(crate) fn stop(&mut self) -> u32 {
        self.recording = false;
        self.frames_remaining = None;
        self.recorded_frames
    }

    /// Records one completed span started at `start` lasting `dur`.
    pub(crate) fn record_span(
        &mut self,
        name: &str,
        cat: &'static str,
        start: Instant,
        dur: Duration,
    ) {
        if !self.recording {
            return;
        }
        if self.events.len() >= MAX_TRACE_EVENTS {
            self.truncated = true;
            return;
        }
        let ts_us = u64::try_from(start.saturating_duration_since(self.origin).as_micros())
            .unwrap_or(u64::MAX);
        let dur_us = u64::try_from(dur.as_micros()).unwrap_or(u64::MAX);
        self.events.push(TraceEvent {
            name: name.to_owned(),
            cat,
            ts_us,
            dur_us,
        });
    }

    /// Records the outer `ServerTick` span and stops automatically once `frames_remaining` hits zero.
    /// Returns whether recording continues into the next frame.
    pub(crate) fn finish_frame(&mut self, started: Instant, dur: Duration) -> bool {
        if !self.recording {
            return false;
        }
        if started < self.origin {
            return true;
        }
        self.record_span("ServerTick", "frame", started, dur);
        self.recorded_frames = self.recorded_frames.saturating_add(1);
        if let Some(left) = self.frames_remaining.as_mut() {
            *left = left.saturating_sub(1);
            if *left == 0 {
                self.recording = false;
                self.frames_remaining = None;
            }
        }
        self.recording
    }

    pub(crate) fn status(&self) -> ProfilerStatus {
        ProfilerStatus {
            recording: self.recording,
            recorded_frames: self.recorded_frames,
            frames_remaining: self.frames_remaining,
            event_count: self.events.len(),
            truncated: self.truncated,
        }
    }

    /// Serializes captured events in Chrome Trace Event JSON format (`chrome://tracing`).
    pub(crate) fn to_chrome_trace_json(&self) -> Vec<u8> {
        let trace_events: Vec<Json> = self
            .events
            .iter()
            .map(|ev| {
                json!({
                    "name": ev.name,
                    "cat": ev.cat,
                    "ph": "X",
                    "ts": ev.ts_us,
                    "dur": ev.dur_us,
                    "pid": 1,
                    "tid": 1,
                })
            })
            .collect();
        let doc = json!({
            "displayTimeUnit": "ms",
            "traceEvents": trace_events,
        });
        serde_json::to_vec_pretty(&doc).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiler_auto_stops_after_requested_frames_and_exports_json() {
        let mut p = Profiler::default();
        assert!(!p.status().recording);

        let earlier_frame = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .expect("earlier instant");
        p.start(Some(2));
        assert!(p.status().recording);
        assert!(p.finish_frame(earlier_frame, Duration::from_millis(2)));
        assert_eq!(p.status().recorded_frames, 0);

        let t0 = Instant::now();
        p.record_span("res_a", "resource", t0, Duration::from_micros(120));
        assert!(p.finish_frame(t0, Duration::from_micros(300)));
        assert_eq!(p.status().recorded_frames, 1);

        let t1 = Instant::now();
        assert!(!p.finish_frame(t1, Duration::from_micros(200)));
        assert!(!p.status().recording);
        assert_eq!(p.status().recorded_frames, 2);
        assert_eq!(p.status().event_count, 3);

        let json: Json = serde_json::from_slice(&p.to_chrome_trace_json()).expect("valid json");
        let events = json["traceEvents"].as_array().expect("traceEvents array");
        assert_eq!(events.len(), 3);
        assert_eq!(events[0]["name"], "res_a");
        assert_eq!(events[0]["cat"], "resource");
        assert_eq!(events[0]["ph"], "X");
        assert_eq!(events[0]["dur"], 120);
        assert_eq!(events[1]["name"], "ServerTick");
    }
}
