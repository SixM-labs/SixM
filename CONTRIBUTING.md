# Contributing to SixM

Thanks for helping. This page explains the few rules that keep SixM clean,
fast and legally safe.

## The clean-room rule

SixM is a clean-room implementation. **Do not copy code from FiveM, RedM,
FXServer or any other Cfx.re or Rockstar project**, even though their source
is public. Most of it is not under an open-source license, and copied code
would put the whole project at risk.

What is fine:

- reimplementing concepts (resources, events, exports, state bags)
- keeping API names that FiveM developers know, such as `RegisterNetEvent`
  or `GlobalState`, so existing resources keep working
- reading public documentation to match behaviour

Never add game files, extracted assets or code taken from a game.

## Requirements for every change

- **Windows and Linux.** CI runs on both, and both must pass. Do not use
  platform-specific APIs without a `#[cfg]` counterpart for the other system.
- **Rust first.** New functionality is written in Rust. Lua is only for
  scripts that resources run, plus tiny shims where Rust cannot do the job.
- **Performance.** The main thread runs every resource. Avoid allocations
  and locks on hot paths (per frame, per event, per packet), and never block
  it on I/O.
- **No unsafe code.** The workspace forbids `unsafe`.
- **No dead code or placeholders.** No `todo!()`, stubs or commented-out
  code. Doc comments say something the code does not.

## Before you open a pull request

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Add tests for new behaviour and for every bug you fix. Network code gets
real loopback tests; Lua APIs get tests in `crates/sixm-lua/tests`.

Keep pull requests focused on one topic, and describe what changed and how
you tested it.

## Security

Report vulnerabilities privately, as described in [SECURITY.md](SECURITY.md),
not in issues or pull requests.

## License

By contributing, you agree that your contribution is licensed under the
[GNU General Public License v3.0](LICENSE), like the rest of SixM.
