# SixM documentation

Guides and reference pages for SixM. Each page describes what the code does
today. Where the code and a page disagree, the code is right.

## Guides

- [guides/first-resource.md](guides/first-resource.md): step-by-step guide to
  creating a resource (`fxmanifest.lua`, shared/server/client scripts, events,
  and state bags) and testing it with `sixm-server` and `sixm-client`.
- [guides/from-fivem.md](guides/from-fivem.md): migration and comparison guide
  for FiveM developers — what works the same, key differences, and what is
  missing without a game adapter.
- [guides/permissions.md](guides/permissions.md): ACE permissions with
  `add_ace`, `add_principal`, `resource.<name>` for `ExecuteCommand`,
  restricted commands, `IsPlayerAceAllowed`, and `test_ace`.
- [guides/deployment.md](guides/deployment.md): production deployment with
  `server.cfg`, identity key and fingerprint pinning, systemd on Linux, Windows
  service notes, TLS reverse proxy for HTTP, and firewall rules.

## Reference

- [manifest.md](manifest.md): every `fxmanifest.lua` directive, how globs and
  `@other/` references work, dependencies, exports, and the limits that keep a
  manifest from hanging the server.
- [lua-api.md](lua-api.md): every global a Lua script can use, grouped by topic,
  with the side it exists on, the lifecycle events and the sandbox differences.
- [js-api.md](js-api.md): every global and Promise/timer helper a JavaScript
  (`.js`) script can use on server and client, plus QuickJS sandbox limits.
- [server.md](server.md): running `sixm-server`, `server.cfg`, console commands,
  convars with their defaults, the identity file and the fingerprint.
- [client.md](client.md): running the headless `sixm-client`, its arguments,
  console and convars, the cache layout and what client scripts can do.
- [protocol.md](protocol.md): an overview of the network protocol for tool
  authors. The format may change between protocol versions.
- [design/game-adapter.md](design/game-adapter.md): the game adapter contract
  between the core and one game; Phase 1 (`GameAdapter`, `NativeRegistry`,
  `SimAdapter`, and client hooks) is implemented.
