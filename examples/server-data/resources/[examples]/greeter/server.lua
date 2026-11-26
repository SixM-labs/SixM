function greet(name)
    return ('Hello, %s!'):format(name)
end

exports('farewell', function(name)
    return ('Goodbye, %s!'):format(name)
end)
