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
- **Client lifecycle limits:** On clients, `GetResourceState(name)` returns
  `'missing'` for any resource that is not currently running (or transitioning)
  on that client (clients are not sent `server_only` or stopped resources), and
  `GetResourcePath(name)` returns the local `<cache>/data/<resource>` folder
  only for the calling resource (`nil` / `undefined` for any other resource).

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
    `deferrals.defer`, `deferrals.update`, `deferrals.presentCard`,
    `deferrals.done`),
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
  Client exports must be declared in `fxmanifest.lua` with `export` / `exports`
  so the server includes them in the client export list, which is capped at
  **256** exports per resource (1–256 bytes per name; exceeding the cap drops all
  client exports for that resource with a server error).
- Call another running resource's exports via `exports[res].name(...)` or
  `exports[res]:name(...)`.
- Callbacks can be passed across exports, and `GetInvokingResource()` tracks
  the caller across both `TriggerEvent` and `exports` calls.

### Stock resource: `chat`

SixM ships `[system]/chat` with the FiveM event contract (`chatMessage`,
`chat:addMessage`, `chat:addTemplate`, `chat:addSuggestion`,
`chat:addSuggestions`, `chat:removeSuggestion`, `chat:clear`, `chat:init`, and
`_chat:messageEntered`). The server derives the author from the connection and
enforces `command.say`, and registers automatic command suggestions from
`GetRegisteredCommands`. On the headless client, `say` sends
`_chat:messageEntered` and incoming messages are printed as plain text to the
client log (there is no NUI browser).

### `spawnmanager`

SixM's stock `[system]/spawnmanager` resource moves the local player ped through
the client entity transform API and emits `playerSpawned(spawnInfo)`. This
transform is simulated by the headless client. Model loading, fade effects,
and death-triggered respawning require a game adapter; use `forceRespawn` for
an explicit repeatable attempt.

### State bags

- `GlobalState`, `Player(id).state`, `LocalPlayer.state`, and `Entity(id).state`
  work with plain assignment for replicated writes and
  `bag:set(key, value, replicated)` for choosing whether to replicate.
- Assigning `nil` deletes a key.
- `AddStateBagChangeHandler(keyFilter, bagFilter, fn)` and
  `RemoveStateBagChangeHandler(cookie)` deliver `(bagName, key, value, reserved, replicated)`
  with `reserved` always `0`.
- `GetPlayerFromStateBagName` and `GetEntityFromStateBagName` resolve
  `player:<id>` and `entity:<id>` bag names.
- Clients can write replicated state on `LocalPlayer.state` and on
  `Entity(id).state` for entities owned by the local client (subject to
  server-side `sv_stateBagStrictMode`, which rejects client writes when
  enabled, and the shared `sv_playerEventRateLimit` token bucket).
  `GlobalState`, other players' bags, and unowned entities cannot be
  replicated from clients; `bag:set(k, v, false)` keeps local client state
  only.
- Because `set` is a method name on state bag handles (`bag:set(...)` in Lua,
  `bag.set(...)` in JS), a state key literally named `"set"` cannot be read
  cleanly via indexing (`bag.set` / `bag["set"]`); use
  `GetStateBagValue(bagName, "set")` instead.

### Stock resource: mapmanager

SixM ships a clean-room `mapmanager` for resources marked with scalar
`resource_type 'map'` or `resource_type 'gametype'`. `name` supplies the
server-info display name; repeated `gameTypes` values on a map restrict which
gametypes it supports. The resource exposes the familiar map/gametype exports
and publishes `SetMapName` / `SetGameType` after a successful change. Resource
start/stop calls are ACE-gated: grant `command.start`, `command.stop`, and
`command.restart` to `resource.mapmanager` in `server.cfg`.

Map resources may list JSON files with `map 'directives.json'` and
`files { 'directives.json' }`. Client resources register directive
handlers through `getMapDirectives(add)`; registration is temporary for that
map start. This headless implementation does not load game map assets or
execute Lua from JSON. Both Lua and JavaScript client resources can register
directive handlers through `getMapDirectives(add)` using SixM function
references; handlers receive parsed JSON data (or `nil` for `null`/missing
data) and the map resource name. `roundEnded()` restarts the current
gametype; maps that depend on it follow normal resource lifecycle, while an
independent map remains active. Start `spawnmanager`, then `mapmanager`, then
map/gametype resources; `chat` can start alongside them.

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
  `false`. On clients, `replicated = false` writes local-only state, while
  `replicated = true` replicates for `player:<localId>` and owned `entity:<id>`
  bags (when `sv_stateBagStrictMode` is disabled) and raises an error on other
  bags. `EnsureEntityStateBag` is a compatibility no-op.
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
  `<cache>/data/<resource>.kvp`. Keys are capped at **256 bytes**, values at
  **64 KiB**, and live data per resource namespace at **16 MiB**.

### HTTP client and HTTP handlers

- **Outgoing requests (server only):**
  `PerformHttpRequest(url, callback, method, data, headers, options)` follows
  the FiveM signature (`callback(statusCode, body, headers, errorData)`),
  supports `http` and `https`, follows up to 5 redirects, caps response bodies at
  **16 MiB**, blocks loopback, link-local, and private IP targets/redirects
  unless `sv_httpAllowPrivate` is enabled, and runs on worker threads
  (`sixm_httpWorkers`) so frames never block.
- **Incoming HTTP routes (server only):**
  `SetHttpHandler(function(req, res) ... end)` serves `/<resource>/...` on the
  TCP game port using `req.address` (proxy-resolved via `sv_proxyIPRanges`),
  `req.method`, `req.path`, `req.query`, `req.headers`, `req.setDataHandler`,
  `req.setCancelHandler`, and `res.writeHead` / `res.write` / `res.send`.
  Incoming request bodies are capped at **1 MiB** (`413` above) and outgoing
  response bodies at **16 MiB** (`500` above).
- **Built-in HTTP endpoints:** `/info.json`, `/players.json`, `/dynamic.json`,
  `/healthz`, `/metrics` (opt-in via `sv_metricsEnabled`, optionally
  protected with `sv_prometheusBasicAuthUser`/`sv_prometheusBasicAuthPassword`),
  `/admin` (opt-in self-contained web admin UI with live `resmon` when
  `sv_adminUiEnabled` is enabled and `rcon_password` is configured), and
  `/resmon.json` (opt-in live per-resource metrics, gated identically to `/admin`) are
  served on the TCP port of `endpoint_add_udp` (unless `sv_httpEnabled` is
  `false`).
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

- **Unified entity IDs:** In SixM, `entity id == network id == handle`. There
  is no separate local handle space or orphan handle-space mismatch:
  `NetworkGetNetworkIdFromEntity` and `NetworkGetEntityFromNetworkId` return
  the same integer ID for any existing entity.
- Clients can create networked entities with `CreateVehicle`, `CreatePed`, and
  `CreateObject` when `isNetwork` is `true` (using leased ID blocks so the
  returned entity ID works immediately).
- The server enforces `sv_entityLockdown` (`inactive`, `relaxed`, `strict`),
  per-bucket lockdown modes (`SetRoutingBucketEntityLockdownMode`),
  `sixm_maxClientEntities` (default `64`), and the cancellable `entityCreating`
  event. Both `sv_entityLockdown` and per-bucket lockdown apply to **client
  entity creations only**; server scripts can always create entities.
- `NetworkGetEntityOwner`, `NetworkRequestControlOfEntity`,
  `SetEntityOrphanMode(entity, 'delete' | 'migrate')` (note SixM takes the
  string `'delete'` or `'migrate'`, not FiveM's numeric enum), and SixM's
  server-side `SetEntityOwner(entity, player)` control entity ownership and
  migration when an owner leaves or moves out of scope.
- Distance-based entity scoping is controlled by `sixm_cullRadius` (default
  `424` metres, mirroring FiveM; `0` disables scoping).

## Key differences

### 1. Rust runtime and Lua 5.4 sandbox

Each resource runs in an isolated **Lua 5.4** state inside `sixm-lua`:

- **Removed globals:** `io`, `package`, `require`, `dofile`, and `loadfile` are
  unavailable. Use `shared_scripts` / `server_scripts` / `client_scripts` in
  `fxmanifest.lua` to load multiple Lua files into a resource, and
  `LoadResourceFile` / `SaveResourceFile` for data files.
- **Restricted `os` and `debug`:** `os` only provides `time`, `clock`, `date`,
  and `difftime`. `debug` only provides `debug.traceback` and `debug.getinfo`.
- **No bytecode loading or `__gc` finalizers:** `load` only accepts source
  text (`"t"` mode), and `setmetatable` rejects metatables defining `__gc`.
- **Runtime budgets:** Each Lua resource state is capped at a **64 MiB heap
  limit** and a **2,000,000-instruction resume budget** (checked every 1,000
  instructions). Scripts with long synchronous loops without `Wait(...)` are
  aborted.
- **Strict manifest sandbox:** `fxmanifest.lua` runs in a smaller Lua state
  with a **16 MiB memory limit**, a **10,000,000 instruction limit**, and no
  `pcall`, `xpcall`, or `coroutine` so manifests cannot hang the server.
- **File path rules:** `SaveResourceFile` on the server only writes inside the
  calling resource's folder; on the client it only writes into
  `<cache>/data/<resource>/` and refuses to overwrite synced files. On the
  client, `GetResourcePath` only returns the calling resource's own data
  folder (`nil` / `undefined` for any other resource).

For comparison with the [FiveM sandbox](https://docs.fivem.net/docs/developers/sandbox/),
SixM uses resource-relative paths through its file natives: `LoadResourceFile`
and `SaveResourceFile` reject symbolic links, `..` traversal, and
Windows-reserved device names (`CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`,
`LPT1`–`LPT9`, etc.) on all platforms; `SaveResourceFile` refuses manifest or
script files (`.lua`, `.js`, `fxmanifest.lua`, `__resource.lua`) and is capped
at **4 MiB** per write (plus **16 MiB** total per resource on clients); KVP is
capped at **64 KiB** per value and **16 MiB** per resource namespace; and
`PerformHttpRequest` blocks access or redirects to loopback, link-local, and
private addresses (with pinned DNS resolution) unless `sv_httpAllowPrivate` is
enabled, capping response bodies at **16 MiB**.

**Convar permissions and size limits:** Server operators use
[`add_convar_permission`](../server.md#private-convar-readers) and
`remove_convar_permission` to restrict plain server convars to named resources;
calling `GetConvar*` on a restricted convar from an unauthorized resource
returns the supplied default (with no change events or write access).
Console-only bootstrap and security convars (such as `rcon_password`,
`sv_httpEnabled`, `sv_httpAllowPrivate`, `sv_playerEventRateLimit`,
`sv_stateBagStrictMode`, and `sv_proxyIPRanges`) cannot be set by scripts
(`SetConvar*` or `ExecuteCommand('set ...')`) or remote players, and
`rcon_password` / `sv_prometheusBasicAuthPassword` are excluded from script
convar reads. `SetConvar*` caps convar names at **256 bytes** and values at
**64 KiB** (**4096 bytes** for `SetConvarReplicated` / `setr`). Neither
resource runtime exposes a child-process or native-module loader API.

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
- **Server identity without Cfx.re:** There is no Cfx.re server list or license
  key authentication (`sv_licenseKey` has no effect). Clients authenticate the
  server by pinning its TLS certificate SHA-256 fingerprint via
  `--fingerprint <64 hex chars>` instead of using a Cfx license key.
- **Identifiers & tokens:** Every connected player has `license:<sha256>`
  (the SHA-256 fingerprint of the client's self-signed TLS certificate in
  `identity.bin`, not Rockstar-verified) and `ip:<address>`, plus a
  certificate-derived token via `GetNumPlayerTokens` (`1`) / `GetPlayerToken`
  (`0:<fingerprint>`). `license2:`, `steam:`, `discord:`, `fivem:`, `xbl:`, and
  `live:` identifiers do not exist. Deleting or losing the client's
  `identity.bin` generates a new keypair and makes the client a **new player**
  with a different `license:` and token.
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
state bags. C# (`Mono`) is not present. JS runs in a strict sandbox (64 MiB heap
limit, 256 KiB call stack, 500 ms script load / 100 ms callback deadlines)
without `eval`, `Function` constructors, or Node.js/Web runtime APIs: `require`,
`fs`, `process`, `Buffer`, `setImmediate`, `npm`, `yarn`/`webpack` build
resources, `node_version` manifest handling, `TextEncoder`, `structuredClone`,
and `performance` do not exist. Libraries and bundles that depend on Node
built-ins need to be ported to the SixM runtime. For database access, use the
bundled `[system]/oxmysql` resource with your existing `dependency 'oxmysql'`,
Lua `@oxmysql/lib/MySQL.lua` import, or JavaScript `exports.oxmysql` calls.
See [JavaScript API](../js-api.md#sql-server-only) and
[Lua API](../lua-api.md#sql-server-only).

The `mysql-async` and `ghmattimysql` aliases resolve to the same resource. The
Lua import provides `MySQL.Async` callbacks and `MySQL.Sync` methods that park
the calling thread. A direct legacy export such as `executeSync`, `scalarSync`,
or `transactionSync` returns a thenable. Use `Await` in Lua or `await` in
JavaScript when calling those exports directly.

Queries use bound `?`, `@name`, or `:name` value placeholders. `prepare` and
`rawExecute` use positional `?` only. Dynamic table or column names must come
from the resource's own fixed choices. The Lua compatibility exports follow
Lua table semantics for SQL `NULL`. A named field with a `nil` value is absent
and a positional table with holes cannot represent an exact parameter list.
JavaScript code that needs explicit `null` fields or bindings can use the SQL
bridge natives with JavaScript objects and arrays directly.

Use the same `set mysql_connection_string` setting when migrating a MySQL or
MariaDB server. The [off-by-default SQLite demo](../server.md#database) runs
without an external database and uses SQLite's conflict-handling syntax.
When changing engines, use the new database's SQL dialect. Keep schema
migrations separate from transaction batches and use transactional MySQL tables.

### 7. Remote console, logging filters, and profiling

SixM's remote console uses an authenticated HTTP `POST /rcon` request (with
`rcon_password` via `Authorization: Bearer`, `X-Rcon-Password`, or JSON body)
and returns command output as JSON; FiveM's UDP RCON protocol and deprecated
`rconCommand` event do not exist. Built-in diagnostic HTTP endpoints `/admin`,
`/resmon.json` (both requiring `sv_adminUiEnabled` and `rcon_password`), and
`/metrics` (requiring `sv_metricsEnabled`) are opt-in and disabled by default,
while `/healthz` is served whenever `sv_httpEnabled` is active.
`con_channelFilters`, `con_addChannelFilter`, and `con_removeChannelFilter` use
case-insensitive `*` / `?` glob patterns matching tracing targets or
`script:<resource>` instead of regular expressions. The `profiler` command
captures server frames and resource ticks for Chrome Trace Event viewers, while
`resmon` and `sixm_hitchWarningMs` monitor live per-resource tick times, memory,
and frame hitches. See
[Remote console](../server.md#remote-console),
[Built-in commands](../server.md#built-in-commands), and
[Frame profiling](../server.md#frame-profiling).

### 8. Inbound event and state bag rate limits

- **Script & state bag token bucket (`sv_playerEventRateLimit`):** Each
  player has a shared token bucket governing both `TriggerServerEvent` calls
  and client-initiated replicated state bag writes. The default rate is **`60`
  events/sec** with a burst of **`120`** (`2×` the rate; `0` disables). Excess
  events and state writes are **dropped** (with a rate-limited server warning)
  without kicking the player.
- **Transport flood protection:** At the QUIC transport layer, inbound events
  and console lines share a token bucket of **`100`/sec** sustained (burst
  **`200`**), and a player whose traffic stays above **`1,000`/sec for 5
  consecutive seconds** is kicked for flooding (`event flood`).
- **Porting note:** Scripts with chatty per-tick `TriggerServerEvent` or state
  bag update loops should batch payloads or throttle client-to-server sends.

### 9. Convar and command migration reference

Standard server identity and listen convars (`sv_hostname`, `sv_maxclients`,
`endpoint_add_udp` / `endpoint_add_tcp`, `rcon_password`) keep their FiveM
names, while SixM-specific runtime tunables use the `sixm_*` prefix.
`endpoint_add_tcp` shares the same `ip:port` as `endpoint_add_udp` (HTTP over
TCP and QUIC over UDP share one socket address). FiveM convars `onesync`,
`gamename`, `sv_licenseKey`, `sv_scriptHookAllowed`, `sv_enforceGameBuild`, and
`sv_master1` can be set as plain convars without error, but have **no effect**.

#### FiveM convar / command -> SixM equivalent / no effect / absent

| FiveM name | SixM status | Notes |
|---|---|---|
| `sv_hostname` | Same (`sv_hostname`) | Server name (default `"SixM Server"`). |
| `sv_maxclients` | Same (`sv_maxclients`) | Max admitted players, clamped `1..=1024` (default `32`). |
| `endpoint_add_udp` | Same (`endpoint_add_udp`) | Console-only listen address for both UDP (QUIC) and TCP (HTTP), default `0.0.0.0:30120`. |
| `endpoint_add_tcp` | Alias (`endpoint_add_udp`) | Built-in command alias; TCP (HTTP) shares the UDP address. |
| `rcon_password` | Same (`rcon_password`) | Console-only credential for HTTP `POST /rcon` and `/admin` (excluded from script reads). |
| `sv_entityLockdown` | Same (`sv_entityLockdown`) | `inactive` \| `relaxed` \| `strict`; applies to client entity creations only. |
| `sv_stateBagStrictMode` | Same (`sv_stateBagStrictMode`) | Console-only; when `true`/`1`, rejects all client state bag writes. |
| `sv_playerEventRateLimit` | SixM tunable (`sv_playerEventRateLimit`) | Console-only; shared `TriggerServerEvent` + client state write limit (`events/s`, default `60`, burst `2×`, `0` disables). |
| `sv_exposePlayerIdentifiers` | SixM tunable (`sv_exposePlayerIdentifiers`) | Console-only; includes `identifiers` in `/players.json` when `true`. |
| `sv_httpEnabled` | SixM tunable (`sv_httpEnabled`) | Console-only; enables/disables HTTP listener on the game TCP port (default `true`). |
| `sv_httpAllowPrivate` | SixM tunable (`sv_httpAllowPrivate`) | Console-only; disables SSRF private-IP block for `PerformHttpRequest` and `sv_masterUrl` when `true`/`1`. |
| `sv_proxyIPRanges` | Same (`sv_proxyIPRanges`) | Console-only; trusted reverse-proxy CIDRs for `X-Forwarded-For` / `X-Real-IP`. |
| `sv_metricsEnabled` | SixM tunable (`sv_metricsEnabled`) | Console-only; opt-in `GET /metrics` Prometheus endpoint (`true`/`1`). |
| `sv_prometheusBasicAuthUser` / `sv_prometheusBasicAuthPassword` | SixM tunables | Console-only; optional HTTP Basic Auth protecting `GET /metrics` (password excluded from scripts). |
| `sv_adminUiEnabled` | SixM tunable (`sv_adminUiEnabled`) | Console-only; opt-in `GET /admin` and `GET /resmon.json` (also requires `rcon_password`). |
| `sv_masterUrl` / `sv_heartbeatInterval` | SixM tunables | Console-only; optional HTTP(S) JSON discovery heartbeat URL and interval (`10..=3600` s, default `30`). |
| `gametype` / `mapname` | Same (`gametype` / `mapname`) | Server-info convars set via `sets` or `SetGameType` / `SetMapName`, exposed in `/info.json`. |
| `sixm_resourcesPath` | SixM tunable | Console-only; resource folder scanned (default `resources`). |
| `sixm_identityFile` | SixM tunable | Console-only; server TLS cert/key file (default `cache/server-identity.bin`, empty = ephemeral). |
| `sixm_kvpPath` | SixM tunable | Console-only; server KVP folder (default `cache/kvp`). |
| `sixm_tickRate` | SixM tunable | Server tick rate (`1..=1000` Hz, default `20`); client has separate `sixm_tickRate` (default `60`). |
| `sixm_playerModel` | SixM tunable | Default model name for each player's auto-created ped (`"player"`). |
| `sixm_cullRadius` | SixM tunable | Entity distance scoping radius in metres (`0..=100000`, default `424`, `0` disables). |
| `sixm_maxClientEntities` | SixM tunable (replicated) | Max networked entities one client may create at once (`0..=4096`, default `64`). |
| `sixm_deferralTimeout` | SixM tunable | Max seconds a deferred connection waits for `done` (`1..=3600`, default `120`). |
| `sixm_httpTimeout` / `sixm_httpWorkers` | SixM tunables | `PerformHttpRequest` timeout (`1..=600` s, default `30`) and worker count (`1..=64`, default `8`). |
| `sixm_httpHandlerTimeout` | SixM tunable | Max seconds for `SetHttpHandler` responses before `504` (`1..=120`, default `25`). |
| `sixm_hitchWarningMs` | SixM tunable | Frame hitch log threshold in ms (`0..=60000`, default `50`, `0` disables). |
| `onesync` | No effect | Stored as a plain convar if set; SixM's entity/scoping model is always active. |
| `gamename` | No effect | Stored as a plain convar if set; `GetGameName()` returns `"sixm"` / `"headless"`. |
| `sv_licenseKey` | No effect | Stored as a plain convar if set; clients pin `--fingerprint` instead. |
| `sv_scriptHookAllowed` | No effect | Stored as a plain convar if set; no ScriptHook support. |
| `sv_enforceGameBuild` | No effect | Stored as a plain convar if set; `GetGameBuildNumber()` returns `0`. |
| `sv_master1` | No effect | Stored as a plain convar if set; use `sv_masterUrl` for custom discovery heartbeats. |
| `onesync_*`, `sv_devMode`, `sv_requestParanoia`, `sv_authMaxVariance`, `sv_filterRequestControl*`, `sv_kvsName`, `rateLimiter_*`, `fileserver_add`, `sv_httpFileServerProxyOnly`, `adhesive_cdnKey`, `sv_tebexSecret`, `steam_webApiKey`, `sv_endpointPrivacy`, `sv_lan`, `sv_projectName`, `sv_projectDesc`, `banner_detail` | Absent | No built-in mechanism (no game/population adapter, Cfx.re listing, Tebex/Steam integration, or external CDN file server). |
| `start`, `stop`, `restart`, `ensure` | Same (`<resource \| [category]>`) | Built-in resource commands; support `[category]` folders. |
| `refresh` | Same (`refresh`) | Built-in; rescans `sixm_resourcesPath` and fires `onResourceListRefresh`. |
| `exec` | Same (`exec <file \| @res/path>`) | Built-in; runs config files (nesting limit 16). |
| `set`, `sets`, `setr` | Same (`set`, `sets`, `setr`) | Built-in convar commands (plain, server-info, replicated). |
| `seta` | Absent | Use `set` / `sets` / `setr` in `server.cfg` (no archived convar store). |
| `add_ace`, `remove_ace`, `add_principal`, `remove_principal`, `list_aces`, `list_principals`, `test_ace` | Same | Built-in ACE commands. |
| `add_convar_permission`, `remove_convar_permission`, `list_convar_permissions` | Same / SixM (`list_convar_permissions`) | Console-only plain-convar reader ACL commands (`<resource> read <convar>`). |
| `status`, `clientkick`, `say`, `cmdlist` | Same | Built-in operator commands (`say` uses `[system]/chat` when running). |
| `con_channelFilters`, `con_addChannelFilter`, `con_removeChannelFilter` | Same (glob filters) | Built-in log filters using case-insensitive `*` / `?` globs instead of regex. |
| `profiler` | Partial (`record`, `stop`, `status`, `saveJSON`) | Built-in Chrome Trace profiler; `profiler view` web UI is absent. |
| `resmon` | Server command + `/resmon.json` + `/admin` | Built-in live per-resource CPU/heap/event stats; client overlay UI is absent. |
| `quit` | Same (`quit [reason]`) | Built-in server/client shutdown command. |
| `help`, `players`, `entities`, `state`, `perf`, `watch_resources` | SixM built-ins | Built-in server diagnostic and hot-reload (`watch_resources`, console-only) commands. |
| `load_server_icon` | Absent | No server-list icon hosting. |
| `bind`, `netgraph`, `nui_devtools`, `connect`, `disconnect` | Absent (headless client) | Client console only implements `help`, `quit`, `set`/`sets`/`setr` (plus `say` with `chat`); connect via CLI args. |

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
  `getPlayerSession`, `isSessionReady`), `baseevents` (registers the FiveM
  channels `baseevents:onPlayerDied`, `onPlayerKilled`, `onPlayerWasted`,
  `enteringVehicle`, `enteringAborted`, `enteredVehicle` and `leftVehicle` for
  scripts and a future GameAdapter; nothing emits them yet), `spawnmanager`
  (headless ped-transform spawning and `playerSpawned`), `mapmanager`
  (canonical map/gametype lifecycle and JSON directive dispatch), and `chat`
  with the `chatMessage`, `chat:addMessage`, suggestion and clear event
  contract. The headless client sends `say` through `_chat:messageEntered`, and
  the server derives the author from the connection and checks `command.say`.
  Messages are printed to the client log; templates render as plain text and
  there is no NUI.
- **No public server browser UI:** `sets` / `SetConvarServerInfo` marks convars
  as server info (exposed on `/info.json`), and `sv_masterUrl` /
  `sv_heartbeatInterval` optionally send periodic JSON heartbeats to a
  configured master endpoint, but there is no Cfx.re server list, license key
  auth, or in-client server browser (clients pin `--fingerprint` directly).
- **Minor API stubs/ignored parameters:**
  - `CreateVehicleServerSetter` ignores its `type` argument.
  - `GetEntityRotation` ignores its `order` argument.
  - `SetRoutingBucketPopulationEnabled` stores the flag per bucket, but has no
    effect without a game adapter.
  - Manifest no-op directives (`fx_version`, `game`, `lua54`,
    `convar_category`, `escrow_ignore`, `node_version`, etc.) and `/`-prefixed
    dependency constraints (`/server:`, `/onesync`, `/gameBuild`, `/native:`)
    are parsed and kept without altering runtime behaviour; see
    [manifest.md](../manifest.md#accepted-directives-with-no-host-effect).
