---@meta
--- Official SixM Lua Language Server (LuaLS) type definitions.
--- Matches SixM's Lua 5.4 scripting runtime, manifest directives, and natives.

-- =============================================================================
-- Manifest directives (fxmanifest.lua / __resource.lua)
-- =============================================================================

---@alias FxManifestValue string | number | (string | number)[]
---@alias FxManifestChain fun(extra: FxManifestValue): nil

--- Declares the manifest version (stored as metadata in SixM).
---@param version FxManifestValue
---@return FxManifestChain
function fx_version(version) end

--- Declares the target game (e.g. `'common'`).
---@param gameName FxManifestValue
---@return FxManifestChain
function game(gameName) end

--- Declares one or more target games.
---@param gameNames FxManifestValue
---@return FxManifestChain
function games(gameNames) end

--- Declares a server-side script (`.lua` or `.js`, globs allowed).
---@param script FxManifestValue
---@return FxManifestChain
function server_script(script) end

--- Declares server-side scripts (`.lua` or `.js`, globs allowed).
---@param scripts FxManifestValue
---@return FxManifestChain
function server_scripts(scripts) end

--- Declares a client-side script (`.lua` or `.js`, globs allowed).
---@param script FxManifestValue
---@return FxManifestChain
function client_script(script) end

--- Declares client-side scripts (`.lua` or `.js`, globs allowed).
---@param scripts FxManifestValue
---@return FxManifestChain
function client_scripts(scripts) end

--- Declares a script loaded on both server and client (`.lua` or `.js`, globs allowed).
---@param script FxManifestValue
---@return FxManifestChain
function shared_script(script) end

--- Declares scripts loaded on both server and client (`.lua` or `.js`, globs allowed).
---@param scripts FxManifestValue
---@return FxManifestChain
function shared_scripts(scripts) end

--- Declares a data file sent to clients (not executed as a script, globs allowed).
---@param path FxManifestValue
---@return FxManifestChain
function file(path) end

--- Declares data files sent to clients (not executed as scripts, globs allowed).
---@param paths FxManifestValue
---@return FxManifestChain
function files(paths) end

--- Declares a resource dependency that must start before this resource.
---@param resource FxManifestValue
---@return FxManifestChain
function dependency(resource) end

--- Declares resource dependencies that must start before this resource.
---@param resources FxManifestValue
---@return FxManifestChain
function dependencies(resources) end

--- Declares a client export name (falls back to global function if not registered via `exports`).
---@param name FxManifestValue
---@return FxManifestChain
function export(name) end

--- Declares a server export name (falls back to global function if not registered via `exports`).
---@param name FxManifestValue
---@return FxManifestChain
function server_export(name) end

--- Declares server export names (falls back to global functions if not registered via `exports`).
---@param names FxManifestValue
---@return FxManifestChain
function server_exports(names) end

--- Enables Lua 5.4 mode flag (`'yes'`). SixM always runs Lua 5.4.
---@param enabled FxManifestValue
---@return FxManifestChain
function lua54(enabled) end

--- Declares a resource name that this resource provides.
---@param resource FxManifestValue
---@return FxManifestChain
function provide(resource) end

--- Declares resource names that this resource provides.
---@param resources FxManifestValue
---@return FxManifestChain
function provides(resources) end

--- Declares a game data file entry (typically chained, e.g. `data_file 'TYPE' 'path'`).
---@param fileType FxManifestValue
---@return FxManifestChain
function data_file(fileType) end

-- =============================================================================
-- Scheduling and Citizen
-- =============================================================================

--- Pauses the current thread for `ms` milliseconds. A value `<= 0` waits one frame.
--- Only works inside a thread (`CreateThread` / `CreateThreadNow` / event handler).
---@param ms? number
function Wait(ms) end

--- Starts `fn` as a coroutine thread on the next frame.
---@param fn fun()
function CreateThread(fn) end

--- Starts `fn` immediately as a coroutine thread until its first `Wait`.
---@param fn fun()
function CreateThreadNow(fn) end

--- Schedules `fn` to run once after `ms` milliseconds.
---@param ms number
---@param fn fun()
---@return integer id
function SetTimeout(ms, fn) end

--- Cancels a pending timeout created by `SetTimeout`.
---@param id integer
function ClearTimeout(id) end

--- Returns milliseconds since the process first queried the timer.
---@return integer
function GetGameTimer() end

-- =============================================================================
-- Promises
-- =============================================================================

---@class SixMPromise
local SixMPromise = {}

--- Fulfils the promise with `value` (or adopts `value` if it is a promise).
---@param value? any
function SixMPromise:resolve(value) end

--- Rejects the promise with `reason`.
---@param reason? any
function SixMPromise:reject(reason) end

--- Attaches fulfilment/rejection callbacks and returns a chained promise.
---@param onFulfilled? (fun(value: any): any) | nil
---@param onRejected? (fun(reason: any): any) | nil
---@return SixMPromise
function SixMPromise:next(onFulfilled, onRejected) end

---@class SixMPromiseLib
promise = {}

--- Creates a new pending promise.
---@return SixMPromise
function promise.new() end

--- Returns a promise that fulfils with an array of results once all entries settle,
--- or rejects with the first rejection.
---@param list any[]
---@return SixMPromise
function promise.all(list) end

--- Returns a promise that settles with the first entry in `list` to settle.
---@param list any[]
---@return SixMPromise
function promise.first(list) end

--- Parks the current thread until `p` settles and returns its fulfilled value,
--- or raises its rejection reason as an error.
---@param p SixMPromise
---@return any
function Await(p) end

---@class CitizenNamespace
Citizen = {}

--- Pauses the current thread for `ms` milliseconds (`<= 0` waits one frame).
---@param ms? number
function Citizen.Wait(ms) end

--- Starts `fn` as a coroutine thread on the next frame.
---@param fn fun()
function Citizen.CreateThread(fn) end

--- Starts `fn` immediately until its first `Wait`.
---@param fn fun()
function Citizen.CreateThreadNow(fn) end

--- Schedules `fn` to run once after `ms` milliseconds.
---@param ms number
---@param fn fun()
---@return integer id
function Citizen.SetTimeout(ms, fn) end

--- Cancels a pending timeout created by `Citizen.SetTimeout` / `SetTimeout`.
---@param id integer
function Citizen.ClearTimeout(id) end

--- Logs `text` like `print`, without joining tabs. One trailing newline is dropped.
---@param text string
function Citizen.Trace(text) end

--- Parks the current thread until `p` settles and returns its value or raises its reason.
---@param p SixMPromise
---@return any
function Citizen.Await(p) end

--- Invokes a registered client game-adapter native by its 32-bit hash. (Client only)
---@param hash integer
---@param ... any
---@return any ...
function Citizen.InvokeNative(hash, ...) end

-- =============================================================================
-- Events and source
-- =============================================================================

---@class EventHandlerHandle
---@field key integer
---@field name string

---@class Deferrals
---@field defer fun() Holds the connection open during `playerConnecting`.
---@field update fun(message: string) Sends a progress message (cut to 4096 bytes) to the connecting client.
---@field done fun(reason?: string) Completes the deferral; a non-empty `reason` refuses the connection.

--- Sender of the active event (`""` for local events, player server id as integer for player events, `65535` on client for server events).
---@type string | integer
source = ""

--- A message received from the running `chat` resource.
---@class SixMChatMessage
---@field args (string | number | boolean)[]
---@field color? number[] RGB channels in order.
---@field multiline? boolean
---@field template? string
---@field templateId? string

---@class SixMChatParameter
---@field name string
---@field help? string

---@class SixMChatSuggestion
---@field name string Slash-prefixed command name.
---@field help? string
---@field params? SixMChatParameter[]

--- Registers a local event handler for `name`.
---@overload fun(name: 'chatMessage', fn: fun(playerId: integer, author: string, text: string)): EventHandlerHandle
---@overload fun(name: 'chat:addMessage', fn: fun(message: SixMChatMessage)): EventHandlerHandle
---@overload fun(name: 'chat:addTemplate', fn: fun(templateId: string, html: string)): EventHandlerHandle
---@overload fun(name: 'chat:addSuggestion', fn: fun(name: string, help?: string, params?: SixMChatParameter[])): EventHandlerHandle
---@overload fun(name: 'chat:addSuggestions', fn: fun(suggestions: SixMChatSuggestion[])): EventHandlerHandle
---@overload fun(name: 'chat:removeSuggestion', fn: fun(name: string)): EventHandlerHandle
---@overload fun(name: 'chat:clear', fn: fun()): EventHandlerHandle
---@param name string
---@param fn fun(...: any)
---@return EventHandlerHandle
function AddEventHandler(name, fn) end

--- Removes an event handler using the handle returned by `AddEventHandler` or `RegisterNetEvent`.
---@param handle EventHandlerHandle
function RemoveEventHandler(handle) end

--- Allows `name` to be triggered over the network and optionally attaches `fn`.
---@overload fun(name: '_chat:messageEntered', fn?: fun(text: string)): EventHandlerHandle?
---@overload fun(name: 'chat:init' | 'chat:clear', fn?: fun()): EventHandlerHandle?
---@overload fun(name: 'chat:addMessage', fn?: fun(message: SixMChatMessage)): EventHandlerHandle?
---@overload fun(name: 'chat:addTemplate', fn?: fun(templateId: string, html: string)): EventHandlerHandle?
---@overload fun(name: 'chat:addSuggestion', fn?: fun(name: string, help?: string, params?: SixMChatParameter[])): EventHandlerHandle?
---@overload fun(name: 'chat:addSuggestions', fn?: fun(suggestions: SixMChatSuggestion[])): EventHandlerHandle?
---@overload fun(name: 'chat:removeSuggestion', fn?: fun(name: string)): EventHandlerHandle?
---@param name string
---@param fn? fun(...: any)
---@return EventHandlerHandle?
function RegisterNetEvent(name, fn) end

--- Alias for `RegisterNetEvent`.
---@param name string
---@param fn? fun(...: any)
---@return EventHandlerHandle?
function RegisterServerEvent(name, fn) end

--- Synchronously triggers all local handlers of `name` on this host.
---@overload fun(name: 'chatMessage', playerId: integer, author: string, text: string)
---@overload fun(name: 'chat:addMessage', message: SixMChatMessage)
---@overload fun(name: 'chat:addTemplate', templateId: string, html: string)
---@overload fun(name: 'chat:addSuggestion', command: string, help?: string, params?: SixMChatParameter[])
---@overload fun(name: 'chat:addSuggestions', suggestions: SixMChatSuggestion[])
---@overload fun(name: 'chat:removeSuggestion', command: string)
---@overload fun(name: 'chat:clear')
---@param name string
---@param ... any
function TriggerEvent(name, ...) end

--- Marks the active event dispatch as canceled.
function CancelEvent() end

--- Returns whether the active event dispatch was canceled.
---@return boolean
function WasEventCanceled() end

--- Sends a network event to the server. Functions cannot be sent. (Client only)
---@overload fun(name: '_chat:messageEntered', text: string)
---@overload fun(name: 'chat:init')
---@param name string
---@param ... any
function TriggerServerEvent(name, ...) end

--- Sends a network event to `target` player id or `-1` for all players. Functions cannot be sent. (Server only)
---@overload fun(name: 'chat:addMessage', target: integer | string, message: SixMChatMessage)
---@overload fun(name: 'chat:addTemplate', target: integer | string, templateId: string, html: string)
---@overload fun(name: 'chat:addSuggestion', target: integer | string, command: string, help?: string, params?: SixMChatParameter[])
---@overload fun(name: 'chat:addSuggestions', target: integer | string, suggestions: SixMChatSuggestion[])
---@overload fun(name: 'chat:removeSuggestion', target: integer | string, command: string)
---@overload fun(name: 'chat:clear', target: integer | string)
---@param name string
---@param target integer | string
---@param ... any
function TriggerClientEvent(name, target, ...) end

--- Sends a bandwidth-paced network event to the server. (Client only)
---@param name string
---@param bytesPerSecond number
---@param ... any
function TriggerLatentServerEvent(name, bytesPerSecond, ...) end

--- Sends a bandwidth-paced network event to `target` player id or `-1` for all players. (Server only)
---@param name string
---@param target integer | string
---@param bytesPerSecond number
---@param ... any
function TriggerLatentClientEvent(name, target, bytesPerSecond, ...) end

-- =============================================================================
-- Exports (runtime + manifest directive)
-- =============================================================================

---@class SixMExports
---@field [string] table<string, function>
---@overload fun(name: string, fn: function)
---@overload fun(manifestNames: FxManifestValue): FxManifestChain
exports = {}

-- =============================================================================
-- Resources and files
-- =============================================================================

---@alias ResourceState 'missing' | 'stopped' | 'starting' | 'started' | 'stopping'

--- Returns the calling resource's own name.
---@return string
function GetCurrentResourceName() end

--- Returns the filesystem path of `name` (server: any installed resource; client: only own data folder), or `nil`.
---@param name string
---@return string?
function GetResourcePath(name) end

--- Reads a file from `resource` at relative `path`, or `nil` if missing or invalid.
---@param resource string
---@param path string
---@return string?
function LoadResourceFile(resource, path) end

--- Writes `data` to `path` inside `resource` (must be the calling resource).
---@param resource string
---@param path string
---@param data string
---@return boolean
function SaveResourceFile(resource, path, data) end

--- Returns the name of the resource that invoked the current export or local event, or `nil`.
---@return string?
function GetInvokingResource() end

--- Returns the current lifecycle state of `name`.
---@param name string
---@return ResourceState
function GetResourceState(name) end

--- Returns the number of resources known to `GetResourceByFindIndex`.
---@return integer
function GetNumResources() end

--- Returns the resource name at 0-based `index`, or `nil`.
---@param index integer
---@return string?
function GetResourceByFindIndex(index) end

--- Returns the metadata value at 0-based `index` for `key` on `resource`, or `nil`.
---@param resource string
---@param key string
---@param index integer
---@return string?
function GetResourceMetadata(resource, key, index) end

--- Returns how many metadata values `resource` has for `key`.
---@param resource string
---@param key string
---@return integer
function GetNumResourceMetadata(resource, key) end

--- Returns a sorted array of the console command names `resource` registered, or `{}`.
---@param resource string
---@return string[]
function GetResourceCommands(resource) end

--- Starts resource `name` if allowed by `command.start` ACE permission. (Server only)
---@param name string
---@return boolean
function StartResource(name) end

--- Stops resource `name` if allowed by `command.stop` ACE permission. (Server only)
---@param name string
---@return boolean
function StopResource(name) end

--- Restarts resource `name` if allowed by ACE permission. (Server only)
---@param name string
---@return boolean
function RestartResource(name) end

-- =============================================================================
-- Key-value storage (KVP)
-- =============================================================================

--- Returns the string value stored under `key` for this resource, or `nil`.
---@param key string
---@return string?
function GetResourceKvpString(key) end

--- Returns the integer stored under `key`, or `0` when unset or not an integer.
---@param key string
---@return integer
function GetResourceKvpInt(key) end

--- Returns the float stored under `key`, or `0.0` when unset or not a number.
---@param key string
---@return number
function GetResourceKvpFloat(key) end

--- Stores a string `value` under `key`.
---@param key string
---@param value string
---@return boolean
function SetResourceKvp(key, value) end

--- Stores an integer `value` under `key` as decimal text.
---@param key string
---@param value integer
---@return boolean
function SetResourceKvpInt(key, value) end

--- Stores a float `value` under `key`.
---@param key string
---@param value number
---@return boolean
function SetResourceKvpFloat(key, value) end

--- Deletes `key` from this resource's KVP store. Returns `true` if the key existed.
---@param key string
---@return boolean
function DeleteResourceKvp(key) end

--- Alias for `SetResourceKvp` (writes are batched asynchronously).
---@param key string
---@param value string
---@return boolean
function SetResourceKvpNoSync(key, value) end

--- Alias for `SetResourceKvpInt` (writes are batched asynchronously).
---@param key string
---@param value integer
---@return boolean
function SetResourceKvpIntNoSync(key, value) end

--- Alias for `SetResourceKvpFloat` (writes are batched asynchronously).
---@param key string
---@param value number
---@return boolean
function SetResourceKvpFloatNoSync(key, value) end

--- Alias for `DeleteResourceKvp` (writes are batched asynchronously).
---@param key string
---@return boolean
function DeleteResourceKvpNoSync(key) end

--- Starts iterating KVP keys matching `prefix` in byte order.
---@param prefix string
---@return integer handle
function StartFindKvp(prefix) end

--- Returns the next key for `handle`, or `nil` when exhausted.
---@param handle integer
---@return string?
function FindKvp(handle) end

--- Closes a KVP find `handle`.
---@param handle integer
function EndFindKvp(handle) end

--- Hands all pending KVP changes to the background disk writer thread immediately.
function FlushResourceKvp() end

-- =============================================================================
-- Convars and commands
-- =============================================================================

--- Returns the convar text value, or `default` when unset.
---@param name string
---@param default string
---@return string
function GetConvar(name, default) end

--- Returns the convar parsed as an integer, or `default` when unset or invalid.
---@param name string
---@param default integer
---@return integer
function GetConvarInt(name, default) end

--- Returns the convar parsed as a floating-point number, or `default` when unset or invalid.
---@param name string
---@param default number
---@return number
function GetConvarFloat(name, default) end

--- Returns the convar parsed as a boolean (`true`/`1`/`yes`/`on`, `false`/`0`/`no`/`off`), or `default`.
---@param name string
---@param default boolean
---@return boolean
function GetConvarBool(name, default) end

--- Sets a plain server-only convar. (Server only)
---@param name string
---@param value string
function SetConvar(name, value) end

--- Sets a replicated convar synced to all connected and future clients. (Server only)
---@param name string
---@param value string
function SetConvarReplicated(name, value) end

--- Sets a convar marked as server info. (Server only)
---@param name string
---@param value string
function SetConvarServerInfo(name, value) end

--- Sets the `gametype` server info convar. (Server only)
---@param name string
function SetGameType(name) end

--- Sets the `mapname` server info convar. (Server only)
---@param name string
function SetMapName(name) end

--- Registers a listener invoked when convars matching glob `filter` (`nil` or `""` for all) change.
---@param filter string?
---@param fn fun(convarName: string, reserved: integer)
---@return integer cookie
function AddConvarChangeListener(filter, fn) end

--- Removes a convar change listener by its cookie.
---@param cookie integer
function RemoveConvarChangeListener(cookie) end

--- Queues a console command line to execute after the current script call returns.
---@param line string
function ExecuteCommand(line) end

--- Registers a console command handler.
---@param name string
---@param fn fun(source: integer, args: string[], raw: string)
---@param restricted? boolean
function RegisterCommand(name, fn, restricted) end

--- Returns a sorted array of registered command descriptors `{ name = ... }`.
---@return { name: string }[]
function GetRegisteredCommands() end

--- Returns `'sixm'` on a server and `'headless'` on a client.
---@return string
function GetGameName() end

--- Returns the game build number, always `0`.
---@return integer
function GetGameBuildNumber() end

--- Returns the process id, stable for the life of the process.
---@return integer
function GetInstanceId() end

-- =============================================================================
-- Players (server and client)
-- =============================================================================

--- Returns an array of connected player server ids as strings, in join order. (Server only)
---@return string[]
function GetPlayers() end

--- Returns the number of connected (admitted) players. (Server only)
---@return integer
function GetNumPlayerIndices() end

--- Returns the player's name, or `nil` when not connected. (Server only)
---@param player integer | string
---@return string?
function GetPlayerName(player) end

--- Returns the entity id of the player's ped, or `0` if none.
---@param player integer | string
---@return integer
function GetPlayerPed(player) end

--- Returns the player's remote `ip:port` endpoint, or `nil`. (Server only)
---@param player integer | string
---@return string?
function GetPlayerEndpoint(player) end

--- Returns an array of identifiers for `player`. (Server only)
---@param player integer | string
---@return string[]
function GetPlayerIdentifiers(player) end

--- Returns the number of identifiers for `player`. (Server only)
---@param player integer | string
---@return integer
function GetNumPlayerIdentifiers(player) end

--- Returns the identifier at 0-based `index` for `player`, or `nil`. (Server only)
---@param player integer | string
---@param index integer
---@return string?
function GetPlayerIdentifier(player, index) end

--- Returns the hex part of the player's `license:` identifier, or `nil`. (Server only)
---@param player integer | string
---@return string?
function GetPlayerGuid(player) end

--- Returns whole seconds since the player was admitted, or `nil`. (Server only)
---@param player integer | string
---@return integer?
function GetPlayerTimeOnline(player) end

--- Returns the player at 0-based `index` of the join-order player list as a string, or `nil`. (Server only)
---@param index integer
---@return string?
function GetPlayerFromIndex(index) end

--- Returns the lowest connected player id as a string, or `nil` when nobody is connected. (Server only)
---@return string?
function GetHostId() end

--- Returns the first identifier whose prefix before `:` equals `kind`, or `nil`. (Server only)
---@param player integer | string
---@param kind string
---@return string?
function GetPlayerIdentifierByType(player, kind) end

--- Returns milliseconds since the last message from `player`, or `-1`. (Server only)
---@param player integer | string
---@return integer
function GetPlayerLastMsg(player) end

--- Returns round-trip latency in milliseconds for `player`, or `-1` if unknown. (Server only)
---@param player integer | string
---@return integer
function GetPlayerPing(player) end

--- Returns the number of client identity tokens for `player`. (Server only)
---@param player integer | string
---@return integer
function GetNumPlayerTokens(player) end

--- Returns the identity token at 0-based `index` for `player`, or `nil`. (Server only)
---@param player integer | string
---@param index integer
---@return string?
function GetPlayerToken(player, index) end

--- Disconnects `player` with optional `reason` (defaults to `'Dropped by server'`). (Server only)
---@param player integer | string
---@param reason? string
function DropPlayer(player, reason) end

--- Returns whether `player` exists / is currently connected. (Server only)
---@param player integer | string
---@return boolean
function DoesPlayerExist(player) end

--- Returns whether `player` is allowed ACE `object`. (Server only)
---@param player integer | string
---@param object string
---@return boolean
function IsPlayerAceAllowed(player, object) end

--- Returns whether ACE `principal` (e.g. `'group.admin'`) is allowed `object`. (Server only)
---@param principal string
---@param object string
---@return boolean
function IsPrincipalAceAllowed(principal, object) end

--- Returns whether the calling resource (`resource.<name>`) is allowed ACE `object`. (Server only)
---@param object string
---@return boolean
function IsAceAllowed(object) end

--- Returns the local client player handle (`0`). (Client only)
---@return integer
function PlayerId() end

--- Returns the local player's server id when `player` is `0`, or `nil`. (Client only)
---@param player integer | string
---@return integer?
function GetPlayerServerId(player) end

--- Returns the entity id of the local player's ped, or `0` until received. (Client only)
---@return integer
function PlayerPedId() end

-- =============================================================================
-- State bags
-- =============================================================================

---@class StateBag
---@field [string] any
local StateBag = {}

--- Writes `value` to `key` in this state bag. `replicated` defaults to `true`.
--- Clients cannot write with `replicated = true`.
---@param key string
---@param value any
---@param replicated? boolean
function StateBag:set(key, value, replicated) end

---@class StateBagHolder
---@field state StateBag

--- The global replicated state bag.
---@type StateBag
GlobalState = {}

--- Returns a table containing `.state` for player `id`. (Server only)
---@param id integer | string
---@return StateBagHolder
function Player(id) end

--- Local player state holder with `.state`. (Client only)
---@type StateBagHolder
LocalPlayer = {}

--- Returns a table containing `.state` for `entity` (`entity:<id>`).
---@param entity integer
---@return StateBagHolder
function Entity(entity) end

--- Registers a state bag change handler. Either filter may be `nil` to match all.
---@param keyFilter string?
---@param bagFilter string?
---@param fn fun(bagName: string, key: string, value: any, reserved: integer, replicated: boolean)
---@return integer cookie
function AddStateBagChangeHandler(keyFilter, bagFilter, fn) end

--- Removes a state bag change handler by its cookie.
---@param cookie integer
function RemoveStateBagChangeHandler(cookie) end

--- Extracts the player server id from a `player:<id>` bag name, or returns `0`.
---@param bagName string
---@return integer
function GetPlayerFromStateBagName(bagName) end

--- Extracts the entity id from an `entity:<id>` bag name if it exists, or returns `0`.
---@param bagName string
---@return integer
function GetEntityFromStateBagName(bagName) end

--- Returns the value of `key` in state bag `bagName`, or nil when unset.
---@param bagName string
---@param key string
---@return any
function GetStateBagValue(bagName, key) end

--- Returns true when state bag `bagName` has a value for `key`.
---@param bagName string
---@param key string
---@return boolean
function StateBagHasKey(bagName, key) end

--- Returns the key names of state bag `bagName`, sorted, or nil for an unknown bag.
---@param bagName string
---@return string[]?
function GetStateBagKeys(bagName) end

--- Writes `value` under `key` in state bag `bagName`. `replicated` defaults to false.
---@param bagName string
---@param key string
---@overload fun(bagName: string, key: string, valueData: string, valueLength: integer, replicated: boolean?)
---@param value any
---@param replicated boolean?
function SetStateBagValue(bagName, key, value, replicated) end

--- Compatibility call; does nothing, since an entity's bag lives as long as the entity.
---@param entity integer
function EnsureEntityStateBag(entity) end

-- =============================================================================
-- Vectors and quaternions
-- =============================================================================

---@class vector2
---@field x number
---@field y number
---@field [1] number
---@field [2] number
---@operator add(vector2 | number): vector2
---@operator sub(vector2 | number): vector2
---@operator mul(vector2 | number): vector2
---@operator div(vector2 | number): vector2
---@operator unm: vector2
---@operator len: number
local vector2_proto = {}

---@class vector3
---@field x number
---@field y number
---@field z number
---@field xy vector2
---@field xz vector2
---@field yz vector2
---@field [1] number
---@field [2] number
---@field [3] number
---@operator add(vector3 | number): vector3
---@operator sub(vector3 | number): vector3
---@operator mul(vector3 | number): vector3
---@operator div(vector3 | number): vector3
---@operator unm: vector3
---@operator len: number
local vector3_proto = {}

---@class vector4
---@field x number
---@field y number
---@field z number
---@field w number
---@field xy vector2
---@field xyz vector3
---@field [1] number
---@field [2] number
---@field [3] number
---@field [4] number
---@operator add(vector4 | number): vector4
---@operator sub(vector4 | number): vector4
---@operator mul(vector4 | number): vector4
---@operator div(vector4 | number): vector4
---@operator unm: vector4
---@operator len: number
local vector4_proto = {}

---@class quat
---@field w number
---@field x number
---@field y number
---@field z number
---@field [1] number
---@field [2] number
---@field [3] number
---@field [4] number
---@operator mul(quat): quat
---@operator mul(vector3): vector3
local quat_proto = {}

--- Constructs a 2D vector from `(x, y)` or a single scalar `(v)`.
---@overload fun(v: number): vector2
---@overload fun(v: vector2): vector2
---@param x number
---@param y number
---@return vector2
function vector2(x, y) end

--- Shorthand alias for `vector2`.
---@overload fun(v: number): vector2
---@overload fun(v: vector2): vector2
---@param x number
---@param y number
---@return vector2
function vec2(x, y) end

--- Constructs a 3D vector from `(x, y, z)`, `(vector2, z)`, `(x, vector2)`, or scalar `(v)`.
---@overload fun(v: number): vector3
---@overload fun(v: vector3): vector3
---@overload fun(xy: vector2, z: number): vector3
---@overload fun(x: number, yz: vector2): vector3
---@param x number
---@param y number
---@param z number
---@return vector3
function vector3(x, y, z) end

--- Shorthand alias for `vector3`.
---@overload fun(v: number): vector3
---@overload fun(v: vector3): vector3
---@overload fun(xy: vector2, z: number): vector3
---@overload fun(x: number, yz: vector2): vector3
---@param x number
---@param y number
---@param z number
---@return vector3
function vec3(x, y, z) end

--- Constructs a 4D vector from components, sub-vectors, or a scalar.
---@overload fun(v: number): vector4
---@overload fun(v: vector4): vector4
---@overload fun(xyz: vector3, w: number): vector4
---@overload fun(xy: vector2, zw: vector2): vector4
---@overload fun(xy: vector2, z: number, w: number): vector4
---@param x number
---@param y number
---@param z number
---@param w number
---@return vector4
function vector4(x, y, z, w) end

--- Shorthand alias for `vector4`.
---@overload fun(v: number): vector4
---@overload fun(v: vector4): vector4
---@overload fun(xyz: vector3, w: number): vector4
---@overload fun(xy: vector2, zw: vector2): vector4
---@overload fun(xy: vector2, z: number, w: number): vector4
---@param x number
---@param y number
---@param z number
---@param w number
---@return vector4
function vec4(x, y, z, w) end

--- Constructs a quaternion from `(w, x, y, z)`, `(w, vector3)`, scalar `(v)`, or another `quat`.
---@overload fun(other: quat): quat
---@overload fun(v: number): quat
---@overload fun(w: number, xyz: vector3): quat
---@param w number
---@param x number
---@param y number
---@param z number
---@return quat
function quat(w, x, y, z) end

--- Builds a vector2, vector3 or vector4 from numbers and vectors, chosen by the total component count.
---@overload fun(a: number, b: number): vector2
---@overload fun(a: number, b: number, c: number): vector3
---@overload fun(a: number, b: number, c: number, d: number): vector4
---@param ... number | vector2 | vector3 | vector4
---@return vector2 | vector3 | vector4
function vector(...) end

--- The sum of the component products of two vectors of the same size.
---@overload fun(a: vector2, b: vector2): number
---@overload fun(a: vector3, b: vector3): number
---@overload fun(a: vector4, b: vector4): number
---@param a vector2 | vector3 | vector4
---@param b vector2 | vector3 | vector4
---@return number
function dot(a, b) end

--- The cross product of two vector3.
---@param a vector3
---@param b vector3
---@return vector3
function cross(a, b) end

--- Scales a vector or quat to length 1. A zero vector is returned unchanged.
---@overload fun(v: vector2): vector2
---@overload fun(v: vector3): vector3
---@overload fun(v: vector4): vector4
---@overload fun(q: quat): quat
---@param v vector2 | vector3 | vector4 | quat
---@return vector2 | vector3 | vector4 | quat
function norm(v) end

--- The inverse of a quat: its conjugate divided by its squared length. Errors for a zero quat.
---@param q quat
---@return quat
function inv(q) end

--- Spherical interpolation between two quats. `t` is not clamped.
---@param a quat
---@param b quat
---@param t number
---@return quat
function slerp(a, b, t) end

-- =============================================================================
-- Entities and routing buckets
-- =============================================================================

--- Creates a vehicle entity. On client, `isNetwork` must be `true`.
---@param model integer | string
---@param x number
---@param y number
---@param z number
---@param heading number
---@param isNetwork? boolean
---@param netMissionEntity? boolean
---@return integer entity
function CreateVehicle(model, x, y, z, heading, isNetwork, netMissionEntity) end

--- Creates a vehicle on the server (`type` is accepted for FiveM compatibility and ignored). (Server only)
---@param model integer | string
---@param type string
---@param x number
---@param y number
---@param z number
---@param heading number
---@return integer entity
function CreateVehicleServerSetter(model, type, x, y, z, heading) end

--- Creates a ped entity. On client, `isNetwork` must be `true`.
---@param pedType integer
---@param model integer | string
---@param x number
---@param y number
---@param z number
---@param heading number
---@param isNetwork? boolean
---@param bScriptHostPed? boolean
---@return integer entity
function CreatePed(pedType, model, x, y, z, heading, isNetwork, bScriptHostPed) end

--- Creates an object entity. On client, `isNetwork` must be `true`.
---@param model integer | string
---@param x number
---@param y number
---@param z number
---@param isNetwork? boolean
---@param netMissionEntity? boolean
---@param doorFlag? boolean
---@return integer entity
function CreateObject(model, x, y, z, isNetwork, netMissionEntity, doorFlag) end

--- Creates an object entity on the server without offset adjustment. (Server only)
---@param model integer | string
---@param x number
---@param y number
---@param z number
---@param isNetwork? boolean
---@param netMissionEntity? boolean
---@param doorFlag? boolean
---@return integer entity
function CreateObjectNoOffset(model, x, y, z, isNetwork, netMissionEntity, doorFlag) end

--- Deletes an entity on the server. Does nothing if unknown. (Server only)
---@param entity integer
function DeleteEntity(entity) end

--- Sets the world coordinates of `entity`. On client, only affects owned entities.
---@param entity integer
---@param xPos number
---@param yPos number
---@param zPos number
---@param xAxis? boolean
---@param yAxis? boolean
---@param zAxis? boolean
---@param clearArea? boolean
function SetEntityCoords(entity, xPos, yPos, zPos, xAxis, yAxis, zAxis, clearArea) end

--- Sets the world coordinates of `entity` without offset. On client, only affects owned entities.
---@param entity integer
---@param xPos number
---@param yPos number
---@param zPos number
---@param xAxis? boolean
---@param yAxis? boolean
---@param zAxis? boolean
function SetEntityCoordsNoOffset(entity, xPos, yPos, zPos, xAxis, yAxis, zAxis) end

--- Sets the yaw heading of `entity` in degrees, preserving pitch and roll.
---@param entity integer
---@param heading number
function SetEntityHeading(entity, heading) end

--- Sets the Euler rotation of `entity` in degrees (`pitch`, `roll`, `yaw`).
---@param entity integer
---@param pitch number
---@param roll number
---@param yaw number
---@param rotationOrder? integer
---@param bDeadCheck? boolean
function SetEntityRotation(entity, pitch, roll, yaw, rotationOrder, bDeadCheck) end

--- Sets the orientation of `entity` from quaternion `(x, y, z, w)`.
---@param entity integer
---@param x number
---@param y number
---@param z number
---@param w number
function SetEntityQuaternion(entity, x, y, z, w) end

--- Returns the server id of the player that owns `entity`, or `-1` if server-owned or unknown.
---@param entity integer
---@return integer
function NetworkGetEntityOwner(entity) end

--- Returns the player id that originally created `entity`, or `-1`. (Server only)
---@param entity integer
---@return integer
function NetworkGetFirstEntityOwner(entity) end

--- Requests ownership of an unowned entity in scope from the server. (Client only)
---@param entity integer
---@return boolean sent
function NetworkRequestControlOfEntity(entity) end

--- Transfers ownership of `entity` to `player` (`0` for server). (Server only)
---@param entity integer
---@param player integer
---@return boolean updated
function SetEntityOwner(entity, player) end

--- Sets what happens when a client-created entity's creator disconnects. (Server only)
---@param entity integer
---@param mode 'delete' | 'migrate'
function SetEntityOrphanMode(entity, mode) end

--- Overrides the distance culling radius in metres for `entity` (`<= 0` resets to global). (Server only)
---@param entity integer
---@param radius number
function SetEntityDistanceCullingRadius(entity, radius) end

--- Returns all vehicle entity ids on the server in ascending order. (Server only)
---@return integer[]
function GetAllVehicles() end

--- Returns all ped entity ids on the server in ascending order. (Server only)
---@return integer[]
function GetAllPeds() end

--- Returns all object entity ids on the server in ascending order. (Server only)
---@return integer[]
function GetAllObjects() end

--- Returns entity ids in the client pool (`'CVehicle'`, `'CPed'`, `'CObject'`). (Client only)
---@param name 'CVehicle' | 'CPed' | 'CObject' | string
---@return integer[]
function GetGamePool(name) end

--- Returns whether `entity` exists.
---@param entity integer
---@return boolean
function DoesEntityExist(entity) end

--- Returns the entity's position as a `vector3`.
---@param entity integer
---@param alive? boolean
---@return vector3
function GetEntityCoords(entity, alive) end

--- Returns the entity's yaw heading in degrees.
---@param entity integer
---@return number
function GetEntityHeading(entity) end

--- Returns the entity's rotation as `vector3(pitch, roll, yaw)`.
---@param entity integer
---@param rotationOrder? integer
---@return vector3
function GetEntityRotation(entity, rotationOrder) end

--- Returns the entity's orientation quaternion as four floats `x, y, z, w`.
---@param entity integer
---@return number x
---@return number y
---@return number z
---@return number w
function GetEntityQuaternion(entity) end

--- Returns the entity's model hash as a signed 32-bit integer.
---@param entity integer
---@return integer
function GetEntityModel(entity) end

--- Returns the entity type (`1` = ped, `2` = vehicle, `3` = object, `0` = none).
---@param entity integer
---@return 0 | 1 | 2 | 3
function GetEntityType(entity) end

--- Returns the name of the resource that created `entity`, or `nil`.
---@param entity integer
---@return string?
function GetEntityScript(entity) end

--- Returns the network id for `entity`, or `0` if it does not exist.
---@param entity integer
---@return integer
function NetworkGetNetworkIdFromEntity(entity) end

--- Returns the entity handle for `networkId`, or `0` if it does not exist.
---@param networkId integer
---@return integer
function NetworkGetEntityFromNetworkId(networkId) end

--- Returns whether `networkId` exists.
---@param networkId integer
---@return boolean
function NetworkDoesNetworkIdExist(networkId) end

--- Freezes or unfreezes `entity` position. (Server only)
---@param entity integer
---@param toggle boolean
function FreezeEntityPosition(entity, toggle) end

--- Returns whether `entity` position is frozen.
---@param entity integer
---@return boolean
function IsEntityPositionFrozen(entity) end

--- Sets `entity` velocity in m/s. (Server only)
---@param entity integer
---@param x number
---@param y number
---@param z number
function SetEntityVelocity(entity, x, y, z) end

--- Returns `entity` velocity as a `vector3`.
---@param entity integer
---@return vector3
function GetEntityVelocity(entity) end

--- Sets `entity` health (clamped to `>= 0`). (Server only)
---@param entity integer
---@param health number
function SetEntityHealth(entity, health) end

--- Returns `entity` health (default `200`, `0` when unknown).
---@param entity integer
---@return integer
function GetEntityHealth(entity) end

--- Sets `entity` maximum health (clamped to `>= 0`). (Server only)
---@param entity integer
---@param maxHealth number
function SetEntityMaxHealth(entity, maxHealth) end

--- Returns `entity` maximum health (default `200`, `0` when unknown).
---@param entity integer
---@return integer
function GetEntityMaxHealth(entity) end

--- Sets whether `entity` is visible. (Server only)
---@param entity integer
---@param toggle boolean
---@param unk? boolean
function SetEntityVisible(entity, toggle, unk) end

--- Returns whether `entity` is visible.
---@param entity integer
---@return boolean
function IsEntityVisible(entity) end

--- Sets `entity` alpha opacity (`0..255`). (Server only)
---@param entity integer
---@param alpha integer
---@param skin? boolean
function SetEntityAlpha(entity, alpha, skin) end

--- Returns `entity` alpha opacity (`0..255`).
---@param entity integer
---@return integer
function GetEntityAlpha(entity) end

--- Resets `entity` alpha opacity to `255`. (Server only)
---@param entity integer
function ResetEntityAlpha(entity) end

--- Attaches `entity` to `target` with local offset and rotation. (Server only)
---@param entity integer
---@param target integer
---@param boneIndex integer
---@param x number
---@param y number
---@param z number
---@param rx number
---@param ry number
---@param rz number
---@param p9? boolean
---@param useSoftPinning? boolean
---@param collision? boolean
---@param isPed? boolean
---@param vertexIndex? integer
---@param fixedRot? boolean
function AttachEntityToEntity(entity, target, boneIndex, x, y, z, rx, ry, rz, p9, useSoftPinning, collision, isPed, vertexIndex, fixedRot) end

--- Detaches `entity` from its parent if attached. (Server only)
---@param entity integer
---@param dynamic? boolean
---@param collision? boolean
function DetachEntity(entity, dynamic, collision) end

--- Returns the parent entity id `entity` is attached to, or `0`.
---@param entity integer
---@return integer
function GetEntityAttachedTo(entity) end

--- Returns whether `entity` is attached to another entity.
---@param entity integer
---@return boolean
function IsEntityAttached(entity) end

--- Assigns `player` (and their ped) to routing `bucket` (`0..4294967295`). (Server only)
---@param player integer | string
---@param bucket integer
function SetPlayerRoutingBucket(player, bucket) end

--- Returns `player`'s current routing bucket id. (Server only)
---@param player integer | string
---@return integer
function GetPlayerRoutingBucket(player) end

--- Assigns `entity` to routing `bucket` (`0..4294967295`). (Server only)
---@param entity integer
---@param bucket integer
function SetEntityRoutingBucket(entity, bucket) end

--- Returns `entity`'s current routing bucket id. (Server only)
---@param entity integer
---@return integer
function GetEntityRoutingBucket(entity) end

--- Returns connected player ids in `bucket`, in ascending order. (Server only)
---@param bucket integer
---@return integer[]
function GetPlayersInRoutingBucket(bucket) end

--- Enables or disables population spawning in `bucket`. (Server only)
---@param bucket integer
---@param enabled boolean
function SetRoutingBucketPopulationEnabled(bucket, enabled) end

--- Returns whether population spawning is enabled in `bucket`. (Server only)
---@param bucket integer
---@return boolean
function GetRoutingBucketPopulationEnabled(bucket) end

--- Sets entity creation lockdown mode for `bucket`. (Server only)
---@param bucket integer
---@param mode 'inactive' | 'relaxed' | 'strict'
function SetRoutingBucketEntityLockdownMode(bucket, mode) end

--- Returns entity creation lockdown mode for `bucket`. (Server only)
---@param bucket integer
---@return 'inactive' | 'relaxed' | 'strict'
function GetRoutingBucketEntityLockdownMode(bucket) end

-- =============================================================================
-- HTTP (server only)
-- =============================================================================

---@class SixMHttpRequestOptions
---@field timeout? number Request timeout in milliseconds.

---@class SixMHttpRequest
---@field method string
---@field path string
---@field query string
---@field headers table<string, string>
---@field setDataHandler fun(callback: fun(data: string))

---@class SixMHttpResponse
---@field writeHead fun(status: integer, headers?: table<string, string | number>)
---@field write fun(data?: string)
---@field send fun(data?: string)

--- Starts an asynchronous HTTP request and invokes `callback(statusCode, body, headers, errorData)`. (Server only)
---@param url string
---@param callback fun(statusCode: integer, body: string, headers: table<string, string>, errorData: string)
---@param method? string
---@param data? string
---@param headers? table<string, string | number>
---@param options? SixMHttpRequestOptions
function PerformHttpRequest(url, callback, method, data, headers, options) end

--- Starts an HTTP request and parks the current thread until it finishes. Works in a thread or a script's main chunk, not in a context that cannot yield. (Server only)
---@param url string
---@param method? string
---@param data? string
---@param headers? table<string, string | number>
---@param options? SixMHttpRequestOptions
---@return integer status
---@return string body
---@return table headers
---@return string errorData
function PerformHttpRequestAwait(url, method, data, headers, options) end

--- Registers the resource's HTTP handler for `/<resource>` and `/<resource>/...`. (Server only)
---@param handler fun(req: SixMHttpRequest, res: SixMHttpResponse)
function SetHttpHandler(handler) end

-- =============================================================================
-- JSON, msgpack, and other globals
-- =============================================================================

---@class SixMJsonLib
json = {}

--- Encodes a Lua value as a JSON string.
---@param value any
---@return string
function json.encode(value) end

--- Decodes a JSON string into a Lua value.
---@param text string
---@return any
function json.decode(text) end

---@class SixMMsgpackLib
msgpack = {}

--- Encodes a Lua value into MessagePack binary data.
---@param value any
---@return string
function msgpack.pack(value) end

--- Decodes MessagePack binary data into a Lua value.
---@param data string
---@return any
function msgpack.unpack(data) end

--- Returns the case-insensitive JOAAT hash of `name` as a signed 32-bit integer.
---@param name string
---@return integer
function GetHashKey(name) end

--- Returns `true` on the server and `false` on the client.
---@return boolean
function IsDuplicityVersion() end

--- Writes arguments joined with tabs to the SixM log, prefixed with the resource name.
---@param ... any
function print(...) end
