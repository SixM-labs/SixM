local resource = GetCurrentResourceName()
local localBag = ('player:%d'):format(GetPlayerServerId(PlayerId()))

AddEventHandler('onClientResourceStart', function(name)
    if name == resource then
        print(('%s is running on the client'):format(name))
        TriggerServerEvent('hello:ping', 'from the client')
    end
end)

RegisterNetEvent('hello:pong', function(text)
    print(('server says: %s'):format(text))
end)

AddStateBagChangeHandler('playerCount', 'global', function(_, _, value)
    print(('GlobalState.playerCount is now %s'):format(tostring(value)))
end)

AddStateBagChangeHandler('joinedAt', localBag, function(_, _, value)
    print(('LocalPlayer.state.joinedAt is now %s'):format(tostring(value)))
end)
