local resource = GetCurrentResourceName()

print(exports.greeter:greet('SixM'))

AddEventHandler('onResourceStart', function(name)
    if name == resource then
        print(('%s started'):format(name))
    end
end)

AddEventHandler('onResourceStop', function(name)
    if name == resource then
        print(('goodbye from %s'):format(name))
    end
end)

CreateThread(function()
    while true do
        Wait(5000)
        print(('uptime: %d ms'):format(GetGameTimer()))
    end
end)

RegisterCommand('hello', function(source, args)
    print(('hello command from %s with %d args'):format(source, #args))
end)

AddEventHandler('playerConnecting', function(name, setKickReason)
    if name:lower():find('banned', 1, true) then
        setKickReason('That name is not allowed on this server')
        CancelEvent()
    end
end)

AddEventHandler('playerJoining', function()
    Player(source).state.joinedAt = GetGameTimer()
    GlobalState.playerCount = #GetPlayers()
    print(('%s (%d) joined, %d connected'):format(GetPlayerName(source), source, #GetPlayers()))
end)

AddEventHandler('playerDropped', function(reason)
    -- The player who left is still listed while this handler runs.
    GlobalState.playerCount = #GetPlayers() - 1
    print(('%s (%d) left: %s'):format(GetPlayerName(source), source, reason))
end)

AddStateBagChangeHandler('playerCount', nil, function(bagName, key, value)
    print(('%s.%s is now %s'):format(bagName, key, value))
end)

RegisterNetEvent('hello:ping', function(text)
    TriggerClientEvent('hello:pong', source, ('pong: %s'):format(text))
end)
