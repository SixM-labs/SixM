# Changelog

All notable changes to SixM are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
follows [Semantic Versioning](https://semver.org/). Before 1.0, minor versions
may change the protocol and the API.

## [Unreleased]

## [0.1.0]

First release. There is no game adapter yet: the server, the scripting runtime
and the headless client are complete, and a game client is planned.

### Added

- Game-independent core contract (`sixm-core`): script values, the `Host`,
  `ServerHost` and `ClientHost` traits, the `ScriptRuntime` trait, entities
  and access control.
- Resource system (`sixm-resources`): discovery, `fxmanifest.lua` parsing with
  glob patterns, metadata, and dependency ordering. Manifests cannot hang the
  server.
- Sandboxed Lua 5.4 runtime (`sixm-lua`) with one isolated state per resource
  and a FiveM-style API: `CreateThread`, `Wait`, events, exports,
  `RegisterCommand`, promises and `Citizen.Await`, `json`, `vector2`,
  `vector3`, `vector4`, `GetHashKey`, resource natives and
  `GetInvokingResource`. `source` stays correct across `Wait`.
- Server binary (`sixm-server`): `server.cfg`, convars (including replicated
  `setr` convars), console commands, resource manager, ACE permissions with
  commands sent by clients, and `ExecuteCommand` running with the rights of the
  calling resource.
- Networking (`sixm-net`): QUIC transport with encryption, a versioned binary
  protocol, a decode budget against oversized input, optional server
  certificate pinning, content-addressed file transfer, and paced latent events.
- Players: server ids, connection lifecycle events (`playerConnecting`,
  `playerJoining`, `playerDropped`), deferrals, kicking, round-trip time, and
  identifiers derived from a client certificate that the client proves it holds.
- Net events in both directions between server and clients.
- State bags: `GlobalState`, `Player(id).state`, change handlers and
  replication to clients.
- Entities: server-owned entities and a ped per player that the client moves,
  compact transform sync that sends only changed, quantized values, and entity
  scoping so that players receive only nearby entities.
- Headless client (`sixm-client`): resource sync, client scripts, client
  exports, and resource file access.
- Performance tooling: frame and traffic measurement with the `perf` console
  command, and a load test with a swarm of bot clients.
- Documentation in `docs/`, a design document for game adapters, and a
  runnable example server in `examples/server-data`.
- Release workflow that builds the server and the client for Linux and Windows.

### Known gaps

- There is no game adapter. The client is headless.
- `LoadResourceFile` on the client only sees the resource's own data folder.

[Unreleased]: https://github.com/SixM-labs/SixM/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/SixM-labs/SixM/releases/tag/v0.1.0
