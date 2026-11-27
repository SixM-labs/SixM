# Running the headless client

The client binary is `sixm-client`. It has no window and no game. It connects
to a server, downloads the server's client files, and runs the client scripts
until it is told to stop. Source: `crates/sixm-client/src/`.

## Command line

```text
sixm-client <addr> <name> [--fingerprint HEX] [--cache DIR] [--identity FILE] [--sim] [+command args...]
```

| Argument | Meaning |
|---|---|
| `<addr>` | Server address, `ip:port`, for example `127.0.0.1:30120`. Required. |
| `<name>` | Player name. Required. It must be 1 to 64 characters after trimming. |
| `--fingerprint HEX` | The server's fingerprint, 64 hex characters. The client refuses any other certificate. |
| `--cache DIR` | Cache folder. Default `sixm-cache`, relative to the working directory. |
| `--identity FILE` | The client certificate. Default `identity.bin` in the cache folder. The file is created on first use. See [Identity](#identity). |
| `--sim` | Attach the built-in simulated [`GameAdapter`](design/game-adapter.md) (`SimAdapter`) for velocity integration, `TaskGoStraightToCoord`, and `InvokeNative` testing. |
| `+command args...` | A console command, run after the client connects. |
| `--version` | Prints the client version and exits. Use it on its own. |

- The two positional arguments and the options can come in any order before
  the first `+command`.
- After a `+command`, the words belong to that command. `--fingerprint` and
  `--cache` are still read as options there, so a command argument with one of
  those exact spellings is taken as an option.
- Logs go to standard output. `RUST_LOG` sets the filter, and defaults to
  `info`.

Example:

```text
sixm-client 127.0.0.1:30120 Tester --fingerprint <64 hex chars> +help
```

## Connecting

1. Open the connection. The client presents its certificate from `--identity`.
   The handshake and the resource list must finish within 10 seconds.
2. If a fingerprint was given, the server's certificate must match it. Without
   a fingerprint the connection is encrypted but the server is not checked, so
   a man in the middle can impersonate the server.
3. The client logs the connection, each synced resource with its files, and
   the download totals.
4. Every synced resource starts. Its files are fetched first, from the cache
   when they are already there.
5. The `+commands` run in order.
6. The client runs until Ctrl+C, the `quit` command, or the connection ends.

A kick or a disconnect stops every resource and the client exits. The client
does not reconnect.

If the server's certificate is wrong, the connection fails with an error and
the client exits.

## Console

Typed lines and `+commands` use the same rules as the server console: words,
quotes and comments. See [server.md](server.md).

### Built-in commands

| Command | Action |
|---|---|
| `help` | Lists the built-in commands and the script commands. |
| `quit [reason]` | Stops the client. |
| `set <name> <value>`, `sets`, `setr` | Stores a local convar. The value is the rest of the line, joined with single spaces. A name the server replicates is refused (see below). |

The client has no `exec`, `start`, `stop` or `refresh`. Those are server
commands.

Any other name runs a script command registered with `RegisterCommand`, or
prints or sets a convar. A name that is neither is sent to the server as the
whole line, and the server runs it with this player's permissions (see
[server.md](server.md#permissions)). The server logs what it refuses or does
not know, so the client does not report an unknown name itself.

The client keeps `help`, `quit`, `set`, `sets` and `setr` for itself, so the
server never sees those lines.

On the example server, type `say Hello` to send a message to the connected
clients. The `chat` resource displays the sender and message in the client log.

### Convars

| Convar | Default | Notes |
|---|---|---|
| `sixm_tickRate` | `60` | Frames per second, clamped to 1 to 1000. A value that is not a number gives 60. |

The server has the same convar name with a default of `20`. The two processes
keep separate values. The server default does not apply to the client.

Scripts read any convar with `GetConvar`, `GetConvarInt` and `GetConvarBool`.
`GetConvar` returns a replicated value first, then the local value, then the
default.

### Replicated convars

The server sends its replicated convars to the client when it joins, and again
whenever one changes or stops being replicated. The client keeps them apart
from its own convars. A convar the server stops replicating is removed here, so
it reads its local value again, or the default.

A local `set`, `sets` or `setr` of a name the server replicates is refused with
a warning, whether it is typed in the console or sent by `ExecuteCommand`. The
value stays the one the server sent.

The server sends the replicated convars, the entities and the state bags
before the resource list, and the client applies them before it starts any
script. A script sees them while it loads.

## Cache layout

The cache folder is `--cache`, default `sixm-cache`.

```text
sixm-cache/
  ab/
    ab12...ef            a downloaded file, named by its SHA-256 hash
  data/
    <resource>/          the data folder of each synced resource
    <resource>.kvp       the key-value store of each resource, see lua-api.md
  identity.bin           the client certificate and its key
```

- Each file is stored under its hash. The first two hex digits make the folder
  name, and the full 64 hex digits make the file name. The server's file names
  are never used on disk.
- A download is checked against the advertised size and hash before it is
  stored. A file that does not match is not stored.
- A stored file is written to a temporary file in the same folder and then
  renamed into place. A reader sees either no file or the whole file.
- A file already in the cache is not downloaded again.
- Each download waits at most 30 seconds.

## Identity

The client keeps a certificate and its private key in `identity.bin`, or in
the file that `--identity` names. The server derives the player's `license`
identifier from the certificate's fingerprint, so:

- The file is created on the first connection. Later connections read it, and
  the same player gets the same `license` every time.
- Deleting the file, or using another one, gives the player a new `license`.
- On Unix the file is created with mode `0600`. On Windows,
  before the key is written, its access list is replaced with one entry: full
  control for the file's owner, with nothing inherited from the folder. A file
  system without access lists keeps the folder's, with a warning in the log.
- A file that is not a SixM identity stops the connection with an error. Delete
  it to create a new one.
- The client logs its fingerprint at connect as `Client fingerprint`.

### Data folder

Each running resource has a data folder at `<cache>/data/<resource>`. It is
created when the resource starts.

The name must be one plain folder name. A name with a path separator or `..`
is refused, and the resource does not start.

Files that scripts save with `SaveResourceFile` are written here (up to 4 MiB per
write and 16 MiB total per resource), and so is
the key-value store of each resource (up to 16 MiB of live data), as `<resource>.kvp` next to the folders. Files that
the server sent are not in this folder. They are in the cache, by hash, and
`LoadResourceFile` reads them from there.

## Client scripts

When a resource starts on the client, the client loads these files in order:

1. The resource's `shared_scripts`
2. The resource's `client_scripts`

Server scripts are never sent to the client. `files` entries are downloaded but
not run. See [manifest.md](manifest.md).

A script can use the client side of the Lua API. The client can:

- Register and trigger events, including `TriggerServerEvent` and
  `RegisterNetEvent` handlers for events from the server
- Use `CreateThread`, `Wait` and timers
- Call exports of running resources and register its own
- Read the replicated `GlobalState` and `LocalPlayer.state`
- Read files with `LoadResourceFile`, and write its own data folder with
  `SaveResourceFile`
- Register console commands and read convars
- Use `json`

The client cannot:

- Call `TriggerClientEvent`, `GetPlayers` or `Player(id)`. These are server
  functions.
- Write replicated state. It can write state with `replicated` set to `false`,
  and that state stays on the client.
- Read a folder path for another resource. `GetResourcePath` returns the
  resource's own data folder, and `nil` for any other name.

`IsDuplicityVersion()` returns `false` on the client.

See [lua-api.md](lua-api.md) for each function.

## Fingerprints

`--fingerprint` is the only way to check the server. The client does not save
fingerprints between runs, so pass the value each time.
