# Game adapters (design)

Status: **Phase 1 implemented** (`sixm-core::adapter`, `sixm-client::SimAdapter`,
`sixm-client` mirror/tick hooks, and Lua/JS `InvokeNative`). Phase 2 (a real
game crate) waits on a PC release with a supported interface. Where the code
and this page disagree about something that exists, the code is right.

## 1. Goal and non-goals

Goal:

- Define the boundary between the game-independent core and one game, so that
  a game can be added without changing the server, the protocol, resources or
  the Lua runtime.
- Keep the boundary small: one Rust trait, implemented once per game, called
  from one place in the client.
- Make the boundary testable without a game, by allowing an adapter that
  renders nothing.

Non-goals:

- No adapter code is written now. No `GameAdapter` trait is added to
  `sixm-core` until a first implementation exists to check it against.
- No game-specific knowledge in the core: no model lists, no native tables, no
  game files, no memory layouts.
- No input, UI, audio or rendering design. These are marked as later below.
- No server-side adapter. The server stays game-independent. Anything a game
  needs from the server is an entity, a state bag or an event.
- No compatibility promise with existing FiveM resources beyond the public API
  names the Lua runtime already uses.

Clean room: this design uses concepts and public FiveM API names only. It uses
no FiveM or Cfx.re code, no game files and no reverse-engineering detail of any
game.

## 2. What the core already provides

Everything below exists today. Names are the real ones.

### Entity mirror

The server owns every entity. Each client keeps a copy, the mirror. In
`sixm-core/src/entity.rs`:

- `EntityKind`: `Ped`, `Vehicle`, `Object`, with `code()` and `from_code()`
  (the `GetEntityType` codes 1, 2, 3).
- `EntityInfo`: `kind`, `model` (a `u32` hash, as `GetHashKey` computes it),
  `position` (`[f32; 3]`, metres), `rotation` (`[f32; 3]`, degrees: pitch,
  roll, yaw) and `owner` (`Option<u32>`, a player id).
- `MAX_ENTITIES`, `is_finite`, `entity_bag` and `entity_of_bag`.

The client stores the mirror in `ClientRuntime` (`sixm-client/src/runtime.rs`)
as a map from entity id to `ClientEntity`: the `EntityInfo`, the `epoch` (a
`u32`) and a `dirty` flag. `ClientEntity` is `pub(crate)`, so the epoch is not
visible outside `sixm-client` today.

The mirror changes in three places, all in `handle_notice`:

- `ClientNotice::EntityCreated { id, info, epoch }` calls `create_entity`.
- `ClientNotice::EntityTransforms { updates }` calls `apply_transforms`.
- `ClientNotice::EntityDeleted { id }` calls `delete_entity`.

Scripts and other code read the mirror through `Host::entity(id)` and
`Host::entities(kind)`. Both return copies, and `entities` allocates a `Vec`.

### Ownership and the teleport epoch

An entity has at most one owner. A player's own ped is owned by that player.
The server creates it just before `playerJoining` (`ServerHost::player_ped`).

The epoch exists so that a server move wins over an owner's older update. On
the server (`sixm-server/src/entities.rs`), a server move increases the epoch,
and `owner_move` refuses an update unless the sender owns the entity and the
epoch it carries is the current one. On the client, `apply_transforms` treats an
update with the epoch the client already holds for an entity it owns as its own
move echoed back, and skips it. Any other update replaces the transform, takes
the new epoch and drops a local move not yet sent.

### Moving an owned entity

`ClientHost::move_owned_entity(entity, position, rotation)` changes the mirror
copy of an entity this client owns. It fails for an entity the client does not
own, or for a transform that is not finite. `None` keeps that part. It marks the
entity `dirty` and queues its id in `owned_moves`.

`ClientRuntime::tick` runs `send_owned_moves` after the scripts and the queued
commands. It sends the latest transform of each dirty entity, with its epoch,
as one batch through `Client::send_entity_transforms`. The buffers are reused,
so a frame allocates only when the connection queues the message.

### The local ped

`ClientHost::local_player()` returns this client's server id.
`ClientHost::local_ped()` returns the id of the local player's ped, or `None`
until the server has sent it. `create_entity` sets it when a created entity is
a ped owned by the local player, and `delete_entity` clears it.

### State bags

State is addressed by bag name. `entity_bag(id)` names an entity's bag
(`entity:<id>`), `player_bag` names a player's, and `GLOBAL_BAG` is the global
one. `Host::state_get` and `Host::state_set` read and write. A client may
write only with `replicated` set to `false`; the server's replicated state
reaches the client through `ClientNotice::StateBag...` notices and
`deliver_state` runs the script handlers. An adapter can use entity bags for
per-entity game data (for example a vehicle's colour) without any new message.

### Events

`Host::trigger_event` dispatches local events. `ClientHost::trigger_server_event`
and `trigger_latent_server_event` send events to the server, and events from the
server arrive with `EventSource::Server`. An adapter can raise local events for
game happenings, and can listen to server events through the same path.

### The frame loop

`ClientRuntime::run` is the loop. Each iteration calls `pump` (drain network
notices without blocking), then runs one `tick(now)` when a frame is due, or
`run_due_timers` when only a timer is due, then waits for console input until
the next frame or timer. The frame rate comes from the `sixm_tickRate` convar
(default 60). `tick` resumes every resource's scheduler, drains queued commands
and sends owned moves. This is the only place an adapter hook can go without
changing how scripts are scheduled.

### What the Lua side already does with the mirror

`sixm-lua/src/entities.rs` installs `DoesEntityExist`, `GetEntityCoords`,
`GetEntityHeading`, `GetEntityRotation`, `GetEntityModel`, `GetEntityType`,
`NetworkGetNetworkIdFromEntity`, `NetworkGetEntityFromNetworkId`,
`NetworkDoesNetworkIdExist`, `GetEntityFromStateBagName` and `Entity` on both
sides, and the server-only spawn and move natives (`CreateVehicle`, `CreatePed`,
`CreateObject`, `SetEntityCoords`, `SetEntityHeading`, `SetEntityRotation`,
`DeleteEntity`, `GetAllVehicles` and so on). On the client it adds
`GetGamePool`, and `SetEntityCoords`, `SetEntityHeading` and `SetEntityRotation`
go through `move_owned_entity` for entities the client owns. `sixm-lua/src/client.rs`
installs `PlayerId`, `GetPlayerServerId`, `PlayerPedId`, `GetPlayerPed` and
`LocalPlayer`. All of these read the mirror, not a game.

There is no `Citizen.InvokeNative` and no native table today. The `Citizen`
table holds the thread and timer functions only.

## 3. Proposed `GameAdapter` trait

Everything in this section is **proposed**. The trait would live in
`sixm-core`, next to `ScriptRuntime`, and be game-independent: it names no
game and uses only core types.

### Design rules

- The client owns the mirror. The adapter holds game objects and a map from
  entity id to its own handle type. The core never sees that handle.
- The core calls the adapter. The adapter does not call back into scripts, so
  there is no re-entrancy to reason about. Anything that must reach scripts
  (a game event) goes through a small `AdapterSink` the adapter is given at
  attach time.
- The per-frame path does not allocate and does not lock. The adapter gets
  slices and fills caller-owned buffers.
- The trait is `!Send` in practice: see "Threading" below.

### The trait

```rust
// PROPOSED. Not in the code.

/// Why the adapter could not do what the core asked. Never fatal to the
/// client by itself; the client logs it and keeps the mirror as it is.
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("the game is not ready")]
    NotReady,
    #[error("unknown model {0:#010x}")]
    UnknownModel(u32),
    #[error("{0}")]
    Game(String),
}

/// One change to the mirror that the adapter must show in the game.
#[derive(Debug, Clone, Copy)]
pub enum MirrorChange {
    /// An entity entered the mirror.
    Created { id: u32, info: EntityInfo },
    /// The server moved an entity, or the owner of a mirrored entity is
    /// another client. The transform in `info` is the new one. `teleport`
    /// is true when the epoch changed, so the adapter must not interpolate.
    Moved { id: u32, info: EntityInfo, teleport: bool },
    /// The owner of the entity changed (see section 5).
    OwnerChanged { id: u32, info: EntityInfo },
    /// An entity left the mirror.
    Removed { id: u32 },
}

/// The game side of a client. One implementation per game.
///
/// Every method is called on the client main thread, from `ClientRuntime`,
/// and never re-entrantly. An implementation may keep non-`Send` state.
pub trait GameAdapter {
    // --- lifecycle ---------------------------------------------------

    /// Called once, after the client has connected and applied the first
    /// snapshot (replicated convars, entities, state bags) and before any
    /// resource starts. `sink` stays valid until `detach`. Returning an error
    /// stops the client with that error.
    fn attach(&mut self, sink: AdapterSink) -> Result<(), AdapterError>;

    /// Called once, after every resource has stopped, on shutdown and on
    /// disconnect. Must release every game object the adapter created.
    fn detach(&mut self);

    /// Called once per frame at `now`, before the resources are ticked.
    /// The adapter reads game input and game state here and may raise events
    /// through its sink. Must not block.
    fn before_scripts(&mut self, now: Instant);

    /// Called once per frame, after the resources were ticked and the queued
    /// commands ran, and before `send_owned_moves`. The adapter shows the
    /// frame in the game here. Must not block.
    fn after_scripts(&mut self, now: Instant);

    // --- entity materialization --------------------------------------

    /// Applies the mirror changes of this frame, in the order they arrived.
    /// Called from `before_scripts` time, so scripts see game objects that
    /// match the mirror. A failure for one entity is returned in `errors`
    /// and does not stop the others.
    fn apply_changes(
        &mut self,
        changes: &[MirrorChange],
        errors: &mut Vec<(u32, AdapterError)>,
    );

    /// Reads the transform of every entity this client owns from the game and
    /// appends `(id, position, rotation)` for each one that moved since the
    /// last call. The client passes each to `ClientHost::move_owned_entity`.
    /// `owned` lists the ids, in ascending order. Called once per frame,
    /// after `after_scripts`.
    fn read_owned(
        &mut self,
        owned: &[u32],
        out: &mut Vec<(u32, [f32; 3], [f32; 3])>,
    );

    // --- native calls ------------------------------------------------

    /// The game-specific natives this adapter provides. Called once, at
    /// `attach`. The core installs them in each Lua runtime (see below).
    fn natives(&self) -> &NativeRegistry;

    // --- input and UI (later) ----------------------------------------
    //
    // Deliberately not designed here. Keyboard, pad and mouse input, text
    // and HUD drawing, menus, and audio are LATER. They will be added as
    // separate traits or as additional methods with default bodies, so that
    // a first adapter does not have to implement them.
}
```

Summary of the shape:

- Lifecycle: `attach`, `detach`, `before_scripts`, `after_scripts`.
- Materialization in: `apply_changes` with `MirrorChange`.
- Materialization out: `read_owned`, which feeds `move_owned_entity`.
- Natives: `natives()` returns a `NativeRegistry`.
- Input and UI: placeholders only.

### Lifecycle and the frame

`ClientRuntime::tick` would become:

1. `adapter.before_scripts(now)`.
2. Apply the pending `MirrorChange` list with `adapter.apply_changes`.
3. Resume every resource (`runtime.tick(now)`), as today.
4. Drain queued commands, as today.
5. `adapter.after_scripts(now)`.
6. `adapter.read_owned(...)`, then `move_owned_entity` for each result.
7. `send_owned_moves`, as today.

Without an adapter, steps 1, 2, 5 and 6 do nothing, and the loop is the one
that exists today. The headless client stays the default.

`MirrorChange` values are collected in `create_entity`, `apply_transforms` and
`delete_entity` into a reused `Vec`. This needs one addition to the mirror:
`apply_transforms` must report whether the epoch changed, which it already
knows when it compares `slot.epoch == epoch`. The epoch itself stays private to
`sixm-client`; the adapter only sees `teleport: bool`.

### Entity materialization

- Created: the adapter looks the model hash up in the game, creates an object
  of the `EntityKind`, and keeps its own handle under the entity id. An unknown
  model returns `AdapterError::UnknownModel`; the entity stays in the mirror
  and scripts still see it. The adapter must tolerate a model that is not
  loaded yet. How it streams models in is its own business, but it must not
  block the frame.
- Moved: the adapter sets or interpolates the transform. For an entity this
  client owns, a `Moved` change means the server moved it (a teleport), because
  `apply_transforms` skips the echo of the owner's own move. The adapter must
  place the object exactly and must not interpolate.
- Removed: the adapter destroys its object and forgets the handle.
- Owned entities: the game is the source of truth for the transform while this
  client owns the entity. `read_owned` copies it into the mirror through
  `move_owned_entity`. That call already validates ownership and finiteness and
  already queues the send, so the adapter adds no network code.

Scripts that call `SetEntityCoords` on an owned entity change the mirror
through `move_owned_entity`, not the game. To keep the game in step, the client
would also pass such a change to the adapter as a `MirrorChange::Moved` with
`teleport` true. This needs `move_owned_entity` to record the change, which it
does not do today.

### Native calls

The core must not know any game's natives. The proposal is a registry the
adapter fills and the core installs.

```rust
// PROPOSED. Not in the code.

/// A native implemented by the adapter. Takes the script's arguments and
/// returns its results. Called on the client main thread.
pub type NativeFn = Box<dyn Fn(&[Value]) -> Result<Vec<Value>, ScriptError>>;

/// Named native functions provided by an adapter.
#[derive(Default)]
pub struct NativeRegistry { /* sorted names, functions, hash index */ }

impl NativeRegistry {
    /// Adds a native under `name` (for example `"SetVehicleColours"`).
    /// Fails if `name` is already registered or is not a valid Lua name.
    pub fn register(&mut self, name: &str, function: NativeFn) -> Result<(), AdapterError>;

    /// The native registered under `name`.
    pub fn get(&self, name: &str) -> Option<&NativeFn>;

    /// The native whose name hashes to `hash`, using `GetHashKey`.
    pub fn get_by_hash(&self, hash: u32) -> Option<&NativeFn>;
}
```

How it reaches Lua:

- `sixm-lua` gets one new installer for client runtimes. For each name in the
  registry it adds a global function with that name. The function converts its
  Lua arguments to `Value`s, calls the native, and converts the results back.
  Argument conversion uses the conversion `sixm-lua` already uses for events
  and exports, so vectors map to `Vector3` and so on.
- Natives would be installed once per runtime at start, not looked up per call.
  A call costs one table lookup in Lua and one boxed call in Rust, plus the
  `Value` conversion. Natives that run per frame on hot paths (for example
  reading a position) should take and return scalars and vectors, which do not
  allocate.
- `Citizen.InvokeNative(hash, ...)` would be added as a client-only function.
  It looks the hash up with `get_by_hash` and calls the same function. It
  exists for scripts that carry hashes instead of names, and for natives the
  adapter does not name. The hash is the `GetHashKey` of the native's name, so
  the core needs no table of its own. An unknown hash raises a script error.
- Return values: `InvokeNative` in FiveM takes result-type markers. We do not
  propose copying that. The adapter native knows what it returns, so the
  markers are not needed. If a script needs a specific type, the native name
  can say so.
- Sandbox: natives run outside the sandbox but see only `Value`s. They must
  validate their arguments themselves, because script input is untrusted.
- Natives are an adapter feature only. A runtime without an adapter has no
  `InvokeNative` and none of the registry's globals.

A native that takes an entity takes the entity id, as the core natives do. The
adapter maps the id to its own handle.

### Threading

- Every `GameAdapter` method is called on the client main thread, the one that
  runs `ClientRuntime::run`. `ClientRuntime` already uses `Rc` and `RefCell`,
  so the client is single-threaded by design and the trait has no `Send` bound.
- An adapter that needs another thread (a render thread, a game callback on a
  thread of the game's own) must marshal across a channel and drain it in
  `before_scripts`. It must not call into `AdapterSink` or the runtime from
  that thread.
- `AdapterSink` is `!Send` for the same reason. It is a handle to raise local
  events and to read the mirror, and it only works on the main thread.
- No method may block. A slow game call must be split over frames by the
  adapter.

### Out of scope, later

Marked here so nobody reads silence as a decision:

- Input (keyboard, pad, mouse): later.
- UI and HUD drawing, text, menus, browser panels: later.
- Audio and voice: later.
- Camera and rendering control: later.
- Streaming, culling and per-entity relevance: later. Today the server sends
  every entity to every client.
- A script-visible game event model (damage, death, vehicle entered): later.
  The sink can raise local events, but their names and arguments are not
  designed.

## 4. How scripts would see it

Scripts keep writing against the same globals. The split:

Stays in the core (exists today, reads the mirror, needs no game):

- Entities: `DoesEntityExist`, `GetEntityCoords`, `GetEntityHeading`,
  `GetEntityRotation`, `GetEntityModel`, `GetEntityType`, `GetGamePool`,
  `NetworkGetNetworkIdFromEntity`, `NetworkGetEntityFromNetworkId`,
  `NetworkDoesNetworkIdExist`, `NetworkGetEntityOwner`, `GetEntityFromStateBagName`,
  `Entity`, and on the client `SetEntityCoords`, `SetEntityHeading` and
  `SetEntityRotation` for owned entities.
- Players: `PlayerId`, `GetPlayerServerId`, `PlayerPedId`, `GetPlayerPed`,
  `LocalPlayer`.
- State: `GlobalState`, `LocalPlayer.state`, `Entity(id).state`, and the state
  bag change handlers.
- Events, exports, threads, timers, convars, commands and `json`.
- All server natives, including `CreateVehicle`, `CreatePed` and `CreateObject`.
  The server creates entities; it never touches a game.

Becomes an adapter native (proposed, none of these exist):

- Anything that needs the game to answer: vehicle and ped appearance and
  behaviour, animations, weapons, camera, world and weather, model loading,
  blips, and any native that takes or returns a game handle.
- `Citizen.InvokeNative` and the hash lookup, as described above.
- Input and UI natives, later.

A rule decides the boundary: if the answer is in the mirror, state bags or an
event, it stays in the core. If only the running game knows the answer, it is
an adapter native.

Existing core natives keep their meaning when an adapter is attached. The
mirror stays the source of truth for position, model, kind and owner. An
adapter native that moves an entity must go through the adapter and the
mirror, and not around it, so that a move still reaches the server.

A resource written for a game should name that game in its manifest
(`game 'gta6'` in the README example). The manifest already allows a `game`
directive. Whether the client refuses a resource built for another game is not
decided (see section 5).

## 5. Open questions and risks

Ownership migration between clients. Today the owner of a ped is its player,
and other entities have `owner: None` unless a script sets one. There is no
message that changes an owner after creation. A game that wants a client to
simulate a vehicle needs one, plus a rule for who owns what when the owner
disconnects or leaves range. The epoch would also need to define what happens
to an update in flight at the moment of handover. Not designed.

Entities a game spawns on its own. A game may create peds, vehicles and objects
that no server created (traffic, ambient world, scripted scenes). Options:
suppress them in the adapter; keep them local and unmirrored; or register them
with the server as entities. Suppressing is simplest and is the likely start.
Registering them needs an id, an owner and a rule against duplicates between
clients, and costs bandwidth. `GetGamePool` today lists mirrored entities only,
which would differ from a game's own pool. We have to decide whether it must
match.

Ids and handles. Entity ids are `u32`, count up from 1, and are not reused
while the server runs. A game handle is the adapter's own. A script must never
see a game handle where it expects an entity id. Natives that return a handle
must translate it.

Interpolation and prediction. The server sends the latest transform once per
server frame (20 per second by default), and the client frame rate is separate.
Smoothing for remote entities is an adapter job. Its contract with the core
(timestamps, velocity) is open, because `EntityInfo` carries neither.

Authority and cheating. The owner's transform is trusted by the server within
the checks it has (finite, current epoch, right owner). A real game makes
speed, teleport and spawn abuse possible. Server-side validation of owner moves
is not designed. This is a risk for the protocol, not for the adapter, but the
adapter makes it real.

Anti-cheat. Many games ship an anti-cheat that forbids code injected into the
game process. An adapter that runs inside the game process may be unacceptable
to the game's publisher, and we cannot decide that here. We will not design
around an anti-cheat. If the game has no supported way to load an adapter, the
adapter does not ship.

Legal and clean room. The adapter is the part of the project closest to a
game's code. Rules:

- No game files, no game code and no data extracted from a game in the
  repository, in tests or in the docs.
- No reverse-engineered layouts, offsets or signatures. An adapter may use only
  what the game publisher documents or supports (an official API, a mod
  interface, a documented scripting layer).
- Native names and hashes in an adapter come from that supported interface,
  not from outside sources.
- No FiveM or Cfx.re code or internals. Public API names are fine.
- Whether a mod interface for a given game permits online play with custom
  servers is a legal question for the maintainer, not a design question.

If no supported interface exists when the game ships on PC, the real adapter
waits. The test adapter and the core are unaffected.

Platform. The core runs on Windows and Linux, and CI runs both. An adapter for
a game that runs on Windows only would be Windows-only. The trait must stay
platform-neutral, and the crate that implements it needs a `#[cfg]` counterpart
or must be excluded from the Linux build explicitly, so that the workspace
still passes both CI jobs. Compatibility layers on Linux are out of scope.

Where adapter code lives. A new crate per game (`sixm-adapter-<name>`) keeps
game code out of `sixm-client`. `sixm-client` would take a
`Box<dyn GameAdapter>` and stay generic. A dynamic library loaded at run time
needs `unsafe` and a stable ABI. The project rules forbid `unsafe`, so the
adapter is linked at build time. This may conflict with how a real game loads
mods. Open.

Script compatibility. `InvokeNative` with hashes is only useful if scripts
carry the hashes they want. We cannot ship a table of another game's hashes
from outside sources. Whether to support it at all, or only named natives, is
open.

Performance. The per-frame cost for N mirrored entities should be proportional
to the number of changes, not to N. `apply_changes` takes only changes.
`read_owned` takes only owned ids. `Host::entities` allocates a `Vec`, so the
client must not call it every frame for the adapter. An adapter that wants all
entities keeps its own map, updated from `MirrorChange`.

## 6. Phased plan

Phase 0, headless (today). The client has no game and no adapter. The mirror
exists, `move_owned_entity` works, the loop has no hook. Nothing to do except
keep the contract in `ClientHost` stable.

Phase 1, the contract and the test adapter. Add the proposed trait,
`MirrorChange`, `AdapterError`, `NativeRegistry` and `AdapterSink` to
`sixm-core`. Add the hooks in `ClientRuntime::tick` and the change list in the
mirror, with the epoch compare reporting `teleport`. Add a `NullAdapter` or
`SimAdapter` that renders nothing and simulates movement:

- It keeps a map of entity id to a simple state.
- It moves owned entities by a scripted pattern (a straight line, a circle, a
  random walk with a seed) and returns the transforms from `read_owned`.
- It applies server moves exactly.
- It registers a few trivial natives so that the registry and the Lua installer
  have something to test.

Uses: integration tests of the whole path (script, mirror, adapter, server,
other client) without a game, load tests with many simulated players, and bots.
It also checks that the trait is sound before a real game depends on it. It
runs in CI on Windows and Linux.

Phase 2, a real game adapter. A separate crate, built against the game's
supported interface, which has to exist first. It implements the trait with
the same hooks, adds its natives, and handles the game-specific questions in
section 5. The trait may change in phase 2 where phase 1 guessed wrong, and
those changes are made in `sixm-core` and the test adapter together, so that
the two never drift.

Later phases, not planned in detail: input and UI traits, ownership migration,
streaming and relevance, anti-cheat and server-side validation.

Each phase ships as its own pull request and passes `cargo fmt`, `cargo clippy
-D warnings` and `cargo test` on both systems, as every change does.
