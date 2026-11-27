-- Load test, client side. Each client moves its own ped every frame on a
-- 20 m circle around a point up to 300 m from the origin. The point is seeded
-- by the server id, so every client has its own area. Every 5 seconds the
-- client pings the server.

local CIRCLE_RADIUS = 20.0
local AREA_RADIUS = 300.0
local GROUND = 72.0
local PING_EVERY = 5000

math.randomseed(GetPlayerServerId(PlayerId()) or 0)
local direction = math.random() * 2.0 * math.pi
local distance = math.sqrt(math.random()) * AREA_RADIUS
local centre_x = distance * math.cos(direction)
local centre_y = distance * math.sin(direction)

CreateThread(function()
    local angle = 0.0
    while true do
        Wait(0)
        local ped = PlayerPedId()
        if ped ~= 0 then
            angle = angle + 2.0
            local rad = math.rad(angle)
            SetEntityCoords(ped, centre_x + CIRCLE_RADIUS * math.cos(rad),
                centre_y + CIRCLE_RADIUS * math.sin(rad), GROUND)
        end
    end
end)

CreateThread(function()
    while true do
        Wait(PING_EVERY)
        TriggerServerEvent('loadtest:ping', GetGameTimer())
    end
end)
