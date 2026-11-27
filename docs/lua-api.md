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
| `TriggerLatentServerEvent` | client | `TriggerLatentServerEvent(name, bytesPerSecond, ...)`. Sends an event to the server paced at the requested byte rate. Nonpositive rates select 25,000 bytes per second. Positive rates are clamped to 1 KiB/s through 100 MiB/s. Function values cannot be sent. |
| `TriggerLatentClientEvent` | server | `TriggerLatentClientEvent(name, target, bytesPerSecond, ...)`. Sends an event to one player id, or to every player with `-1`, paced at the requested byte rate. Nonpositive rates select 25,000 bytes per second. Positive rates are clamped to 1 KiB/s through 100 MiB/s. Function values cannot be sent. |
| `GetPlayerPing` | server | `GetPlayerPing(player)`. Returns round-trip latency in milliseconds, or `-1` when the player or ping is unknown. |

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
| `playerConnecting` | `name`, `setKickReason`, `deferrals` | `source` is the new player's id, which is assigned before the handlers run. Call `setKickReason(text)` or `CancelEvent()` to refuse. `setKickReason` is valid only during this event. `deferrals` holds the connection open, see [Deferrals](#deferrals). |
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

### Deferrals

`playerConnecting` receives a third argument, `deferrals`, a table of three
functions. They let a handler hold the connection while it does slow work, such
as a lookup, and tell the player what is happening.

```lua
AddEventHandler('playerConnecting', function(name, setKickReason, deferrals)
    deferrals.defer()
    deferrals.update('Checking your account')
    CreateThread(function()
        Wait(500)
        deferrals.done()
    end)
end)
```

- `deferrals.defer()` holds the connection. It is valid only while the
  `playerConnecting` handlers run. Once they return, the player is neither
  admitted nor refused until every deferring handler has called `done`.
- `deferrals.update(message)` sends a progress text to the connecting client,
  which shows it and keeps waiting. The text is cut to 4096 bytes on a
  character boundary. An update made before the connection is deferred is not
  sent. Updates are valid until the deferral ends.
- `deferrals.done([reason])` finishes the handler. Each `defer` is matched by
  one `done` without a reason. A `done` with a non-empty reason refuses the
  player at once with that reason.

The rules that follow from these:

- Several resources can defer the same player. It is admitted only when every
  deferring handler has called `done` without a reason. The first `done` with
  a reason refuses it.
- When the last deferral finishes without a reason, the player is admitted as
  it is without deferrals. The player limit (`sv_maxclients`) is checked at
  that moment, and a full server refuses the player with `Server is full`.
  An admitted player then joins as usual, with `playerJoining`.
- `done` without a reason and without a deferral does nothing. `done` with a
  reason and without a deferral refuses the player. A second `done` does
  nothing.
- `setKickReason` and `CancelEvent()` used during the dispatch refuse the
  player, even if a handler deferred it.
- A deferred player is known to `GetPlayerName`, the identifier functions and
  `IsPlayerAceAllowed`. It is not listed by `GetPlayers` and does not count
  against `sv_maxclients` until it is admitted.
- A deferral that is not finished within `sixm_deferralTimeout` seconds
  refuses the player with `connection timed out`. See [server.md](server.md#convars).
- If the client disconnects while it is deferred, the player is forgotten.
  `playerJoining` and `playerDropped` do not run for it.
- `defer` is removed when the `playerConnecting` dispatch ends, and `update` and
  `done` are removed when the deferral ends, whether it finished, was refused,
  timed out or was abandoned. A later call through a saved copy of the table is
  a stale reference error. Catch it with `pcall` if the script may run late.
- The effects of `done` and `update` happen once the script call that made
  them has returned. Within that same call the functions still work.

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
| `GetInvokingResource` | both | `GetInvokingResource()`. Returns the name of the resource whose script started the running code, or `nil`. See below. |
| `GetResourceState` | both | `GetResourceState(name)`. Returns `missing`, `stopped`, `starting`, `started` or `stopping`. See below. |
| `GetNumResources` | both | `GetNumResources()`. Returns the number of resources listed by `GetResourceByFindIndex`. |
| `GetResourceByFindIndex` | both | `GetResourceByFindIndex(index)`. Returns the name at the 0-based `index` of the resource list, or `nil`. |
| `GetResourceMetadata` | both | `GetResourceMetadata(resource, key, index)`. Returns the value at the 0-based `index` of the metadata `key` of `resource`, or `nil`. |
| `GetNumResourceMetadata` | both | `GetNumResourceMetadata(resource, key)`. Returns how many values the metadata `key` of `resource` has. |

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

### `GetInvokingResource`

Returns the name of the resource that caused the running code to run, so a
library can tell who called it. It is `nil` when no resource caused it.

- Inside an event handler started by `TriggerEvent`, it is the resource whose
  script called `TriggerEvent`.
- Inside an export called as `exports[res]:name(...)`, it is the resource whose
  script made the call. After the call returns, the caller sees its own value
  again, also when the export raised an error.
- A thread started with `CreateThread` or `SetTimeout` inside such a handler
  keeps the same value, and so does a handler after a `Wait`.
- It is `nil` for events the host fires: lifecycle events (`onResourceStarting`,
  `onResourceStart`, `onServerResourceStart`, `onResourceStop`,
  `onServerResourceStop`, `onClientResourceStart`, `onClientResourceStop`),
  player events (`playerConnecting`, `playerJoining`, `playerDropped`), entity
  events (`entityCreated`, `entityRemoved`), events from the network
  (`TriggerServerEvent` and `TriggerClientEvent`), console commands and state
  bag change handlers.

### Resource state and list

`GetResourceState` reports the state of a resource:

| State | Server | Client |
|---|---|---|
| `missing` | The resource is not installed. | The resource is not running. |
| `stopped` | Installed, and not running. | Not used. |
| `starting` | Its scripts are loading. | Its scripts are loading. |
| `started` | Running. | Running. |
| `stopping` | Its stop events are running. | Its stop events are running. |

- On the server, `GetNumResources` and `GetResourceByFindIndex` list every
  installed resource, sorted by name, whether it runs or not.
- On the client, they list the running resources in start order.
- The metadata functions read the `version 'x'` style pairs of the manifest.
  Values come in declaration order. On the server, any installed resource can
  be read. On the client, only running resources are known, and the metadata
  is what the server sent with them.

## Key-value storage

Each resource has its own key-value store, which survives restarts. A resource
reads and writes only its own keys, under the same names FiveM uses.

| Name | Side | Signature and result |
|---|---|---|
| `GetResourceKvpString` | both | `GetResourceKvpString(key)`. Returns the stored text, or `nil`. |
| `GetResourceKvpInt` | both | `GetResourceKvpInt(key)`. Returns the stored integer, or `0` when unset or not an integer. |
| `GetResourceKvpFloat` | both | `GetResourceKvpFloat(key)`. Returns the stored float, or `0.0` when unset or not a number. |
| `SetResourceKvp` | both | `SetResourceKvp(key, value)`. Stores a string. Returns a boolean, `false` when refused. |
| `SetResourceKvpInt` | both | `SetResourceKvpInt(key, value)`. Stores an integer as decimal text. Returns a boolean. |
| `SetResourceKvpFloat` | both | `SetResourceKvpFloat(key, value)`. Stores a float as text that reads back exactly. Returns a boolean. |
| `DeleteResourceKvp` | both | `DeleteResourceKvp(key)`. Returns `true` when the key was set. |
| `StartFindKvp` | both | `StartFindKvp(prefix)`. Returns a handle to the keys that start with `prefix`, in byte order. |
| `FindKvp` | both | `FindKvp(handle)`. Returns the next key, or `nil` when there are no more. |
| `EndFindKvp` | both | `EndFindKvp(handle)`. Closes the handle. |
| `FlushResourceKvp` | both | `FlushResourceKvp()`. Hands every pending change to the writer thread now. It does not wait for the disk. |

The `NoSync` variants (`SetResourceKvpNoSync`, `SetResourceKvpIntNoSync`,
`SetResourceKvpFloatNoSync` and `DeleteResourceKvpNoSync`) do the same as their
plain names, because every write is batched (see below).

- A key is 1 to 256 bytes. A key that is not valid UTF-8 raises an error.
  A value is at most 64 KiB. A write that breaks a limit is refused: nothing
  is stored and the function returns `false`.
- Integers and floats are stored as text, so `GetResourceKvpString` returns
  `"42"` for a stored integer.
- Reads and writes only touch memory. A background thread writes the changes
  to the disk, in batches of at most one per second, so a script never waits
  for the disk. `FlushResourceKvp` hands the changes over at once, and the
  server and client write everything on shutdown. A crash can lose the writes
  of the last second or two.
- Storage is append-only, and the file is rewritten when it holds much more dead
  data than live data. A damaged end of a file, such as one cut short by a crash,
  is dropped, and the records before it are kept.
- The server keeps each resource in `<sixm_kvpPath>/<resource>.kvp`. The client
  keeps it in the resource's data folder area, `<cache>/data/<resource>.kvp`,
  and it is local to that client.
- A resource name that is not plain (ASCII letters, digits, `-`, `_` and `.`,
  with no leading dot) has no store: reads find nothing and writes return `false`.

## Convars and commands

| Name | Side | Signature and result |
|---|---|---|
| `GetConvar` | both | `GetConvar(name, default)`. Returns the convar's text, or `default` when it is not set. On a client, a replicated value comes first. |
| `GetConvarInt` | both | `GetConvarInt(name, default)`. Returns the convar parsed as an integer, or `default` when it is not set or does not parse. |
| `GetConvarBool` | both | `GetConvarBool(name, default)`. Returns `true` for `true`, `1`, `yes` and `on`, and `false` for `false`, `0`, `no` and `off`, in any letter case and ignoring spaces at the ends. Any other text, and an unset convar, give `default`. |
| `SetConvar` | server | `SetConvar(name, value)`. Stores a plain convar, which only the server keeps. |
| `SetConvarReplicated` | server | `SetConvarReplicated(name, value)`. Stores a replicated convar. Every connected player gets it at once, and players who join later get it too. Fails, and changes nothing, when the name is longer than 256 bytes or the value longer than 4096 bytes. |
| `SetConvarServerInfo` | server | `SetConvarServerInfo(name, value)`. Stores a convar marked as server information. Nothing sends it anywhere yet. |
| `ExecuteCommand` | both | `ExecuteCommand(line)`. Queues a console line. It runs after the current script call returns. On a server it runs with the rights of the calling resource, as its principal `resource.<name>`. On a client it runs locally, as a typed console line. |
| `RegisterCommand` | both | `RegisterCommand(name, fn [, restricted])`. Registers a console command. `fn` is called as `fn(source, args, raw)`, where `source` is the player's id or `0` (the console), `args` is an array of strings and `raw` is the line. |

- Convar names are case-insensitive.
- The setters take effect at once, not after the current call, and follow the
  rules of the console commands `set`, `setr` and `sets`. The last store of a
  convar decides whether it is replicated. See [server.md](server.md#convars).
- A client cannot set a replicated convar. A local set of one is refused with a
  warning, and the client's `GetConvar` keeps returning the server's value. See
  [client.md](client.md#replicated-convars).
- `restricted` is shown in `help` as "(restricted)". A player may run a
  restricted command only if allowed `command.<name>`. An unrestricted command
  runs for everyone. See [server.md](server.md#permissions).
- `source` is the player's id for a command a player sent, and `0` for the
  console and for a line that `ExecuteCommand` ran.
- `ExecuteCommand` on a server runs its line with the calling resource's rights,
  not console rights. A built-in or restricted command needs
  `command.<name>` for `resource.<name>`, and a resource cannot read or set a
  convar by its name. This is a deliberate change: a resource that used
  `ExecuteCommand` for `stop`, `set` or `exec` now needs an access control
  entry for it. See [server.md](server.md#resources).
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
| `GetPlayerPed` | `GetPlayerPed(player)`. Returns the id of the player's ped, or `0` for a player without one. See [Entities](#entities). |
| `GetPlayerEndpoint` | `GetPlayerEndpoint(player)`. Returns the remote address (`ip:port`), or `nil`. |
| `GetPlayerIdentifiers` | `GetPlayerIdentifiers(player)`. Returns an array of the player's identifiers, in order, or an empty table for an unknown player. |
| `GetNumPlayerIdentifiers` | `GetNumPlayerIdentifiers(player)`. Returns the number of identifiers as an integer, `0` for an unknown player. |
| `GetPlayerIdentifier` | `GetPlayerIdentifier(player, index)`. Returns the identifier at the 0-based `index`, or `nil`. |
| `GetPlayerIdentifierByType` | `GetPlayerIdentifierByType(player, kind)`. Returns the first identifier whose text before `:` is `kind`, whole, for example `license:<hex>`, or `nil`. |
| `DropPlayer` | `DropPlayer(player [, reason])`. Disconnects the player after the current call. The default reason is `Dropped by server`. |
| `IsPlayerAceAllowed` | `IsPlayerAceAllowed(player, object)`. Returns whether the player may use `object`, as the principal `player.<id>` with the identifiers it has. `false` for an id that is not connected. See [server.md](server.md#permissions). |
| `IsPrincipalAceAllowed` | `IsPrincipalAceAllowed(principal, object)`. Returns whether the principal, such as `group.admin`, may use `object`. Names are case-insensitive. |
| `Player` | `Player(id)`. Returns a table with a `state` field: the player's state bag. Raises an error when the id is not a number or a numeric string. |

Player ids are whole numbers from 1. A new player gets the next number, and
the counter only goes up while the server runs.

A player has two identifiers, in this order:

- `license:<hex>`: the SHA-256 fingerprint of the client's certificate, as 64
  lowercase hex digits. It is the same on every connection from the same
  client identity file.
- `ip:<address>`: the address the player connects from, without the port.

The identifier functions and `GetPlayerName` work in `playerConnecting`, where
the player is not yet listed by `GetPlayers`. `GetPlayers` and
`GetNumPlayerIndices` count only admitted players.

## Players (client only)

| Name | Signature and result |
|---|---|
| `PlayerId` | `PlayerId()`. Always returns `0`. Remote players are not tracked on the client, so `0` names the local player. |
| `GetPlayerServerId` | `GetPlayerServerId(player)`. Returns the local player's server id when `player` is `0`, and `nil` otherwise. |
| `PlayerPedId` | `PlayerPedId()`. Returns the id of the local player's ped, or `0` until the server has sent it. |
| `GetPlayerPed` | `GetPlayerPed(player)`. Returns the local player's ped when `player` is `-1` or `PlayerId()`, and `0` for any other player. |
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
  The state a client gets when it joins is already in place when its scripts
  load, so it runs no handlers.
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

The server owns every entity, with one exception: each player has a ped, which
the client of that player moves. An entity id is also its network id and its
handle on both sides. There is no game yet, so an entity is its kind, model,
position and rotation. Clients hold the entities in their scope only, see
[Scoping](#scoping).

| Name | Side | Signature and result |
|---|---|---|
| `CreateVehicle` | server | `CreateVehicle(model, x, y, z, heading, isNetwork, netMissionEntity)`. Returns the new entity id. The flags are ignored. |
| `CreateVehicleServerSetter` | server | `CreateVehicleServerSetter(model, type, x, y, z, heading)`. Returns the id. `type` is ignored. |
| `CreatePed` | server | `CreatePed(pedType, model, x, y, z, heading, isNetwork, bScriptHostPed)`. Returns the id. |
| `CreateObject` / `CreateObjectNoOffset` | server | `CreateObject(model, x, y, z, isNetwork, netMissionEntity, doorFlag)`. Returns the id. |
| `DeleteEntity` | server | `DeleteEntity(entity)`. Does nothing for an unknown entity. |
| `SetEntityCoords` / `SetEntityCoordsNoOffset` | both | `SetEntityCoords(entity, x, y, z, ...)`. Extra arguments are ignored. On the server it moves any entity. On a client it moves only an entity the client owns. |
| `SetEntityHeading` | both | `SetEntityHeading(entity, heading)`. Keeps pitch and roll. Same rules as `SetEntityCoords`. |
| `SetEntityRotation` | both | `SetEntityRotation(entity, pitch, roll, yaw, ...)`. Same rules as `SetEntityCoords`. |
| `NetworkGetEntityOwner` | server | `NetworkGetEntityOwner(entity)`. Returns the id of the player that owns the entity, or `-1` for a server-owned or unknown entity. |
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
- Coordinates and angles must be finite. NaN or infinity raises an error, on
  both sides and whoever owns the entity.
- The server sends creation and deletion at once. Moves are collected, and each
  entity that moved is sent once per server frame with its latest transform.
  Only the part that changed, the position, the rotation or both, is sent.
- Other clients see positions to 1 cm and angles to about 0.0055 degrees, since
  transforms travel quantized. A script's own values stay exact on the server,
  and the owner of an entity keeps the exact values it set on its client.
- A deleted entity's state bag is removed on the server and on every client.
- The console command `entities` lists every entity.

### Scoping

Each player holds only some of the entities. This is distance-based scoping,
and it is controlled by the `sixm_cullRadius` convar (see
[Convars](server.md#convars)), in metres. The default is `424`, and the largest
value is `100000`.

- A player's scope is every entity within the radius of its ped, measured in
  three dimensions, plus every entity the player owns, always. A player without
  a ped has only its owned entities.
- An entity that enters a player's scope is created for that player with its
  current transform and owner, then every replicated key of its state bag is
  sent. An entity that leaves the scope is removed from the player's state bag
  and deleted for that player only. The player's scripts see the entity
  disappear, exactly as they would for a deletion.
- A player receives transforms only for the entities in its scope. A state bag
  of an entity, `entity:<id>`, reaches only the players that hold the entity.
  Every other state bag is still sent to every player.
- The scope is recomputed once per server frame, so a change of the convar
  takes effect on the next frame. Setting it to `0` sends every entity to every
  player, as it did before scoping. Setting it above `0` again removes the
  entities that are out of range.
- The owner of an entity is always in scope for it, so a player always sees its
  own ped.


### Player peds and ownership

When a player is admitted, the server creates its ped at the origin, owned by
that player and modelled on the `sixm_playerModel` convar (default `player`).
`GetPlayerPed` returns it on the server and, for the local player, on the
client. The ped is created before `playerJoining` and deleted after
`playerDropped`, through the normal creation and deletion path, so the other
players see it appear and disappear with `entityCreated` and `entityRemoved`.
Entities created by scripts have no owner.

Which side moves what:

- **The owner moves its entity from its client.** A client that owns an entity
  calls `SetEntityCoords` (or a related setter) as usual. The change shows at
  once in that client's own natives, and the latest transform is sent to the
  server once per client frame, in one message. The server applies it and sends
  it on to the other clients.
- **Nobody else can move it.** A client that does not own an entity does
  nothing when a script moves it. The server ignores transforms that a player
  sends for entities it does not own.
- **The server can move any entity.** A server move reaches every client,
  including the owner, which takes it even though it owns the entity. A
  teleport such as `SetEntityCoords(GetPlayerPed(source), x, y, z)` therefore
  moves the player's ped everywhere.
- **A server move wins over owner moves sent before it.** Each entity has a
  teleport epoch, which the server increases on every move it makes itself. An
  owner's update is accepted only for the current epoch, so an update that was
  computed before a server move is dropped, whenever it arrives. The server's
  move then stands everywhere, even if the owner stops moving afterwards.

`NetworkGetEntityOwner` is available on the server only.

## HTTP (server only)

`PerformHttpRequest(url, callback, method, data, headers, options)` starts an
HTTP request and returns at once. `callback(statusCode, body, headers, errorData)`
runs later, on a frame, with the outcome:

- A response of any status code, including `404` or `500`, is a result:
  `statusCode` is the status, `body` the response body, and `errorData` is `''`.
- A request that fails before a response arrives (DNS, connection, TLS, timeout,
  too many redirects, or a body over 16 MiB) calls the callback with status `0`,
  an empty body, no headers and the message as `errorData`.
- `headers` maps lower-case header names to values. Repeated headers are joined with `, `.

Arguments:

- `url`: an absolute `http` or `https` URL. Anything else raises an error.
- `method`: such as `'GET'` (the default) or `'POST'`. An invalid method raises an error.
- `data`: the request body, a string. Optional.
- `headers`: a table of header names to string or number values. Optional.
- `options.timeout`: the timeout in milliseconds for this request. Optional. The
  default is the `sixm_httpTimeout` convar. Other keys are ignored.

Redirects are followed up to 5. A resource may have 64 requests in flight at
once; a 65th raises an error, and so does a request while 256 others wait for a
worker. A callback whose resource has stopped by the time the response arrives is
dropped, and requests of a stopped resource that have not begun never run.
Requests run on a small pool of worker threads (`sixm_httpWorkers`), so they
never block a frame.

The client does not have `PerformHttpRequest`. A request from a player's
machine would expose the player's address and could reach hosts the server
operator never chose. A client asks the server for the data instead.

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
