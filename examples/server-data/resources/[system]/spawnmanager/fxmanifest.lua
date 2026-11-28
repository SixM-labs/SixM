fx_version 'cerulean'
game 'common'

description 'Game-independent client spawn manager for SixM'

client_scripts {
    'client.lua',
    'json.js',
}

export 'spawnPlayer'
export 'addSpawnPoint'
export 'removeSpawnPoint'
export 'loadSpawns'
export 'setAutoSpawn'
export 'setAutoSpawnCallback'
export 'forceRespawn'
