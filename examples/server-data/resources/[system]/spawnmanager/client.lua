-- Game-independent client spawning. Model loading, fading and death detection
-- require a game adapter.
local spawnPoints = {}
local nextSpawnId = 1
local autoSpawnEnabled = false
local autoSpawnEpoch = 0
local initialAttempted = false
local autoSpawnCallback
local retryTimer
local initialTimer
local pendingTimers = {}

local function fail(name, message)
    error(name .. ': ' .. message, 3)
end

local function finiteCoordinate(value, name, field)
    if type(value) ~= 'number' or value ~= value or value == math.huge or value == -math.huge
        or math.abs(value) > 3.402823466e38 then
        fail(name, field .. ' must be a finite, representable number')
    end
    return value
end

local function normalizeSpawn(value, name)
    if type(value) ~= 'table' then fail(name, 'spawn must be an object') end
    local spawn = {
        x = finiteCoordinate(value.x, name, 'x'),
        y = finiteCoordinate(value.y, name, 'y'),
        z = finiteCoordinate(value.z, name, 'z'),
        heading = finiteCoordinate(value.heading, name, 'heading'),
        skipFade = false,
    }
    if value.model ~= nil then
        if type(value.model) == 'string' and #value.model > 0 then
            spawn.model = GetHashKey(value.model)
        elseif type(value.model) == 'number' and value.model % 1 == 0
            and value.model >= -2147483648 and value.model <= 4294967295 then
            local hash = value.model
            if hash > 2147483647 then hash = hash - 4294967296 end
            local intHash = math.tointeger(hash)
            if intHash == nil then
                fail(name, 'model must be a nonempty name or 32-bit hash')
            end
            spawn.model = intHash
        else
            fail(name, 'model must be a nonempty name or 32-bit hash')
        end
    end
    if value.skipFade ~= nil then
        if type(value.skipFade) ~= 'boolean' then fail(name, 'skipFade must be a boolean') end
        spawn.skipFade = value.skipFade
    end
    return spawn
end

local function allocateId(name)
    if nextSpawnId >= math.maxinteger then fail(name, 'spawn point id space exhausted') end
    local id = nextSpawnId
    nextSpawnId = nextSpawnId + 1
    return id
end

local function localPedExists()
    local ped = PlayerPedId()
    return ped ~= 0 and DoesEntityExist(ped)
end

local function scheduleTimer(delay, callback)
    local timer
    timer = SetTimeout(delay, function()
        pendingTimers[timer] = nil
        callback()
    end)
    pendingTimers[timer] = true
    return timer
end

local function cancelTimer(timer)
    if timer ~= nil and pendingTimers[timer] then
        ClearTimeout(timer)
        pendingTimers[timer] = nil
    end
end

local function cancelAllTimers()
    for timer in pairs(pendingTimers) do
        ClearTimeout(timer)
    end
    pendingTimers = {}
    retryTimer = nil
    initialTimer = nil
end

local function cancelRetry()
    if retryTimer ~= nil then
        cancelTimer(retryTimer)
        retryTimer = nil
    end
end

local function isCallable(value)
    if type(value) == 'function' then return true end
    return type(value) == 'userdata'
        and tostring(value):match('^function: [^#]+#%d+$') ~= nil
end

local spawnPlayer
local function considerInitialSpawn()
    if not autoSpawnEnabled or initialAttempted then return end
    local hasPoint = next(spawnPoints) ~= nil
    if not hasPoint and autoSpawnCallback == nil then return end
    if not localPedExists() then
        if retryTimer == nil then
            retryTimer = scheduleTimer(0, function()
                retryTimer = nil
                if autoSpawnEnabled and not initialAttempted then
                    considerInitialSpawn()
                end
            end)
        end
        return
    end
    if initialTimer ~= nil then return end
    initialTimer = scheduleTimer(0, function()
        initialTimer = nil
        if not autoSpawnEnabled or initialAttempted then return end
        if next(spawnPoints) == nil and autoSpawnCallback == nil then return end
        if not localPedExists() then
            considerInitialSpawn()
            return
        end
        initialAttempted = true
        if autoSpawnCallback ~= nil then autoSpawnCallback()
        else spawnPlayer() end
    end)
end

local function addSpawnPoint(input)
    local point = normalizeSpawn(input, 'addSpawnPoint')
    local id = allocateId('addSpawnPoint')
    spawnPoints[id] = point
    considerInitialSpawn()
    return id
end

local function removeSpawnPoint(id)
    local intId = type(id) == 'number' and math.tointeger(id) or nil
    if intId == nil or intId <= 0 then
        fail('removeSpawnPoint', 'id must be a positive integer')
    end
    if spawnPoints[intId] == nil then return false end
    spawnPoints[intId] = nil
    return true
end

local function loadSpawns(text)
    if type(text) ~= 'string' then fail('loadSpawns', 'argument must be a JSON string') end
    local ok, sequence = pcall(function()
        return exports[GetCurrentResourceName()].parseSpawnArray(text)
    end)
    if not ok then fail('loadSpawns', tostring(sequence)) end
    if type(sequence) ~= 'table' then fail('loadSpawns', 'internal JSON parser returned an invalid array') end
    local count, maximum = 0, 0
    for key in pairs(sequence) do
        if type(key) ~= 'number' or key % 1 ~= 0 or key <= 0 then
            fail('loadSpawns', 'spawns must be a sequence with positive integer keys')
        end
        count = count + 1
        if key > maximum then maximum = key end
    end
    if count ~= maximum then fail('loadSpawns', 'spawns must not contain holes or null entries') end

    local points = {}
    for index = 1, maximum do
        local point = sequence[index]
        if point == nil then fail('loadSpawns', 'spawns must not contain null entries') end
        points[index] = normalizeSpawn(point, 'loadSpawns')
    end
    if #points > math.maxinteger - nextSpawnId then fail('loadSpawns', 'spawn point id space exhausted') end

    for _, point in ipairs(points) do
        spawnPoints[allocateId('loadSpawns')] = point
    end
    considerInitialSpawn()
    return true
end

spawnPlayer = function(spawnIdOrObject, callback)
    if callback ~= nil and not isCallable(callback) then
        fail('spawnPlayer', 'callback must be a function')
    end
    local point, id
    if spawnIdOrObject == nil then
        local ids = {}
        for spawnId in pairs(spawnPoints) do ids[#ids + 1] = spawnId end
        if #ids == 0 then return false end
        id = ids[math.random(#ids)]
        point = spawnPoints[id]
    elseif type(spawnIdOrObject) == 'number' then
        local intId = math.tointeger(spawnIdOrObject)
        if intId == nil or intId <= 0 then
            fail('spawnPlayer', 'spawn id must be a positive integer')
        end
        id = intId
        point = spawnPoints[id]
        if point == nil then return false end
    else
        point = normalizeSpawn(spawnIdOrObject, 'spawnPlayer')
    end
    local ped = PlayerPedId()
    if ped == 0 or not DoesEntityExist(ped)
        or IsEntityPositionFrozen(ped) or IsEntityAttached(ped) then
        return false
    end

    local info = {
        x = point.x,
        y = point.y,
        z = point.z,
        heading = point.heading,
        model = point.model or GetEntityModel(ped),
        skipFade = point.skipFade,
    }
    if id ~= nil then info.idx = id end
    SetEntityCoords(ped, point.x, point.y, point.z)
    SetEntityHeading(ped, point.heading)
    scheduleTimer(0, function()
        TriggerEvent('playerSpawned', info)
        if callback ~= nil then callback(info) end
    end)
    return true
end

local function setAutoSpawn(enabled)
    if type(enabled) ~= 'boolean' then fail('setAutoSpawn', 'enabled must be a boolean') end
    autoSpawnEnabled = enabled
    if not enabled then
        autoSpawnEpoch = autoSpawnEpoch + 1
        cancelRetry()
        if initialTimer ~= nil then
            cancelTimer(initialTimer)
            initialTimer = nil
        end
    else
        considerInitialSpawn()
    end
end

local function setAutoSpawnCallback(callback)
    if not isCallable(callback) then fail('setAutoSpawnCallback', 'callback must be a function') end
    autoSpawnCallback = callback
    considerInitialSpawn()
end

local function forceRespawn()
    if not autoSpawnEnabled then return end
    cancelRetry()
    if initialTimer ~= nil then
        cancelTimer(initialTimer)
        initialTimer = nil
    end
    initialAttempted = true
    local epoch = autoSpawnEpoch
    scheduleTimer(0, function()
        if not autoSpawnEnabled or epoch ~= autoSpawnEpoch then return end
        if autoSpawnCallback ~= nil then autoSpawnCallback()
        else spawnPlayer() end
    end)
end

AddEventHandler('onResourceStop', function(resource)
    if resource == GetCurrentResourceName() then cancelAllTimers() end
end)

exports('spawnPlayer', spawnPlayer)
exports('addSpawnPoint', addSpawnPoint)
exports('removeSpawnPoint', removeSpawnPoint)
exports('loadSpawns', loadSpawns)
exports('setAutoSpawn', setAutoSpawn)
exports('setAutoSpawnCallback', setAutoSpawnCallback)
exports('forceRespawn', forceRespawn)
