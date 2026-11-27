# SixM

**SixM is a multiplayer modification framework written in Rust.** It provides
a dedicated server, a resource system and a scripting runtime in the style of
FiveM, so that developers who know FiveM can start building right away.

The core does not depend on any particular game. Game support is planned as
separate adapters, starting with GTA VI once a PC version is released.

> [!WARNING]
> SixM is in early development. The server, scripting and networking work, but
> there is no game client yet. See [Status](#status).

## Goals

- **Familiar for FiveM developers.** Resources use `fxmanifest.lua`, and the
  Lua API keeps the names you know: `CreateThread`, `Wait`, `AddEventHandler`,
  `RegisterNetEvent`, `TriggerEvent`, `exports`, `RegisterCommand`, `json`,
  `vector2`, `vector3`, `vector4` and `GetHashKey`.
  Existing resources should need as few changes as possible.
- **Fast.** The server, scheduler and resource handling are written in Rust.
  Each resource runs in its own isolated Lua 5.4 state.
- **Safe by default.** Scripts run in a sandbox without `io`, without
  bytecode loading and without access to files outside their resource.
  Manifests cannot hang the server.
- **Game-agnostic.** Everything game-specific sits behind an adapter
  interface. The core, resources and scripts do not need to change when a new
  game is added.

## Status

| Area | State |
|---|---|
| Core contract (`sixm-core`) | ✅ done |
| Resource discovery, `fxmanifest.lua`, dependencies (`sixm-resources`) | ✅ done |
| Lua runtime with FiveM-style API (`sixm-lua`) | ✅ done |
| Server binary: `server.cfg`, console, resource manager (`sixm-server`) | ✅ done |
| Networking: QUIC transport and protocol (`sixm-net`) | ✅ done |
| Players, net events and client events (`sixm-server`, `sixm-lua`) | ✅ done |
| State bags (`sixm-server`, `sixm-lua`, `sixm-net`) | ✅ done |
| Entity sync and player peds | ✅ done (server-owned entities and client-moved peds, no game yet) |
| Headless client: resource sync, client scripts (`sixm-client`) | ✅ done |
| GTA VI game adapter | ⏳ planned (needs a PC release) |

## Layout

```
crates/
├── sixm-core/        Shared types: script values, the Host and ScriptRuntime traits
├── sixm-resources/   Resource discovery, fxmanifest.lua parsing, dependency order
├── sixm-lua/         Lua 5.4 runtime: scheduler, events, exports, sandbox
└── sixm-server/      Server binary: server.cfg, console, resource manager, main loop
examples/server-data/ A runnable example server with two resources
```

Like FiveM, the split between game-independent and game-specific code is the
central idea. Unlike FiveM, a new game is added by implementing a single Rust
trait instead of a parallel set of components.

## Building

SixM supports **Windows** and **Linux** (x86_64). CI builds and tests every
change on both.

Requirements:

- Rust (stable, edition 2024)
- A C compiler for the bundled Lua 5.4: on Windows, the MSVC toolchain
  (Visual Studio Build Tools); on Linux, `gcc` or `clang`

```sh
cargo build --release
cargo test
```

Run the example server:

```sh
cd examples/server-data
cargo run --release -p sixm-server
```

The console accepts `start`, `stop`, `restart`, `ensure`, `refresh`, `set`,
`exec`, `endpoint_add_udp`, `players`, `entities`, `state`, `perf`, `help` and
`quit`, plus every command registered with `RegisterCommand`. `perf` prints the
frame times and the network load; see [Load testing](docs/server.md#load-testing).

A test client can connect while the example server runs:

```sh
cargo run -p sixm-net --example bot -- 127.0.0.1:30120 Tester hello:ping hi
```

## Writing resources

A resource is a folder containing an `fxmanifest.lua`, the same as in FiveM:

```lua
fx_version 'cerulean'
game 'gta6'

server_scripts {
    'server/*.lua',
}

dependency 'other_resource'
```

```lua
-- server/main.lua
RegisterNetEvent('myresource:hello', function(name)
    print(('hello from %s (player %s)'):format(name, source))
end)

CreateThread(function()
    while true do
        Wait(1000)
        TriggerEvent('myresource:tick', GetGameTimer())
    end
end)
```

Differences from FiveM:

- `source` stays correct inside a handler even after a `Wait`.
- `io` is not available. Use `LoadResourceFile` and `SaveResourceFile`
  instead.
- `load` only accepts source text, not precompiled bytecode.
- Vectors are userdata, so `local b = a; b.x = 1` also changes `a`. In FiveM,
  vectors are values and `a` would keep its old `x`.
- On the client, `LoadResourceFile` reads the files a running resource declares
  (`files`, `client_scripts` and `shared_scripts`). `SaveResourceFile` writes to
  that resource's local data folder, but never over a declared file.

Full reference: [docs/](docs/README.md).

## Players and network

The server listens on `endpoint_add_udp` (default `0.0.0.0:30120`) over QUIC.
`sv_maxclients` (default 32, between 1 and 1024) caps the players, and
`sv_hostname` is the name shown when they connect. SixM speaks UDP only, so
`endpoint_add_tcp` is accepted as an alias.

Each player gets a server id, counting up from 1 and never reused while the
server runs. Players can send console commands that the server runs with their
permissions, and scripts can queue console lines that run with the permissions
of their resource; see [Permissions](docs/server.md#permissions). Scripts see the
connection lifecycle as events:

- `playerConnecting(name, setKickReason, deferrals)` runs before a player is admitted.
  Call `setKickReason(text)` or `CancelEvent()` to refuse the connection. The
  function is valid only during this event. A handler can also hold the
  connection with `deferrals`, to show progress and decide later; see
  [Deferrals](docs/lua-api.md#deferrals).
- `playerJoining` runs once the player is admitted.
- `playerDropped(reason)` runs when the player leaves, whether they
  disconnected or were kicked.

In all three, `source` is the player's id, as in FiveM.

Connections are encrypted. The server presents a self-signed certificate
stored in `sixm_identityFile` (default `cache/server-identity.bin`, created on
the first start), so its fingerprint stays the same across restarts. The
server logs that SHA-256 fingerprint at startup as `Server fingerprint`. A
client that knows it can pin it and refuse any other server. Unpinned
connections are encrypted but not authenticated, so a man in the middle can
impersonate the server. An empty `sixm_identityFile` makes a new certificate on
every start instead.

Clients present a certificate too, and the server requires one. The
`license` identifier of a player is derived from that certificate, which the
client keeps in `identity.bin` in its cache folder (`--identity` chooses another
file). The client proves that it holds the key during the handshake, so the
same file gives the same `license` on every connection. Losing `identity.bin`
gives the player a new `license`. The `ip` identifier is the address the player
connects from, without the port. Scripts read the identifiers with
`GetPlayerIdentifiers` and `GetPlayerIdentifierByType(player, 'license')`, and
they are known from the start of `playerConnecting`.

## State bags

A state bag is a table of keys that the server keeps, and that clients can
receive. There are two kinds of bag:

- `GlobalState` is the bag shared by the whole server.
- `Player(id).state` is the bag of a connected player. It is created just
  before `playerJoining` and removed after `playerDropped`. Writing to the bag
  of a player who is not connected is an error.

```lua
GlobalState.playerCount = #GetPlayers()        -- replicated to clients
Player(source).state.joinedAt = GetGameTimer() -- replicated to clients
GlobalState:set('debug', true, false)          -- kept on the server only
GlobalState.playerCount = nil                  -- deletes the key

AddStateBagChangeHandler('playerCount', nil, function(bagName, key, value, reserved, replicated)
    print(bagName, key, value, replicated)
end)
local cookie = AddStateBagChangeHandler(nil, nil, print)
RemoveStateBagChangeHandler(cookie)

GetPlayerFromStateBagName('player:3') -- 3, or 0 for any other bag name
```

- Plain assignment replicates. `bag:set(key, value, replicated)` chooses, and
  replicates when `replicated` is omitted.
- A replicated write is sent to every connected player. A new player gets every
  replicated key of every bag when it joins. A player bag is announced as
  removed when its player leaves.
- A non-replicated write stays on the server. Clients keep the last replicated
  value they were sent, so a non-replicated write does not change it.
- Every write runs the change handlers on the server, replicated or not. A
  handler gets `(bagName, key, value, reserved, replicated)`. `reserved` is
  always `0`, as in FiveM. A filter of `nil` matches every key or bag.
- Keys are 1 to 256 bytes. Values are plain data: functions cannot be stored.
- `set` is the method name of a bag. A key called `set` can be written, but
  reading it returns the method, so choose another name.
- Clients cannot write state. The server accepts no state messages from
  clients, so only server scripts change it.
- The console command `state` lists the bags. `state <bag>` prints the keys of
  one bag, with their values.

Events sent by players reach only handlers registered with `RegisterNetEvent`.
Use `TriggerClientEvent(name, id, ...)` to send to one player, or `-1` for
everyone. For large payloads, `TriggerLatentClientEvent` and
`TriggerLatentServerEvent` send at a chosen byte rate while ordinary events
continue. Server scripts can read the QUIC round-trip time in milliseconds
with `GetPlayerPing(id)`. Also available: `GetPlayers`, `GetPlayerName`, `GetPlayerEndpoint`
and `DropPlayer`. The console command `players` lists the connected players.

## Entities

The server owns every entity: a vehicle, a ped or an object, with a model, a
position and a rotation. Clients keep a copy of each one, so the entity
natives read the same values on both sides.

- Creating and deleting an entity reaches the clients at once. Each entity
  has a state bag, `entity:<id>`, which replicates like the others.
- A move is queued and sent once per server frame, with the latest position
  and rotation, however often the script moved the entity in that frame.
- Scoping: by default a player is sent only the entities within 424 metres of
  its ped, and the entities it owns. An entity that comes within range is
  created for the player, and one that goes out of range is deleted for it. Set
  `sixm_cullRadius` to change the distance, or to `0` to send every entity to
  every player. See [Scoping](docs/lua-api.md#scoping).
- A player who joins receives every entity in its scope.
- Each admitted player has a ped, owned by that player. The owner's client moves
  it, with `SetEntityCoords` and the other setters, and the server passes the
  move on to everyone else. Nobody else can move it. A server move, such as
  `SetEntityCoords(GetPlayerPed(source), ...)`, moves it for everyone,
  including the owner. Server scripts see the owner with `NetworkGetEntityOwner`.
  See [Player peds and ownership](docs/lua-api.md#player-peds-and-ownership).

```lua
local vehicle = CreateVehicle('adder', 0.0, 0.0, 72.0, 90.0, true, false)
Entity(vehicle).state.plate = 'SIXM'
SetEntityCoords(vehicle, 10.0, 0.0, 72.0)
```

The server raises `entityCreated` and `entityRemoved`, each with the entity id.
The console command `entities` lists every entity.

Natives, on both sides: `DoesEntityExist`, `GetEntityCoords`,
`GetEntityHeading`, `GetEntityRotation`, `GetEntityModel`, `GetEntityType`,
`NetworkGetNetworkIdFromEntity`, `NetworkGetEntityFromNetworkId`,
`NetworkDoesNetworkIdExist`, `GetEntityFromStateBagName`, `Entity`,
`SetEntityCoords`, `SetEntityCoordsNoOffset`, `SetEntityHeading` and
`SetEntityRotation`. On a client the setters move only the entities it owns.

On the server: `CreateVehicle`, `CreateVehicleServerSetter`, `CreatePed`,
`CreateObject`, `CreateObjectNoOffset`, `DeleteEntity`, `GetPlayerPed`,
`NetworkGetEntityOwner`, `GetAllVehicles`, `GetAllPeds` and `GetAllObjects`.

On the client: `GetGamePool`, with `'CVehicle'`, `'CPed'` or `'CObject'`,
`PlayerPedId` and `GetPlayerPed`.

## Contributing

Contributions are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md) first: SixM
is a clean-room implementation, so code from FiveM must never be copied.
Report security problems privately as described in [SECURITY.md](SECURITY.md).

## License

SixM is licensed under the [GNU General Public License v3.0](LICENSE)
(`GPL-3.0-only`). Modified versions that are distributed must also be released
under the GPL with their source code.

## Legal

SixM is not affiliated with Rockstar Games, Take-Two Interactive or Cfx.re.
"Grand Theft Auto", "GTA" and "FiveM" are trademarks of their respective
owners. This repository contains no game code or game files.
