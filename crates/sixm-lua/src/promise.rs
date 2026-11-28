//! Promises: values that settle later, and `Await`, which parks a task until one settles.
//!
//! A promise is shared state (`Core`) behind a userdata handle. Settling a
//! promise never runs a Lua callback. Each attached Lua callback is queued as a
//! task for the next frame, and a task parked in `Await` is queued with the
//! outcome. Reactions that only move values between promises (adoption,
//! `promise.all`, `promise.first`) run at settle time, since they call no Lua.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use mlua::{
    AnyUserData, Function, MultiValue, Table, UserData, UserDataMethods, Value as LuaValue,
};

use crate::runtime::{LuaRuntime, bind_fn};

/// Error raised by `Await` when no task is running.
const AWAIT_OUTSIDE_TASK: &str = "Await can only be called from a thread (Citizen.CreateThread)";

/// Runs one reaction as a task. It calls the callback under `pcall`, so an error
/// rejects the child, and a yield inside the callback parks the task as usual.
/// The chunk receives the Rust `settle` function as `...`.
const REACTION_SOURCE: &str = r"
local settle = ...
local pcall = pcall
return function(child, callback, ok, value)
	if callback == nil then
		return settle(child, ok, value)
	end
	return settle(child, pcall(callback, value))
end
";

/// `Await`. It yields the task until the promise settles, then returns the value
/// or raises the reason. A table with `then(resolve, reject)` can be adopted
/// across resource boundaries, where promise userdata cannot travel.
const AWAIT_SOURCE: &str = r"
local register, new = ...
local yield, isyieldable, error = coroutine.yield, coroutine.isyieldable, error
local type, pcall = type, pcall
return function(promise)
	if not isyieldable() then
		error('Await can only be called from a thread (Citizen.CreateThread)', 2)
	end
	if type(promise) == 'table' then
		local thenable, adopted = promise, new()
		local ok, reason = pcall(function()
			local subscribe = thenable['then']
			if subscribe == nil then
				error('Await expects a promise or thenable', 0)
			end
			subscribe(function(value) adopted:resolve(value) end,
				function(reason) adopted:reject(reason) end)
		end)
		if not ok then adopted:reject(reason) end
		promise = adopted
	end
	local settled, ok, value = register(promise)
	if not settled then
		ok, value = yield()
	end
	if not ok then
		error(value, 0)
	end
	return value
end
";

/// Shared state of one promise.
type Shared = Rc<RefCell<Core>>;

/// Promise bookkeeping held by a runtime.
#[derive(Default)]
pub(crate) struct Promises {
    /// The function that runs one reaction, set when the runtime is built.
    reaction: Option<Function>,
    /// Promises rejected since the current frame began.
    rejected: Vec<Shared>,
}

/// Promises rejected before the current frame. Checked when it ends.
#[derive(Default)]
pub(crate) struct Rejections(Vec<Shared>);

/// State of one promise.
#[derive(Default)]
struct Core {
    /// The settled outcome, or `None` while pending.
    outcome: Option<Outcome>,
    /// Set by the first `resolve` or `reject`. Later calls do nothing.
    locked: bool,
    /// Set once any reaction is attached. A rejection without one is reported.
    handled: bool,
    reactions: Vec<Reaction>,
}

/// How a promise settled.
#[derive(Clone)]
enum Outcome {
    Fulfilled(LuaValue),
    Rejected(LuaValue),
}

impl Outcome {
    /// Splits the outcome into `(fulfilled, value_or_reason)`.
    fn into_parts(self) -> (bool, LuaValue) {
        match self {
            Self::Fulfilled(value) => (true, value),
            Self::Rejected(reason) => (false, reason),
        }
    }
}

/// What happens when a promise settles, for one attached reaction.
enum Reaction {
    /// `next`. The child takes the callback's result. A missing callback passes the outcome on.
    Then {
        child: Shared,
        on_fulfilled: Option<Function>,
        on_rejected: Option<Function>,
    },
    /// Adoption: the child settles with the same outcome.
    Adopt { child: Shared },
    /// `Await`: wakes the parked task with `(fulfilled, value_or_reason)`.
    Wake { slot: usize, seq: u64 },
    /// One input of `promise.all`.
    Gather {
        gather: Rc<RefCell<Gather>>,
        index: usize,
    },
    /// One input of `promise.first`. The first input to settle decides the child.
    Race { child: Shared },
}

/// The state of one `promise.all` call.
struct Gather {
    child: Shared,
    /// Fulfilled values by input position.
    values: Vec<LuaValue>,
    /// Inputs not fulfilled yet.
    remaining: usize,
}

/// The userdata a script holds: one promise of one runtime.
struct PromiseHandle {
    runtime: Weak<LuaRuntime>,
    core: Shared,
}

impl UserData for PromiseHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("resolve", |_, this, value: LuaValue| {
            LuaRuntime::upgrade(&this.runtime)?.resolve(&this.core, value)
        });
        methods.add_method("reject", |_, this, reason: LuaValue| {
            LuaRuntime::upgrade(&this.runtime)?.reject(&this.core, reason)
        });
        methods.add_method(
            "next",
            |_, this, (on_fulfilled, on_rejected): (Option<Function>, Option<Function>)| {
                let runtime = LuaRuntime::upgrade(&this.runtime)?;
                let child = new_core();
                runtime.attach(
                    &this.core,
                    Reaction::Then {
                        child: Rc::clone(&child),
                        on_fulfilled,
                        on_rejected,
                    },
                )?;
                runtime.promise_value(child)
            },
        );
    }
}

/// Installs the `promise` table, `Await` (also as `Citizen.Await`) and the reaction shim.
pub(crate) fn install(
    runtime: &Rc<LuaRuntime>,
    globals: &Table,
    citizen: &Table,
) -> mlua::Result<()> {
    let lua = &runtime.lua;
    let weak = &runtime.me;

    let settle = bind_fn(
        lua,
        weak,
        |rt, (child, ok, value): (AnyUserData, bool, LuaValue)| {
            let child = core_of(&child)
                .ok_or_else(|| mlua::Error::runtime("a promise reaction lost its promise"))?;
            if ok {
                rt.resolve(&child, value)
            } else {
                rt.reject(&child, value)
            }
        },
    )?;
    let reaction: Function = lua
        .load(REACTION_SOURCE)
        .set_name("=sixm promise")
        .call(settle)?;
    runtime.promises.borrow_mut().reaction = Some(reaction);

    let register = bind_fn(lua, weak, |rt, (promise,): (AnyUserData,)| {
        rt.await_promise(&promise)
    })?;
    let new = bind_fn(lua, weak, |rt, (): ()| rt.promise_value(new_core()))?;
    let await_fn: Function = lua
        .load(AWAIT_SOURCE)
        .set_name("=sixm await")
        .call((register, new.clone()))?;
    citizen.raw_set("Await", await_fn.clone())?;
    globals.raw_set("Await", await_fn)?;

    let module = lua.create_table()?;
    module.raw_set("new", new)?;
    module.raw_set(
        "all",
        bind_fn(lua, weak, |rt, (list,): (Table,)| {
            let core = rt.promise_all(&list)?;
            rt.promise_value(core)
        })?,
    )?;
    module.raw_set(
        "first",
        bind_fn(lua, weak, |rt, (list,): (Table,)| {
            let core = rt.promise_first(&list)?;
            rt.promise_value(core)
        })?,
    )?;
    globals.raw_set("promise", module)?;
    Ok(())
}

fn new_core() -> Shared {
    Rc::new(RefCell::new(Core::default()))
}

/// The state behind a promise userdata, or `None` for any other userdata.
fn core_of(data: &AnyUserData) -> Option<Shared> {
    let handle = data.borrow::<PromiseHandle>().ok()?;
    Some(Rc::clone(&handle.core))
}

/// The state behind a promise value, or `None` for any other value.
fn core_of_value(value: &LuaValue) -> Option<Shared> {
    value.as_userdata().and_then(core_of)
}

/// Marks `core` as resolved or rejected. Returns `false` when it already was.
fn lock(core: &Shared) -> bool {
    let mut state = core.borrow_mut();
    if state.locked {
        false
    } else {
        state.locked = true;
        true
    }
}

/// The reason of a rejected promise that no reaction handles, if `core` is one.
fn unhandled(core: &Core) -> Option<&LuaValue> {
    match &core.outcome {
        Some(Outcome::Rejected(reason)) if !core.handled => Some(reason),
        _ => None,
    }
}

/// Text of a rejection reason for the log.
fn describe(reason: &LuaValue) -> String {
    match reason {
        LuaValue::String(text) => text.to_string_lossy(),
        other => other
            .to_string()
            .unwrap_or_else(|_| other.type_name().to_owned()),
    }
}

impl LuaRuntime {
    /// Wraps `core` in a promise value of this runtime.
    fn promise_value(&self, core: Shared) -> mlua::Result<AnyUserData> {
        self.lua.create_userdata(PromiseHandle {
            runtime: self.me.clone(),
            core,
        })
    }

    /// Resolves `core` with `value`. A promise value is adopted: `core` then
    /// settles with that promise's outcome.
    fn resolve(&self, core: &Shared, value: LuaValue) -> mlua::Result<()> {
        if !lock(core) {
            return Ok(());
        }
        let Some(other) = core_of_value(&value) else {
            return self.settle(core, &Outcome::Fulfilled(value));
        };
        if Rc::ptr_eq(&other, core) {
            let reason = LuaValue::String(
                self.lua
                    .create_string("a promise cannot be resolved with itself")?,
            );
            return self.settle(core, &Outcome::Rejected(reason));
        }
        self.attach(
            &other,
            Reaction::Adopt {
                child: Rc::clone(core),
            },
        )
    }

    /// Rejects `core` with `reason`, unless it was already resolved or rejected.
    fn reject(&self, core: &Shared, reason: LuaValue) -> mlua::Result<()> {
        if lock(core) {
            self.settle(core, &Outcome::Rejected(reason))
        } else {
            Ok(())
        }
    }

    /// Records the outcome of `core` and dispatches its reactions. Does nothing
    /// when `core` has already settled.
    fn settle(&self, core: &Shared, outcome: &Outcome) -> mlua::Result<()> {
        let reactions = {
            let mut state = core.borrow_mut();
            if state.outcome.is_some() {
                return Ok(());
            }
            if matches!(outcome, Outcome::Rejected(_)) && !state.handled {
                self.promises.borrow_mut().rejected.push(Rc::clone(core));
            }
            state.outcome = Some(outcome.clone());
            std::mem::take(&mut state.reactions)
        };
        for reaction in reactions {
            self.dispatch(reaction, outcome.clone())?;
        }
        Ok(())
    }

    /// Attaches `reaction` to `core`. If `core` has already settled, the reaction is dispatched now.
    fn attach(&self, core: &Shared, reaction: Reaction) -> mlua::Result<()> {
        let settled = {
            let mut state = core.borrow_mut();
            state.handled = true;
            if let Some(outcome) = state.outcome.clone() {
                outcome
            } else {
                state.reactions.push(reaction);
                return Ok(());
            }
        };
        self.dispatch(reaction, settled)
    }

    /// Runs one reaction for a settled outcome. A Lua callback is queued as a
    /// task for the next frame. Rust-only reactions run here.
    fn dispatch(&self, reaction: Reaction, outcome: Outcome) -> mlua::Result<()> {
        match reaction {
            Reaction::Then {
                child,
                on_fulfilled,
                on_rejected,
            } => {
                let (ok, value) = outcome.into_parts();
                let callback = if ok { on_fulfilled } else { on_rejected };
                let args = MultiValue::from_vec(vec![
                    LuaValue::UserData(self.promise_value(child)?),
                    callback.map_or(LuaValue::Nil, LuaValue::Function),
                    LuaValue::Boolean(ok),
                    value,
                ]);
                let (slot, seq) = self.spawn(
                    self.promise_reaction()?,
                    self.current_source.get(),
                    self.current_invoker(),
                )?;
                self.schedule_wake(slot, seq, args);
                Ok(())
            }
            Reaction::Adopt { child } | Reaction::Race { child } => self.settle(&child, &outcome),
            Reaction::Wake { slot, seq } => {
                let (ok, value) = outcome.into_parts();
                self.schedule_wake(
                    slot,
                    seq,
                    MultiValue::from_vec(vec![LuaValue::Boolean(ok), value]),
                );
                Ok(())
            }
            Reaction::Gather { gather, index } => self.gather(&gather, index, outcome),
        }
    }

    /// The reaction function set up by [`install`](Self::install).
    fn promise_reaction(&self) -> mlua::Result<Function> {
        self.promises
            .borrow()
            .reaction
            .clone()
            .ok_or_else(|| mlua::Error::runtime("promises are not installed"))
    }

    /// Records one input of a `promise.all` call. The first rejection rejects the
    /// child. The last fulfilment fulfils it with the values in order.
    fn gather(
        &self,
        gather: &Rc<RefCell<Gather>>,
        index: usize,
        outcome: Outcome,
    ) -> mlua::Result<()> {
        let (ok, value) = outcome.into_parts();
        if !ok {
            let child = Rc::clone(&gather.borrow().child);
            return self.settle(&child, &Outcome::Rejected(value));
        }
        let done = {
            let mut state = gather.borrow_mut();
            if let Some(slot) = state.values.get_mut(index) {
                *slot = value;
            }
            state.remaining = state.remaining.saturating_sub(1);
            state.remaining == 0
        };
        if done {
            self.gather_done(gather)
        } else {
            Ok(())
        }
    }

    /// Fulfils the child of a finished `promise.all` call with its values.
    fn gather_done(&self, gather: &Rc<RefCell<Gather>>) -> mlua::Result<()> {
        let (child, values) = {
            let mut state = gather.borrow_mut();
            (Rc::clone(&state.child), std::mem::take(&mut state.values))
        };
        let table = self.lua.create_sequence_from(values)?;
        self.settle(&child, &Outcome::Fulfilled(LuaValue::Table(table)))
    }

    /// `promise.all(list)`. Plain values in `list` count as fulfilled.
    fn promise_all(&self, list: &Table) -> mlua::Result<Shared> {
        let items = list
            .sequence_values::<LuaValue>()
            .collect::<mlua::Result<Vec<_>>>()?;
        let child = new_core();
        let gather = Rc::new(RefCell::new(Gather {
            child: Rc::clone(&child),
            values: vec![LuaValue::Nil; items.len()],
            remaining: items.len(),
        }));
        if items.is_empty() {
            self.gather_done(&gather)?;
        }
        for (index, item) in items.into_iter().enumerate() {
            match core_of_value(&item) {
                Some(core) => self.attach(
                    &core,
                    Reaction::Gather {
                        gather: Rc::clone(&gather),
                        index,
                    },
                )?,
                None => self.gather(&gather, index, Outcome::Fulfilled(item))?,
            }
        }
        Ok(child)
    }

    /// `promise.first(list)`. The child settles like the first input to settle.
    fn promise_first(&self, list: &Table) -> mlua::Result<Shared> {
        let child = new_core();
        for item in list.sequence_values::<LuaValue>() {
            let item = item?;
            match core_of_value(&item) {
                Some(core) => self.attach(
                    &core,
                    Reaction::Race {
                        child: Rc::clone(&child),
                    },
                )?,
                None => self.settle(&child, &Outcome::Fulfilled(item))?,
            }
        }
        Ok(child)
    }

    /// `Await(promise)`, called from Lua. Returns `(settled, fulfilled, value_or_reason)`.
    /// When the promise is pending, the current task is parked on it.
    fn await_promise(&self, promise: &AnyUserData) -> mlua::Result<(bool, bool, LuaValue)> {
        let core =
            core_of(promise).ok_or_else(|| mlua::Error::runtime("Await expects a promise"))?;
        let Some((slot, seq)) = self.current_task.get() else {
            return Err(mlua::Error::runtime(AWAIT_OUTSIDE_TASK));
        };
        let settled = {
            let mut state = core.borrow_mut();
            state.handled = true;
            state.outcome.clone()
        };
        if let Some(outcome) = settled {
            let (ok, value) = outcome.into_parts();
            return Ok((true, ok, value));
        }
        self.attach(&core, Reaction::Wake { slot, seq })?;
        self.mark_parked_on_promise();
        Ok((false, false, LuaValue::Nil))
    }

    /// Takes the promises rejected before this frame. They are checked when the frame ends.
    pub(crate) fn take_rejected(&self) -> Rejections {
        Rejections(std::mem::take(&mut self.promises.borrow_mut().rejected))
    }

    /// Logs each promise in `rejected` that is still unhandled, with the resource name and reason.
    pub(crate) fn report_unhandled(&self, rejected: Rejections) {
        for core in rejected.0 {
            let state = core.borrow();
            if let Some(reason) = unhandled(&state) {
                tracing::warn!(
                    target: "sixm::script",
                    "[{}] unhandled promise rejection: {}",
                    self.resource,
                    describe(reason)
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn core_with(outcome: Option<Outcome>, handled: bool) -> Core {
        Core {
            outcome,
            locked: true,
            handled,
            reactions: Vec::new(),
        }
    }

    #[test]
    fn only_a_rejection_that_nothing_handles_is_reported() {
        let reason = || Some(Outcome::Rejected(LuaValue::Nil));
        assert!(unhandled(&core_with(reason(), false)).is_some());
        assert!(unhandled(&core_with(reason(), true)).is_none());
        assert!(unhandled(&core_with(None, false)).is_none());
        let fulfilled = Some(Outcome::Fulfilled(LuaValue::Nil));
        assert!(unhandled(&core_with(fulfilled, false)).is_none());
    }
}
