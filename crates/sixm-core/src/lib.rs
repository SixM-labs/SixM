//! Shared types for SixM.
//!
//! This crate is the contract between the server (which owns resources and
//! routes events) and script runtimes (Lua today, others later). It has no
//! knowledge of any particular game or scripting language.
//!
//! # Threading model
//!
//! Everything here is single-threaded by design: all resources run on the
//! server's main thread, like FXServer's main thread. Runtimes and the host are
//! shared through `Rc`/`Weak`, and every method takes `&self`, so a script can
//! re-enter the host (for example `TriggerEvent` inside an event handler)
//! without fighting the borrow checker. Implementations must never hold a
//! `RefCell` borrow across a call into a runtime or the host.

mod runtime;
mod value;

pub use runtime::{EventContext, EventSource, Host, ScriptError, ScriptRuntime};
pub use value::{FuncRef, ResourceName, Value};
