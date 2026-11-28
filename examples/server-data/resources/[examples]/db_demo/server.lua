MySQL.ready(function()
    MySQL.query.await([[
        CREATE TABLE IF NOT EXISTS sixm_demo_starts (
            name TEXT PRIMARY KEY NOT NULL,
            starts INTEGER NOT NULL
        )
    ]])

    local sample = 'example'
    MySQL.update.await([[
        INSERT INTO sixm_demo_starts (name, starts) VALUES (?, 1)
        ON CONFLICT (name) DO UPDATE SET starts = starts + 1
    ]], { sample })

    local starts = MySQL.scalar.await('SELECT starts FROM sixm_demo_starts WHERE name = ?', { sample })
    print(('Database demo started %d time(s)'):format(starts))
end)
