//! Error types for manifest evaluation, resource loading and dependency ordering.

use std::io;
use std::path::PathBuf;

use sixm_core::ResourceName;

/// Why a manifest could not be evaluated.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// The Lua source failed to parse or raised an error. The message carries
    /// the chunk name and line number.
    #[error("{0}")]
    Lua(String),
    /// A directive received a value that is not a string, a number, or a list
    /// of those.
    #[error("directive `{directive}` cannot take a {kind} value")]
    UnsupportedValue {
        /// The directive that received the value, after plural normalisation.
        directive: String,
        /// Lua type name of the offending value.
        kind: &'static str,
    },
    /// The manifest executed more Lua instructions than allowed.
    #[error("manifest exceeded the limit of {limit} Lua instructions")]
    InstructionLimit {
        /// The instruction budget that was exhausted.
        limit: u64,
    },
    /// The recorded directives exceeded the entry or byte limit.
    #[error("manifest directives exceeded the output limit")]
    OutputLimit,
}

/// Why a resource folder could not be loaded, or why its scripts could not be
/// expanded.
#[derive(Debug, thiserror::Error)]
pub enum ResourceError {
    /// Reading the folder or one of its files failed.
    #[error("{}: {source}", .path.display())]
    Io {
        /// The file or folder that failed.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// The folder contains neither `fxmanifest.lua` nor `__resource.lua`.
    #[error("{}: no fxmanifest.lua or __resource.lua", .path.display())]
    NoManifest {
        /// The resource folder.
        path: PathBuf,
    },
    /// The folder name is not valid UTF-8, so it cannot be a resource name.
    #[error("{}: folder name is not valid UTF-8", .path.display())]
    InvalidName {
        /// The resource folder.
        path: PathBuf,
    },
    /// The manifest exists but could not be evaluated.
    #[error("{}: {source}", .path.display())]
    Manifest {
        /// The manifest file.
        path: PathBuf,
        /// The evaluation error.
        #[source]
        source: ManifestError,
    },
    /// A literal script path does not name an existing file.
    #[error("`{pattern}` in `{resource}` does not exist")]
    MissingFile {
        /// The resource whose manifest declares the script.
        resource: ResourceName,
        /// The pattern as written in the manifest.
        pattern: String,
    },
    /// A script path is absolute or contains `..`.
    #[error("`{pattern}` in `{resource}` leaves the resource folder")]
    PathEscape {
        /// The resource whose manifest declares the script.
        resource: ResourceName,
        /// The pattern as written in the manifest.
        pattern: String,
    },
    /// A script refers to a resource that is not installed.
    #[error("`{resource}` references unknown resource `{referenced}`")]
    UnknownResource {
        /// The resource whose manifest declares the script.
        resource: ResourceName,
        /// The resource named after `@`.
        referenced: String,
    },
    /// A glob pattern is malformed.
    #[error("invalid pattern `{pattern}`: {message}")]
    BadPattern {
        /// The pattern as written in the manifest.
        pattern: String,
        /// Why the pattern was rejected.
        message: String,
    },
}

/// Why a dependency order could not be computed.
#[derive(Debug, thiserror::Error)]
pub enum DependencyError {
    /// The requested resource is not in the catalog.
    #[error("unknown resource `{0}`")]
    UnknownResource(String),
    /// A resource depends on something that is not installed.
    #[error("`{resource}` depends on `{dependency}`, which is not installed")]
    MissingDependency {
        /// The resource that declares the dependency.
        resource: ResourceName,
        /// The missing resource.
        dependency: String,
    },
    /// Dependencies form a cycle. The path starts and ends with the same resource.
    #[error("dependency cycle: {}", .0.iter().map(AsRef::<str>::as_ref).collect::<Vec<&str>>().join(" -> "))]
    Cycle(Vec<ResourceName>),
}
