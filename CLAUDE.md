# SixM: notes for Claude

SixM is a FiveM-style multiplayer framework in Rust: a game-independent core
(server, resources, Lua runtime, networking, state bags, headless client),
with game adapters planned for later (GTA VI once it is on PC). The repository
is SixM-labs/SixM. Read README.md and CONTRIBUTING.md first.

## Rules

- **Clean room.** Never copy code from FiveM/Cfx.re or use game files. Concepts
  and FiveM API names are fine.
- **Windows and Linux.** Every change must pass on both; CI runs both. No
  platform-specific code without a `#[cfg]` counterpart.
- **Rust first, performance first.** Hot paths (per frame, event, packet) must
  not allocate or lock needlessly. No `unsafe`. No stubs, TODOs or dead code.
- **Checks before every commit:** `cargo fmt --all --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`. On Windows, also run the Linux checks in Docker
  (`rust:1-bookworm`, separate `CARGO_TARGET_DIR`).

## How we work

- Code is written by Haiku subagents from precise specs; the main session
  designs the contracts in `sixm-core`, reviews every result, fixes what review
  finds, and verifies on both systems.
- Work happens on feature branches with pull requests. Merge (merge commit)
  only when both CI jobs (`ubuntu-latest`, `windows-latest`) are green.
- Docs and code comments are in English; talk to the maintainer in German.

## Current work

`docs/design/fivem-parity/PLAN.md`: FiveM parity plan with checkpoints and a
handoff log shared with ChatGPT/Codex (`AGENTS.md`) and Gemini (`GEMINI.md`).

## Layout

`crates/sixm-core` (contract: values, Host/ServerHost/ClientHost,
ScriptRuntime), `sixm-resources` (fxmanifest, globs, dependencies), `sixm-lua`
(sandboxed Lua 5.4 runtime, FiveM API), `sixm-net` (QUIC transport, protocol,
pinning, file transfer), `sixm-kvp` (resource KVP store with a background
writer thread), `sixm-server`, `sixm-client`. Example server in
`examples/server-data`.

## Known gaps

- No game adapter yet; the client is headless.
