# Lua API

Every resource script runs in its own Lua 5.4 state. This page lists the
globals a script can use. Names follow FiveM where SixM has an equivalent.

Each entry says where it exists:

- **both**: server and client
- **server**: server scripts only
- **client**: client scripts only

Sources: `crates/sixm-lua/src/` (`natives.rs`, `client.rs`, `players.rs`,
`state.rs`, `exports.rs`, `events.rs`, `json.rs`, `sandbox.rs`,
`scheduler.rs`). Behaviour checks are in `crates/sixm-lua/tests/`.

## Scheduling

| Name | Side | Signature and result |
|---|---|---|
| `Wait` / `Citizen.Wait` | both | `Wait(ms)`. Pauses the current thread. Returns nothing. A value of `0`, a negative value or a non-number waits one frame. Only works inside a thread, otherwise it raises an error. |
| `CreateThread` / `Citizen.CreateThread` | both | `CreateThread(fn)`. Starts `fn` as a thread on the next frame. |
| `CreateThreadNow` / `Citizen.CreateThreadNow` | both | `CreateThreadNow(fn)`. Starts `fn` at once. It runs until its first `Wait`. |
| `SetTimeout` / `Citizen.SetTimeout` | both | `SetTimeout(ms, fn)`. Runs `fn` once after `ms`. Returns an integer id. |
| `ClearTimeout` / `Citizen.ClearTimeout` | both | `ClearTimeout(id)`. Cancels a timeout that has not run yet. |
| `GetGameTimer` | both | `GetGameTimer()`. Returns milliseconds since the process first asked for the timer, as an integer. |

Frames run at `sixm_tickRate` (see [server.md](server.md) and
[client.md](client.md)). A `Wait(ms)` with a positive delay resumes when its
deadline has passed. It does not wait for the next frame. A wait of zero, or of
no delay, resumes on the next frame.

## Promises

A promise is a value that settles once, later. Promises belong to the resource
that made them. They cannot be passed through exports, events or state bags:
the call fails with "cannot pass a userdata value".

| Name | Side | Signature and result |
|---|---|---|
| `promise.new` | both | `promise.new()`. Returns a pending promise. |
| `promise.all` | both | `promise.all(list)`. Returns a promise that fulfils with an array of the values, in order, once every entry is fulfilled. It rejects with the first rejection. An empty list fulfils at once with an empty table. An entry that is not a promise counts as fulfilled. |
| `promise.first` | both | `promise.first(list)`. Settles like the first entry to settle. An empty list stays pending. |
| `Await` / `Citizen.Await` | both | `Await(p)`. Returns the value of `p`, or raises its reason as an error. In a thread, a pending `p` parks the thread until it settles. Outside a thread it raises an error. |

Methods of a promise:

| Method | Result |
|---|---|
| `p:resolve(value)` | Fulfils `p` with `value`. If `value` is a promise, `p` adopts its eventual state. |
| `p:reject(reason)` | Rejects `p` with `reason`. |
| `p:next(onFulfilled, onRejected)` | Returns a new promise. Either callback may be `nil`. |

- Only the first `resolve` or `reject` counts. Later calls do nothing.
- Resolving a promise with itself rejects it.
- A callback given to `next` runs as its own thread on the next frame. It never
  runs inside `resolve`. Its return value fulfils the new promise, and a
  returned promise is adopted. An error it raises rejects the new promise with
  that error. A missing callback passes the value or the reason through.
- A callback may call `Await` or `Wait`.
- A thread parked in `Await` does not run on any frame until its promise
  settles. It then resumes on the next frame with the outcome.
- A rejection that no `next` or `Await` handles by the end of the frame after
  the rejection is logged once as a warning, with the resource name and the
  reason.

A function that returns its result through a callback can be wrapped in a
promise:

```lua
local p = promise.new()
exports.other:get(function(v) p:resolve(v) end)
local v = Citizen.Await(p)
```

## Events

| Name | Side | Signature and result |
|---|---|---|
| `AddEventHandler` | both | `AddEventHandler(name, fn)`. Returns a handle table `{ key, name }`. |
| `RemoveEventHandler` | both | `RemoveEventHandler(handle)`. Takes the table that `AddEventHandler` returned. |
| `RegisterNetEvent` / `RegisterServerEvent` | both | `RegisterNetEvent(name [, fn])`. Lets the event come from the network. Without this, events from players or the server are dropped. `fn` is optional. |
| `TriggerEvent` | both | `TriggerEvent(name, ...)`. Runs every local handler of `name` on this host now. Each handler runs until its first `Wait`. The `source` global is restored afterwards. |
| `CancelEvent` | both | `CancelEvent()`. Marks the current dispatch as canceled. |
| `WasEventCanceled` | both | `WasEventCanceled()`. Returns whether the current dispatch was canceled. Handlers in other resources see the flag too. |
| `TriggerServerEvent` | client | `TriggerServerEvent(name, ...)`. Sends an event to the server. Function values cannot be sent. |
| `TriggerClientEvent` | server | `TriggerClientEvent(name, target, ...)`. Sends an event to one player id, or to every player with `-1`. Function values cannot be sent. |

### The `source` global

Inside a handler, `source` says where the event came from:

| Origin | Value of `source` |
|---|---|
| `TriggerEvent` from a script, the console or the server | `""` (empty string) |
| Sent by a connected player (`TriggerServerEvent`) | the player's server id, as an integer |
| Lifecycle events about a player (`playerConnecting`, `playerJoining`, `playerDropped`) | the player's server id |
| Sent by the server (`TriggerClientEvent`) | `65535` |

`source` is saved and restored around `Wait`, so it stays correct after a
pause.

### Network events

- An event sent by a player reaches only handlers registered with
  `RegisterNetEvent` on the server. Other events from players are dropped with
  a warning.
- An event sent by the server reaches only handlers registered with
  `RegisterNetEvent` on the client.
- Local events (`TriggerEvent`) do not need `RegisterNetEvent`.

### Lifecycle events fired by the host

Server (`EventSource::Local` unless noted):

| Event | Arguments | Notes |
|---|---|---|
| `onResourceStarting` | `name` | Fired before a resource starts. `CancelEvent()` refuses the start. |
| `onResourceStart` | `name` | After the resource's scripts have loaded. |
| `onServerResourceStart` | `name` | Same time as `onResourceStart`. |
| `onResourceStop` | `name` | Before the resource stops. |
| `onServerResourceStop` | `name` | Same time as `onResourceStop`. |
| `playerConnecting` | `name`, `setKickReason` | `source` is the new player's id, which is assigned before the handlers run. Call `setKickReason(text)` or `CancelEvent()` to refuse. `setKickReason` is valid only during this event. |
| `entityCreated` | `entity` | Right after a script created an entity. Its state bag already exists. |
| `entityRemoved` | `entity` | Before an entity is deleted, while it still exists. |
| `playerJoining` | none | `source` is the player's id. Fired after admission and after the player's state bag exists. |
| `playerDropped` | `reason` | `source` is the player's id. The player is still listed while the handlers run. |

Client (`EventSource::Local`):

| Event | Arguments | Notes |
|---|---|---|
| `onClientResourceStart` | `name` | After the resource's client scripts load. |
| `onResourceStart` | `name` | Same time as `onClientResourceStart`. |
| `onClientResourceStop` | `name` | Before the resource stops. |
| `onResourceStop` | `name` | Same time as `onClientResourceStop`. |

The client has no `onResourceStarting`. Clients do not cancel starts.

## Exports

| Name | Side | Signature and result |
|---|---|---|
| `exports` | both | `exports(name, fn)` registers an export. `exports[resource]` returns a proxy for another resource's exports. |
| `exports[res].name(...)` | both | Calls the export `name` of resource `res`. The colon form `exports[res]:name(...)` also works. Returns the export's results. The call is synchronous. |

- Calling an export of a resource that is not running, or one that does not
  exist, raises an error.
- A function can be passed to an export. The callback is kept until the
  receiving side releases it.
- Threads cannot cross resources.
- A name declared with `export` or `server_export` in the manifest falls back
  to a global function of the same name, if no `exports(...)` registration
  exists.
- Client exports are sent to clients as declared in the manifest. A client can
  call the exports of another client resource in the same way.

## Resources and files

| Name | Side | Signature and result |
|---|---|---|
| `GetCurrentResourceName` | both | `GetCurrentResourceName()`. Returns the resource's own name. |
| `GetResourcePath` | both | `GetResourcePath(name)`. Server: the folder of any installed resource, running or not. Client: only the resource's own data folder. For any other name it returns `nil`. Returns a string or `nil`. |
| `LoadResourceFile` | both | `LoadResourceFile(resource, path)`. Returns the file's contents as a string, or `nil`. See below. |
| `SaveResourceFile` | both | `SaveResourceFile(resource, path, data)`. Writes `data` and returns a boolean. See below. |

`path` is normalised first. It must be relative, must not contain `..`, and
must not contain a colon. Backslashes are accepted as separators.

### `LoadResourceFile`

- **Server.** Reads a file from the folder of any installed resource. It does
  not need the resource to be running.
- **Client.** Only a running resource can be read. A file the server sent is
  read from the cache. Any other file is read from that resource's data folder.

### `SaveResourceFile`

- **Server.** Writes only to the calling resource (the name must be its own).
  The file goes into the resource folder, and an existing file there is
  overwritten. The folders are created if needed.
- **Client.** Writes only to the calling resource's data folder. A file name
  that the server sent is refused, so a saved file never replaces a synced one.

The client keeps its data folders under the cache. See [client.md](client.md).

## Convars and commands

| Name | Side | Signature and result |
|---|---|---|
| `GetConvar` | both | `GetConvar(name, default)`. Returns the convar's text, or `default` when it is not set. |
| `GetConvarInt` | both | `GetConvarInt(name, default)`. Returns the convar parsed as an integer, or `default` when it is not set or does not parse. |
| `ExecuteCommand` | both | `ExecuteCommand(line)`. Queues a console line. It runs after the current script call returns. |
| `RegisterCommand` | both | `RegisterCommand(name, fn [, restricted])`. Registers a console command. `fn` is called as `fn(source, args, raw)`, where `source` is `0` (the console), `args` is an array of strings and `raw` is the line. |

- Convar names are case-insensitive.
- `restricted` is shown in `help` as "(restricted)". The host does not enforce
  it. Only the console can run commands, so no other source exists.
- A command registered by a resource is removed when that resource stops.
- Built-in console commands are matched before script commands. A script
  command with the same name as a built-in can never run. See
  [server.md](server.md).

## Players (server only)

| Name | Signature and result |
|---|---|
| `GetPlayers` | `GetPlayers()`. Returns an array of player ids as strings, in join order. |
| `GetNumPlayerIndices` | `GetNumPlayerIndices()`. Returns the number of connected players as an integer. |
| `GetPlayerName` | `GetPlayerName(player)`. Returns the name, or `nil` when the id is not connected. Accepts an id as a number or a string. |
| `GetPlayerEndpoint` | `GetPlayerEndpoint(player)`. Returns the remote address (`ip:port`), or `nil`. |
| `DropPlayer` | `DropPlayer(player [, reason])`. Disconnects the player after the current call. The default reason is `Dropped by server`. |
| `Player` | `Player(id)`. Returns a table with a `state` field: the player's state bag. Raises an error when the id is not a number or a numeric string. |

Player ids are whole numbers from 1. A new player gets the next number, and
the counter only goes up while the server runs.

## Players (client only)

| Name | Signature and result |
|---|---|
| `PlayerId` | `PlayerId()`. Always returns `0`. Remote players are not tracked on the client, so `0` names the local player. |
| `GetPlayerServerId` | `GetPlayerServerId(player)`. Returns the local player's server id when `player` is `0`, and `nil` otherwise. |
| `LocalPlayer` | A table. `LocalPlayer.state` is the local player's state bag. |

`PlayerId()` and `GetPlayerServerId(PlayerId())` are not the same value on a
client: the first is `0`, the second is the server id.

## State bags

| Name | Side | Signature and result |
|---|---|---|
| `GlobalState` | both | The global bag. Read and write its keys as fields: `GlobalState.key`. |
| `bag:set` | both | `bag:set(key, value [, replicated])`. Writes a key. `replicated` defaults to `true`. |
| `AddStateBagChangeHandler` | both | `AddStateBagChangeHandler(key, bag, fn)`. Either filter may be `nil`, which matches every key or bag. Returns a cookie. |
| `RemoveStateBagChangeHandler` | both | `RemoveStateBagChangeHandler(cookie)`. Removes a handler. |
| `GetPlayerFromStateBagName` | both | `GetPlayerFromStateBagName(bagName)`. Returns the player id of a `player:<id>` bag, or `0` for any other name. |
| `Player(id).state` | server | A player's state bag. See above. |
| `LocalPlayer.state` | client | The local player's state bag. |

Behaviour:

- A plain assignment (`GlobalState.key = value`) replicates. `bag:set(...)`
  replicates unless the third argument is `false`.
- Assigning `nil` deletes the key.
- Replicated writes are sent to every connected client. A new client receives
  every replicated key of every bag when it joins.
- Non-replicated writes stay on the server. A client keeps the last replicated
  value it was sent.
- Keys are 1 to 256 bytes. Values are plain data. Functions cannot be stored.
- A change handler receives `(bagName, key, value, reserved, replicated)`.
  `reserved` is always `0`. Handlers run on the server for every write. On a
  client they run for replicated values it receives and for its own writes.
- `set` is a method name, so a key called `set` can be written but reads back
  the method. Choose another key name.
- On a client, a replicated write raises the error "clients cannot write
  replicated state". A non-replicated write is kept on the client only.
- Writing to the bag of a player who is not connected is an error.

## Vectors

| Name | Side | Signature and result |
|---|---|---|
| `vector2` / `vec2` | both | `vector2(x, y)`. Returns a 2D vector. |
| `vector3` / `vec3` | both | `vector3(x, y, z)`. Returns a 3D vector. |
| `vector4` / `vec4` | both | `vector4(x, y, z, w)`. Returns a 4D vector. |

- Arguments can be numbers or vectors, flattened in order:
  `vector3(vector2(1, 2), 3)`. A single number fills every component:
  `vector3(0)`. Any other count raises an error.
- Components are 32-bit floats, as in the games.
- Read components as `v.x`, `v.y`, `v.z`, `v.w` or `v[1]` to `v[n]`. A
  component the vector does not have reads as `nil`.
- Swizzles of 2 to 4 letters return a new vector: `v.xy`, `v.zyx`, `v.xxyy`.
- Assigning a component changes the vector in place: `v.z = 10`.
- `+` and `-` work between vectors of the same size, or with a number on either
  side. `*` and `/` also work component-wise between vectors of the same size.
  Unary minus negates. Mixing sizes raises an error.
- `#v` is the length, so `#(a - b)` is the distance between two points.
- `==` compares size and components. `tostring(v)` gives `vector3(1.0, 2.0, 3.0)`.
- `type(v)` returns `"vector2"`, `"vector3"` or `"vector4"`.
- Vectors cross exports, events, state bags and the network as vectors.
- Unlike FiveM, a vector is shared, not copied: after `local b = a`, setting
  `b.x` also changes `a`. Arithmetic always returns a new vector.

## Entities

The server owns every entity. Clients keep a copy and cannot change it. An
entity id is also its network id and its handle on both sides. There is no
game yet, so an entity is its kind, model, position and rotation. Every client
receives every entity: there is no distance scoping yet.

| Name | Side | Signature and result |
|---|---|---|
| `CreateVehicle` | server | `CreateVehicle(model, x, y, z, heading, isNetwork, netMissionEntity)`. Returns the new entity id. The flags are ignored. |
| `CreateVehicleServerSetter` | server | `CreateVehicleServerSetter(model, type, x, y, z, heading)`. Returns the id. `type` is ignored. |
| `CreatePed` | server | `CreatePed(pedType, model, x, y, z, heading, isNetwork, bScriptHostPed)`. Returns the id. |
| `CreateObject` / `CreateObjectNoOffset` | server | `CreateObject(model, x, y, z, isNetwork, netMissionEntity, doorFlag)`. Returns the id. |
| `DeleteEntity` | server | `DeleteEntity(entity)`. Does nothing for an unknown entity. |
| `SetEntityCoords` / `SetEntityCoordsNoOffset` | server | `SetEntityCoords(entity, x, y, z, ...)`. Extra arguments are ignored. |
| `SetEntityHeading` | server | `SetEntityHeading(entity, heading)`. Keeps pitch and roll. |
| `SetEntityRotation` | server | `SetEntityRotation(entity, pitch, roll, yaw, ...)`. |
| `GetAllVehicles` / `GetAllPeds` / `GetAllObjects` | server | Return arrays of entity ids, ascending. |
| `GetGamePool` | client | `GetGamePool(name)` with `'CVehicle'`, `'CPed'` or `'CObject'`. Returns an array of ids. Other names give an empty array. |
| `DoesEntityExist` | both | `DoesEntityExist(entity)`. Returns a boolean. |
| `GetEntityCoords` | both | Returns a `vector3`. |
| `GetEntityHeading` | both | Returns the yaw in degrees. |
| `GetEntityRotation` | both | `GetEntityRotation(entity [, order])`. Returns `vector3(pitch, roll, yaw)`. `order` is ignored. |
| `GetEntityModel` | both | Returns the model hash as a signed integer, like `GetHashKey`. |
| `GetEntityType` | both | Returns `1` for a ped, `2` for a vehicle, `3` for an object and `0` for none. |
| `NetworkGetNetworkIdFromEntity` | both | Returns the id, or `0` when the entity does not exist. |
| `NetworkGetEntityFromNetworkId` | both | Returns the id, or `0` when it does not exist. |
| `NetworkDoesNetworkIdExist` | both | Returns a boolean. |
| `GetEntityFromStateBagName` | both | `GetEntityFromStateBagName('entity:5')`. Returns the id of an existing entity, or `0`. |
| `Entity` | both | `Entity(entity)`. Returns a table with a `state` field: the entity's state bag, named `entity:<id>`. |

- A model is a hash or a name. A name is hashed like `GetHashKey`, and a
  negative hash names the same model as its unsigned value.
- Reading an unknown entity gives a neutral value: `vector3(0, 0, 0)`, `0` or
  `false`. Setting one does nothing.
- Coordinates and angles must be finite. NaN or infinity raises an error.
- The server sends creation and deletion at once. Moves are collected, and each
  entity that moved is sent once per server frame with its latest transform.
- A deleted entity's state bag is removed on the server and on every client.
- The console command `entities` lists every entity.

## JSON

| Name | Side | Signature and result |
|---|---|---|
| `json.encode` | both | `json.encode(value)`. Returns a JSON string. Raises an error for functions, userdata other than vectors, NaN, infinity, invalid UTF-8 and nesting deeper than 64 levels. |
| `json.decode` | both | `json.decode(text)`. Returns a Lua value. Integers stay integers, JSON `null` becomes `nil`. Raises an error on invalid JSON or invalid UTF-8. |

- A table whose keys are `1..n` encodes as a JSON array. Other tables encode as
  objects. Number keys become strings.
- A vector encodes as an object of its components, such as
  `{"x":1.0,"y":2.0,"z":3.0}`. Decoding gives a table, not a vector.

## Other globals

| Name | Side | Notes |
|---|---|---|
| `GetHashKey` | both | `GetHashKey(name)`. Returns the hash of `name` as a signed 32-bit integer, case-insensitive. `GetHashKey('adder')` is `-1216765807`. |
| `IsDuplicityVersion` | both | `IsDuplicityVersion()`. Returns `true` on the server and `false` on the client. |
| `print` | both | Writes a line to the log, prefixed with the resource name. Arguments are joined with tabs. |
| `source` | both | Global. See the events section. |
| `Citizen` | both | A table with `Wait`, `CreateThread`, `CreateThreadNow`, `SetTimeout`, `ClearTimeout` and `Await`. The `promise` table is a global, see [Promises](#promises). |

## Sandbox differences

The sandbox removes parts of standard Lua:

| Area | Behaviour |
|---|---|
| Standard libraries | `coroutine`, `table`, `string`, `utf8`, `math`, and the `os` functions `time`, `clock`, `date` and `difftime`. |
| Not available | `io`, `package`, `require`, `dofile`, `loadfile`. |
| `os` | Only `time`, `clock`, `date` and `difftime`. Other `os` functions are removed. |
| `load` | Accepts source text only. The mode is forced to text, so precompiled bytecode is rejected. |
| `debug` | Only `debug.traceback` and `debug.getinfo`. |
| `pcall`, `xpcall` | Available to scripts. |

Manifests have a smaller environment. See [manifest.md](manifest.md).
