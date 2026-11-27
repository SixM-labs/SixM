# Migrating from FiveM

SixM is designed so that developers who already know FiveM can write and port
server logic with minimal changes. The scripting model, manifest structure, and
Lua API names follow FiveM conventions, while the runtime, security boundaries,
and transport are built from scratch in Rust.

This guide covers what works the same, what differs intentionally, and what is
not implemented yet. For exact function signatures, see
[lua-api.md](../lua-api.md) and [js-api.md](../js-api.md).

## What works the same

### Resource layout and `fxmanifest.lua`

- Resources live in folders under `sixm_resourcesPath` (default `resources/`),
  optionally grouped inside `[category]` folders. Dot-prefixed folders are
  ignored.
- Manifests use `fxmanifest.lua` (with `__resource.lua` fallback).
- `start`, `stop`, `restart` and `ensure` accept a `[category]` argument as well as a
  resource name. Manifest `server_only 'yes'` keeps a resource away from clients.
  Only the exact string `'yes'` enables the flag. Other strings leave it
  disabled. A Lua boolean such as `true` raises a manifest error. Another
  client-synced resource can explicitly include a file from it with a path such
  as `@server_only_res/data/config.json`; the file is sent as part of that
  other resource, while the `server_only` resource itself remains unsynced.
- Supported functional directives: `shared_script` / `shared_scripts`,
  `server_script` / `server_scripts`, `client_script` / `client_scripts`,
  `file` / `files`, `dependency` / `dependencies`, `provide` / `provides`,
  `export` / `exports`, and `server_export` / `server_exports`.
- Globs (`*`, `**`), cross-resource `@other/path` (including `@alias/...`)
  script/file references, `LoadResourceFile`, `SaveResourceFile`,
  `GetCurrentResourceName`, `GetResourcePath`, `GetInvokingResource`,
  `GetResourceState`, `GetNumResources`, `GetResourceByFindIndex`,
  `GetResourceMetadata`, `GetNumResourceMetadata`, `StartResource`,
  `StopResource`, and `RestartResource` follow FiveM conventions (with
  `provide` aliases resolved across dependencies, exports, files, metadata, and
  lifecycle calls).

See [manifest.md](../manifest.md) for all manifest rules.

### Events and `source`

- Local and network events use `AddEventHandler`, `RemoveEventHandler`,
  `RegisterNetEvent` (and `RegisterServerEvent` alias), `TriggerEvent`,
  `TriggerClientEvent` / `emitNet` (`-1`, a player ID, or an array of player
  IDs), `TriggerServerEvent`, `CancelEvent`, and `WasEventCanceled`.
- Latent transfers use `TriggerLatentClientEvent` (single player ID, array of
  player IDs, or `-1`) and `TriggerLatentServerEvent` with pacing in bytes per
  second.
- Standard lifecycle events work as expected:
  - Server: `onResourceStarting` (cancellable), `onResourceStart`,
    `onServerResourceStart`, `onResourceStop`, `onServerResourceStop`,
    `playerConnecting(name, setKickReason, deferrals)` (including
    `deferrals.defer`, `deferrals.update`, `deferrals.done`),
    `playerJoining(oldID)`, `playerDropped(reason, resourceName,
    clientDropReason)`, `entityCreating` (cancellable), `entityCreated`,
    `entityRemoved`, `onEntityBucketChange`, `onPlayerBucketChange` and
    `onResourceListRefresh`.
  - Difference: `oldID` always equals `source`, because SixM assigns the
    player id before `playerConnecting` and keeps it. The `clientDropReason`
    numbers (`0` client, `1` script, `2` server) are SixM's own.
  - Client: `onClientResourceStart`, `onResourceStart`, `onClientResourceStop`,
    and `onResourceStop`.

### Exports

- Register exports via `exports('name', fn)` or manifest `export` /
  `server_export` (which fall back to a global function of the same name).
- Call another running resource's exports via `exports[res].name(...)` or
  `exports[res]:name(...)`.
- Callbacks can be passed across exports, and `GetInvokingResource()` tracks
  the caller across both `TriggerEvent` and `exports` calls.

### State bags

- `GlobalState`, `Player(id).state`, `LocalPlayer.state`, and `Entity(id).state`
  work with plain assignment for replicated writes and
  `bag:set(key, value, replicated)` for choosing whether to replicate.
- Assigning `nil` deletes a key.
- `AddStateBagChangeHandler(keyFilter, bagFilter, fn)` and
  `RemoveStateBagChangeHandler(cookie)` deliver `(bagName, key, value, reserved, replicated)`
  with `reserved == 0`.
- `GetPlayerFromStateBagName` and `GetEntityFromStateBagName` resolve
  `player:<id>` and `entity:<id>` bag names.
- Clients cannot write replicated state (`bag:set(k, v, false)` keeps local
  client state only).

### Script API compatibility additions

- **Lua vectors:** `vector(...)` flattens numbers and vectors, then chooses
  `vector2`, `vector3` or `vector4` from the component count. Global `dot`,
  `cross`, and `norm` helpers complement vector methods. `inv` and `slerp`
  operate on quaternions. JavaScript already has vector methods such as
  `.dot()` and `.cross()`.
- **Legacy helpers:** `Citizen.Trace(text)` writes one text argument without
  `print`'s tab joining and drops one trailing newline. Lua
  `PerformHttpRequestAwait(url, method, data, headers, options)`
  returns the four HTTP results and yields in a thread or the script's main
  chunk. It errors in a context that cannot yield. JavaScript also accepts
  `addEventListener` and `addNetEventListener` aliases. Raw event listeners
  can use `addRawEventListener` or `addRawEventHandler` to receive the msgpack
  argument array as a `Uint8Array` and the event source.
- **State bag natives:** `GetStateBagValue`, `StateBagHasKey`, and
  `GetStateBagKeys` address `global`, `player:<id>`, and `entity:<id>` bags
  directly. `GetStateBagValue` returns `nil` in Lua and `undefined` in
  JavaScript for a missing bag or key. `StateBagHasKey` returns `false` when
  the bag or key is missing. `GetStateBagKeys` returns `nil` or `undefined` for
  an unknown bag. `SetStateBagValue` follows `bag:set` validation and also
  supports the FiveM msgpack-byte signature. Its `replicated` flag defaults to
  `false`. On clients, `replicated = false` writes local-only state and
  replicated writes fail. `EnsureEntityStateBag` is a compatibility no-op.
- **ACE and server information:** `IsAceAllowed(object)` checks the calling
  resource's `resource.<name>` principal. Use `IsPlayerAceAllowed` to check a
  player's permissions. `GetGameName()` returns `sixm` on the server and
  `headless` on the client. `GetGameBuildNumber()` returns `0`, and
  `GetInstanceId()` is the process id, stable until that process exits.
  `GetPlayerGuid` is the hex fingerprint after the player's `license:` prefix.
  `GetPlayerTimeOnline` counts whole seconds from admission after deferrals.
  `GetPlayerFromIndex` uses a zero-based index and returns a string player id.
  `GetHostId` is the lowest connected player id. `SetGameType` and `SetMapName`
  set the `gametype` and `mapname` values shown in `/info.json`. For a resource,
  `GetResourceCommands` returns its sorted, lowercase command names, or an
  empty list if the resource is unknown. See
  [Lua API: convars and commands](../lua-api.md#convars-and-commands),
  [Lua API: players](../lua-api.md#players-server-only), and [Lua API:
  resources](../lua-api.md#resources-and-files).

### Routing buckets

- Server-side routing buckets isolate entities, transforms, and entity state
  bags per bucket (bucket `0` is the default, and player peds follow their
  player):
  - `SetPlayerRoutingBucket` / `GetPlayerRoutingBucket`
  - `SetEntityRoutingBucket` / `GetEntityRoutingBucket`
  - `SetRoutingBucketPopulationEnabled` / `GetRoutingBucketPopulationEnabled`
  - `SetRoutingBucketEntityLockdownMode` / `GetRoutingBucketEntityLockdownMode`
  - `GetPlayersInRoutingBucket` (a SixM extension returning sorted player IDs in
    that bucket)

### Key-value storage (KVP)

- All per-resource KVP functions exist on both server and client:
  `GetResourceKvpString`, `GetResourceKvpInt`, `GetResourceKvpFloat`,
  `SetResourceKvp`, `SetResourceKvpInt`, `SetResourceKvpFloat`,
  `DeleteResourceKvp`, `StartFindKvp`, `FindKvp`, `EndFindKvp`, the `NoSync`
  variants, and `FlushResourceKvp`.
- On the server, KVP files are stored under `sixm_kvpPath`
  (`cache/kvp/<resource>.kvp`). On the client, they are stored locally under
  `<cache>/data/<resource>.kvp`.

### HTTP client and HTTP handlers

- **Outgoing requests (server only):**
  `PerformHttpRequest(url, callback, method, data, headers, options)` follows
  the FiveM signature (`callback(statusCode, body, headers, errorData)`),
  supports `http` and `https`, follows up to 5 redirects, and runs on worker
  threads (`sixm_httpWorkers`) so frames never block.
- **Incoming HTTP routes (server only):**
  `SetHttpHandler(function(req, res) ... end)` serves `/<resource>/...` on the
  TCP game port using `req.method`, `req.path`, `req.query`, `req.headers`,
  `req.setDataHandler`, and `res.writeHead` / `res.write` / `res.send`.
- **Built-in HTTP endpoints:** `/info.json`, `/players.json`, `/dynamic.json`,
  `/healthz`, `/metrics` (when `sv_metricsEnabled` is enabled), and `/admin`
  (self-contained web admin UI when `sv_adminUiEnabled` is enabled and
  `rcon_password` is configured) are served on the TCP port of
  `endpoint_add_udp` (unless `sv_httpEnabled` is `false`).
- **Optional discovery heartbeats:** Setting `sv_masterUrl` sends periodic JSON
  heartbeats (`info` and `dynamic`) every `sv_heartbeatInterval` seconds.

### Threads, timers, promises, JSON, and MessagePack

- `CreateThread`, `CreateThreadNow`, `Wait`, `SetTimeout`, `ClearTimeout`,
  `GetGameTimer`, and the `Citizen.*` aliases.
- `promise.new`, `promise.all`, `promise.first`, `Await` / `Citizen.Await`,
  with `:resolve`, `:reject`, and `:next`.
- `json.encode` and `json.decode`, `msgpack.pack` and `msgpack.unpack`
  (preserving `vector2`/`vector3`/`vector4`/`quat`, capped at 4 MiB and 32
  nesting levels), `GetHashKey`, `RegisterCommand`, `DoesPlayerExist`,
  `IsPrincipalAceAllowed`, `GetConvar` / `GetConvarInt` / `GetConvarFloat` /
  `GetConvarBool`, and `SetConvar` / `SetConvarReplicated` /
  `SetConvarServerInfo`.

### Client-created entities and ownership

- Clients can create networked entities with `CreateVehicle`, `CreatePed`, and
  `CreateObject` when `isNetwork` is `true` (using leased ID blocks so the
  returned entity ID works immediately).
- The server enforces `sv_entityLockdown` (`inactive`, `relaxed`, `strict`),
  per-bucket lockdown modes, `sixm_maxClientEntities` (default `64`), and the
  cancellable `entityCreating` event.
- `NetworkGetEntityOwner`, `NetworkRequestControlOfEntity`,
  `SetEntityOrphanMode(entity, 'delete' | 'migrate')`, and SixM's server-side
  `SetEntityOwner(entity, player)` control entity ownership and migration when
  an owner leaves or moves out of scope.
- Distance-based entity scoping is controlled by `sixm_cullRadius` (default
  `424` metres, `0` disables scoping).

## Key differences

### 1. Rust runtime and Lua 5.4 sandbox

Each resource runs in an isolated **Lua 5.4** state inside `sixm-lua`:

- **Removed globals:** `io`, `package`, `require`, `dofile`, and `loadfile` are
  unavailable. Use `shared_scripts` / `server_scripts` / `client_scripts` in
  `fxmanifest.lua` to load multiple Lua files into a resource, and
  `LoadResourceFile` / `SaveResourceFile` for data files.
- **Restricted `os` and `debug`:** `os` only provides `time`, `clock`, `date`,
  and `difftime`. `debug` only provides `debug.traceback` and `debug.getinfo`.
- **No bytecode loading:** `load` only accepts source text (`"t"` mode).
- **Strict manifest sandbox:** `fxmanifest.lua` runs in a smaller Lua state
  with a **16 MiB memory limit**, a **10,000,000 instruction limit**, and no
  `pcall`, `xpcall`, or `coroutine` so manifests cannot hang the server.
- **File path rules:** `SaveResourceFile` on the server only writes inside the
  calling resource's folder; on the client it only writes into
  `<cache>/data/<resource>/` and refuses to overwrite synced files. On the
  client, `GetResourcePath` only returns the calling resource's own data
  folder (`nil` for any other resource).

For comparison with the [FiveM sandbox](https://docs.fivem.net/docs/developers/sandbox/),
SixM uses resource-relative paths through its file natives, refuses symbolic
links, cross-resource writes, and `SaveResourceFile` overwrites of manifests or
script files (`.lua`/`.js`), caps `SaveResourceFile` at 4 MiB per write (plus
16 MiB total per resource on clients) and KVP at 16 MiB per resource, and
blocks `PerformHttpRequest` access or redirects to loopback, link-local, and
private addresses (with pinned DNS resolution) unless `sv_httpAllowPrivate` is
enabled. Server operators use
[`add_convar_permission`](../server.md#private-convar-readers) and
`remove_convar_permission` to control access to plain server convars, while
bootstrap/security convars remain console-only. Neither resource runtime
exposes a child-process or native-module loader API.

### 2. `ExecuteCommand` runs with `resource.<name>` permissions

In FiveM, server scripts calling `ExecuteCommand` effectively run with full
console privileges. In SixM, `ExecuteCommand` runs as the principal
`resource.<name>` of the calling resource:

- Built-in commands (`start`, `stop`, `restart`, `ensure`, `set`, `exec`, etc.)
  and restricted script commands require explicit ACE grants for
  `resource.<name>` (for example, `add_ace resource.admin command.stop allow`).
- Resources can **never** read or set convars by bare name through
  `ExecuteCommand` (use `SetConvar` / `SetConvarReplicated` in Lua, or grant
  `command.set` if needed).
- A script that runs `ExecuteCommand` on behalf of a player must check
  `IsPlayerAceAllowed(source, ...)` first, because `ExecuteCommand` executes
  with the resource's permissions, not the player's.

See [Permissions](permissions.md) for examples and details.

### 3. Shared Lua vectors (reference semantics)

In FiveM, `vector2`, `vector3`, and `vector4` are immutable value types. In
SixM, vectors are mutable 32-bit float userdata:

```lua
local a = vector3(1.0, 2.0, 3.0)
local b = a
b.x = 10.0
print(a.x) -- prints 10.0 in SixM (in FiveM, a.x would remain 1.0)
```

- Assigning a vector variable aliases the same userdata.
- Arithmetic (`+`, `-`, `*`, `/`, unary `-`) and swizzles (`a.xy`, `a.zyx`)
  always return a **new** vector, so `local b = a + vector3(0, 0, 0)` or
  `local b = vector3(a.x, a.y, a.z)` creates an independent copy.
- `type(v)` returns `"vector2"`, `"vector3"`, or `"vector4"`.

### 4. `source` is preserved across `Wait`

In FiveM, saving `local src = source` before calling `Wait(...)` inside an event
handler is mandatory because the global `source` can be overwritten while
suspended. SixM saves and restores `source` around `Wait`, so `source` remains
valid after yielding. Saving `local src = source` is still good practice if you
want scripts to stay portable back to FiveM.

### 5. Headless client, identity, and transport

- **Transport & TLS:** Game traffic uses **QUIC over UDP** (default
  `0.0.0.0:30120`) with TLS 1.3 and mandatory client certificates. There is no
  ENet-style plaintext UDP fallback, and `endpoint_add_tcp` is an alias that
  sets the same address for both UDP (QUIC) and TCP (HTTP).
- **Identifiers & tokens:** Every connected player has `license:<sha256>`
  (derived from the client's certificate in `identity.bin`) and
  `ip:<address>`, plus deterministic certificate tokens via
  `GetNumPlayerTokens` / `GetPlayerToken`. External Steam, Discord, XBL, and
  Cfx.re account tokens do not exist.
- **Client `PlayerId()`:** On `sixm-client`, `PlayerId()` always returns `0`
  (remote players are not assigned local player indices). Use
  `GetPlayerServerId(PlayerId())` to get the client's server ID, and
  `GetGamePool('CPed' | 'CVehicle' | 'CObject')` for entities in scope.
- **Client `PerformHttpRequest`:** Intentionally disabled on the client so
  scripts cannot make requests from a player's IP address; proxy external HTTP
  requests through server scripts instead.

### 6. Scripting languages (Lua 5.4 and JavaScript)

SixM supports both **Lua 5.4** (`sixm-lua`) and **ES2023 JavaScript**
(`sixm-js`, embedded QuickJS) on both server and client, including mixed `.lua`
and `.js` scripts inside the same resource with shared events, exports, and
state bags. C# (`Mono`) is not present. JS runs in a strict sandbox without Node
APIs (`require`, `fs`, `process`), `eval`, or `Function` constructors; see
[js-api.md](../js-api.md).

### 7. Remote console and profiling

SixM's remote console uses an authenticated HTTP `POST /rcon` request and
returns command output as JSON. The `profiler` command captures server frames
and resource ticks for Chrome Trace Event viewers. See
[Remote console](../server.md#remote-console) and
[Frame profiling](../server.md#frame-profiling).

## What is missing (not yet implemented)

Because SixM is a game-independent core without a game adapter yet, the
following FiveM areas are missing:

- **No game client or game adapter:** `sixm-client` is headless. A GTA VI game
  adapter is planned once a PC release exists (see
  [game-adapter.md](../design/game-adapter.md)).
- **No GTA game natives:** Only core entity management natives exist
  (`CreateVehicle`, `CreatePed`, `CreateObject`, `DeleteEntity`,
  `GetEntityCoords` / `SetEntityCoords`, `GetEntityHeading` /
  `SetEntityHeading`, `GetEntityRotation` / `SetEntityRotation`,
  `GetEntityModel`, `GetEntityType`, `DoesEntityExist`, `NetworkGetEntityOwner`,
  `GetAllVehicles` / `GetAllPeds` / `GetAllObjects`, `GetGamePool`, etc.).
  Gameplay natives (weapons, animations, tasks, vehicle physics/doors/damage,
  weather/time, world queries, blips, markers, audio) do not exist yet.
- **No UI / NUI:** There is no embedded browser, `SendNUIMessage`,
  `RegisterNUICallback`, DUI, or scaleform/HUD API.
- **Default resources:** The example server includes clean-room
  `[system]` resources `sessionmanager` (session ready handshake via
  `sixm_session:ready`, `Player(src).state.sessionActive`, `isPlayerActive`,
  `getPlayerSession`, `isSessionReady`) and `baseevents` (registers the FiveM
  channels `baseevents:onPlayerDied`, `onPlayerKilled`, `onPlayerWasted`,
  `enteringVehicle`, `enteringAborted`, `enteredVehicle` and `leftVehicle` for
  scripts and a future GameAdapter; nothing emits them yet), plus a console-only
  `chat` resource with a `say` command. `spawnmanager` and
  `mapmanager` are not bundled.
- **No public server browser UI:** `sets` / `SetConvarServerInfo` marks convars
  as server info (exposed on `/info.json`), and `sv_masterUrl` /
  `sv_heartbeatInterval` optionally send periodic JSON heartbeats to a
  configured master endpoint, but there is no built-in Cfx.re server list or
  in-client server browser yet.
- **Minor API stubs/ignored parameters:**
  - `CreateVehicleServerSetter` ignores its `type` argument.
  - `GetEntityRotation` ignores its `order` argument.
  - `SetRoutingBucketPopulationEnabled` stores the flag per bucket, but has no
    effect without a game adapter.
  - `fx_version`, `game`, and `lua54` in `fxmanifest.lua` are parsed and stored
    as metadata, but do not alter runtime behaviour.
  - Dependency strings starting with `/` in `fxmanifest.lua` are stored but
    ignored in start ordering.
