//! Client side of SixM.
//!
//! [`Client`] connects to a server and syncs the client files of every running
//! resource into a content-addressed [`Cache`]: a file the cache already holds
//! is never downloaded again.
//!
//! [`ClientRuntime`] takes that connection and runs the shared and client
//! scripts of each synced resource, in the server's order, with a Lua runtime
//! per resource. Server events and replicated state reach the scripts the way
//! they reach server scripts, and the client's own console and frame loop drive
//! it.

mod cache;
mod client;
mod console;
mod convars;
mod error;
mod host;
mod runtime;
mod state;

pub use cache::Cache;
pub use client::{Client, ClientNotice, SyncStats, SyncedFile, SyncedResource};
pub use error::ClientError;
pub use runtime::{ClientRuntime, Input};
