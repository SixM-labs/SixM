-- Clean-room base gameplay event channels, with the FiveM names.
--
-- SixM never emits these itself yet: the headless core has no GameAdapter
-- that emits vehicle seating or death events. A client script (or a future
-- adapter) sends these with TriggerServerEvent. The channels are registered
-- so the server accepts them and AddEventHandler listeners receive them.
-- `source` is the player. Argument lists, as documented for FiveM (tables are
-- plain Lua tables):
--
--   * `baseevents:onPlayerDied(killerType, deathCoords)`
--   * `baseevents:onPlayerKilled(killerId, data)`
--       data = { killertype, weaponhash, killerinvehicle, killervehseat,
--                killervehname, killerpos }
--   * `baseevents:onPlayerWasted(deathCoords)`
--   * `baseevents:enteringVehicle(vehicle, seat, displayName, netId)`
--   * `baseevents:enteringAborted()`
--   * `baseevents:enteredVehicle(vehicle, seat, displayName, netId)`
--   * `baseevents:leftVehicle(vehicle, seat, displayName, netId)`

local EVENT_CHANNELS = {
    'baseevents:onPlayerDied',
    'baseevents:onPlayerKilled',
    'baseevents:onPlayerWasted',
    'baseevents:enteringVehicle',
    'baseevents:enteringAborted',
    'baseevents:enteredVehicle',
    'baseevents:leftVehicle',
}

for _, eventName in ipairs(EVENT_CHANNELS) do
    RegisterNetEvent(eventName)
end
