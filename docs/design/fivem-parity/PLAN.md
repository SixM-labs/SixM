# FiveM parity plan

Goal: a FiveM developer can port a typical resource (ESX, QBCore, ox_lib-style
scripts) to SixM with as few changes as possible and feels at home.

This file is the **single source of truth** for the work. It is written so that
any agent (Claude, ChatGPT/Codex, Gemini) can pick it up cold, continue at the
next open checkpoint and hand over again. The research behind it is in this
folder:

| File | Content |
|---|---|
| `gap-report.md` | Verified gaps, prioritized (P1/P2/P3, deferred, deliberate differences) |
| `sixm-inventory.md` | What SixM offered on 2026-11-27 (commit a752a53) |
| `native-coverage.tsv` / `native-coverage-summary.md` | Every CFX native: implemented yes/no, [core]/[game] |
| `sources/*.md` | Paraphrased catalogues of the FiveM docs (names + one-line descriptions) |

## 1. Rules for every agent

1. Read `CLAUDE.md` (project rules: clean room, Windows + Linux, no `unsafe`,
   no stubs/TODOs/dead code, hot paths without needless allocation or locks)
   and `CONTRIBUTING.md` before touching code. They override this file.
2. **Clean room.** Never open, copy or paraphrase FiveM/Cfx.re source code.
   Docs-level names, signatures and behavior descriptions are fine (that is
   what `sources/` contains).
3. Work on the phase branch named in the checkpoint (`feat/fivem-parity-<phase>`),
   branched from up-to-date `main`. One PR per phase. Merge with a merge commit
   only when both CI jobs (`ubuntu-latest`, `windows-latest`) are green.
4. **Every checkpoint ends with one commit** that contains the code, tests,
   docs, type stubs and the status update in this file (section 4 and 5).
   Commit message: `<checkpoint id>: <summary>`.
5. **Checks before every commit** (all must pass):
   ```
   cargo fmt --all --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   ```
   On Windows also run the Linux checks in Docker (`rust:1-bookworm`, separate
   `CARGO_TARGET_DIR`). On Linux, Windows is verified by CI on the PR.
6. **Every new script API** is added in Lua *and* JS (unless the checkpoint says
   otherwise), in `types/sixm.lua` and `types/sixm.d.ts`, in `docs/lua-api.md`
   / `docs/js-api.md`, and gets tests in the crate's `tests/`. User-visible
   changes get a line in `CHANGELOG.md` under "Unreleased".
7. FiveM names and argument order are kept exactly. Where SixM must differ,
   document it in `docs/guides/from-fivem.md`.
8. **Stopping (limit reached, end of session):** never leave uncommitted work.
   If a checkpoint is half done and the checks pass, commit it as
   `WIP <checkpoint id>: <what is done>`; if the checks fail, commit anyway as
   `WIP <checkpoint id> (checks failing): <what fails>` on the phase branch, push,
   and write the exact state in the handoff log (section 5). Push the branch.
9. Talk to the maintainer in German; code, comments and docs in English.
10. Decisions marked **DECISION** need the maintainer. Do not implement them
    before the answer is written into this file.

## 2. How to work (roles)

- **Claude Code:** main session plans and reviews; a Sonnet subagent reads the
  code and writes the precise spec for one checkpoint; Haiku subagents
  implement from that spec; a Sonnet subagent reviews the diff against the spec
  and `CLAUDE.md`; the main session runs the checks and commits.
- **ChatGPT/Codex and Gemini:** same discipline. If you can spawn sub-agents,
  use a cheap one to implement and a stronger one to review. If you cannot,
  write the spec into your own notes first, implement, then review your own
  diff against the spec and section 1 before committing.
- Specs name exact files, function signatures, error messages, edge cases and
  test cases. Look at how a neighbouring API is implemented (e.g. an existing
  native in the same file) and follow that pattern.

### Handoff order

Claude → ChatGPT/Codex → Gemini. Whoever takes over:
1. `git fetch && git switch <branch from section 5>` and `git pull`.
2. Read section 5 (last entry) and the open checkpoint.
3. Run the three checks once to confirm the starting state.
4. Continue; append your own entry to section 5 when you stop.

## 3. Phases and checkpoints

Effort: S under a day, M days, L weeks. IDs are stable; do not renumber.

### Phase A: quick wins for script authors (branch `feat/fivem-parity-a`)

**A1. Lua vector helpers** (S). `sixm-lua/src/vector.rs`. Add the CfxLua
global helpers that work on `vector2/3/4` and `quat` as described in
`sources/scripting-reference.md` / `sources/scripting-manual.md`: `vector(...)`
(picks vector2/3/4 by argument count), `dot(a, b)`, `cross(a, b)` (vector3),
`norm(v)` (length), `normalize`-style behavior where documented, `inv(q)` and
`slerp(a, b, t)` for quaternions only if the catalogue lists them. Verify each
name against the catalogue first; skip and note any name it does not list.
Wrong types raise a Lua error naming the function. JS already has
`.dot/.cross/.normalize`; no JS change. Tests in `sixm-lua/tests/`.

**A2. Legacy aliases** (S).
- Lua `Citizen.Trace(msg)` → same sink as `print` without the newline
  semantics of `print` (writes the string as is). `sixm-lua/src/natives.rs`.
- Lua `PerformHttpRequestAwait(url, method, data, headers, options)` → returns
  `status, body, headers, errorData` from inside a coroutine (built on the
  existing `PerformHttpRequest` + promise machinery); calling it outside a
  thread raises an error. `sixm-lua/src/http.rs`, `promise.rs`.
- JS `addEventListener(name, fn, netSafe?)` (= `on`, or `onNet` when
  `netSafe` is true), `addNetEventListener` (= `onNet`), `addRawEventListener`
  / `addRawEventHandler` (handler receives the raw msgpack payload as a
  `Uint8Array` plus `source`). `sixm-js/src/events.rs`.

**A3. State bag natives** (S). Lua and JS, server and client:
`GetStateBagValue(bagName, key)`, `SetStateBagValue(bagName, key, value, replicated)`
(server only; same validation as `state:set`), `StateBagHasKey(bagName, key)`,
`GetStateBagKeys(bagName)`, `EnsureEntityStateBag(entity)`. Bag names use the
same format the change handlers already deliver (`player:<id>`,
`entity:<id>`, `global`). Reuse the existing host trait methods; extend
`sixm-core` only if a method is missing. Unknown bag → `nil`/`undefined`,
not an error.

**A4. ACE and info natives** (S). Lua + JS:
`IsAceAllowed(object)` (checks the calling resource's principal
`resource.<name>`), `GetGameName()` (server: `"sixm"` unless a game adapter
says otherwise; client: adapter name or `"headless"`), `GetGameBuildNumber()`
(0 without adapter), `GetPlayerGuid(src)`, `GetPlayerTimeOnline(src)`
(seconds since the connection was accepted), `GetPlayerFromIndex(i)`,
`GetHostId()` (lowest connected player id or `nil`), `GetInstanceId()`
(stable per server process), `SetGameType(name)` / `SetMapName(name)` (set the
`gametype` / `mapname` convars, shown in `/info.json`),
`GetResourceCommands(resource)`. Document the exact SixM meaning of each in
the API docs.

**A5. Missing server events and arguments** (S).
- `onPlayerBucketChange(source, bucket, oldBucket)` and
  `onEntityBucketChange(entity, bucket, oldBucket)` from `sixm-server/src/buckets.rs`
  whenever the bucket actually changes.
- `onResourceListRefresh()` after `refresh`.
- `playerJoining(source, oldID)` second argument (temporary id from
  `playerConnecting`, as string, like FiveM).
- `playerDropped(reason, resourceName, clientDropReason)` extra arguments
  (`resourceName` = resource that called `DropPlayer`, else `""`;
  `clientDropReason` = numeric category, document the values).

**A6. Resource commands and `server_only`** (S).
- `start|stop|restart|ensure [category]` where `[category]` is a bracketed
  folder: applies to every resource under it, in dependency order (stop in
  reverse). `sixm-server/src/commands.rs`, `sixm-resources/src/catalog.rs`.
- `server_only 'yes'` manifest key: the resource is never offered to clients
  (no file list, no client scripts). `sixm-resources/src/manifest.rs` and the
  client file list builder in `sixm-server`. Document in `docs/manifest.md`.

**A7. `baseevents` with FiveM names** (S). `examples/server-data/resources/[system]/baseevents`.
Replace the SixM-specific names with the FiveM channel names:
`baseevents:onPlayerDied`, `baseevents:onPlayerKilled`,
`baseevents:onPlayerWasted`, `baseevents:enteringVehicle`,
`baseevents:enteringAborted`, `baseevents:enteredVehicle`,
`baseevents:leftVehicle`, with the FiveM argument lists from
`sources/ecosystem.md`. Update the game adapter doc if it references the old
names.

**A8. Phase A wrap-up.** Changelog, `docs/guides/from-fivem.md` updated,
PR `feat/fivem-parity-a` → `main`, CI green on both OS, merge.

### Phase B: stock resources with the FiveM contract (branch `feat/fivem-parity-b`)

**B1. `chat`** (M). Move to `resources/[system]/chat` with the FiveM event
contract: client events `chat:addMessage`, `chat:addTemplate`,
`chat:addSuggestion`, `chat:addSuggestions`, `chat:removeSuggestion`,
`chat:clear`; server event `chatMessage(source, author, text)` (cancelable with
`CancelEvent`, otherwise broadcast); `_chat:messageEntered` from clients;
`chat:init`; automatic suggestions from `GetRegisteredCommands`. The headless
client prints messages to its console and sends `say` text as
`_chat:messageEntered`. No UI (NUI comes with a game adapter).

**B2. `spawnmanager`** (M). Exports `spawnPlayer`, `addSpawnPoint`,
`removeSpawnPoint`, `loadSpawns`, `setAutoSpawn`, `setAutoSpawnCallback`,
`forceRespawn`; event `playerSpawned`. Spawning moves the player ped through
the existing entity API; with the `SimAdapter` this is observable in tests.

**B3. `mapmanager`** (M). Map/gametype resources (`resource_type 'map'` /
`'gametype'`), exports `getCurrentGameType`, `getCurrentMap`, `changeGameType`,
`changeMap`, `doesMapSupportGameType`, `getMaps`, `roundEnded`; events
`onClientMapStart`, `onClientMapStop`, `onClientGameTypeStart`,
`onClientGameTypeStop`, `onMapStart`, `onGameTypeStart`, `getMapDirectives`.
Uses `SetGameType` / `SetMapName` from A4.

**B4. Phase B wrap-up.** Example `server.cfg` ensures the new resources;
docs; PR; CI green; merge.

### Phase C: client-written state bags (branch `feat/fivem-parity-c`)

**C1. Protocol and server** (M). Clients may set replicated keys on their own
player bag and on entities they own; the server validates (owner, size limits
as for server writes, rate limit shared with `sv_playerEventRateLimit`) and
relays. New convar `sv_stateBagStrictMode` (default `false`, like FiveM): when
`true`, all client writes are rejected. Bump the protocol version; update
`docs/protocol.md`; fuzz target for the new message.

**C2. Script side** (S). `LocalPlayer.state:set(k, v, true)` and
`Entity(owned).state:set(k, v, true)` on clients go through C1 instead of
raising. Change handlers fire on server and other clients. Remove the
"clients cannot write replicated state" note from `from-fivem.md`.

**C3. Phase C wrap-up.** PR, CI, merge.

### Phase D: operators and developer experience (branch `feat/fivem-parity-d`)

**D1. Console commands** (S-M). `status`, `clientkick <id> [reason]`,
built-in `say <text>` (server broadcast via `chat` contract if running, else
log), `cmdlist`, `exec @resource/file.cfg`. `sixm-server/src/commands.rs`.

**D2. Console channel filters** (S). `con_channelFilters`,
`con_addChannelFilter <channel> <action>`, `con_removeChannelFilter`.

**D3. Live resource monitor (`resmon`)** (M). Console command and `/admin`
panel: per resource tick CPU ms (avg/max over a window), Lua/JS heap, event
count; hitch warnings in the log when a frame exceeds a convar threshold
(`sixm_hitchWarningMs`). Data must be collected without allocations on the
hot path (fixed ring buffers).

**D4. Proxy and metrics hardening** (S-M). `sv_proxyIPRanges` (trust
`X-Forwarded-For` / `X-Real-IP` only from listed CIDRs, used for rate limits
and `ip:` identifiers of HTTP clients); `sv_prometheusBasicAuthUser` /
`sv_prometheusBasicAuthPassword` for `/metrics`.

**D5. `deferrals.presentCard`** (M). Adaptive Card JSON from server to
connecting client, client answers with the submitted data, callback on the
server. Headless client prints the card and lets the user answer via its
console. Protocol bump.

**D6. HTTP handler request fields** (S). `req.address` and
`req.setCancelHandler(fn)` in `SetHttpHandler` for Lua and JS.

**D7. Phase D wrap-up.** PR, CI, merge.

### Phase E: database access (branch `feat/fivem-parity-e`)

The biggest blocker: without it ESX/QBCore cannot run.

**E1. ADR** (S). `docs/design/database.md`: crate `sixm-sql`; drivers MySQL/
MariaDB and SQLite (both must build on Windows and Linux without system
libraries; prefer pure-Rust or bundled); connection string convar
`mysql_connection_string` (oxmysql format, both URI and `key=value;` form);
pool on background threads, results delivered to the script runtime through
the existing async/promise path (no blocking the main loop); query parameter
binding only (no string interpolation); limits (row count, result size,
timeout convars). **DECISION:** which crates; SQLite on by default?

**E2. `sixm-sql` crate** (L). Implementation per ADR with tests against SQLite
(always) and MySQL (CI service container on Linux only, behind a feature or
env var so Windows CI still passes).

**E3. `oxmysql`-compatible resource** (M). `resources/[system]/oxmysql`
exporting the oxmysql API (`query`, `single`, `scalar`, `insert`, `update`,
`prepare`, `rawExecute`, `transaction`, `isReady`/`ready`, `.await` /
`Async` variants and `MySQL.*` Lua lib via `@oxmysql/lib/MySQL.lua`), plus
`provide 'mysql-async'` and `provide 'ghmattimysql'` with their API names.
Clean room: implement from the public API documentation only.

**E4. Phase E wrap-up.** Docs, example resource using the DB, PR, CI, merge.

### Phase F: moderation (branch `feat/fivem-parity-f`)

**F1. Ban and allowlist store** (M). Persistent bans (identifier, reason,
expiry, author) and optional allowlist, checked before `playerConnecting`
handlers; console `ban`, `unban`, `banlist`, `allowlist_add/remove`;
`TempBanPlayer(src, reason)` native.
**F2. Scheduled restarts and announcements** (S). Convars/commands; announce
via `chat` contract.
**F3. `/admin` panel** (M). Bans, allowlist, resmon (D3), log viewer.
**F4. Phase F wrap-up.** PR, CI, merge.

### Phase G: migration docs (can run any time; branch of the phase in progress)

**G1.** Add the 12 "not yet documented" items from `gap-report.md` section 5
to `docs/guides/from-fivem.md`, plus a table "FiveM convar/command → SixM
equivalent / no effect / absent".
**G2.** List manifest keys that are accepted but have no effect
(`gap-report.md` P3 item 5) in `docs/manifest.md`.

### Backlog (not scheduled)

- **DECISION — identifiers:** may `playerConnecting` handlers attach extra
  identifiers (`discord:`, `steam:` ...) verified by a resource? (gap-report
  P1 item 15)
- P3 items in `gap-report.md` (client core natives, function-reference
  internals, `GetPasswordHash`/`VerifyPasswordHash`, entity natives from P2
  item 9, CfxLua syntax extras from P2 item 7, server listing).
- Everything in gap-report section 4 waits for a game adapter (NUI, DUI,
  voice, key mapping, 196 server + 490 client game natives).

## 4. Status

| ID | Status | By | Commit (find with) |
|---|---|---|---|
| A1 | done | Claude (Haiku impl, Sonnet review) | `git log --grep "^A1:"` |
| A2 | done | Claude (Haiku impl, Sonnet review) | `git log --grep "^A2:"` |
| A3 | done | Claude (Haiku impl, Sonnet review) | `git log --grep "^A3:"` |
| A4 | done | Claude (Haiku impl, Sonnet review) | `git log --grep "^A4:"` |
| A5 | done | Claude (Haiku impl, Sonnet review) | `git log --grep "^A5:"` |
| A6 | done | Claude (Haiku impl), ChatGPT (Luna fixes, Terra review) | `git log --grep "^A6:"` |
| A7 | done | ChatGPT (Luna impl, Terra review) | `git log --grep "^A7:"` |
| A8 | todo | | |
| B1 | todo | | |
| B2 | todo | | |
| B3 | todo | | |
| B4 | todo | | |
| C1 | todo | | |
| C2 | todo | | |
| C3 | todo | | |
| D1 | todo | | |
| D2 | todo | | |
| D3 | todo | | |
| D4 | todo | | |
| D5 | todo | | |
| D6 | todo | | |
| D7 | todo | | |
| E1 | todo | | |
| E2 | todo | | |
| E3 | todo | | |
| E4 | todo | | |
| F1 | todo | | |
| F2 | todo | | |
| F3 | todo | | |
| F4 | todo | | |
| G1 | todo | | |
| G2 | todo | | |

Status values: `todo`, `in progress`, `wip (see log)`, `done`, `blocked (see log)`.

## 5. Handoff log

Newest entry last. Each entry: date, agent, branch, last commit, what is done,
what is next, anything surprising.

- 2026-11-27, Claude (Opus): research done, plan written. Branch
  `feat/fivem-parity-a` created from `main` (a752a53). Next: A1.
- 2026-11-28, Claude (Opus, main) with Sonnet (specs, review) and Haiku
  (implementation): A1–A4 done on `feat/fivem-parity-a`, each its own commit,
  full checks green on Linux. Specs for A1–A4 are in `specs/`. Notes:
  `SetStateBagValue` also accepts the FiveM msgpack form
  `(bag, key, data, length, replicated)`; `GetPlayerTimeOnline` counts from
  admission (after deferrals) and is nil while pending. Next: write specs
  A5–A7 (same format as `specs/A1.md`), implement, then A8 (PR).
- 2026-11-28, Claude (Opus, main): A5 done (Sonnet-reviewed). A6 implemented
  by Haiku from `specs/A6.md`, full checks green on Linux, but **not reviewed
  yet**: the next agent reviews the A6 commit against `specs/A6.md` and
  section 1 first (category resolution incl. nested `[a]/[b]`, start order by
  dependencies and reverse for stop, `server_only` never reaching clients),
  fixes findings in a follow-up commit `A6: review fixes`, then sets A6 to
  `done`. Spec `specs/A7.md` is written, not implemented. Next: A6 review,
  A7, A8 (PR `feat/fivem-parity-a` → `main`, CI green on both OS, merge),
  then Phase B (write specs `specs/B1.md`… first). Handing over to
  ChatGPT/Codex (Claude limit).
- 2026-11-28, ChatGPT, `feat/fivem-parity-a`: `A6: review fixes` follows
  d552314. Reviewed nested categories, dependency order and server-only client
  sync. Fixed duplicate membership for repeated category names, documented
  explicit cross-resource client file references and expanded client sync
  coverage. Next: A7.
- 2026-11-28, ChatGPT, `feat/fivem-parity-a`: `A7: register FiveM baseevents`
  follows c707449. The example registers seven FiveM channels and the
  loopback test verifies that the former vehicle channel names are no
  longer registered. Next: A8 and the Phase A pull request.
