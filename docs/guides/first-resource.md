# Your first resource

This guide walks through creating a SixM resource from scratch, running it on
`sixm-server`, and testing its shared, server, and client scripts—including
network events and state bags—with the headless `sixm-client`.

> [!NOTE]
> SixM does not have a game adapter yet, so `sixm-client` runs without a game
> window and prints script output to its terminal. See
> [From FiveM](from-fivem.md) for what works today and what is planned.

## 1. Prepare a server folder

Get `sixm-server` and `sixm-client` either from a release archive on the
[Releases page](https://github.com/SixM-labs/SixM/releases) or by building from
source:

```sh
cargo build --release -p sixm-server -p sixm-client
```

On Linux, the binaries are at `target/release/sixm-server` and
`target/release/sixm-client`. On Windows, they end in `.exe`.

Create a clean working directory for your server (or start from
`examples/server-data/`):

```text
my-server/
├── server.cfg
└── resources/
    └── notes/
        ├── fxmanifest.lua
        ├── shared/
        │   └── config.lua
        ├── server/
        │   └── main.lua
        └── client/
            └── main.lua
```

`sixm-server` looks for `server.cfg` and `resources/` relative to the current
working directory unless configured otherwise (`sixm_resourcesPath`).

## 2. Write `fxmanifest.lua`

Every resource is a folder containing an `fxmanifest.lua`. Its folder name
(`notes`) is the resource name. Create `resources/notes/fxmanifest.lua`:

```lua
fx_version 'cerulean'
game 'common'

shared_scripts {
    'shared/*.lua',
}

server_scripts {
    'server/*.lua',
}

client_scripts {
    'client/*.lua',
}
```

How SixM loads this manifest:

- On the **server**, `shared_scripts` run first in declaration order, followed
  by `server_scripts`.
- On the **client**, `shared_scripts` run first, followed by `client_scripts`.
  `server_scripts` are never sent to clients.
- Glob patterns like `shared/*.lua` match files inside that folder sorted by
  path. Use `**/*.lua` to match subdirectories as well.
- To organize many resources into groups, you can put resource folders inside
  category folders whose names use square brackets, such as
  `resources/[gameplay]/notes/`. Category folders do not change the resource
  name, and folders starting with `.` are skipped.

For every manifest directive and limit, see [manifest.md](../manifest.md).

## 3. Add a shared script

Shared scripts run in both the server Lua state and each client's Lua state.
Remember that server and client run in separate processes and Lua states, so
mutating a variable in `shared/config.lua` on the server does not mutate it on
clients.

Create `resources/notes/shared/config.lua`:

```lua
Config = {
    maxNoteLength = 120,
    channel = 'notes',
}

function FormatTag(label)
    local side = IsDuplicityVersion() and 'server' or 'client'
    return ('[%s:%s] %s'):format(Config.channel, side, label)
end
```

`IsDuplicityVersion()` returns `true` on the server and `false` on the client.

## 4. Write the server script

Create `resources/notes/server/main.lua`:

```lua
local resource = GetCurrentResourceName()

AddEventHandler('onResourceStart', function(name)
    if name ~= resource then
        return
    end
    GlobalState.serverMotd = 'Welcome to the SixM notes resource'
    GlobalState.noteCount = 0
    print(FormatTag('started'))
end)

AddEventHandler('playerJoining', function()
    local playerId = source
    Player(playerId).state.notesPosted = 0
    print(FormatTag(('player %s (%d) joined'):format(GetPlayerName(playerId), playerId)))
end)

AddEventHandler('playerDropped', function(reason)
    print(FormatTag(('player %d left (%s)'):format(source, reason)))
end)

RegisterNetEvent('notes:post', function(text)
    local sender = source
    if type(text) ~= 'string' then
        return
    end
    text = text:sub(1, Config.maxNoteLength)

    local count = (GlobalState.noteCount or 0) + 1
    GlobalState.noteCount = count

    local bag = Player(sender).state
    bag.notesPosted = (bag.notesPosted or 0) + 1
    bag.lastNote = text

    print(FormatTag(('#%d from %s (%d): %s'):format(
        count,
        GetPlayerName(sender),
        sender,
        text
    )))

    TriggerClientEvent('notes:broadcast', -1, GetPlayerName(sender), text, count)
end)

RegisterCommand('motd', function(source, args)
    if #args == 0 then
        print(FormatTag('current MOTD: ' .. tostring(GlobalState.serverMotd)))
        return
    end
    GlobalState.serverMotd = table.concat(args, ' ')
    print(FormatTag('MOTD updated to: ' .. GlobalState.serverMotd))
end, true)
```

Key points in this script:

- **Lifecycle events:** `onResourceStart` fires after the resource's scripts
  load; `playerJoining` and `playerDropped` fire with `source` set to the
  player's server ID (counting up from `1`).
- **State bags:** Assigning to `GlobalState.<key>` or `Player(id).state.<key>`
  replicates the value to every connected client (and to players who join
  later). To keep a key server-only, call `bag:set('secret', value, false)`.
- **Network events:** Events sent by clients are ignored unless the event name
  is registered with `RegisterNetEvent`. Inside the handler, `source` is the
  sending player's integer ID (and unlike older FiveM versions, SixM preserves
  `source` across `Wait`). `TriggerClientEvent(name, -1, ...)` broadcasts to
  all connected players.
- **Commands:** `RegisterCommand('motd', fn, true)` registers a restricted
  command. The server console can always run it, while players and resources
  need `command.motd` (see [Permissions](permissions.md)).

## 5. Write the client script

Create `resources/notes/client/main.lua`:

```lua
local resource = GetCurrentResourceName()

AddEventHandler('onClientResourceStart', function(name)
    if name ~= resource then
        return
    end
    print(FormatTag('client ready; MOTD is: ' .. tostring(GlobalState.serverMotd)))
    TriggerServerEvent('notes:post', 'Hello from ' .. GetPlayerServerId(PlayerId()))
end)

RegisterNetEvent('notes:broadcast', function(author, text, total)
    print(FormatTag(('%s wrote (#%d): %s'):format(author, total, text)))
end)

AddStateBagChangeHandler('serverMotd', 'global', function(_, _, value)
    print(FormatTag('MOTD changed to: ' .. tostring(value)))
end)

local myBag = ('player:%d'):format(GetPlayerServerId(PlayerId()))
AddStateBagChangeHandler('notesPosted', myBag, function(_, _, value)
    print(FormatTag('your total notes posted: ' .. tostring(value)))
end)

RegisterCommand('postnote', function(_, args)
    if #args == 0 then
        print(FormatTag('usage: postnote <message>'))
        return
    end
    TriggerServerEvent('notes:post', table.concat(args, ' '))
end, false)

RegisterCommand('status', function()
    local serverId = GetPlayerServerId(PlayerId())
    local ped = PlayerPedId()
    print(FormatTag(('serverId=%d ped=%d coords=%s posted=%s lastNote=%s'):format(
        serverId,
        ped,
        tostring(GetEntityCoords(ped)),
        tostring(LocalPlayer.state.notesPosted),
        tostring(LocalPlayer.state.lastNote)
    )))
end, false)
```

Things to watch on the client:

- **`PlayerId()` vs `GetPlayerServerId(PlayerId())`:** On the client,
  `PlayerId()` always returns `0`. Use `GetPlayerServerId(PlayerId())` to get
  your server ID for state bag names like `player:<id>`.
- **Join snapshot vs change handlers:** `GlobalState` and `LocalPlayer.state`
  already contain their replicated join snapshot before your client scripts
  load, so `AddStateBagChangeHandler` only fires for changes that arrive
  afterwards.
- **Read-only replicated state:** Clients can read `GlobalState` and
  `LocalPlayer.state`, or write local-only keys with `bag:set(k, v, false)`,
  but calling a replicated write from a client raises an error.

## 6. Create `server.cfg` and start the server

Create `my-server/server.cfg`:

```text
set sv_hostname "My First SixM Server"
endpoint_add_udp 0.0.0.0:30120
sixm_tickRate 20

ensure notes
```

Start `sixm-server` from inside `my-server/`:

```sh
cd my-server
/path/to/sixm-server
```

Wait for these log lines:

```text
Started resource notes
[notes:server] started
Listening on 0.0.0.0:30120
Server fingerprint: <64 lowercase hex characters>
```

Keep that 64-character hex fingerprint handy for connecting with certificate
pinning.

## 7. Connect with `sixm-client`

In a second terminal (use a separate working directory or `--cache` if you want
to keep client files separate from the server folder):

```sh
/path/to/sixm-client 127.0.0.1:30120 Alice --fingerprint <64 hex chars>
```

You should see:

1. `Client fingerprint: <hex>` and `connected to My First SixM Server as player 1`.
2. Resource sync logs listing `notes` (`shared/config.lua` and `client/main.lua`).
3. `[notes:client] client ready; MOTD is: Welcome to the SixM notes resource`.
4. `[notes:client] your total notes posted: 1` and
   `[notes:client] Alice wrote (#1): Hello from 1`.

Try the interactive consoles in both terminals:

- In the **client** terminal, type:
  - `postnote Testing state bags`
  - `status`
  - `quit` (or press `Ctrl+C`) to disconnect cleanly.
- In the **server** terminal, type:
  - `players` to list connected players and their `license:` identifiers.
  - `state` and `state global` or `state player:1` to inspect state bags.
  - `motd New message of the day` to update `GlobalState.serverMotd` and watch
    the client's `AddStateBagChangeHandler` fire.
  - `quit` to stop the server.

## 8. Live reload while developing

When you edit an existing script in `resources/notes/`:

1. Type `ensure notes` (or `restart notes`) in the server console.
2. If you added a new resource or changed `fxmanifest.lua`, run `refresh` first,
   then `ensure notes`.

Connected `sixm-client` instances automatically receive `ResourceStopped` and
`ResourceStarted`, download any changed files by SHA-256 hash, and reload the
resource's client scripts without reconnecting.

## Next steps

- Learn how access control works in [Permissions](permissions.md).
- See every available Lua function and event in [lua-api.md](../lua-api.md).
- Compare FiveM and SixM behaviour in [From FiveM](from-fivem.md).
- Review [server.md](../server.md) and [client.md](../client.md) for all
  console commands, convars, and CLI flags.
