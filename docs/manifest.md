# fxmanifest.lua

Every resource has a manifest. It is a Lua file that declares what the
resource contains. SixM evaluates it in a restricted Lua state and reads the
recorded directives. Source of truth:
`crates/sixm-resources/src/manifest.rs` and `crates/sixm-resources/src/resource.rs`.

## Finding the manifest

- The resource is a folder. Its name is the resource name.
- `fxmanifest.lua` is used. If it is missing, the legacy `__resource.lua` is used.
- A folder without either file is not a resource.
- Folders named `[category]` group resources and can nest. Folders that start
  with a dot are skipped.

## How directives are read

Any global that is not a Lua built-in is a directive recorder. Calling it
records its arguments:

```lua
fx_version 'cerulean'
game 'common'

server_scripts {
    'server/*.lua',
}
```

Rules:

- A string argument is recorded as one entry.
- A table argument records one entry per element. Only scalars are allowed
  inside the table. A nested table is an error.
- Strings and numbers are accepted. Numbers keep their text form, but a float
  with no fraction loses its `.0` (`2.0` is stored as `2`).
- Booleans, `nil` and functions are errors. The message names the directive,
  for example ``directive `author` cannot take a boolean value``.
- Reading an unknown global without calling it does not record anything.
  A typo in a directive name is stored as metadata, and nothing warns about it.

### Chained arguments

A second call on the result of a directive stores its arguments as
`<name>_extra`. Only the first two calls in a chain are kept:

```lua
data_file 'HANDLING_FILE' 'data/handling.meta'
-- metadata: data_file = HANDLING_FILE, data_file_extra = data/handling.meta
```

A table after a chained call stores one `<name>_extra` entry per element.
Calls after the second are ignored.

### Plural and singular names

Some names are stored in singular form, so a plural string or a table works
the same way:

| Written as | Stored as |
|---|---|
| `dependencies { ... }` or `dependency 'x'` | `dependency` |
| `files`, `exports`, `server_exports`, `games`, `server_scripts`, `client_scripts`, `shared_scripts` | the name without the trailing `s` |

Other table directives also lose one trailing `s`. So `this_is { 'x' }` is
stored as `this_i`. A plain string with an unknown name keeps its spelling.

## Directives with a meaning

| Directive | Field | Notes |
|---|---|---|
| `server_script` / `server_scripts` | `server_scripts` | Loaded on the server. Globs allowed. |
| `client_script` / `client_scripts` | `client_scripts` | Sent to clients and loaded there. Globs allowed. |
| `shared_script` / `shared_scripts` | `shared_scripts` | Loaded on the server and sent to clients. Globs allowed. |
| `file` / `files` | `files` | Sent to clients as data files. Not loaded as scripts. Globs allowed. |
| `dependency` / `dependencies` | `dependencies` | Resources that must start first. See below. |
| `export` / `exports` | `exports` | Client exports, sent to clients. See below. |
| `server_export` / `server_exports` | `server_exports` | Server exports. See below. |
| `fx_version` | `fx_version` | Stored only. The host does not read it. |
| `game` / `games` | `games` | Stored only. The host does not read it. |
| `lua54 'yes'` | `lua54` | Stored only. Any value other than `'yes'` is false. The host does not read it. |
| anything else | `metadata` | Stored in order as `(name, value)`. The host does not read it. |

Only the first group changes behaviour. `fx_version`, `game`, `lua54` and
metadata are parsed and kept, but nothing in SixM uses them yet.

## Script load order

Server side, the server loads:

1. `shared_scripts`, in declaration order
2. `server_scripts`, in declaration order

Client side, the server sends and the client loads, in this order:

1. `shared_scripts`
2. `client_scripts`
3. `files`, which are sent but not loaded as scripts

Each file appears once. If several patterns match the same file, the first
match decides its position.

## Paths and globs

- Paths are relative to the resource folder. Backslashes are accepted as
  separators.
- Absolute paths, a leading `/`, and any `..` component are rejected.
- A pattern without `*`, `?`, `[`, `]`, `{` or `}` is a literal path. If the
  file does not exist, the resource fails to load its scripts.
- A glob with no matches adds no files and logs a warning. It is not an error.
- In a glob, `*` does not cross a folder. `**` does.
- Matches of one glob are sorted by path.

Example:

```lua
server_scripts {
    'srv/*.lua',   -- srv/a.lua, srv/b.lua, not srv/sub/c.lua
    'deep/**/*.lua',
}
```

### References to other resources

A pattern that starts with `@other/` reads files from the installed resource
`other`:

```lua
server_scripts { '@oxmysql/lib/MySQL.lua', '@oxmysql/lib/*.lua' }
```

- The resource must be installed, running or not. Otherwise the resource
  fails with an unknown-resource error.
- The rest of the path follows the same rules, and must not escape `other`.
- Files reached this way are named `@other/<path>` when sent to clients. The
  client keeps that name, so `LoadResourceFile('res', '@other/...')` can find
  them.

### Client file names

A client file is named by its path relative to the resource folder, using `/`.
The name is the key that `LoadResourceFile` uses on the client. Files are
stored by their SHA-256 hash in the client cache, so the name is only a key on
the client side. See [client.md](client.md).

If a resource's client file list cannot be expanded (a missing literal file,
for example), the server logs an error and sends no client files for it. The
resource still starts on the server.

If a server script cannot be expanded, the resource does not start.

## Dependencies

```lua
dependency 'greeter'
dependencies { 'a', 'b' }
```

- Starting a resource starts its dependencies first, in dependency order.
- Stopping a resource stops the running resources that depend on it first.
- A dependency that is not installed is an error.
- A dependency cycle is an error. The error lists the cycle.
- Entries that start with `/` are kept but ignored. The code leaves them out
  of the start order, so they have no effect yet.

## Exports

```lua
export 'getScore'          -- client export, sent to clients
server_export 'greet'      -- server export
```

- A resource can register an export at run time with `exports('name', fn)`.
  See [lua-api.md](lua-api.md).
- A declared name with no registration uses a global function of the same
  name, if one exists.
- Only client exports (`export`) are sent to clients. The client gets a copy
  of the list for each resource.
- At most 256 client exports per resource. Each name is 1 to 256 bytes. If
  either limit is broken, no exports are sent for that resource and an error
  is logged. Server exports have no such limit in the code.

## Limits

The manifest runs in its own Lua state, so a manifest cannot hang the server
or exhaust memory:

| Limit | Value | What happens |
|---|---|---|
| Memory | 16 MiB | The Lua allocation fails and the manifest is an error. |
| Instructions | 10,000,000 | Checked every 10,000 instructions. The manifest is aborted. |

The limits are constants in `manifest.rs` and are not configurable.

The manifest state has the `table`, `string` and `math` libraries and the Lua
base functions. These globals read as `nil`: `coroutine`, `debug`, `dofile`,
`io`, `load`, `loadfile`, `os`, `package`, `pcall`, `require`, `utf8` and
`xpcall`. `pcall` is removed so that the instruction limit cannot be caught.

A manifest that fails to parse or run is reported and skipped during the scan.
Other resources are not affected.

## Limits on files

| Item | Limit |
|---|---|
| File size | 64 MiB per file. Larger files are skipped with a warning. |
| Resource name | 256 bytes |
| File name sent to clients | 1024 bytes |
| Files per resource entry on the wire | 256 |

## Example

From `examples/server-data`:

```lua
-- hello/fxmanifest.lua
fx_version 'cerulean'
game 'common'

dependency 'greeter'

server_scripts {
    'server/*.lua',
}

client_scripts {
    'client/*.lua',
}

files {
    'data/greeting.txt',
}
```

The `hello` resource loads its server scripts on the server, sends its client
scripts and `data/greeting.txt` to clients, and starts after `greeter`.
