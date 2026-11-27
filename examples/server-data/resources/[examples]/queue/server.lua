-- Holds every connection for a moment, the way a whitelist or a queue does.

-- Licenses refused at the door, from the convar `queue_banned` (comma separated).
local function banned(license)
    for entry in GetConvar('queue_banned', ''):gmatch('[^,%s]+') do
        if entry == license then
            return true
        end
    end
    return false
end

-- Resolves after `ms` milliseconds, so a thread can `Citizen.Await` it.
local function sleep(ms)
    local p = promise.new()
    SetTimeout(ms, function()
        p:resolve(ms)
    end)
    return p
end

AddEventHandler('playerConnecting', function(name, _, deferrals)
    local player = source
    deferrals.defer()

    CreateThread(function()
        deferrals.update(('Hello %s, checking your license...'):format(name))
        Citizen.Await(sleep(500))

        local license = GetPlayerIdentifierByType(player, 'license')
        if not license then
            deferrals.done('Your client did not prove a license.')
            return
        end
        if banned(license) then
            deferrals.done('You are banned from this server.')
            return
        end

        deferrals.update('License checked, joining...')
        Citizen.Await(sleep(250))
        print(('%s passed the queue with %s'):format(name, license))
        deferrals.done()
    end)
end)
