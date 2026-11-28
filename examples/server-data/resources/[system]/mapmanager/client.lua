local known = {}
local started = {}

local function metadata(resource, key, index)
    return GetResourceMetadata(resource, key, index)
end

local function isTypedMap(resource)
    return metadata(resource, 'resource_type', 0) == 'map'
end

local function isTypedGameType(resource)
    return metadata(resource, 'resource_type', 0) == 'gametype'
end

local function isSafeRelativePath(path)
    if type(path) ~= 'string' then
        return false
    end
    path = path:gsub('\\', '/')
    if path == '' or path:sub(1, 1) == '/' or path:find(':', 1, true) ~= nil then
        return false
    end
    for component in path:gmatch('[^/]+') do
        if component == '..' or component == '.' then
            return false
        end
    end
    return true
end

local function isDenseArray(value)
    if type(value) ~= 'table' then
        return false
    end
    local count = 0
    local maxIndex = 0
    for key in pairs(value) do
        if type(key) ~= 'number' or key < 1 or key % 1 ~= 0 then
            return false
        end
        count = count + 1
        maxIndex = math.max(maxIndex, key)
    end
    return count == maxIndex
end

local function readDirectiveFile(resource, path)
    if not isSafeRelativePath(path) then
        print(('mapmanager: refusing unsafe directive path `%s` in `%s`'):format(tostring(path), resource))
        return nil
    end
    local text = LoadResourceFile(resource, path)
    if type(text) ~= 'string' then
        print(('mapmanager: cannot read `%s` from map `%s`'):format(path, resource))
        return nil
    end
    local ok, document = pcall(json.decode, text)
    if not ok or type(document) ~= 'table' or not isDenseArray(document.directives) then
        print(('mapmanager: invalid directive JSON in `%s/%s`'):format(resource, path))
        return nil
    end
    for index, directive in ipairs(document.directives) do
        if type(directive) ~= 'table'
            or type(directive.name) ~= 'string'
            or directive.name == ''
        then
            print(('mapmanager: invalid directive %d in `%s/%s`'):format(index, resource, path))
            return nil
        end
    end
    return document.directives
end

local function applyMap(resource)
    local handlers = {}
    local registering = true
    local function add(name, handler)
        if not registering then
            error('getMapDirectives add may only be called during getMapDirectives', 2)
        end
        local kind = type(handler)
        local callable = kind == 'function'
        if kind == 'userdata' then
            callable = tostring(handler):match('^function: [^#]+#%d+$') ~= nil
        end
        if type(name) ~= 'string' or name == '' or not callable then
            error('getMapDirectives add expects a nonempty name and a Lua function or SixM function reference', 2)
        end
        local list = handlers[name]
        if not list then
            list = {}
            handlers[name] = list
        end
        list[#list + 1] = handler
    end

    TriggerEvent('getMapDirectives', add)
    registering = false
    for index = 0, GetNumResourceMetadata(resource, 'map') - 1 do
        local path = GetResourceMetadata(resource, 'map', index)
        local directives = readDirectiveFile(resource, path)
        if directives then
            for _, directive in ipairs(directives) do
                local list = handlers[directive.name]
                if not list then
                    print(('mapmanager: map `%s` has unknown directive `%s`'):format(resource, directive.name))
                else
                    for _, handler in ipairs(list) do
                        local ok, err = pcall(handler, directive.data, resource)
                        if not ok then
                            print(('mapmanager: directive `%s` failed in map `%s`, file `%s`: %s')
                                :format(directive.name, resource, path, tostring(err)))
                        end
                    end
                end
            end
        end
    end
    for k in pairs(handlers) do
        handlers[k] = nil
    end
end

local function startedResource(resource)
    if started[resource] then
        return
    end
    started[resource] = true
    if isTypedMap(resource) then
        known[resource] = 'map'
        TriggerEvent('onClientMapStart', resource)
        applyMap(resource)
    elseif isTypedGameType(resource) then
        known[resource] = 'gametype'
        TriggerEvent('onClientGameTypeStart', resource)
    end
end

local function stoppedResource(resource)
    local kind = known[resource]
    if not kind then
        started[resource] = nil
        return
    end
    if kind == 'map' then
        TriggerEvent('onClientMapStop', resource)
    else
        TriggerEvent('onClientGameTypeStop', resource)
    end
    known[resource] = nil
    started[resource] = nil
end

AddEventHandler('onClientResourceStart', function(resource)
    startedResource(resource)
end)

AddEventHandler('onClientResourceStop', function(resource)
    stoppedResource(resource)
end)

local names = {}
for index = 0, GetNumResources() - 1 do
    local resource = GetResourceByFindIndex(index)
    if resource and GetResourceState(resource) == 'started' then
        names[#names + 1] = resource
    end
end
table.sort(names)
for _, resource in ipairs(names) do
    startedResource(resource)
end
