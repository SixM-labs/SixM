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
- Idle timeout is 30 seconds. Keep-alive packets are sent every 5 seconds.

## Streams

- The client opens one bidirectional stream. It sends `Hello` first.
- The client sends its later messages (`Event`, `RequestFile`) on that stream.
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

Function references have no encoding. A peer cannot send one, so it cannot
forge a reference into server code.

### Decoding limits

| Limit | Value |
|---|---|
| Nesting depth of containers | 64 |
| Values in one message, map keys included | 65,536 |
| Event name length | 256 bytes |
| Player name length | 1 to 64 characters, after trimming |
| State bag name and state key length | 256 bytes |
| Files in one resource entry | 256 |
| Export names per resource | 256 |
| Export name length | 256 bytes |
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

### Handshake

1. The client sends `Hello` with `protocol = 2` and its player name.
2. A different protocol number is refused with `Rejected`, for example
   `unsupported protocol version 3, server speaks 2`.
3. A name outside 1 to 64 characters is refused.
4. The server replies `Welcome`, or `Rejected` with a reason.
5. The server sends a `Resources` snapshot. It may span several frames, which
   add entries. An empty `Resources` frame ends the snapshot.
6. The server sends the replicated state as a `StateSnapshot`.

After the handshake, the server sends `ResourceStarted`, `ResourceStopped` and
state messages as they happen.

### Resource entries

A resource entry has the resource name, the list of its client files, and its
client exports. Each file has its name, its kind (shared script, client script
or data file), its size and its SHA-256 hash. A client that receives a
`ResourceStarted` replaces any earlier entry with the same name.

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
| Event rate | 100 events per second, burst 200 | Events over the rate are dropped. The connection stays open. |
| Event flood | More than 1000 events in a second, for five seconds | The player is kicked with `event flood`. |
| Server shutdown | none | Every player is kicked with `server shutting down`. |

## Versioning

- `PROTOCOL_VERSION` is `2`. It is sent in `Hello`. A server refuses any other
  value.
- The ALPN string is `sixm/1`. It does not change with the protocol version.
- Enum order, field order and limits can change between protocol versions.
  Check the version before you decode anything.
