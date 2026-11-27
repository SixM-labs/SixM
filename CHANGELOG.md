# Changelog

All notable changes to SixM are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
follows [Semantic Versioning](https://semver.org/). Before 1.0, minor versions
may change the protocol and the API.

## [Unreleased]

### Added

- Frame and per-resource profiling with `profiler record [frames]`,
  `profiler stop`, `profiler status`, and Chrome Trace export via
  `profiler saveJSON <path>`.
- Authenticated remote console on `POST /rcon` with a console-only
  `rcon_password`, per-address failure lockout, and JSON command output.

- FiveM scripting compatibility (`PROTOCOL_VERSION` 14):
  - Lua compile-time backtick JOAAT hash literals (`` `adder` ``) with comment/string awareness and line-number preservation.
  - `quat(w, x, y, z)` (`Value::Quat`) across Lua, JavaScript, exports/state bags/network, plus `GetEntityQuaternion` and `SetEntityQuaternion`.
  - Convar change listeners (`AddConvarChangeListener` with `*`/`?` glob filtering and `RemoveConvarChangeListener`) on server and client.
  - CFX and OneSync natives on Lua & JS: `GetRegisteredCommands`, `StartResource`/`StopResource` (server, ACE-gated via `resource.<caller>`), `GetEntityScript`, `NetworkGetFirstEntityOwner`, `SetEntityDistanceCullingRadius`, `GetPlayerLastMsg`, `GetNumPlayerTokens`, and `GetPlayerToken`.

## [0.1.0]

First release. There is no GTA VI game client yet (waiting on a PC release);
the dedicated server, Lua 5.4 and JavaScript scripting runtimes, HTTP/JSON
endpoints, networking, persistence, routing buckets, and headless/simulated
client (`SimAdapter`) are complete.

### Added

- Game adapter contract (`sixm-core::adapter`: `GameAdapter`, `MirrorChange`,
  `AdapterError`, `AdapterSink`, `NativeRegistry`), adapter native installation
  and `Citizen.InvokeNative` (plus global `InvokeNative` in JS) on client
  runtimes when an adapter is attached, `ClientRuntime` tick/mirror hooks, and
  built-in Phase 1 `SimAdapter` (`--sim` CLI flag in `sixm-client`) with
  `GetEntitySpeed`, `TaskGoStraightToCoord`, and `GetSimEntityCoords`.
- JavaScript scripting runtime (`sixm-js`) backed by QuickJS (`rquickjs`),
  supporting `.js` scripts in `server_scripts`, `client_scripts`, and
  `shared_scripts` alongside `.lua` (including mixed Lua/JS resources):
  `setTick`/`clearTick`, `setTimeout`/`clearTimeout`, `setInterval`/`clearInterval`,
  async `Delay`/`Wait`, `on`/`onNet`/`emit`/`emitNet` + FiveM aliases, `exports`
  cross-runtime dispatch, `GlobalState`/`Player`/`LocalPlayer`/`Entity` state bags
  and change handlers, `vector2`/`vector3`/`vector4` (`Vector*`), KVP, resource
  natives, entities/state/buckets, `PerformHttpRequest`, and `SetHttpHandler`.
  Runs inside a strict memory/stack/operation-budget sandbox with `eval` and
  `Function` disabled.
- Per-entity replicated state (`PROTOCOL_VERSION` 13): `FreezeEntityPosition` /
  `IsEntityPositionFrozen`, `SetEntityVelocity` / `GetEntityVelocity`,
  `SetEntityHealth` / `GetEntityHealth` / `SetEntityMaxHealth` /
  `GetEntityMaxHealth` (default `200`), `SetEntityVisible` / `IsEntityVisible`,
  `SetEntityAlpha` / `GetEntityAlpha` / `ResetEntityAlpha` (clamped `0..=255`),
  and `AttachEntityToEntity` / `DetachEntity` / `GetEntityAttachedTo` /
  `IsEntityAttached`. Snapshots include full entity state, and per-frame delta
  updates (`EntityStates`) replicate only changed fields within scope and
  routing buckets. Frozen and attached entities refuse independent owner moves,
  attached children follow parent movement, and deleting a parent
  automatically detaches its children.
- HTTP on the server's TCP port (the game address): the built-in `/info.json`,
  `/players.json` and `/dynamic.json`, and `SetHttpHandler` for resources under
  `/<resource>/...`. Request bodies, connections, timeouts and the request rate
  per client address are limited. `sv_exposePlayerIdentifiers` controls the
  identifiers in `/players.json`, `sv_httpEnabled` switches HTTP off and
  `sixm_httpHandlerTimeout` sets the handler timeout. A busy TCP port leaves the
  server running without HTTP.
- Resource key-value storage (`sixm-kvp`) on the server and the client:
  `SetResourceKvp`, the typed variants, `GetResourceKvp*`, `DeleteResourceKvp`,
  `StartFindKvp`/`FindKvp`/`EndFindKvp` and `FlushResourceKvp`. Reads stay in
  memory; a background thread appends CRC-checked records to one log per
  resource and compacts it, so the frame never waits for the disk. The server
  folder is `sixm_kvpPath`.
- Client-created networked entities and ownership migration: clients can call
  `CreateVehicle`, `CreatePed` and `CreateObject` (`isNetwork = true`) using
  leased id blocks so the returned id is usable immediately, plus
  `NetworkRequestControlOfEntity` and `NetworkGetEntityOwner`. The server
  enforces `sixm_maxClientEntities` (default 64, replicated),
  `sv_entityLockdown` and per-bucket lockdown modes, and fires the cancellable
  `entityCreating` event; `SetEntityOwner` and `SetEntityOrphanMode` control
  ownership and orphan handling, migrating ownership to the nearest in-scope
  player or the server while preserving teleport epochs.
- Routing buckets on the server: `SetPlayerRoutingBucket`,
  `GetPlayerRoutingBucket`, `SetEntityRoutingBucket`, `GetEntityRoutingBucket`,
  `GetPlayersInRoutingBucket`, and the stored population and entity lockdown
  settings. A player holds only the entities, transforms and entity state of its
  own bucket, and its ped moves with it. Bucket 0 costs nothing extra.

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

- There is no GTA VI game adapter yet; `sixm-client` runs headless by default
  or with the built-in simulated adapter (`--sim`).
- `LoadResourceFile` on the client only sees the resource's own data folder.

[Unreleased]: https://github.com/SixM-labs/SixM/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/SixM-labs/SixM/releases/tag/v0.1.0
