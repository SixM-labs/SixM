RegisterCommand('say', function(player, args)
    local message = table.concat(args, ' '):gsub('%c', ' '):match('^%s*(.-)%s*$')
    if #message == 0 or #message > 256 then
        print('Usage: say <message of 1 to 256 bytes>')
        return
    end

    local name = player == 0 and 'Console' or GetPlayerName(player)
    if name then
        name = name:gsub('%c', ' ')
        TriggerClientEvent('sixm_chat:message', -1, name, message)
        print(('%s: %s'):format(name, message))
    end
end, true)
