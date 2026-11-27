-- Load test, server side. The server owns `loadtest_vehicles` vehicles (500 by
-- default) placed on a grid over a 2 km square. Every tenth vehicle moves on
-- each frame along a small circle, so transforms are sent every frame. The
-- server counts the pings the clients send and prints a line every 10 seconds.

local SQUARE = 2000.0
local GROUND = 72.0
local CIRCLE_RADIUS = 5.0
local MOVING_EVERY = 10
local REPORT_EVERY = 10000

local count = GetConvarInt('loadtest_vehicles', 500)
local columns = math.max(1, math.ceil(math.sqrt(count)))
local rows = math.max(1, math.ceil(count / columns))
local cell_x = SQUARE / columns
local cell_y = SQUARE / rows

local moving = {}
local pings = 0

for index = 0, count - 1 do
    local column = index % columns
    local row = index // columns
    local x = -SQUARE / 2 + (column + 0.5) * cell_x
    local y = -SQUARE / 2 + (row + 0.5) * cell_y
    local vehicle = CreateVehicle('adder', x, y, GROUND, (index * 37) % 360, true, false)
    if index % MOVING_EVERY == 0 then
        moving[#moving + 1] = { vehicle = vehicle, x = x, y = y }
    end
end

CreateThread(function()
    local step = 0.0
    while true do
        Wait(0)
        step = step + 1.0
        local rad = math.rad(step * 5.0)
        local dx = CIRCLE_RADIUS * math.cos(rad)
        local dy = CIRCLE_RADIUS * math.sin(rad)
        for _, car in ipairs(moving) do
            if DoesEntityExist(car.vehicle) then
                SetEntityCoords(car.vehicle, car.x + dx, car.y + dy, GROUND)
            end
        end
    end
end)

RegisterNetEvent('loadtest:ping', function()
    pings = pings + 1
end)

CreateThread(function()
    local last = GetGameTimer()
    while true do
        Wait(REPORT_EVERY)
        local now = GetGameTimer()
        local seconds = (now - last) / 1000.0
        print(('loadtest: %d pings in %.1f s (%.1f per second), %d of %d vehicles moving'):format(
            pings, seconds, pings / seconds, #moving, count))
        pings = 0
        last = now
    end
end)

print(('loadtest: %d vehicles on a %d x %d grid, %d moving'):format(count, columns, rows, #moving))
