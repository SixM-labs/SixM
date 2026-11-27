# Running the server

The server binary is `sixm-server`. Source: `crates/sixm-server/src/`.

## Command line

```text
sixm-server [+command [args...]]...
sixm-server --help
sixm-server --version
```

- Each `+name` starts a console command. The words after it, up to the next
  `+name`, are its arguments. Words are kept exactly as the shell passed them,
  so `+set motd "a # b"` sets `a # b`.
- Commands run in the order given, before the network starts.
- Arguments that come before the first `+command` are an error.
- A bare `+` is an error.
- `--help` and `--version` win over everything else.
- If no `+command` is given and `server.cfg` exists in the working directory,
  the server runs `exec server.cfg`. If you give any `+command`, `server.cfg` is
  not run unless you name it, for example `+exec server.cfg`.
- Ctrl+C asks the server to quit.

The log goes to standard output. `RUST_LOG` sets the filter, and defaults to
`info`.

## Startup sequence

1. Parse the command line and set up logging.
2. Run the `+commands`, or `server.cfg`, in order. Resources start here if the
   config asks for them. Players cannot connect yet.
3. Start the network on `endpoint_add_udp`: the HTTP listener on TCP first, then
   the game transport on UDP, both on that address. If it fails, the error is
   logged and the server runs without networking.
4. Log `Listening on <address>` and `Server fingerprint: <hex>`.
5. Run the frame loop until a quit is requested.
6. Stop resources in reverse start order, then stop the network. Connected
   players are disconnected with the reason `server shutting down`.

The console reads standard input. If standard input closes, for example when
the server runs without a terminal, the server keeps running.

## server.cfg

`server.cfg` is plain text. Each line is one console command. The rules are
the same for `exec`, `+commands` and stdin:

- Words are separated by whitespace.
- A double-quoted string is one word, and may contain spaces. `\"` inside
  quotes is a literal quote.
- `#` or `//` outside quotes starts a comment to the end of the line.
- An unclosed quote is an error for that line. The rest of the file still runs.

`exec <file>` runs a file. The path is relative to the working directory.
Files can include other files. Nesting stops at 16 levels.

Example from `examples/server-data/server.cfg`:

```text
# Example server. Run from this folder: cargo run -p sixm-server
set sv_hostname "SixM Example Server"
sixm_tickRate 20

ensure greeter
ensure hello
```

## Console commands

### Built-in commands

Built-in commands are matched first. Any script command or convar with the
same name is never reached.

| Command | Action |
|---|---|
| `start <resource>` | Starts the resource after its dependencies that are not running. A resource that is already running is left alone. |
| `stop <resource>` | Stops the running resources that depend on it, then the resource. A resource that is not running is a warning. |
| `restart <resource>` | Stops the resource and its running dependents, starts the resource, then starts the dependents in dependency order. If the resource is not running, it is started. |
| `ensure <resource>` | Restarts the resource if it is running, otherwise starts it. |
| `refresh` | Scans `sixm_resourcesPath` again. Running resources are not affected. `start` scans on its own if no scan has run yet. |
| `endpoint_add_udp <ip:port>` | Stores the listen address in `endpoint_add_udp`. It applies at the next start. The address must parse. |
| `endpoint_add_tcp <ip:port>` | The same as `endpoint_add_udp`: TCP (HTTP) and UDP (the game) share one address, so this sets both. |
| `perf` | Prints the frame times of the last 1200 frames: count, average, p50, p99 and max, in milliseconds. Then the tick rate and the budget per frame, the players, entities and running resources, and the network rate in KiB/s and frames/s since the previous `perf` call, or since the server started. See [Load testing](#load-testing). |
| `profiler <record [frames] \| stop \| status \| saveJSON <path>>` | Captures `ServerTick` and per-resource tick durations. See [Frame profiling](#frame-profiling). |
| `players` | Lists connected players with their id, name, `license` identifier and endpoint. |
| `entities` | Lists every entity with its id, kind, model hash, position and heading. |
| `state [bag]` | Without a name, lists the state bags. With a name, prints the bag's keys and values. Replicated keys are marked. |
| `set <name> <value>` | Stores a plain convar. The value is the rest of the line, joined with single spaces. See [Convars](#convars). |
| `sets <name> <value>` | Stores a convar published in `/info.json`. The value is read the same way. |
| `setr <name> <value>` | Stores a replicated convar. The value is read the same way. Every connected player gets it, and players who join later get it too. |
| `add_convar_permission <resource> read <name>` | Grants read access to a plain server convar. See [Private convar readers](#private-convar-readers). |
| `remove_convar_permission <resource> read <name>` | Revokes `<resource>`'s read access to a restricted convar. When the last reader is removed, the convar becomes readable by every resource again. |
| `list_convar_permissions` | Lists every private convar reader entry (`console` only). |
| `exec <file>` | Runs each line of the file. See above. |
| `help` | Lists the built-in commands and the script commands. Restricted commands are marked. |
| `quit [reason]` | Asks the server to stop. The reason is written to the log. |
| `add_ace <principal> <object> <allow\|deny>` | Sets the entry of a principal for an object. An earlier entry for the same pair is replaced. See [Permissions](#permissions). |
| `remove_ace <principal> <object> <allow\|deny>` | Removes that entry, if it has that permission. |
| `add_principal <child> <parent>` | Makes `child` inherit from `parent`. |
| `remove_principal <child> <parent>` | Removes that inheritance link. |
| `list_aces` | Lists every entry as `ace <principal> <object> <allow\|deny>`. |
| `list_principals` | Lists every link as `principal <child> <parent>`. |
| `test_ace <principal> <object>` | Prints whether the principal is allowed or denied the object. |

A failed resource command is logged. Expected refusals, such as a resource
canceled by `onResourceStarting`, are warnings.

### Other names

For any other name, the server checks in this order:

1. A script command registered with `RegisterCommand`. It receives the
   player id (`0` for the console), the arguments and the raw line.
2. An existing convar. Given no value, it prints the convar. Given a value, it
   sets it.
3. Otherwise the server logs `No such command`.

A console command is run by the console, a player's command by the player,
and a line that a script queues with `ExecuteCommand` by that resource (see
below). A script command that is not restricted runs for every source.

## Permissions

Players can send console lines from their client (see
[client.md](client.md)). The server runs such a line with the player's
permissions, which come from access control entries (ACEs). The console runs
every command. Scripts can queue console lines with `ExecuteCommand`, and the
server runs those with the permissions of the calling resource (see
[Resources](#resources)).

- A player may run a built-in command only if its principal, `player.<id>`, is
  allowed `command.<name>`. For example, `quit` needs `command.quit`.
- A player may run a script command registered with `restricted` only if it is
  allowed `command.<name>`. A script command that is not restricted runs for
  every player.
- A player can never print or set a convar by its bare name.
- A refused line is logged at `info` with the player id and the command name.
  An unknown name from a player is logged at `debug` only.
- `ExecuteCommand` does not run its line with console rights. A script that
  passes a player's request to `ExecuteCommand` should first check the player
  with `IsPlayerAceAllowed`. The line then runs with the resource's rights,
  not the player's.

A principal is a name such as `identifier.license:<hex>`, `group.admin`,
`player.3` or `resource.<name>`. `add_principal` makes one principal inherit
from another, so a player can be placed in a group. Every principal inherits
from `builtin.everyone`. A connected player, `player.<id>`, also inherits from
`identifier.<identifier>` for each of its identifiers, so a license can be
named directly.

### Resources

A resource is the principal `resource.<name>`. A line that its script queues
with `ExecuteCommand` runs with that principal's rights. The rules are the same
as for a player:

- A built-in command, such as `stop`, `set` or `exec`, needs
  `command.<name>` for `resource.<name>`.
- A script command registered with `restricted` needs `command.<name>`. An
  unrestricted script command runs for every resource.
- A resource can never print or set a convar by its bare name.
- A refused line is logged at `warn`, with the resource and the command name.
  Resources are trusted code, so a refusal is worth seeing.
- A line that `exec` runs from such a command keeps the resource's rights.
- A script command that such a line runs gets `0` as its source, as it does for
  the console.

This is a deliberate change. Earlier versions ran every `ExecuteCommand` line
with console rights, so any resource could do anything. A resource that stops,
starts, sets or execs now needs an entry for that command, as a player does.
The entries below let a resource called `admin` start and stop resources, and
nothing else:

```text
add_ace resource.admin command.start allow
add_ace resource.admin command.stop allow
```

With these entries, `ExecuteCommand('stop other')` in `admin` works, while
`ExecuteCommand('quit')` is refused. `ExecuteCommand('sv_hostname x')` is
refused whatever the grants, because a resource never sets a convar by its name.

An object is a dotted path, such as `command.stop`. An entry on `command`
also covers `command.stop`. The check works like this:

1. Collect the principal's ancestors: the principal itself, each principal it
   inherits from, directly or through other principals, and `builtin.everyone`.
   Each name is compared case-insensitively.
2. Check the object at its full path, then at each shorter dotted prefix, from
   the longest to the shortest (`command.stop`, then `command`).
3. At the first path that any ancestor has an entry for, a deny means no, and
   an allow means yes. A deny beats an allow on the same path.
4. If no path has an entry for any ancestor, the answer is no.

So with `add_ace group.admin command allow` and
`add_ace group.admin command.quit deny`, an admin may run `start` and may not
run `quit`. A principal with no entries is denied everything.

Example `server.cfg` that lets one player and its group run every command:

```text
add_principal identifier.license:<hex> group.admin
add_ace group.admin command allow
```

Replace `<hex>` with the player's `license` identifier, as printed by
`players` or `GetPlayerIdentifierByType`. Entries take effect as soon as the
line runs, so they can also be set from the console while the server runs.
Granting `command` grants `exec`, which runs any file the server can read, and
the access commands themselves. Grant only what a player needs.

## Convars

Convar names are case-insensitive. Scripts read them with `GetConvar` and
`GetConvarInt`.

| Convar | Default | Notes |
|---|---|---|
| `sixm_resourcesPath` | `resources` | Folder scanned for resources. Relative to the working directory. |
| `sixm_identityFile` | `cache/server-identity.bin` | File that holds the server certificate. Empty means a new certificate on every start. Relative to the working directory. |
| `sixm_kvpPath` | `cache/kvp` | Folder of the resource key-value stores, one `<resource>.kvp` file each. Relative to the working directory. Read the first time a resource uses key-value storage. See [Key-value storage](lua-api.md#key-value-storage). |
| `sixm_tickRate` | `20` | Frames per second, clamped to 1 to 1000. A value that is not a number gives 20. |
| `endpoint_add_udp` | `0.0.0.0:30120` | Listen address, for TCP (HTTP) and UDP (the game). Read when the network starts. An address that does not parse stops the network. A TCP port that is in use does not: the error is logged and the server runs without HTTP. |
| `sv_httpEnabled` | `true` | Whether the HTTP listener runs. `false` or `0` turns it off. Read when the network starts. |
| `sv_httpAllowPrivate` | `false` | Console-only. Whether `PerformHttpRequest` may reach loopback, link-local (such as `169.254.169.254`), or private network IP addresses (`true` or `1` turns it on). |
| `sixm_httpHandlerTimeout` | `25` | Seconds a `SetHttpHandler` handler may take to complete its response before the client gets `504`, clamped to 1 to 120. A value that is not a number gives 25. |
| `sv_exposePlayerIdentifiers` | `false` | Whether `/players.json` lists each player's identifiers. Only `true` turns it on. See [HTTP endpoints](#http-endpoints). |
| `rcon_password` | empty | Console-only credential for [`POST /rcon`](#remote-console). Set with `set rcon_password "<password>"`. |
| `sv_maxclients` | `32` | Player limit, clamped to 1 to 1024. A value that is not a number gives 32. |
| `sixm_deferralTimeout` | `120` | Seconds a connection that `playerConnecting` deferred may wait for `done`, clamped to 1 to 3600. A deferral that runs longer is refused with `connection timed out`. A value that is not a number gives 120. |
| `sixm_httpTimeout` | `30` | Seconds an HTTP request from `PerformHttpRequest` may take, clamped to 1 to 600, unless its `options.timeout` gives another. A value that is not a number gives 30. See [HTTP](lua-api.md#http-server-only). |
| `sixm_httpWorkers` | `8` | Threads that run the HTTP requests of `PerformHttpRequest`, clamped to 1 to 64. Read when the first request starts; a later change needs a restart. A value that is not a number gives 8. |
| `sv_hostname` | `SixM Server` | Server name sent to players when they connect. |
| `sixm_playerModel` | `player` | Model name of the ped created for each player when it is admitted. The ped is owned by the player and deleted when it leaves. |
| `sixm_cullRadius` | `424` | Distance in metres within which a player is sent entities, measured from its ped. Entities the player owns are always sent. Clamped to 0 to 100000. `0` turns scoping off, so every player is sent every entity. A value that is not a number gives 424. A change takes effect on the next frame. See [Scoping](lua-api.md#scoping). |
| `sixm_maxClientEntities` | `64` | Replicated. Most networked entities one client may create at once, clamped to 0 to 4096. |
| `sv_entityLockdown` | `inactive` | Global client entity creation lockdown mode (`inactive`, `relaxed` to block peds, or `strict` to block all client creations), overridable per routing bucket via `SetRoutingBucketEntityLockdownMode`. |

Any other convar can be set with `set`, `sets` or `setr`, or with the Lua
setters, and read by scripts. Console-only bootstrap and security convars
(`rcon_password`, `sv_httpAllowPrivate`, `sv_httpEnabled`,
`sv_exposePlayerIdentifiers`, `sixm_identityFile`, `sixm_resourcesPath`,
`sixm_kvpPath`, and `endpoint_add_udp`) cannot be modified by scripts or remote
players. The server checks a value only when the convar is replicated (see
below).

### Kinds of convar

Every convar has a kind. The command or function that stores it decides the
kind, and the last store wins:

| Console | Lua | Kind |
|---|---|---|
| `set` | `SetConvar` | Plain. Kept on the server. |
| `sets` | `SetConvarServerInfo` | Server information. Included in `/info.json`. |
| `setr` | `SetConvarReplicated` | Replicated. Sent to clients. |

A replicated convar is sent to every connected player as soon as it is stored.
A player who joins gets all replicated convars before the resource list, so
its client scripts can read them while they load. When a replicated convar is stored with another
kind, for example by `set`, every player is told that it has no value, and the
player forgets it.

A replicated convar's name must be 1 to 256 bytes, and its value at most 4096
bytes. A `setr` or `SetConvarReplicated` that breaks a limit fails with an
error, and the convar keeps its previous value.

The console shows the kind of a convar that is not plain, for example:

```text
sv_hostname = "x" (replicated)
```

Players cannot read or set convars through the server, as described under
[Permissions](#permissions).

### Private convar readers

Plain server convars are readable by every server resource by default. The
first `add_convar_permission` limits a convar to its named readers. Add one
line per resource, before the resources start and before setting a secret:

```text
add_convar_permission billing read billing_token
set billing_token "<private-token>"
```

The command requires the server console or `server.cfg`. It applies to Lua
and JavaScript. A resource without access receives its supplied default from
the getters, receives no change notifications for the convar, and cannot
overwrite the convar through `SetConvar*` or `ExecuteCommand('set ...')`. The
policy applies to plain convars, and a restricted name cannot be stored with
`sets` or `setr`. Console-only convars cannot be granted via
`add_convar_permission`. Remove an entry with
`remove_convar_permission <resource> read <name>`, and inspect current entries
with `list_convar_permissions`.

## Resource discovery

- Each direct subfolder of `sixm_resourcesPath` is checked.
- A folder with a manifest is a resource. See [manifest.md](manifest.md).
- A folder named `[category]` is searched in turn. Categories can nest.
- Other folders, and folders that start with a dot, are ignored. Resources are
  not searched inside a resource.
- Folders are visited in sorted order. If two folders have the same resource
  name, the first one wins and the other is logged and ignored.
- A folder whose manifest fails to load is logged and skipped.

## Identity and fingerprint

The server presents a self-signed certificate. Its file is set by
`sixm_identityFile`.

- The file is created on the first start, together with its folder if needed.
- The file holds the certificate and its PKCS#8 private key. Each part is
  preceded by its length as a `u32` in little-endian order.
- On Unix the file is written with mode `0600`. On Windows,
  before the key is written, its access list is replaced with one entry: full
  control for the file's owner, with nothing inherited from the folder. A file
  system without access lists keeps the folder's, with a warning in the log.
- A file that is not a SixM identity is an error. The network does not start.
  Delete the file to create a new one.
- The fingerprint is the SHA-256 digest of the certificate, written as 64
  lowercase hex characters. It is logged at startup as `Server fingerprint`.
- A client that passes that fingerprint with `--fingerprint` refuses any other
  certificate. See [client.md](client.md).

## HTTP endpoints

The server answers plain HTTP/1.1 on the TCP port of `endpoint_add_udp`. Each
connection carries one request and is closed after its answer.

| Path | Answer |
|---|---|
| `/info.json` | JSON: `server`, the running `resources`, and `vars`, the server-info convars (`sets`). |
| `/players.json` | JSON array of `{id, name, ping}` per player. `identifiers` is added only with `sv_exposePlayerIdentifiers true`. |
| `/dynamic.json` | JSON: `hostname`, `clients`, `maxclients` and `server`. |
| `/rcon` | [`POST /rcon`](#remote-console) executes one authenticated console command and returns its output as JSON. |
| `/<resource>/...` | The `SetHttpHandler` function of that resource, if it has one. See [HTTP handlers](lua-api.md#http-handlers-server-only). |

A request is routed by the first segment of its raw path. It is not decoded or
normalized, so `/app%2Fother` and `/app/../other` never reach the handler of
`other`; the handler of `app` receives the path as sent. Other paths get `404`. The built-in documents accept `GET` and `HEAD`; other
methods get `405`.

Limits: a request body of 1 MiB at most (`413` above it), 128 connections at
once (further ones are closed on arrival), 10 seconds to send the request head,
130 seconds from the first byte to the answer at most (`503`), and 20 requests a second
per client address with bursts of 60 (`429`). A handler that does not complete
its response within `sixm_httpHandlerTimeout` (25 seconds) gets `504`; a request whose resource stops gets
`503`.

The listener speaks plain HTTP. Put a reverse proxy with TLS in front of it when
the endpoints must be reached over the internet, and open the TCP port in the
firewall with the UDP port.

### Remote console

Set `rcon_password` in `server.cfg` or from the server console. An empty or
unset password disables the endpoint. The credential is available only to the
console configuration and is excluded from script APIs and public convars.
Authenticated requests run with full console permissions. The listener uses
plain HTTP, so use a private connection or put a TLS reverse proxy in front of
it and restrict direct access to the HTTP listener.

Send `POST /rcon` with a JSON body such as `{"command":"help","password":"<password>"}`.
`Authorization: Bearer <password>` and
`X-Rcon-Password: <password>` also accept either a JSON body with `command` or
a plain-text command body. A `200` response has the shape
`{"ok":true,"output":["..."]}`. Command errors appear in `output`.

The endpoint returns `400` for an invalid request, `401` for a bad password,
`403` when disabled and `405` for other methods. After five consecutive
failures from the same TCP peer IP, it returns `429` until 30 seconds have
elapsed since the first failure. Clients sharing a reverse proxy's connection
address share that limit.

## Players

- Player ids start at 1 and count up. An id is not reused while the server
  runs. A refused connection also uses an id.
- The player limit is `sv_maxclients`. A connection over the limit is refused
  with `Server is full`. Only admitted players count, not players still in
  `playerConnecting` or held by deferrals.
- Each player has the identifiers `license:<hex>`, the fingerprint of the
  client's certificate, and `ip:<address>`. See [lua-api.md](lua-api.md).
  Clients must present a certificate, so every player has a `license`.
- A script can refuse a connection in `playerConnecting`. Without a reason the
  client sees `Rejected by the server`.
- Each player's inbound events and console lines are rate limited together, at
  100 per second sustained, with a burst of 200. A player whose traffic stays
  above 1000 per second for five seconds in a row is kicked for flooding.

## Quitting

The `quit` command and Ctrl+C both set the quit flag. The loop checks the flag
on each pass and then shuts down.

## Frame profiling

Use `profiler record 120` to capture the next 120 server ticks, or
`profiler record` to keep recording until `profiler stop`. `profiler status`
shows the frame and event counts and reports whether the capture reached its
250,000-event limit. `profiler saveJSON profiles/capture.json` saves the current
capture relative to the server's working directory. Open the file in a Chrome
Trace Event viewer such as Perfetto to inspect each `ServerTick` and its
resource spans. When recording is idle, ticks skip timestamping resource spans.

## Load testing

`examples/loadtest` is a server for measuring load, and the `swarm` example
connects many clients to it. Run the server from its folder, and the swarm
from the repository root, in two terminals:

```sh
cd examples/loadtest
cargo run --release -p sixm-server
```

```sh
cargo run --release -p sixm-client --example swarm -- 127.0.0.1:30120 50 --seconds 60
```

Then type `perf` in the server console. The swarm prints a line per bot and
a summary when the run ends.

- The server owns `loadtest_vehicles` vehicles (500 by default) on a 2 km
  square. Every tenth vehicle moves on each frame, so transforms go out every
  frame.
- Each client moves its own ped on a 20 m circle, and pings the server every 5
  seconds. The server prints the pings per second every 10 seconds.
- `swarm <addr> <count>` starts `count` bots named `bot1` to `botN`, 50 ms
  apart, each with an ephemeral certificate and its own cache. `--seconds N`
  sets the run length (60 by default). `--cache DIR` keeps the caches in
  `DIR`; without it they go to a temporary folder. Bots log at warn level, and
  `RUST_LOG` overrides that.
- The server needs `sv_maxclients` above the number of bots. The example
  configuration sets 128.

## Files and logs

| Path | Contents |
|---|---|
| `server.cfg` | Read when no `+command` is given. |
| `resources/` | Default resource folder (`sixm_resourcesPath`). |
| `cache/server-identity.bin` | Default certificate and key (`sixm_identityFile`). |

Server-side log lines use the target `sixm::script` for script output. Script
`print` calls go there, prefixed with the resource name.
