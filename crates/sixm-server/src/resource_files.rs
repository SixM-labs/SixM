//! Client files of resources: their hashes, and how they are listed to clients.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf, absolute};
use std::time::SystemTime;

use ring::digest::{Context, SHA256};
use sixm_net::{
    FILE_KIND_CLIENT, FILE_KIND_FILE, FILE_KIND_SHARED, FileEntry, FileHash, MAX_FILE_SIZE,
    PublishedFile, PublishedResource, ResourceEntry,
};
use sixm_resources::{ClientFile, ClientFileKind};
use tracing::warn;

/// Bytes read at a time while hashing, so that no file is held in memory whole.
const HASH_CHUNK_LEN: usize = 64 * 1024;

/// A hash remembered for a file, with the metadata it was computed from.
#[derive(Debug, Clone, Copy)]
struct Known {
    size: u64,
    modified: SystemTime,
    hash: FileHash,
}

/// Hashes of client files, remembered by path. A file whose size and
/// modification time are unchanged keeps its hash, so restarting a resource
/// does not read its files again.
#[derive(Debug, Default)]
pub(crate) struct FileHashes {
    known: HashMap<PathBuf, Known>,
}

impl FileHashes {
    /// Describes `file` for clients: its size and hash, computed or remembered.
    /// Returns `None`, with a warning, for a file that cannot be read or is
    /// larger than [`MAX_FILE_SIZE`].
    pub(crate) fn publish(&mut self, file: ClientFile) -> Option<PublishedFile> {
        // Clients never see paths, but the network checks that they are absolute.
        let path = match absolute(&file.path) {
            Ok(path) => path,
            Err(err) => {
                warn!("skipping client file {}: {err}", file.path.display());
                return None;
            }
        };
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(err) => {
                warn!("skipping client file {}: {err}", path.display());
                return None;
            }
        };
        let size = metadata.len();
        if size > MAX_FILE_SIZE {
            warn!(
                "skipping client file {}: {size} bytes is over the {MAX_FILE_SIZE} byte limit",
                path.display()
            );
            return None;
        }
        let hash = match self.remembered(&path, size, metadata.modified().ok()) {
            Some(hash) => hash,
            None => match hash_file(&path) {
                Ok(hash) => {
                    if let Ok(modified) = metadata.modified() {
                        self.known.insert(
                            path.clone(),
                            Known {
                                size,
                                modified,
                                hash,
                            },
                        );
                    }
                    hash
                }
                Err(err) => {
                    warn!("skipping client file {}: {err}", path.display());
                    return None;
                }
            },
        };
        Some(PublishedFile {
            entry: FileEntry {
                name: file.name,
                kind: kind_code(file.kind),
                hash,
                size,
            },
            path,
        })
    }

    /// The hash of `path` if it was computed from a file with this size and
    /// modification time.
    fn remembered(&self, path: &Path, size: u64, modified: Option<SystemTime>) -> Option<FileHash> {
        let modified = modified?;
        self.known
            .get(path)
            .filter(|known| known.size == size && known.modified == modified)
            .map(|known| known.hash)
    }
}

/// The wire entry of a resource as clients see it.
pub(crate) fn listing(resource: &PublishedResource) -> ResourceEntry {
    ResourceEntry {
        name: resource.name.clone(),
        files: resource
            .files
            .iter()
            .map(|file| file.entry.clone())
            .collect(),
    }
}

/// The wire code of a manifest directive.
fn kind_code(kind: ClientFileKind) -> u8 {
    match kind {
        ClientFileKind::SharedScript => FILE_KIND_SHARED,
        ClientFileKind::ClientScript => FILE_KIND_CLIENT,
        ClientFileKind::File => FILE_KIND_FILE,
    }
}

/// SHA-256 of the file at `path`, read in chunks.
fn hash_file(path: &Path) -> io::Result<FileHash> {
    let mut file = File::open(path)?;
    let mut context = Context::new(&SHA256);
    let mut buffer = vec![0_u8; HASH_CHUNK_LEN];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        context.update(&buffer[..read]);
    }
    let digest = context.finish();
    let mut hash = [0_u8; 32];
    hash.copy_from_slice(digest.as_ref());
    Ok(hash)
}

/// SHA-256 of `bytes`, for callers that already hold the contents.
#[cfg(test)]
fn hash_bytes(bytes: &[u8]) -> FileHash {
    let digest = ring::digest::digest(&SHA256, bytes);
    let mut hash = [0_u8; 32];
    hash.copy_from_slice(digest.as_ref());
    hash
}

#[cfg(test)]
mod tests {
    use std::fs;

    use sixm_net::FILE_KIND_SHARED;
    use sixm_resources::{ClientFile, ClientFileKind};

    use super::{FileHashes, hash_bytes};

    fn client_file(path: &std::path::Path) -> ClientFile {
        ClientFile {
            path: path.to_path_buf(),
            name: "main.lua".to_owned(),
            kind: ClientFileKind::SharedScript,
        }
    }

    #[test]
    fn published_file_carries_size_kind_and_hash() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("main.lua");
        fs::write(&path, b"print(1)").expect("write");
        let published = FileHashes::default()
            .publish(client_file(&path))
            .expect("published");
        assert_eq!(published.entry.name, "main.lua");
        assert_eq!(published.entry.kind, FILE_KIND_SHARED);
        assert_eq!(published.entry.size, 8);
        assert_eq!(published.entry.hash, hash_bytes(b"print(1)"));
    }

    #[test]
    fn unchanged_file_is_not_read_again() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("main.lua");
        fs::write(&path, b"abcd").expect("write");
        let mut hashes = FileHashes::default();
        let first = hashes.publish(client_file(&path)).expect("published");

        // A remembered hash is served without reading the file, so a file whose
        // contents were swapped for the same size and mtime keeps its old hash.
        let modified = fs::metadata(&path)
            .expect("metadata")
            .modified()
            .expect("mtime");
        fs::write(&path, b"wxyz").expect("rewrite");
        fs::File::options()
            .write(true)
            .open(&path)
            .expect("open")
            .set_modified(modified)
            .expect("restore mtime");
        let second = hashes.publish(client_file(&path)).expect("published");
        assert_eq!(second.entry.hash, first.entry.hash);
    }

    #[test]
    fn changed_file_is_hashed_again() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("main.lua");
        fs::write(&path, b"abcd").expect("write");
        let mut hashes = FileHashes::default();
        hashes.publish(client_file(&path)).expect("published");
        fs::write(&path, b"changed contents, longer").expect("rewrite");
        let again = hashes.publish(client_file(&path)).expect("published");
        assert_eq!(again.entry.hash, hash_bytes(b"changed contents, longer"));
    }

    #[test]
    fn relative_paths_are_published_as_absolute_paths() {
        // Tests run in the crate folder, so `Cargo.toml` is a relative path that exists.
        let published = FileHashes::default()
            .publish(client_file(std::path::Path::new("Cargo.toml")))
            .expect("published");
        assert!(published.path.is_absolute());
        let contents = fs::read("Cargo.toml").expect("manifest is readable");
        assert_eq!(published.entry.hash, hash_bytes(&contents));
    }

    #[test]
    fn missing_file_is_skipped() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("gone.lua");
        assert!(FileHashes::default().publish(client_file(&path)).is_none());
    }
}
