-- Vehicles owned by the server. Every client keeps a copy of them.

local parked = {}

-- `spawncar` is restricted: a player needs `command.spawncar` (see server.cfg).
RegisterCommand('spawncar', function(source, args)
    local model = args[1] or 'adder'
    local x = 10.0 * (#parked + 1)
    local vehicle = CreateVehicle(model, x, 0.0, 72.0, 90.0, true, false)
    Entity(vehicle).state.plate = ('SIXM%03d'):format(vehicle)
    Entity(vehicle).state.owner = source
    parked[#parked + 1] = vehicle
    print(('spawned %s as entity %d at %s'):format(model, vehicle, GetEntityCoords(vehicle)))
end, true)

-- Drives every parked vehicle in a circle. Moves are sent once per frame.
CreateThread(function()
    local angle = 0.0
    while true do
        Wait(0)
        angle = angle + 1.0
        for index, vehicle in ipairs(parked) do
            if DoesEntityExist(vehicle) then
                local radius = 10.0 * index
                local rad = math.rad(angle)
                SetEntityCoords(vehicle, radius * math.cos(rad), radius * math.sin(rad), 72.0)
                SetEntityHeading(vehicle, (angle + 90.0) % 360.0)
            end
        end
    end
end)

RegisterCommand('clearcars', function()
    for _, vehicle in ipairs(parked) do
        DeleteEntity(vehicle)
    end
    parked = {}
    print('all vehicles removed')
end, true)

AddEventHandler('entityCreated', function(entity)
    print(('entityCreated %d (type %d)'):format(entity, GetEntityType(entity)))
end)

AddEventHandler('entityRemoved', function(entity)
    print(('entityRemoved %d'):format(entity))
end)
