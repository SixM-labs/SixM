//! Shared types for SixM.
//!
//! This crate is the contract between the host (the server, or a client that
//! joins one) and script runtimes (Lua today, others later). It has no
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

mod console;
mod entity;
mod files;
mod hash;
mod runtime;
mod state;
mod value;

pub use console::{TokenError, tokenize};
pub use entity::{EntityInfo, EntityKind, MAX_ENTITIES, entity_bag, entity_of_bag, is_finite};
pub use files::normalize_resource_path;
pub use hash::joaat;
pub use runtime::{
    ClientHost, ClientTarget, ConvarKind, EventContext, EventSource, Host, HttpRequest, PlayerInfo,
    ResourceState, ScriptError, ScriptRuntime, ServerHost, WebReply, WebRequest, WebResponse,
};
pub use state::{GLOBAL_BAG, MAX_STATE_NAME_LEN, player_bag, player_of_bag};
pub use value::{FuncRef, ResourceName, Value};
