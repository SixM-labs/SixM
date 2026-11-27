-- Clean-room session lifecycle tracker for SixM.
-- Tracks joined players from `playerJoining` to `playerDropped`, completes a
-- ready handshake over `sixm_session:ready`, and mirrors readiness onto
-- `Player(src).state.sessionActive`.

local sessions = {}

local function normalizePlayerId(player)
    local id = tonumber(player)
    if not id or id < 1 or math.floor(id) ~= id then
        return nil
    end
    return math.floor(id)
end

local function setSessionStateBag(playerId, active)
    pcall(function()
        Player(playerId).state.sessionActive = active
    end)
end

local function createSession(playerId)
    local session = {
        id = playerId,
        name = GetPlayerName(playerId),
        joinedAt = GetGameTimer(),
        readyAt = nil,
        active = false,
    }
    sessions[playerId] = session
    return session
end

-- Seed any already-connected players if the resource is restarted at runtime.
for _, rawId in ipairs(GetPlayers()) do
    local id = normalizePlayerId(rawId)
    if id then
        local session = createSession(id)
        local active = false
        local ok, current = pcall(function()
            return Player(id).state.sessionActive == true
        end)
        if ok and current then
            active = true
            session.active = true
            session.readyAt = session.joinedAt
        end
        if not active then
            setSessionStateBag(id, false)
        end
    end
end

AddEventHandler('playerJoining', function()
    local src = normalizePlayerId(source)
    if not src then
        return
    end
    createSession(src)
    setSessionStateBag(src, false)
end)

RegisterNetEvent('sixm_session:ready', function()
    local src = normalizePlayerId(source)
    if not src or not GetPlayerName(src) then
        return
    end

    local session = sessions[src] or createSession(src)
    session.name = GetPlayerName(src) or session.name
    if not session.active then
        session.active = true
        session.readyAt = GetGameTimer()
    end

    setSessionStateBag(src, true)
    TriggerClientEvent('sixm_session:ready', src, true)
end)

AddEventHandler('playerDropped', function()
    local src = normalizePlayerId(source)
    if not src then
        return
    end
    sessions[src] = nil
    setSessionStateBag(src, false)
end)

exports('isPlayerActive', function(player)
    local id = normalizePlayerId(player)
    local session = id and sessions[id]
    return session ~= nil and session.active == true
end)

exports('getPlayerSession', function(player)
    local id = normalizePlayerId(player)
    local session = id and sessions[id]
    if not session then
        return nil
    end
    return {
        id = session.id,
        name = session.name,
        active = session.active,
        joinedAt = session.joinedAt,
        readyAt = session.readyAt,
    }
end)
