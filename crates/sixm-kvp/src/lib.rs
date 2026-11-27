//! Key-value storage that a resource keeps across restarts, behind
//! `SetResourceKvp` and the other KVP natives.
//!
//! Every resource has one file, `<resource>.kvp`, in the store's folder. The
//! file is an append-only log of records (see the `log` module for the
//! format). The store keeps the entries of every resource that uses it in
//! memory, so reads never touch the disk after the first use of a resource.
//!
//! The thread that owns the store only records changes in memory. Once per
//! [`FLUSH_INTERVAL`] it hands the changes of each resource, as one batch, to
//! a background writer thread through a bounded channel. If the channel is
//! full the batch stays in memory and grows with later changes, so the owner
//! is never blocked by a slow disk. The writer appends the batches, and
//! rewrites a log into a temporary file that replaces it when the log has
//! grown to more than twice the size of its live entries. A log whose tail is
//! truncated or corrupt is read up to the last good record and rewritten.
//!
//! The writer thread is started on the first batch, and [`KvpStore::shutdown`]
//! (also run when the store is dropped) writes everything and joins it.

mod log;
mod writer;

use std::collections::HashMap;
use std::fs;
use std::io;
use std::ops::Bound;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tracing::warn;

use log::{Entries, TAG_DELETE, TAG_SET, encode, len_u64, record_len, replay_into};
use writer::{Job, Refused, Writer};

/// Longest key in bytes.
pub const MAX_KEY_LEN: usize = 256;
/// Largest value in bytes.
pub const MAX_VALUE_LEN: usize = 64 * 1024;
/// Shortest time between two hand-overs of the pending changes to the writer.
pub const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

/// Logs smaller than this are never rewritten, however many dead records they hold.
const COMPACT_MIN_LEN: u64 = 64 * 1024;

/// The key-value store of a host: one log per resource in a folder.
#[derive(Debug)]
pub struct KvpStore {
    root: PathBuf,
    resources: HashMap<String, Namespace>,
    /// Some namespace has changes the writer does not have yet.
    dirty: bool,
    last_flush: Option<Instant>,
    writer: Option<Writer>,
    /// The store was shut down, or its writer could not be started: changes
    /// stay in memory only.
    closed: bool,
}

/// The loaded entries of one resource, and the state of its log file.
#[derive(Debug)]
struct Namespace {
    path: PathBuf,
    entries: Entries,
    /// Bytes the log file has once the writer has everything handed to it.
    log_len: u64,
    /// Bytes that a freshly written log of the live entries would take.
    live_len: u64,
    /// Encoded records the writer does not have yet.
    pending: Vec<u8>,
    /// The log has a damaged tail that the next write must rewrite away.
    repair: bool,
}

/// How handing a namespace to the writer ended.
enum Handover {
    Done,
    Full,
    Closed,
}

impl KvpStore {
    /// A store whose logs live in `root`. The folder is created on the first write.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            resources: HashMap::new(),
            dirty: false,
            last_flush: None,
            writer: None,
            closed: false,
        }
    }

    /// The value stored under `key` for `resource`, or `None`.
    pub fn get(&mut self, resource: &str, key: &str) -> Option<Vec<u8>> {
        let namespace = self.namespace(resource)?;
        namespace.entries.get(key).cloned()
    }

    /// Stores `value` under `key` for `resource`. Returns `false`, and stores
    /// nothing, when the key is not 1 to [`MAX_KEY_LEN`] bytes, the value is
    /// larger than [`MAX_VALUE_LEN`], or the resource name cannot name a file.
    /// The write reaches the writer thread with the next flush.
    pub fn set(&mut self, resource: &str, key: &str, value: &[u8]) -> bool {
        if !(1..=MAX_KEY_LEN).contains(&key.len()) || value.len() > MAX_VALUE_LEN {
            return false;
        }
        let Some(namespace) = self.namespace(resource) else {
            return false;
        };
        let record = record_len(key, value);
        if let Some(current) = namespace.entries.get_mut(key) {
            namespace.live_len = namespace.live_len - record_len(key, current) + record;
            current.clear();
            current.extend_from_slice(value);
        } else {
            namespace.live_len += record;
            namespace.entries.insert(key.to_owned(), value.to_vec());
        }
        encode(&mut namespace.pending, TAG_SET, key, value);
        self.dirty = true;
        true
    }

    /// Removes `key` for `resource`. Returns whether it was set.
    pub fn delete(&mut self, resource: &str, key: &str) -> bool {
        let Some(namespace) = self.namespace(resource) else {
            return false;
        };
        let Some(current) = namespace.entries.remove(key) else {
            return false;
        };
        namespace.live_len -= record_len(key, &current);
        encode(&mut namespace.pending, TAG_DELETE, key, &[]);
        self.dirty = true;
        true
    }

    /// The keys of `resource` that start with `prefix`, in byte order.
    pub fn keys(&mut self, resource: &str, prefix: &str) -> Vec<String> {
        let Some(namespace) = self.namespace(resource) else {
            return Vec::new();
        };
        namespace
            .entries
            .range::<str, _>((Bound::Included(prefix), Bound::Unbounded))
            .take_while(|(key, _)| key.starts_with(prefix))
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// Hands the pending changes to the writer thread if the last hand-over
    /// was at least [`FLUSH_INTERVAL`] ago, or if there has been none. Call it
    /// once per frame; it costs one flag check when nothing changed, and it
    /// never waits for the disk.
    pub fn flush_due(&mut self, now: Instant) {
        if !self.dirty {
            return;
        }
        if self
            .last_flush
            .is_some_and(|last| now.saturating_duration_since(last) < FLUSH_INTERVAL)
        {
            return;
        }
        if self.hand_over() {
            self.last_flush = Some(now);
        }
    }

    /// Hands the pending changes of every resource to the writer thread now,
    /// without waiting for the disk. Changes that do not fit into the writer's
    /// queue are handed over by a later [`flush_due`](Self::flush_due).
    pub fn flush(&mut self) {
        if self.hand_over() {
            self.last_flush = Some(Instant::now());
        }
    }

    /// Hands every pending change to the writer, waits until the writer has
    /// written all of it, and stops the writer. Later changes stay in memory.
    /// This waits for the disk, so call it only when the host stops.
    pub fn shutdown(&mut self) {
        if self.closed {
            return;
        }
        if self.dirty && self.ensure_writer() {
            if let Some(writer) = &self.writer {
                for namespace in self.resources.values_mut() {
                    namespace.hand_over(|job| writer.submit(job));
                }
            }
            self.dirty = false;
        }
        if let Some(writer) = self.writer.take() {
            writer.finish();
        }
        self.closed = true;
    }

    /// Hands every namespace with changes to the writer without waiting.
    /// Returns whether all of them were handed over.
    fn hand_over(&mut self) -> bool {
        if !self.dirty || !self.ensure_writer() {
            return !self.dirty;
        }
        let Some(writer) = &self.writer else {
            return false;
        };
        let mut all = true;
        let mut stopped = false;
        for namespace in self.resources.values_mut() {
            match namespace.hand_over(|job| writer.try_submit(job)) {
                Handover::Done => {}
                Handover::Full => all = false,
                Handover::Closed => {
                    all = false;
                    stopped = true;
                    break;
                }
            }
        }
        if stopped && let Some(writer) = self.writer.take() {
            // A writer that stopped on its own is replaced on the next hand-over.
            writer.finish();
        }
        self.dirty = !all;
        all
    }

    /// Starts the writer thread if it is not running. Returns whether there is
    /// one. A thread that cannot be started closes the store.
    fn ensure_writer(&mut self) -> bool {
        if self.closed {
            return false;
        }
        if self.writer.is_none() {
            match Writer::spawn() {
                Ok(writer) => self.writer = Some(writer),
                Err(err) => {
                    warn!("cannot start the key-value writer, changes are not saved: {err}");
                    self.closed = true;
                    return false;
                }
            }
        }
        true
    }

    /// The loaded namespace of `resource`, loading it on first use.
    fn namespace(&mut self, resource: &str) -> Option<&mut Namespace> {
        if !valid_resource(resource) {
            return None;
        }
        if !self.resources.contains_key(resource) {
            let path = self.root.join(format!("{resource}.kvp"));
            match Namespace::open(path) {
                Ok(namespace) => {
                    self.dirty |= namespace.repair;
                    self.resources.insert(resource.to_owned(), namespace);
                }
                Err(err) => {
                    warn!("cannot read key-value store of `{resource}`: {err}");
                    return None;
                }
            }
        }
        self.resources.get_mut(resource)
    }
}

impl Drop for KvpStore {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Namespace {
    /// Reads the log at `path`. A missing file is an empty store.
    fn open(path: PathBuf) -> io::Result<Self> {
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(err) => return Err(err),
        };
        let mut entries = Entries::new();
        let good_len = replay_into(&mut entries, &bytes);
        let live_len = entries
            .iter()
            .map(|(key, value)| record_len(key, value))
            .sum();
        let repair = good_len != bytes.len();
        if repair {
            warn!(
                "{} has a damaged tail after {good_len} bytes; it will be rewritten",
                path.display()
            );
        }
        Ok(Self {
            path,
            entries,
            log_len: len_u64(good_len),
            live_len,
            pending: Vec::new(),
            repair,
        })
    }

    /// Hands the pending records, or the repair of a damaged tail, to the
    /// writer through `submit`. On a refusal the records stay pending.
    fn hand_over(&mut self, submit: impl FnOnce(Job) -> Result<(), Refused>) -> Handover {
        if self.pending.is_empty() && !self.repair {
            return Handover::Done;
        }
        let total = self.log_len + len_u64(self.pending.len());
        let compact = self.repair || (total > COMPACT_MIN_LEN && total > 2 * self.live_len);
        let job = Job {
            path: self.path.clone(),
            records: std::mem::take(&mut self.pending),
            compact,
        };
        match submit(job) {
            Ok(()) => {
                self.log_len = if compact { self.live_len } else { total };
                self.repair = false;
                Handover::Done
            }
            Err(Refused::Full(job)) => {
                self.pending = job.records;
                Handover::Full
            }
            Err(Refused::Closed(job)) => {
                self.pending = job.records;
                Handover::Closed
            }
        }
    }
}

/// Resource names become file names, so only plain names are stored: ASCII
/// letters, digits, `-`, `_` and `.`, without a leading dot.
fn valid_resource(resource: &str) -> bool {
    !resource.is_empty()
        && !resource.starts_with('.')
        && resource
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Instant;

    use super::{FLUSH_INTERVAL, KvpStore, MAX_KEY_LEN, MAX_VALUE_LEN, Writer};
    use crate::log::{Entries, replay_into};

    #[test]
    fn values_survive_a_restart_after_a_shutdown() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut store = KvpStore::new(dir.path());
        assert!(store.set("res", "a", b"one"));
        assert!(store.set("res", "a", b"two"));
        assert!(store.set("res", "gone", b"x"));
        assert!(store.delete("res", "gone"));
        store.shutdown();

        let mut again = KvpStore::new(dir.path());
        assert_eq!(again.get("res", "a"), Some(b"two".to_vec()));
        assert_eq!(again.get("res", "gone"), None);
        assert_eq!(again.keys("res", ""), vec!["a".to_owned()]);
    }

    #[test]
    fn dropping_the_store_writes_its_changes() {
        let dir = tempfile::tempdir().expect("temp dir");
        {
            let mut store = KvpStore::new(dir.path());
            assert!(store.set("res", "a", b"one"));
            store.flush();
            assert!(store.set("res", "b", b"two"));
        }
        let mut again = KvpStore::new(dir.path());
        assert_eq!(again.keys("res", ""), vec!["a", "b"]);
    }

    #[test]
    fn unflushed_changes_are_not_on_disk() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut store = KvpStore::new(dir.path());
        assert!(store.set("res", "a", b"one"));
        let mut again = KvpStore::new(dir.path());
        assert_eq!(again.get("res", "a"), None);
        // Neither store is shut down yet, so neither has written anything.
        again.shutdown();
        assert!(!dir.path().join("res.kvp").exists());
    }

    #[test]
    fn resources_are_separate() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut store = KvpStore::new(dir.path());
        assert!(store.set("one", "k", b"1"));
        assert_eq!(store.get("two", "k"), None);
        let none: Vec<String> = Vec::new();
        assert_eq!(store.keys("two", ""), none);
    }

    #[test]
    fn prefix_listing_is_sorted_and_bounded() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut store = KvpStore::new(dir.path());
        for key in ["player:2", "player:1", "playerx", "other", "player:10"] {
            assert!(store.set("res", key, b"v"));
        }
        assert_eq!(
            store.keys("res", "player:"),
            vec!["player:1", "player:10", "player:2"]
        );
        assert_eq!(
            store.keys("res", "player"),
            vec!["player:1", "player:10", "player:2", "playerx"]
        );
        let none: Vec<String> = Vec::new();
        assert_eq!(store.keys("res", "nothing"), none);
    }

    #[test]
    fn keys_and_values_have_limits() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut store = KvpStore::new(dir.path());
        let long_key = "k".repeat(MAX_KEY_LEN + 1);
        assert!(!store.set("res", "", b"v"));
        assert!(!store.set("res", &long_key, b"v"));
        assert!(store.set("res", &"k".repeat(MAX_KEY_LEN), b"v"));
        assert!(!store.set("res", "big", &vec![0_u8; MAX_VALUE_LEN + 1]));
        assert!(store.set("res", "big", &vec![7_u8; MAX_VALUE_LEN]));
        assert!(!store.set("..", "k", b"v"));
        assert!(!store.set("a/b", "k", b"v"));
        assert_eq!(
            store.get("res", "big").map(|value| value.len()),
            Some(MAX_VALUE_LEN)
        );
    }

    #[test]
    fn a_truncated_tail_keeps_the_records_before_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut store = KvpStore::new(dir.path());
        assert!(store.set("res", "kept", b"yes"));
        assert!(store.set("res", "lost", b"no"));
        store.shutdown();
        let path = dir.path().join("res.kvp");
        let bytes = fs::read(&path).expect("log");
        fs::write(&path, &bytes[..bytes.len() - 1]).expect("truncate");

        let mut again = KvpStore::new(dir.path());
        assert_eq!(again.get("res", "kept"), Some(b"yes".to_vec()));
        assert_eq!(again.get("res", "lost"), None);
        // The damaged tail is cut off before new records follow valid ones.
        assert!(again.set("res", "after", b"ok"));
        again.shutdown();
        let mut third = KvpStore::new(dir.path());
        assert_eq!(third.get("res", "after"), Some(b"ok".to_vec()));
        assert_eq!(third.get("res", "kept"), Some(b"yes".to_vec()));
    }

    #[test]
    fn a_damaged_tail_is_repaired_even_without_new_writes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut store = KvpStore::new(dir.path());
        assert!(store.set("res", "kept", b"yes"));
        assert!(store.set("res", "lost", b"no"));
        store.shutdown();
        let path = dir.path().join("res.kvp");
        let bytes = fs::read(&path).expect("log");
        fs::write(&path, &bytes[..bytes.len() - 1]).expect("truncate");

        let mut again = KvpStore::new(dir.path());
        assert_eq!(again.get("res", "kept"), Some(b"yes".to_vec()));
        again.shutdown();
        let repaired = fs::read(&path).expect("log");
        let mut entries = Entries::new();
        assert_eq!(replay_into(&mut entries, &repaired), repaired.len());
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn a_corrupt_record_ends_the_log() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut store = KvpStore::new(dir.path());
        assert!(store.set("res", "first", b"1"));
        assert!(store.set("res", "second", b"2"));
        store.shutdown();
        let path = dir.path().join("res.kvp");
        let mut bytes = fs::read(&path).expect("log");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        fs::write(&path, &bytes).expect("corrupt");

        let mut again = KvpStore::new(dir.path());
        assert_eq!(again.get("res", "first"), Some(b"1".to_vec()));
        assert_eq!(again.get("res", "second"), None);
    }

    #[test]
    fn hand_overs_are_batched_to_one_per_interval() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut store = KvpStore::new(dir.path());
        let start = Instant::now();
        assert!(store.set("res", "a", b"1"));
        store.flush_due(start);
        assert!(store.resources["res"].pending.is_empty());

        assert!(store.set("res", "b", b"2"));
        store.flush_due(start + FLUSH_INTERVAL / 2);
        assert!(!store.resources["res"].pending.is_empty());
        assert!(store.dirty);

        store.flush_due(start + FLUSH_INTERVAL);
        assert!(store.resources["res"].pending.is_empty());
        assert!(!store.dirty);
        store.shutdown();

        let mut again = KvpStore::new(dir.path());
        assert_eq!(again.keys("res", ""), vec!["a", "b"]);
    }

    #[test]
    fn a_full_queue_never_blocks_and_coalesces_the_changes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut store = KvpStore::new(dir.path());
        // A writer whose queue holds one job and that nobody reads.
        let (sender, receiver) = mpsc::sync_channel(1);
        store.writer = Some(Writer {
            sender,
            thread: thread::spawn(|| {}),
        });

        assert!(store.set("res", "a", b"1"));
        store.flush();
        assert!(!store.dirty, "the first job fits into the queue");

        assert!(store.set("res", "b", b"2"));
        store.flush();
        assert!(store.set("res", "c", b"3"));
        store.flush();
        assert!(store.dirty, "the queue is full, so the changes wait");

        let first = receiver.recv().expect("first job");
        let mut entries = Entries::new();
        replay_into(&mut entries, &first.records);
        assert_eq!(entries.keys().collect::<Vec<_>>(), ["a"]);

        store.flush();
        assert!(!store.dirty);
        let second = receiver.recv().expect("second job");
        let mut entries = Entries::new();
        replay_into(&mut entries, &second.records);
        assert_eq!(entries.keys().collect::<Vec<_>>(), ["b", "c"]);
    }

    #[test]
    fn rewriting_keeps_only_live_entries_and_replaces_the_old_log() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut store = KvpStore::new(dir.path());
        let value = vec![3_u8; 8 * 1024];
        let start = Instant::now();
        for round in 0..40_u32 {
            assert!(store.set("res", "hot", &value));
            store.flush_due(start + FLUSH_INTERVAL * round);
        }
        store.shutdown();
        // 40 records of 8 KiB would be 320 KiB; rewrites keep the log near the
        // 64 KiB minimum instead. Each rewrite renames over the existing log.
        let size = fs::metadata(dir.path().join("res.kvp")).expect("log").len();
        assert!(size < 80 * 1024, "log holds {size} bytes");
        assert!(!dir.path().join("res.kvp.tmp").exists());

        let mut again = KvpStore::new(dir.path());
        assert_eq!(again.get("res", "hot"), Some(value));
    }

    #[test]
    fn a_rewrite_replaces_an_existing_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut store = KvpStore::new(dir.path());
        assert!(store.set("res", "old", b"1"));
        store.shutdown();
        let path = dir.path().join("res.kvp");
        assert!(path.exists());

        // Damage the tail so that the next write rewrites into a temporary
        // file and renames it over the existing log.
        let mut bytes = fs::read(&path).expect("log");
        bytes.extend_from_slice(b"junk");
        fs::write(&path, &bytes).expect("damage");
        let mut again = KvpStore::new(dir.path());
        assert!(again.set("res", "new", b"2"));
        again.shutdown();

        assert!(!dir.path().join("res.kvp.tmp").exists());
        let mut third = KvpStore::new(dir.path());
        assert_eq!(third.keys("res", ""), vec!["new", "old"]);
    }

    #[test]
    fn invalid_resource_names_are_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut store = KvpStore::new(dir.path());
        assert_eq!(store.get(".hidden", "k"), None);
        assert!(!store.delete("", "k"));
        assert!(!store.set("sub/res", "k", b"v"));
        assert!(!dir.path().join("sub").exists());
    }
}
