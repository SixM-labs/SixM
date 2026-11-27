-- Clean-room base gameplay event channels for SixM.
--
-- SixM's headless core currently has no GameAdapter emitting vehicle seating
-- or combat death events. These channels are registered here so scripts can
-- attach `AddEventHandler` listeners and future GameAdapter integrations can
-- dispatch them without dropped-event warnings:
--
--   * `baseevents:onPlayerEnteredVehicle(vehicle, seat, displayName, netId)`
--   * `baseevents:onPlayerLeftVehicle(vehicle, seat, displayName, netId)`
--   * `baseevents:onPlayerDied(killerType, deathCoords)`

local EVENT_CHANNELS = {
    'baseevents:onPlayerEnteredVehicle',
    'baseevents:onPlayerLeftVehicle',
    'baseevents:onPlayerDied',
}

for _, eventName in ipairs(EVENT_CHANNELS) do
    RegisterNetEvent(eventName)
end
