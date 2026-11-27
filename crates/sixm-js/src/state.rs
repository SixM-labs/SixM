//! State bags via ES6 `Proxy`: `GlobalState`, `Player(id).state`, `Entity(id).state`,
//! `LocalPlayer.state`, change handlers, and `GetPlayerFromStateBagName`.

use std::rc::Weak;

use rquickjs::class::Trace;
use rquickjs::function::Opt;
use rquickjs::proxy::{Proxy, ProxyHandler, ProxyProperty, ProxyReceiver, ProxyTarget};
use rquickjs::{Class, Ctx, Exception, Function, JsLifetime, Persistent, Value as JsValue};
use sixm_core::{GLOBAL_BAG, Value, player_of_bag};

use crate::runtime::{CALLBACK_BUDGET, JsRuntime};
use crate::sandbox::catch_script;

const RESERVED: i64 = 0;

struct ChangeHandler {
    cookie: i64,
    key: Option<String>,
    bag: Option<String>,
    func: Persistent<Function<'static>>,
}

#[derive(Default)]
pub(crate) struct ChangeHandlers {
    handlers: Vec<ChangeHandler>,
    last_cookie: i64,
}

impl ChangeHandlers {
    fn add<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        key: Option<String>,
        bag: Option<String>,
        func: Function<'js>,
    ) -> i64 {
        self.last_cookie += 1;
        let cookie = self.last_cookie;
        self.handlers.push(ChangeHandler {
            cookie,
            key,
            bag,
            func: Persistent::save(ctx, func),
        });
        cookie
    }

    fn remove(&mut self, cookie: i64) -> Option<Persistent<Function<'static>>> {
        let index = self
            .handlers
            .iter()
            .position(|handler| handler.cookie == cookie)?;
        Some(self.handlers.remove(index).func)
    }

    fn matching(&self, bag: &str, key: &str) -> Vec<Persistent<Function<'static>>> {
        self.handlers
            .iter()
            .filter(|handler| {
                handler.key.as_deref().is_none_or(|wanted| wanted == key)
                    && handler.bag.as_deref().is_none_or(|wanted| wanted == bag)
            })
            .map(|handler| handler.func.clone())
            .collect()
    }
}

#[derive(Trace, JsLifetime)]
#[rquickjs::class(frozen)]
struct StateBagTarget {
    bag: String,
    #[qjs(skip_trace)]
    runtime: Weak<JsRuntime>,
}

/// Builds an ES6 `Proxy` representing state bag `bag`.
pub(crate) fn state_bag<'js>(
    ctx: &Ctx<'js>,
    weak: &Weak<JsRuntime>,
    bag: &str,
) -> rquickjs::Result<JsValue<'js>> {
    let target = Class::instance(
        ctx.clone(),
        StateBagTarget {
            bag: bag.to_owned(),
            runtime: weak.clone(),
        },
    )?;

    let handler = ProxyHandler::new(ctx.clone())?
        .with_getter(bag_get)?
        .with_setter(bag_set)?
        .with_delete(bag_delete)?
        .with_has(bag_has)?;

    let proxy = Proxy::new(ctx.clone(), target, handler)?;
    Ok(proxy.into_value())
}

fn bag_target<'js>(
    target: &ProxyTarget<'js>,
    ctx: &Ctx<'js>,
) -> rquickjs::Result<(String, Weak<JsRuntime>)> {
    let cls = target
        .0
        .as_class::<StateBagTarget>()
        .ok_or_else(|| Exception::throw_type(ctx, "expected StateBag"))?;
    let guard = cls.borrow();
    Ok((guard.bag.clone(), guard.runtime.clone()))
}

// rquickjs::FromJs is only implemented for owned values, so proxy trap params are passed by value.
#[allow(clippy::needless_pass_by_value)]
fn bag_get<'js>(
    target: ProxyTarget<'js>,
    prop: ProxyProperty<'js>,
    _recv: ProxyReceiver<'js>,
) -> rquickjs::Result<JsValue<'js>> {
    let ctx = target.0.ctx().clone();
    if prop.is_symbol() {
        return Ok(JsValue::new_undefined(ctx));
    }
    let Ok(key) = prop.to_string() else {
        return Ok(JsValue::new_undefined(ctx));
    };
    let (bag, weak) = bag_target(&target, &ctx)?;
    let rt = JsRuntime::upgrade(&ctx, &weak)?;

    if key == "set" {
        let set_fn = Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>,
                  k: String,
                  val: JsValue<'js>,
                  replicated: Opt<Option<bool>>|
                  -> rquickjs::Result<()> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                write_state(
                    &rt,
                    &ctx,
                    &bag,
                    &k,
                    &val,
                    replicated.0.flatten().unwrap_or(true),
                )
            },
        )?;
        return Ok(set_fn.into_value());
    }

    match rt.host(&ctx)?.state_get(&bag, &key) {
        Some(value) => rt
            .to_js(&ctx, &value)
            .map_err(|err| Exception::throw_message(&ctx, &err.to_string())),
        None => Ok(JsValue::new_undefined(ctx)),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn bag_set<'js>(
    target: ProxyTarget<'js>,
    prop: ProxyProperty<'js>,
    value: JsValue<'js>,
    _recv: ProxyReceiver<'js>,
) -> rquickjs::Result<bool> {
    let ctx = target.0.ctx().clone();
    if prop.is_symbol() {
        return Ok(false);
    }
    let Ok(key) = prop.to_string() else {
        return Ok(false);
    };
    let (bag, weak) = bag_target(&target, &ctx)?;
    let rt = JsRuntime::upgrade(&ctx, &weak)?;
    write_state(&rt, &ctx, &bag, &key, &value, true)?;
    Ok(true)
}

#[allow(clippy::needless_pass_by_value)]
fn bag_delete<'js>(target: ProxyTarget<'js>, prop: ProxyProperty<'js>) -> rquickjs::Result<bool> {
    let ctx = target.0.ctx().clone();
    if prop.is_symbol() {
        return Ok(true);
    }
    let Ok(key) = prop.to_string() else {
        return Ok(true);
    };
    let (bag, weak) = bag_target(&target, &ctx)?;
    let rt = JsRuntime::upgrade(&ctx, &weak)?;
    let prev_skip = rt.skip_self_state.replace(true);
    let outcome = rt.host(&ctx)?.state_set(&bag, &key, Value::Nil, true);
    rt.skip_self_state.set(prev_skip);
    outcome.map_err(|err| Exception::throw_message(&ctx, &err.to_string()))?;
    rt.dispatch_state_change_in_ctx(&ctx, &bag, &key, &Value::Nil, true);
    Ok(true)
}

#[allow(clippy::needless_pass_by_value)]
fn bag_has<'js>(target: ProxyTarget<'js>, prop: ProxyProperty<'js>) -> rquickjs::Result<bool> {
    let ctx = target.0.ctx().clone();
    if prop.is_symbol() {
        return Ok(false);
    }
    let Ok(key) = prop.to_string() else {
        return Ok(false);
    };
    let (bag, weak) = bag_target(&target, &ctx)?;
    let rt = JsRuntime::upgrade(&ctx, &weak)?;
    Ok(rt.host(&ctx)?.state_get(&bag, &key).is_some())
}

fn write_state<'js>(
    rt: &JsRuntime,
    ctx: &Ctx<'js>,
    bag: &str,
    key: &str,
    value: &JsValue<'js>,
    replicated: bool,
) -> rquickjs::Result<()> {
    let val = rt
        .to_network_value(ctx, value)
        .map_err(|err| Exception::throw_message(ctx, &err.to_string()))?;
    let prev_skip = rt.skip_self_state.replace(true);
    let outcome = rt.host(ctx)?.state_set(bag, key, val.clone(), replicated);
    rt.skip_self_state.set(prev_skip);
    outcome.map_err(|err| Exception::throw_message(ctx, &err.to_string()))?;
    rt.dispatch_state_change_in_ctx(ctx, bag, key, &val, replicated);
    Ok(())
}

/// Installs `GlobalState`, `AddStateBagChangeHandler`,
/// `RemoveStateBagChangeHandler`, and `GetPlayerFromStateBagName`.
pub(crate) fn install<'js>(ctx: &Ctx<'js>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    globals.set("GlobalState", state_bag(ctx, weak, GLOBAL_BAG)?)?;

    let add_handler = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>,
                  key: Option<String>,
                  bag: Option<String>,
                  func: Function<'js>|
                  -> rquickjs::Result<i64> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                Ok(rt.state.borrow_mut().add(&ctx, key, bag, func))
            },
        )?
    };
    let remove_handler = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, cookie: i64| -> rquickjs::Result<()> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let removed = rt.state.borrow_mut().remove(cookie);
                drop(removed);
                Ok(())
            },
        )?
    };
    let player_from_bag = Function::new(ctx.clone(), |bag: String| {
        i64::from(player_of_bag(&bag).unwrap_or(0))
    })?;

    globals.set("AddStateBagChangeHandler", add_handler)?;
    globals.set("RemoveStateBagChangeHandler", remove_handler)?;
    globals.set("GetPlayerFromStateBagName", player_from_bag)?;
    Ok(())
}

impl JsRuntime {
    pub(crate) fn dispatch_state_change(
        &self,
        bag: &str,
        key: &str,
        value: &Value,
        replicated: bool,
    ) {
        if self.skip_self_state.get() {
            return;
        }
        if self.in_ctx.get() {
            self.pending_state_changes.borrow_mut().push_back((
                bag.to_owned(),
                key.to_owned(),
                value.clone(),
                replicated,
            ));
            return;
        }
        self.with_ctx(|ctx| {
            self.dispatch_state_change_in_ctx(ctx, bag, key, value, replicated);
        });
    }

    pub(crate) fn dispatch_state_change_in_ctx(
        &self,
        ctx: &Ctx<'_>,
        bag: &str,
        key: &str,
        value: &Value,
        replicated: bool,
    ) {
        let functions = self.state.borrow().matching(bag, key);
        if functions.is_empty() {
            return;
        }
        let outer_source = self.current_source.get();
        let outer_invoker = self.current_invoker();

        for func in functions {
            let _budget = self.enter_budget(CALLBACK_BUDGET);
            let outcome = (|| -> Result<(), sixm_core::ScriptError> {
                let func = func
                    .restore(ctx)
                    .map_err(|err| crate::runtime::js_err(ctx, err))?;
                let js_value = self.to_js(ctx, value)?;
                catch_script(
                    ctx,
                    func.call::<_, ()>((bag, key, js_value, RESERVED, replicated)),
                )
            })();
            if let Err(err) = outcome {
                tracing::error!(
                    target: "sixm::script",
                    "[{}] state change of `{bag}.{key}`: {err}",
                    self.resource
                );
            }
            self.drain_jobs();
        }

        let _ = Self::set_source_global(ctx, outer_source);
        *self.current_invoker.borrow_mut() = outer_invoker;
        self.current_source.set(outer_source);
    }
}
