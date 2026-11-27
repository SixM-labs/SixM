/**
 * Official SixM TypeScript type definitions.
 * Matches SixM's QuickJS ES2023 scripting runtime and natives.
 */

// =============================================================================
// Scheduling, timers, and async
// =============================================================================

/**
 * Registers a callback to execute once every server or client tick until
 * `clearTick(handle)` is called or the callback throws.
 */
declare function setTick(fn: () => void | Promise<void>): number;

/**
 * Cancels a tick callback registered with `setTick`.
 */
declare function clearTick(handle: number): void;

/**
 * Schedules `fn` to run once after `ms` milliseconds.
 * Accepts both `(fn, ms, ...args)` and FiveM-style `(ms, fn)`.
 */
declare function setTimeout(
    fn: (...args: any[]) => void | Promise<void>,
    ms?: number,
    ...args: any[]
): number;
declare function setTimeout(ms: number, fn: () => void | Promise<void>): number;

/**
 * FiveM-style timeout alias: schedules `fn` to run once after `ms` milliseconds.
 */
declare function SetTimeout(ms: number, fn: () => void | Promise<void>): number;
declare function SetTimeout(
    fn: (...args: any[]) => void | Promise<void>,
    ms?: number,
    ...args: any[]
): number;

/**
 * Cancels a pending timeout created by `setTimeout` or `SetTimeout`.
 */
declare function clearTimeout(id?: number): void;

/**
 * Cancels a pending timeout created by `SetTimeout` or `setTimeout`.
 */
declare function ClearTimeout(id?: number): void;

/**
 * Repeatedly calls `fn` every `ms` milliseconds (clamped to a 1 ms minimum).
 */
declare function setInterval(
    fn: (...args: any[]) => void | Promise<void>,
    ms?: number,
    ...args: any[]
): number;

/**
 * Stops a repeating interval created by `setInterval`.
 */
declare function clearInterval(id?: number): void;

/**
 * Returns a Promise that resolves after `ms` milliseconds (`0` waits one tick).
 */
declare function Delay(ms?: number): Promise<void>;

/**
 * Alias for `Delay`. Returns a Promise that resolves after `ms` milliseconds.
 */
declare function Wait(ms?: number): Promise<void>;

/**
 * Returns milliseconds elapsed since the process first queried the timer.
 */
declare function GetGameTimer(): number;

/**
 * Schedules `fn` to run on the next tick.
 */
declare function CreateThread(fn: () => void | Promise<void>): void;

declare namespace Citizen {
    function setTick(fn: () => void | Promise<void>): number;
    function clearTick(handle: number): void;
    function setTimeout(
        fn: (...args: any[]) => void | Promise<void>,
        ms?: number,
        ...args: any[]
    ): number;
    function clearTimeout(id?: number): void;
    function setInterval(
        fn: (...args: any[]) => void | Promise<void>,
        ms?: number,
        ...args: any[]
    ): number;
    function clearInterval(id?: number): void;
    function Delay(ms?: number): Promise<void>;
    function Wait(ms?: number): Promise<void>;
    function CreateThread(fn: () => void | Promise<void>): void;
    /**
     * Invokes a registered client game-adapter native by its 32-bit hash. (Client only)
     */
    function InvokeNative(hash: number, ...args: any[]): any;
}

// =============================================================================
// Events and source
// =============================================================================

interface EventHandlerHandle {
    key: number;
    name: string;
}

interface Deferrals {
    /** Holds the connection open during `playerConnecting`. */
    defer(): void;
    /** Sends a progress message (truncated to 4096 bytes) to the connecting client. */
    update(message: string): void;
    /** Completes the deferral; a non-empty `reason` refuses the connection. */
    done(reason?: string): void;
}

/**
 * Sender of the active event (`""` for local events, player server id for
 * player events, `65535` on client for server-to-client events).
 */
declare var source: string | number;

interface SixMChatMessage {
    args: (string | number | boolean)[];
    color?: [red: number, green: number, blue: number];
    multiline?: boolean;
    template?: string;
    templateId?: string;
}

interface SixMChatParameter {
    name: string;
    help?: string;
}

interface SixMChatSuggestion {
    name: `/${string}`;
    help?: string;
    params?: SixMChatParameter[];
}

interface SixMChatClientEvents {
    "chat:addMessage": [message: SixMChatMessage];
    "chat:addTemplate": [templateId: string, html: string];
    "chat:addSuggestion": [name: `/${string}`, help?: string, params?: SixMChatParameter[]];
    "chat:addSuggestions": [suggestions: SixMChatSuggestion[]];
    "chat:removeSuggestion": [name: `/${string}`];
    "chat:clear": [];
}

interface SixMChatServerNetworkEvents {
    "_chat:messageEntered": [text: string];
    "chat:init": [];
}

interface SixMChatNetworkEvents extends SixMChatClientEvents, SixMChatServerNetworkEvents {}

/** Events exposed by the stock resources while they are running. */
interface SixMStockEvents extends SixMChatNetworkEvents {
    chatMessage: [playerId: number, author: string, text: string];
    playerSpawned: [spawnInfo: SpawnInfo];
}

/**
 * Registers a local event handler for `name` and returns its handle.
 */
declare function on<Name extends keyof SixMStockEvents>(
    name: Name,
    fn: (...args: SixMStockEvents[Name]) => void | Promise<void>
): EventHandlerHandle;
declare function on(name: string, fn: (...args: any[]) => void | Promise<void>): EventHandlerHandle;

/**
 * Alias for `on`. Registers a local event handler for `name`.
 */
declare function AddEventHandler<Name extends keyof SixMStockEvents>(
    name: Name,
    fn: (...args: SixMStockEvents[Name]) => void | Promise<void>
): EventHandlerHandle;
declare function AddEventHandler(
    name: string,
    fn: (...args: any[]) => void | Promise<void>
): EventHandlerHandle;

/**
 * Removes an event handler using the handle returned by `on` / `AddEventHandler` / `onNet`.
 */
declare function RemoveEventHandler(handle: EventHandlerHandle): void;

/**
 * Removes a registered listener function from event `name`.
 */
declare function removeEventListener(name: string, fn: (...args: any[]) => void): void;

/**
 * Alias for `on`. With `netSafe` true, `name` is also marked as a network-safe event.
 */
declare function addEventListener(
    name: string,
    fn: (...args: any[]) => void | Promise<void>,
    netSafe?: boolean
): EventHandlerHandle;

/**
 * Alias for `onNet`. Marks `name` as a network-safe event and attaches `fn`.
 */
declare function addNetEventListener(
    name: string,
    fn: (...args: any[]) => void | Promise<void>
): EventHandlerHandle;

/**
 * Registers a raw handler for `name`. `fn` receives the msgpack bytes of the arguments and the source.
 * Network events still need `onNet` / `RegisterNetEvent`.
 */
declare function addRawEventListener(
    name: string,
    fn: (payload: Uint8Array, source: number | string) => void | Promise<void>
): EventHandlerHandle;

/**
 * Alias for `addRawEventListener`.
 */
declare function addRawEventHandler(
    name: string,
    fn: (payload: Uint8Array, source: number | string) => void | Promise<void>
): EventHandlerHandle;

/**
 * Marks `name` as a network-safe event and attaches `fn`.
 */
declare function onNet<Name extends keyof SixMChatNetworkEvents>(
    name: Name,
    fn: (...args: SixMChatNetworkEvents[Name]) => void | Promise<void>
): EventHandlerHandle;
declare function onNet(
    name: string,
    fn: (...args: any[]) => void | Promise<void>
): EventHandlerHandle;

/**
 * Allows `name` to arrive over the network, optionally registering `fn`.
 */
declare function RegisterNetEvent<Name extends keyof SixMChatNetworkEvents>(
    name: Name,
    fn?: (...args: SixMChatNetworkEvents[Name]) => void | Promise<void>
): EventHandlerHandle | undefined;
declare function RegisterNetEvent(
    name: string,
    fn?: (...args: any[]) => void | Promise<void>
): EventHandlerHandle | undefined;

/**
 * Alias for `RegisterNetEvent`.
 */
declare function RegisterServerEvent(
    name: string,
    fn?: (...args: any[]) => void | Promise<void>
): EventHandlerHandle | undefined;

/**
 * Synchronously triggers all local handlers of `name` on this host.
 */
declare function emit<Name extends keyof SixMStockEvents>(name: Name, ...args: SixMStockEvents[Name]): void;
declare function emit(name: string, ...args: any[]): void;

/**
 * Alias for `emit`. Synchronously triggers all local handlers of `name`.
 */
declare function TriggerEvent<Name extends keyof SixMStockEvents>(name: Name, ...args: SixMStockEvents[Name]): void;
declare function TriggerEvent(name: string, ...args: any[]): void;

/**
 * Sends a network event.
 * - Server: `emitNet(name, target, ...args)` (`target` is player id or `-1` for all).
 * - Client: `emitNet(name, ...args)` (sends to the server).
 */
declare function emitNet(name: string, target: number | string, ...args: any[]): void;
declare function emitNet(name: string, ...args: any[]): void;

/**
 * Sends a network event to `target` player id or `-1` for all players. (Server only)
 */
declare function TriggerClientEvent<Name extends keyof SixMChatClientEvents>(
    name: Name,
    target: number | string,
    ...args: SixMChatClientEvents[Name]
): void;
declare function TriggerClientEvent(name: string, target: number | string, ...args: any[]): void;

/**
 * Sends a network event to the server. (Client only)
 */
declare function TriggerServerEvent<Name extends keyof SixMChatServerNetworkEvents>(
    name: Name,
    ...args: SixMChatServerNetworkEvents[Name]
): void;
declare function TriggerServerEvent(name: string, ...args: any[]): void;

/**
 * Sends a bandwidth-paced network event to `target` player id or `-1`. (Server only)
 */
declare function TriggerLatentClientEvent(
    name: string,
    target: number | string,
    bytesPerSecond: number,
    ...args: any[]
): void;

/**
 * Sends a bandwidth-paced network event to the server. (Client only)
 */
declare function TriggerLatentServerEvent(
    name: string,
    bytesPerSecond: number,
    ...args: any[]
): void;

/**
 * Marks the active event dispatch as canceled.
 */
declare function CancelEvent(): void;

/**
 * Returns whether the active event dispatch was canceled.
 */
declare function WasEventCanceled(): boolean;

// =============================================================================
// Exports
// =============================================================================

interface ExportsProxy {
    /** Registers a runtime export on the current resource. */
    (name: string, fn: (...args: any[]) => any): void;
    /** Accesses synchronous exports exposed by `resource`. */
    [resource: string]: Record<string, (...args: any[]) => any>;
    /** Client exports provided by the stock [system]/spawnmanager resource. */
    spawnmanager: SpawnManagerExports;
}

declare var exports: ExportsProxy;

interface SpawnPoint {
    x: number;
    y: number;
    z: number;
    heading: number;
    model?: string | number;
    skipFade?: boolean;
}

interface SpawnInfo extends SpawnPoint {
    model: number;
    skipFade: boolean;
    idx?: number;
}

type SpawnManagerExports = {
    addSpawnPoint(spawn: SpawnPoint): number;
    removeSpawnPoint(id: number): boolean;
    loadSpawns(json: string): boolean;
    spawnPlayer(spawn?: number | SpawnPoint, callback?: (info: SpawnInfo) => void): boolean;
    setAutoSpawn(enabled: boolean): void;
    setAutoSpawnCallback(callback: () => void): void;
    forceRespawn(): void;
};

// =============================================================================
// State bags
// =============================================================================

interface StateBag {
    /** Explicitly writes `value` to `key` (`replicated` defaults to `true`). */
    set(key: string, value: unknown, replicated?: boolean): void;
    /** Explicitly reads the value stored under `key`. */
    get(key: string): unknown;
    [key: string]: unknown;
}

interface StateBagHolder {
    state: StateBag;
}

/** Global replicated state bag. */
declare var GlobalState: StateBag;

/** Returns a holder with `.state` for player `id`. (Server only) */
declare function Player(id: number | string): StateBagHolder;

/** Local player state holder with `.state`. (Client only) */
declare var LocalPlayer: StateBagHolder;

/** Returns a holder with `.state` for `entity` (`entity:<id>`). */
declare function Entity(entity: number): StateBagHolder;

/**
 * Registers a change handler for state bags. `keyFilter` and `bagFilter` may be `null`.
 */
declare function AddStateBagChangeHandler(
    keyFilter: string | null | undefined,
    bagFilter: string | null | undefined,
    fn: (
        bagName: string,
        key: string,
        value: unknown,
        reserved: number,
        replicated: boolean
    ) => void
): number;

/**
 * Removes a state bag change handler by its cookie.
 */
declare function RemoveStateBagChangeHandler(cookie: number): void;

/**
 * Extracts the player server id from a `player:<id>` state bag name, or `0`.
 */
declare function GetPlayerFromStateBagName(bagName: string): number;

/**
 * Extracts the entity id from an `entity:<id>` state bag name if it exists, or `0`.
 */
declare function GetEntityFromStateBagName(bagName: string): number;

/**
 * The value of `key` in state bag `bagName`, or `undefined` when unset.
 */
declare function GetStateBagValue(bagName: string, key: string): any;

/**
 * Whether state bag `bagName` has a value for `key`.
 */
declare function StateBagHasKey(bagName: string, key: string): boolean;

/**
 * The sorted key names of state bag `bagName`, or `undefined` for an unknown bag.
 */
declare function GetStateBagKeys(bagName: string): string[] | undefined;

/**
 * Writes `value` under `key` in state bag `bagName`. `replicated` defaults to false.
 */
declare function SetStateBagValue(bagName: string, key: string, value: any, replicated?: boolean): void;

/**
 * FiveM form: `valueData` is msgpack bytes and only its first `valueLength` bytes are decoded.
 */
declare function SetStateBagValue(bagName: string, key: string, valueData: Uint8Array | string, valueLength: number, replicated?: boolean): void;

/**
 * Compatibility call; does nothing, since an entity's bag lives as long as the entity.
 */
declare function EnsureEntityStateBag(entity: number): void;

// =============================================================================
// Vectors, quaternions, and msgpack
// =============================================================================

interface Vector2 {
    x: number;
    y: number;
    r: number;
    g: number;
    [index: number]: number;
    [Symbol.iterator](): IterableIterator<number>;
    readonly length: number & (() => number);
    normalize(): Vector2;
    add(other: Vector2 | number): Vector2;
    sub(other: Vector2 | number): Vector2;
    mul(scalar: Vector2 | number): Vector2;
    div(scalar: Vector2 | number): Vector2;
    dot(other: Vector2): number;
    toString(): string;
    toJSON(): { x: number; y: number };
    readonly [swizzle: string]: any;
}

interface Vector3 {
    x: number;
    y: number;
    z: number;
    r: number;
    g: number;
    b: number;
    xy: Vector2;
    xz: Vector2;
    yz: Vector2;
    [index: number]: number;
    [Symbol.iterator](): IterableIterator<number>;
    readonly length: number & (() => number);
    normalize(): Vector3;
    add(other: Vector3 | number): Vector3;
    sub(other: Vector3 | number): Vector3;
    mul(scalar: Vector3 | number): Vector3;
    div(scalar: Vector3 | number): Vector3;
    dot(other: Vector3): number;
    cross(other: Vector3): Vector3;
    toString(): string;
    toJSON(): { x: number; y: number; z: number };
    readonly [swizzle: string]: any;
}

interface Vector4 {
    x: number;
    y: number;
    z: number;
    w: number;
    r: number;
    g: number;
    b: number;
    a: number;
    xy: Vector2;
    xyz: Vector3;
    [index: number]: number;
    [Symbol.iterator](): IterableIterator<number>;
    readonly length: number & (() => number);
    normalize(): Vector4;
    add(other: Vector4 | number): Vector4;
    sub(other: Vector4 | number): Vector4;
    mul(scalar: Vector4 | number): Vector4;
    div(scalar: Vector4 | number): Vector4;
    dot(other: Vector4): number;
    toString(): string;
    toJSON(): { x: number; y: number; z: number; w: number };
    readonly [swizzle: string]: any;
}

interface Quat {
    w: number;
    x: number;
    y: number;
    z: number;
    [index: number]: number;
    [Symbol.iterator](): IterableIterator<number>;
    readonly length: number;
    toString(): string;
    toJSON(): { w: number; x: number; y: number; z: number };
}

declare function vector2(x: number, y: number): Vector2;
declare function vector2(scalar: number): Vector2;
declare function vector2(other: Vector2): Vector2;
declare function Vector2(x: number, y: number): Vector2;
declare function Vector2(scalar: number): Vector2;
declare function Vector2(other: Vector2): Vector2;
declare function vec2(x: number, y: number): Vector2;
declare function vec2(scalar: number): Vector2;
declare function vec2(other: Vector2): Vector2;

declare function vector3(x: number, y: number, z: number): Vector3;
declare function vector3(xy: Vector2, z: number): Vector3;
declare function vector3(x: number, yz: Vector2): Vector3;
declare function vector3(scalar: number): Vector3;
declare function vector3(other: Vector3): Vector3;
declare function Vector3(x: number, y: number, z: number): Vector3;
declare function Vector3(xy: Vector2, z: number): Vector3;
declare function Vector3(x: number, yz: Vector2): Vector3;
declare function Vector3(scalar: number): Vector3;
declare function Vector3(other: Vector3): Vector3;
declare function vec3(x: number, y: number, z: number): Vector3;
declare function vec3(xy: Vector2, z: number): Vector3;
declare function vec3(x: number, yz: Vector2): Vector3;
declare function vec3(scalar: number): Vector3;
declare function vec3(other: Vector3): Vector3;

declare function vector4(x: number, y: number, z: number, w: number): Vector4;
declare function vector4(xyz: Vector3, w: number): Vector4;
declare function vector4(xy: Vector2, zw: Vector2): Vector4;
declare function vector4(xy: Vector2, z: number, w: number): Vector4;
declare function vector4(scalar: number): Vector4;
declare function vector4(other: Vector4): Vector4;
declare function Vector4(x: number, y: number, z: number, w: number): Vector4;
declare function Vector4(xyz: Vector3, w: number): Vector4;
declare function Vector4(xy: Vector2, zw: Vector2): Vector4;
declare function Vector4(xy: Vector2, z: number, w: number): Vector4;
declare function Vector4(scalar: number): Vector4;
declare function Vector4(other: Vector4): Vector4;
declare function vec4(x: number, y: number, z: number, w: number): Vector4;
declare function vec4(xyz: Vector3, w: number): Vector4;
declare function vec4(xy: Vector2, zw: Vector2): Vector4;
declare function vec4(xy: Vector2, z: number, w: number): Vector4;
declare function vec4(scalar: number): Vector4;
declare function vec4(other: Vector4): Vector4;

declare function quat(w: number, x: number, y: number, z: number): Quat;
declare function quat(w: number, xyz: Vector3): Quat;
declare function quat(scalar: number): Quat;
declare function quat(other: Quat): Quat;

declare namespace msgpack {
    function pack(value: unknown): Uint8Array;
    function unpack(data: Uint8Array | ArrayBuffer): unknown;
}

// =============================================================================
// Convars and commands
// =============================================================================

declare function GetConvar(name: string, defaultValue: string): string;
declare function GetConvarInt(name: string, defaultValue: number): number;
declare function GetConvarFloat(name: string, defaultValue: number): number;
declare function GetConvarBool(name: string, defaultValue: boolean): boolean;

/** Sets a plain server-only convar. (Server only) */
declare function SetConvar(name: string, value: string): void;
/** Sets a replicated convar synced to all clients. (Server only) */
declare function SetConvarReplicated(name: string, value: string): void;
/** Sets a convar marked as server info. (Server only) */
declare function SetConvarServerInfo(name: string, value: string): void;
/** Sets the `gametype` server info convar. (Server only) */
declare function SetGameType(name: string): void;
/** Sets the `mapname` server info convar. (Server only) */
declare function SetMapName(name: string): void;

declare function AddConvarChangeListener(
    filter: string | null | undefined,
    fn: (convarName: string, reserved: number) => void
): number;
declare function RemoveConvarChangeListener(cookie: number): void;

declare function ExecuteCommand(line: string): void;
declare function RegisterCommand(
    name: string,
    fn: (source: number, args: string[], raw: string) => void,
    restricted?: boolean
): void;
declare function GetRegisteredCommands(): Array<{ name: string }>;
/** Returns `"sixm"` on a server and `"headless"` on a client. */
declare function GetGameName(): string;
/** Returns the game build number, always `0`. */
declare function GetGameBuildNumber(): number;
/** Returns the process id, stable for the life of the process. */
declare function GetInstanceId(): number;

// =============================================================================
// Resources and files
// =============================================================================

type ResourceState = "missing" | "stopped" | "starting" | "started" | "stopping";

declare function GetCurrentResourceName(): string;
declare function GetInvokingResource(): string | null;
declare function GetResourceState(name: string): ResourceState;
declare function GetNumResources(): number;
declare function GetResourceByFindIndex(index: number): string | null;
declare function GetResourceMetadata(resource: string, key: string, index: number): string | null;
declare function GetNumResourceMetadata(resource: string, key: string): number;
declare function GetResourceCommands(resource: string): string[];
declare function GetResourcePath(name: string): string | null;
declare function LoadResourceFile(resource: string, path: string): string | null;
declare function SaveResourceFile(
    resource: string,
    path: string,
    data: string | Uint8Array | ArrayBuffer
): boolean;

/** Starts `name` if allowed by `command.start` ACE permission. (Server only) */
declare function StartResource(name: string): boolean;
/** Stops `name` if allowed by `command.stop` ACE permission. (Server only) */
declare function StopResource(name: string): boolean;
/** Restarts `name` if allowed by ACE permission. (Server only) */
declare function RestartResource(name: string): boolean;

// =============================================================================
// Key-value storage (KVP)
// =============================================================================

declare function GetResourceKvpString(key: string): string | null;
declare function GetResourceKvpInt(key: string): number;
declare function GetResourceKvpFloat(key: string): number;
declare function SetResourceKvp(key: string, value: string): boolean;
declare function SetResourceKvpInt(key: string, value: number): boolean;
declare function SetResourceKvpFloat(key: string, value: number): boolean;
declare function DeleteResourceKvp(key: string): boolean;
declare function SetResourceKvpNoSync(key: string, value: string): boolean;
declare function SetResourceKvpIntNoSync(key: string, value: number): boolean;
declare function SetResourceKvpFloatNoSync(key: string, value: number): boolean;
declare function DeleteResourceKvpNoSync(key: string): boolean;
declare function StartFindKvp(prefix: string): number;
declare function FindKvp(handle: number): string | null;
declare function EndFindKvp(handle: number): void;
declare function FlushResourceKvp(): void;

// =============================================================================
// Players (server and client)
// =============================================================================

/** Returns connected player ids as decimal strings in join order. (Server only) */
declare function getPlayers(): string[];
/** Returns connected player ids as decimal strings in join order. (Server only) */
declare function GetPlayers(): string[];
/** Returns the number of connected players. (Server only) */
declare function GetNumPlayerIndices(): number;
declare function GetPlayerName(player: number | string): string | null;
declare function GetPlayerEndpoint(player: number | string): string | null;
declare function GetPlayerPing(player: number | string): number;
declare function GetPlayerLastMsg(player: number | string): number;
declare function GetPlayerPed(player: number | string): number;
declare function GetPlayerIdentifiers(player: number | string): string[];
declare function GetNumPlayerIdentifiers(player: number | string): number;
declare function GetPlayerIdentifier(player: number | string, index: number): string | null;
declare function GetPlayerIdentifierByType(player: number | string, kind: string): string | null;
/** Returns the hex part of the player's `license:` identifier. (Server only) */
declare function GetPlayerGuid(player: number | string): string | undefined;
/** Returns whole seconds since the player was admitted. (Server only) */
declare function GetPlayerTimeOnline(player: number | string): number | undefined;
/** Returns the player at 0-based `index` of the join-order list as a string. (Server only) */
declare function GetPlayerFromIndex(index: number): string | undefined;
/** Returns the lowest connected player id as a string. (Server only) */
declare function GetHostId(): string | undefined;
declare function GetNumPlayerTokens(player: number | string): number;
declare function GetPlayerToken(player: number | string, index: number): string | null;
declare function DropPlayer(player: number | string, reason?: string): void;
declare function DoesPlayerExist(player: number | string): boolean;
declare function IsPlayerAceAllowed(player: number | string, object: string): boolean;
declare function IsPrincipalAceAllowed(principal: string, object: string): boolean;
/** Returns whether the calling resource (`resource.<name>`) is allowed ACE `object`. (Server only) */
declare function IsAceAllowed(object: string): boolean;

/** Returns the local client player handle (`0`). (Client only) */
declare function PlayerId(): number;
/** Returns the local player's server id when `player` is `0`, or `null`. (Client only) */
declare function GetPlayerServerId(player: number | string): number | null;
/** Returns the local player's ped entity id, or `0` until received. (Client only) */
declare function PlayerPedId(): number;

// =============================================================================
// Entities and routing buckets
// =============================================================================

declare function DoesEntityExist(entity: number): boolean;
declare function GetEntityCoords(entity: number, alive?: boolean): Vector3;
declare function GetEntityHeading(entity: number): number;
declare function GetEntityRotation(entity: number, rotationOrder?: number): Vector3;
declare function GetEntityQuaternion(entity: number): [number, number, number, number];
declare function GetEntityModel(entity: number): number;
declare function GetEntityType(entity: number): 0 | 1 | 2 | 3;
declare function GetEntityScript(entity: number): string | null;
declare function NetworkGetNetworkIdFromEntity(entity: number): number;
declare function NetworkGetEntityFromNetworkId(networkId: number): number;
declare function NetworkDoesNetworkIdExist(networkId: number): boolean;
declare function NetworkGetEntityOwner(entity: number): number;
declare function NetworkGetFirstEntityOwner(entity: number): number;
declare function NetworkRequestControlOfEntity(entity: number): boolean;
declare function IsEntityPositionFrozen(entity: number): boolean;
declare function GetEntityVelocity(entity: number): Vector3;
declare function GetEntityHealth(entity: number): number;
declare function GetEntityMaxHealth(entity: number): number;
declare function IsEntityVisible(entity: number): boolean;
declare function GetEntityAlpha(entity: number): number;
declare function GetEntityAttachedTo(entity: number): number;
declare function IsEntityAttached(entity: number): boolean;

declare function CreateVehicle(
    model: number | string,
    x: number,
    y: number,
    z: number,
    heading: number,
    isNetwork?: boolean,
    netMissionEntity?: boolean
): number;
declare function CreateVehicleServerSetter(
    model: number | string,
    type: string,
    x: number,
    y: number,
    z: number,
    heading: number
): number;
declare function CreatePed(
    pedType: number,
    model: number | string,
    x: number,
    y: number,
    z: number,
    heading: number,
    isNetwork?: boolean,
    bScriptHostPed?: boolean
): number;
declare function CreateObject(
    model: number | string,
    x: number,
    y: number,
    z: number,
    isNetwork?: boolean,
    netMissionEntity?: boolean,
    doorFlag?: boolean
): number;
declare function CreateObjectNoOffset(
    model: number | string,
    x: number,
    y: number,
    z: number,
    isNetwork?: boolean,
    netMissionEntity?: boolean,
    doorFlag?: boolean
): number;
declare function DeleteEntity(entity: number): void;

declare function SetEntityCoords(
    entity: number,
    xPos: number,
    yPos: number,
    zPos: number,
    xAxis?: boolean,
    yAxis?: boolean,
    zAxis?: boolean,
    clearArea?: boolean
): void;
declare function SetEntityCoordsNoOffset(
    entity: number,
    xPos: number,
    yPos: number,
    zPos: number,
    xAxis?: boolean,
    yAxis?: boolean,
    zAxis?: boolean
): void;
declare function SetEntityHeading(entity: number, heading: number): void;
declare function SetEntityRotation(
    entity: number,
    pitch: number,
    roll: number,
    yaw: number,
    rotationOrder?: number,
    bDeadCheck?: boolean
): void;
declare function SetEntityQuaternion(
    entity: number,
    x: number,
    y: number,
    z: number,
    w: number
): void;

declare function SetEntityOwner(entity: number, player: number): boolean;
declare function SetEntityOrphanMode(entity: number, mode: "delete" | "migrate"): void;
declare function SetEntityDistanceCullingRadius(entity: number, radius: number): void;
declare function FreezeEntityPosition(entity: number, toggle: boolean): void;
declare function SetEntityVelocity(entity: number, x: number, y: number, z: number): void;
declare function SetEntityHealth(entity: number, health: number): void;
declare function SetEntityMaxHealth(entity: number, maxHealth: number): void;
declare function SetEntityVisible(entity: number, toggle: boolean, unk?: boolean): void;
declare function SetEntityAlpha(entity: number, alpha: number, skin?: boolean): void;
declare function ResetEntityAlpha(entity: number): void;
declare function AttachEntityToEntity(
    entity: number,
    target: number,
    boneIndex: number,
    x: number,
    y: number,
    z: number,
    rx: number,
    ry: number,
    rz: number,
    p9?: boolean,
    useSoftPinning?: boolean,
    collision?: boolean,
    isPed?: boolean,
    vertexIndex?: number,
    fixedRot?: boolean
): void;
declare function DetachEntity(entity: number, dynamic?: boolean, collision?: boolean): void;

declare function GetAllVehicles(): number[];
declare function GetAllPeds(): number[];
declare function GetAllObjects(): number[];
declare function GetGamePool(name: "CVehicle" | "CPed" | "CObject" | string): number[];

declare function SetPlayerRoutingBucket(player: number | string, bucket: number): void;
declare function GetPlayerRoutingBucket(player: number | string): number;
declare function SetEntityRoutingBucket(entity: number, bucket: number): void;
declare function GetEntityRoutingBucket(entity: number): number;
declare function GetPlayersInRoutingBucket(bucket: number): number[];
declare function SetRoutingBucketPopulationEnabled(bucket: number, enabled: boolean): void;
declare function GetRoutingBucketPopulationEnabled(bucket: number): boolean;
declare function SetRoutingBucketEntityLockdownMode(
    bucket: number,
    mode: "inactive" | "relaxed" | "strict"
): void;
declare function GetRoutingBucketEntityLockdownMode(
    bucket: number
): "inactive" | "relaxed" | "strict";

// =============================================================================
// HTTP (server only)
// =============================================================================

interface SixMHttpRequestOptions {
    /** Request timeout in milliseconds. */
    timeout?: number;
}

interface SixMHttpRequest {
    method: string;
    path: string;
    query: string;
    headers: Record<string, string>;
    setDataHandler(callback: (body: string) => void): void;
}

interface SixMHttpResponse {
    writeHead(status: number, headers?: Record<string, string | number>): void;
    write(data?: string | Uint8Array | ArrayBuffer): void;
    send(data?: string | Uint8Array | ArrayBuffer): void;
}

/**
 * Starts an asynchronous HTTP request and calls `callback(statusCode, body, headers, errorData)`. (Server only)
 */
declare function PerformHttpRequest(
    url: string,
    callback: (
        statusCode: number,
        body: string,
        headers: Record<string, string>,
        errorData: string
    ) => void,
    method?: string,
    data?: string | Uint8Array | ArrayBuffer | null,
    headers?: Record<string, string | number> | null,
    options?: SixMHttpRequestOptions | null
): void;

/**
 * Registers the resource's HTTP handler for `/<resource>` and `/<resource>/...`. (Server only)
 */
declare function SetHttpHandler(
    handler: (req: SixMHttpRequest, res: SixMHttpResponse) => void
): void;

// =============================================================================
// General utilities and logging
// =============================================================================

/** Returns the case-insensitive JOAAT hash of `name` as a signed 32-bit integer. */
declare function GetHashKey(name: string): number;

/** Returns `true` on the server and `false` on the client. */
declare function IsDuplicityVersion(): boolean;

/** Invokes a registered client game-adapter native by its 32-bit hash. (Client only) */
declare function InvokeNative(hash: number, ...args: any[]): any;

/** Logs values to the SixM console prefixed with the resource name. */
declare function print(...args: any[]): void;

declare const console: {
    log(...args: any[]): void;
    info(...args: any[]): void;
    warn(...args: any[]): void;
    error(...args: any[]): void;
    debug(...args: any[]): void;
};
