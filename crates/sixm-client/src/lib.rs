//! Client side of SixM.
//!
//! [`Client`] connects to a server and syncs the client files of every running
//! resource into a content-addressed [`Cache`]: a file the cache already holds
//! is never downloaded again. Running the client scripts is not implemented
//! yet; the files are only put in place, in load order.

mod cache;
mod client;
mod error;

pub use cache::Cache;
pub use client::{Client, ClientNotice, SyncStats, SyncedFile, SyncedResource};
pub use error::ClientError;
