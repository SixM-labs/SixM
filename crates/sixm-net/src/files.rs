//! Resource files as the protocol describes them: what a server advertises,
//! what a client learns, and the limits that keep transfers bounded.

use std::fmt::Write as _;
use std::path::PathBuf;

use serde::Serialize;

use crate::error::NetError;
use crate::wire::validate_length;

/// SHA-256 digest of a file's contents. It names the file on the wire.
pub type FileHash = [u8; 32];

/// Largest file a server advertises or a client downloads, in bytes.
pub const MAX_FILE_SIZE: u64 = 64 * 1024 * 1024;

/// Longest resource name in bytes.
pub const MAX_RESOURCE_NAME_LEN: usize = 256;

/// Longest file name in bytes.
pub const MAX_FILE_NAME_LEN: usize = 1024;

/// [`FileEntry::kind`] of a file declared in `shared_scripts`.
pub const FILE_KIND_SHARED: u8 = 0;

/// [`FileEntry::kind`] of a file declared in `client_scripts`.
pub const FILE_KIND_CLIENT: u8 = 1;

/// [`FileEntry::kind`] of a file declared in `files`.
pub const FILE_KIND_FILE: u8 = 2;

/// One file of a resource, as the wire carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileEntry {
    /// `/`-separated name relative to the resource, as the manifest names it.
    pub name: String,
    /// One of [`FILE_KIND_SHARED`], [`FILE_KIND_CLIENT`] or [`FILE_KIND_FILE`].
    pub kind: u8,
    /// SHA-256 of the contents.
    pub hash: FileHash,
    /// Size of the contents in bytes.
    pub size: u64,
}

/// The client files of one running resource, in load order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResourceEntry {
    /// Name of the resource.
    pub name: String,
    /// The resource's files, in load order.
    pub files: Vec<FileEntry>,
}

/// A file a server advertises: its wire entry and where it is on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedFile {
    /// What the file is called and how to verify it.
    pub entry: FileEntry,
    /// Absolute path of the file on the server's disk.
    pub path: PathBuf,
}

/// A running resource and the files it advertises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedResource {
    /// Name of the resource.
    pub name: String,
    /// The resource's files, in load order.
    pub files: Vec<PublishedFile>,
}

/// Lowercase hexadecimal text of `hash`, as used in logs and cache paths.
pub fn hash_hex(hash: &FileHash) -> String {
    hash.iter()
        .fold(String::with_capacity(hash.len() * 2), |mut text, byte| {
            let _ = write!(text, "{byte:02x}");
            text
        })
}

/// Checks that `resources` can be advertised: names within their limits, known
/// kinds, sizes within [`MAX_FILE_SIZE`] and absolute paths.
pub(crate) fn check_published(resources: &[PublishedResource]) -> Result<(), NetError> {
    for resource in resources {
        validate_length("resource name", &resource.name, MAX_RESOURCE_NAME_LEN)?;
        for file in &resource.files {
            validate_length("file name", &file.entry.name, MAX_FILE_NAME_LEN)?;
            if file.entry.kind > FILE_KIND_FILE {
                return Err(NetError::InvalidInput(format!(
                    "file `{}` has unknown kind {}",
                    file.entry.name, file.entry.kind
                )));
            }
            if file.entry.size > MAX_FILE_SIZE {
                return Err(NetError::InvalidInput(format!(
                    "file `{}` is {} bytes, more than the {MAX_FILE_SIZE} byte limit",
                    file.entry.name, file.entry.size
                )));
            }
            if !file.path.is_absolute() {
                return Err(NetError::InvalidInput(format!(
                    "file `{}` has a relative path {}",
                    file.entry.name,
                    file.path.display()
                )));
            }
        }
    }
    Ok(())
}
