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
3. Start the network on `endpoint_add_udp`. If it fails, the error is logged
   and the server runs without networking.
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
| `endpoint_add_tcp <ip:port>` | The same as `endpoint_add_udp`. SixM speaks only UDP, and the command logs a note. |
| `players` | Lists connected players with their id, name and endpoint. |
| `state [bag]` | Without a name, lists the state bags. With a name, prints the bag's keys and values. Replicated keys are marked. |
| `set <name> <value>` | Stores a convar. The value is the rest of the line, joined with single spaces. |
| `sets <name> <value>` | Same as `set`. |
| `setr <name> <value>` | Same as `set`. |
| `exec <file>` | Runs each line of the file. See above. |
| `help` | Lists the built-in commands and the script commands. Restricted commands are marked. |
| `quit [reason]` | Asks the server to stop. The reason is written to the log. |

A failed resource command is logged. Expected refusals, such as a resource
canceled by `onResourceStarting`, are warnings.

### Other names

For any other name, the server checks in this order:

1. A script command registered with `RegisterCommand`. It receives the
   arguments and the raw line.
2. An existing convar. Given no value, it prints the convar. Given a value, it
   sets it.
3. Otherwise the server logs `No such command`.

## Convars

Convar names are case-insensitive. Scripts read them with `GetConvar` and
`GetConvarInt`.

| Convar | Default | Notes |
|---|---|---|
| `sixm_resourcesPath` | `resources` | Folder scanned for resources. Relative to the working directory. |
| `sixm_identityFile` | `cache/server-identity.bin` | File that holds the server certificate. Empty means a new certificate on every start. Relative to the working directory. |
| `sixm_tickRate` | `20` | Frames per second, clamped to 1 to 1000. A value that is not a number gives 20. |
| `endpoint_add_udp` | `0.0.0.0:30120` | Listen address. Read when the network starts. An address that does not parse stops the network. |
| `sv_maxclients` | `32` | Player limit, clamped to 1 to 1024. A value that is not a number gives 32. |
| `sv_hostname` | `SixM Server` | Server name sent to players when they connect. |

Any other convar can be set with `set` and read by scripts. The server does
not check them.

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
- On Unix the file is written with mode `0600`. On Windows that restriction is
  not applied, so the private key is readable by other users of the machine.
- A file that is not a SixM identity is an error. The network does not start.
  Delete the file to create a new one.
- The fingerprint is the SHA-256 digest of the certificate, written as 64
  lowercase hex characters. It is logged at startup as `Server fingerprint`.
- A client that passes that fingerprint with `--fingerprint` refuses any other
  certificate. See [client.md](client.md).

## Players

- Player ids start at 1 and count up. An id is not reused while the server
  runs. A refused connection also uses an id.
- The player limit is `sv_maxclients`. A connection over the limit is refused
  with `Server is full`.
- A script can refuse a connection in `playerConnecting`. Without a reason the
  client sees `Rejected by the server`.
- Each player's inbound events are rate limited to 100 events per second
  sustained, with a burst of 200. A player whose traffic stays above 1000
  events per second for five seconds in a row is kicked for flooding.

## Quitting

The `quit` command and Ctrl+C both set the quit flag. The loop checks the flag
on each pass and then shuts down.

## Files and logs

| Path | Contents |
|---|---|
| `server.cfg` | Read when no `+command` is given. |
| `resources/` | Default resource folder (`sixm_resourcesPath`). |
| `cache/server-identity.bin` | Default certificate and key (`sixm_identityFile`). |

Server-side log lines use the target `sixm::script` for script output. Script
`print` calls go there, prefixed with the resource name.
