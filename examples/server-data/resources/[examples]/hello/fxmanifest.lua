fx_version 'cerulean'
game 'common'

dependency 'greeter'

server_scripts {
    'server/*.lua',
}

client_scripts {
    'client/*.lua',
}

files {
    'data/greeting.txt',
}
