fx_version 'cerulean'
game 'common'

server_only 'yes'
dependency 'oxmysql'

server_scripts {
    '@oxmysql/lib/MySQL.lua',
    'server.lua',
}
