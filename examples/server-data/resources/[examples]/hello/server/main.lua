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
