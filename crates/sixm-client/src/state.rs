//! The client's copy of the state bags: what the server replicated, plus the
//! values scripts wrote locally.

use std::collections::HashMap;

use sixm_core::{MAX_STATE_NAME_LEN, ScriptError, Value};

/// Every bag the client knows, and the keys set in each.
#[derive(Debug, Default)]
pub(crate) struct StateMirror {
    bags: HashMap<String, HashMap<String, Value>>,
}

impl StateMirror {
    /// The value of `key` in `bag`.
    pub(crate) fn get(&self, bag: &str, key: &str) -> Option<&Value> {
        self.bags.get(bag)?.get(key)
    }

    /// Whether the client holds any key of `bag`.
    pub(crate) fn has_bag(&self, bag: &str) -> bool {
        self.bags.contains_key(bag)
    }

    /// Stores `value` under `key`, or deletes the key when `value` is nil.
    pub(crate) fn apply(&mut self, bag: &str, key: &str, value: &Value) {
        if matches!(value, Value::Nil) {
            if let Some(keys) = self.bags.get_mut(bag) {
                keys.remove(key);
            }
        } else {
            self.bags
                .entry(bag.to_owned())
                .or_default()
                .insert(key.to_owned(), value.clone());
        }
    }

    /// Forgets `bag` and every key in it.
    pub(crate) fn remove_bag(&mut self, bag: &str) {
        self.bags.remove(bag);
    }
}

/// Fails unless `key` is 1 to [`MAX_STATE_NAME_LEN`] bytes long.
pub(crate) fn validate_key(key: &str) -> Result<(), ScriptError> {
    if key.is_empty() || key.len() > MAX_STATE_NAME_LEN {
        return Err(ScriptError::Runtime(format!(
            "state key must be 1 to {MAX_STATE_NAME_LEN} bytes, got {}",
            key.len()
        )));
    }
    Ok(())
}
