# Lua API

Every resource script runs in its own Lua 5.4 state. This page lists the
globals a script can use. Names follow FiveM where SixM has an equivalent.

Each entry says where it exists:

- **both**: server and client
- **server**: server scripts only
- **client**: client scripts only

Sources: `crates/sixm-lua/src/` (`natives.rs`, `client.rs`, `players.rs`,
`state.rs`, `exports.rs`, `events.rs`, `json.rs`, `msgpack.rs`, `resources.rs`,
`sandbox.rs`, `scheduler.rs`). Behaviour checks are in `crates/sixm-lua/tests/`.

## Scheduling

| Name | Side | Signature and result |
|---|---|---|
| `Wait` / `Citizen.Wait` | both | `Wait(ms)`. Pauses the current thread. Returns nothing. A value of `0`, a negative value or a non-number waits one frame. Only works inside a thread, otherwise it raises an error. |
| `CreateThread` / `Citizen.CreateThread` | both | `CreateThread(fn)`. Starts `fn` as a thread on the next frame. |
| `CreateThreadNow` / `Citizen.CreateThreadNow` | both | `CreateThreadNow(fn)`. Starts `fn` at once. It runs until its first `Wait`. |
| `SetTimeout` / `Citizen.SetTimeout` | both | `SetTimeout(ms, fn)`. Runs `fn` once after `ms`. Returns an integer id. |
| `ClearTimeout` / `Citizen.ClearTimeout` | both | `ClearTimeout(id)`. Cancels a timeout that has not run yet. |
| `GetGameTimer` | both | `GetGameTimer()`. Returns milliseconds since the process first asked for the timer, as an integer. |
| `Citizen.Trace` | both | `Citizen.Trace(text)`. Logs `text` like `print`, without tab joining. |

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
| `Await` / `Citizen.Await` | both | `Await(p)`. Accepts a promise or a table with `['then'] = function(resolve, reject) … end`. A pending value parks the thread until it settles. Returns the value or raises the rejection reason. Outside a thread it raises an error. |

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
| `TriggerClientEvent` | server | `TriggerClientEvent(name, target, ...)`. Sends an event to one player id, a sequence table of player ids (for example `{ 1, 2 }`; an empty table sends to nobody), or `-1` for every player (`-1` is not allowed inside a table). Function values cannot be sent. |
| `TriggerLatentServerEvent` | client | `TriggerLatentServerEvent(name, bytesPerSecond, ...)`. Sends an event to the server paced at the requested byte rate. Nonpositive rates select 25,000 bytes per second. Positive rates are clamped to 1 KiB/s through 100 MiB/s. Function values cannot be sent. |
| `TriggerLatentClientEvent` | server | `TriggerLatentClientEvent(name, target, bytesPerSecond, ...)`. Sends an event to one player id, a sequence table of player ids, or `-1` for every player, paced at the requested byte rate. Nonpositive rates select 25,000 bytes per second. Positive rates are clamped to 1 KiB/s through 100 MiB/s. Function values cannot be sent. |
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
| `entityCreating` | `entity`, `kind`, `model` | Fired before a client-created entity is accepted (`source` is the creator's player id). `kind` is `1` (ped), `2` (vehicle) or `3` (object). `CancelEvent()` refuses it. |
| `entityCreated` | `entity` | Right after a server- or client-created entity is stored. Its state bag already exists. |
| `entityRemoved` | `entity` | Before an entity is deleted, while it still exists. |
| `onResourceListRefresh` | none | Fired when `refresh` (or the hot-reload watcher) has replaced the resource list. |
| `playerJoining` | `oldID` | `source` is the player's id. `oldID` is the id as a string and always equals `source`, because SixM assigns the id before `playerConnecting` and keeps it (FiveM uses a temporary id there). Fired after admission and after the player's state bag exists. |
| `playerDropped` | `reason`, `resourceName`, `clientDropReason` | `source` is the player's id. The player is still listed while the handlers run. `reason` is the text shown to the player. `resourceName` is the resource that called `DropPlayer`, or `""` when none did. `clientDropReason` is `0` when the client closed or lost the connection, `1` for a `DropPlayer` call from a script, and `2` for a kick the server decides itself (for example an entity id that was not leased). These numbers are SixM's own; FiveM's values are not documented here. |
| `onPlayerBucketChange` | `player`, `bucket`, `oldBucket` | `player` is the player id as a string, like `GetPlayers`. Fired only when `SetPlayerRoutingBucket` really changes the bucket (a call with the same bucket, or for an unknown player, fires nothing), after the move is complete. |
| `onEntityBucketChange` | `entity`, `bucket`, `oldBucket` | Fired only when `SetEntityRoutingBucket` really changes the bucket, after the move is complete. It fires for every entity kind. A player's ped moves with the player, so a change of the player fires this event for the ped too, after `onPlayerBucketChange`, when the ped's own bucket was different. A player without a ped fires no entity event. |
| `onMapStart` | `resourceName` | Server local event fired by the stock `mapmanager` after a map resource starts. |
| `onGameTypeStart` | `resourceName` | Server local event fired by the stock `mapmanager` after a gametype resource starts. |

Client (`EventSource::Local`):

| Event | Arguments | Notes |
|---|---|---|
| `onClientResourceStart` | `name` | After the resource's client scripts load. |
| `onResourceStart` | `name` | Same time as `onClientResourceStart`. |
| `onClientResourceStop` | `name` | Before the resource stops. |
| `onResourceStop` | `name` | Same time as `onClientResourceStop`. |
| `onClientMapStart` | `resourceName` | Stock `mapmanager`: a map resource has started on this client; its JSON directives are processed immediately afterward. |
| `onClientMapStop` | `resourceName` | Stock `mapmanager`: a map resource is stopping on this client. |
| `onClientGameTypeStart` | `resourceName` | Stock `mapmanager`: a gametype resource has started on this client. |
| `onClientGameTypeStop` | `resourceName` | Stock `mapmanager`: a gametype resource is stopping on this client. |

The client has no `onResourceStarting`. Clients do not cancel starts.

### Events provided by the `chat` resource

These resource events are available only while the example `[system]/chat`
resource is running. The event names follow the stock chat contract; SixM's
headless client renders them as console text and has no chat UI.

| Event | Side and arguments | Behavior |
|---|---|---|
| `_chat:messageEntered` | Client to server: `text` | The headless `say` command sends only the text. The server takes the player ID from the connection's `source`, checks `command.say`, and derives the author with `GetPlayerName`; client-supplied identity fields are ignored. |
| `chat:init` | Client to server: none | Requests command suggestions for this client. The server responds with `chat:addSuggestions` built from `GetRegisteredCommands()`. |
| `chatMessage` | Server local: `playerId`, `author`, `text` | Cancelable legacy message hook. Here `source` is `""`; the first explicit argument is the sender ID (`0` for console). If any synchronous handler calls `CancelEvent()`, the chat resource does not broadcast the message. |
| `chat:addMessage` | Client: `message` object | The default broadcast is `{ args = { author, text } }`. `args` is required; optional `color`, `multiline`, `template` and `templateId` are accepted. The headless client prints scalar args as `author: text`; it strips HTML tags and substitutes `{0}`, `{1}`, … when a template is used. Color and multiline formatting are ignored, and template HTML is never executed. |
| `chat:addTemplate` | Client: `templateID`, `htmlString` | Stores a template for later plain-text rendering. Markup is stripped; no HTML runs. |
| `chat:addSuggestion` | Client: `commandName`, `commandDescription`, `commandParameters` | Stores or replaces one slash-prefixed command suggestion. |
| `chat:addSuggestions` | Client: `suggestions` array | Stores or replaces the listed suggestions. The automatic response contains `{ name = '/command', help = '', params = {} }` for each server command. Use the headless `chat_suggestions` command to list them. |
| `chat:removeSuggestion` | Client: `commandName` | Removes that suggestion. |
| `chat:clear` | Client: no arguments | Prints a marker; the append-only console log cannot erase prior output. |

Client chat events are network-safe so the server can send them with
`TriggerClientEvent`; they can also be triggered locally. Malformed display,
template or suggestion payloads are warned about and ignored.

### Deferrals

`playerConnecting` receives a third argument, `deferrals`, a table of four
functions. They let a handler hold the connection while it does slow work, such
as a lookup or an interactive Adaptive Card prompt, and tell the player what is
happening.

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
- `deferrals.presentCard(card[, cb])` sends an Adaptive Card payload to the
  connecting client while the connection is deferred. `card` may be a valid JSON
  string or a Lua table/array serialized to JSON, up to 64 KiB (`MAX_CARD_BYTES`).
  When the client responds or cancels, the optional callback `cb(data, rawData)`
  runs with the decoded JSON response table/value and the raw JSON string, or
  `(nil, nil)` if the client canceled or dismissed the card. Both sending a
  card and receiving a card response reset the `sixm_deferralTimeout` timer.
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
- `defer` is removed when the `playerConnecting` dispatch ends, and `update`,
  `presentCard`, and `done` are removed when the deferral ends, whether it
  finished, was refused, timed out or was abandoned. A later call through a
  saved copy of the table is a stale reference error. Catch it with `pcall` if
  the script may run late.
- The effects of `done`, `update`, and `presentCard` happen once the script call
  that made them has returned. Within that same call the functions still work.

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

## Stock client resources

### `spawnmanager`

The example `[system]/spawnmanager` resource provides these client exports to
Lua and JavaScript resources:

| Export | Arguments | Result |
|---|---|---|
| `addSpawnPoint` | `{ x, y, z, heading, model?, skipFade? }` | Positive integer id. Coordinates and heading must be finite. `model` is a nonempty name or 32-bit hash; `skipFade` defaults to `false`. |
| `removeSpawnPoint` | `id` | `true` when removed, otherwise `false` for an unknown id. |
| `loadSpawns` | JSON string containing `{ "spawns": [ ... ] }` | `true`; malformed input raises an error and adds no points. New ids are assigned in array order. |
| `spawnPlayer` | `[idOrSpawnObject [, callback]]` | `true` on success; `false` for an unknown id, no registered point, or unavailable ped. With no argument it chooses a registered point randomly. The callback receives the normalized `spawnInfo`. |
| `setAutoSpawn` | `enabled` boolean | No return. Enables or disables the single initial spawn attempt. |
| `setAutoSpawnCallback` | `callback` function | No return. Sets the callback used for the initial attempt and `forceRespawn`. |
| `forceRespawn` | none | No specified return. When auto-spawn is enabled, invokes the configured callback or attempts `spawnPlayer()`. |

Successful spawns emit local `playerSpawned(spawnInfo)` once before invoking
the optional callback, both on the next client tick after movement. The object
contains `x`, `y`, `z`, `heading`,
`skipFade`, and `model` (the requested model hash, or the ped's current model
hash); it also contains `idx` for a registered spawn point. A direct spawn
object has no `idx`. Enabling auto-spawn provides one initial attempt after a
ped and a point or callback are available. `forceRespawn` is the repeatable
entry point. There is no death-triggered respawn; model loading, fade, and
death integration require a game adapter.

`loadSpawns` validates JSON shape with a private parser before converting the
spawn array to Lua. It rejects object-shaped `spawns`, null entries (including
a trailing null), and non-object spawn entries before the registry is changed.

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
| `GetResourceCommands` | both | `GetResourceCommands(resource)`. Returns a sorted array of the names of the console commands `resource` registered, in lowercase, or an empty table for an unknown resource. |
| `StartResource` | server | `StartResource(name)`. Starts `name` if allowed (`command.start` ACE permission for `resource.<caller>`), returning whether it was started. |
| `StopResource` | server | `StopResource(name)`. Stops `name` if allowed (`command.stop` ACE permission for `resource.<caller>`), returning whether it was stopped. |
| `RestartResource` | server | `RestartResource(name)`. Restarts `name` (and any running dependents, in dependency order; starts `name` if stopped) if allowed (`command.restart` ACE permission for `resource.<caller>`), returning a boolean. Resolves `provide` aliases. |

`path` is relative to a resource folder. Backslashes are accepted as
separators. The host refuses parent-directory components, symbolic links,
Windows device names, and components ending in a dot or space. The file must
be a regular file.

### `LoadResourceFile`

- **Server.** Reads a file from the folder of any installed resource. It does
  not need the resource to be running.
- **Client.** Only a running resource can be read. A file the server sent is
  read from the cache. Any other file is read from that resource's data folder.

### `SaveResourceFile`

- **Server.** Writes only to the calling resource (the name must be its own),
  up to 4 MiB per call. The file goes into the resource folder, and an existing
  data file there is overwritten. Resource manifests (`fxmanifest.lua`,
  `__resource.lua`) and script files (`.lua`, `.js`) cannot be overwritten and
  return `false`. The folders are created if needed.
- **Client.** Writes only to the calling resource's data folder, up to 4 MiB per
  call and 16 MiB total per resource data folder. A file name that the server
  sent, or a manifest/script path, is refused so a saved file never replaces a
  synced or executable one.

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
  `onServerResourceStop`, `onClientResourceStart`, `onClientResourceStop`,
  `onResourceListRefresh`), player events (`playerConnecting`, `playerJoining`,
  `playerDropped`, `onPlayerBucketChange`), entity events (`entityCreated`,
  `entityRemoved`, `onEntityBucketChange`), events from the network
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

## Stock resource: `mapmanager`

`mapmanager` is a server and client resource shipped under `[system]`. Its server exports are
called as `exports.mapmanager:exportName(...)` in Lua:

| Export | Side | Signature and result |
|---|---|---|
| `getCurrentGameType` | server | `getCurrentGameType()`. Canonical resource name, or `nil`. |
| `getCurrentMap` | server | `getCurrentMap()`. Canonical resource name, or `nil`. |
| `changeGameType` | server | `changeGameType(gameType)`. Starts a known gametype; returns a boolean. Stops the previous gametype and any current map that explicitly excludes the new gametype after starting it; repeated calls for the already-active gametype are idempotent and return `true`. |
| `changeMap` | server | `changeMap(map)`. Starts a known map; returns false without side effects if the current gametype explicitly excludes it. Stops the previous map after starting the new one; repeated calls for the already-active map are idempotent and return `true`. |
| `doesMapSupportGameType` | server | `doesMapSupportGameType(gameType, map)`. Boolean; a map with no `gameTypes` metadata supports any known gametype. |
| `getMaps` | server | `getMaps()`. Map keyed by canonical resource name; values contain `name` and optional `gameTypes`. |
| `roundEnded` | server | `roundEnded()`. Restarts the current gametype and returns whether the restart succeeded. A dependent map follows normal resource lifecycle; an independent map stays running. |

`changeGameType`, `changeMap` and `roundEnded` use the synchronous resource
lifecycle natives. Grant the mapmanager resource `command.start`,
`command.stop`, and `command.restart` in `server.cfg`; denied operations return
false. Changes are rolled back and current state is reconciled if a lifecycle
step fails. `SetGameType` and `SetMapName` publish the display names to server
info after a committed change.

Client map directives are JSON data named by the map resource's `map` metadata
and listed in `files`. At each map start `mapmanager` fires
`getMapDirectives(add)` before reading those files. Both Lua and JavaScript
client resources can call `add(name, handler)` (JavaScript handlers are passed
as SixM function references and invoked after `TriggerEvent('getMapDirectives',
add)` returns); each directive calls matching handlers with
`(data, mapResourceName)` (`null` or absent `data` arrives as `nil`).
Registrations are temporary for that map start, so a provider started later
takes effect on the next map start. Invalid files and handler errors are logged
and skipped; this path never loads game assets or executes code.

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
  A value is at most 64 KiB, and each resource namespace is capped at 16 MiB
  of live KVP data. A write that breaks a limit is refused: nothing
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
| `GetConvarFloat` | both | `GetConvarFloat(name, default)`. Returns the convar parsed as a finite floating-point number, or `default` when it is not set, does not parse, or is non-finite (`NaN` / infinity). |
| `GetConvarBool` | both | `GetConvarBool(name, default)`. Returns `true` for `true`, `1`, `yes` and `on`, and `false` for `false`, `0`, `no` and `off`, in any letter case and ignoring spaces at the ends. Any other text, and an unset convar, give `default`. |
| `SetConvar` | server | `SetConvar(name, value)`. Stores a plain convar, which only the server keeps. |
| `SetConvarReplicated` | server | `SetConvarReplicated(name, value)`. Stores a replicated convar. Every connected player gets it at once, and players who join later get it too. Fails, and changes nothing, when the name is longer than 256 bytes or the value longer than 4096 bytes. |
| `SetConvarServerInfo` | server | `SetConvarServerInfo(name, value)`. Stores a convar marked as server information. Nothing sends it anywhere yet. |
| `SetGameType` | server | `SetGameType(name)`. Sets the `gametype` server-info convar, shown in `/info.json` `vars`. Errors as `SetConvar` does. |
| `SetMapName` | server | `SetMapName(name)`. Sets the `mapname` server-info convar, shown in `/info.json` `vars`. Errors as `SetConvar` does. |
| `AddConvarChangeListener` | both | `AddConvarChangeListener(filter, fn)`. Calls `fn(convarName, reserved)` when a matching convar changes (`*`/`?` globs, case-insensitive, `nil` or `""` matches all). Returns a numeric cookie. |
| `RemoveConvarChangeListener` | both | `RemoveConvarChangeListener(cookie)`. Removes the listener returned by `AddConvarChangeListener`. |
| `ExecuteCommand` | both | `ExecuteCommand(line)`. Queues a console line. It runs after the current script call returns. On a server it runs with the rights of the calling resource, as its principal `resource.<name>`. On a client it runs locally, as a typed console line. |
| `RegisterCommand` | both | `RegisterCommand(name, fn [, restricted])`. Registers a console command. `fn` is called as `fn(source, args, raw)`, where `source` is the player's id or `0` (the console), `args` is an array of strings and `raw` is the line. |
| `GetRegisteredCommands` | both | `GetRegisteredCommands()`. Returns a sorted array of `{ name = ... }` tables for built-in and script commands. |
| `GetGameName` | both | `GetGameName()`. Returns `sixm` on a server and `headless` on a client. Constant. |
| `GetGameBuildNumber` | both | `GetGameBuildNumber()`. Returns `0`. Constant. |
| `GetInstanceId` | both | `GetInstanceId()`. Returns the process id as an integer, stable for the life of the process. |

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
| `DoesPlayerExist` | `DoesPlayerExist(player)`. Returns `true` when `player` (integer or numeric string) is currently connected (admitted), and `false` otherwise. |
| `GetPlayerName` | `GetPlayerName(player)`. Returns the name, or `nil` when the id is not connected. Accepts an id as a number or a string. |
| `GetPlayerPed` | `GetPlayerPed(player)`. Returns the id of the player's ped, or `0` for a player without one. See [Entities](#entities). |
| `GetPlayerEndpoint` | `GetPlayerEndpoint(player)`. Returns the remote address (`ip:port`), or `nil`. |
| `GetPlayerIdentifiers` | `GetPlayerIdentifiers(player)`. Returns an array of the player's identifiers, in order, or an empty table for an unknown player. |
| `GetNumPlayerIdentifiers` | `GetNumPlayerIdentifiers(player)`. Returns the number of identifiers as an integer, `0` for an unknown player. |
| `GetPlayerIdentifier` | `GetPlayerIdentifier(player, index)`. Returns the identifier at the 0-based `index`, or `nil`. |
| `GetPlayerIdentifierByType` | `GetPlayerIdentifierByType(player, kind)`. Returns the first identifier whose text before `:` is `kind`, whole, for example `license:<hex>`, or `nil`. |
| `GetPlayerGuid` | `GetPlayerGuid(player)`. Returns the hex part of the player's `license:` identifier (the text after `license:`), or `nil` for an unknown player or one without a license. |
| `GetPlayerTimeOnline` | `GetPlayerTimeOnline(player)`. Returns whole seconds since the server admitted the player (after `playerConnecting` and deferrals), or `nil` for an unknown or still-connecting player. |
| `GetPlayerFromIndex` | `GetPlayerFromIndex(index)`. Returns the player at the 0-based `index` of the connected-player list (join order), as a decimal string like `GetPlayers`, or `nil` when `index` is out of range or not a non-negative whole number. |
| `GetHostId` | `GetHostId()`. Returns the lowest connected player id as a string, or `nil` when nobody is connected. |
| `GetPlayerLastMsg` | `GetPlayerLastMsg(player)`. Returns milliseconds since the last message from the player, or `-1`. |
| `GetNumPlayerTokens` | `GetNumPlayerTokens(player)`. Returns the number of client identity tokens (`1` for a connected player, `0` when unknown). |
| `GetPlayerToken` | `GetPlayerToken(player, index)`. Returns the token at 0-based `index` (`0:<fingerprint>`), or `nil`. |
| `DropPlayer` | `DropPlayer(player [, reason])`. Disconnects the player after the current call. The default reason is `Dropped by server`. |
| `IsAceAllowed` | `IsAceAllowed(object)`. Returns whether the calling resource, as the principal `resource.<name>`, may use `object`. |
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
| `GetStateBagValue` | both | `GetStateBagValue(bagName, key)`. Returns the value, or `nil` when the bag or the key is not set. Bag names are `global`, `player:<id>` and `entity:<id>`. |
| `StateBagHasKey` | both | `StateBagHasKey(bagName, key)`. Returns a boolean. A key set to `nil` is deleted, so it is `false`. |
| `GetStateBagKeys` | both | `GetStateBagKeys(bagName)`. Returns the key names sorted ascending, `{}` for a bag without keys, and `nil` for a bag that does not exist. |
| `SetStateBagValue` | both | `SetStateBagValue(bagName, key, value [, replicated])`. Same semantics as `bag:set`, except that `replicated` defaults to `false`. On a client, `replicated = true` is allowed only on the local player's bag (`player:<id>`) and entities owned by the local client. The FiveM form `SetStateBagValue(bagName, key, valueData, valueLength, replicated)` takes msgpack bytes (a string from `msgpack.pack`), decodes only the first `valueLength` bytes and stores the result. It fails with `valueLength exceeds data` when the length is longer than the data, and with the decoder error for malformed bytes. |
| `EnsureEntityStateBag` | both | `EnsureEntityStateBag(entity)`. Does nothing and never fails: an entity's bag exists for the whole life of the entity. Kept for FiveM compatibility. |
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
- On a client, a replicated write is allowed on `LocalPlayer.state` and on
  `Entity(id).state` for entities owned by the local client; replicated writes
  to `GlobalState`, other players' bags, or unowned entities raise an error.
  The server validates ownership, rate limits, and `sv_stateBagStrictMode`
  (when enabled, the server ignores client writes). A non-replicated write
  (`replicated = false`) is kept on the client only.
- Writing to the bag of a player who is not connected is an error.

## Vectors

| Name | Side | Signature and result |
|---|---|---|
| `vector2` / `vec2` | both | `vector2(x, y)`. Returns a 2D vector. |
| `vector3` / `vec3` | both | `vector3(x, y, z)`. Returns a 3D vector. |
| `vector4` / `vec4` | both | `vector4(x, y, z, w)`. Returns a 4D vector. |
| `quat` | both | `quat(w, x, y, z)`, `quat(w, vector3)` or `quat(other)`. Returns a quaternion with `.w`, `.x`, `.y`, `.z`, `*` composition/rotation and `json.encode`. |
| `vector(...)` | both | Flattens numbers and vectors like `vector3(...)`. Returns a vector2, vector3 or vector4 by the total component count. |
| `dot(a, b)` | both | Sum of the component products of two vectors of the same size. Returns a number. |
| `cross(a, b)` | both | Cross product of two `vector3`. Returns a `vector3`. |
| `norm(v)` | both | `v` scaled to length 1, as a vector or quat. A zero vector is returned unchanged. |
| `inv(q)` | both | Inverse of a quat: its conjugate divided by its squared length. Raises an error for a zero quat. |
| `slerp(a, b, t)` | both | Spherical interpolation between two quats. `t` is not clamped. Returns a quat. |

- `vector` picks vector2/3/4 from the total component count.

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
- Lua backtick hash literals such as `` `adder` `` are rewritten at load time outside comments and strings into signed 32-bit `joaat` integers matching `GetHashKey`.
- Vectors cross exports, events, state bags and the network as vectors.
- Unlike FiveM, a vector is shared, not copied: after `local b = a`, setting
  `b.x` also changes `a`. Arithmetic always returns a new vector.

## Entities

Entities can be created on the server or as networked entities on a client, and
each player also has a ped owned by its client. An entity id is also its network
id and its handle on both sides. There is no game yet, so an entity is its kind,
model, position and rotation. Clients hold the entities in their scope only,
see [Scoping](#scoping).

| Name | Side | Signature and result |
|---|---|---|
| `CreateVehicle` | both | `CreateVehicle(model, x, y, z, heading, isNetwork, netMissionEntity)`. Returns the new entity id. On the client, `isNetwork` must be `true` and the id comes from the client's leased block. |
| `CreateVehicleServerSetter` | server | `CreateVehicleServerSetter(model, type, x, y, z, heading)`. Returns the id. `type` is ignored. |
| `CreatePed` | both | `CreatePed(pedType, model, x, y, z, heading, isNetwork, bScriptHostPed)`. Returns the id. On the client, `isNetwork` must be `true`. |
| `CreateObject` / `CreateObjectNoOffset` | both | `CreateObject(model, x, y, z, isNetwork, netMissionEntity, doorFlag)`. Returns the id. On the client, `isNetwork` must be `true` (`CreateObjectNoOffset` is server only). |
| `DeleteEntity` | server | `DeleteEntity(entity)`. Does nothing for an unknown entity. |
| `SetEntityCoords` / `SetEntityCoordsNoOffset` | both | `SetEntityCoords(entity, x, y, z, ...)`. Extra arguments are ignored. On the server it moves any entity. On a client it moves only an entity the client owns. |
| `SetEntityHeading` | both | `SetEntityHeading(entity, heading)`. Keeps pitch and roll. Same rules as `SetEntityCoords`. |
| `SetEntityRotation` | both | `SetEntityRotation(entity, pitch, roll, yaw, ...)`. Same rules as `SetEntityCoords`. |
| `SetEntityQuaternion` | both | `SetEntityQuaternion(entity, x, y, z, w)`. Same rules as `SetEntityCoords`. |
| `NetworkGetEntityOwner` | both | `NetworkGetEntityOwner(entity)`. Returns the id of the player that owns the entity, or `-1` for a server-owned or unknown entity. |
| `NetworkGetFirstEntityOwner` | server | `NetworkGetFirstEntityOwner(entity)`. Returns the player id that originally created the entity, or `-1` for server-created or unknown entities. |
| `NetworkRequestControlOfEntity` | client | `NetworkRequestControlOfEntity(entity)`. Asks the server for control of an unowned entity in scope. Returns `true` if the request was sent, or `false` if unknown or already owned. |
| `SetEntityOwner` | server | `SetEntityOwner(entity, player)` (SixM extension). Gives `entity` to `player`, or `0` for the server, keeping its teleport epoch. Returns `true` if updated. |
| `SetEntityOrphanMode` | server | `SetEntityOrphanMode(entity, mode)`. Sets what happens when a client-created entity's creator leaves: `'delete'` (default) or `'migrate'`. |
| `SetEntityDistanceCullingRadius` | server | `SetEntityDistanceCullingRadius(entity, radius)`. Overrides the scoping radius in metres for `entity` (`<= 0` resets to global). |
| `GetAllVehicles` / `GetAllPeds` / `GetAllObjects` | server | Return arrays of entity ids, ascending. |
| `GetGamePool` | client | `GetGamePool(name)` with `'CVehicle'`, `'CPed'` or `'CObject'`. Returns an array of ids. Other names give an empty array. |
| `DoesEntityExist` | both | `DoesEntityExist(entity)`. Returns a boolean. |
| `GetEntityCoords` | both | Returns a `vector3`. |
| `GetEntityHeading` | both | Returns the yaw in degrees. |
| `GetEntityRotation` | both | `GetEntityRotation(entity [, order])`. Returns `vector3(pitch, roll, yaw)`. `order` is ignored. |
| `GetEntityQuaternion` | both | `GetEntityQuaternion(entity)`. Returns four floats `x, y, z, w`. |
| `GetEntityModel` | both | Returns the model hash as a signed integer, like `GetHashKey`. |
| `GetEntityType` | both | Returns `1` for a ped, `2` for a vehicle, `3` for an object and `0` for none. |
| `GetEntityScript` | both | Returns the name of the resource that created `entity`, or `nil`. |
| `NetworkGetNetworkIdFromEntity` | both | Returns the id, or `0` when the entity does not exist. |
| `NetworkGetEntityFromNetworkId` | both | Returns the id, or `0` when it does not exist. |
| `NetworkDoesNetworkIdExist` | both | Returns a boolean. |
| `GetEntityFromStateBagName` | both | `GetEntityFromStateBagName('entity:5')`. Returns the id of an existing entity, or `0`. |
| `Entity` | both | `Entity(entity)`. Returns a table with a `state` field: the entity's state bag, named `entity:<id>`. |
| `FreezeEntityPosition` | server | `FreezeEntityPosition(entity, toggle)`. Freezes or unfreezes an entity. Frozen entities refuse owner transform updates. |
| `IsEntityPositionFrozen` | both | `IsEntityPositionFrozen(entity)`. Returns a boolean (default `false`). |
| `SetEntityVelocity` | server | `SetEntityVelocity(entity, x, y, z)`. Sets the entity's velocity in m/s. Components must be finite. |
| `GetEntityVelocity` | both | `GetEntityVelocity(entity)`. Returns a `vector3` (default `vector3(0, 0, 0)`). |
| `SetEntityHealth` | server | `SetEntityHealth(entity, health)`. Sets entity health (clamped to `>= 0`). |
| `GetEntityHealth` | both | `GetEntityHealth(entity)`. Returns an integer (default `200`, `0` when unknown). |
| `SetEntityMaxHealth` | server | `SetEntityMaxHealth(entity, max)`. Sets maximum health (clamped to `>= 0`). |
| `GetEntityMaxHealth` | both | `GetEntityMaxHealth(entity)`. Returns an integer (default `200`, `0` when unknown). |
| `SetEntityVisible` | server | `SetEntityVisible(entity, toggle [, ...])`. Sets whether the entity is visible. |
| `IsEntityVisible` | both | `IsEntityVisible(entity)`. Returns a boolean (default `true`, `false` when unknown). |
| `SetEntityAlpha` | server | `SetEntityAlpha(entity, alpha [, ...])`. Sets opacity, clamped to `0..=255`. |
| `GetEntityAlpha` | both | `GetEntityAlpha(entity)`. Returns an integer in `0..=255` (default `255`, `0` when unknown). |
| `ResetEntityAlpha` | server | `ResetEntityAlpha(entity)`. Resets opacity to `255`. |
| `AttachEntityToEntity` | server | `AttachEntityToEntity(entity, target, boneIndex, x, y, z, rx, ry, rz, ...)`. Attaches `entity` to `target` with local offset and rotation. Rejects self-attachment and attachment cycles. |
| `DetachEntity` | server | `DetachEntity(entity [, ...])`. Detaches `entity` from its parent if attached. |
| `GetEntityAttachedTo` | both | `GetEntityAttachedTo(entity)`. Returns the parent entity id, or `0` when unattached or unknown. |
| `IsEntityAttached` | both | `IsEntityAttached(entity)`. Returns a boolean. |

- A model is a hash or a name. A name is hashed like `GetHashKey`, and a
  negative hash names the same model as its unsigned value.
- Reading an unknown entity gives a neutral value: `vector3(0, 0, 0)`, `0` or
  `false`. Setting one does nothing.
- Coordinates, angles, velocities, health and alpha must be finite numbers.
  NaN or infinity raises an error, on both sides and whoever owns the entity.
- The server sends creation and deletion at once. Moves and entity state changes
  are collected once per server frame: transforms travel in `EntityTransforms`,
  and state deltas (frozen, velocity, health, max health, visibility, alpha,
  attachment) travel in `EntityStates` with only changed fields encoded.
- Newly created entities default to unfrozen (`false`), zero velocity, health
  and max health `200` (all kinds), visible (`true`), alpha `255`, and detached.
- While frozen or attached, an entity refuses independent owner transform
  updates. Moving a parent entity (from the server or the parent's owner)
  recomputes attached children's world transforms and queues updates for them.
  Deleting a parent automatically detaches its children.
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

- **Client-created entities and migration.** Clients create networked
  entities from leased id blocks, so the returned handle works immediately on
  the client while the server checks `sv_entityLockdown` / the bucket lockdown
  mode, `sixm_maxClientEntities` (default 64) and `entityCreating`, refusing
  with `EntityCreateRefused` if rejected. When an entity's owner disconnects or
  leaves the entity's bucket, ownership migrates to the nearest in-scope player
  (or the server) with the current teleport epoch intact. Client-created
  entities are deleted when their creator leaves unless
  `SetEntityOrphanMode(entity, 'migrate')` was set.

`NetworkGetEntityOwner` is available on both the server and the client.

### Routing buckets

Routing buckets are separate worlds on one server. Every player and entity is in
one bucket, bucket 0 by default. A player holds only the entities of its own
bucket, in scope or not, and an entity reaches only the players of its bucket.
A player's ped moves with the player. Changing a bucket adds and removes the
affected entities for the clients in the same call. Server only:

- `SetPlayerRoutingBucket(player, bucket)`, `GetPlayerRoutingBucket(player)`.
  The ped of the player moves with it. Unknown players are ignored and read as 0.
- `SetEntityRoutingBucket(entity, bucket)`, `GetEntityRoutingBucket(entity)`.
- `GetPlayersInRoutingBucket(bucket)`: the connected players of a bucket, in
  ascending order (a SixM extension).
- `SetRoutingBucketPopulationEnabled(bucket, enabled)` and
  `GetRoutingBucketPopulationEnabled(bucket)`: stored per bucket, enabled by
  default. Nothing acts on it until a game adapter exists.
- `SetRoutingBucketEntityLockdownMode(bucket, mode)` and
  `GetRoutingBucketEntityLockdownMode(bucket)`: `inactive` (default), `relaxed`
  or `strict`. Stored per bucket for the entity creation rules.

Buckets are a bucket number from 0 to 4294967295. Bucket 0 costs nothing extra.

## SQL (server only)

The stock `oxmysql` resource provides callback exports and awaitable exports
such as `Await(exports.oxmysql:query_async(sql, params))`. Add `dependency
'oxmysql'` and `server_script '@oxmysql/lib/MySQL.lua'` to your manifest to use
`MySQL.query(sql, params, callback)`, `MySQL.query.await(sql, params)`, and the
legacy `MySQL.Async` / `MySQL.Sync` methods. `MySQL.ready(callback)` runs the
callback in a thread once the database is ready. The operations are `query`,
`single`, `scalar`, `insert`, `update`, `prepare`, `rawExecute`, and `transaction`.

The resource uses four server natives:

| Native | Signature and result |
|---|---|
| `SixmSqlIsReady` | `SixmSqlIsReady()` returns whether the configured database accepts queries. |
| `SixmSqlExecute` | `SixmSqlExecute(kind, sql, params, callback)` for `query`, `single`, `scalar`, `insert`, `update`, or `rawExecute`. |
| `SixmSqlPrepare` | `SixmSqlPrepare(sql, params, callback)` executes one set or a batch of parameter sets with one prepared statement. |
| `SixmSqlTransaction` | `SixmSqlTransaction(queries, params, callback)` takes SQL strings with shared parameters or entries `{ query = sql, values = params }`, `{ query = sql, parameters = params }`, or `{ sql, params }`. |

Pass an array for positional `?` bindings, a table with string keys for named
bindings, or `nil` for no parameters. The bridge raises submission errors and
calls `callback(error, result)` on a later server frame for accepted requests.
Success has `error == nil`. The resource exports instead pass `(result, error)`
to callbacks. A failed transaction returns `false` on a database error, and
other awaitable exports raise database errors. Resource stop discards that
resource's pending callbacks. See [the migration guide](guides/from-fivem.md).

```lua
MySQL.ready(function()
    local row = MySQL.single.await('SELECT starts FROM sixm_demo_starts WHERE name = ?', { 'example' })
    if row then print(row.starts) end
end)
```

The [database demo](server.md#database) creates and fills this table.

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

Redirects are followed up to 5. By default (`sv_httpAllowPrivate` unset or
`false`), requests and redirects to loopback, link-local (such as cloud metadata
endpoints), or private network addresses are refused with status `0` and an
error message; cross-origin redirects strip sensitive request headers
(`Authorization`, `Proxy-Authorization`, `Cookie`). A resource may have 64 requests in flight at
once; a 65th raises an error, and so does a request while 256 others wait for a
worker. A callback whose resource has stopped by the time the response arrives is
dropped, and requests of a stopped resource that have not begun never run.
Requests run on a small pool of worker threads (`sixm_httpWorkers`), so they
never block a frame.

`PerformHttpRequestAwait(url, method, data, headers, options)` takes the same
arguments without a callback and returns `status, body, headers, errorData`. It
parks the current thread until the response arrives. It works in a thread
(`CreateThread`) and in a script's main chunk (which runs as a task), like
`Await`; in a context that cannot yield, such as an export or a plain callback,
it raises an error.

The client does not have `PerformHttpRequest`. A request from a player's
machine would expose the player's address and could reach hosts the server
operator never chose. A client asks the server for the data instead.

## HTTP handlers (server only)

`SetHttpHandler(function(req, res) ... end)` registers the HTTP handler of the
calling resource. Requests to `/<resource>` and `/<resource>/...` on the server's
TCP port call it, after the built-in routes of the server. Calling it again
replaces the handler. The handler is removed when the resource stops.

- `req.address` (the client's IP string, honoring `sv_proxyIPRanges` when configured),
  `req.method`, `req.path` (the full path, such as `/res/page`), `req.query`
  (without the `?`) and `req.headers` (lower-case names). The path is the raw one sent by the client: it is not decoded or normalized. The body is not in
  `req`: `req.setDataHandler(function(data) ... end)` registers a function that
  is called with the whole body after the handler returns. `req.setCancelHandler(function() ... end)`
  registers a callback that runs if the client disconnects or the request times out before `res.send` completes. The body is at most
  1 MiB; a larger request is refused before the handler runs.
- `res.writeHead(status, headers)` sets the status and headers. Optional, and
  the default is `200`.
- `res.write(data)` adds to the body, and `res.send(data)` adds the last part and
  completes the response. `data` is optional in `send`.

Both `res.send(...)` and `res:send(...)` work. The handler may complete the
response later, from another handler or a frame, so it can keep the `res` it was
given. A response left incomplete for `sixm_httpHandlerTimeout` seconds (default 25) gets `504`. A handler that
raises an error before it completes gets `500`. A response body is limited to
16 MiB.

```lua
SetHttpHandler(function(req, res)
  res.writeHead(200, { ['Content-Type'] = 'text/plain' })
  res.send('hello ' .. req.query)
end)
```

## JSON

| Name | Side | Signature and result |
|---|---|---|
| `json.encode` | both | `json.encode(value)`. Returns a JSON string. Raises an error for functions, userdata other than vectors, NaN, infinity, invalid UTF-8 and nesting deeper than 64 levels. |
| `json.decode` | both | `json.decode(text)`. Returns a Lua value. Integers stay integers, JSON `null` becomes `nil`. Raises an error on invalid JSON or invalid UTF-8. |

- A table whose keys are `1..n` encodes as a JSON array. Other tables encode as
  objects. Number keys become strings.
- A vector encodes as an object of its components, such as
  `{"x":1.0,"y":2.0,"z":3.0}`. Decoding gives a table, not a vector.

## MessagePack (`msgpack`)

| Name | Side | Signature and result |
|---|---|---|
| `msgpack.pack` | both | `msgpack.pack(value)`. Encodes `value` into a binary Lua string. Raises an error for functions, userdata other than vectors/quaternions, cyclic tables, nesting deeper than 32 levels, or payloads exceeding 4 MiB. |
| `msgpack.unpack` | both | `msgpack.unpack(bytes)`. Decodes a MessagePack Lua string back into a Lua value. Raises an error on malformed, truncated, or trailing-byte payloads, nesting deeper than 32 levels, or payloads exceeding 4 MiB. |

- A table whose keys are `1..n` encodes as a MessagePack array; other tables
  encode as maps.
- Valid UTF-8 strings encode as MessagePack `str`; binary strings encode as
  `bin` and decode back to Lua strings.
- `vector2`, `vector3`, `vector4`, and `quat` round-trip as SixM vector and
  quaternion userdata (32-bit float components via SixM extension types).
- Maximum payload size is 4 MiB (`MAX_MSGPACK_BYTES`) and maximum container
  nesting depth is 32 levels.

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
| Script loading | Resource files and `load` accept Lua source text only. |
| `debug` | Only `debug.traceback` and `debug.getinfo`. |
| Execution | Each task resume or synchronous entry has a 2,000,000-instruction budget, checked every 1,000 instructions. Nested calls into the same runtime share its remaining budget. `pcall` and `xpcall` still handle ordinary Lua errors. |
| Memory | Each resource's Lua VM heap is limited to 64 MiB. |
| Table finalizers | `setmetatable` rejects metatables with `__gc`. |

Shell and process APIs such as `os.execute` and `io.popen` are unavailable.
`PerformHttpRequest` receives data and `SaveResourceFile` stores data.
`ExecuteCommand` only interprets SixM console commands with the resource's
permissions. Lua source passed to `load` stays inside the same Lua sandbox.

Manifests have a smaller environment. See [manifest.md](manifest.md).
