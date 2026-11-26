//! The [`Host`] and [`ClientHost`] services of [`ClientRuntime`], which the
//! script runtimes call back into.

use std::path::PathBuf;

use sixm_core::{
    ClientHost, EventSource, FuncRef, GLOBAL_BAG, Host, ScriptError, ScriptRuntime, ServerHost,
    Value, player_of_bag,
};
use tracing::warn;

use crate::runtime::{ClientRuntime, ScriptCommand};
use crate::state::validate_key;

impl Host for ClientRuntime {
    fn server(&self) -> Option<&dyn ServerHost> {
        None
    }

    fn client(&self) -> Option<&dyn ClientHost> {
        Some(self)
    }

    fn trigger_event(
        &self,
        event: &str,
        source: EventSource,
        args: &[Value],
    ) -> Result<bool, ScriptError> {
        Ok(self.dispatch(event, source, args))
    }

    fn call_export(
        &self,
        resource: &str,
        export: &str,
        args: &[Value],
    ) -> Result<Vec<Value>, ScriptError> {
        let runtime = self
            .running_runtime(resource)
            .ok_or_else(|| ScriptError::ResourceNotRunning(resource.to_owned()))?;
        runtime.call_export(export, args)
    }

    fn call_ref(&self, func: &FuncRef, args: &[Value]) -> Result<Vec<Value>, ScriptError> {
        let runtime = self
            .runtime_of(func)
            .ok_or_else(|| ScriptError::StaleRef(func.clone()))?;
        runtime.call_ref(func.id, args)
    }

    fn retain_ref(&self, func: &FuncRef) -> bool {
        self.runtime_of(func)
            .is_some_and(|runtime| runtime.retain_ref(func.id))
    }

    fn release_ref(&self, func: &FuncRef) {
        if let Some(runtime) = self.runtime_of(func) {
            runtime.release_ref(func.id);
        }
    }

    fn execute_command(&self, line: &str) {
        self.queued.borrow_mut().push_back(line.to_owned());
    }

    fn register_command(&self, name: &str, handler: FuncRef, restricted: bool) {
        if !self.retain_ref(&handler) {
            warn!("command `{name}` ignored: its function is no longer valid");
            return;
        }
        let previous = self.commands.borrow_mut().insert(
            name.to_ascii_lowercase(),
            ScriptCommand {
                handler: handler.clone(),
                restricted,
            },
        );
        if let Some(previous) = previous {
            if previous.handler.owner != handler.owner {
                warn!(
                    "command `{name}` of `{}` replaced by `{}`",
                    previous.handler.owner, handler.owner
                );
            }
            self.release_ref(&previous.handler);
        }
    }

    fn get_convar(&self, name: &str) -> Option<String> {
        self.convars.borrow().get(name).map(str::to_owned)
    }

    /// The synced files live in the content cache under their hashes, so the
    /// client has no folder to report for any resource. A script reads its own
    /// data folder through `LuaRuntime`, which resolves it without this call.
    fn resource_path(&self, _resource: &str) -> Option<PathBuf> {
        None
    }

    fn state_get(&self, bag: &str, key: &str) -> Option<Value> {
        self.state.borrow().get(bag, key).cloned()
    }

    /// Writes local state. Replicated state belongs to the server, so a
    /// replicated write is refused here.
    fn state_set(
        &self,
        bag: &str,
        key: &str,
        value: Value,
        replicated: bool,
    ) -> Result<(), ScriptError> {
        if replicated {
            return Err(ScriptError::Runtime(
                "clients cannot write replicated state".to_owned(),
            ));
        }
        validate_key(key)?;
        if value.contains_func_ref() {
            return Err(ScriptError::Runtime(
                "state values cannot contain functions".to_owned(),
            ));
        }
        if !self.bag_exists(bag) {
            return Err(ScriptError::Runtime(format!(
                "state bag `{bag}` does not exist"
            )));
        }
        self.state.borrow_mut().apply(bag, key, &value);
        self.deliver_state(bag, key, &value, false);
        Ok(())
    }
}

impl ClientRuntime {
    /// Whether a local write to `bag` makes sense: the global bag, this
    /// client's own bag, and any bag the server has replicated keys into.
    fn bag_exists(&self, bag: &str) -> bool {
        bag == GLOBAL_BAG
            || player_of_bag(bag) == Some(self.local_player)
            || self.state.borrow().has_bag(bag)
    }
}

impl ClientHost for ClientRuntime {
    fn trigger_server_event(&self, event: &str, args: &[Value]) -> Result<(), ScriptError> {
        if args.iter().any(Value::contains_func_ref) {
            return Err(ScriptError::Runtime(
                "function references cannot be sent to the server".to_owned(),
            ));
        }
        self.client
            .borrow()
            .send_event(event, args)
            .map_err(|err| ScriptError::Runtime(err.to_string()))
    }

    fn local_player(&self) -> u32 {
        self.local_player
    }
}
