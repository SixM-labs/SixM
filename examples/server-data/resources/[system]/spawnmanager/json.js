function parseSpawnArray(text) {
    if (GetInvokingResource() !== GetCurrentResourceName()) {
        throw new Error("parseSpawnArray is private to spawnmanager");
    }
    if (typeof text !== "string") throw new Error("loadSpawns: argument must be a JSON string");
    let parsed;
    try {
        parsed = JSON.parse(text);
    } catch (_) {
        throw new Error("loadSpawns: invalid JSON");
    }
    if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)
        || !Array.isArray(parsed.spawns)) {
        throw new Error("loadSpawns: JSON must be an object with a spawns array");
    }
    if (parsed.spawns.some(point => point === null || typeof point !== "object" || Array.isArray(point))) {
        throw new Error("loadSpawns: every spawn must be a non-null object");
    }
    return parsed.spawns;
}

exports("parseSpawnArray", parseSpawnArray);
