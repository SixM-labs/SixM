# JavaScript Runtime (design)

Status: **implemented** in `crates/sixm-js`. This document records the original
design. For the supported API and current limits, see [JavaScript API](../js-api.md).

Clean room: this design uses public FiveM JavaScript API names and concepts only. It copies no FiveM or Cfx.re code.

## 1. Engine choice and sandbox model

### Engine: `rquickjs` 0.14

`crates/sixm-js` embeds QuickJS via `rquickjs = "0.14"` (vendored QuickJS C sources compiled by `rquickjs-sys 0.14.0` via `cc`):

```toml
# Workspace Cargo.toml
rquickjs = { version = "0.14", default-features = false, features = ["std", "macro"] }
```

Notes from verifying `rquickjs` 0.14.0 in this workspace:

- **Features.** In `rquickjs 0.14.0`, `classes` and `array-buffer` are enabled by default and marked deprecated as explicit feature flags (enabling them emits deprecation warnings under `-D warnings`). `futures` is **not** needed because SixM drives all runtimes synchronously on the host main thread via `ScriptRuntime::tick`, `run_timers`, and `execute_pending_job`. Do **not** enable `rust-alloc`, because `Runtime::set_memory_limit` is a no-op when a custom Rust allocator replaces QuickJS's allocator.
- **Linux build.** Verified compiling cleanly on `x86_64-unknown-linux-gnu` with the bundled QuickJS C sources (`libregexp.c`, `libunicode.c`, `quickjs.c`, `dtoa.c`).
- **Windows/MSVC build.** `rquickjs-sys 0.14.0` ships pre-generated bindings (`src/bindings/x86_64-pc-windows-msvc.rs` and `aarch64-pc-windows-msvc.rs`, plus `x86_64-unknown-linux-gnu.rs`), so neither `bindgen` nor `libclang` is needed at build time. In `rquickjs-sys/build.rs`, MSVC builds automatically pass `/DWIN32_LEAN_AND_MEAN /std:c11 /experimental:c11atomics` because `quickjs-c-atomics.h` includes `<stdatomic.h>`. This requires MSVC 19.35+ (Visual Studio 2022 17.5+, standard on GitHub Actions `windows-latest`).
- **Zero `unsafe` in SixM code.** Workspace `[workspace.lints.rust] unsafe_code = "forbid"` applies to `crates/sixm-js`. Verified that `rquickjs`'s safe wrappers plus `#[derive(Trace, JsLifetime)]`, `#[rquickjs::class]`, and `#[rquickjs::methods]` compile cleanly under `#![forbid(unsafe_code)]` and `clippy -F unsafe_code -D warnings`.

### Resource isolation and sandbox limits

Each running JS resource instance owns one dedicated `rquickjs::Runtime` and one `rquickjs::Context`. Resources never share a QuickJS `Runtime`, `Context`, or JS heap.

| Control | Value / Mechanism | Behavior on breach |
|---|---|---|
| Memory limit | `rt.set_memory_limit(64 * 1024 * 1024)` (64 MiB) | QuickJS throws `InternalError: out of memory` (`Err(Error::Exception)`), caught cleanly without aborting the host. |
| Call stack limit | `rt.set_max_stack_size(256 * 1024)` (256 KiB) | Deep recursion throws `InternalError: Maximum call stack size exceeded`. |
| CPU budget (interrupt handler) | `rt.set_interrupt_handler(...)` checking a `Cell<Option<Instant>>` deadline | QuickJS throws an **uncatchable** `InternalError: interrupted` (`JS_SetUncatchableError` in `quickjs.c`), so `try { while (true) {} } catch (e) {}` cannot escape. `libregexp` (`lre_check_timeout`) also checks the handler, preventing ReDoS. |

#### Re-entrant execution budget

`ScriptRuntime` methods are re-entrant (e.g., `emit` inside an event handler, or a cross-resource export calling back into the same resource). Each `JsRuntime` holds:

- `budget_deadline: Rc<Cell<Option<Instant>>>` shared with the `Runtime` interrupt closure: `move || deadline.get().is_some_and(|limit| Instant::now() >= limit)`.
- `BudgetGuard`: an RAII guard entered around every JS execution (`load_script`, `tick`, `run_timers`, `deliver_event`, `deliver_state_change`, `call_export`, `call_ref`, `call_web_handler`, and `drain_jobs`).
  - If `budget_deadline` is `None` (outermost entry), the guard sets `Some(Instant::now() + budget)` (`SCRIPT_LOAD_BUDGET = 500 ms` during `load_script`, `CALLBACK_BUDGET = 100 ms` during callbacks/ticks) and clears it back to `None` on `Drop`.
  - If `budget_deadline` is already `Some(_)` (re-entrant call), the guard leaves the outer deadline untouched so self-recursion cannot multiply CPU budget.

### Disabled dangerous globals and intrinsics

Instead of `Context::full`, `sandbox::new_context(&rt)` builds the context with `Context::builder()`:

- **Enabled intrinsics:** `intrinsic::Date`, `intrinsic::RegExpCompiler`, `intrinsic::RegExp`, `intrinsic::Json`, `intrinsic::Proxy`, `intrinsic::MapSet`, `intrinsic::TypedArrays`, `intrinsic::Promise`, `intrinsic::WeakRef`.
- **Omitted intrinsics:** `intrinsic::Eval` (leaves `ctx->eval_internal == NULL`, so `eval` is not defined and `new Function(...)` throws `EvalError: eval is not supported`) and `intrinsic::DOMException`.
- **Removed from `globalThis`:** `SharedArrayBuffer` and `Atomics` (installed by `JS_AddIntrinsicTypedArrays`, removed because they expose high-resolution timing primitives and shared-memory APIs irrelevant to single-threaded scripts) and `eval` (if present).
- **Absent by default in QuickJS bare contexts:** `require`, `process`, `fetch`, `XMLHttpRequest`, `WebAssembly`, `Intl`, `std`, `os`, `import`, filesystem/network/subprocess APIs.
- **Installed safe output globals:** `print(...args)` and `console` (`log`, `info`, `warn`, `error`, `debug`), formatting values and logging through `tracing` with `target: "sixm::script"` and `[<resource>]` prefix.

### Microtask job queue draining and unhandled rejections

QuickJS enqueues `Promise.then` callbacks and `async`/`await` continuations into its per-runtime job queue rather than running them inline.

- `JsRuntime::drain_jobs(&self)` loops `while self.rt.is_job_pending()` and calls `self.rt.execute_pending_job()` under the active `BudgetGuard`:
  - Returns `Ok(true)` -> continue draining.
  - Returns `Ok(false)` -> queue empty, stop.
  - Returns `Err(job_exc)` -> `job_exc.0.with(|ctx| format_caught_exception(&ctx))` logs the error (`tracing::error!(target: "sixm::script", ...)`) and continues draining remaining jobs so one failing continuation does not starve unrelated promises.
  - If the `BudgetGuard` trips while draining an infinite microtask loop (`Promise.resolve().then(spin)`), `drain_jobs` logs the interruption and stops draining for this call.
- `drain_jobs()` runs at the end of `load_script`, `deliver_event`, `deliver_state_change`, `call_export`, `call_ref`, `call_web_handler`, `tick`, and `run_timers`.
- **Unhandled rejection tracking:** `rt.set_host_promise_rejection_tracker(...)` records rejected promises whose `is_handled == false` (and removes them if later handled with `is_handled == true`). At the end of `tick` (matching `sixm-lua/src/promise.rs`), any rejection still unhandled from the previous frame is logged once via `tracing::warn!`.

### Error and stack formatting

Every call into QuickJS wraps errors using `rquickjs::CatchResultExt::catch(&ctx)`:

- On `CaughtError::Exception(exc)`: format `exc.to_string()` (or `message` + `stack`, which includes `@<resource>/<relative_path>:<line>:<col>` from `EvalOptions { filename: Some(chunk_name), .. }`).
- On `CaughtError::Value(val)`: convert thrown primitive/object to a readable string (never leaving an uncaught exception pending on `ctx`).
- On `CaughtError::Error(err)`: `err.to_string()`.
- Map into `ScriptError::Runtime(formatted)` so `ResourceManager`, `ClientRuntime`, and cross-runtime `exports` callers see full JS stack traces rather than `"Exception generated by QuickJS"`.

### Shutdown and `Persistent` lifecycle invariant

Experimentally verified against QuickJS in `rquickjs 0.14.0`: `JS_FreeRuntime` asserts `list_empty(&rt->gc_obj_list)` if any `Persistent<T>` handle outlives `Context` or if a Rust closure stored in QuickJS holds an `Rc` back to a `Persistent` handle (which QuickJS's cycle collector cannot trace across Rust opaque objects).

`JsRuntime` enforces two strict rules:

1. **No strong cycles through JS closures.** Every Rust function installed via `Function::new` or `ProxyHandler` captures only `Weak<JsRuntime>` (never `Rc<JsRuntime>` and never `Rc<RefCell<...Persistent...>>`).
2. **Ordered teardown in `clear_state()` (called on `shutdown()` and `Drop`).**
   1. Drain and drop all `Persistent` tables (`sched`, `events`, `state`, `exports`, `refs`, `rejections`) outside any `RefCell` borrow.
   2. Clear properties on `ctx.globals()` inside `ctx.with(...)`.
   3. Call `self.rt.run_gc()`.
   4. Clear `self.rt.set_interrupt_handler(None)` and `self.rt.set_host_promise_rejection_tracker(None)`.

## 2. Multi-runtime resource architecture and cross-runtime interop

### Crate layout: `crates/sixm-js`

`crates/sixm-js` mirrors `crates/sixm-lua` in structure and implements `sixm_core::ScriptRuntime` on `pub struct JsRuntime`.

```
crates/sixm-js/
├── Cargo.toml
├── src/
│   ├── lib.rs         # pub use runtime::JsRuntime
│   ├── runtime.rs     # JsRuntime, Side, BudgetGuard, ScriptRuntime impl, drain_jobs, shutdown
│   ├── sandbox.rs     # ContextBuilder, intrinsic trimming, print/console, exception formatting
│   ├── convert.rs     # JsValue <-> sixm_core::Value, depth limit (64), FuncRef round-trip
│   ├── refs.rs        # RefTable (Persistent<Function> + pointer dedup + sweep) & foreign FuncRef wrapper
│   ├── scheduler.rs   # setTick/clearTick, setTimeout/setInterval, Wait/Delay promises, deadlines
│   ├── events.rs      # on/onNet/emit/emitNet/AddEventHandler/CancelEvent/source
│   ├── exports.rs     # callable Proxy `exports`, local registry, manifest fallback, invoke_export
│   ├── state.rs       # StateBag Proxy, GlobalState, AddStateBagChangeHandler
│   ├── vector.rs      # Vector2/3/4 class (.x/.y/.z/.w + [0..n] + [Symbol.iterator])
│   ├── natives.rs     # Host/convar/command/hash/GetGameTimer/Citizen assembly
│   ├── resources.rs   # GetCurrentResourceName, LoadResourceFile, SaveResourceFile, metadata
│   ├── kvp.rs         # SetResourceKvp*, GetResourceKvp*, StartFindKvp/FindKvp/EndFindKvp
│   ├── players.rs     # Server-only player natives & Player(id).state
│   ├── entities.rs    # Entity natives & server routing buckets & client GetGamePool
│   ├── client.rs      # Client-only PlayerId/GetPlayerServerId/PlayerPedId/LocalPlayer
│   ├── http.rs        # Server-only PerformHttpRequest
│   └── web.rs         # Server-only SetHttpHandler, req/res JS objects
└── tests/             # Mirror of crates/sixm-lua/tests/*
```

### `fxmanifest.lua` script dispatch (`.lua` vs `.js` and mixed resources)

`sixm-resources` (`Manifest`, `ResourceInfo::server_script_files`, `ResourceInfo::client_files`) already expands `shared_scripts`, `server_scripts`, and `client_scripts` without filtering by extension, and `sixm-net` already syncs all `SharedScript` and `ClientScript` files to clients regardless of extension. **No changes to `sixm-resources` or `sixm-net` are needed.**

Instead of forcing a resource to pick one language, both `ResourceManager` (`crates/sixm-server/src/manager.rs`) and `ClientRuntime` (`crates/sixm-client/src/runtime.rs`) support **per-script language dispatch** inside each resource:

```rust
struct RunningResource {
    name: ResourceName,
    runtimes: Vec<Rc<dyn ScriptRuntime>>,
}
```

Alongside `running: RefCell<Vec<RunningResource>>`, both hosts maintain the existing flat `snapshot: RefCell<Rc<[Rc<dyn ScriptRuntime>]>>` for zero-allocation hot-path iteration.

During `start_one` (server) and `start_resource` (client):

1. Iterate the resource's expanded script paths in manifest load order (`shared_scripts`, then `server_scripts` or `client_scripts`).
2. Dispatch by file extension (ASCII case-insensitive):
   - `.lua`: lazily create a `LuaRuntime` (with a fresh `instance` from `next_instance`) and call `lua.load_script(path)`.
   - `.js`: lazily create a `JsRuntime` (with a fresh `instance` from `next_instance`) and call `js.load_script(path)` in classic global script mode (`EvalOptions { global: true, strict: false, filename: Some(chunk_name), .. }`, so multiple `.js` files in the same resource share globals just like multiple `.lua` files do, and `'use strict'` remains opt-in per file).
   - Any other extension: log a warning (`unsupported script extension`) and skip.
3. A pure `.lua` resource creates only a `LuaRuntime`; a pure `.js` resource creates only a `JsRuntime`; a mixed `.lua` + `.js` resource creates both, each with its own `instance` ID, and executes scripts in exact manifest order.
4. A resource with zero scripts on this side (e.g., `files`-only or client-only resource) still records `RunningResource { name, runtimes: Vec::new() }` so `is_running(name)` and `GetResourceState(name)` remain `Started`.

### Cross-runtime `Host` dispatch

Because `LuaRuntime` and `JsRuntime` in a mixed resource receive **distinct `instance` IDs**:

- **`FuncRef` routing (`call_ref`, `retain_ref`, `release_ref`):** `FuncRef { owner, instance, id }` uniquely identifies the exact runtime instance (`LuaRuntime` or `JsRuntime`) via `runtime_of(func)` searching `snapshot`. Cross-runtime callbacks (Lua passing a function to a JS export, JS passing a closure to `PerformHttpRequest`, Lua receiving `deferrals`, etc.) work with zero changes to `FuncRef` or `Host`.
- **`exports` (`call_export`):** `Host::call_export(resource, export, args, invoker)` looks up `RunningResource` for `resource` and tries each runtime in `runtimes` order. If a runtime returns `Err(ScriptError::NoSuchExport { .. })`, the next runtime is tried; any `Ok` or other `Err` is returned immediately. Thus:
  - Lua calling JS `exports['js_res'].fn(...)` works.
  - JS calling Lua `exports['lua_res'].fn(...)` works.
  - Even within a mixed resource, `exports[GetCurrentResourceName()].fn(...)` reaches whichever runtime defines `fn`.
- **Events (`trigger_event`, `deliver_event`) & state bags (`deliver_state_change`):** `dispatch_from` and `deliver_state` iterate the flat `snapshot`. A single `TriggerEvent` / `emit` shares one `EventContext` across both Lua and JS handlers, so `CancelEvent()` in JS is visible to `WasEventCanceled()` in Lua (and vice versa), and `GetInvokingResource()` works identically across languages.
- **Per-resource lifecycle & HTTP:** `running_resources()` / `running_names()`, `stop_runtime`, and `info_document` (`/info.json` in `crates/sixm-server/src/web.rs`) iterate `running` (`RunningResource`) rather than `snapshot`, ensuring mixed resources appear once in `/info.json`, fire `onResourceStart` / `onResourceStop` once per resource, and have `http.discard(&name, rt.instance())` + `rt.shutdown()` called for every runtime in the resource.

### `sixm_core::Value` <-> JS value mapping

All cross-resource data travels through `sixm_core::Value` (max nesting depth `MAX_DEPTH = 64` -> `ScriptError::TooDeep(64)`). Cycle detection in `js_to_value` uses either the depth counter or an active ancestor pointer set (checking `depth > 64` guarantees termination even on cyclic JS objects).

| `sixm_core::Value` | JS -> `Value` | `Value` -> JS |
|---|---|---|
| `Value::Nil` | `null`, `undefined` | `null` (state bag missing key read returns `undefined`) |
| `Value::Bool(b)` | `boolean` | `boolean` |
| `Value::Int(i)` | `number` when `n.is_finite() && n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0`; `BigInt` if fits in `i64` | `number` (when `abs <= 9_007_199_254_740_991`), else `number` (or `i32`/`f64` via `rquickjs`) |
| `Value::Float(f)` | `number` (non-integer, `NaN`, or `±Infinity`; note `NaN`/`Infinity` rejected by JSON/entity natives where appropriate) | `number` |
| `Value::String(bytes)` | `string` (UTF-8 bytes) or `Uint8Array` / `ArrayBuffer` (raw bytes) | Valid UTF-8 -> `string`; non-UTF-8 bytes -> `Uint8Array` |
| `Value::Array(items)` | JS `Array` (0-indexed `0..len`) | JS `Array` (0-indexed; converts cleanly to/from Lua 1-based sequences) |
| `Value::Map(pairs)` | Plain JS `Object` (own enumerable keys) | Plain JS `Object` (keys converted to JS property strings) |
| `Value::Vector2/3/4` | `Vector` class instance | `Vector` class instance |
| `Value::FuncRef(f)` | JS `Function` -> `ref_function` (or unwrapped original `FuncRef` if tagged with private `Symbol`); refused when `Functions::Refuse` (network events, state bags) | Local owner -> original JS `Function`; foreign owner -> callable JS `Function` holding a retained `FuncHandle` guard (`Drop` calls `host.release_ref`) + private `Symbol` storing `FuncRef` |
| Disallowed JS types | `Promise`, `Symbol`, `Map`/`Set` (unless plain object? refuse with clear error), non-`Vector` class/userdata -> `ScriptError::Runtime` | — |

#### Cross-runtime `exports` return convention

`ScriptRuntime::call_export` returns `Result<Vec<Value>, ScriptError>` because Lua functions support multiple return values:

- **JS calling Lua export (`Vec<Value>` -> JS):**
  - `0` values -> `undefined`
  - `1` value `[v]` -> `v`
  - `>1` values `[v0, v1, ...]` -> JS `Array` `[v0, v1, ...]` (preserves all Lua return values without silent truncation)
- **Lua calling JS export (JS return -> `Vec<Value>`):**
  - `undefined` -> `vec![]`
  - any other value `v` (including `null` or a JS `Array`) -> `vec![to_value(v)]`

## 3. Complete FiveM JS API surface

All APIs below match `sixm-lua` (`docs/lua-api.md`) through `Host`, `ServerHost`, and `ClientHost`, while supporting FiveM's idiomatic JS names alongside the PascalCase FiveM native names.

### Scheduling, timers, and `Wait`

| JS Global / Method | Side | Behavior |
|---|---|---|
| `setTick(fn)` / `Citizen.setTick(fn)` | both | Schedules `fn()` to run every frame in `tick(now)`. Returns numeric tick id. Uncaught errors are logged with stack trace and the tick remains active until cleared. |
| `clearTick(id)` / `Citizen.clearTick(id)` | both | Stops a tick registered by `setTick`. |
| `setTimeout(fn, ms, ...args)` / `Citizen.setTimeout` | both | Standard JS argument order `(fn, ms)`. Runs `fn(...args)` once after `ms`. Returns numeric timer id. |
| `clearTimeout(id)` / `Citizen.clearTimeout` | both | Cancels a pending timeout. |
| `setInterval(fn, ms, ...args)` / `Citizen.setInterval` | both | Standard JS repeating timer. Runs `fn(...args)` every `ms` until cleared. Returns numeric id. |
| `clearInterval(id)` / `Citizen.clearInterval` | both | Cancels a pending interval. |
| `SetTimeout(ms, fn)` / `ClearTimeout(id)` | both | FiveM Lua-order aliases (`(ms, fn)`) for compatibility. |
| `Wait(ms)` / `Delay(ms)` / `Citizen.Wait(ms)` | both | Returns a `Promise<void>`. Positive `ms` resolves when its deadline passes in `run_timers`/`tick` (affecting `next_deadline()`); `<= 0` or omitted/non-number resolves on the next frame (`tick`). |
| `GetGameTimer()` | both | Milliseconds since the process first queried the game timer (`i64`). |

**Context propagation across `Wait` / timers:** Creating a `setTick`, `setTimeout`, `setInterval`, or `Wait` captures the current `EventSource` and `current_invoker`, restoring `globalThis.source` and `current_invoker` when the callback runs or the `Wait` promise resolves (matching `sixm-lua`).

### Events

| JS Global | Side | Behavior |
|---|---|---|
| `on(name, fn)` / `AddEventHandler(name, fn)` | both | Registers local handler. Returns handle `{ key, name }`. |
| `RemoveEventHandler(handle)` | both | Removes handler by `{ key, name }`. |
| `removeEventListener(name, fn)` | both | Removes matching handler by `(name, fn)` identity. |
| `onNet(name [, fn])` / `RegisterNetEvent` / `RegisterServerEvent` | both | Marks `name` as network-safe and optionally registers `fn`. |
| `emit(name, ...args)` / `TriggerEvent(name, ...args)` | both | Synchronous local dispatch via `Host::trigger_event`. Restores `globalThis.source` afterwards. |
| `CancelEvent()` / `WasEventCanceled()` | both | Sets/reads cancellation flag on innermost dispatch (propagated across Lua and JS via `EventContext`). |
| `emitNet(name, target, ...args)` / `TriggerClientEvent` | server | Sends event to player `target` or `-1` (`ClientTarget::All`). Rejects functions. |
| `TriggerLatentClientEvent(name, target, bps, ...args)` | server | Paced client event. |
| `emitNet(name, ...args)` / `TriggerServerEvent` | client | Sends event to server. Rejects functions. |
| `TriggerLatentServerEvent(name, bps, ...args)` | client | Paced server event. |
| `source` (`globalThis.source`) | both | `""` for `Local`, numeric player id for `Net`/`Player`, `65535` for `Server`. |

`playerConnecting(name, setKickReason, deferrals)` works with zero special cases: `setKickReason` arrives as a callable JS `Function`, and `deferrals` arrives as `{ defer, update, done }` whose properties are callable JS `Function`s backed by `FuncRef`.

### Exports

- `exports` is a callable ES6 `Proxy` (target is a dummy JS `Function`, so `typeof exports === 'function'`):
  - `apply` trap: `exports(name, fn)` registers a local export `name`.
  - `get` trap: `exports[resource]` returns a cached per-resource `Proxy` whose `get(_, exportName)` trap returns a callable function invoking `host.call_export(resource, exportName, args, Some(&rt.resource))`.
- Manifest `server_export` / `export` fallback: if `exports(name, fn)` was not called, `find_export` checks `globalThis[name]` for a `Function` when `name` is in `declared_exports`.
- `GetInvokingResource()`: returns `string | null`.

### State bags (via `Proxy`)

- `StateBag` is created as an ES6 `Proxy` over a small `#[rquickjs::class]` target holding `{ bag: String, runtime: Weak<JsRuntime> }`:
  - `get(target, prop)`: if `prop === "set"`, returns `(key, value, replicated = true) => state_set(...)`; if `prop` is a `Symbol`, handles cleanly without converting to string; otherwise calls `host.state_get(&bag, &key)`, returning `undefined` if unset.
  - `set(target, prop, value)`: converts `value` with `Functions::Refuse`, calls `host.state_set(&bag, &key, value, true)`, returns `true`.
  - `deleteProperty(target, prop)`: calls `host.state_set(&bag, &key, Value::Nil, true)`, returns `true`.
  - `has(target, prop)`: returns `host.state_get(&bag, &key).is_some()`.
- Globals:
  - `GlobalState` (`GLOBAL_BAG`), `AddStateBagChangeHandler(key, bag, fn)` (`null`/`undefined` filters act as wildcards, returns numeric cookie), `RemoveStateBagChangeHandler(cookie)`, `GetPlayerFromStateBagName(bagName)`.
  - Server: `Player(id)` -> `{ state: StateBagProxy }`.
  - Client: `LocalPlayer` -> `{ state: StateBagProxy }`.
  - Both: `Entity(id)` -> `{ state: StateBagProxy }`.
- Change handler signature: `(bagName, key, value, reserved, replicated)` with `reserved = 0` and `value = null` on delete.

### Convars, commands, players, entities, routing buckets, KVP, HTTP, resources, vectors, hashes

All remaining `sixm-lua` natives are exposed with identical semantics and validation:

- **Convars & commands:** `GetConvar(name, default)`, `GetConvarInt(name, default)`, `GetConvarBool(name, default)`, `ExecuteCommand(line)`, `RegisterCommand(name, fn, restricted = false)`; server-only `SetConvar`, `SetConvarReplicated`, `SetConvarServerInfo`.
- **Players (server):** `getPlayers()` / `GetPlayers()` (returns `string[]`), `GetNumPlayerIndices()`, `GetPlayerName`, `GetPlayerPing`, `GetPlayerPed`, `GetPlayerEndpoint`, `GetPlayerIdentifiers`, `GetNumPlayerIdentifiers`, `GetPlayerIdentifier`, `GetPlayerIdentifierByType`, `DropPlayer`, `IsPlayerAceAllowed`, `IsPrincipalAceAllowed`, `Player(id)`.
- **Players (client):** `PlayerId()` (`0`), `GetPlayerServerId(player)`, `PlayerPedId()`, `GetPlayerPed(player)`, `LocalPlayer`.
- **Entities (both):** `DoesEntityExist`, `GetEntityCoords`, `GetEntityHeading`, `GetEntityRotation`, `GetEntityModel`, `GetEntityType`, `NetworkGetNetworkIdFromEntity`, `NetworkGetEntityFromNetworkId`, `NetworkDoesNetworkIdExist`, `GetEntityFromStateBagName`, `Entity`, `SetEntityCoords`, `SetEntityCoordsNoOffset`, `SetEntityHeading`, `SetEntityRotation`.
- **Entities & buckets (server):** `CreateVehicle`, `CreateVehicleServerSetter`, `CreatePed`, `CreateObject`, `CreateObjectNoOffset`, `DeleteEntity`, `NetworkGetEntityOwner`, `GetAllVehicles`, `GetAllPeds`, `GetAllObjects`, `SetPlayerRoutingBucket`, `GetPlayerRoutingBucket`, `SetEntityRoutingBucket`, `GetEntityRoutingBucket`, `GetPlayersInRoutingBucket`, `SetRoutingBucketPopulationEnabled`, `GetRoutingBucketPopulationEnabled`, `SetRoutingBucketEntityLockdownMode`, `GetRoutingBucketEntityLockdownMode`.
- **Entities (client):** `GetGamePool('CVehicle' | 'CPed' | 'CObject')`.
- **Vectors & hashes:** `vector2`/`vec2`, `vector3`/`vec3`, `vector4`/`vec4`, `GetHashKey(name)`.
  - `Vector` is a `#[rquickjs::class]` wrapped in a `Proxy` (or implementing indexed getters/setters + `[Symbol.iterator]` + `.x/.y/.z/.w` + `.length` + `.toString()` + `.toJSON()`), so both FiveM Lua-style (`v.x`, `v.xy`, `vector3(1, 2, 3)`) and FiveM JS-style destructuring/indexing (`const [x, y, z] = GetEntityCoords(ped); v[0]`) work on the same value and preserve `Value::Vector2/3/4` across resource boundaries.
- **KVP (both):** `GetResourceKvpString`, `GetResourceKvpInt`, `GetResourceKvpFloat`, `SetResourceKvp`, `SetResourceKvpNoSync`, `SetResourceKvpInt`, `SetResourceKvpIntNoSync`, `SetResourceKvpFloat`, `SetResourceKvpFloatNoSync`, `DeleteResourceKvp`, `DeleteResourceKvpNoSync`, `StartFindKvp`, `FindKvp`, `EndFindKvp`, `FlushResourceKvp`.
- **HTTP (server):**
  - `PerformHttpRequest(url, cb, method, data, headers, options)` (accepts `string` or `Uint8Array`/`ArrayBuffer` for `data`).
  - `SetHttpHandler(fn)` passing `(req, res)` where `req` exposes `{ method, path, query, headers, setDataHandler(cb) }` and `res` exposes `{ writeHead(status, headers), write(data), send(data) }` (supporting both `string` and `Uint8Array`/`ArrayBuffer` response chunks).
- **Resources & misc (both):** `GetCurrentResourceName`, `GetResourcePath`, `LoadResourceFile`, `SaveResourceFile`, `GetInvokingResource`, `GetResourceState`, `GetNumResources`, `GetResourceByFindIndex`, `GetResourceMetadata`, `GetNumResourceMetadata`, `IsDuplicityVersion`, `print`, `console`, native `JSON.stringify`/`JSON.parse`.

## 4. Implementation plan (4 chunks)

### Chunk 1: Core crate skeleton, sandbox, value bridge, scheduler, events, exports

- **Scope:**
  - `sixm-core/src/runtime.rs`: add `fmt::Debug` supertrait to `ScriptRuntime` (`pub trait ScriptRuntime: fmt::Debug`).
  - `sixm-core/src/lib.rs` (optional small helper): extract `pub fn game_timer_ms() -> i64` so `sixm-lua` and `sixm-js` share one monotonic clock.
  - Root `Cargo.toml`: add `rquickjs = { version = "0.14", default-features = false, features = ["std", "macro"] }` and `sixm-js = { path = "crates/sixm-js" }` to `[workspace.dependencies]`.
  - New crate `crates/sixm-js`: `lib.rs`, `runtime.rs`, `sandbox.rs`, `convert.rs`, `refs.rs`, `scheduler.rs`, `events.rs`, `exports.rs`, `vector.rs`.
- **Verification:**
  - Unit & integration tests in `crates/sixm-js/tests/`: `sandbox.rs` (OOM limit, stack overflow, infinite loop interrupt, ReDoS interrupt, disabled `eval`/`Function`/`SharedArrayBuffer`/`Atomics`/`require`), `scheduler.rs` (`setTick`/`clearTick`, `setTimeout`/`clearTimeout`, `setInterval`/`clearInterval`, `Wait`/`Delay` with promises, `next_deadline`, `run_timers`), `promises.rs` (microtask draining, unhandled rejection warning), `events.rs` (`on`/`onNet`/`emit`/`CancelEvent`/`source`), `exports.rs` (local/manifest exports, callable Proxy, `FuncRef` round-trip and GC release), `vectors.rs`, `lifecycle.rs` (clean `shutdown` with active `Persistent` refs/timers, no QuickJS GC assertion failure).

### Chunk 2: Full FiveM JS API surface

- **Scope:**
  - `crates/sixm-js/src/`: `state.rs`, `natives.rs`, `resources.rs`, `kvp.rs`, `players.rs`, `entities.rs`, `client.rs`, `http.rs`, `web.rs`.
- **Verification:**
  - Integration tests in `crates/sixm-js/tests/` matching `crates/sixm-lua/tests/`: `state.rs` (`GlobalState` Proxy get/set/delete/has, `bag.set`, `AddStateBagChangeHandler`, `Player(id).state`, `Entity(id).state`, `LocalPlayer.state`), `players.rs`, `entities.rs`, `buckets.rs`, `kvp.rs`, `resources.rs`, `http.rs`, `web.rs`, `sides.rs` (server-only vs client-only global isolation).

### Chunk 3: Multi-runtime integration in `sixm-server` and `sixm-client`

- **Scope:**
  - `crates/sixm-server/Cargo.toml` & `crates/sixm-client/Cargo.toml`: depend on `sixm-js.workspace = true`.
  - `crates/sixm-server/src/manager.rs`, `crates/sixm-server/src/web.rs`:
    - Replace `Vec<Rc<LuaRuntime>>` with `Vec<RunningResource>` + `snapshot: RefCell<Rc<[Rc<dyn ScriptRuntime>]>>`.
    - Dispatch `.lua` vs `.js` scripts in `start_one` in manifest order; support mixed `.lua` + `.js` resources with distinct `instance` IDs.
    - Update `call_export` to probe all runtimes of the target resource; update `stop_runtime`, `running_resources`, and `web.rs::info_document` to operate per resource.
  - `crates/sixm-client/src/runtime.rs` & `crates/sixm-client/src/host.rs`:
    - Mirror `RunningResource` + `snapshot: RefCell<Rc<[Rc<dyn ScriptRuntime>]>>`, dispatching `.lua` vs `.js` in `start_resource` and updating `call_export` / `stop_runtime` / `running_names`.
- **Verification:**
  - `crates/sixm-server/tests/js_interop.rs`:
    - Pure JS resource start/stop/restart, `SetHttpHandler`, `PerformHttpRequest`, KVP persistence.
    - **Cross-runtime Lua <-> JS tests:**
      - Lua calls JS export (`exports['js_res'].compute(...)`) and JS calls Lua export (`exports['lua_res'].compute(...)`), including multi-return Lua export, `Vector3`, nested objects/arrays, and `GetInvokingResource()`.
      - Lua passes a callback function to a JS export (and vice versa), JS invokes it synchronously and asynchronously from `setTimeout`/`Wait`, and verifies `release_ref` after GC.
      - Cross-runtime `TriggerEvent` / `emit` with `CancelEvent()` / `WasEventCanceled()` across Lua and JS resources, plus `GlobalState` writes in Lua firing `AddStateBagChangeHandler` in JS (and vice versa).
      - Mixed resource with both `shared.lua` + `server.js` in one `fxmanifest.lua`, verifying `onResourceStart` fires once, `/info.json` lists the resource once, and both runtimes shut down cleanly on `stop`.
    - **End-to-end client/server JS test** in `crates/sixm-server/tests/client_scripts.rs` (or `js_interop.rs`): server + headless `ClientRuntime` syncing `.js` `client_script` & `shared_script`, exchanging `emitNet` / `TriggerClientEvent` / `TriggerServerEvent`, client exports, and replicated state bags.

### Chunk 4: Documentation and final cross-platform verification

- **Scope:**
  - `docs/js-api.md` (documenting the JS API surface, differences from Lua such as `Wait` returning a `Promise`, `setTimeout(fn, ms)` vs `SetTimeout(ms, fn)`, `JSON`, and disabled globals).
  - `docs/manifest.md`, `docs/server.md`, `docs/client.md`, `README.md`: update references to Lua-only scripting where JS is now supported.
  - Add a minimal JS example resource (or check `examples/server-data`) if desired.
- **Verification:**
  - `cargo fmt --all --check`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo test --workspace`
  - Verify both `ubuntu-latest` and `windows-latest` CI workflows pass (especially MSVC C11 atomics compilation of `rquickjs-sys`).
