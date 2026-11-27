# FiveM / Cfx.re ecosystem catalogue

Read-only research. Sources: docs.fivem.net (cookbook now served there, old cookbook.fivem.net redirects), github.com/citizenfx/cfx-server-data (master, resources/ tree via GitHub API), docs.fivem.net/docs/ index.

## Cookbook (https://docs.fivem.net/docs/cookbook/) (20 posts)
- Marking a vehicle as player vehicle (2022) — flag vehicle as player-owned for game code — tag: [game]
- Console key bind wait between commands (2021) — delays between commands in a key bind — tag: [core]
- Built-in ACL / security (2021) — server access control and permissions — tag: [core]
- Modified scenario files (2021) — load customized scenario definitions — tag: [game]
- Raw .ymap/.ytyp file support (2021) — load unpacked map/type metadata directly — tag: [game]
- Routing buckets: split game state (2020) — isolate game state between player groups — tag: [core]
- CitizenFX C# templates fixed (2020) — corrections to C# resource templates — tag: [core]
- OneSync server-side persistence (2020) — server-side entity persistence under OneSync — tag: [core]
- New CitizenFX C# templates (2020) — new C# project templates — tag: [core]
- Uploading in-game photos to Cfx.re Gallery (2020) — upload screenshots to Gallery — tag: [platform]
- New console key bindings (2020) — updated console key binding system — tag: [core]
- Caching proxy for resource downloads (2019) — speed up resource downloads via cache proxy — tag: [core]
- OneSync: intercepting game events (2019) — intercept explosions and similar under OneSync — tag: [game]
- Two experimental OneSync variables (2019, historical) — two experimental OneSync convars — tag: [core]
- Minimap top-left in screen coordinates (2019) — snippet converting minimap position — tag: [game]
- Improved script stack traces (2019) — better stack traces for script errors — tag: [core]
- GET_ACTIVE_PLAYERS (2019) — native replacing manual player loops — tag: [game]
- Adaptive Cards in deferrals (2019) — Adaptive Cards during connection deferrals — tag: [core]
- Compile-time Jenkins hashes in Lua (2019) — compile-time Jenkins hashing — tag: [core]
- Emoji in Scaleform GFx (2019) — emoji rendering in Scaleform — tag: [game]

## Docs index (https://docs.fivem.net/docs/)
- Documentation Home — entry points for server owners and asset developers — tag: [core]
- Native Reference — searchable catalogue of game natives — tag: [game]
- Getting Started — prerequisites, install, server setup, first scripts — tag: [core]
- Client Manual — client install, console commands, config, troubleshooting — tag: [core]
- Server Download — server software download — tag: [core]
- Guides for 3D Assets — nine-part beginner series on GTAV asset tooling — tag: [game]
- Stock Resources — bundled resources (chat, managers, session, baseevents) — tag: [core]
- txAdmin — server administration panel and permissions — tag: [platform]
- Products — Cfx.re tools incl. Alchemist, Catalyst, FxDK — tag: [platform]
- Developer Docs — sandbox, script runtimes, event security, Enhanced version — tag: [core]
- Support — client/server troubleshooting, resource FAQ, debugging — tag: [core]
- Contributing — contribution guidelines and Git guides — tag: [core]
- Cookbook Archive — older dated scripting and server articles — tag: [core]
- Game References: Blips [game]
- Game References: Checkpoints [game]
- Game References: Controls [game]
- Game References: Data Files [game]
- Game References: Game Events [game]
- Game References: Gamer Tags [game]
- Game References: HUD Colors [game]
- Game References: Input mapper / parameter IDs [game]
- Game References: Instructional Buttons [game]
- Game References: Markers [game]
- Game References: Net Game Events [game]
- Game References: Ped Models [game]
- Game References: Pickup Hashes [game]
- Game References: Profile Settings [game]
- Game References: Radio Stations [game]
- Game References: Speeches [game]
- Game References: Text Formatting [game]
- Game References: Vehicle References [game]
- Game References: Weapon Models [game]
- Game References: Zones [game]

## cfx-server-data resources (https://github.com/citizenfx/cfx-server-data/tree/master/resources)
Current master tree has only the groups below. Chat, sessionmanager, hardcap, rconlog, monitor, money-style system resources named in the brief are NOT present in this tree.

### [managers]
- spawnmanager — spawn handling and auto-spawn — tag: [core]
  - exports/events: spawnPlayer, addSpawnPoint, removeSpawnPoint, loadSpawns, setAutoSpawn, setAutoSpawnCallback, forceRespawn, getMapDirectives, playerSpawned
- mapmanager — loads and unloads map files — tag: [core]
  - functions: addMap, loadMap, unloadMap, parseMap; event: getMapDirectives (no named map types)

### [system]
- baseevents — death/vehicle base events — tag: [core]
  - events: onPlayerWasted, onPlayerKilled, onPlayerDied, enteringVehicle, enteringAborted, enteredVehicle, leftVehicle (prefixed baseevents:)
- runcode — runs code remotely from a web UI — tag: [core]
  - exports/events not extracted (files: runcode_sv/cl/shared/ui/web.lua, runcode.js)

### [gameplay]
- playernames — player name display (API, client, server files) — tag: [core]
- player-data — player data handling (server.lua) — tag: [core]
- [examples]/money, money-fountain, money-fountain-example-map, ped-money-drops — example scripts — tag: [game]
- chat-theme-example — chat theme example — tag: [core]

### [gamemodes]
- basic-gamemode — minimal gamemode — tag: [core]
- [maps]/fivem-map-hipster, fivem-map-skater, redm-map-one — map resources — tag: [game]

### [test] and [local]
- example-loadscreen — example loading screen — tag: [core]
- [local] — empty (.gitkeep), for custom resources — tag: [core]

Not extracted: exports for playernames, player-data, runcode, chat-theme-example; the brief's "builder" resources (yarn, webpack) were not found in the tree.

Total entries: 69 (33 docs index + 20 cookbook + 16 resources). Counts exclude cookbook from the 69 except noted: 33 + 20 + 16 = 69.
