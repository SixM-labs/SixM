//! The server: console dispatch, the frame loop and shutdown.

use std::rc::Rc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use tracing::{info, warn};

use crate::commands;
use crate::manager::ResourceManager;

/// Most queued commands that run in one drain. Beyond this, scripts are
/// assumed to be looping and the rest of the queue is dropped.
const MAX_QUEUED_COMMANDS: usize = 1000;

/// Input for the server loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// A console line, read from stdin.
    Line(String),
    /// Stop the server. The text is shown in the log.
    Quit(String),
}

/// A SixM server: the resource manager, the console and the frame loop.
#[derive(Debug)]
pub struct Server {
    manager: Rc<ResourceManager>,
}

impl Default for Server {
    fn default() -> Self {
        Self::new()
    }
}

impl Server {
    /// Creates a server with the default convars and no resources scanned yet.
    pub fn new() -> Self {
        Self {
            manager: ResourceManager::new(),
        }
    }

    /// The resource manager, for starting resources and calling exports.
    pub fn manager(&self) -> &ResourceManager {
        &self.manager
    }

    /// Runs one console line, then every command it or the scripts queued.
    pub fn execute(&self, line: &str) {
        commands::run_line(&self.manager, line, 0);
        self.drain_commands();
    }

    /// Runs one command given as words, name first, then every queued command.
    /// This is how `+command` arguments run.
    pub fn execute_words(&self, words: &[String]) {
        commands::run_words(&self.manager, words, &words.join(" "), 0);
        self.drain_commands();
    }

    /// Runs one frame at `now`: resumes every running resource, then runs the queued commands.
    pub fn tick(&self, now: Instant) {
        self.manager.tick_all(now);
        self.drain_commands();
    }

    /// Runs the console and the frame loop until a quit is requested, then
    /// stops every resource.
    ///
    /// The loop sleeps on `input` until the earlier of the next frame and the
    /// next timer. A timer that comes due between frames runs at once, while
    /// `Wait(0)` tasks only run on frames.
    /// If the server falls more than a frame behind, it resynchronises rather
    /// than running frames back to back. Once `input` is disconnected the loop
    /// keeps running without console input.
    pub fn run(&self, input: &Receiver<Input>) {
        let mut next_frame = Instant::now();
        let mut input_open = true;
        while !self.manager.quit_requested() {
            let now = Instant::now();
            if now >= next_frame {
                next_frame = advance(next_frame, now, frame_period(self.manager.tick_rate()));
                self.tick(now);
            } else if self
                .manager
                .next_deadline()
                .is_some_and(|deadline| deadline <= now)
            {
                self.manager.run_timers_all(now);
                self.drain_commands();
            }

            let wake = self
                .manager
                .next_deadline()
                .map_or(next_frame, |deadline| deadline.min(next_frame));
            let wait = wake.saturating_duration_since(Instant::now());
            if !input_open {
                thread::sleep(wait);
                continue;
            }
            match input.recv_timeout(wait) {
                Ok(Input::Line(line)) => self.execute(&line),
                Ok(Input::Quit(reason)) => {
                    info!("Shutting down: {reason}");
                    self.manager.request_quit();
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => input_open = false,
            }
        }
        self.shutdown();
    }

    /// Stops every running resource, last started first, and logs `Server stopped`.
    pub fn shutdown(&self) {
        self.manager.stop_all();
        info!("Server stopped");
    }

    /// Runs queued commands until the queue is empty, up to [`MAX_QUEUED_COMMANDS`].
    fn drain_commands(&self) {
        let mut ran = 0;
        while let Some(line) = self.manager.pop_command() {
            if ran == MAX_QUEUED_COMMANDS {
                let dropped = self.manager.discard_commands() + 1;
                warn!("more than {MAX_QUEUED_COMMANDS} commands queued at once; dropped {dropped}");
                return;
            }
            ran += 1;
            commands::run_line(&self.manager, &line, 0);
        }
    }
}

/// The frame after `scheduled`. A server more than one frame behind schedules
/// the next frame relative to `now` instead.
fn advance(scheduled: Instant, now: Instant, period: Duration) -> Instant {
    let next = scheduled + period;
    if next > now { next } else { now + period }
}

/// Time between frames at `rate` frames per second. `rate` is at least 1.
fn frame_period(rate: u64) -> Duration {
    Duration::from_micros(1_000_000 / rate)
}
