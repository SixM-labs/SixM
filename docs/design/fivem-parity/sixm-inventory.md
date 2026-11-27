# SixM inventory (what script developers and server operators get today)

Sources: docs/*.md, docs/guides, types/, examples/server-data, crates/sixm-{lua,js,server,client,resources}/src.
Registered global names were taken from string literals in crates/sixm-lua/src and crates/sixm-js/src.
Legend: (S)=server only, (C)=client only, (B)=both. Everything else is B.
No game adapter: no GTA natives; client is headless. `--sim` SimAdapter gives GetEntitySpeed, TaskGoStraightToCoord, InvokeNative (client only, test aid).

## 1. Lua globals / functions (sixm-lua, Lua 5.4 sandbox)
### Scheduling / async
- Wait, Citizen.Wait: pause thread
- CreateThread, Citizen.CreateThread: start thread next frame
- CreateThreadNow, Citizen.CreateThreadNow: start thread immediately
- SetTimeout, Citizen.SetTimeout: one-shot timer, returns id
- ClearTimeout, Citizen.ClearTimeout: cancel timer
- GetGameTimer: ms since process timer start
- promise.new / promise.all / promise.first: promises
- p:resolve, p:reject, p:next: promise methods
- Await, Citizen.Await: await promise in thread
### Events
- AddEventHandler, RemoveEventHandler: handle table {key,name}
- RegisterNetEvent, RegisterServerEvent: allow network event, optional fn
- TriggerEvent: local dispatch
- TriggerServerEvent (C), TriggerLatentServerEvent (C): to server, latent paced
- TriggerClientEvent (S), TriggerLatentClientEvent (S): id, id table, or -1
- CancelEvent, WasEventCanceled: cancel dispatch
- source global: "" local, player id, 65535 from server; preserved across Wait
### Exports
- exports(name, fn): register export
- exports[res].name(...) / exports[res]:name(...): sync cross-resource call (Lua<->JS mixed)
- manifest export/server_export fall back to global function
### Resources / files
- GetCurrentResourceName, GetInvokingResource
- GetResourcePath, LoadResourceFile, SaveResourceFile (4 MiB; no script/manifest overwrite)
- GetResourceState (missing/stopped/starting/started/stopping)
- GetNumResources, GetResourceByFindIndex
- GetResourceMetadata, GetNumResourceMetadata
- StartResource (S), StopResource (S), RestartResource (S): ACE-gated command.start/stop/restart
### KVP
- GetResourceKvpString/Int/Float
- SetResourceKvp, SetResourceKvpInt, SetResourceKvpFloat
- SetResourceKvpNoSync, SetResourceKvpIntNoSync, SetResourceKvpFloatNoSync: same as synced (batched)
- DeleteResourceKvp, DeleteResourceKvpNoSync
- StartFindKvp, FindKvp, EndFindKvp
- FlushResourceKvp
### Convars / commands
- GetConvar, GetConvarInt, GetConvarFloat, GetConvarBool
- SetConvar (S), SetConvarReplicated (S), SetConvarServerInfo (S)
- AddConvarChangeListener, RemoveConvarChangeListener: glob filter
- ExecuteCommand: runs with resource.<name> ACE principal
- RegisterCommand(name, fn, restricted): fn(source,args,raw)
- GetRegisteredCommands
### Players (S)
- GetPlayers, GetNumPlayerIndices, DoesPlayerExist
- GetPlayerName, GetPlayerPed, GetPlayerEndpoint, GetPlayerPing, GetPlayerLastMsg
- GetPlayerIdentifiers, GetNumPlayerIdentifiers, GetPlayerIdentifier, GetPlayerIdentifierByType
- GetNumPlayerTokens, GetPlayerToken: certificate-fingerprint tokens
- DropPlayer
- IsPlayerAceAllowed, IsPrincipalAceAllowed
- Player(id).state: player state bag
- Identifiers only license:<sha256 of cert> and ip:<addr>
### Players (C)
- PlayerId (always 0), GetPlayerServerId, PlayerPedId, GetPlayerPed
- LocalPlayer.state
### State bags
- GlobalState, bag:set(key,val,replicated)
- Player(id).state (S), LocalPlayer.state (C), Entity(id).state
- AddStateBagChangeHandler, RemoveStateBagChangeHandler
- GetPlayerFromStateBagName, GetEntityFromStateBagName
- bag names: global, player:<id>, entity:<id>
### Entities (server objects, no game world)
- CreateVehicle, CreateVehicleServerSetter, CreatePed, CreateObject, CreateObjectNoOffset (B; client needs isNetwork=true)
- DeleteEntity (S)
- DoesEntityExist, GetEntityCoords, GetEntityHeading, GetEntityRotation, GetEntityQuaternion
- GetEntityModel, GetEntityType, GetEntityScript
- SetEntityCoords, SetEntityCoordsNoOffset, SetEntityHeading, SetEntityRotation, SetEntityQuaternion (B; client owner only)
- GetEntityVelocity (B), SetEntityVelocity (S)
- GetEntityHealth/MaxHealth (B), SetEntityHealth/MaxHealth (S)
- IsEntityVisible (B), SetEntityVisible (S)
- GetEntityAlpha (B), SetEntityAlpha (S), ResetEntityAlpha (S)
- IsEntityPositionFrozen (B), FreezeEntityPosition (S)
- AttachEntityToEntity (S), DetachEntity (S), GetEntityAttachedTo, IsEntityAttached
- GetAllVehicles, GetAllPeds, GetAllObjects (S)
- GetGamePool (C): CVehicle/CPed/CObject in scope
- NetworkGetNetworkIdFromEntity, NetworkGetEntityFromNetworkId, NetworkDoesNetworkIdExist (entity id == net id)
- NetworkGetEntityOwner (B), NetworkGetFirstEntityOwner (S), NetworkRequestControlOfEntity (C)
- SetEntityOwner (S, SixM ext), SetEntityOrphanMode (S: delete/migrate), SetEntityDistanceCullingRadius (S)
- Entity(id): entity state bag
- Client-created networked entities, leased id blocks, owner migration, distance scoping
### Routing buckets (S)
- SetPlayerRoutingBucket, GetPlayerRoutingBucket
- SetEntityRoutingBucket, GetEntityRoutingBucket
- GetPlayersInRoutingBucket (SixM ext)
- SetRoutingBucketPopulationEnabled, GetRoutingBucketPopulationEnabled (stored only)
- SetRoutingBucketEntityLockdownMode, GetRoutingBucketEntityLockdownMode (inactive/relaxed/strict)
### HTTP (S)
- PerformHttpRequest(url,cb,method,data,headers,options{timeout}): SSRF-guarded, DNS pinned
- SetHttpHandler(fn(req,res)): req.method/path/query/headers/setDataHandler; res.writeHead/write/send
### Data / misc
- json.encode, json.decode (Lua only; JS has native JSON)
- msgpack.pack, msgpack.unpack
- vector2/vec2, vector3/vec3, vector4/vec4, quat: swizzles, ops, #v length, shared (reference) semantics
- backtick hash literals `adder` rewritten to joaat
- GetHashKey, IsDuplicityVersion, print
- Citizen.InvokeNative (C, only with adapter natives)
- Sandbox: no io/package/require/dofile/loadfile; os only time/clock/date/difftime; debug only traceback/getinfo; 2M instruction budget; 64 MiB heap; no __gc

## 2. JS globals (sixm-js, QuickJS ES2023)
- setTick, clearTick
- setTimeout, clearTimeout, setInterval, clearInterval (+ SetTimeout/ClearTimeout/Citizen.* aliases)
- Delay, Wait, Citizen.Wait: promise-returning
- CreateThread, Citizen.CreateThread
- on, AddEventHandler, removeEventListener, RemoveEventHandler
- onNet, RegisterNetEvent, RegisterServerEvent
- emit, TriggerEvent; emitNet, TriggerClientEvent (S), TriggerServerEvent (C)
- TriggerLatentClientEvent (S), TriggerLatentServerEvent (C)
- CancelEvent, WasEventCanceled, globalThis.source
- exports(name, fn), exports[res].name Proxy
- GlobalState, Player(id).state, LocalPlayer.state, Entity(id).state (Proxy; .get/.set)
- AddStateBagChangeHandler, RemoveStateBagChangeHandler, GetPlayerFromStateBagName, GetEntityFromStateBagName
- vector2/Vector2, vector3/Vector3, vector4/Vector4, quat: .length/normalize/add/sub/mul/div/dot/cross
- msgpack.pack/unpack (Uint8Array)
- console (log/info/warn/error)
- getPlayers (JS-only alias of GetPlayers)
- Same native set as Lua for: players, entities, routing buckets, KVP, convars, resources, commands, HTTP, GetHashKey, IsDuplicityVersion, InvokeNative (C)
- Sandbox: no eval, no Function ctor, no require/import/fs/process/Buffer; 64 MiB heap, 256 KiB stack, 500 ms load, 100 ms callback deadlines
- No C# / Mono runtime

## 3. Natives exposed by name
See native_coverage.tsv for the CFX-list diff. Beyond the CFX list SixM also exposes:
TriggerEvent, TriggerServerEvent, TriggerClientEvent, TriggerLatent*Event, AddEventHandler, RegisterNetEvent, GetPlayers, SetEntityOwner, GetPlayersInRoutingBucket, IsPrincipalAceAllowed, SetEntityHealth, FreezeEntityPosition, AttachEntityToEntity, SetEntityAlpha, GetEntityVelocity etc. (entity state extras).

## 4. Events emitted
Server (local): onResourceStarting (cancellable), onResourceStart, onServerResourceStart, onResourceStop, onServerResourceStop
Server (players): playerConnecting(name,setKickReason,deferrals), playerJoining, playerDropped(reason)
Server (entities): entityCreating (cancellable, client-created), entityCreated, entityRemoved
Client: onClientResourceStart, onResourceStart, onClientResourceStop, onResourceStop
Not emitted by core (channels registered only by baseevents): baseevents:onPlayerEnteredVehicle, baseevents:onPlayerLeftVehicle, baseevents:onPlayerDied
Deferrals: deferrals.defer/update/done; sixm_deferralTimeout
Example-resource net events: sixm_session:ready, sixm_chat:message, hello:ping, hello:pong
State bag change handlers fire for replicated writes
Protocol-level: EntityCreateRefused, EntityTransforms, EntityStates

## 5. Manifest keys (crates/sixm-resources/src/manifest.rs)
- fx_version (stored), game / games (stored), lua54 (stored)
- server_script(s), client_script(s), shared_script(s)
- file / files
- dependency / dependencies
- provide / provides: virtual alias resolution
- export / exports (client), server_export / server_exports
- Any other directive: stored as metadata (author, description, version ...), chainable data_file 'T' 'f' => <name>_extra
- Globs (* **, ?, [], {}), @other/ and @alias/ references
- legacy __resource.lua accepted
- NOT read by host: ui_page, loadscreen, this_is_a_map, data_file effect, client/server_only, escrow, rdr3_warning
- Limits: 16 MiB mem, 10M instructions, 8192 entries/1 MiB; 256 client exports; 64 MiB per file

## 6. Console commands (server)
start, stop, restart, ensure, refresh, watch_resources
set, sets, setr
exec, help, quit
endpoint_add_udp, endpoint_add_tcp
players, entities, state, perf, profiler (record/stop/status/saveJSON)
add_ace, remove_ace, add_principal, remove_principal, list_aces, list_principals, test_ace
add_convar_permission, remove_convar_permission, list_convar_permissions
Script commands via RegisterCommand; convar print/set by bare name (console only)
Server CLI: +command args, --log-file, --log-json, --watch, --help, --version; server.cfg auto exec
Client console: help, quit, set, sets, setr; unknown lines forwarded to server (with player ACEs)
Client CLI: <addr> <name>, --fingerprint, --cache, --identity, --sim, +command, --version
Not present: status, clientkick, refresh-alias, cmdlist, sv_scriptHookAllowed, load_server_icon, svgui, txAdmin

## 7. Convars
Server: sixm_resourcesPath, sixm_identityFile, sixm_kvpPath, sixm_tickRate(20), endpoint_add_udp(0.0.0.0:30120)
sv_httpEnabled, sv_httpAllowPrivate, sixm_httpHandlerTimeout, sixm_httpTimeout, sixm_httpWorkers
sv_exposePlayerIdentifiers, rcon_password, sv_maxclients(32), sixm_deferralTimeout, sv_hostname
sixm_playerModel, sixm_cullRadius(424), sixm_maxClientEntities(64), sv_entityLockdown
sv_playerEventRateLimit(60), sv_metricsEnabled, sv_adminUiEnabled, sv_masterUrl, sv_heartbeatInterval
Client: sixm_tickRate(60)
Convar kinds: plain (set), server-info (sets, in /info.json), replicated (setr; sent to clients before scripts)
Console-only/secured convars: rcon_password, sv_httpAllowPrivate, sv_httpEnabled, sv_exposePlayerIdentifiers, sixm_identityFile, sixm_resourcesPath, sixm_kvpPath, endpoint_add_udp, sv_playerEventRateLimit, sv_metricsEnabled, sv_adminUiEnabled, sv_masterUrl, sv_heartbeatInterval
Limits: names 256 B, values 64 KiB (4096 replicated)
Private convar readers (add_convar_permission <res> read <name>)
Not present: sv_licenseKey, sv_scriptHookAllowed, sv_enforceGameBuild, onesync, sv_endpointPrivacy, sv_projectName, sv_projectDesc, sv_tebexSecret, steam_webApiKey, sv_lan, sv_authMaxVariance, locale, tags, sv_requestParanoia, rateLimiter_* , mysql_connection_string

## 8. Permission / ACE
- add_ace/remove_ace principal object allow|deny; add_principal/remove_principal inheritance
- Principals: identifier.<id>, group.<name>, player.<id>, resource.<name>, builtin.everyone
- Dotted-object prefix match, deny beats allow, case-insensitive, default deny
- command.<name> gates console commands for players and resources
- IsPlayerAceAllowed, IsPrincipalAceAllowed, test_ace, list_aces, list_principals
- resource.<name> principal for ExecuteCommand, StartResource/StopResource/RestartResource
- Convar read permissions per resource
- Not present: IsAceAllowed native (only IsPlayerAceAllowed, IsPrincipalAceAllowed)
- Rate limits: transport 100/s burst 200 (kick at >1000/s for 5 s); TriggerServerEvent token bucket
- HTTP rcon limiter: 5 failures => 429 for 30 s; HTTP 20 req/s per IP

## 9. System / example resources (examples/server-data/resources)
[system]/sessionmanager: playerJoining->ready handshake sixm_session:ready, state.sessionActive; exports isPlayerActive, getPlayerSession (S), isSessionReady (C)
[system]/baseevents: only registers baseevents:* net event channels (no emitter yet)
[examples]/chat: say command, sixm_chat:message to clients (console style)
[examples]/queue: playerConnecting deferrals demo, queue_banned convar, license check
[examples]/garage: spawncar/cars/clearcars commands, server entity creation
[examples]/hello, greeter: events, exports, dependency, files
Not bundled: spawnmanager, mapmanager, chat UI, hardcap, rconlog, playernames, runcode, monitor/txAdmin, mysql, oxmysql, webpack/yarn builders

## 10. HTTP endpoints / ops tooling
- /info.json, /players.json (identifiers opt-in), /dynamic.json, /healthz, /metrics (Prometheus, opt-in)
- /admin (built-in HTML admin UI; needs rcon_password)
- POST /rcon (JSON output; bearer / X-Rcon-Password; rate limited)
- /<resource>/... via SetHttpHandler
- Master heartbeat POST (sv_masterUrl, sv_heartbeatInterval)
- profiler record/stop/status/saveJSON (Chrome Trace / Perfetto), perf command
- watch_resources hot reload (500 ms poll; manifest + scripts), --watch
- --log-file, --log-json, RUST_LOG filter
- Transport: QUIC/TLS 1.3, mandatory client certs, server fingerprint pinning (--fingerprint), identity file (0600 / ACL)
- Resource file sync by SHA-256 to client cache, file streaming
- Limits: 128 HTTP conns, 1 MiB bodies, 16 MiB responses
- Load test: examples/loadtest + `swarm` example client
- Fuzz targets (fuzz/): decode_client_message, decode_server_message, file_stream_header, latent_event, manifest
- CI: ci.yml, fuzz.yml, release.yml (release zips in dist/); scripts/check_types.py, scripts/release_package.py
- IDE types: types/sixm.lua (LuaLS, manifest directives + natives), types/sixm.d.ts (TS)
- Docs: README, lua-api, js-api, manifest, server, client, protocol, guides (first-resource, from-fivem, permissions, deployment), design/game-adapter

## 11. Known missing (per docs/guides/from-fivem.md)
No NUI/DUI/scaleform, no game natives (weapons, tasks, vehicle physics, blips, audio), no C#, no server browser/Cfx.re list, no spawnmanager/mapmanager, no Steam/Discord/XBL identifiers, no PerformHttpRequest on client, no OneSync infinity population, no commerce/Tebex APIs, no Mumble voice.
