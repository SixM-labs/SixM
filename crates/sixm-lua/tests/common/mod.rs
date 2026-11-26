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
    EventContext, EventSource, FuncRef, Host, ResourceName, ScriptError, ScriptRuntime, Value,
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
