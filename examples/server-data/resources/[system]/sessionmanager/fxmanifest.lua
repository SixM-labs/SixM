fx_version 'cerulean'
game 'common'

description 'Clean-room player session lifecycle manager for SixM'

server_script 'server.lua'
client_script 'client.lua'

server_export 'isPlayerActive'
server_export 'getPlayerSession'
export 'isSessionReady'
