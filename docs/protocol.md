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
- The client sends its later messages (`Event`, `RequestFile`, `Command`,
  `EntityTransforms`, `CreateEntity`, `RequestEntityControl`, `SetStateBag`) on that stream.
  The server sends its messages on the send side of the same stream.
- The server opens a unidirectional stream for each requested file. Either peer
  can open a unidirectional stream for a latent event. The first byte identifies
  the stream kind.

## Framing

The bidirectional stream carries length-prefixed frames.

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
| Deferral message length | 4096 bytes, cut on a character boundary when sent |
| Refusal reason length | 256 bytes |
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
| 4 | `EntityTransforms` | list of transforms (see [Entity transforms](#entity-transforms)), at most 1024 |
| 5 | `CreateEntity` | `id: u32`, `kind: u8`, `model: u32`, `position: [f32; 3]`, `rotation: [f32; 3]` |
| 6 | `RequestEntityControl` | `id: u32` |
| 7 | `SetStateBag` | `bag: string`, `key: string`, `value: value` (nil deletes the key) |

`Command` carries a console line typed on the client that the client does not
know. The server runs it with the player's permissions, and it shares the
event rate limit. A line longer than 4096 bytes is a protocol error.

`EntityTransforms` carries the transforms of entities the client owns, such as
its own ped. It carries only the parts that changed, and `epoch` is the latest
epoch the client knows for the entity (see [Teleport epochs](#teleport-epochs)).
The server applies an update only when the entity exists, the player owns it,
every part present is finite and `epoch` is the entity's current epoch. Any
other update is ignored. A message counts as one
event for the rate limit, whatever its size, and a message with more than 1024
updates is a protocol error.

`SetStateBag` asks the server to set a replicated key in a state bag the client
owns (`player:<id>` for the sender, or `entity:<id>` for an entity whose owner
is the sender). `bag` and `key` must each be 1 to 256 bytes, the value must
obey the decoding depth/count limits with no function references, and the frame
must fit within 1 MiB (`MAX_FRAME`); breaking wire limits is a protocol error.
The server rejects writes silently (without kicking) when
`sv_stateBagStrictMode` is enabled, the shared `sv_playerEventRateLimit` token
bucket drops the write, or the client does not own the bag. Accepted writes
are relayed to other eligible clients (excluding the originating client), stored
on the server, and delivered to server state bag change handlers.

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
| 12 | `EntityTransforms` | list of transforms (see [Entity transforms](#entity-transforms)) |
| 13 | `EntityDeleted` | `id: u32` |
| 14 | `EntitySnapshot` | list of entity entries |
| 15 | `Convars` | list of `(name: string, value: string or nil)` (see below) |
| 16 | `Deferral` | `message: string`, at most 4096 bytes |
| 17 | `EntityIdLease` | `first: u32`, `count: u32` |
| 18 | `EntityCreateRefused` | `id: u32`, `reason: string`, at most 256 bytes |
| 19 | `EntityOwner` | `id: u32`, `owner: u32` (`0` for server), `epoch: u32`, `position: [f32; 3]`, `rotation: [f32; 3]` |
| 20 | `EntityStates` | list of delta state updates (see [Entity states](#entity-states)) |

### Convars

`Convars` carries replicated convars. A `nil` value means the server no longer
replicates the convar, and the client removes it. A name is 1 to 256 bytes and a
value is at most 4096 bytes. The decoder rejects a frame that breaks either
limit, and the encoder refuses to build one. Each pair costs one unit of the
decoding budget. A join snapshot may span several frames.

### Handshake

1. The client sends `Hello` with `protocol = 15` and its player name. The
   server has already verified the client certificate, so only a client with a
   certificate gets this far.
2. A different protocol number is refused with `Rejected`, for example
   `unsupported protocol version 7, server speaks 8`.
3. A name outside 1 to 64 characters is refused.
4. The server replies `Welcome`, or `Rejected` with a reason.

   While a script holds the connection with a deferral, the server sends any
   number of `Deferral` messages before that reply. Each carries a progress
   text. The client keeps waiting, and each `Deferral` restarts its handshake
   timeout, so a long queue does not time out. The server never sends
   `Deferral` after `Welcome`.
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
frames. Each entity appears at most once per frame, with its latest transform
and its current epoch. The update carries the position, the rotation or both:
only the parts that changed since the last frame (see
[Entity transforms](#entity-transforms)).

#### Teleport epochs

Every entity has an epoch, which starts at 0 and wraps around at `u32::MAX`.
The server increments it on each move it makes itself (`SetEntityCoords`,
`SetEntityRotation` and related natives on the server). An owner's move never
changes it.

- The server accepts an owner's update only when its epoch equals the entity's
  current epoch. An update computed before a server move therefore never
  lands, whenever it arrives, and the server move stands.
- A client that owns an entity applies a received update only when its epoch
  differs from the one it knows. That is a server move: the client takes each
  part the update carries, records the epoch and drops a local move of that
  part that it has not sent yet. A part the update does not carry keeps the
  client's value, and a local move of that part is still sent, with the new
  epoch. An update with the epoch it already knows is its own move echoed
  back, and is skipped.
- A client that does not own an entity applies the parts of every update that
  carry them, and records the epoch.

### Resource entries

A resource entry has the resource name, the list of its client files, its
client exports and its metadata: the `(key, value)` pairs of the manifest, in
declaration order. Each file has its name, its kind (shared script, client
script or data file), its size and its SHA-256 hash. A client that receives a
`ResourceStarted` replaces any earlier entry with the same name. In a
`Resources` snapshot, the exports and metadata travel once, with the first
part of the entry.

### Entity scoping

With the `sixm_cullRadius` convar above zero, entity messages are sent per
player rather than to everyone. The server keeps, for each player, the set of
entities the player holds, and sends:

- `EntityCreated` when an entity enters the player's scope, followed by a
  `StateBag` for each replicated key of the entity's bag. The join snapshot,
  `EntitySnapshot`, lists the entities in the joining player's scope.
- `EntityDeleted` and `StateBagRemoved` for the entity's bag when an entity
  leaves the player's scope. `EntityDeleted` therefore also means "left your
  scope": the client forgets the entity as if it had been deleted.
- `EntityTransforms`, at most one message per player per frame, with the
  transforms of the entities that player holds.
- `EntityStates`, at most one batch per player per frame, with delta state
  updates for the entities that player holds whose state changed.
- A `StateBag` write to an `entity:<id>` bag reaches only the players that hold
  the entity. Other bags are still sent to every player.

With scoping off (`sixm_cullRadius 0`) every message is sent to every player, as
described above. The message formats are the same either way.

### Entity transforms

`EntityTransforms` (client tag 4, server tag 12) is a variant index, a count
and the updates, each of which is:

| Field | Encoding |
|---|---|
| `id` | varint `u32` |
| `flags` | one byte. Bit 0 set: a position follows. Bit 1 set: a rotation follows. Every other bit must be 0, and at least one of bits 0 and 1 must be set. Any other value rejects the frame. |
| `position` | if bit 0 is set: three zigzag varints, each an `i32` count of centimetres |
| `rotation` | if bit 1 is set: three `u16` little-endian counts of angle units |
| `epoch` | varint `u32` |

A position component is the metres times 100, rounded to the nearest whole
centimetre and clamped to the `i32` range. A receiver divides by 100, so the
error is at most 0.5 cm, and coordinates beyond about 21 474 km are clamped.

A rotation angle is normalized to `[0, 360)` first, so `-90` is sent as `270`
and `450` as `90`. Its count is `round(angle / 360 * 65536) mod 65536`, and a
receiver decodes `count * 360 / 65536` degrees. The error is at most
`360 / 65536`, about 0.0055 degrees, and every decoded angle is in `[0, 360)`.

Each update costs one unit of the decoding budget. A client message carries at
most 1024 updates. Batches longer than that, or longer than one frame, are
split across frames, as with the other entity messages.

### Entity entries

An entity entry is `(id: u32, kind: u8, model: u32, position: 3 floats,
rotation: 3 floats, owner: u32, epoch: u32, state: EntityStateEntry)`. The id is
also the entity's network id. `kind` is 1 for a ped, 2 for a vehicle and 3 for
an object, and any other value is rejected. The model is the `GetHashKey` hash,
and the rotation is pitch, roll and yaw in degrees. `owner` is the id of the
player whose client moves the entity, or `0` when only the server does.
`epoch` is the entity's [teleport epoch](#teleport-epochs). `state` carries
`(frozen: bool, velocity: [f32; 3], health: i32, max_health: i32, visible: bool,
alpha: u8, attachment: Option<(parent: u32, bone_index: i32, offset: [f32; 3],
rotation: [f32; 3])>)`.

### Entity states

`EntityStates` (server tag 20) is a variant index, a count and the delta state
updates, each of which is:

| Field | Encoding |
|---|---|
| `id` | varint `u32` |
| `flags` | one byte bitmask. Bits 0..=6 select `frozen`, `velocity`, `health`, `max_health`, `visible`, `alpha`, `attachment`. Bit 7 must be 0, and at least one bit in `0..=6` must be set. |
| `frozen` | if bit 0 is set: `bool` |
| `velocity` | if bit 1 is set: `[f32; 3]` (all components finite) |
| `health` | if bit 2 is set: zigzag varint `i32` |
| `max_health` | if bit 3 is set: zigzag varint `i32` |
| `visible` | if bit 4 is set: `bool` |
| `alpha` | if bit 5 is set: `u8` |
| `attachment` | if bit 6 is set: `Option<(parent: u32, bone_index: i32, offset: [f32; 3], rotation: [f32; 3])>` (offsets/rotations finite, `parent != id`) |

Each update costs one unit of the decoding budget.

## File transfer

1. The client sends `RequestFile` with the hash it wants.
2. If the hash is advertised, the server opens a unidirectional stream and
   writes:
   - the stream kind as a single byte, `1`
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

## Latent events

Each direction numbers its latent events from zero. A stream starts with kind
`2`, followed by the sequence number as a little-endian `u64` and the encoded
body length as a little-endian `u32`. The body is a postcard tuple containing
the event name and argument list. It can occupy up to 16 MiB. The same name
and value validation applies as for ordinary events.

A sender paces the body at 1 KiB/s through 100 MiB/s. The default is 25,000 bytes per second. The receiver delivers
completed events in sequence order within that direction. Ordinary frames and
resource files can arrive while a latent transfer is in progress.

## Connection limits and kicks

| Limit | Value | Result |
|---|---|---|
| Outbound queue per player | 1024 frames | The player is kicked with `send queue overflow`. |
| Outbound latent queue per player | 16 waiting events | The server kicks the player with `latent queue overflow` if its queue fills. The client API reports a full local queue to its caller. |
| Server latent send requests awaiting dispatch | 16 | Enqueue reports a full queue to the caller. |
| Concurrent incoming latent bodies per connection | 4 | Further streams wait for a receive slot or are refused. |
| Event rate | 100 events, commands, entity messages, or state bag writes per second, burst 200 | Messages over the rate are dropped. The connection stays open. |
| Event flood | More than 1000 events, commands, entity messages, or state bag writes in a second, for five seconds | The player is kicked with `event flood`. |
| Server shutdown | none | Every player is kicked with `server shutting down`. |

## Fuzzing and hardening

The decoders of untrusted input are fuzzed. Two layers run them:

- `cargo test -p sixm-net` feeds 100,000 seeded inputs per run into the
  message decoders and the latent event decoder. Half are random bytes, half
  are valid frames mutated by bit flips, truncation, insertion, splicing and
  oversized length prefixes. Each input must decode or fail, never panic, and
  the value, command and transform limits must hold for every accepted message.
  `cargo test -p sixm-resources` runs 5,000 generated manifests the same way.
- The cargo-fuzz targets in `fuzz/` (outside the workspace, nightly only) cover
  `decode_client_message`, `decode_server_message`, `latent_event`,
  `file_stream_header`, `manifest` (the `fxmanifest.lua` parser), and
  `set_state_bag`. The `fuzz.yml` workflow runs each target for five minutes
  weekly and on demand.

The value budget bounds the values of one message. Strings and byte strings
are read from the frame, so their size cannot exceed it.

## Versioning

- `PROTOCOL_VERSION` is `15`. It is sent in `Hello`. A server refuses any other
  value.
- The ALPN string is `sixm/1`. It does not change with the protocol version.
- Enum order, field order and limits can change between protocol versions.
  Check the version before you decode anything.
