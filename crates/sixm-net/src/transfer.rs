//! Serving advertised files. The server answers a `RequestFile` with a
//! unidirectional stream that carries the file's hash, its size and its bytes.
//! Only hashes in the advertised set are served, and the path comes from that
//! set, never from the peer.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, PoisonError, RwLock};

use quinn::{Connection, SendStream, VarInt};
use tokio::io::AsyncReadExt;
use tokio::sync::{Semaphore, mpsc};
use tracing::{debug, warn};

use crate::files::{FileHash, PublishedResource, ResourceEntry};
use crate::protocol::{MAX_FRAME, STREAM_KIND_FILE, ServerMessage};
use crate::traffic::Counters;
use crate::wire::encode_frame;

/// Bytes read from disk and written to the stream at a time.
const CHUNK_LEN: usize = 64 * 1024;

/// Bytes of the header that starts every file stream: the 32-byte hash, then
/// the size as a `u64` in little-endian order.
pub(crate) const HEADER_LEN: usize = 32 + 8;

/// Splits the header of a file stream into the file's hash and its size.
pub(crate) fn parse_file_header(header: &[u8; HEADER_LEN]) -> (FileHash, u64) {
    let mut hash = [0_u8; 32];
    hash.copy_from_slice(&header[..32]);
    let mut size = [0_u8; 8];
    size.copy_from_slice(&header[32..]);
    (hash, u64::from_le_bytes(size))
}

/// Application error code for a stream that could not carry the whole file.
const TRANSFER_ABORTED: u32 = 2;

/// Where an advertised file lives on the server's disk.
#[derive(Debug, Clone)]
struct Source {
    path: PathBuf,
    size: u64,
}

/// The set a server advertises: its resources in start order, and the files of
/// those resources by hash.
#[derive(Debug, Default)]
pub(crate) struct Advertised {
    /// Wire form of the resources, in start order.
    pub(crate) resources: Vec<ResourceEntry>,
    files: HashMap<FileHash, Source>,
}

impl Advertised {
    /// The set that advertises `resources`. If two files have the same hash,
    /// the first one is served.
    pub(crate) fn new(resources: Vec<PublishedResource>) -> Self {
        let mut files = HashMap::new();
        let mut listed = Vec::with_capacity(resources.len());
        for resource in resources {
            let mut entries = Vec::with_capacity(resource.files.len());
            for file in resource.files {
                files.entry(file.entry.hash).or_insert_with(|| Source {
                    path: file.path,
                    size: file.entry.size,
                });
                entries.push(file.entry);
            }
            listed.push(ResourceEntry {
                name: resource.name,
                files: entries,
                exports: resource.exports,
                metadata: resource.metadata,
            });
        }
        Self {
            resources: listed,
            files,
        }
    }

    fn source(&self, hash: &FileHash) -> Option<Source> {
        self.files.get(hash).cloned()
    }
}

/// Everything a transfer task needs. The task owns it.
pub(crate) struct Transfer {
    pub(crate) connection: Connection,
    pub(crate) advertised: Arc<RwLock<Advertised>>,
    /// Permits for concurrent transfers to this peer.
    pub(crate) slots: Arc<Semaphore>,
    /// The peer's outbound queue, for the reply to an unknown hash.
    pub(crate) reply: mpsc::Sender<Arc<[u8]>>,
    /// The server's traffic totals.
    pub(crate) traffic: Arc<Counters>,
}

/// Serves the file with `hash`. The advertised set is checked when the request
/// arrives and again once a transfer slot is free, so a file unpublished in
/// the meantime is reported as unavailable.
pub(crate) async fn serve_file(transfer: Transfer, hash: FileHash) {
    let Transfer {
        connection,
        advertised,
        slots,
        reply,
        traffic,
    } = transfer;
    if lookup(&advertised, &hash).is_none() {
        return unavailable(&reply, hash).await;
    }
    let Ok(_slot) = slots.acquire_owned().await else {
        return;
    };
    let Some(source) = lookup(&advertised, &hash) else {
        return unavailable(&reply, hash).await;
    };
    let mut file = match tokio::fs::File::open(&source.path).await {
        Ok(file) => file,
        Err(err) => {
            warn!(path = %source.path.display(), %err, "cannot open an advertised file");
            return unavailable(&reply, hash).await;
        }
    };
    let Ok(mut stream) = connection.open_uni().await else {
        return;
    };
    let mut header = [0_u8; HEADER_LEN + 1];
    header[0] = STREAM_KIND_FILE;
    header[1..33].copy_from_slice(&hash);
    header[33..].copy_from_slice(&source.size.to_le_bytes());
    if stream.write_all(&header).await.is_err() {
        return;
    }
    traffic.sent(HEADER_LEN + 1);
    if copy_exact(&mut file, &mut stream, source.size, &traffic).await {
        let _ = stream.finish();
    } else {
        debug!("a file transfer was aborted before the advertised size");
        let _ = stream.reset(VarInt::from_u32(TRANSFER_ABORTED));
    }
}

fn lookup(advertised: &RwLock<Advertised>, hash: &FileHash) -> Option<Source> {
    advertised
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .source(hash)
}

/// Tells the peer on the main stream that `hash` is not advertised.
async fn unavailable(reply: &mpsc::Sender<Arc<[u8]>>, hash: FileHash) {
    if let Ok(frame) = encode_frame(&ServerMessage::FileUnavailable { hash }, MAX_FRAME) {
        let _ = reply.send(frame).await;
    }
}

/// Copies exactly `size` bytes from `file` to `stream`, reading in chunks. Returns
/// false if the file ends early or either side fails.
async fn copy_exact(
    file: &mut tokio::fs::File,
    stream: &mut SendStream,
    size: u64,
    traffic: &Counters,
) -> bool {
    let mut buffer = vec![0_u8; CHUNK_LEN];
    let mut remaining = size;
    while remaining > 0 {
        let want = usize::try_from(remaining).map_or(CHUNK_LEN, |left| left.min(CHUNK_LEN));
        let read = match file.read(&mut buffer[..want]).await {
            Ok(0) | Err(_) => return false,
            Ok(read) => read,
        };
        if stream.write_all(&buffer[..read]).await.is_err() {
            return false;
        }
        traffic.sent_data(read);
        remaining = remaining.saturating_sub(u64::try_from(read).unwrap_or(u64::MAX));
    }
    true
}
