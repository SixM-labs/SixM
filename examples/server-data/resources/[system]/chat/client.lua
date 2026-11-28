local templates = {}
local suggestions = {}

local function warn_invalid(event)
    print(('[chat] ignored malformed %s payload'):format(event))
end

local function dense_array(value)
    if type(value) ~= 'table' then
        return false
    end

    local count = 0
    local largest = 0
    for key in pairs(value) do
        if type(key) ~= 'number' or math.type(key) ~= 'integer' or key < 1 then
            return false
        end
        count = count + 1
        largest = math.max(largest, key)
    end
    return count == largest
end

local function scalar_text(value)
    return tostring(value)
end

local function is_scalar(value)
    local kind = type(value)
    return kind == 'string' or kind == 'number' or kind == 'boolean'
end

local function valid_color(color)
    if not dense_array(color) or #color ~= 3 then
        return false
    end
    for _, channel in ipairs(color) do
        if type(channel) ~= 'number' then
            return false
        end
    end
    return true
end

local function message_text(message)
    local arguments = {}
    for _, value in ipairs(message.args) do
        arguments[#arguments + 1] = scalar_text(value)
    end

    local template = message.template
    if type(template) ~= 'string' and type(message.templateId) == 'string' then
        template = templates[message.templateId]
    end
    if type(template) == 'string' then
        local plain = template:gsub('<[^>]*>', '')
        return plain:gsub('{(%d+)}', function(index)
            return arguments[tonumber(index) + 1] or ''
        end)
    end
    if #arguments == 1 then
        return arguments[1]
    end
    if #arguments > 1 then
        return arguments[1] .. ': ' .. table.concat(arguments, ' ', 2)
    end
    return ''
end

RegisterNetEvent('chat:addMessage', function(message)
    if type(message) ~= 'table' or not dense_array(message.args) then
        warn_invalid('chat:addMessage')
        return
    end
    if (message.color ~= nil and not valid_color(message.color))
        or (message.multiline ~= nil and type(message.multiline) ~= 'boolean')
        or (message.template ~= nil and type(message.template) ~= 'string')
        or (message.templateId ~= nil and type(message.templateId) ~= 'string')
    then
        warn_invalid('chat:addMessage')
        return
    end
    for _, value in ipairs(message.args) do
        if not is_scalar(value) then
            warn_invalid('chat:addMessage')
            return
        end
    end

    local line = message_text(message):gsub('%c', ' ')
    print(line)
end)

RegisterNetEvent('chat:addTemplate', function(template_id, html)
    if type(template_id) ~= 'string' or type(html) ~= 'string' then
        warn_invalid('chat:addTemplate')
        return
    end
    templates[template_id] = html
end)

local function valid_suggestion(suggestion)
    if type(suggestion) ~= 'table'
        or type(suggestion.name) ~= 'string'
        or not suggestion.name:match('^/')
        or (suggestion.help ~= nil and type(suggestion.help) ~= 'string')
    then
        return false
    end
    if suggestion.params ~= nil and not dense_array(suggestion.params) then
        return false
    end
    for _, parameter in ipairs(suggestion.params or {}) do
        if type(parameter) ~= 'table'
            or type(parameter.name) ~= 'string'
            or (parameter.help ~= nil and type(parameter.help) ~= 'string')
        then
            return false
        end
    end
    return true
end

local function store_suggestion(suggestion)
    if not valid_suggestion(suggestion) then
        warn_invalid('chat:addSuggestion')
        return
    end
    suggestions[suggestion.name] = {
        name = suggestion.name,
        help = suggestion.help or '',
        params = suggestion.params or {},
    }
end

RegisterNetEvent('chat:addSuggestion', function(name, help, params)
    store_suggestion({ name = name, help = help, params = params })
end)

RegisterNetEvent('chat:addSuggestions', function(items)
    if not dense_array(items) then
        warn_invalid('chat:addSuggestions')
        return
    end
    for _, suggestion in ipairs(items) do
        store_suggestion(suggestion)
    end
end)

RegisterNetEvent('chat:removeSuggestion', function(name)
    if type(name) ~= 'string' or not name:match('^/') then
        warn_invalid('chat:removeSuggestion')
        return
    end
    suggestions[name] = nil
end)

RegisterNetEvent('chat:clear', function(...)
    if select('#', ...) ~= 0 then
        warn_invalid('chat:clear')
        return
    end
    print('[chat] clear requested; previous console output is retained')
end)

RegisterCommand('say', function(_, args)
    if #args == 0 then
        print('Usage: say <message of 1 to 256 bytes>')
        return
    end
    TriggerServerEvent('_chat:messageEntered', table.concat(args, ' '))
end, false)

RegisterCommand('chat_suggestions', function()
    local names = {}
    for name in pairs(suggestions) do
        names[#names + 1] = name
    end
    table.sort(names)

    local lines = {}
    for _, name in ipairs(names) do
        local suggestion = suggestions[name]
        lines[#lines + 1] = suggestion.help == ''
            and name
            or (name .. ' - ' .. suggestion.help)
    end
    print('[chat] suggestions: ' .. table.concat(lines, ', '))
end, false)

AddEventHandler('onClientResourceStart', function(name)
    if name == GetCurrentResourceName() then
        TriggerServerEvent('chat:init')
    end
end)
