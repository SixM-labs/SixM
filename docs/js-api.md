# JavaScript API

Any `.js` script declared in `server_scripts`, `client_scripts`, or
`shared_scripts` inside `fxmanifest.lua` runs in an isolated ES2023 JavaScript
context backed by QuickJS (`crates/sixm-js`). A resource can contain Lua
scripts, JavaScript scripts, or both. When both are present in the same
resource, files execute in manifest order, and exports and events work
transparently between them and across other resources.

Each entry says where it exists:

- **both**: server and client
- **server**: server scripts only
- **client**: client scripts only

Sources: `crates/sixm-js/src/` (`runtime.rs`, `sandbox.rs`, `scheduler.rs`,
`events.rs`, `exports.rs`, `state.rs`, `vector.rs`, `msgpack.rs`, `natives.rs`,
`resources.rs`, `kvp.rs`, `players.rs`, `entities.rs`, `client.rs`, `http.rs`,
`web.rs`).

## Sandbox

Scripts run inside a constrained QuickJS runtime per resource:

- **Memory and CPU guards:** each runtime enforces a 64 MiB heap limit, a
  256 KiB call stack limit, and execution deadlines of 500 ms for script
  loading and 100 ms per callback. QuickJS also checks the deadline when
  running regular expressions.
- **No dynamic code execution or Node/OS access:** `eval` is disabled,
  `Function` constructors throw, and Node/filesystem modules (`require`,
  `import`, `fs`, `process`, `Buffer`) do not exist. Load multiple JS files via
  `fxmanifest.lua` and read/write data files with `LoadResourceFile` and
  `SaveResourceFile`.

The runtime exposes no shell or process API. The server's `PerformHttpRequest`
blocks loopback, link-local, and private network targets (and redirects to
them) unless `sv_httpAllowPrivate` is enabled, and passes response data to
scripts. `ExecuteCommand` only interprets SixM console commands with the
resource's permissions.

Resource file paths follow the same [file rules](lua-api.md#loadresourcefile)
as Lua (`SaveResourceFile` cannot overwrite manifests or scripts). Server
convar getters, setters, and listeners respect
[private convar readers](server.md#private-convar-readers) and console-only
convars.

## Scheduling, timers, and async

| Name | Side | Signature and result |
|---|---|---|
| `setTick` | both | `setTick(fn)`. Runs `fn` once every server/client tick until `clearTick` or throw. Returns a numeric handle. |
| `clearTick` | both | `clearTick(handle)`. Cancels a tick callback. |
| `setTimeout` / `SetTimeout` / `Citizen.SetTimeout` | both | `setTimeout(fn, ms)` (both `(fn, ms)` and `(ms, fn)` are accepted). Runs `fn` once after `ms`. Returns a numeric id. |
| `clearTimeout` / `ClearTimeout` / `Citizen.ClearTimeout` | both | `clearTimeout(id)`. Cancels a pending timeout. |
| `setInterval` | both | `setInterval(fn, ms)`. Repeats `fn` every `ms` (clamped at 1 ms minimum). Returns a numeric id. |
| `clearInterval` | both | `clearInterval(id)`. Stops a repeating interval. |
| `Delay` / `Wait` / `Citizen.Wait` | both | `await Delay(ms)`. Returns a `Promise` that resolves after `ms` milliseconds (`0` waits one tick). Standard `async` / `await` works inside handlers, callbacks, and top-level `setTick` / `setTimeout`. |
| `GetGameTimer` | both | `GetGameTimer()`. Milliseconds since the process first queried the timer. |
| `CreateThread` / `Citizen.CreateThread` | both | `CreateThread(fn)`. Schedules `fn` on the next tick. |

QuickJS drains Promise microtasks before leaving every script call and after each
tick/timer callback, preserving the active `source` and `GetInvokingResource()`
for synchronous continuations and across event/timer boundaries.

## Events

| Name | Side | Signature and result |
|---|---|---|
| `on` / `AddEventHandler` | both | `on(name, fn)`. Registers a local event handler and returns `{ key, name }`. |
| `RemoveEventHandler` | both | `RemoveEventHandler(handle)`. Removes the listener returned by `on` / `AddEventHandler`. |
| `onNet` | both | `onNet(name, fn)`. Marks `name` as a network event and attaches `fn`. |
| `RegisterNetEvent` / `RegisterServerEvent` | both | `RegisterNetEvent(name [, fn])`. Allows `name` to arrive over the network, optionally registering `fn`. |
| `emit` / `TriggerEvent` | both | `emit(name, ...args)`. Triggers every local handler of `name` synchronously. |
| `emitNet` / `TriggerClientEvent` | server | `emitNet(name, target, ...args)`. Sends an event to a player id (`number` or decimal string), an array of player ids (e.g. `[1, 2]`; `[]` sends to nobody, and `-1` is not allowed inside an array), or `-1` for all players. Functions cannot be sent. |
| `emitNet` / `TriggerServerEvent` | client | `emitNet(name, ...args)`. Sends an event to the server. Functions cannot be sent. |
| `TriggerLatentClientEvent` | server | `TriggerLatentClientEvent(name, target, bps, ...args)`. Sends a paced event to one player id, an array of player ids, or `-1`. |
| `TriggerLatentServerEvent` | client | `TriggerLatentServerEvent(name, bps, ...args)`. Sends a paced event to the server. |
| `CancelEvent` | both | `CancelEvent()`. Marks the active event dispatch as canceled. |
| `WasEventCanceled` | both | `WasEventCanceled()`. Returns whether the active dispatch was canceled. |

Inside an event handler, `globalThis.source` holds the sender (`""` for local
server events, player id as string/number for client-to-server events, `65535`
for server-to-client events).

## Exports

```js
// Provider (server or client)
exports("add", (a, b) => a + b);

// Consumer (Lua or JS)
const sum = exports.myresource.add(2, 3);
```

- `exports("name", fn)` registers a runtime export; manifest `export` /
  `server_export` names fall back to `globalThis[name]` if defined as a
  function.
- `exports[resource]` is a `Proxy` resolving exports synchronously (and
  probing all script runtimes in a mixed Lua/JS resource).
- Multiple return values from Lua arrive in JS as the first value (or spreadable
  through JS callback arguments); functions passed as callbacks become
  cross-runtime references released when GC collects them.

## State bags

`GlobalState`, `Player(id).state`, `LocalPlayer.state` (client), and
`Entity(id).state` expose FiveM state bags through JS `Proxy` objects:

- Property read/write: `GlobalState.mode = "race"` (replicated by default),
  `GlobalState.mode = null` / `undefined` deletes a key.
- `.set(key, value, replicated)` allows explicit local-only writes (`replicated = false`).
- `.get(key)` reads explicitly.
- `AddStateBagChangeHandler(keyFilter, bagFilter, fn)` passes
  `(bagName, key, value, reserved, replicated)` and returns an integer cookie.
- `RemoveStateBagChangeHandler(cookie)` removes a handler.
- `GetPlayerFromStateBagName(bag)` and `GetEntityFromStateBagName(bag)` map
  bag names back to server/entity ids (`0` if invalid/missing).

On the client, writes with `replicated = true` throw an error.

## Vectors

| Constructor | Result |
|---|---|
| `vector2(x, y)` / `Vector2(x, y)` | Mutable `vec2` proxy |
| `vector3(x, y, z)` / `Vector3(x, y, z)` | Mutable `vec3` proxy |
| `vector4(x, y, z, w)` / `Vector4(x, y, z, w)` | Mutable `vec4` proxy |
| `quat(w, x, y, z)` | Mutable quaternion proxy (`[w, x, y, z]`) |

Vectors are 32-bit float wrappers supporting:

- Property access & mutation: `v.x`, `v.y`, `v.z`, `v.w`, `v.r`, `v.g`, `v.b`, `v.a`
- Numeric indexing (`v[0]`) and array destructuring (`const [x, y, z] = v`)
- Single-scalar construction (`vector3(5)`) and swizzles (`v.xy`, `v.zyx`)
- Helper methods: `.length()`, `.normalize()`, `.add(other)`, `.sub(other)`,
  `.mul(scalar)`, `.div(scalar)`, `.dot(other)`, `.cross(other)` (3D)

## MessagePack (`msgpack`)

The global `msgpack` object is available on both server and client:

| Name | Side | Signature and result |
|---|---|---|
| `msgpack.pack` | both | `msgpack.pack(value)`. Serializes `value` into a `Uint8Array` (`msgpack.pack()` with no argument packs `nil`). Throws on functions, unsupported objects (such as `Map` or cyclic objects), nesting deeper than 32 levels, or payloads exceeding 4 MiB. |
| `msgpack.unpack` | both | `msgpack.unpack(data)`. Decodes a `Uint8Array`, `ArrayBuffer`, or string back into a JS value (`Uint8Array` for MessagePack `bin`, `null` for `nil`). Throws on invalid input types, empty/malformed/truncated payloads, trailing bytes, nesting deeper than 32 levels, or payloads exceeding 4 MiB. |

- `Vector2`, `Vector3`, `Vector4`, and `Quat` values round-trip with their
  32-bit float components intact via SixM extension types.
- Maximum payload size is 4 MiB (`MAX_MSGPACK_BYTES`) and maximum container
  nesting depth is 32 levels.

## General natives, convars, resources, and KVP

All core utility, command, convar, resource, and KVP functions match
[lua-api.md](lua-api.md):

- **General & Commands:** `GetHashKey`, `RegisterCommand`,
  `GetRegisteredCommands`, `ExecuteCommand`, `GetConvar`, `GetConvarInt`,
  `GetConvarFloat`, `GetConvarBool`, `SetConvar`, `SetConvarReplicated`,
  `SetConvarServerInfo`, `AddConvarChangeListener`, `RemoveConvarChangeListener`.
- **Resources:** `GetCurrentResourceName`, `GetInvokingResource` (returns `null`
  when called without a resource invoker), `GetResourceState`, `GetNumResources`,
  `GetResourceByFindIndex`, `GetResourceMetadata`, `GetNumResourceMetadata`,
  `GetResourcePath`, `LoadResourceFile` (returns `null` if missing),
  `SaveResourceFile`, `StartResource` (server, ACE-gated),
  `StopResource` (server, ACE-gated), `RestartResource` (server, ACE-gated).
- **KVP:** `GetResourceKvpString`, `GetResourceKvpInt`, `GetResourceKvpFloat`,
  `SetResourceKvp`, `SetResourceKvpInt`, `SetResourceKvpFloat`,
  `DeleteResourceKvp`, `SetResourceKvpNoSync`, `SetResourceKvpIntNoSync`,
  `SetResourceKvpFloatNoSync`, `DeleteResourceKvpNoSync`, `StartFindKvp`,
  `FindKvp` (returns `null` when exhausted), `EndFindKvp`,
  `FlushResourceKvp`.

## Players, entities, routing buckets, and HTTP

- **Players (server):** `getPlayers()` / `GetPlayers()`, `GetNumPlayerIndices()`,
  `DoesPlayerExist`, `GetPlayerName`, `GetPlayerEndpoint`, `GetPlayerPing`,
  `GetPlayerLastMsg`, `GetPlayerPed`, `GetPlayerIdentifiers`,
  `GetNumPlayerIdentifiers`, `GetPlayerIdentifier`,
  `GetPlayerIdentifierByType`, `GetNumPlayerTokens`, `GetPlayerToken`,
  `DropPlayer`, `IsPlayerAceAllowed`, `IsPrincipalAceAllowed`.
- **Client identity & pool:** `PlayerId()` (`0`), `GetPlayerServerId`,
  `PlayerPedId`, `GetGamePool`, `IsDuplicityVersion()`.
- **Entities (server & shared readers):** `DoesEntityExist`, `GetEntityCoords`,
  `GetEntityHeading`, `GetEntityRotation`, `GetEntityQuaternion`,
  `GetEntityModel`, `GetEntityType`, `GetEntityScript`,
  `NetworkGetNetworkIdFromEntity`, `NetworkGetEntityFromNetworkId`,
  `NetworkDoesNetworkIdExist`, `NetworkGetEntityOwner`,
  `IsEntityPositionFrozen`, `GetEntityVelocity`, `GetEntityHealth`,
  `GetEntityMaxHealth`, `IsEntityVisible`, `GetEntityAlpha`,
  `GetEntityAttachedTo`, `IsEntityAttached`.
- **Server-only entity control:** `CreateVehicle`, `CreateVehicleServerSetter`,
  `CreatePed`, `CreateObject`, `CreateObjectNoOffset`, `DeleteEntity`,
  `SetEntityOwner`, `NetworkGetFirstEntityOwner`, `SetEntityOrphanMode`,
  `SetEntityDistanceCullingRadius`, `FreezeEntityPosition`,
  `SetEntityVelocity`, `SetEntityHealth`, `SetEntityMaxHealth`,
  `SetEntityVisible`, `SetEntityAlpha`, `ResetEntityAlpha`,
  `AttachEntityToEntity`, `DetachEntity`.
- **Server & Client movement:** `SetEntityCoords`, `SetEntityCoordsNoOffset`,
  `SetEntityHeading`, `SetEntityRotation`, `SetEntityQuaternion`, plus
  client-side `CreateVehicle`, `CreatePed`, `CreateObject` (`isNetwork = true`),
  and `NetworkRequestControlOfEntity`.
- **Routing buckets (server):** `SetPlayerRoutingBucket`,
  `GetPlayerRoutingBucket`, `SetEntityRoutingBucket`, `GetEntityRoutingBucket`,
  `GetPlayersInRoutingBucket`, `SetRoutingBucketPopulationEnabled`,
  `GetRoutingBucketPopulationEnabled`, `SetRoutingBucketEntityLockdownMode`,
  `GetRoutingBucketEntityLockdownMode`.
- **HTTP (server):**
  - `PerformHttpRequest(url, cb, method, data, headers, options)`
  - `SetHttpHandler((req, res) => { req.setDataHandler((body) => { ... res.writeHead(...); res.write(...); res.send(...); }); })`
