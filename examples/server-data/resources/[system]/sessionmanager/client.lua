-- Clean-room client session handshake for SixM.
-- Waits one frame after resource startup, notifies the server that the client
-- scripts are active, and exposes `isSessionReady()`.

local sessionReady = false

RegisterNetEvent('sixm_session:ready', function(accepted)
    if accepted == nil or accepted == true then
        sessionReady = true
    elseif accepted == false then
        sessionReady = false
    end
end)

CreateThread(function()
    Wait(0)
    TriggerServerEvent('sixm_session:ready')
    sessionReady = true
end)

exports('isSessionReady', function()
    return sessionReady
end)
