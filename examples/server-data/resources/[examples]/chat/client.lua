RegisterNetEvent('sixm_chat:message', function(name, message)
    print(('%s: %s'):format(name, message))
end)
