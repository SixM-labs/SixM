//! The background thread that writes key-value logs to disk.
//!
//! The thread owns all file writes and compactions, so the thread that uses
//! the store never waits for the disk. It receives [`Job`]s over a bounded
//! channel, one at a time, in the order they were sent.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};

use tracing::warn;

use crate::log::{Entries, TAG_SET, encode, replay_into};

/// Jobs that can wait for the thread. A producer that finds the channel full
/// keeps its records and tries again later, so this only bounds the memory
/// that is in flight.
pub(crate) const QUEUE_LEN: usize = 64;

/// Unwritten records the thread keeps for a path after a failed write, at most.
const MAX_RETRY_LEN: usize = 8 * 1024 * 1024;

/// Records for one log file.
#[derive(Debug)]
pub(crate) struct Job {
    pub(crate) path: PathBuf,
    /// Encoded records to add to the log.
    pub(crate) records: Vec<u8>,
    /// Rewrite the log into a temporary file that replaces it, instead of
    /// appending. The rewrite keeps the valid records of the log and applies
    /// `records` on top, so dead records and a damaged tail are dropped.
    pub(crate) compact: bool,
}

/// A job the thread did not take.
#[derive(Debug)]
pub(crate) enum Refused {
    /// The channel is full.
    Full(Job),
    /// The thread has stopped.
    Closed(Job),
}

/// The writer thread and the way to reach it.
#[derive(Debug)]
pub(crate) struct Writer {
    pub(crate) sender: SyncSender<Job>,
    pub(crate) thread: JoinHandle<()>,
}

impl Writer {
    /// Starts the thread.
    pub(crate) fn spawn() -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(QUEUE_LEN);
        let thread = thread::Builder::new()
            .name("sixm-kvp-writer".to_owned())
            .spawn(move || run(&receiver))?;
        Ok(Self { sender, thread })
    }

    /// Hands `job` over without waiting.
    pub(crate) fn try_submit(&self, job: Job) -> Result<(), Refused> {
        self.sender.try_send(job).map_err(|err| match err {
            TrySendError::Full(job) => Refused::Full(job),
            TrySendError::Disconnected(job) => Refused::Closed(job),
        })
    }

    /// Hands `job` over, waiting while the channel is full.
    pub(crate) fn submit(&self, job: Job) -> Result<(), Refused> {
        self.sender.send(job).map_err(|err| Refused::Closed(err.0))
    }

    /// Lets the thread write everything it was handed, then stops it.
    pub(crate) fn finish(self) {
        drop(self.sender);
        if self.thread.join().is_err() {
            warn!("the key-value writer thread panicked");
        }
    }
}

fn run(receiver: &Receiver<Job>) {
    // Records of a failed write, by path. A failed append can leave a damaged
    // tail, so the next job for the path rewrites the log with them included.
    let mut failed: HashMap<PathBuf, Vec<u8>> = HashMap::new();
    for mut job in receiver {
        if let Some(mut earlier) = failed.remove(&job.path) {
            earlier.extend_from_slice(&job.records);
            job.records = earlier;
            job.compact = true;
        }
        if let Err(err) = write(&job) {
            warn!("cannot write key-value store {}: {err}", job.path.display());
            let kept = if job.records.len() <= MAX_RETRY_LEN {
                job.records
            } else {
                Vec::new()
            };
            failed.insert(job.path, kept);
        }
    }
}

fn write(job: &Job) -> io::Result<()> {
    if job.compact {
        rewrite(&job.path, &job.records)
    } else {
        append(&job.path, &job.records)
    }
}

fn append(path: &Path, records: &[u8]) -> io::Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    ensure_parent(path)?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(records)
}

/// Replaces the log at `path` with one record per live entry: the valid
/// records of the old log, then `records`. The new log is written and synced
/// under another name first, so the log is never half rewritten.
fn rewrite(path: &Path, records: &[u8]) -> io::Result<()> {
    let old = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(err) => return Err(err),
    };
    let mut entries = Entries::new();
    replay_into(&mut entries, &old);
    drop(old);
    replay_into(&mut entries, records);
    let mut out = Vec::new();
    for (key, value) in &entries {
        encode(&mut out, TAG_SET, key, value);
    }
    ensure_parent(path)?;
    let temp = path.with_extension("kvp.tmp");
    let result = fs::File::create(&temp)
        .and_then(|mut file| {
            file.write_all(&out)?;
            file.sync_all()
        })
        .and_then(|()| fs::rename(&temp, path));
    if result.is_err() {
        // Best effort: the failure to report is the one above.
        let _ = fs::remove_file(&temp);
    }
    result
}

fn ensure_parent(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(parent) => fs::create_dir_all(parent),
        None => Ok(()),
    }
}
