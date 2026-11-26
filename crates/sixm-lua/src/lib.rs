//! Lua 5.4 script runtime for SixM with a FiveM-compatible API.
//!
//! Each resource instance gets one sandboxed Lua state, wrapped in
//! [`LuaRuntime`]. Scripts suspend with `Wait`, and the scheduler resumes them
//! on later frames or timers. Values cross resource boundaries as
//! [`sixm_core::Value`], and functions cross as [`sixm_core::FuncRef`].

mod client;
mod convert;
mod events;
mod exports;
mod json;
mod natives;
mod players;
mod refs;
mod runtime;
mod sandbox;
mod scheduler;
mod state;

pub use runtime::LuaRuntime;
