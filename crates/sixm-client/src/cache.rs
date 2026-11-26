//! The content-addressed store of downloaded resource files.

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use sixm_net::{FileHash, hash_hex};
use tempfile::NamedTempFile;

/// Files stored by hash: `<root>/<first two hex digits>/<all 64 hex digits>`.
///
/// Names come from the hash alone, so the cache never uses a name the server
/// chose and a file cannot escape the cache folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cache {
    root: PathBuf,
}

impl Cache {
    /// A cache in `root`. The folder is created when the first file is stored.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Where the file with `hash` lives, whether or not it is stored yet.
    pub fn path(&self, hash: &FileHash) -> PathBuf {
        let hex = hash_hex(hash);
        self.root.join(&hex[..2]).join(&hex)
    }

    /// Whether the file with `hash` is stored.
    pub fn contains(&self, hash: &FileHash) -> bool {
        self.path(hash).is_file()
    }

    /// Stores `bytes` under `hash` and returns the path of the stored file.
    ///
    /// The bytes are written to a temporary file in the same folder and then
    /// renamed into place, so a reader sees either no file or the whole file.
    /// The caller guarantees that `bytes` hash to `hash`; a download verifies
    /// that before it gets here.
    pub fn insert(&self, hash: &FileHash, bytes: &[u8]) -> io::Result<PathBuf> {
        let path = self.path(hash);
        let folder = path.parent().unwrap_or(&self.root);
        fs::create_dir_all(folder)?;
        let mut temporary = NamedTempFile::new_in(folder)?;
        temporary.write_all(bytes)?;
        temporary.persist(&path).map_err(|err| err.error)?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::Cache;

    #[test]
    fn files_are_stored_under_their_hash_in_a_two_digit_folder() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cache = Cache::new(dir.path());
        let hash = [0xab; 32];
        let path = cache.path(&hash);
        let hex = "ab".repeat(32);
        assert_eq!(path, dir.path().join("ab").join(&hex));
        assert!(!cache.contains(&hash));

        let stored = cache.insert(&hash, b"hello").expect("insert");
        assert_eq!(stored, path);
        assert!(cache.contains(&hash));
        assert_eq!(std::fs::read(&path).expect("read back"), b"hello");
    }

    #[test]
    fn insert_replaces_a_file_atomically_and_leaves_no_temporaries() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cache = Cache::new(dir.path());
        let hash = [0x01; 32];
        cache.insert(&hash, b"first").expect("first insert");
        cache.insert(&hash, b"second").expect("second insert");
        assert_eq!(std::fs::read(cache.path(&hash)).expect("read"), b"second");

        let folder = cache.path(&hash).parent().expect("folder").to_path_buf();
        let entries: Vec<_> = std::fs::read_dir(folder)
            .expect("list folder")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "only the stored file remains: {entries:?}"
        );
    }
}
