//! Cooperative JS scheduling: `setTick`/`clearTick`, `setTimeout`/`clearTimeout`,
//! `setInterval`/`clearInterval`, `SetTimeout`/`ClearTimeout`, `Wait`/`Delay`,
//! and `Citizen` namespace aliases.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::rc::Weak;
use std::time::{Duration, Instant};

use rquickjs::function::{Opt, Rest};
use rquickjs::{Ctx, Function, Object, Persistent, Promise, Value as JsValue};
use sixm_core::{EventSource, ResourceName};

use crate::convert::{int_to_js, MAX_SAFE_INTEGER};
use crate::runtime::{CALLBACK_BUDGET, JsRuntime};
use crate::sandbox::catch_script;

/// Longest delay a script may request. Larger values are clamped.
const MAX_DELAY: Duration = Duration::from_secs(365 * 24 * 60 * 60);
/// `source` value for events sent by the server to a client.
const SERVER_SOURCE: i64 = 65_535;

struct TickEntry {
    func: Persistent<Function<'static>>,
    source: EventSource,
    invoker: Option<ResourceName>,
}

struct TimeoutEntry {
    func: Persistent<Function<'static>>,
    args: Vec<Persistent<JsValue<'static>>>,
    source: EventSource,
    invoker: Option<ResourceName>,
}

struct IntervalEntry {
    func: Persistent<Function<'static>>,
    args: Vec<Persistent<JsValue<'static>>>,
    interval: Duration,
    source: EventSource,
    invoker: Option<ResourceName>,
}

struct WaitEntry {
    resolve: Persistent<Function<'static>>,
    source: EventSource,
    invoker: Option<ResourceName>,
}

enum TimerAction {
    Timeout(u64),
    Interval(u64),
    Wait(u64),
}

enum NextFrameAction {
    Timeout(u64),
    Wait(u64),
}

#[derive(Default)]
pub(crate) struct Sched {
    next_id: u64,
    ticks: HashMap<u64, TickEntry>,
    tick_order: Vec<u64>,
    timeouts: HashMap<u64, TimeoutEntry>,
    intervals: HashMap<u64, IntervalEntry>,
    waits: HashMap<u64, WaitEntry>,
    timers: BinaryHeap<Reverse<(Instant, u64)>>,
    next_frame: Vec<NextFrameAction>,
    spare_frame: Vec<NextFrameAction>,
}

impl Sched {
    fn alloc_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn is_live_timer(&self, id: u64) -> bool {
        self.timeouts.contains_key(&id)
            || self.intervals.contains_key(&id)
            || self.waits.contains_key(&id)
    }

    fn take_timer_action(&mut self, id: u64) -> Option<TimerAction> {
        if self.timeouts.contains_key(&id) {
            Some(TimerAction::Timeout(id))
        } else if self.intervals.contains_key(&id) {
            Some(TimerAction::Interval(id))
        } else if self.waits.contains_key(&id) {
            Some(TimerAction::Wait(id))
        } else {
            None
        }
    }
}

/// Installs `setTick`, `clearTick`, `setTimeout`, `clearTimeout`, `setInterval`,
/// `clearInterval`, `SetTimeout`, `ClearTimeout`, `Wait`, `Delay`, and `Citizen`.
pub(crate) fn install(ctx: &Ctx<'_>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    let citizen = Object::new(ctx.clone())?;
    install_ticks(ctx, weak, &citizen)?;
    install_timers(ctx, weak, &citizen)?;
    install_waits(ctx, weak, &citizen)?;
    ctx.globals().set("Citizen", citizen)?;
    Ok(())
}

fn install_ticks<'js>(
    ctx: &Ctx<'js>,
    weak: &Weak<JsRuntime>,
    citizen: &Object<'js>,
) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    let set_tick = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, func: Function<'js>| -> rquickjs::Result<JsValue<'js>> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let id = rt.add_tick(&ctx, func);
                Ok(int_to_js(&ctx, id_to_i64(id)))
            },
        )?
    };
    let clear_tick = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, id: Opt<JsValue<'js>>| -> rquickjs::Result<()> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                if let Some(id) = id.0.and_then(|v| id_from_js(&v)) {
                    rt.remove_tick(id);
                }
                Ok(())
            },
        )?
    };
    for (name, func) in [("setTick", set_tick), ("clearTick", clear_tick)] {
        citizen.set(name, func.clone())?;
        globals.set(name, func)?;
    }
    Ok(())
}

fn install_timers<'js>(
    ctx: &Ctx<'js>,
    weak: &Weak<JsRuntime>,
    citizen: &Object<'js>,
) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    let set_timeout = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>,
                  func: Function<'js>,
                  ms: Opt<JsValue<'js>>,
                  rest: Rest<JsValue<'js>>|
                  -> rquickjs::Result<JsValue<'js>> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let delay = ms.0.as_ref().map_or(Duration::ZERO, delay_from_value);
                let id = rt.add_timeout(&ctx, func, delay, rest.0);
                Ok(int_to_js(&ctx, id_to_i64(id)))
            },
        )?
    };
    let clear_timeout = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, id: Opt<JsValue<'js>>| -> rquickjs::Result<()> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                if let Some(id) = id.0.and_then(|v| id_from_js(&v)) {
                    rt.remove_timeout(id);
                }
                Ok(())
            },
        )?
    };
    let set_interval = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>,
                  func: Function<'js>,
                  ms: Opt<JsValue<'js>>,
                  rest: Rest<JsValue<'js>>|
                  -> rquickjs::Result<JsValue<'js>> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let delay = ms
                    .0
                    .as_ref()
                    .map_or(Duration::ZERO, delay_from_value)
                    .max(Duration::from_millis(1));
                let id = rt.add_interval(&ctx, func, delay, rest.0);
                Ok(int_to_js(&ctx, id_to_i64(id)))
            },
        )?
    };
    let clear_interval = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, id: Opt<JsValue<'js>>| -> rquickjs::Result<()> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                if let Some(id) = id.0.and_then(|v| id_from_js(&v)) {
                    rt.remove_interval(id);
                }
                Ok(())
            },
        )?
    };
    let fivem_set_timeout = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, ms: JsValue<'js>, func: Function<'js>| -> rquickjs::Result<JsValue<'js>> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let delay = delay_from_value(&ms);
                let id = rt.add_timeout(&ctx, func, delay, Vec::new());
                Ok(int_to_js(&ctx, id_to_i64(id)))
            },
        )?
    };

    for (name, func) in [
        ("setTimeout", set_timeout),
        ("clearTimeout", clear_timeout.clone()),
        ("setInterval", set_interval),
        ("clearInterval", clear_interval),
    ] {
        citizen.set(name, func.clone())?;
        globals.set(name, func)?;
    }
    globals.set("SetTimeout", fivem_set_timeout)?;
    globals.set("ClearTimeout", clear_timeout)?;
    Ok(())
}

fn install_waits<'js>(
    ctx: &Ctx<'js>,
    weak: &Weak<JsRuntime>,
    citizen: &Object<'js>,
) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    let wait_fn = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, ms: Opt<JsValue<'js>>| -> rquickjs::Result<Promise<'js>> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let delay = ms.0.as_ref().map_or(Duration::ZERO, delay_from_value);
                rt.add_wait(&ctx, delay)
            },
        )?
    };
    for name in ["Wait", "Delay"] {
        citizen.set(name, wait_fn.clone())?;
        globals.set(name, wait_fn.clone())?;
    }
    Ok(())
}

impl JsRuntime {
    pub(crate) fn current_invoker(&self) -> Option<ResourceName> {
        self.current_invoker.borrow().clone()
    }

    pub(crate) fn set_source_global(ctx: &Ctx<'_>, source: EventSource) -> rquickjs::Result<()> {
        let globals = ctx.globals();
        match source {
            EventSource::Local => globals.set("source", ""),
            EventSource::Net(id) | EventSource::Player(id) => {
                globals.set("source", i64::from(id))
            }
            EventSource::Server => globals.set("source", SERVER_SOURCE),
        }
    }

    fn add_tick<'js>(&self, ctx: &Ctx<'js>, func: Function<'js>) -> u64 {
        let entry = TickEntry {
            func: Persistent::save(ctx, func),
            source: self.current_source.get(),
            invoker: self.current_invoker(),
        };
        let mut sched = self.sched.borrow_mut();
        let id = sched.alloc_id();
        sched.ticks.insert(id, entry);
        sched.tick_order.push(id);
        id
    }

    fn remove_tick(&self, id: u64) {
        let removed = self.sched.borrow_mut().ticks.remove(&id);
        if removed.is_some() {
            self.sched.borrow_mut().tick_order.retain(|&tid| tid != id);
        }
        drop(removed);
    }

    fn add_timeout<'js>(
        &self,
        ctx: &Ctx<'js>,
        func: Function<'js>,
        delay: Duration,
        args: Vec<JsValue<'js>>,
    ) -> u64 {
        let entry = TimeoutEntry {
            func: Persistent::save(ctx, func),
            args: args.into_iter().map(|v| Persistent::save(ctx, v)).collect(),
            source: self.current_source.get(),
            invoker: self.current_invoker(),
        };
        let mut sched = self.sched.borrow_mut();
        let id = sched.alloc_id();
        sched.timeouts.insert(id, entry);
        if delay.is_zero() {
            sched.next_frame.push(NextFrameAction::Timeout(id));
        } else {
            let deadline = Instant::now()
                .checked_add(delay)
                .unwrap_or_else(Instant::now);
            sched.timers.push(Reverse((deadline, id)));
        }
        id
    }

    fn remove_timeout(&self, id: u64) {
        let removed = self.sched.borrow_mut().timeouts.remove(&id);
        drop(removed);
    }

    fn add_interval<'js>(
        &self,
        ctx: &Ctx<'js>,
        func: Function<'js>,
        interval: Duration,
        args: Vec<JsValue<'js>>,
    ) -> u64 {
        let entry = IntervalEntry {
            func: Persistent::save(ctx, func),
            args: args.into_iter().map(|v| Persistent::save(ctx, v)).collect(),
            interval,
            source: self.current_source.get(),
            invoker: self.current_invoker(),
        };
        let mut sched = self.sched.borrow_mut();
        let id = sched.alloc_id();
        sched.intervals.insert(id, entry);
        let deadline = Instant::now()
            .checked_add(interval)
            .unwrap_or_else(Instant::now);
        sched.timers.push(Reverse((deadline, id)));
        id
    }

    fn remove_interval(&self, id: u64) {
        let removed = self.sched.borrow_mut().intervals.remove(&id);
        drop(removed);
    }

    fn add_wait<'js>(&self, ctx: &Ctx<'js>, delay: Duration) -> rquickjs::Result<Promise<'js>> {
        let (promise, resolve, _reject) = Promise::new(ctx)?;
        let entry = WaitEntry {
            resolve: Persistent::save(ctx, resolve),
            source: self.current_source.get(),
            invoker: self.current_invoker(),
        };
        let mut sched = self.sched.borrow_mut();
        let id = sched.alloc_id();
        sched.waits.insert(id, entry);
        if delay.is_zero() {
            sched.next_frame.push(NextFrameAction::Wait(id));
        } else {
            let deadline = Instant::now()
                .checked_add(delay)
                .unwrap_or_else(Instant::now);
            sched.timers.push(Reverse((deadline, id)));
        }
        Ok(promise)
    }

    /// Runs one frame: sweeps unretained refs, moves previous rejections into check
    /// buffer, executes `next_frame` waits/timeouts, due timers, active `setTick`
    /// callbacks, drains microtasks, and warns about any still-unhandled rejections.
    pub(crate) fn run_frame(&self, now: Instant) {
        self.sweep_refs();
        self.advance_rejections();
        self.run_next_frame();
        self.run_due_timers(now);
        self.run_ticks();
        self.report_unhandled_rejections();
    }

    fn run_next_frame(&self) {
        let frame = {
            let mut sched = self.sched.borrow_mut();
            if sched.next_frame.is_empty() {
                return;
            }
            let spare = std::mem::take(&mut sched.spare_frame);
            std::mem::replace(&mut sched.next_frame, spare)
        };
        for action in &frame {
            match *action {
                NextFrameAction::Timeout(id) => self.fire_timeout(id),
                NextFrameAction::Wait(id) => self.fire_wait(id),
            }
        }
        let mut frame = frame;
        frame.clear();
        self.sched.borrow_mut().spare_frame = frame;
    }

    /// Executes due `setTimeout`, `setInterval`, and positive-`Wait` timers.
    pub(crate) fn run_due_timers(&self, now: Instant) {
        while let Some(action) = self.pop_due_timer(now) {
            match action {
                TimerAction::Timeout(id) => self.fire_timeout(id),
                TimerAction::Interval(id) => self.fire_interval(id, now),
                TimerAction::Wait(id) => self.fire_wait(id),
            }
        }
    }

    fn pop_due_timer(&self, now: Instant) -> Option<TimerAction> {
        let mut sched = self.sched.borrow_mut();
        while let Some(&Reverse((deadline, id))) = sched.timers.peek() {
            if deadline > now {
                break;
            }
            sched.timers.pop();
            if let Some(action) = sched.take_timer_action(id) {
                return Some(action);
            }
        }
        None
    }

    pub(crate) fn earliest_timer(&self) -> Option<Instant> {
        let mut sched = self.sched.borrow_mut();
        while let Some(&Reverse((deadline, id))) = sched.timers.peek() {
            if sched.is_live_timer(id) {
                return Some(deadline);
            }
            sched.timers.pop();
        }
        None
    }

    fn run_ticks(&self) {
        let tick_ids = self.sched.borrow().tick_order.clone();
        for id in tick_ids {
            let entry = {
                let sched = self.sched.borrow();
                sched
                    .ticks
                    .get(&id)
                    .map(|e| (e.func.clone(), e.source, e.invoker.clone()))
            };
            if let Some((func, source, invoker)) = entry {
                self.invoke_scheduled_callback(func, Vec::new(), source, invoker);
            }
        }
    }

    fn fire_timeout(&self, id: u64) {
        let entry = self.sched.borrow_mut().timeouts.remove(&id);
        if let Some(entry) = entry {
            self.invoke_scheduled_callback(entry.func, entry.args, entry.source, entry.invoker);
        }
    }

    fn fire_interval(&self, id: u64, now: Instant) {
        let entry = {
            let sched = self.sched.borrow();
            sched.intervals.get(&id).map(|e| {
                (
                    e.func.clone(),
                    e.args.clone(),
                    e.interval,
                    e.source,
                    e.invoker.clone(),
                )
            })
        };
        let Some((func, args, interval, source, invoker)) = entry else {
            return;
        };
        self.invoke_scheduled_callback(func, args, source, invoker);
        let mut sched = self.sched.borrow_mut();
        if sched.intervals.contains_key(&id) && !self.shut.get() {
            let next_deadline = now.checked_add(interval).unwrap_or(now);
            sched.timers.push(Reverse((next_deadline, id)));
        }
    }

    fn fire_wait(&self, id: u64) {
        let entry = self.sched.borrow_mut().waits.remove(&id);
        if let Some(entry) = entry {
            self.invoke_scheduled_callback(entry.resolve, Vec::new(), entry.source, entry.invoker);
        }
    }

    fn invoke_scheduled_callback(
        &self,
        func: Persistent<Function<'static>>,
        args: Vec<Persistent<JsValue<'static>>>,
        source: EventSource,
        invoker: Option<ResourceName>,
    ) {
        if self.shut.get() {
            return;
        }
        let outer_source = self.current_source.replace(source);
        let outer_invoker = std::mem::replace(&mut *self.current_invoker.borrow_mut(), invoker);
        {
            let _budget = self.enter_budget(CALLBACK_BUDGET);
            let outcome = self.with_ctx(|ctx| {
                Self::set_source_global(ctx, source)
                    .map_err(|err| crate::runtime::js_err(ctx, err))?;
                let func = func
                    .restore(ctx)
                    .map_err(|err| crate::runtime::js_err(ctx, err))?;
                let mut call_args = rquickjs::function::Args::new(ctx.clone(), args.len());
                for arg in args {
                    let val = arg
                        .restore(ctx)
                        .map_err(|err| crate::runtime::js_err(ctx, err))?;
                    call_args
                        .push_arg(val)
                        .map_err(|err| crate::runtime::js_err(ctx, err))?;
                }
                catch_script(ctx, func.call_arg::<()>(call_args))
            });
            if let Err(err) = outcome {
                tracing::error!(target: "sixm::script", "[{}] {err}", self.resource);
            }
            self.drain_jobs();
        }
        let _ = self.with_ctx(|ctx| Self::set_source_global(ctx, outer_source));
        *self.current_invoker.borrow_mut() = outer_invoker;
        self.current_source.set(outer_source);
    }
}

/// Converts a JS delay value into a `Duration` clamped to [`MAX_DELAY`].
/// Anything non-numeric, `NaN`, or `<= 0` returns `Duration::ZERO`.
pub(crate) fn delay_from_value(value: &JsValue<'_>) -> Duration {
    if let Some(int) = value.as_int() {
        u64::try_from(int)
            .map_or(Duration::ZERO, Duration::from_millis)
            .min(MAX_DELAY)
    } else if let Some(ms) = value.as_float() {
        delay_from_ms(ms)
    } else {
        Duration::ZERO
    }
}

fn delay_from_ms(ms: f64) -> Duration {
    if ms.is_nan() || ms <= 0.0 {
        return Duration::ZERO;
    }
    Duration::try_from_secs_f64(ms / 1000.0)
        .unwrap_or(MAX_DELAY)
        .min(MAX_DELAY)
}

#[expect(
    clippy::cast_possible_wrap,
    reason = "sequential scheduler ids start at 1 and stay far below i64::MAX"
)]
fn id_to_i64(id: u64) -> i64 {
    id as i64
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "checked finite non-negative integer within MAX_SAFE_INTEGER"
)]
fn id_from_js(value: &JsValue<'_>) -> Option<u64> {
    if let Some(int) = value.as_int() {
        u64::try_from(int).ok()
    } else if let Some(float) = value.as_float() {
        (float.is_finite() && float >= 0.0 && float.fract() == 0.0 && float <= MAX_SAFE_INTEGER)
            .then_some(float as u64)
    } else {
        None
    }
}
