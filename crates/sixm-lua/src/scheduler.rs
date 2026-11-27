//! Cooperative scheduling: a task slab, timers, and the per-frame resume loop.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::time::{Duration, Instant};

use mlua::thread::ThreadStatus;
use mlua::{Function, MultiValue, Thread, Value as LuaValue};
use sixm_core::EventSource;

use crate::runtime::LuaRuntime;

/// Longest delay a script may request. Larger values are clamped.
const MAX_DELAY: Duration = Duration::from_secs(365 * 24 * 60 * 60);
/// Finished coroutines kept for reuse.
const IDLE_THREAD_LIMIT: usize = 64;
/// The `source` of an event sent by the server, as FiveM exposes it to clients.
const SERVER_SOURCE: i64 = 65_535;

/// A coroutine the scheduler resumes.
struct Task {
    thread: Thread,
    source: EventSource,
    /// Unique per task; timers carry it so a recycled slot cannot be woken by a stale entry.
    seq: u64,
    /// The handler, passed as the first resume argument to the task wrapper.
    pending: Option<Function>,
    /// Arguments for the next resume, set when a promise wakes the task.
    wake: MultiValue,
}

/// Scheduler state. Lives in a `RefCell` and is never borrowed across a Lua call.
#[derive(Default)]
pub(crate) struct Sched {
    tasks: Vec<Option<Task>>,
    free_slots: Vec<usize>,
    idle_threads: Vec<Thread>,
    /// Min-heap of `(deadline, task seq, slot)`. Entries for finished tasks are dropped lazily.
    timers: BinaryHeap<Reverse<(Instant, u64, usize)>>,
    /// `(slot, task seq)` of tasks that run on the next frame.
    next_frame: Vec<(usize, u64)>,
    spare_frame: Vec<(usize, u64)>,
    next_seq: u64,
    /// Pending `SetTimeout` tasks, keyed by id, so `ClearTimeout` can find them.
    timeouts: HashMap<u64, usize>,
    /// Set by `Await` just before it yields, so the yield parks the task without a deadline.
    parked_on_promise: bool,
}

impl Sched {
    fn is_live(&self, slot: usize, seq: u64) -> bool {
        matches!(self.tasks.get(slot), Some(Some(task)) if task.seq == seq)
    }

    /// Takes the arguments queued for a live task. `None` when the task has finished.
    fn take_wake(&mut self, slot: usize, seq: u64) -> Option<MultiValue> {
        match self.tasks.get_mut(slot) {
            Some(Some(task)) if task.seq == seq => {
                Some(std::mem::replace(&mut task.wake, MultiValue::new()))
            }
            _ => None,
        }
    }

    fn timer_due(&self, now: Instant) -> bool {
        self.timers
            .peek()
            .is_some_and(|Reverse((deadline, _, _))| *deadline <= now)
    }
}

impl LuaRuntime {
    /// Creates a task that will call `handler` and returns its slot and sequence number.
    pub(crate) fn spawn(
        &self,
        handler: Function,
        source: EventSource,
    ) -> mlua::Result<(usize, u64)> {
        let idle = self.sched.borrow_mut().idle_threads.pop();
        let thread = match idle {
            Some(thread) if thread.reset(self.task_wrapper.clone()).is_ok() => thread,
            _ => self.lua.create_thread(self.task_wrapper.clone())?,
        };
        let mut sched = self.sched.borrow_mut();
        let seq = sched.next_seq;
        sched.next_seq += 1;
        let task = Task {
            thread,
            source,
            seq,
            pending: Some(handler),
            wake: MultiValue::new(),
        };
        let slot = if let Some(slot) = sched.free_slots.pop() {
            sched.tasks[slot] = Some(task);
            slot
        } else {
            sched.tasks.push(Some(task));
            sched.tasks.len() - 1
        };
        Ok((slot, seq))
    }

    /// Queues an existing task to run on the next frame.
    pub(crate) fn schedule_next_frame(&self, slot: usize, seq: u64) {
        self.sched.borrow_mut().next_frame.push((slot, seq));
    }

    /// Queues a task to run on the next frame with `args` as its resume arguments.
    /// Does nothing when the task has finished.
    pub(crate) fn schedule_wake(&self, slot: usize, seq: u64, args: MultiValue) {
        let mut guard = self.sched.borrow_mut();
        let sched = &mut *guard;
        if !sched.is_live(slot, seq) {
            return;
        }
        if let Some(Some(task)) = sched.tasks.get_mut(slot) {
            task.wake = args;
        }
        sched.next_frame.push((slot, seq));
    }

    /// Tells the scheduler that the task yielding now waits on a promise, so it
    /// is not put on a timer or the next-frame list. A promise wakes it.
    pub(crate) fn mark_parked_on_promise(&self) {
        self.sched.borrow_mut().parked_on_promise = true;
    }

    /// Creates a `SetTimeout` task that starts after `delay` and returns its id.
    pub(crate) fn set_timeout(&self, delay: Duration, handler: Function) -> mlua::Result<u64> {
        let (slot, seq) = self.spawn(handler, self.current_source.get())?;
        let deadline = Instant::now()
            .checked_add(delay)
            .unwrap_or_else(Instant::now);
        let mut sched = self.sched.borrow_mut();
        sched.timeouts.insert(seq, slot);
        sched.timers.push(Reverse((deadline, seq, slot)));
        Ok(seq)
    }

    /// Cancels a `SetTimeout` task that has not started. Returns whether one was cancelled.
    pub(crate) fn cancel_timeout(&self, id: u64) -> bool {
        let Some(slot) = self.sched.borrow_mut().timeouts.remove(&id) else {
            return false;
        };
        drop(self.free_slot(slot));
        true
    }

    /// Runs the frame: sweeps references, runs the queued tasks and due timers,
    /// then reports the promises rejected in the previous frame that nothing handled.
    pub(crate) fn run_frame(&self, now: Instant) {
        self.sweep_refs();
        let rejected = self.take_rejected();
        self.run_queued(now);
        self.report_unhandled(rejected);
    }

    /// Runs every task queued for the next frame, then every timer that is due.
    fn run_queued(&self, now: Instant) {
        let frame = {
            let mut sched = self.sched.borrow_mut();
            if sched.next_frame.is_empty() && !sched.timer_due(now) {
                return;
            }
            let spare = std::mem::take(&mut sched.spare_frame);
            std::mem::replace(&mut sched.next_frame, spare)
        };
        for &(slot, seq) in &frame {
            let wake = self.sched.borrow_mut().take_wake(slot, seq);
            if let Some(args) = wake {
                self.run(slot, now, args);
            }
        }
        let mut frame = frame;
        frame.clear();
        self.sched.borrow_mut().spare_frame = frame;
        self.run_due_timers(now);
    }

    /// Resumes every timer task whose deadline has passed.
    pub(crate) fn run_due_timers(&self, now: Instant) {
        while let Some(slot) = self.pop_due_timer(now) {
            self.run(slot, now, MultiValue::new());
        }
    }

    /// Earliest deadline of a live timer. Next-frame tasks are not counted.
    pub(crate) fn earliest_timer(&self) -> Option<Instant> {
        let mut sched = self.sched.borrow_mut();
        while let Some(&Reverse((deadline, seq, slot))) = sched.timers.peek() {
            if sched.is_live(slot, seq) {
                return Some(deadline);
            }
            sched.timers.pop();
        }
        None
    }

    /// Makes the `source` global match `source`, the way FiveM exposes it to handlers.
    pub(crate) fn set_source(&self, source: EventSource) -> mlua::Result<()> {
        let value = match source {
            EventSource::Local => LuaValue::String(self.empty_string.clone()),
            EventSource::Net(id) | EventSource::Player(id) => LuaValue::Integer(i64::from(id)),
            EventSource::Server => LuaValue::Integer(SERVER_SOURCE),
        };
        self.lua.globals().raw_set("source", value)
    }

    /// Resumes task `slot` at time `now`.
    ///
    /// `args` go to the handler on its first resume and are discarded afterwards.
    /// A task that fails is logged with its traceback and dropped. Other tasks
    /// keep running.
    pub(crate) fn run(&self, slot: usize, now: Instant, args: MultiValue) {
        let taken = {
            let mut sched = self.sched.borrow_mut();
            sched
                .tasks
                .get_mut(slot)
                .and_then(Option::as_mut)
                .map(|task| {
                    (
                        task.thread.clone(),
                        task.source,
                        task.pending.take(),
                        task.seq,
                    )
                })
        };
        let Some((thread, source, pending, seq)) = taken else {
            return;
        };
        let resume_args = match pending {
            Some(handler) => {
                let mut values = Vec::with_capacity(args.len() + 1);
                values.push(LuaValue::Function(handler));
                values.extend(args);
                MultiValue::from_vec(values)
            }
            None => args,
        };

        let outer = self.current_source.replace(source);
        let outer_task = self.current_task.replace(Some((slot, seq)));
        let result = self.step(slot, thread, source, now, resume_args);
        self.current_task.set(outer_task);
        self.current_source.set(outer);
        if let Err(err) = result {
            tracing::error!(target: "sixm::script", "[{}] {err}", self.resource);
            drop(self.free_slot(slot));
        }
    }

    /// Resumes a task once and decides where it goes next.
    fn step(
        &self,
        slot: usize,
        thread: Thread,
        source: EventSource,
        now: Instant,
        args: MultiValue,
    ) -> mlua::Result<()> {
        self.set_source(source)?;
        let values: MultiValue = thread.resume(args)?;
        if thread.status() == ThreadStatus::Resumable {
            let first = values.into_vec().into_iter().next();
            self.park(slot, now, first.as_ref());
            return Ok(());
        }
        if let [LuaValue::Boolean(false), message, ..] = values.into_vec().as_slice() {
            let text = match message {
                LuaValue::String(text) => text.to_string_lossy(),
                _ => String::from("error object is not a string"),
            };
            tracing::error!(target: "sixm::script", "[{}] {text}", self.resource);
        }
        self.finish(slot, thread);
        Ok(())
    }

    /// Puts a yielded task on the timer heap or the next-frame list.
    fn park(&self, slot: usize, now: Instant, yielded: Option<&LuaValue>) {
        let delay = yielded.map_or(Duration::ZERO, delay_from_value);
        let mut sched = self.sched.borrow_mut();
        if std::mem::take(&mut sched.parked_on_promise) {
            return;
        }
        let Some(seq) = sched
            .tasks
            .get(slot)
            .and_then(Option::as_ref)
            .map(|task| task.seq)
        else {
            return;
        };
        if delay.is_zero() {
            sched.next_frame.push((slot, seq));
        } else {
            let deadline = now.checked_add(delay).unwrap_or(now);
            sched.timers.push(Reverse((deadline, seq, slot)));
        }
    }

    /// Frees a finished task's slot and keeps its coroutine for reuse.
    fn finish(&self, slot: usize, thread: Thread) {
        drop(self.free_slot(slot));
        let mut sched = self.sched.borrow_mut();
        if sched.idle_threads.len() < IDLE_THREAD_LIMIT {
            sched.idle_threads.push(thread);
        }
    }

    /// Empties `slot` and returns its task. The task is dropped by the caller, outside the borrow.
    fn free_slot(&self, slot: usize) -> Option<Task> {
        let mut sched = self.sched.borrow_mut();
        let task = sched.tasks.get_mut(slot).and_then(Option::take);
        if task.is_some() {
            sched.free_slots.push(slot);
        }
        task
    }

    /// Pops the next live timer whose deadline has passed.
    fn pop_due_timer(&self, now: Instant) -> Option<usize> {
        let mut sched = self.sched.borrow_mut();
        while let Some(&Reverse((deadline, seq, slot))) = sched.timers.peek() {
            if deadline > now {
                break;
            }
            sched.timers.pop();
            if sched.is_live(slot, seq) {
                sched.timeouts.remove(&seq);
                return Some(slot);
            }
        }
        None
    }
}

/// Reads the delay a task yielded. Anything that is not a positive number means "next frame".
pub(crate) fn delay_from_value(value: &LuaValue) -> Duration {
    match value {
        LuaValue::Integer(ms) => u64::try_from(*ms)
            .map_or(Duration::ZERO, Duration::from_millis)
            .min(MAX_DELAY),
        LuaValue::Number(ms) => delay_from_ms(*ms),
        _ => Duration::ZERO,
    }
}

/// Converts a millisecond count into a delay, clamped to [`MAX_DELAY`].
pub(crate) fn delay_from_ms(ms: f64) -> Duration {
    if ms.is_nan() || ms <= 0.0 {
        return Duration::ZERO;
    }
    Duration::try_from_secs_f64(ms / 1000.0)
        .unwrap_or(MAX_DELAY)
        .min(MAX_DELAY)
}
