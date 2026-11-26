//! Console commands: the built-in set, `exec`, and dispatch to script commands
//! and convars.

use std::fs;
use std::net::SocketAddr;

use sixm_core::{Host, Value};
use tracing::{error, info, warn};

use crate::config::tokenize;
use crate::convars::ENDPOINT;
use crate::manager::{ManagerError, ResourceManager};

/// Deepest `exec` nesting. A file that includes itself stops here.
const MAX_EXEC_DEPTH: usize = 16;

/// Built-in command names, as listed by `help`.
const BUILT_IN: [&str; 14] = [
    "endpoint_add_tcp",
    "endpoint_add_udp",
    "ensure",
    "exec",
    "help",
    "players",
    "quit",
    "refresh",
    "restart",
    "set",
    "setr",
    "sets",
    "start",
    "stop",
];

/// Tokenizes `line` and runs it. `depth` is the number of `exec` files around it.
pub(crate) fn run_line(manager: &ResourceManager, line: &str, depth: usize) {
    match tokenize(line) {
        Ok(words) => run_words(manager, &words, line.trim(), depth),
        Err(err) => error!("{}: {err}", line.trim()),
    }
}

/// Runs one command given as words, name first. `raw` is the line as the
/// script command handler should see it.
pub(crate) fn run_words(manager: &ResourceManager, words: &[String], raw: &str, depth: usize) {
    let Some((name, args)) = words.split_first() else {
        return;
    };
    match name.to_ascii_lowercase().as_str() {
        "set" | "sets" | "setr" => set_convar(manager, args),
        "start" => resource_command(args, "start <resource>", |resource| manager.start(resource)),
        "stop" => resource_command(args, "stop <resource>", |resource| manager.stop(resource)),
        "restart" => resource_command(args, "restart <resource>", |resource| {
            manager.restart(resource)
        }),
        "ensure" => resource_command(args, "ensure <resource>", |resource| {
            manager.ensure(resource)
        }),
        "refresh" => manager.refresh(),
        "endpoint_add_udp" => endpoint(manager, args, false),
        "endpoint_add_tcp" => endpoint(manager, args, true),
        "players" => players(manager),
        "quit" => quit(manager, args),
        "exec" => exec(manager, args, depth),
        "help" => help(manager),
        _ => run_script_or_convar(manager, name, args, raw),
    }
}

/// Runs a script command, or prints or sets an existing convar, or reports an
/// unknown name.
fn run_script_or_convar(manager: &ResourceManager, name: &str, args: &[String], raw: &str) {
    if let Some(command) = manager.script_command(&name.to_ascii_lowercase()) {
        let call_args = [
            Value::Int(0),
            Value::Array(args.iter().map(|arg| Value::from(arg.as_str())).collect()),
            Value::from(raw),
        ];
        if let Err(err) = manager.call_ref(&command.handler, &call_args) {
            error!("command `{name}` failed: {err}");
        }
        return;
    }
    if let Some(value) = manager.convar(name) {
        if args.is_empty() {
            info!("{name} = \"{value}\"");
        } else {
            manager.set_convar(name, &args.join(" "));
        }
        return;
    }
    warn!("No such command {name}.");
}

/// `set`, `sets` and `setr`: store a convar. The value is the rest of the line,
/// joined by single spaces.
fn set_convar(manager: &ResourceManager, args: &[String]) {
    let Some((name, value)) = args.split_first() else {
        warn!("usage: set <name> <value>");
        return;
    };
    manager.set_convar(name, &value.join(" "));
}

/// Runs a resource command with the resource name as its only argument.
fn resource_command(
    args: &[String],
    usage: &str,
    action: impl FnOnce(&str) -> Result<(), ManagerError>,
) {
    if let Some(resource) = args.first() {
        report(action(resource));
    } else {
        warn!("usage: {usage}");
    }
}

/// Logs a failed resource operation. Refusals that are expected, such as
/// stopping a resource that is not running, are warnings.
fn report(result: Result<(), ManagerError>) {
    if let Err(err) = result {
        match &err {
            ManagerError::Canceled(_) | ManagerError::NotRunning(_) => warn!("{err}"),
            _ => error!("{err}"),
        }
    }
}

/// `endpoint_add_udp <ip:port>`: sets the address the server listens on. It
/// applies at the next start. `endpoint_add_tcp` is an alias, since SixM only
/// speaks UDP.
fn endpoint(manager: &ResourceManager, args: &[String], alias: bool) {
    if alias {
        info!("SixM uses UDP only; endpoint_add_tcp is treated as endpoint_add_udp");
    }
    let Some(address) = args.first() else {
        warn!("usage: endpoint_add_udp <ip:port>");
        return;
    };
    match address.parse::<SocketAddr>() {
        Ok(_) => {
            manager.set_convar(ENDPOINT, address);
            info!("Listening address set to {address}; it applies at the next start");
        }
        Err(err) => error!("endpoint_add_udp: `{address}` is not an ip:port address: {err}"),
    }
}

/// `players`: lists the connected players with their server id, name and endpoint.
fn players(manager: &ResourceManager) {
    let players = manager.player_list();
    if players.is_empty() {
        info!("No players are connected");
        return;
    }
    info!("{} player(s) connected", players.len());
    for (id, player) in players {
        info!("{id:>5}  {}  {}", player.name, player.endpoint);
    }
}

/// `quit [reason]`: asks the server to stop.
fn quit(manager: &ResourceManager, args: &[String]) {
    let reason = if args.is_empty() {
        "requested from the console".to_owned()
    } else {
        args.join(" ")
    };
    info!("Shutting down: {reason}");
    manager.request_quit();
}

/// `exec <file>`: runs each line of a file. The path is relative to the working directory.
fn exec(manager: &ResourceManager, args: &[String], depth: usize) {
    let Some(file) = args.first() else {
        warn!("usage: exec <file>");
        return;
    };
    if depth >= MAX_EXEC_DEPTH {
        error!("exec {file}: files are nested more than {MAX_EXEC_DEPTH} levels deep");
        return;
    }
    let bytes = match fs::read(file) {
        Ok(bytes) => bytes,
        Err(err) => {
            error!("exec {file}: {err}");
            return;
        }
    };
    let text = String::from_utf8_lossy(&bytes);
    for (index, line) in text.lines().enumerate() {
        if manager.quit_requested() {
            break;
        }
        match tokenize(line) {
            Ok(words) => run_words(manager, &words, line.trim(), depth + 1),
            Err(err) => warn!("{file}:{}: {err}", index + 1),
        }
    }
}

/// `help`: lists the built-in and script commands.
fn help(manager: &ResourceManager) {
    info!("Built-in commands: {}", BUILT_IN.join(", "));
    let scripts = manager.script_commands();
    if scripts.is_empty() {
        info!("No script commands are registered");
        return;
    }
    let listing: Vec<String> = scripts
        .into_iter()
        .map(|(name, restricted)| {
            if restricted {
                format!("{name} (restricted)")
            } else {
                name
            }
        })
        .collect();
    info!("Script commands: {}", listing.join(", "));
}
