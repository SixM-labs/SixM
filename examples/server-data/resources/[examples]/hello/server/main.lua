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
    print(('%s (%d) joined, %d connected'):format(GetPlayerName(source), source, #GetPlayers()))
end)

AddEventHandler('playerDropped', function(reason)
    print(('%s (%d) left: %s'):format(GetPlayerName(source), source, reason))
end)

RegisterNetEvent('hello:ping', function(text)
    TriggerClientEvent('hello:pong', source, ('pong: %s'):format(text))
end)
