# ACE permissions

SixM uses access control entries (ACEs) to decide which players and resources
may run commands or pass script permission checks. By default, players and
resources start with **no permissions**: anything not explicitly allowed is
denied.

## Principals, objects, and inheritance

An ACE links a **principal** (who) to an **object** (what) with `allow` or
`deny`:

- **Principals** represent identities, groups, players, or resources:
  - `player.<id>` — a connected or connecting player by server ID (for example
    `player.1`).
  - `identifier.license:<hex>` — the player's certificate-backed license
    identifier (64 lowercase hex characters). Every `player.<id>` automatically
    inherits from `identifier.license:<hex>` and `identifier.ip:<address>`.
  - `identifier.ip:<address>` — the IP address the player connected from,
    without the port.
  - `resource.<name>` — a running resource calling `ExecuteCommand` (for
    example `resource.admin`).
  - `group.<name>` — a logical group such as `group.admin` or `group.mod`.
  - `builtin.everyone` — every principal implicitly inherits from
    `builtin.everyone`.
- **Objects** are dotted capability names such as `command.start`,
  `command.spawncar`, or custom script permissions such as `admin.kick`.
- **Case-insensitivity:** All principal and object names are compared
  case-insensitively.

### Inheritance (`add_principal`)

Use `add_principal <child> <parent>` to make one principal inherit all ACEs
from another:

```text
add_principal identifier.license:0123...abcd group.admin
add_principal group.admin group.staff
```

Inheritance is transitive (cycles are safely ignored). Because `player.<id>`
automatically inherits from its `identifier.*` principals and from
`builtin.everyone`, assigning `identifier.license:<hex>` to `group.admin`
grants `group.admin` permissions to that player whenever they join with that
client identity file.

> [!WARNING]
> Prefer `identifier.license:<hex>` over `identifier.ip:<address>` for staff
> permissions. IP addresses can be shared across NAT or reassigned, whereas
> `license:<hex>` is proven cryptographically by the client's `identity.bin`
> keypair during the TLS handshake.

## Console commands

You can manage ACEs in `server.cfg`, in `+commands`, or live in the server
console:

| Command | Action |
|---|---|
| `add_ace <principal> <object> <allow\|deny>` | Sets an entry for `principal` on `object` (replaces any earlier entry for the same pair). |
| `remove_ace <principal> <object> <allow\|deny>` | Removes that exact entry. |
| `add_principal <child> <parent>` | Makes `child` inherit from `parent`. |
| `remove_principal <child> <parent>` | Removes an inheritance link. |
| `list_aces` | Prints every active ACE as `ace <principal> <object> <allow\|deny>`. |
| `list_principals` | Prints every inheritance link as `principal <child> inherits from <parent>`. |
| `test_ace <principal> <object>` | Prints whether `principal` is `allowed` or `denied` for `object`. |

Changes take effect immediately. Note that ACEs are held in memory; put
persistent rules in `server.cfg` (or an included config via `exec`).

## How rules are evaluated

When SixM checks whether a principal is allowed an object (for example
`command.quit`):

1. **Collect ancestors:** The principal itself, any automatic parents (for
   `player.<id>`, its `identifier.license:<hex>` and `identifier.ip:<address>`),
   `builtin.everyone`, and all transitive parents via `add_principal`.
2. **Walk the object path from most specific to least specific:** For
   `command.quit`, SixM checks `command.quit` across all ancestors first, then
   `command`.
3. **Resolve at the first matching depth:** At the most specific path where at
   least one ancestor has an entry:
   - Any `deny` returns **denied** (`deny` beats `allow` at the same path depth).
   - Otherwise, an `allow` returns **allowed**.
4. **Default deny:** If no ancestor has an entry at any prefix, the result is
   **denied**.

Because more-specific paths are checked before shorter prefixes:

```text
add_ace group.admin command allow
add_ace group.admin command.quit deny
```

Here, `group.admin` can run `command.start` and `command.stop`, but is denied
`command.quit`. Conversely, a deeper `allow` overrides a shallower `deny`:

```text
add_ace builtin.everyone command deny
add_ace builtin.everyone command.help allow
```

## Built-in and restricted commands

- **Server console (`stdin`, `server.cfg`, `exec`, `+commands`):** Always
  allowed to run every command.
- **Built-in commands:** All 24 built-in server commands (`start`, `stop`,
  `restart`, `ensure`, `refresh`, `set`, `sets`, `setr`, `exec`, `quit`,
  `players`, `entities`, `state`, `perf`, `help`, `endpoint_add_udp`,
  `endpoint_add_tcp`, `add_ace`, `remove_ace`, `add_principal`,
  `remove_principal`, `list_aces`, `list_principals`, `test_ace`) require
  `command.<name>` when invoked by a player or by a resource via
  `ExecuteCommand`.
- **Restricted script commands:** Registering a command with `restricted = true`
  checks `command.<name>`:

  ```lua
  RegisterCommand('spawncar', function(source, args)
      -- ...
  end, true)
  ```

  Omitting the third argument (or passing `false`) leaves the command
  unrestricted so any player or resource can run it.
- **Convars by bare name:** A player or resource can **never** read or set a
  convar by typing its bare name (such as `sv_hostname` or
  `sv_hostname "New"`). Only the server console can do that.
- **Console-only & restricted convars:** Security-critical and bootstrap
  convars (`rcon_password`, `sv_httpAllowPrivate`, `sv_httpEnabled`,
  `sv_exposePlayerIdentifiers`, `sixm_identityFile`, `sixm_resourcesPath`,
  `sixm_kvpPath`, `endpoint_add_udp`) cannot be modified from scripts or remote
  commands, and restricted convars (`add_convar_permission`) cannot be
  overwritten by resources without read access.

> [!CAUTION]
> Granting `add_ace group.admin command allow` grants `command.exec`,
> `command.set`, `command.quit`, `command.add_ace`, and `command.add_principal`.
> Give `command` only to fully trusted administrators; for moderators or
> builders, grant only the specific commands they need (for example
> `command.spawncar` and `command.clearcars`).

## `resource.<name>` principals and `ExecuteCommand`

Unlike FiveM—where `ExecuteCommand` runs with full console permissions—SixM
runs lines queued by `ExecuteCommand` with the permissions of the calling
resource's principal, `resource.<name>`.

For example, if resource `admin` runs `ExecuteCommand('stop hello')`, the
server checks `resource.admin` for `command.stop`. Without an ACE entry, the
server refuses the command and logs a warning:
`resource \`admin\` may not run \`stop\``.

Grant only what the resource needs in `server.cfg`:

```text
add_ace resource.admin command.start allow
add_ace resource.admin command.stop allow
add_ace resource.admin command.restart allow
```

Keep these rules in mind when writing resources that call `ExecuteCommand`:

- **Restricted script commands:** `ExecuteCommand` also checks `command.<name>`
  against `resource.<name>` for restricted script commands.
- **Nested `exec`:** If a resource is allowed `command.exec` and runs
  `ExecuteCommand('exec extra.cfg')`, every line inside `extra.cfg` still runs
  under `resource.<name>` (not console rights).
- **Command `source`:** Script commands triggered by `ExecuteCommand` receive
  `source = 0`.
- **Always authorize player requests first:** If a player triggers a server
  event or an unrestricted command that calls `ExecuteCommand`, `ExecuteCommand`
  checks `resource.<name>`, **not** `player.<id>`. Always check
  `IsPlayerAceAllowed(source, ...)` before running `ExecuteCommand` on a
  player's behalf.

## Checking permissions in Lua

Server scripts have two permission natives (see [lua-api.md](../lua-api.md)):

- `IsPlayerAceAllowed(player, object)` — checks `player.<id>` along with its
  `identifier.license:<hex>` and `identifier.ip:<address>` principals (and
  `builtin.everyone`). Works for connected players as well as connecting or
  deferred players inside `playerConnecting`. Returns `false` if the ID is not
  connected.
- `IsPrincipalAceAllowed(principal, object)` — checks any principal string
  (such as `'group.admin'` or `'resource.admin'`) directly against `object`.

Example server script combining a restricted command, a custom permission
object, and `ExecuteCommand`:

```lua
-- resources/admin/server.lua

-- Only players with `command.kickplayer` may run this command directly.
RegisterCommand('kickplayer', function(source, args)
    local target = tonumber(args[1])
    local reason = table.concat(args, ' ', 2)
    if not target or not GetPlayerName(target) then
        print('usage: kickplayer <id> [reason]')
        return
    end
    DropPlayer(target, reason ~= '' and reason or 'Kicked by an administrator')
end, true)

-- A network event must verify the caller explicitly.
RegisterNetEvent('admin:reloadResource', function(resourceName)
    local playerId = source
    if not IsPlayerAceAllowed(playerId, 'admin.reload') then
        print(('denied admin:reloadResource for player %d'):format(playerId))
        return
    end
    if type(resourceName) == 'string' and resourceName:match('^[%w_-]+$') then
        -- Requires `add_ace resource.admin command.restart allow` in server.cfg
        ExecuteCommand('restart ' .. resourceName)
    end
end)
```

Matching `server.cfg`:

```text
# Find a player's license with the `players` console command.
add_principal identifier.license:<64 hex chars> group.admin

add_ace group.admin command.kickplayer allow
add_ace group.admin admin.reload allow

# Allow the `admin` resource itself to run `restart` via ExecuteCommand.
add_ace resource.admin command.restart allow

ensure admin
```

## Testing and debugging with `test_ace`

Before letting players test permissions—or while a player is online—verify your
rules from the server console:

```text
list_principals
list_aces
test_ace group.admin command.kickplayer
test_ace resource.admin command.restart
test_ace identifier.license:0123...abcd admin.reload
```

When a player is connected (or deferred in `playerConnecting`), you can also
test their runtime player principal directly, including their automatic
`identifier.*` inheritance:

```text
players
test_ace player.1 command.kickplayer
```

`test_ace` prints `<principal> is allowed <object>` or
`<principal> is denied <object>` using the exact same evaluator that enforces
commands and `IsPlayerAceAllowed`.

## Further reading

- [server.md](../server.md#permissions) — complete server console and
  permission reference.
- [lua-api.md](../lua-api.md#players-server-only) — `IsPlayerAceAllowed`,
  `IsPrincipalAceAllowed`, and identifier natives.
- [First resource](first-resource.md) — walkthrough of creating a resource with
  commands and events.
