//! Sandboxed `QuickJS` JavaScript runtime for SixM with a FiveM-compatible API.
//!
//! Each resource instance gets one isolated `QuickJS` runtime and context,
//! wrapped in [`JsRuntime`]. Scripts use standard JavaScript (`async`/`await`,
//! `Promise`, `setTick`, `setTimeout`, `setInterval`, `Wait`, `exports`,
//! `GlobalState`, and FiveM globals). Values cross resource boundaries as
//! [`sixm_core::Value`], and functions cross as [`sixm_core::FuncRef`].

mod client;
mod convert;
mod entities;
mod events;
mod exports;
mod http;
mod kvp;
mod msgpack;
mod natives;
mod players;
mod refs;
mod resources;
mod runtime;
mod sandbox;
mod scheduler;
mod sql;
mod state;
mod vector;
mod web;

pub use runtime::JsRuntime;
