//! Function references: ids handed to other resources, and the callable JS
//! wrapper holding a retained [`FuncHandle`] for foreign functions.

use std::collections::HashMap;
use std::rc::Weak;

use rquickjs::class::Trace;
use rquickjs::function::Rest;
use rquickjs::{Ctx, Exception, Function, JsLifetime, Object, Persistent, Value as JsValue};
use sixm_core::{FuncRef, Host, ScriptError, Value};

use crate::convert::{js_to_value_vec, values_to_js_return};
use crate::runtime::{CALLBACK_BUDGET, JsRuntime, js_err};
use crate::sandbox::catch_script;

/// A function handed out to other resources.
struct Entry {
    func: Persistent<Function<'static>>,
    /// Retains held by receivers (and pins by this runtime).
    retains: u32,
}

/// Functions this runtime has handed out, keyed by id.
#[derive(Default)]
pub(crate) struct RefTable {
    last_id: u32,
    entries: HashMap<u32, Entry>,
    by_func: HashMap<Persistent<Function<'static>>, u32>,
    /// Entries that had no retains at some point since the last sweep.
    unretained: Vec<u32>,
}

impl RefTable {
    /// Id of `func`, registering it without retains if it is new.
    fn register<'js>(&mut self, ctx: &Ctx<'js>, func: Function<'js>) -> Result<u32, ScriptError> {
        let persistent = Persistent::save(ctx, func);
        if let Some(&id) = self.by_func.get(&persistent) {
            return Ok(id);
        }
        self.last_id = self
            .last_id
            .checked_add(1)
            .ok_or_else(|| ScriptError::Runtime("too many function references".to_owned()))?;
        let id = self.last_id;
        self.entries.insert(
            id,
            Entry {
                func: persistent.clone(),
                retains: 0,
            },
        );
        self.by_func.insert(persistent, id);
        self.unretained.push(id);
        Ok(id)
    }

    fn retain(&mut self, id: u32) -> bool {
        let Some(entry) = self.entries.get_mut(&id) else {
            return false;
        };
        entry.retains = entry.retains.saturating_add(1);
        true
    }

    fn release(&mut self, id: u32) {
        if let Some(entry) = self.entries.get_mut(&id) {
            entry.retains = entry.retains.saturating_sub(1);
            if entry.retains == 0 {
                self.unretained.push(id);
            }
        }
    }

    /// Removes entries that still have no retains. The functions are returned
    /// so the caller drops them outside the borrow.
    fn sweep(&mut self) -> Vec<Persistent<Function<'static>>> {
        let mut freed = Vec::new();
        for id in std::mem::take(&mut self.unretained) {
            if self
                .entries
                .get(&id)
                .is_some_and(|entry| entry.retains == 0)
                && let Some(entry) = self.entries.remove(&id)
            {
                self.by_func.remove(&entry.func);
                freed.push(entry.func);
            }
        }
        freed
    }
}

/// Hidden tag stored under a private `Symbol` on foreign function wrappers so
/// passing a foreign function onward unwraps the original [`FuncRef`].
#[derive(Trace, JsLifetime)]
#[rquickjs::class(frozen)]
pub(crate) struct FuncTag {
    #[qjs(skip_trace)]
    pub(crate) func: FuncRef,
}

/// Retained guard held inside the JS closure representing a foreign [`FuncRef`].
/// Dropping the JS function releases the retain on the owner runtime.
pub(crate) struct FuncHandle {
    pub(crate) host: Weak<dyn Host>,
    pub(crate) func: FuncRef,
    pub(crate) runtime: Weak<JsRuntime>,
}

impl Drop for FuncHandle {
    fn drop(&mut self) {
        if let Some(host) = self.host.upgrade() {
            host.release_ref(&self.func);
        }
    }
}

impl JsRuntime {
    /// The reference a foreign resource uses to call function `id` of this runtime.
    pub(crate) fn func_ref(&self, id: u32) -> FuncRef {
        FuncRef {
            owner: self.resource.clone(),
            instance: self.instance,
            id,
        }
    }

    /// Returns a reference to `func`, valid at least until the next frame.
    pub(crate) fn ref_function<'js>(
        &self,
        ctx: &Ctx<'js>,
        func: Function<'js>,
    ) -> Result<FuncRef, ScriptError> {
        let id = self.refs.borrow_mut().register(ctx, func)?;
        Ok(self.func_ref(id))
    }

    /// Returns a reference to `func` that stays valid until shutdown.
    pub(crate) fn pin_function<'js>(
        &self,
        ctx: &Ctx<'js>,
        func: Function<'js>,
    ) -> Result<FuncRef, ScriptError> {
        let mut refs = self.refs.borrow_mut();
        let id = refs.register(ctx, func)?;
        refs.retain(id);
        Ok(self.func_ref(id))
    }

    /// Returns the persistent function registered under `id`, if it is still alive.
    pub(crate) fn lookup_function(&self, id: u32) -> Option<Persistent<Function<'static>>> {
        self.refs
            .borrow()
            .entries
            .get(&id)
            .map(|entry| entry.func.clone())
    }

    pub(crate) fn retain_function(&self, id: u32) -> bool {
        self.refs.borrow_mut().retain(id)
    }

    /// Gives back one retain of `id`. A release that arrives while the table
    /// is borrowed is queued and applied at the next safe point.
    pub(crate) fn release_function(&self, id: u32) {
        if let Ok(mut refs) = self.refs.try_borrow_mut() {
            refs.release(id);
        } else {
            self.deferred_releases.borrow_mut().push(id);
        }
    }

    /// Applies queued releases and frees every entry without retains.
    pub(crate) fn sweep_refs(&self) {
        let pending = std::mem::take(&mut *self.deferred_releases.borrow_mut());
        let freed = {
            let mut refs = self.refs.borrow_mut();
            for id in pending {
                refs.release(id);
            }
            refs.sweep()
        };
        drop(freed);
    }

    /// Calls the function behind `id` synchronously.
    pub(crate) fn invoke_ref(&self, id: u32, args: &[Value]) -> Result<Vec<Value>, ScriptError> {
        if self.in_ctx.get() {
            return Err(ScriptError::Runtime(
                "re-entrant callback into active JS runtime is not supported".to_owned(),
            ));
        }
        let func = self
            .lookup_function(id)
            .ok_or_else(|| ScriptError::StaleRef(self.func_ref(id)))?;
        let _budget = self.enter_budget(CALLBACK_BUDGET);
        let outcome = self.with_ctx(|ctx| {
            let func = func.restore(ctx).map_err(|err| js_err(ctx, err))?;
            let js_args = self.values_to_js(ctx, args)?;
            let mut call_args = rquickjs::function::Args::new(ctx.clone(), js_args.len());
            call_args
                .push_args(js_args)
                .map_err(|err| js_err(ctx, err))?;
            let result: JsValue<'_> = catch_script(ctx, func.call_arg(call_args))?;
            self.js_return_to_values(ctx, &result)
        });
        self.drain_jobs();
        outcome
    }

    /// Wraps a foreign [`FuncRef`] as a callable JS `Function` tagged with `FuncTag`.
    pub(crate) fn foreign_js_function<'js>(
        &self,
        ctx: &Ctx<'js>,
        func: &FuncRef,
    ) -> Result<JsValue<'js>, ScriptError> {
        let host = self.host_or_err()?;
        if !host.retain_ref(func) {
            return Err(ScriptError::StaleRef(func.clone()));
        }
        let handle = FuncHandle {
            host: self.host.clone(),
            func: func.clone(),
            runtime: self.me.clone(),
        };
        let js_fn = Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, args: Rest<JsValue<'js>>| -> rquickjs::Result<JsValue<'js>> {
                let runtime = JsRuntime::upgrade(&ctx, &handle.runtime)?;
                let host = handle.host.upgrade().ok_or_else(|| {
                    Exception::throw_message(&ctx, "the server is no longer available")
                })?;
                let values = js_to_value_vec(&runtime, &ctx, &args.0)
                    .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))?;
                let results = host
                    .call_ref(&handle.func, &values)
                    .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))?;
                values_to_js_return(&runtime, &ctx, &results)
                    .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))
            },
        )
        .map_err(|err| js_err(ctx, err))?;

        if let Some(sym) = self.func_ref_symbol() {
            let sym = sym.restore(ctx).map_err(|err| js_err(ctx, err))?;
            let tag = rquickjs::Class::instance(ctx.clone(), FuncTag { func: func.clone() })
                .map_err(|err| js_err(ctx, err))?;
            let obj: &Object<'_> = js_fn.as_ref();
            obj.set(sym, tag).map_err(|err| js_err(ctx, err))?;
        }
        Ok(js_fn.into_value())
    }

    /// If `func` is a wrapper created by [`foreign_js_function`](Self::foreign_js_function),
    /// returns the underlying [`FuncRef`].
    pub(crate) fn unwrap_foreign_func<'js>(
        &self,
        ctx: &Ctx<'js>,
        func: &Function<'js>,
    ) -> Option<FuncRef> {
        let sym = self.func_ref_symbol()?.restore(ctx).ok()?;
        let obj: &Object<'_> = func.as_ref();
        let tag = obj.get::<_, Option<rquickjs::Class<FuncTag>>>(sym).ok()??;
        Some(tag.borrow().func.clone())
    }
}
