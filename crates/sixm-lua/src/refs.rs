//! Function references: ids handed to other resources, and the handle userdata
//! that calls them.
//!
//! An entry is created when a function first leaves this runtime, with no
//! retains. Receivers that keep the function retain it; every retain is given
//! back by a release. Entries without retains are swept at the start of the
//! next frame, which is how references that only lived for one synchronous
//! call are freed (see the lifetime rules on [`FuncRef`]).

use std::collections::HashMap;
use std::ffi::c_void;
use std::rc::Weak;

use mlua::{Function, MetaMethod, MultiValue, UserData, UserDataMethods};
use sixm_core::{FuncRef, Host, ScriptError, Value};

use crate::runtime::{IntoScriptResult, LuaRuntime, lua_err};

/// A function handed out to other resources.
struct Entry {
    func: Function,
    /// Retains held by receivers (and pins by this runtime).
    retains: u32,
    /// Identity used to reuse the entry for the same function.
    pointer: *const c_void,
}

/// Functions this runtime has handed out, keyed by id.
#[derive(Default)]
pub(crate) struct RefTable {
    last_id: u32,
    entries: HashMap<u32, Entry>,
    by_pointer: HashMap<*const c_void, u32>,
    /// Entries that had no retains at some point since the last sweep.
    unretained: Vec<u32>,
}

impl RefTable {
    /// Id of `func`, registering it without retains if it is new.
    fn register(&mut self, func: Function) -> Result<u32, ScriptError> {
        let pointer = func.to_pointer();
        if let Some(&id) = self.by_pointer.get(&pointer) {
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
                func,
                retains: 0,
                pointer,
            },
        );
        self.by_pointer.insert(pointer, id);
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
    fn sweep(&mut self) -> Vec<Function> {
        let mut freed = Vec::new();
        for id in std::mem::take(&mut self.unretained) {
            if self
                .entries
                .get(&id)
                .is_some_and(|entry| entry.retains == 0)
            {
                if let Some(entry) = self.entries.remove(&id) {
                    self.by_pointer.remove(&entry.pointer);
                    freed.push(entry.func);
                }
            }
        }
        freed
    }
}

impl LuaRuntime {
    /// The reference a foreign resource uses to call function `id` of this runtime.
    pub(crate) fn func_ref(&self, id: u32) -> FuncRef {
        FuncRef {
            owner: self.resource.clone(),
            instance: self.instance,
            id,
        }
    }

    /// Returns a reference to `func`, valid at least until the next frame.
    pub(crate) fn ref_function(&self, func: Function) -> Result<FuncRef, ScriptError> {
        let id = self.refs.borrow_mut().register(func)?;
        Ok(self.func_ref(id))
    }

    /// Returns a reference to `func` that stays valid until shutdown.
    pub(crate) fn pin_function(&self, func: Function) -> Result<FuncRef, ScriptError> {
        let mut refs = self.refs.borrow_mut();
        let id = refs.register(func)?;
        refs.retain(id);
        Ok(self.func_ref(id))
    }

    /// Returns the function registered under `id`, if it is still alive.
    pub(crate) fn lookup_function(&self, id: u32) -> Option<Function> {
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

    /// Applies queued releases and frees every entry without retains. Runs at
    /// the start of each frame.
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

    /// Calls the function behind `id` synchronously, outside any task.
    pub(crate) fn invoke_ref(&self, id: u32, args: &[Value]) -> Result<Vec<Value>, ScriptError> {
        let func = self
            .lookup_function(id)
            .ok_or_else(|| ScriptError::StaleRef(self.func_ref(id)))?;
        let lua_args = self.to_lua_multi(args).into_script()?;
        let results: MultiValue = func.call(lua_args).into_script()?;
        self.multi_to_values(results)
    }
}

/// Userdata that stands in for a function owned by another resource.
///
/// It holds one retain on the owner's function. Calling it routes through the
/// host; dropping it, when Lua collects it, gives the retain back.
pub(crate) struct FuncHandle {
    pub(crate) host: Weak<dyn Host>,
    pub(crate) func: FuncRef,
    pub(crate) runtime: Weak<LuaRuntime>,
}

impl UserData for FuncHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Call, |_, this, args: MultiValue| {
            let runtime = LuaRuntime::upgrade(&this.runtime)?;
            let host = this
                .host
                .upgrade()
                .ok_or_else(|| mlua::Error::runtime("the server is no longer available"))?;
            let values = runtime.multi_to_values(args).map_err(lua_err)?;
            let results = host.call_ref(&this.func, &values).map_err(lua_err)?;
            runtime.to_lua_multi(&results)
        });
        methods.add_meta_method(MetaMethod::ToString, |_, this, ()| {
            Ok(format!("function: {}#{}", this.func.owner, this.func.id))
        });
    }
}

impl Drop for FuncHandle {
    fn drop(&mut self) {
        if let Some(host) = self.host.upgrade() {
            host.release_ref(&self.func);
        }
    }
}
