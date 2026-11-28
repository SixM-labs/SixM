local resource = GetCurrentResourceName()
local active = {}
local waiting = {}
local pollTimer

-- The state object distinguishes a caller from a later restart of the same resource.
local function ownerState()
    local owner = GetInvokingResource() or resource
    local state = active[owner]
    if not state then
        assert(GetResourceState(owner) ~= 'stopping', 'The database caller is stopping')
        state = { owner = owner, statements = {} }
        active[owner] = state
    end
    return state
end

local function callable(value)
    -- A callback from another runtime reaches Lua as callable userdata.
    return type(value) == 'function' or type(value) == 'userdata'
end

local function notify(callback, ...)
    local ok, reason = pcall(callback, ...)
    if not ok then
        print('Database callback failed: ' .. tostring(reason))
    end
end

local function defer(state, callback, ...)
    local args = table.pack(...)
    SetTimeout(0, function()
        if active[state.owner] == state then
            notify(callback, table.unpack(args, 1, args.n))
        end
    end)
end

-- Submit eagerly and keep the outcome so multiple awaits observe one execution.
local function thenable(state, submit)
    local done, failure, result
    local subscribers = {}
    local function settle(err, value)
        if done then return end
        done, failure, result = true, err, value
        local pending = subscribers
        subscribers = nil
        for _, subscriber in ipairs(pending) do
            if err ~= nil then
                notify(subscriber.reject, err)
            else
                notify(subscriber.resolve, value)
            end
        end
    end

    local ok, reason = pcall(submit, settle)
    if not ok then
        settle(tostring(reason))
    end

    return {
        ['then'] = function(resolve, reject)
            assert(callable(resolve) and callable(reject), 'then expects resolve and reject callbacks')
            if done then
                -- A JS promise cannot be resolved by re-entering its active runtime.
                if failure ~= nil then
                    defer(state, reject, failure)
                else
                    defer(state, resolve, result)
                end
            else
                subscribers[#subscribers + 1] = { resolve = resolve, reject = reject }
            end
        end,
    }
end

local function statement(state, query)
    if type(query) == 'number' then
        return assert(state.statements[query], 'Unknown stored SQL statement')
    end
    return query
end

local function makeExports(submit, isTransaction)
    local function async(query, params)
        local state = ownerState()
        return thenable(state, function(settle)
            submit(statement(state, query), params, function(err, result)
                if isTransaction and err ~= nil then
                    settle(nil, false)
                else
                    settle(err, result)
                end
            end)
        end)
    end

    return function(query, params, callback)
        if callback == nil and callable(params) then
            callback, params = params, nil
        end
        if callback == nil then
            return async(query, params)
        end
        assert(callable(callback), 'Expected a database callback')
        local state = ownerState()
        local function complete(err, result)
            if isTransaction and err ~= nil then
                result = false
            end
            notify(callback, result, err)
        end
        local ok, reason = pcall(function()
            submit(statement(state, query), params, complete)
        end)
        if not ok then
            defer(state, complete, tostring(reason))
        end
    end, async
end

local methods = {}
local awaited = {}
local function register(name, submit)
    methods[name], awaited[name] = makeExports(submit, name == 'transaction')
    exports(name, methods[name])
    exports(name .. '_async', awaited[name])
end

for _, kind in ipairs({ 'query', 'single', 'scalar', 'insert', 'update', 'rawExecute' }) do
    register(kind, function(query, params, callback)
        SixmSqlExecute(kind, query, params, callback)
    end)
end
register('prepare', SixmSqlPrepare)
register('transaction', SixmSqlTransaction)

local function poll()
    pollTimer = nil
    if not SixmSqlIsReady() then
        pollTimer = SetTimeout(100, poll)
        return
    end
    local pending = waiting
    waiting = {}
    for state, callbacks in pairs(pending) do
        for _, callback in ipairs(callbacks) do
            defer(state, callback)
        end
    end
end

local function whenReady(state, callback)
    if SixmSqlIsReady() then
        defer(state, callback)
        return
    end
    local callbacks = waiting[state]
    if not callbacks then
        callbacks = {}
        waiting[state] = callbacks
    end
    callbacks[#callbacks + 1] = callback
    if not pollTimer then
        pollTimer = SetTimeout(100, poll)
    end
end

local function readyAsync()
    local state = ownerState()
    return thenable(state, function(settle)
        whenReady(state, function()
            settle(nil, true)
        end)
    end)
end

local function ready(callback)
    if callback == nil then
        return readyAsync()
    end
    assert(callable(callback), 'Expected a readiness callback')
    whenReady(ownerState(), callback)
end

exports('isReady', SixmSqlIsReady)
exports('isReady_async', function()
    return thenable(ownerState(), function(settle)
        settle(nil, SixmSqlIsReady())
    end)
end)
exports('ready', ready)
exports('ready_async', readyAsync)

local function store(query, callback)
    assert(type(query) == 'string' and query ~= '', 'Expected a SQL statement to store')
    assert(callback == nil or callable(callback), 'Expected a database callback')
    local state = ownerState()
    local id = #state.statements + 1
    state.statements[id] = query
    if callback ~= nil then
        defer(state, callback, id)
    end
    return id
end

-- The manifest routes both legacy resource names to the same export table.
exports('mysql_fetch_all', methods.query)
exports('mysql_fetch_scalar', methods.scalar)
exports('mysql_execute', methods.update)
exports('mysql_insert', methods.insert)
exports('mysql_transaction', methods.transaction)
exports('mysql_store', store)
exports('store', store)
exports('execute', methods.query)
exports('executeSync', awaited.query)
exports('scalarSync', awaited.scalar)
exports('transactionSync', awaited.transaction)

AddEventHandler('onResourceStop', function(name)
    -- Only a lifecycle event from the host can invalidate a resource's work.
    if GetInvokingResource() ~= nil then return end
    local state = active[name]
    if not state then return end
    active[name] = nil
    waiting[state] = nil
    if pollTimer and next(waiting) == nil then
        ClearTimeout(pollTimer)
        pollTimer = nil
    end
end)
