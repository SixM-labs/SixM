local function clean_message(text)
    local message = text:gsub('%c', ' '):match('^%s*(.-)%s*$')
    if #message == 0 or #message > 256 then
        return nil
    end
    return message
end

local function send_message(player, text)
    local message = clean_message(text)
    if not message then
        return
    end

    local author
    if player == 0 then
        author = 'Console'
    else
        author = GetPlayerName(player)
        if not author then
            return
        end
        author = author:gsub('%c', ' ')
    end

    TriggerEvent('chatMessage', player, author, message)
    if WasEventCanceled() then
        return
    end

    print(('%s: %s'):format(author, message))
    TriggerClientEvent('chat:addMessage', -1, { args = { author, message } })
end

RegisterNetEvent('_chat:messageEntered', function(text)
    local player = source
    if type(text) ~= 'string' then
        return
    end
    if not IsPlayerAceAllowed(player, 'command.say') then
        return
    end

    -- WasEventCanceled observes the active network dispatch while its handler
    -- is running. Defer the nested local event until that dispatch has ended.
    SetTimeout(0, function()
        send_message(player, text)
    end)
end)

RegisterNetEvent('chat:init', function()
    local player = source
    local suggestions = {}
    for _, command in ipairs(GetRegisteredCommands()) do
        suggestions[#suggestions + 1] = {
            name = '/' .. command.name,
            help = '',
            params = {},
        }
    end
    TriggerClientEvent('chat:addSuggestions', player, suggestions)
end)

RegisterCommand('say', function(player, args)
    local text = table.concat(args, ' ')
    SetTimeout(0, function()
        send_message(player, text)
    end)
end, true)
