//! Functions the server itself offers to scripts, such as the `setKickReason`
//! passed to `playerConnecting`.
//!
//! They are not part of any runtime, so they are owned by [`HOST_OWNER`] and
//! removed by the server when their purpose ends. A call after that is a stale
//! reference, the same as for any other function that has gone away.

use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;

use sixm_core::{FuncRef, ScriptError, Value};

/// Owner of host functions. Instance `0` is never given to a runtime, so a
/// reference with this owner can never name a script function.
pub(crate) const HOST_OWNER: &str = "__sixm";

/// A host function: receives the call's arguments and returns its results.
pub(crate) type HostFn = Rc<dyn Fn(&[Value]) -> Result<Vec<Value>, ScriptError>>;

/// The live host functions, by id.
#[derive(Default)]
pub(crate) struct HostFunctions {
    last_id: u32,
    entries: HashMap<u32, HostFn>,
}

impl HostFunctions {
    /// Registers `function` and returns a reference that calls it.
    ///
    /// Ids wrap after 2^32 registrations. A stale reference that old is not a
    /// realistic concern, and the ids are only ever compared against live entries.
    pub(crate) fn add(&mut self, function: HostFn) -> FuncRef {
        let id = self.last_id.wrapping_add(1);
        self.last_id = id;
        self.entries.insert(id, function);
        FuncRef {
            owner: Arc::from(HOST_OWNER),
            instance: 0,
            id,
        }
    }

    /// The function `func` names, if it is a live host function.
    pub(crate) fn get(&self, func: &FuncRef) -> Option<HostFn> {
        if is_host(func) {
            self.entries.get(&func.id).cloned()
        } else {
            None
        }
    }

    /// Whether `func` names a live host function.
    pub(crate) fn contains(&self, func: &FuncRef) -> bool {
        is_host(func) && self.entries.contains_key(&func.id)
    }

    /// Removes the host function `func` names. Other references are ignored.
    pub(crate) fn remove(&mut self, func: &FuncRef) {
        if is_host(func) {
            self.entries.remove(&func.id);
        }
    }
}

impl fmt::Debug for HostFunctions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostFunctions")
            .field("live", &self.entries.len())
            .finish_non_exhaustive()
    }
}

/// Whether `func` is owned by the host rather than by a runtime.
pub(crate) fn is_host(func: &FuncRef) -> bool {
    func.instance == 0 && func.owner.as_ref() == HOST_OWNER
}
