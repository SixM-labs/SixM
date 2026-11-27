# SixM documentation

Reference pages for SixM. Each page describes what the code does today. Where
the code and a page disagree, the code is right.

- [manifest.md](manifest.md): every `fxmanifest.lua` directive, how globs and
  `@other/` references work, dependencies, exports, and the limits that keep a
  manifest from hanging the server.
- [lua-api.md](lua-api.md): every global a script can use, grouped by topic,
  with the side it exists on, the lifecycle events and the sandbox differences.
- [server.md](server.md): running `sixm-server`, `server.cfg`, console commands,
  convars with their defaults, the identity file and the fingerprint.
- [client.md](client.md): running the headless `sixm-client`, its arguments,
  console and convars, the cache layout and what client scripts can do.
- [protocol.md](protocol.md): an overview of the network protocol for tool
  authors. The format may change between protocol versions.
