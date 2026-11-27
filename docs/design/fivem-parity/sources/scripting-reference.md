# FiveM Scripting Reference catalogue

## Index (https://docs.fivem.net/docs/scripting-reference/)
- Tools for Server Owners: Convars, OneSync, State Bags (scripting-manual), Resource Manifest
- Develop Scripts: Create first script, Client Events, Server Events, Native Functions, Client Functions, Server Functions

## Resource manifest (https://docs.fivem.net/docs/scripting-reference/resource-manifest/)
- `fx_version` — sets FXv2 feature level; values cerulean (current, 2020-05), bodacious (2020-02), adamant (2019-12) — tag: [core]
- `game` — declares API sets; values common (CitizenFX only), gta5 (FiveM), rdr3 (RedM) — tag: [game]
- `resource_manifest_version` — deprecated GUID version selector; current 44febabe-d386-4d18-afbe-5e627f4af937 — tag: [core]
- `client_script` / `client_scripts` — loads client scripts, globbing; .lua, .net.dll, .js — tag: [core]
- `server_script` / `server_scripts` — loads server scripts, globbing; .lua, .net.dll, .js — tag: [core]
- `shared_script` / `shared_scripts` — loads scripts on client and server — tag: [core]
- `export` / `exports` — exports a global client function for Lua/JS consumers — tag: [core]
- `server_export` — exports a global server function — tag: [core]
- `ui_page` — sets NUI page to local file or absolute URL — tag: [core]
- `loadscreen` — sets HTML file as game loading screen — tag: [game]
- `loadscreen_manual_shutdown` — keeps loading screen open until shutdown native; value yes — tag: [game]
- `file` / `files` — adds files to packfile for client download — tag: [core]
- `dependency` — requires another resource to load first, accepts runtime constraints — tag: [core]
- `dependencies` — block form of dependencies plus constraints: server, policy, onesync, gameBuild, native — tag: [core]
- `escrow_ignore` — files excluded when using Asset Escrow — tag: [core]
- `lua54` — deprecated; Lua 5.4 is now default — tag: [core]
- `node_version` — NodeJS server runtime version; values 16 (default), 22 — tag: [core]
- `provide` — marks resource as replacement for a named resource — tag: [core]
- `use_experimental_fxv2_oal` — experimental One Argument List for Lua; value yes — tag: [core]
- `clr_disable_task_scheduler` — disables custom C# task scheduler; default on for bodacious+; value yes — tag: [core]
- `convar_category` — adds convars to FxDK Project Settings; types CV_STRING, CV_BOOL, CV_INT, CV_SLIDER, CV_COMBI, CV_PASSWORD, CV_MULTI; $ replicated, # server info — tag: [core]
- `data_file` — registers data file of a type with extra content system; globbing — tag: [game]
- `this_is_a_map` — marks resource as GTA map, reloads map storage — tag: [game]
- `replace_level_meta` — replaces default level meta with resource file — tag: [game]
- `before_level_meta` — deprecated; loads level meta before primary — tag: [game]
- `after_level_meta` — deprecated; loads level meta after primary — tag: [game]
- `server_only` — prevents clients from downloading resource files — tag: [core]
- Custom metadata keys — any other key accepted as arbitrary metadata — tag: [core]

## Client events (https://docs.fivem.net/docs/scripting-reference/events/client-events/)
- `CEventName` — internal game network events; params entities, eventEntity, data — tag: [game]
- `entityDamaged` — local entity damage; params victim, culprit, weapon, baseDamage — tag: [game]
- `gameEventTriggered` — game internal network event; params name, data — tag: [game]
- `mumbleConnected` — (re)connected to Mumble; params address, reconnecting — tag: [core]
- `mumbleDisconnected` — disconnected from Mumble; param address — tag: [core]
- `onClientResourceStart` — queued after resource start; param resource — tag: [core]
- `onClientResourceStop` — after resource stop; param resource — tag: [core]
- `onResourceStart` — immediately on resource start; param resource — tag: [core]
- `onResourceStarting` — as resource starts, cancelable; param resource — tag: [core]
- `onResourceStop` — immediately when stopping; param resource — tag: [core]
- `populationPedCreating` — population ped creation, cancelable; params x, y, z, model, overrideCalls — tag: [game]
- spawnmanager `playerSpawned` — resource event; params undocumented — tag: [game]
- mapmanager `onClientMapStart`, `onClientGameTypeStart`, `onClientMapStop`, `onClientGameTypeStop`, `getMapDirectives` — resource events — tag: [game]
- baseevents `onPlayerDied`, `onPlayerKilled` — resource events — tag: [game]
- sessionmanager `playerActivated`, `sessionInitialized` — resource events — tag: [core]
- chat `chatMessage`, `chat:addMessage`, `chat:addTemplate`, `chat:addSuggestion`, `chat:removeSuggestion`, `chat:clear` — chat resource events — tag: [core]

## Server events (https://docs.fivem.net/docs/scripting-reference/events/server-events/)
- `entityCreated` — after entity created; param handle — tag: [game]
- `entityCreating` — as entity created, cancelable to delete; param handle — tag: [game]
- `entityRemoved` — entity removed on server; param entity — tag: [game]
- `onResourceListRefresh` — refresh command completed; no params — tag: [core]
- `onResourceStart` — immediately after resource start; param resource — tag: [core]
- `onResourceStarting` — resource starting, cancelable; param resource — tag: [core]
- `onResourceStop` — resource begins stopping; param resource — tag: [core]
- `onServerResourceStart` — queued after resource start; param resource — tag: [core]
- `onServerResourceStop` — after resource stop; param resource — tag: [core]
- `playerConnecting` — connection attempt, cancelable; params playerName, setKickReason, deferrals, source — tag: [core]
- `playerEnteredScope` — player enters another's scope; param data (for, player) — tag: [game]
- `playerLeftScope` — player leaves another's scope; param data (for, player) — tag: [game]
- `playerJoining` — connecting player gets final NetID; params source, oldID — tag: [core]
- `ptFxEvent` — particle effect created; params sender, data — tag: [game]
- `removeAllWeaponsEvent` — weapons removed from another ped; params sender, data (pedId) — tag: [game]
- `startProjectileEvent` — projectile created; params sender, data — tag: [game]
- `weaponDamageEvent` — client damages remote entity, cancelable; params sender, data — tag: [game]
- `respawnPlayerPedEvent` — player respawns; params player, content (posX/Y/Z); requires OneSync — tag: [game]
- `vehicleComponentControlEvent` — ped takes vehicle component control; params sender, data (vehicleGlobalId, pedGlobalId, componentIndex, request, componentIsSeat, pedInSeat); OneSync — tag: [game]

## Events list (https://docs.fivem.net/docs/scripting-reference/events/list/)
- `playerConnecting` — params playerName, setKickReason, deferrals, source; deferrals methods defer, update, presentCard, done, handover — tag: [core]
- `playerDropped` — server, disconnect; params source, reason, resourceName, clientDropReason — tag: [core]
- `gameEventTriggered` — params name, args[]; example type CEventNetworkEntityDamage — tag: [game]
- `populationPedCreating` — cancelable; params posX/posY/posZ, model, setters (setModel, setPosition) — tag: [game]
- `rconCommand` — deprecated; params command, arguments; use REGISTER_COMMAND restricted — tag: [core]
- `onEntityBucketChange` — server; params entity, bucket, oldBucket — tag: [core]
- `onPlayerBucketChange` — listed in nav only, not documented — tag: [core]

## Convars (https://docs.fivem.net/docs/scripting-reference/convars/)
- Page documents no built-in convars; describes standard, server-replicated, server-info convar types — tag: [core]
- `Discord` — example server info convar holding invite link (example only) — tag: [core]

## OneSync (https://docs.fivem.net/docs/scripting-reference/onesync/)
- OneSync: Infinity / big mode / plain OneSync / non-big mode (removed) — tag: [game]
- Player ped/vehicle culling, player culling, culling radius (424 default) — tag: [game]
- Object ID extension (65535), head blend bug workaround, player 31 reserved sync player — tag: [game]
- CSectorPositionDataNode — tag: [game]
- Routing buckets — tag: [core]
- Lockdown modes: full, strict, relaxed, inactive — tag: [core]
- KeepEntity orphan mode flag — tag: [game]
- State bags (linked, scripting-manual) — tag: [core]

## Runtimes (https://docs.fivem.net/docs/scripting-reference/runtimes/)
- Lua: client-functions, server-functions pages — tag: [core]
- JavaScript: client-functions, server-functions pages — tag: [core]
- C#: client-functions, server-functions pages — tag: [core]

## Lua client functions (https://docs.fivem.net/docs/scripting-reference/runtimes/lua/client-functions/)
- AddEventHandler, Citizen.Await, Citizen.CreateThread, Citizen.SetTimeout, Citizen.Trace, Citizen.Wait — tag: [core]
- cross, dot, inv, norm, quat, slerp, vector, vector2, vector3, vector4 — math helpers — tag: [core]
- RegisterNetEvent, RemoveEventHandler, TriggerEvent, TriggerServerEvent — tag: [core]
- RegisterNUICallback, SendNUIMessage — NUI bridge — tag: [core]

## Lua server functions (https://docs.fivem.net/docs/scripting-reference/runtimes/lua/server-functions/)
- AddEventHandler, Citizen.Await, Citizen.CreateThread, Citizen.SetTimeout, Citizen.Trace, Citizen.Wait — tag: [core]
- cross, dot, inv, norm, quat, slerp, vector, vector2, vector3, vector4 — math helpers — tag: [core]
- GetPlayerIdentifiers, GetPlayers — tag: [core]
- PerformHttpRequest, PerformHttpRequestAwait — tag: [core]
- RegisterNetEvent, RemoveEventHandler, TriggerClientEvent, TriggerEvent — tag: [core]

## JavaScript server functions (https://docs.fivem.net/docs/scripting-reference/runtimes/javascript/server-functions/)
- exports, emit, emitNet, onNet, on, setTick, clearTick, getPlayers — tag: [core]

## C# server functions (https://docs.fivem.net/docs/scripting-reference/runtimes/csharp/server-functions/)
- None documented; points to native reference and CitizenFX.Core.Native.API — tag: [core]

## Client / server functions indexes (https://docs.fivem.net/docs/scripting-reference/client-functions/, .../server-functions/)
- Index pages only: native functions, runtime-specific functions, per-language links — tag: [core]
