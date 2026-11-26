#![allow(dead_code)]
//! A minimal server for the tests: it owns the runtimes and routes events,
//! exports, function references and console commands between them.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Instant;

use sixm_core::{
    ClientTarget, EventContext, EventSource, FuncRef, GLOBAL_BAG, Host, PlayerInfo, ResourceName,
    ScriptError, ScriptRuntime, Value, player_of_bag,
};
use sixm_lua::LuaRuntime;
use tempfile::TempDir;

/// Owns runtimes in start order and routes calls between them.
#[derive(Debug, Default)]
pub struct TestHost {
    runtimes: RefCell<Vec<Rc<LuaRuntime>>>,
    roots: RefCell<HashMap<String, PathBuf>>,
    dirs: RefCell<Vec<TempDir>>,
    convars: HashMap<String, String>,
    next_instance: Cell<u64>,
    /// Function references released by runtimes, in order.
    pub released: RefCell<Vec<FuncRef>>,
    /// Lines queued by `ExecuteCommand`.
    pub commands: RefCell<Vec<String>>,
    /// Connected players as `(id, info)`, in join order. Tests set this directly.
    pub players: RefCell<Vec<(u32, PlayerInfo)>>,
    /// Client events sent, as `(event, target, args)`, in order.
    pub client_events: RefCell<Vec<(String, ClientTarget, Vec<Value>)>>,
    /// Players dropped, as `(id, reason)`, in order.
    pub dropped: RefCell<Vec<(u32, String)>>,
    /// State values by `(bag, key)`.
    pub state: RefCell<HashMap<(String, String), Value>>,
}

impl TestHost {
    fn snapshot(&self) -> Vec<Rc<LuaRuntime>> {
        self.runtimes.borrow().clone()
    }

    fn find(&self, resource: &str) -> Option<Rc<LuaRuntime>> {
        self.runtimes
            .borrow()
            .iter()
            .rev()
            .find(|runtime| runtime.resource().as_ref() == resource)
            .cloned()
    }

    fn find_instance(&self, resource: &str, instance: u64) -> Option<Rc<LuaRuntime>> {
        self.runtimes
            .borrow()
            .iter()
            .find(|runtime| {
                runtime.resource().as_ref() == resource && runtime.instance() == instance
            })
            .cloned()
    }

    /// Removes `runtime` from the host and shuts it down.
    pub fn stop(&self, runtime: &Rc<LuaRuntime>) {
        self.runtimes
            .borrow_mut()
            .retain(|candidate| !Rc::ptr_eq(candidate, runtime));
        runtime.shutdown();
    }
}

impl Host for TestHost {
    fn trigger_event(
        &self,
        event: &str,
        source: EventSource,
        args: &[Value],
    ) -> Result<bool, ScriptError> {
        let ctx = EventContext::new();
        for runtime in self.snapshot() {
            if runtime.handles_event(event) {
                runtime.deliver_event(event, source, args, &ctx)?;
            }
        }
        Ok(ctx.is_canceled())
    }

    fn call_export(
        &self,
        resource: &str,
        export: &str,
        args: &[Value],
    ) -> Result<Vec<Value>, ScriptError> {
        let runtime = self
            .find(resource)
            .ok_or_else(|| ScriptError::ResourceNotRunning(resource.to_owned()))?;
        runtime.call_export(export, args)
    }

    fn call_ref(&self, func: &FuncRef, args: &[Value]) -> Result<Vec<Value>, ScriptError> {
        let runtime = self
            .find_instance(&func.owner, func.instance)
            .ok_or_else(|| ScriptError::StaleRef(func.clone()))?;
        runtime.call_ref(func.id, args)
    }

    fn retain_ref(&self, func: &FuncRef) -> bool {
        self.find_instance(&func.owner, func.instance)
            .is_some_and(|runtime| runtime.retain_ref(func.id))
    }

    fn release_ref(&self, func: &FuncRef) {
        self.released.borrow_mut().push(func.clone());
        if let Some(runtime) = self.find_instance(&func.owner, func.instance) {
            runtime.release_ref(func.id);
        }
    }

    fn execute_command(&self, line: &str) {
        self.commands.borrow_mut().push(line.to_owned());
    }

    fn register_command(&self, _name: &str, _handler: FuncRef, _restricted: bool) {}

    fn get_convar(&self, name: &str) -> Option<String> {
        self.convars.get(name).cloned()
    }

    fn resource_path(&self, resource: &str) -> Option<PathBuf> {
        self.roots.borrow().get(resource).cloned()
    }

    fn trigger_client_event(
        &self,
        event: &str,
        target: ClientTarget,
        args: &[Value],
    ) -> Result<(), ScriptError> {
        if args.iter().any(Value::contains_func_ref) {
            return Err(ScriptError::Runtime(
                "function references cannot be sent to clients".to_owned(),
            ));
        }
        self.client_events
            .borrow_mut()
            .push((event.to_owned(), target, args.to_vec()));
        Ok(())
    }

    fn players(&self) -> Vec<u32> {
        self.players.borrow().iter().map(|(id, _)| *id).collect()
    }

    fn player_info(&self, player: u32) -> Option<PlayerInfo> {
        self.players
            .borrow()
            .iter()
            .find(|(id, _)| *id == player)
            .map(|(_, info)| info.clone())
    }

    fn drop_player(&self, player: u32, reason: &str) {
        self.dropped.borrow_mut().push((player, reason.to_owned()));
    }

    fn state_get(&self, bag: &str, key: &str) -> Option<Value> {
        self.state
            .borrow()
            .get(&(bag.to_owned(), key.to_owned()))
            .cloned()
    }

    /// Stores the value like the server does, refusing unknown player bags, and
    /// runs the change handlers of every runtime. Replication is not modelled.
    fn state_set(
        &self,
        bag: &str,
        key: &str,
        value: Value,
        replicated: bool,
    ) -> Result<(), ScriptError> {
        let player_connected = player_of_bag(bag).is_some_and(|id| {
            self.players
                .borrow()
                .iter()
                .any(|(connected, _)| *connected == id)
        });
        if bag != GLOBAL_BAG && !player_connected {
            return Err(ScriptError::Runtime(format!(
                "state bag `{bag}` does not exist"
            )));
        }
        if value.contains_func_ref() {
            return Err(ScriptError::Runtime(
                "state values cannot contain functions".to_owned(),
            ));
        }
        let slot = (bag.to_owned(), key.to_owned());
        if matches!(value, Value::Nil) {
            self.state.borrow_mut().remove(&slot);
        } else {
            self.state.borrow_mut().insert(slot, value.clone());
        }
        for runtime in self.snapshot() {
            runtime.deliver_state_change(bag, key, &value, replicated);
        }
        Ok(())
    }
}

/// Creates a host with a few convars set.
pub fn new_host() -> Rc<TestHost> {
    let convars = HashMap::from([("sv_maxclients".to_owned(), "32".to_owned())]);
    Rc::new(TestHost {
        convars,
        ..TestHost::default()
    })
}

/// Writes `scripts` into a fresh folder, creates a runtime for it, registers it
/// with `host`, and runs its scripts in order.
pub fn start(
    host: &Rc<TestHost>,
    name: &str,
    exports: &[&str],
    scripts: &[(&str, &str)],
) -> Rc<LuaRuntime> {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().to_path_buf();
    for (file, body) in scripts {
        let path = root.join(file);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("script folder");
        }
        fs::write(&path, body).expect("script file");
    }

    let resource: ResourceName = Arc::from(name);
    let instance = host.next_instance.get() + 1;
    host.next_instance.set(instance);
    let concrete: Weak<TestHost> = Rc::downgrade(host);
    let weak: Weak<dyn Host> = concrete;
    let runtime = LuaRuntime::new(
        resource,
        instance,
        root.clone(),
        exports.iter().map(|export| (*export).to_owned()).collect(),
        weak,
    )
    .expect("runtime");

    host.runtimes.borrow_mut().push(Rc::clone(&runtime));
    host.roots
        .borrow_mut()
        .insert(name.to_owned(), root.clone());
    host.dirs.borrow_mut().push(dir);
    for (file, _) in scripts {
        runtime
            .load_script(&root.join(file))
            .unwrap_or_else(|err| panic!("loading {file} in {name}: {err}"));
    }
    runtime
}

/// Runs one frame of every runtime at `now`.
pub fn tick(host: &TestHost, now: Instant) {
    for runtime in host.snapshot() {
        runtime.tick(now);
    }
}

/// Calls an export with no arguments and returns its results.
pub fn call(runtime: &LuaRuntime, export: &str, args: &[Value]) -> Vec<Value> {
    runtime
        .call_export(export, args)
        .unwrap_or_else(|err| panic!("export `{export}` failed: {err}"))
}

/// Calls an export with no arguments and returns its first result.
pub fn first(runtime: &LuaRuntime, export: &str) -> Value {
    call(runtime, export, &[])
        .into_iter()
        .next()
        .unwrap_or(Value::Nil)
}

/// Calls an export that returns a string, as text.
pub fn text(runtime: &LuaRuntime, export: &str) -> String {
    match first(runtime, export) {
        Value::String(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        other => panic!("export `{export}` returned {other:?}, not a string"),
    }
}
