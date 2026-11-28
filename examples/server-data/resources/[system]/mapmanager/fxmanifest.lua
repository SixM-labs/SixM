fx_version 'cerulean'
game 'common'

description 'Clean-room map and gametype resource manager for SixM'

server_script 'server.lua'
client_script 'client.lua'

server_exports {
    'getCurrentGameType',
    'getCurrentMap',
    'changeGameType',
    'changeMap',
    'doesMapSupportGameType',
    'getMaps',
    'roundEnded',
}
