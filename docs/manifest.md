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
| `files`, `exports`, `server_exports`, `games`, `provides`, `server_scripts`, `client_scripts`, `shared_scripts` | the name without the trailing `s` |

Other table directives also lose one trailing `s`. So `this_is { 'x' }` is
stored as `this_i`. A plain string with an unknown name keeps its spelling.

## Directives with a meaning

| Directive | Field | Notes |
|---|---|---|
| `server_script` / `server_scripts` | `server_scripts` | Loaded on the server (`.lua` or `.js`). Globs allowed. |
| `client_script` / `client_scripts` | `client_scripts` | Sent to clients and loaded there (`.lua` or `.js`). Globs allowed. |
| `shared_script` / `shared_scripts` | `shared_scripts` | Loaded on the server and sent to clients (`.lua` or `.js`). Globs allowed. |
| `file` / `files` | `files` | Sent to clients as data files. Not loaded as scripts. Globs allowed. |
| `dependency` / `dependencies` | `dependencies` | Resources that must start first. See below. |
| `provide` / `provides` | `provides` | Virtual resource names this resource provides as aliases. See below. |
| `export` / `exports` | `exports` | Client exports, sent to clients. See below. |
| `server_export` / `server_exports` | `server_exports` | Server exports. See below. |
| `server_only 'yes'` | `server_only` | Never offered to clients: no files, scripts, exports or metadata are sent, and clients are not told it started or stopped. It still runs on the server and can `provide` and be depended on. Any value other than `'yes'` is false. |
| `fx_version` | `fx_version` | Stored only. The host does not read it. |
| `game` / `games` | `games` | Stored only. The host does not read it. |
| `lua54 'yes'` | `lua54` | Stored only. Any value other than `'yes'` is false. The host does not read it. |
| `resource_type 'map'` / `'gametype'` | `resource_type` | Stored as metadata. The stock `mapmanager` recognizes these exact values. |
| `name 'Display name'` | `name` | Stored as metadata; `mapmanager` uses the first value as the map/gametype display name. |
| `gameTypes 'race'` | `gameTypes` | Repeated scalar metadata. A map's optional compatible gametype resource names; no entries means any known gametype. |
| `map 'directives.json'` | `map` | Repeated scalar metadata. A mapmanager JSON directive file, also listed in `files`. Never loads game assets or executes code. |
| anything else | `metadata` | Stored in order as `(name, value)`. The host does not read it. |

The first group and `server_only` change behaviour directly. `resource_type`, `name`, `gameTypes` and `map` remain ordinary metadata and are interpreted by the stock `mapmanager` when it is running. `fx_version`, `game`, `lua54` and all other metadata are parsed and kept, but nothing else in SixM uses them yet.

### Accepted directives with no host effect

The directives below are valid in `fxmanifest.lua` / `__resource.lua` (they are parsed and stored in the manifest or `metadata`), so ported FiveM manifests load without errors, but the SixM host ignores them:

| Directive | How SixM stores it | Notes |
|---|---|---|
| `fx_version` | `fx_version` | Stored as the last declared string; does not change runtime behaviour. |
| `game` / `games` | `games` | Stored as a list; does not gate loading or change runtime behaviour. |
| `lua54` | `lua54` | Sets `lua54 = true` only when `'yes'`; Lua scripts always run on Lua 5.4 regardless. |
| `resource_type` | `metadata` | Values other than stock `mapmanager`'s `'map'` and `'gametype'` have no host effect. |
| `convar_category` | `metadata` | Stored as metadata only; no host effect. |
| `escrow_ignore` | `metadata` | Stored as metadata only; SixM has no Cfx.re asset escrow encryption. |
| `resource_manifest_version` | `metadata` | Legacy `__resource.lua` version directive; stored as metadata with no host effect. |
| `use_experimental_fxv2_oal` | `metadata` | Stored as metadata only; SixM does not implement OAL. |
| `clr_disable_task_scheduler` | `metadata` | Stored as metadata only; SixM has no C# (`Mono`) runtime. |
| `node_version` | `metadata` | Stored as metadata only; JS runs in embedded QuickJS (ES2023), not Node.js. |

UI and game-adapter directives such as `ui_page`, `loadscreen`, `loadscreen_cursor`, `loadscreen_manual_shutdown`, `this_is_a_map`, `data_file`, and `replace_level_meta` are likewise accepted as metadata without host effect until a game or UI adapter exists.

The server does not directly sync a `server_only` resource to clients. A
different, client-synced resource may explicitly reference one of its files
with a path such as `@server_only_res/data/config.json` in `files`,
`client_scripts`, or `shared_scripts`. That file is sent as part of the
referencing resource; this does not advertise or start the `server_only`
resource on clients.

## Mapmanager metadata and JSON directives

The stock `mapmanager` reads scalar metadata in this supported form:

```lua
resource_type 'map'
name 'Downtown'
gameTypes 'race'
gameTypes 'arena'
map 'directives.json'
files { 'directives.json' }
```

Use `resource_type 'gametype'` for a gametype. The table form
`resource_type 'gametype' { name = '...' }` is not preserved by SixM's scalar
metadata recorder; use `name '...'` instead. Resource names in `gameTypes` are
canonical resource names. If a map has no `gameTypes` values, it supports any
known gametype.

Each `map` path names UTF-8 JSON data relative to the map resource and must
also appear in `files`. The top-level object contains a dense `directives`
array. Each item has a nonempty string `name` and may have a JSON `data` value; absent or `null` data is passed to handlers as `nil` in Lua or `null` in JavaScript. For
example:

```json
{"directives":[{"name":"spawnpoints","data":[{"x":1,"y":2,"z":3,"heading":90}]}]}
```

On a client map start, `mapmanager` fires `getMapDirectives(add)` immediately
before reading the files. Both Lua and JavaScript client resources can register
handlers with `add(name, handler)` (JavaScript callbacks are passed through
SixM function references and invoked after the registration event returns); each
matching handler receives `(data, mapResourceName)`. The handler registry exists
only for that map start. Invalid or unreadable files are logged and skipped;
handler errors do not prevent later directives from running. JSON cannot execute
scripts, and SixM does not load map or game assets.

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
- `@alias/...` also resolves through [`provide` aliases](#providing-aliases-provide--provides)
  to the provider resource's folder.
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
- Dependencies resolve `provide` aliases to the provider resource, and cycles
  through aliases are detected as well.
- Constraint entries that start with `/` — `/server:`, `/onesync`, `/gameBuild`, and `/native:` — are kept in `dependencies` as raw strings, but omitted from dependency resolution and start/stop ordering (they are not treated as missing resources and have no runtime effect).

## Providing aliases (`provide` / `provides`)

A resource can declare virtual names it provides so existing resources that
depend on or reference another resource name continue to work:

```lua
provide 'mysql-async'
provides { 'ghmattimysql', 'db-core' }
```

- **Validation:** Each entry must be a valid resource name component (for
  example, empty strings, path separators, `..`, and leading dots are rejected
  when parsing the manifest).
- **Resolution:** Aliases resolve to the canonical provider resource across:
  - `dependency` / `dependencies` start order and dependent stop order
  - `@alias/...` manifest script and file references
  - `exports['alias']` / `exports.alias` calls
  - Resource natives (`GetResourceState`, `GetResourceMetadata`,
    `GetNumResourceMetadata`, `GetResourcePath`, `LoadResourceFile`,
    `StartResource`, `StopResource`, `RestartResource`) and console commands
    (`start`, `stop`, `restart`, `ensure`)
  - `GetNumResources` and `GetResourceByFindIndex` continue to list canonical
    installed resource names only.
- **Duplicates and conflicts:**
  - Duplicate `provide` entries within the same resource are deduplicated
    without error.
  - A canonical installed resource name always takes precedence over an alias.
  - If a `provide` alias matches an installed resource's canonical name, that
    alias is skipped and logged as a scan warning.
  - If two resources declare the same `provide` alias, the first resource in
    sorted scan order keeps the alias, and the conflicting alias on later
    resources is skipped with a warning (non-conflicting aliases on those
    resources are still registered).

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
| Directive output | 8,192 entries and 1 MiB of combined keys and values | Evaluation fails when either limit is exceeded. |

The limits are constants in `manifest.rs` and are not configurable.

The manifest state has the `table`, `string` and `math` libraries and the Lua
base functions. These globals read as `nil`: `coroutine`, `debug`, `dofile`,
`io`, `load`, `loadfile`, `os`, `package`, `pcall`, `require`, `setmetatable`,
`utf8` and `xpcall`. The manifest can declare files and scripts using metadata.
The server and client run only `.lua` and `.js` script files. Other extensions
can be declared as data in `files`, where the runtime leaves them as data.

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
