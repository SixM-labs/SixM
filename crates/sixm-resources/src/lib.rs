//! Resource discovery, `fxmanifest.lua` evaluation and dependency ordering.
//!
//! A resource is a folder with a `fxmanifest.lua` (or legacy `__resource.lua`).
//! The manifest is evaluated in a sandbox, so scripts that FiveM resources
//! declare load unchanged. [`Catalog`] finds resources under one or more roots
//! and orders them by their `dependencies`.

mod catalog;
mod error;
mod manifest;
mod resource;

pub use catalog::{Catalog, ScanIssue};
pub use error::{DependencyError, ManifestError, ResourceError};
pub use manifest::{Manifest, parse_manifest};
pub use resource::{ClientFile, ClientFileKind, ResourceInfo};
