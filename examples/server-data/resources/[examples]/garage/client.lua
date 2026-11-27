-- `cars` lists the vehicles this client knows, with their state.
RegisterCommand('cars', function()
    local vehicles = GetGamePool('CVehicle')
    print(('%d vehicles'):format(#vehicles))
    for _, vehicle in ipairs(vehicles) do
        local coords = GetEntityCoords(vehicle)
        print(('  %d %s at %s heading %.1f, %.1f m from the centre'):format(
            vehicle,
            tostring(Entity(vehicle).state.plate),
            coords,
            GetEntityHeading(vehicle),
            #(coords - vector3(0.0, 0.0, 72.0))))
    end
end, false)

print(('garage %s loaded, game mode %s'):format(
    GetResourceMetadata(GetCurrentResourceName(), 'version', 0),
    GetConvar('game_mode', 'unknown')))
