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
  `RegisterNetEvent`, `TriggerEvent`, `exports`, `RegisterCommand`, `json`.
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
| Entity sync | ⏳ planned |
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
`exec`, `endpoint_add_udp`, `players`, `state`, `help` and `quit`, plus every
command registered with `RegisterCommand`.

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
server runs. Scripts see the connection lifecycle as events:

- `playerConnecting(name, setKickReason)` runs before a player is admitted.
  Call `setKickReason(text)` or `CancelEvent()` to refuse the connection. The
  function is valid only during this event.
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
everyone. Also available: `GetPlayers`, `GetPlayerName`, `GetPlayerEndpoint`
and `DropPlayer`. The console command `players` lists the connected players.

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
