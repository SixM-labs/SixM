"""Validate SixM IDE type definitions in types/sixm.lua and types/sixm.d.ts."""

from __future__ import annotations

from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parents[1]
LUA_TYPES = ROOT / "types" / "sixm.lua"
TS_TYPES = ROOT / "types" / "sixm.d.ts"

# Manifest directives documented in docs/manifest.md and required in types/sixm.lua.
MANIFEST_DIRECTIVES = [
    "fx_version",
    "game",
    "games",
    "server_script",
    "server_scripts",
    "client_script",
    "client_scripts",
    "shared_script",
    "shared_scripts",
    "file",
    "files",
    "dependency",
    "dependencies",
    "export",
    "exports",
    "server_export",
    "server_exports",
    "lua54",
    "provide",
    "provides",
    "data_file",
]

# Shared SixM API symbols present in both Lua and JS type definitions.
SHARED_API_SYMBOLS = [
    # Scheduling & Citizen
    "Wait",
    "CreateThread",
    "SetTimeout",
    "ClearTimeout",
    "GetGameTimer",
    "Citizen",
    # Events & source
    "AddEventHandler",
    "RemoveEventHandler",
    "RegisterNetEvent",
    "RegisterServerEvent",
    "TriggerEvent",
    "TriggerClientEvent",
    "TriggerServerEvent",
    "TriggerLatentClientEvent",
    "TriggerLatentServerEvent",
    "CancelEvent",
    "WasEventCanceled",
    "source",
    # Exports
    "exports",
    # Resources & files
    "GetCurrentResourceName",
    "GetResourcePath",
    "LoadResourceFile",
    "SaveResourceFile",
    "GetInvokingResource",
    "GetResourceState",
    "GetNumResources",
    "GetResourceByFindIndex",
    "GetResourceMetadata",
    "GetNumResourceMetadata",
    "StartResource",
    "StopResource",
    "RestartResource",
    # Key-value storage
    "GetResourceKvpString",
    "GetResourceKvpInt",
    "GetResourceKvpFloat",
    "SetResourceKvp",
    "SetResourceKvpInt",
    "SetResourceKvpFloat",
    "DeleteResourceKvp",
    "SetResourceKvpNoSync",
    "SetResourceKvpIntNoSync",
    "SetResourceKvpFloatNoSync",
    "DeleteResourceKvpNoSync",
    "StartFindKvp",
    "FindKvp",
    "EndFindKvp",
    "FlushResourceKvp",
    # Convars & commands
    "GetConvar",
    "GetConvarInt",
    "GetConvarFloat",
    "GetConvarBool",
    "SetConvar",
    "SetConvarReplicated",
    "SetConvarServerInfo",
    "AddConvarChangeListener",
    "RemoveConvarChangeListener",
    "ExecuteCommand",
    "RegisterCommand",
    "GetRegisteredCommands",
    # SQL bridge
    "SixmSqlIsReady",
    "SixmSqlExecute",
    "SixmSqlPrepare",
    "SixmSqlTransaction",
    # Players
    "GetPlayers",
    "GetNumPlayerIndices",
    "GetPlayerName",
    "GetPlayerPed",
    "GetPlayerEndpoint",
    "GetPlayerIdentifiers",
    "GetNumPlayerIdentifiers",
    "GetPlayerIdentifier",
    "GetPlayerIdentifierByType",
    "GetPlayerLastMsg",
    "GetPlayerPing",
    "GetNumPlayerTokens",
    "GetPlayerToken",
    "DropPlayer",
    "DoesPlayerExist",
    "IsPlayerAceAllowed",
    "IsPrincipalAceAllowed",
    "PlayerId",
    "GetPlayerServerId",
    "PlayerPedId",
    # State bags
    "GlobalState",
    "Player",
    "LocalPlayer",
    "Entity",
    "AddStateBagChangeHandler",
    "RemoveStateBagChangeHandler",
    "GetPlayerFromStateBagName",
    "GetEntityFromStateBagName",
    # Vectors & quaternions
    "vector2",
    "vec2",
    "vector3",
    "vec3",
    "vector4",
    "vec4",
    "quat",
    # Entities
    "CreateVehicle",
    "CreateVehicleServerSetter",
    "CreatePed",
    "CreateObject",
    "CreateObjectNoOffset",
    "DeleteEntity",
    "SetEntityCoords",
    "SetEntityCoordsNoOffset",
    "SetEntityHeading",
    "SetEntityRotation",
    "SetEntityQuaternion",
    "NetworkGetEntityOwner",
    "NetworkGetFirstEntityOwner",
    "NetworkRequestControlOfEntity",
    "SetEntityOwner",
    "SetEntityOrphanMode",
    "SetEntityDistanceCullingRadius",
    "GetAllVehicles",
    "GetAllPeds",
    "GetAllObjects",
    "GetGamePool",
    "DoesEntityExist",
    "GetEntityCoords",
    "GetEntityHeading",
    "GetEntityRotation",
    "GetEntityQuaternion",
    "GetEntityModel",
    "GetEntityType",
    "GetEntityScript",
    "NetworkGetNetworkIdFromEntity",
    "NetworkGetEntityFromNetworkId",
    "NetworkDoesNetworkIdExist",
    "FreezeEntityPosition",
    "IsEntityPositionFrozen",
    "SetEntityVelocity",
    "GetEntityVelocity",
    "SetEntityHealth",
    "GetEntityHealth",
    "SetEntityMaxHealth",
    "GetEntityMaxHealth",
    "SetEntityVisible",
    "IsEntityVisible",
    "SetEntityAlpha",
    "GetEntityAlpha",
    "ResetEntityAlpha",
    "AttachEntityToEntity",
    "DetachEntity",
    "GetEntityAttachedTo",
    "IsEntityAttached",
    # Routing buckets
    "SetPlayerRoutingBucket",
    "GetPlayerRoutingBucket",
    "SetEntityRoutingBucket",
    "GetEntityRoutingBucket",
    "GetPlayersInRoutingBucket",
    "SetRoutingBucketPopulationEnabled",
    "GetRoutingBucketPopulationEnabled",
    "SetRoutingBucketEntityLockdownMode",
    "GetRoutingBucketEntityLockdownMode",
    # HTTP & utilities
    "PerformHttpRequest",
    "SetHttpHandler",
    "GetHashKey",
    "IsDuplicityVersion",
    "print",
    "msgpack",
]

LUA_ONLY_SYMBOLS = [
    "CreateThreadNow",
    "promise",
    "Await",
    "json",
    "Citizen.Wait",
    "Citizen.CreateThread",
    "Citizen.CreateThreadNow",
    "Citizen.SetTimeout",
    "Citizen.ClearTimeout",
    "Citizen.Await",
    "Citizen.InvokeNative",
    "promise.new",
    "promise.all",
    "promise.first",
    "json.encode",
    "json.decode",
    "msgpack.pack",
    "msgpack.unpack",
]

JS_ONLY_SYMBOLS = [
    "setTick",
    "clearTick",
    "setTimeout",
    "clearTimeout",
    "setInterval",
    "clearInterval",
    "Delay",
    "on",
    "onNet",
    "emit",
    "emitNet",
    "removeEventListener",
    "getPlayers",
    "Vector2",
    "Vector3",
    "Vector4",
    "InvokeNative",
    "console",
]


def read_utf8_non_empty(path: Path) -> str:
    """Ensure `path` exists, is non-empty, and decodes cleanly as UTF-8."""
    if not path.is_file():
        raise AssertionError(f"missing type file: {path}")
    text = path.read_text(encoding="utf-8")
    if not text.strip():
        raise AssertionError(f"empty type file: {path}")
    return text


def _match_lua_long_bracket(text: str, pos: int) -> int | None:
    """If `text[pos:]` starts a Lua long bracket `[=*[`, return its `=` count."""
    if pos >= len(text) or text[pos] != "[":
        return None
    idx = pos + 1
    while idx < len(text) and text[idx] == "=":
        idx += 1
    if idx < len(text) and text[idx] == "[":
        return idx - pos - 1
    return None


def check_lua_balance(text: str) -> None:
    """Verify balanced `()`, `{}`, and `[]` in Lua source, ignoring comments/strings."""
    pairs = {")": "(", "}": "{", "]": "["}
    stack: list[tuple[str, int]] = []
    line = 1
    i = 0
    n = len(text)

    while i < n:
        ch = text[i]
        if ch == "\n":
            line += 1
            i += 1
            continue

        # Comments (`--` or `--[=[ ... ]=]`)
        if text.startswith("--", i):
            eq_count = _match_lua_long_bracket(text, i + 2)
            if eq_count is not None:
                closing = "]" + "=" * eq_count + "]"
                end = text.find(closing, i + 4 + eq_count)
                if end == -1:
                    raise AssertionError(f"unterminated Lua block comment at line {line}")
                line += text[i : end + len(closing)].count("\n")
                i = end + len(closing)
            else:
                nl = text.find("\n", i)
                i = n if nl == -1 else nl
            continue

        # Long string (`[[ ... ]]` or `[=[ ... ]=]`)
        eq_count = _match_lua_long_bracket(text, i)
        if eq_count is not None:
            closing = "]" + "=" * eq_count + "]"
            end = text.find(closing, i + 2 + eq_count)
            if end == -1:
                raise AssertionError(f"unterminated Lua long string at line {line}")
            line += text[i : end + len(closing)].count("\n")
            i = end + len(closing)
            continue

        # Quoted string
        if ch in ("'", '"'):
            quote = ch
            start_line = line
            i += 1
            while i < n:
                if text[i] == "\\":
                    i += 2
                    continue
                if text[i] == quote:
                    i += 1
                    break
                if text[i] == "\n":
                    raise AssertionError(f"unterminated Lua string at line {start_line}")
                i += 1
            else:
                raise AssertionError(f"unterminated Lua string at line {start_line}")
            continue

        if ch in ("(", "{", "["):
            stack.append((ch, line))
        elif ch in pairs:
            if not stack or stack[-1][0] != pairs[ch]:
                raise AssertionError(f"unmatched Lua '{ch}' at line {line}")
            stack.pop()
        i += 1

    if stack:
        open_ch, open_line = stack[-1]
        raise AssertionError(f"unclosed Lua '{open_ch}' opened at line {open_line}")


def check_ts_balance(text: str) -> None:
    """Verify balanced `()`, `{}`, and `[]` in TypeScript, ignoring comments/strings."""
    pairs = {")": "(", "}": "{", "]": "["}
    stack: list[tuple[str, int]] = []
    line = 1
    i = 0
    n = len(text)

    while i < n:
        ch = text[i]
        if ch == "\n":
            line += 1
            i += 1
            continue

        # Line comment
        if text.startswith("//", i):
            nl = text.find("\n", i)
            i = n if nl == -1 else nl
            continue

        # Block comment
        if text.startswith("/*", i):
            end = text.find("*/", i + 2)
            if end == -1:
                raise AssertionError(f"unterminated TypeScript comment at line {line}")
            line += text[i : end + 2].count("\n")
            i = end + 2
            continue

        # Quoted or template string
        if ch in ("'", '"', "`"):
            quote = ch
            start_line = line
            i += 1
            while i < n:
                if text[i] == "\\":
                    if i + 1 < n and text[i + 1] == "\n":
                        line += 1
                    i += 2
                    continue
                if text[i] == "\n":
                    if quote != "`":
                        raise AssertionError(f"unterminated TypeScript string at line {start_line}")
                    line += 1
                if text[i] == quote:
                    i += 1
                    break
                i += 1
            else:
                raise AssertionError(f"unterminated TypeScript string at line {start_line}")
            continue

        if ch in ("(", "{", "["):
            stack.append((ch, line))
        elif ch in pairs:
            if not stack or stack[-1][0] != pairs[ch]:
                raise AssertionError(f"unmatched TypeScript '{ch}' at line {line}")
            stack.pop()
        i += 1

    if stack:
        open_ch, open_line = stack[-1]
        raise AssertionError(f"unclosed TypeScript '{open_ch}' opened at line {open_line}")


def check_symbols(label: str, text: str, symbols: list[str]) -> list[str]:
    """Return any symbols from `symbols` not declared in `text`."""
    missing = []
    for sym in symbols:
        pattern = re.compile(rf"(?<![\w.]){re.escape(sym)}(?![\w])")
        if not pattern.search(text):
            missing.append(f"{label}: missing symbol `{sym}`")
    return missing


def main() -> int:
    lua_text = read_utf8_non_empty(LUA_TYPES)
    ts_text = read_utf8_non_empty(TS_TYPES)

    if not lua_text.lstrip().startswith("---@meta"):
        raise AssertionError("types/sixm.lua must begin with `---@meta`")

    check_lua_balance(lua_text)
    check_ts_balance(ts_text)

    errors: list[str] = []
    errors.extend(check_symbols("types/sixm.lua", lua_text, MANIFEST_DIRECTIVES))
    errors.extend(check_symbols("types/sixm.lua", lua_text, SHARED_API_SYMBOLS))
    errors.extend(check_symbols("types/sixm.lua", lua_text, LUA_ONLY_SYMBOLS))
    errors.extend(check_symbols("types/sixm.d.ts", ts_text, SHARED_API_SYMBOLS))
    errors.extend(check_symbols("types/sixm.d.ts", ts_text, JS_ONLY_SYMBOLS))

    # Verify JS msgpack and Citizen namespace members explicitly.
    for method in ("pack(value: unknown): Uint8Array", "unpack(data: Uint8Array | ArrayBuffer): unknown"):
        if method not in ts_text:
            errors.append(f"types/sixm.d.ts: missing msgpack signature `{method}`")

    citizen_match = re.search(r"declare namespace Citizen\s*\{([\s\S]*?)\n\}", ts_text)
    if not citizen_match:
        errors.append("types/sixm.d.ts: missing `declare namespace Citizen` block")
    else:
        citizen_body = citizen_match.group(1)
        for member in (
            "setTick",
            "clearTick",
            "setTimeout",
            "clearTimeout",
            "setInterval",
            "clearInterval",
            "Delay",
            "Wait",
            "CreateThread",
            "InvokeNative",
        ):
            if not re.search(rf"\bfunction {re.escape(member)}\(", citizen_body):
                errors.append(f"types/sixm.d.ts: missing `Citizen.{member}`")

    if errors:
        for err in errors:
            print(f"ERROR: {err}", file=sys.stderr)
        return 1

    lua_count = len(MANIFEST_DIRECTIVES) + len(SHARED_API_SYMBOLS) + len(LUA_ONLY_SYMBOLS)
    ts_count = len(SHARED_API_SYMBOLS) + len(JS_ONLY_SYMBOLS)
    print(
        f"OK: validated {LUA_TYPES.relative_to(ROOT)} ({lua_count} symbols) "
        f"and {TS_TYPES.relative_to(ROOT)} ({ts_count} symbols)."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
