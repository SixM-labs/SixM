local db = exports.oxmysql
local mysql = { Async = {}, Sync = {} }

local function withAwait(callback, await)
    return setmetatable({ await = await }, {
        __call = function(_, ...)
            return callback(...)
        end,
    })
end

for _, name in ipairs({ 'query', 'single', 'scalar', 'insert', 'update', 'prepare', 'rawExecute', 'transaction' }) do
    local asyncName = name .. '_async'
    local function callback(...)
        return db[name](...)
    end
    local function await(query, params)
        return Await(db[asyncName](query, params))
    end
    mysql[name] = withAwait(callback, await)
    mysql.Async[name] = callback
    mysql.Sync[name] = await
end

for alias, name in pairs({ fetchAll = 'query', fetchScalar = 'scalar', execute = 'update' }) do
    mysql.Async[alias] = mysql.Async[name]
    mysql.Sync[alias] = mysql.Sync[name]
end

function mysql.Async.store(query, callback)
    db:mysql_store(query, callback)
end

function mysql.Sync.store(query)
    return db:mysql_store(query)
end

function mysql.isReady()
    return db:isReady()
end

local function awaitReady()
    return Await(db:ready_async())
end

mysql.ready = withAwait(function(callback)
    assert(type(callback) == 'function', 'Expected a readiness callback')
    CreateThread(function()
        awaitReady()
        callback()
    end)
end, awaitReady)

MySQL = mysql
