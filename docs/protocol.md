# Network protocol

This page describes the protocol as the code implements it, for people who
write tools that talk to a SixM server. Source: `crates/sixm-net/src/`
(`protocol.rs`, `wire.rs`, `transfer.rs`, `files.rs`, `identity.rs`,
`ratelimit.rs`, `connection.rs`).

The format may change between protocol versions. Do not rely on it outside a
given `PROTOCOL_VERSION`.

## Transport

- **QUIC** over UDP, using `quinn`. The default port is 30120.
- **TLS 1.3** through `rustls`. The server presents a self-signed certificate.
  The server name in the handshake is `localhost`. Clients do not check the
  name.
- **ALPN** is `sixm/1`. Both sides set it in the TLS configuration.
- The server's identity can be pinned by SHA-256 fingerprint of the DER
  certificate. Without a pin, the client accepts any certificate. See
  [server.md](server.md) for the fingerprint.
- **Client certificate.** The server requires every client to present a
  certificate in the TLS handshake. The server does not check it against an
  authority. The client proves that it holds the key by signing the handshake,
  and the server verifies that signature with the crypto provider's algorithms,
  as the client does for the server. The SHA-256 fingerprint of the certificate
  is the player's `license` identifier. A connection without a verified
  certificate is closed before its `Hello` is read.
- Idle timeout is 30 seconds. Keep-alive packets are sent every 5 seconds.

## Streams

- The client opens one bidirectional stream. It sends `Hello` first.
- The client sends its later messages (`Event`, `RequestFile`, `Command`) on
  that stream.
  The server sends its messages on the send side of the same stream.
- The server opens a **unidirectional** stream for each file transfer. See
  below.

## Framing

Every message on a stream is a frame:

```text
+----------------------+-----------------------------+
| length: u32 LE       | payload: postcard encoded   |
+----------------------+-----------------------------+
```

- The length is the payload size in bytes, not counting the four-byte prefix.
- The payload is at most 1 MiB (`MAX_FRAME`). The same limit applies to frames
  that a server sends to a client.
- Payloads use `postcard`. Enums are encoded by variant index, so the order of
  variants is part of the format.

## Values

Script values inside events and state are encoded as a `Value` enum with these
variants, in this order:

| Index | Variant | Payload |
|---|---|---|
| 0 | `Nil` | none |
| 1 | `Bool` | boolean |
| 2 | `Int` | 64-bit integer |
| 3 | `Float` | 64-bit float |
| 4 | `String` | raw bytes |
| 5 | `Array` | sequence of values |
| 6 | `Map` | sequence of key and value pairs |
| 7 | `Vector2` | two 32-bit floats |
| 8 | `Vector3` | three 32-bit floats |
| 9 | `Vector4` | four 32-bit floats |

Function references have no encoding. A peer cannot send one, so it cannot
forge a reference into server code.

### Decoding limits

| Limit | Value |
|---|---|
| Nesting depth of containers | 64 |
| Values in one message, map keys included | 65,536 (a vector, an entity entry and an entity update count as one each) |
| Event name length | 256 bytes |
| Player name length | 1 to 64 characters, after trimming |
| State bag name and state key length | 256 bytes |
| Files in one resource entry | 256 |
| Export names per resource | 256 |
| Export name length | 256 bytes |
| Metadata pairs per resource | 1024 |
| Metadata key length | 1 to 256 bytes |
| Metadata value length | 4096 bytes |
| Resource name length | 256 bytes |
| Client file name length | 1024 bytes |
| File size | 64 MiB |

A message that breaks a limit is rejected. A protocol error closes the
connection.

## Messages

Client to server (`ClientMessage`):

| Tag | Message | Fields |
|---|---|---|
| 0 | `Hello` | `protocol: u32`, `name: string` |
| 1 | `Event` | `name: string`, `args: values` |
| 2 | `RequestFile` | `hash: 32 bytes` |
| 3 | `Command` | `line: string`, at most 4096 bytes |

`Command` carries a console line typed on the client that the client does not
know. The server runs it with the player's permissions, and it shares the
event rate limit. A line longer than 4096 bytes is a protocol error.

Server to client (`ServerMessage`):

| Tag | Message | Fields |
|---|---|---|
| 0 | `Welcome` | `player: u32`, `server_name: string` |
| 1 | `Rejected` | `reason: string` |
| 2 | `Event` | `name: string`, `args: values` |
| 3 | `Kicked` | `reason: string` |
| 4 | `StateBag` | `bag`, `key`, `value` (nil deletes the key) |
| 5 | `StateBagRemoved` | `bag: string` |
| 6 | `StateSnapshot` | list of `(bag, key, value)` |
| 7 | `Resources` | list of resource entries (see below) |
| 8 | `ResourceStarted` | one resource entry |
| 9 | `ResourceStopped` | `name: string` |
| 10 | `FileUnavailable` | `hash: 32 bytes` |
| 11 | `EntityCreated` | one entity entry (see below) |
| 12 | `EntityTransforms` | list of `(id: u32, position: 3 floats, rotation: 3 floats)` |
| 13 | `EntityDeleted` | `id: u32` |
| 14 | `EntitySnapshot` | list of entity entries |
| 15 | `Convars` | list of `(name: string, value: string or nil)` (see below) |

### Convars

`Convars` carries replicated convars. A `nil` value means the server no longer
replicates the convar, and the client removes it. A name is 1 to 256 bytes and a
value is at most 4096 bytes. The decoder rejects a frame that breaks either
limit, and the encoder refuses to build one. Each pair costs one unit of the
decoding budget. A join snapshot may span several frames.

### Handshake

1. The client sends `Hello` with `protocol = 7` and its player name. The
   server has already verified the client certificate, so only a client with a
   certificate gets this far.
2. A different protocol number is refused with `Rejected`, for example
   `unsupported protocol version 6, server speaks 7`.
3. A name outside 1 to 64 characters is refused.
4. The server replies `Welcome`, or `Rejected` with a reason.
5. The server sends every replicated convar as `Convars`, when there are any.
   It may span several frames. Without replicated convars, no frame is sent.
6. The server sends every existing entity as an `EntitySnapshot`, which may
   span several frames. With no entities, no frame is sent.
7. The server sends the replicated state as a `StateSnapshot`.
8. The server sends a `Resources` snapshot. It may span several frames, which
   add entries. An empty `Resources` frame ends the snapshot and the join.
   Everything before it describes the world the client's scripts start in.

After the handshake, the server sends `ResourceStarted`, `ResourceStopped`,
state and entity messages as they happen. `EntityCreated` and `EntityDeleted`
are sent at once. Moves are collected, and once per server frame every entity
that moved is sent once, with its latest transform, in `EntityTransforms`
frames.

### Resource entries

A resource entry has the resource name, the list of its client files, its
client exports and its metadata: the `(key, value)` pairs of the manifest, in
declaration order. Each file has its name, its kind (shared script, client
script or data file), its size and its SHA-256 hash. A client that receives a
`ResourceStarted` replaces any earlier entry with the same name. In a
`Resources` snapshot, the exports and metadata travel once, with the first
part of the entry.

### Entity entries

An entity entry is `(id: u32, kind: u8, model: u32, position: 3 floats,
rotation: 3 floats)`. The id is also the entity's network id. `kind` is 1 for a
ped, 2 for a vehicle and 3 for an object, and any other value is rejected. The
model is the `GetHashKey` hash, and the rotation is pitch, roll and yaw in
degrees.

## File transfer

1. The client sends `RequestFile` with the hash it wants.
2. If the hash is advertised, the server opens a unidirectional stream and
   writes:
   - the 32-byte hash
   - the size as a `u64`, little-endian
   - the file bytes, in chunks of 64 KiB
3. If not, the server sends `FileUnavailable` with the hash.
4. The client checks the size and the SHA-256 hash of the bytes before it uses
   them.

Rules on the server:

- Only hashes in the advertised set are served. The path comes from the set,
  never from the request.
- A player may have at most 8 transfers running at once. Further requests wait.
- A player may have at most 256 requests queued. More than that kicks the
  player with `too many file requests`.

Clients keep each file in a cache under its hash. See [client.md](client.md).

## Connection limits and kicks

| Limit | Value | Result |
|---|---|---|
| Outbound queue per player | 1024 frames | The player is kicked with `send queue overflow`. |
| Event rate | 100 events or commands per second, burst 200 | Events and commands over the rate are dropped. The connection stays open. |
| Event flood | More than 1000 events or commands in a second, for five seconds | The player is kicked with `event flood`. |
| Server shutdown | none | Every player is kicked with `server shutting down`. |

## Versioning

- `PROTOCOL_VERSION` is `5`. It is sent in `Hello`. A server refuses any other
  value.
- The ALPN string is `sixm/1`. It does not change with the protocol version.
- Enum order, field order and limits can change between protocol versions.
  Check the version before you decode anything.
