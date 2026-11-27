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

mod adapter;
mod console;
mod entity;
mod files;
mod hash;
pub mod msgpack;
mod runtime;
mod state;
mod value;

pub use adapter::{AdapterError, AdapterSink, GameAdapter, MirrorChange, NativeFn, NativeRegistry};
pub use console::{TokenError, tokenize};
pub use entity::{
    DEFAULT_ENTITY_HEALTH, EntityAttachment, EntityInfo, EntityKind, EntityState, MAX_ENTITIES,
    entity_bag, entity_of_bag, euler_to_quat, is_finite, quat_to_euler,
};
pub use files::{
    MAX_CLIENT_DATA_BYTES, MAX_RESOURCE_FILE_WRITE_BYTES, ResourceDirectory, ScriptLanguage,
    is_protected_resource_file, is_resource_component, normalize_resource_path,
};
pub use hash::joaat;
pub use runtime::{
    ClientHost, ClientTarget, ConvarKind, EventContext, EventSource, Host, HttpRequest, PlayerInfo,
    ResourceState, ScriptError, ScriptRuntime, ServerHost, WebReply, WebRequest, WebResponse,
    convar_filter_matches,
};
pub use state::{GLOBAL_BAG, MAX_STATE_NAME_LEN, player_bag, player_of_bag};
pub use value::{FuncRef, ResourceName, Value};

static GAME_TIMER_START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Milliseconds since the process first asked for the game timer.
pub fn game_timer_ms() -> i64 {
    let elapsed = GAME_TIMER_START
        .get_or_init(std::time::Instant::now)
        .elapsed();
    i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
}
