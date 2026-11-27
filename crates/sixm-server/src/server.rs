//! The server: console dispatch, networking, the frame loop and shutdown.

use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use sixm_net::{NetError, NetEvent};
use tracing::{error, info, warn};

use crate::commands::{self, CommandContext, CommandSource};
use crate::manager::ResourceManager;

/// Most queued commands that run in one drain. Beyond this, scripts are
/// assumed to be looping and the rest of the queue is dropped.
const MAX_QUEUED_COMMANDS: usize = 1000;

/// Input for the server loop.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// A console line, read from stdin.
    Line(String),
    /// Stop the server. The text is shown in the log.
    Quit(String),
    /// Something that happened on the network. Sent by the network thread.
    Net(NetEvent),
}

/// A SixM server: the resource manager, the console, networking and the frame loop.
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
        commands::run_line(
            &self.manager,
            line,
            &CommandContext::new(CommandSource::Console),
        );
        self.drain_commands();
    }

    /// Runs one command given as words, name first, then every queued command.
    /// This is how `+command` arguments run.
    pub fn execute_words(&self, words: &[String]) {
        commands::run_words(
            &self.manager,
            words,
            &words.join(" "),
            &CommandContext::new(CommandSource::Console),
        );
        self.drain_commands();
    }

    /// Runs one frame at `now`: resumes every running resource, runs the queued
    /// commands, refuses the deferred connections that ran out of time and
    /// sends the entity changes. The frame's duration is recorded for `perf`.
    pub fn tick(&self, now: Instant) {
        let started = Instant::now();
        self.manager.deliver_http();
        self.manager.tick_all(now);
        self.drain_commands();
        self.manager.expire_deferrals(now);
        self.manager.flush_entities();
        self.manager.flush_kvp_due(now);
        self.manager.frames.borrow_mut().record(started.elapsed());
    }

    /// Binds the listening socket and sends its events to `input`, the sending
    /// end of the channel the loop reads. The address comes from
    /// `endpoint_add_udp`, and the player limit and server name are read from
    /// their convars. Returns the bound address, with the port resolved.
    ///
    /// [`run`](Self::run) calls this before its first frame. Call it yourself
    /// only when driving the server with [`handle`](Self::handle).
    pub fn start_network(&self, input: Sender<Input>) -> Result<SocketAddr, NetError> {
        self.manager.start_network(input)
    }

    /// Processes one input: a console line, a network event or a quit request,
    /// then the commands it queued. The loop calls this for every input.
    pub fn handle(&self, input: Input) {
        match input {
            Input::Line(line) => self.execute(&line),
            Input::Net(event) => {
                self.manager.handle_net(event);
                self.drain_commands();
            }
            Input::Quit(reason) => {
                info!("Shutting down: {reason}");
                self.manager.request_quit();
            }
        }
    }

    /// Starts the network, then runs the console and the frame loop until a
    /// quit is requested, then stops every resource and the network.
    ///
    /// `sender` is the sending end of `input`. The network needs its own clone,
    /// so that network events wake the loop at once. If the network cannot
    /// start, the error is logged and the server runs without it.
    ///
    /// The loop sleeps on `input` until the earlier of the next frame and the
    /// next timer. A timer that comes due between frames runs at once, while
    /// `Wait(0)` tasks only run on frames.
    /// If the server falls more than a frame behind, it resynchronises rather
    /// than running frames back to back. Once `input` is disconnected the loop
    /// keeps running without console input.
    pub fn run(&self, sender: &Sender<Input>, input: &Receiver<Input>) {
        match self.start_network(sender.clone()) {
            Ok(addr) => {
                info!("Listening on {addr}");
                if let Some(fingerprint) = self.manager.network_fingerprint() {
                    info!("Server fingerprint: {fingerprint}");
                }
            }
            Err(err) => error!("network is disabled: {err}"),
        }
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
                Ok(message) => self.handle(message),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => input_open = false,
            }
        }
        self.shutdown();
    }

    /// Stops every running resource, last started first, then the network, and
    /// logs `Server stopped`. Players are not told one by one: the resources
    /// stop first, and the network closes every connection on the way out.
    pub fn shutdown(&self) {
        self.manager.stop_all();
        self.manager.shutdown_kvp();
        self.manager.stop_network();
        info!("Server stopped");
    }

    /// Runs queued commands, applies queued kicks and deferral calls until
    /// the queues are empty, up to [`MAX_QUEUED_COMMANDS`] commands.
    fn drain_commands(&self) {
        let mut ran = 0;
        loop {
            self.manager.apply_deferral_calls();
            self.manager.apply_drops();
            let Some((line, source)) = self.manager.pop_command() else {
                return;
            };
            if ran == MAX_QUEUED_COMMANDS {
                let dropped = self.manager.discard_commands() + 1;
                warn!("more than {MAX_QUEUED_COMMANDS} commands queued at once; dropped {dropped}");
                return;
            }
            ran += 1;
            commands::run_line(&self.manager, &line, &CommandContext::new(source));
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
