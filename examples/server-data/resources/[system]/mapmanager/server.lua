local currentGameType
local currentMap
local transactionDepth = 0
local pendingLifecycle = {}

local function resourceType(resource)
    return GetResourceMetadata(resource, 'resource_type', 0)
end

local function displayName(resource)
    return GetResourceMetadata(resource, 'name', 0) or resource
end

local function resourceNames()
    local names = {}
    for index = 0, GetNumResources() - 1 do
        local name = GetResourceByFindIndex(index)
        if name then
            names[#names + 1] = name
        end
    end
    table.sort(names)
    return names
end

local function isCanonicalResource(name)
    if type(name) ~= 'string' or name == '' then
        return false
    end
    for _, resource in ipairs(resourceNames()) do
        if resource == name then
            return true
        end
    end
    return false
end

local function metadataValues(resource, key)
    local values = {}
    for index = 0, GetNumResourceMetadata(resource, key) - 1 do
        local value = GetResourceMetadata(resource, key, index)
        if value then
            values[#values + 1] = value
        end
    end
    return values
end

local function running(resource)
    return GetResourceState(resource) == 'started'
end

local function findRunning(resourceTypeName)
    for _, resource in ipairs(resourceNames()) do
        if resourceType(resource) == resourceTypeName and running(resource) then
            return resource
        end
    end
end

local function supports(map, gameType)
    if resourceType(map) ~= 'map' or resourceType(gameType) ~= 'gametype' then
        return false
    end
    local allowed = metadataValues(map, 'gameTypes')
    if #allowed == 0 then
        return true
    end
    for _, candidate in ipairs(allowed) do
        if candidate == gameType then
            return true
        end
    end
    return false
end

local function reconcile(preferred)
    local previousMap = currentMap
    local previousGameType = currentGameType
    local preferredType = preferred and resourceType(preferred)
    if preferredType == 'map' and running(preferred) then
        currentMap = preferred
    elseif not currentMap or not running(currentMap) then
        currentMap = findRunning('map')
    end
    if preferredType == 'gametype' and running(preferred) then
        currentGameType = preferred
    elseif not currentGameType or not running(currentGameType) then
        currentGameType = findRunning('gametype')
    end
    if currentMap ~= previousMap then
        SetMapName(currentMap and displayName(currentMap) or '')
    end
    if currentGameType ~= previousGameType then
        SetGameType(currentGameType and displayName(currentGameType) or '')
    end
end

local function recordLifecycle(name)
    if resourceType(name) ~= 'map' and resourceType(name) ~= 'gametype' then
        return
    end
    if transactionDepth > 0 then
        pendingLifecycle[name] = true
        return
    end
    reconcile(name)
    if running(name) then
        if resourceType(name) == 'map' then
            TriggerEvent('onMapStart', name)
        else
            TriggerEvent('onGameTypeStart', name)
        end
    end
end

AddEventHandler('onResourceStart', function(name)
    recordLifecycle(name)
end)

AddEventHandler('onResourceStop', function(name)
    if transactionDepth == 0 and (resourceType(name) == 'map' or resourceType(name) == 'gametype') then
        reconcile()
    end
end)

local function withTransaction(callback)
    transactionDepth = transactionDepth + 1
    local ok, result = pcall(callback)
    transactionDepth = transactionDepth - 1
    if transactionDepth == 0 then
        local names = {}
        for name in pairs(pendingLifecycle) do
            names[#names + 1] = name
        end
        pendingLifecycle = {}
        table.sort(names)
        reconcile()
        for _, name in ipairs(names) do
            if running(name) then
                if resourceType(name) == 'map' then
                    TriggerEvent('onMapStart', name)
                elseif resourceType(name) == 'gametype' then
                    TriggerEvent('onGameTypeStart', name)
                end
            end
        end
    end
    if not ok then
        print(('mapmanager: transaction failed: %s'):format(tostring(result)))
        return false
    end
    return result
end

local function snapshotRunning()
    local snapshot = {}
    for _, resource in ipairs(resourceNames()) do
        if running(resource) then
            snapshot[resource] = true
        end
    end
    return snapshot
end

local function restoreSnapshot(snapshot)
    local names = resourceNames()
    for index = #names, 1, -1 do
        local resource = names[index]
        if running(resource) and not snapshot[resource] and IsAceAllowed('command.stop') then
            if not StopResource(resource) and running(resource) then
                print(('mapmanager: rollback could not stop newly started `%s`'):format(resource))
            end
        end
    end
    for _ = 1, #names do
        local changed = false
        for _, resource in ipairs(names) do
            if snapshot[resource] and not running(resource) then
                if IsAceAllowed('command.start') and StartResource(resource) and running(resource) then
                    changed = true
                end
            end
        end
        if not changed then
            break
        end
    end
    for resource in pairs(snapshot) do
        if not running(resource) then
            print(('mapmanager: rollback left previously running `%s` stopped'):format(resource))
        end
    end
    reconcile()
end

local function stopIfRunning(resource)
    return not resource or not running(resource) or StopResource(resource)
end

local function startIfStopped(resource)
    if running(resource) then
        return true
    end
    return StartResource(resource) and running(resource)
end

local function committedStateMatches(snapshot, allowedStops, target)
    if not running(target) then
        return false
    end
    for resource in pairs(snapshot) do
        if not allowedStops[resource] and not running(resource) then
            return false
        end
    end
    return true
end

local function commit(resourceTypeName, target, incompatible)
    local previous
    if resourceTypeName == 'map' then
        previous = currentMap
    else
        previous = currentGameType
    end
    if target == previous and running(target) then
        return true
    end
    local before = snapshotRunning()
    local previousWasRunning = previous and before[previous] or false
    local incompatibleWasRunning = incompatible and before[incompatible] or false
    local allowedStops = {}
    if previousWasRunning then
        allowedStops[previous] = true
    end
    if incompatibleWasRunning then
        allowedStops[incompatible] = true
    end
    if not IsAceAllowed('command.start') or not IsAceAllowed('command.stop') then
        return false
    end
    return withTransaction(function()
        if not startIfStopped(target) then
            restoreSnapshot(before)
            return false
        end
        if incompatibleWasRunning and not stopIfRunning(incompatible) then
            restoreSnapshot(before)
            return false
        end
        if previousWasRunning and previous ~= target and not stopIfRunning(previous) then
            restoreSnapshot(before)
            return false
        end
        if not committedStateMatches(before, allowedStops, target) then
            restoreSnapshot(before)
            return false
        end
        if resourceTypeName == 'map' then
            currentMap = target
            SetMapName(displayName(target))
        else
            currentGameType = target
            SetGameType(displayName(target))
            if incompatibleWasRunning then
                currentMap = nil
                SetMapName('')
            end
        end
        return true
    end)
end

local function enumerateMaps()
    local maps = {}
    for _, resource in ipairs(resourceNames()) do
        if resourceType(resource) == 'map' then
            local gameTypes = metadataValues(resource, 'gameTypes')
            maps[resource] = {
                name = displayName(resource),
                gameTypes = #gameTypes > 0 and gameTypes or nil,
            }
        end
    end
    return maps
end

exports('getCurrentGameType', function()
    return currentGameType
end)

exports('getCurrentMap', function()
    return currentMap
end)

exports('doesMapSupportGameType', function(gameType, map)
    if not isCanonicalResource(gameType) or not isCanonicalResource(map) then
        return false
    end
    return supports(map, gameType)
end)

exports('getMaps', enumerateMaps)

exports('changeGameType', function(gameType)
    if not isCanonicalResource(gameType) or resourceType(gameType) ~= 'gametype' then
        return false
    end
    if gameType == currentGameType and running(gameType) then
        return true
    end
    local incompatible = currentMap and not supports(currentMap, gameType) and currentMap or nil
    local ok = commit('gametype', gameType, incompatible)
    if not ok then
        reconcile()
    end
    return ok
end)

exports('changeMap', function(map)
    if not isCanonicalResource(map) or resourceType(map) ~= 'map' then
        return false
    end
    if currentGameType and not supports(map, currentGameType) then
        return false
    end
    if map == currentMap and running(map) then
        return true
    end
    local ok = commit('map', map, nil)
    if not ok then
        reconcile()
    end
    return ok
end)

exports('roundEnded', function()
    if not currentGameType or not running(currentGameType) or not IsAceAllowed('command.restart') then
        return false
    end
    local before = snapshotRunning()
    return withTransaction(function()
        local gameType = currentGameType
        local ok = RestartResource(gameType)
        if not ok or not running(gameType) then
            restoreSnapshot(before)
            return false
        end
        for resource in pairs(before) do
            if not running(resource) then
                restoreSnapshot(before)
                return false
            end
        end
        reconcile()
        return true
    end)
end)

local function warnMultipleRunning(resourceTypeName)
    local found = {}
    for _, resource in ipairs(resourceNames()) do
        if resourceType(resource) == resourceTypeName and running(resource) then
            found[#found + 1] = resource
        end
    end
    if #found > 1 then
        print(('mapmanager: multiple %s resources are already running; using `%s` and leaving the others running'):format(resourceTypeName, found[1]))
    end
end

reconcile()
warnMultipleRunning('map')
warnMultipleRunning('gametype')
