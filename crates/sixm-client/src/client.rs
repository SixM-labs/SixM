//! A connection to a server that keeps the resources' files in the cache.

use std::collections::VecDeque;
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use sixm_core::{EntityInfo, Value};
use sixm_net::{
    ClientEvent, FILE_KIND_CLIENT, FILE_KIND_FILE, FILE_KIND_SHARED, FileEntry, Fingerprint,
    Identity, NetClient, NetError, ResourceEntry,
};
use sixm_resources::ClientFileKind;

use crate::cache::Cache;
use crate::error::ClientError;

/// Longest a single file download may take.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// A file of a synced resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncedFile {
    /// Name relative to the resource, as the manifest names it.
    pub name: String,
    /// The manifest directive that declared the file.
    pub kind: ClientFileKind,
    /// Where the file is in the cache.
    pub path: PathBuf,
}

/// A running resource whose files are all in the cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncedResource {
    /// Name of the resource.
    pub name: String,
    /// The resource's files, in load order.
    pub files: Vec<SyncedFile>,
    /// Names of the client exports the manifest declares with `export`. A global
    /// function with one of these names is callable as an export.
    pub exports: Vec<String>,
    /// The manifest's metadata pairs, in declaration order, as the server sent them.
    pub metadata: Vec<(String, String)>,
}

/// How many files a sync needed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SyncStats {
    /// Files downloaded from the server.
    pub downloaded: usize,
    /// Files that were already in the cache.
    pub cached: usize,
}

impl SyncStats {
    fn absorb(&mut self, other: SyncStats) {
        self.downloaded += other.downloaded;
        self.cached += other.cached;
    }
}

/// Something the server did, as reported by [`Client::poll`].
#[derive(Debug, Clone, PartialEq)]
pub enum ClientNotice {
    /// A resource started and its files are in the cache.
    ResourceStarted {
        name: String,
        downloaded: usize,
        cached: usize,
    },
    /// A resource started, but its files could not be synced. It is not listed
    /// in [`Client::resources`].
    ResourceFailed { name: String, reason: String },
    /// A resource stopped.
    ResourceStopped { name: String },
    /// The server sent an event.
    Event { name: String, args: Vec<Value> },
    /// A state bag key was set. `value` is nil when the key was deleted.
    StateBag {
        bag: String,
        key: String,
        value: Value,
    },
    /// A state bag was removed, because its player left.
    StateBagRemoved { bag: String },
    /// An entity was created, or was already there when the client joined.
    EntityCreated { id: u32, info: EntityInfo },
    /// New positions and rotations, as `(id, position, rotation)`. Entities the
    /// client does not know are ignored.
    EntityTransforms {
        updates: Vec<(u32, [f32; 3], [f32; 3])>,
    },
    /// An entity was deleted.
    EntityDeleted { id: u32 },
    /// The server kicked the client, or shut down.
    Kicked { reason: String },
    /// The connection ended.
    Disconnected { reason: String },
}

/// A connected client. Dropping it closes the connection.
pub struct Client {
    net: NetClient,
    cache: Cache,
    resources: Vec<SyncedResource>,
    /// Events that arrived while something else was awaited. `poll` handles them first.
    backlog: VecDeque<ClientEvent>,
    stats: SyncStats,
}

impl Client {
    /// Connects as `name` with the certificate of `identity` and syncs the
    /// resources the server advertises: every file missing from the cache in
    /// `cache_dir` is downloaded and verified.
    ///
    /// With a `fingerprint`, only a server whose certificate has that
    /// fingerprint is accepted. `timeout` bounds the handshake and the wait for
    /// the resource list.
    pub fn connect(
        addr: SocketAddr,
        name: &str,
        fingerprint: Option<Fingerprint>,
        identity: &Identity,
        cache_dir: impl Into<PathBuf>,
        timeout: Duration,
    ) -> Result<Self, ClientError> {
        let net = match fingerprint {
            Some(fingerprint) => {
                NetClient::connect_pinned(addr, name, &fingerprint, identity, timeout)?
            }
            None => NetClient::connect(addr, name, identity, timeout)?,
        };
        let mut client = Self {
            net,
            cache: Cache::new(cache_dir),
            resources: Vec::new(),
            backlog: VecDeque::new(),
            stats: SyncStats::default(),
        };
        for entry in client.receive_snapshot(timeout)? {
            let (synced, stats) = client.fetch(&entry)?;
            client.stats.absorb(stats);
            client.resources.push(synced);
        }
        Ok(client)
    }

    /// Player id the server assigned to this client.
    pub fn player(&self) -> u32 {
        self.net.player()
    }

    /// Name the server sent in its welcome.
    pub fn server_name(&self) -> &str {
        self.net.server_name()
    }

    /// Fingerprint of the certificate this client presented to the server.
    pub fn fingerprint(&self) -> Fingerprint {
        self.net.fingerprint()
    }

    /// Sends an event to the server.
    pub fn send_event(&self, name: &str, args: &[Value]) -> Result<(), NetError> {
        self.net.send_event(name, args)
    }

    /// Sends a console line to the server, which runs it with this player's
    /// permissions.
    pub fn send_command(&self, line: &str) -> Result<(), NetError> {
        self.net.send_command(line)
    }

    /// The running resources, in start order, with their files in the cache.
    pub fn resources(&self) -> &[SyncedResource] {
        &self.resources
    }

    /// Totals of the downloads and cache hits since the connection began.
    pub fn stats(&self) -> SyncStats {
        self.stats
    }

    /// Waits up to `timeout` for the next thing the server does and handles
    /// it. A resource start is synced before it is reported. Returns `None`
    /// when nothing was reported in time.
    pub fn poll(&mut self, timeout: Duration) -> Option<ClientNotice> {
        let deadline = Instant::now() + timeout;
        loop {
            let event = match self.backlog.pop_front() {
                Some(event) => event,
                None => self
                    .net
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))?,
            };
            if let Some(notice) = self.handle(event) {
                return Some(notice);
            }
        }
    }

    /// Reads the resource snapshot that follows the welcome, up to its empty
    /// frame. Other events received meanwhile are kept for [`poll`](Self::poll).
    fn receive_snapshot(&mut self, timeout: Duration) -> Result<Vec<ResourceEntry>, ClientError> {
        let deadline = Instant::now() + timeout;
        let mut listed = Vec::new();
        loop {
            let Some(event) = self
                .net
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            else {
                return Err(if Instant::now() >= deadline {
                    ClientError::Net(NetError::Timeout)
                } else {
                    ClientError::Disconnected("the connection ended".to_owned())
                });
            };
            match event {
                ClientEvent::Resources { entries } if entries.is_empty() => return Ok(listed),
                ClientEvent::Resources { entries } => merge_frame(&mut listed, entries),
                ClientEvent::Kicked { reason } => {
                    return Err(ClientError::Disconnected(format!("kicked: {reason}")));
                }
                ClientEvent::Disconnected { reason } => {
                    return Err(ClientError::Disconnected(reason));
                }
                other => self.backlog.push_back(other),
            }
        }
    }

    /// Handles one event. Returns `None` for a resource frame, which only queues
    /// one start per resource for `poll` to handle.
    fn handle(&mut self, event: ClientEvent) -> Option<ClientNotice> {
        let notice = match event {
            ClientEvent::Resources { entries } => {
                self.backlog.extend(
                    entries
                        .into_iter()
                        .map(|entry| ClientEvent::ResourceStarted { entry }),
                );
                return None;
            }
            ClientEvent::ResourceStarted { entry } => self.start(&entry),
            ClientEvent::ResourceStopped { name } => {
                self.resources.retain(|resource| resource.name != name);
                ClientNotice::ResourceStopped { name }
            }
            ClientEvent::Event { name, args } => ClientNotice::Event { name, args },
            ClientEvent::StateBag { bag, key, value } => ClientNotice::StateBag { bag, key, value },
            ClientEvent::StateBagRemoved { bag } => ClientNotice::StateBagRemoved { bag },
            ClientEvent::EntityCreated { entity } => {
                // The network decoder already rejects unknown kinds, so this
                // conversion does not fail for a frame it accepted.
                let Ok((id, info)) = <(u32, EntityInfo)>::try_from(entity) else {
                    return None;
                };
                ClientNotice::EntityCreated { id, info }
            }
            ClientEvent::EntityTransforms { updates } => ClientNotice::EntityTransforms { updates },
            ClientEvent::EntityDeleted { id } => ClientNotice::EntityDeleted { id },
            ClientEvent::EntitySnapshot { entities } => {
                self.backlog.extend(
                    entities
                        .into_iter()
                        .map(|entity| ClientEvent::EntityCreated { entity }),
                );
                return None;
            }
            ClientEvent::Kicked { reason } => ClientNotice::Kicked { reason },
            ClientEvent::Disconnected { reason } => ClientNotice::Disconnected { reason },
        };
        Some(notice)
    }

    /// Syncs a resource that started and lists it. A started resource replaces
    /// an earlier entry of the same name and moves to the end, as on the server.
    fn start(&mut self, entry: &ResourceEntry) -> ClientNotice {
        let name = entry.name.clone();
        match self.fetch(entry) {
            Ok((synced, stats)) => {
                self.stats.absorb(stats);
                self.resources.retain(|resource| resource.name != name);
                self.resources.push(synced);
                ClientNotice::ResourceStarted {
                    name,
                    downloaded: stats.downloaded,
                    cached: stats.cached,
                }
            }
            Err(err) => ClientNotice::ResourceFailed {
                name,
                reason: err.to_string(),
            },
        }
    }

    /// Makes sure every file of `entry` is in the cache.
    fn fetch(&mut self, entry: &ResourceEntry) -> Result<(SyncedResource, SyncStats), ClientError> {
        let mut stats = SyncStats::default();
        let mut files = Vec::with_capacity(entry.files.len());
        for file in &entry.files {
            let kind = kind_of(file.kind)?;
            let (path, downloaded) = self.ensure(file)?;
            if downloaded {
                stats.downloaded += 1;
            } else {
                stats.cached += 1;
            }
            files.push(SyncedFile {
                name: file.name.clone(),
                kind,
                path,
            });
        }
        Ok((
            SyncedResource {
                name: entry.name.clone(),
                files,
                exports: entry.exports.clone(),
                metadata: entry.metadata.clone(),
            },
            stats,
        ))
    }

    /// Returns the cache path of `file` and whether it had to be downloaded.
    fn ensure(&mut self, file: &FileEntry) -> Result<(PathBuf, bool), ClientError> {
        if self.cache.contains(&file.hash) {
            return Ok((self.cache.path(&file.hash), false));
        }
        let bytes = self.net.download(file.hash, file.size, DOWNLOAD_TIMEOUT)?;
        let path = self
            .cache
            .insert(&file.hash, &bytes)
            .map_err(|source| ClientError::Cache {
                path: self.cache.path(&file.hash),
                source,
            })?;
        Ok((path, true))
    }
}

/// Appends one snapshot frame to `listed`. A resource may span frames, so an
/// entry continues the previous one when the names match.
fn merge_frame(listed: &mut Vec<ResourceEntry>, frame: Vec<ResourceEntry>) {
    for entry in frame {
        match listed.last_mut() {
            Some(last) if last.name == entry.name => {
                last.files.extend(entry.files);
                last.exports.extend(entry.exports);
                last.metadata.extend(entry.metadata);
            }
            _ => listed.push(entry),
        }
    }
}

/// The manifest directive behind a wire file kind.
fn kind_of(kind: u8) -> Result<ClientFileKind, ClientError> {
    match kind {
        FILE_KIND_SHARED => Ok(ClientFileKind::SharedScript),
        FILE_KIND_CLIENT => Ok(ClientFileKind::ClientScript),
        FILE_KIND_FILE => Ok(ClientFileKind::File),
        other => Err(ClientError::UnknownKind(other)),
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("net", &self.net)
            .field("resources", &self.resources)
            .finish_non_exhaustive()
    }
}
