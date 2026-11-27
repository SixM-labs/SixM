//! Console commands: the built-in set, `exec`, the access control commands and
//! dispatch to script commands and convars.
//!
//! A line comes from the console, from a connected player or from a script
//! through `ExecuteCommand`. The console may run everything. A player or a
//! resource may run a built-in command, or a restricted script command, only
//! when its access control entries allow `command.<name>`. A player or a
//! resource never reaches a convar through its bare name.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Instant;
use std::{fs, io};

use sixm_core::{ConvarKind, Host, ResourceName, Value, normalize_resource_path};
use tracing::{debug, error, info, warn};

use crate::access::Permission;
use crate::config::tokenize;
use crate::convars::{ENDPOINT, RCON_PASSWORD};
use crate::manager::{ManagerError, ResourceManager};
use crate::perf::{format_millis, per_second};

/// Deepest `exec` nesting. A file that includes itself stops here.
const MAX_EXEC_DEPTH: usize = 16;
/// Maximum commands drained per console line, input, or frame.
const MAX_QUEUED_COMMANDS: usize = 1000;

/// Built-in command names, as listed by `help`.
pub(crate) const BUILT_IN: [&str; 25] = [
    "add_ace",
    "add_principal",
    "endpoint_add_tcp",
    "endpoint_add_udp",
    "entities",
    "ensure",
    "exec",
    "help",
    "list_aces",
    "list_principals",
    "perf",
    "players",
    "profiler",
    "quit",
    "refresh",
    "remove_ace",
    "remove_principal",
    "restart",
    "set",
    "setr",
    "sets",
    "start",
    "state",
    "stop",
    "test_ace",
];

/// Who sent a command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommandSource {
    /// Standard input, `server.cfg`, `exec` and `+command`.
    Console,
    /// A connected player, by server id.
    Player(u32),
    /// A script of this resource, through `ExecuteCommand`. The line runs with
    /// the rights of the principal `resource.<name>`.
    Resource(ResourceName),
}

/// Where a command line comes from, and how many `exec` files it is nested in.
#[derive(Debug, Clone)]
pub(crate) struct CommandContext {
    pub(crate) source: CommandSource,
    /// The number of `exec` files around the line.
    depth: usize,
}

impl CommandContext {
    /// The context of a line that `source` sent.
    pub(crate) fn new(source: CommandSource) -> Self {
        Self { source, depth: 0 }
    }

    /// The context of a line in a file that `exec` runs from this context.
    fn nested(&self) -> Self {
        Self {
            source: self.source.clone(),
            depth: self.depth + 1,
        }
    }
}

/// Tokenizes `line` and runs it.
pub(crate) fn run_line(manager: &ResourceManager, line: &str, context: &CommandContext) {
    match tokenize(line) {
        Ok(words) => run_words(manager, &words, line.trim(), context),
        Err(err) => match &context.source {
            CommandSource::Console => error!("{}: {err}", line.trim()),
            CommandSource::Player(player) => {
                info!("player {player} sent a line that does not tokenize: {err}");
            }
            CommandSource::Resource(resource) => {
                warn!("resource `{resource}` ran a line that does not tokenize: {err}");
            }
        },
    }
}

/// Runs one command given as words, name first. `raw` is the line as the
/// script command handler should see it.
pub(crate) fn run_words(
    manager: &ResourceManager,
    words: &[String],
    raw: &str,
    context: &CommandContext,
) {
    let Some((name, args)) = words.split_first() else {
        return;
    };
    let lower = name.to_ascii_lowercase();
    if BUILT_IN.contains(&lower.as_str()) && !permitted(manager, &context.source, &lower) {
        return;
    }
    match lower.as_str() {
        "set" => set_convar(manager, "set", args, ConvarKind::Plain, &context.source),
        "sets" => set_convar(
            manager,
            "sets",
            args,
            ConvarKind::ServerInfo,
            &context.source,
        ),
        "setr" => set_convar(
            manager,
            "setr",
            args,
            ConvarKind::Replicated,
            &context.source,
        ),
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
        "perf" => perf(manager, args),
        "profiler" => profiler(manager, args),
        "players" => players(manager),
        "entities" => entities(manager),
        "state" => state(manager, args),
        "quit" => quit(manager, args),
        "exec" => exec(manager, args, context),
        "help" => help(manager),
        "add_ace" => add_ace(manager, args),
        "remove_ace" => remove_ace(manager, args),
        "add_principal" => add_principal(manager, args),
        "remove_principal" => remove_principal(manager, args),
        "list_aces" => list_aces(manager, args),
        "list_principals" => list_principals(manager, args),
        "test_ace" => test_ace(manager, args),
        _ => run_script_or_convar(manager, name, &lower, args, raw, &context.source),
    }
}

/// Whether `source` may run `command`, a built-in or restricted command in
/// lower case. The console may run everything. A player is checked as
/// `player.<id>` and a resource as `resource.<name>`, both against
/// `command.<name>`. A refusal is logged with the sender and the name.
fn permitted(manager: &ResourceManager, source: &CommandSource, command: &str) -> bool {
    match source {
        CommandSource::Console => true,
        CommandSource::Player(player) => {
            let allowed = manager.player_may(*player, &format!("command.{command}"));
            if !allowed {
                info!("player {player} may not run `{command}`");
            }
            allowed
        }
        CommandSource::Resource(resource) => {
            let allowed = manager.resource_may(resource, &format!("command.{command}"));
            if !allowed {
                warn!("resource `{resource}` may not run `{command}`");
            }
            allowed
        }
    }
}

/// The first argument of a script command: the player's id, or `0` for the
/// console and for a resource's line.
fn caller_id(source: &CommandSource) -> Value {
    Value::Int(match source {
        CommandSource::Console | CommandSource::Resource(_) => 0,
        CommandSource::Player(player) => i64::from(*player),
    })
}

/// Runs a script command, or prints or sets an existing convar, or reports an
/// unknown name. `lower` is `name` in lower case. A restricted script command
/// is checked for a player or a resource. A player or a resource may not read
/// or set a convar by its name.
fn run_script_or_convar(
    manager: &ResourceManager,
    name: &str,
    lower: &str,
    args: &[String],
    raw: &str,
    source: &CommandSource,
) {
    if let Some(command) = manager.script_command(lower) {
        if command.restricted && !permitted(manager, source, lower) {
            return;
        }
        let call_args = [
            caller_id(source),
            Value::Array(args.iter().map(|arg| Value::from(arg.as_str())).collect()),
            Value::from(raw),
        ];
        if let Err(err) = manager.call_ref(&command.handler, &call_args) {
            error!("command `{name}` failed: {err}");
        }
        return;
    }
    if let Some(value) = manager.convar(name) {
        match source {
            CommandSource::Player(player) => {
                info!("player {player} may not read or set the convar `{name}`");
            }
            CommandSource::Resource(resource) => {
                warn!("resource `{resource}` may not read or set the convar `{name}`");
            }
            CommandSource::Console if args.is_empty() => {
                match manager.convar_kind(name).unwrap_or(ConvarKind::Plain) {
                    ConvarKind::Plain => info!("{name} = \"{value}\""),
                    kind => info!("{name} = \"{value}\" ({})", kind.as_str()),
                }
            }
            CommandSource::Console => {
                store_convar(manager, name, &args.join(" "), ConvarKind::Plain);
            }
        }
        return;
    }
    match source {
        CommandSource::Console => warn!("No such command {name}."),
        CommandSource::Player(player) => debug!("player {player} sent an unknown command `{name}`"),
        CommandSource::Resource(resource) => {
            warn!("resource `{resource}` ran an unknown command `{name}`");
        }
    }
}

/// `set`, `sets` and `setr`: store a convar with `kind`. The value is the rest
/// of the line, joined by single spaces. `command` names the command in the
/// usage line.
fn set_convar(
    manager: &ResourceManager,
    command: &str,
    args: &[String],
    kind: ConvarKind,
    source: &CommandSource,
) {
    let Some((name, value)) = args.split_first() else {
        warn!("usage: {command} <name> <value>");
        return;
    };
    if name.eq_ignore_ascii_case(RCON_PASSWORD) && !matches!(source, CommandSource::Console) {
        warn!("rcon_password is console-only");
        return;
    }
    store_convar(manager, name, &value.join(" "), kind);
}

/// Stores a convar. A refused store is logged, and the convar keeps its value.
fn store_convar(manager: &ResourceManager, name: &str, value: &str, kind: ConvarKind) {
    if let Err(err) = manager.set_convar(name, value, kind) {
        error!("cannot set `{name}`: {err}");
    }
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
/// applies at the next start. `endpoint_add_tcp` is an alias: TCP (HTTP) and
/// UDP (the game) share this address.
fn endpoint(manager: &ResourceManager, args: &[String], alias: bool) {
    if alias {
        info!("endpoint_add_tcp sets the address for both TCP (HTTP) and UDP (game)");
    }
    let Some(address) = args.first() else {
        warn!("usage: endpoint_add_udp <ip:port>");
        return;
    };
    match address.parse::<SocketAddr>() {
        Ok(_) => {
            store_convar(manager, ENDPOINT, address, ConvarKind::Plain);
            info!("Listening address set to {address}; it applies at the next start");
        }
        Err(err) => error!("endpoint_add_udp: `{address}` is not an ip:port address: {err}"),
    }
}

/// `players`: lists the connected players with their server id, name, license and endpoint.
fn players(manager: &ResourceManager) {
    let players = manager.player_list();
    if players.is_empty() {
        info!("No players are connected");
        return;
    }
    info!("{} player(s) connected", players.len());
    for (id, player) in players {
        let license = manager
            .identifiers_of(id)
            .into_iter()
            .find(|identifier| identifier.starts_with("license:"))
            .unwrap_or_default();
        info!("{id:>5}  {}  {license}  {}", player.name, player.endpoint);
    }
}

/// `perf`: prints the frame times of the recent frames against the tick budget,
/// the load, and the network rates since the previous `perf` call, or since the
/// server started.
fn perf(manager: &ResourceManager, args: &[String]) {
    if !args.is_empty() {
        warn!("usage: perf");
        return;
    }
    let tick_rate = manager.tick_rate();
    let budget = 1_000_000 / tick_rate;
    let summary = manager.frames.borrow().summary();
    if let Some(frames) = summary {
        info!(
            "frames: {} recorded, average {} ms, p50 {} ms, p99 {} ms, max {} ms",
            frames.count,
            format_millis(frames.average),
            format_millis(frames.p50),
            format_millis(frames.p99),
            format_millis(frames.max),
        );
    } else {
        info!("frames: none recorded yet");
    }
    info!(
        "tick rate {tick_rate} per second, budget {} ms per frame",
        format_millis(budget)
    );
    info!(
        "{} players, {} entities, {} running resources",
        manager.player_ids().len(),
        manager.entity_list().len(),
        manager.running_resources().len()
    );
    let now = Instant::now();
    let traffic = manager.network_traffic();
    let (since, previous) = manager.perf_mark.replace((now, traffic));
    let millis = now.saturating_duration_since(since).as_millis();
    info!(
        "network over {:.1} s: sent {} KiB/s ({} frames/s), received {} KiB/s ({} frames/s)",
        now.saturating_duration_since(since).as_secs_f64(),
        per_second(
            traffic.bytes_sent.saturating_sub(previous.bytes_sent),
            1024,
            millis
        ),
        per_second(
            traffic.frames_sent.saturating_sub(previous.frames_sent),
            1,
            millis
        ),
        per_second(
            traffic
                .bytes_received
                .saturating_sub(previous.bytes_received),
            1024,
            millis
        ),
        per_second(
            traffic
                .frames_received
                .saturating_sub(previous.frames_received),
            1,
            millis
        ),
    );
}

/// Drains queued script commands, applying deferrals and drops.
pub(crate) fn drain_queued(manager: &ResourceManager) {
    let mut ran = 0;
    loop {
        manager.apply_deferral_calls();
        manager.apply_drops();
        let Some((line, source)) = manager.pop_command() else {
            return;
        };
        if ran == MAX_QUEUED_COMMANDS {
            let dropped = manager.discard_commands() + 1;
            warn!("more than {MAX_QUEUED_COMMANDS} commands queued at once; dropped {dropped}");
            return;
        }
        ran += 1;
        run_line(manager, &line, &CommandContext::new(source));
    }
}

/// `profiler <record [frames] | stop | status | saveJSON <path>>`
fn profiler(manager: &ResourceManager, args: &[String]) {
    let Some((sub, rest)) = args.split_first() else {
        warn!("usage: profiler <record [frames] | stop | status | saveJSON <path>>");
        return;
    };
    match sub.to_ascii_lowercase().as_str() {
        "record" => {
            let max_frames = match rest {
                [] => None,
                [count] => match count.parse::<u32>() {
                    Ok(n) if n > 0 => Some(n),
                    _ => {
                        warn!("usage: profiler record [positive-frame-count]");
                        return;
                    }
                },
                _ => {
                    warn!("usage: profiler record [frames]");
                    return;
                }
            };
            manager.profiler.borrow_mut().start(max_frames);
            manager.profiler_active.set(true);
            if let Some(n) = max_frames {
                info!("profiler recording next {n} frames");
            } else {
                info!("profiler recording until stopped");
            }
        }
        "stop" => {
            if !rest.is_empty() {
                warn!("usage: profiler stop");
                return;
            }
            let frames = manager.profiler.borrow_mut().stop();
            manager.profiler_active.set(false);
            info!("profiler stopped ({frames} frames recorded)");
        }
        "status" => {
            if !rest.is_empty() {
                warn!("usage: profiler status");
                return;
            }
            let status = manager.profiler.borrow().status();
            let state = if status.recording {
                match status.frames_remaining {
                    Some(left) => format!("recording ({left} frames remaining)"),
                    None => "recording".to_owned(),
                }
            } else {
                "stopped".to_owned()
            };
            info!(
                "profiler: {state}, {} frames, {} events{}",
                status.recorded_frames,
                status.event_count,
                if status.truncated { " (truncated)" } else { "" }
            );
        }
        "savejson" => {
            let [path] = rest else {
                warn!("usage: profiler saveJSON <relative-path>");
                return;
            };
            let Some(rel) = normalize_resource_path(path) else {
                error!("profiler saveJSON: invalid relative path `{path}`");
                return;
            };
            let bytes = manager.profiler.borrow().to_chrome_trace_json();
            if let Err(err) = write_relative_file(Path::new(&rel), &bytes) {
                error!("profiler saveJSON `{rel}` failed: {err}");
                return;
            }
            info!("profiler trace written to {rel} ({} bytes)", bytes.len());
        }
        _ => warn!("usage: profiler <record [frames] | stop | status | saveJSON <path>>"),
    }
}

fn write_relative_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, bytes)
}

/// `entities`: lists every entity with its id, kind, model in hex, position and
/// heading, in id order.
fn entities(manager: &ResourceManager) {
    let entities = manager.entity_list();
    info!("{} entities", entities.len());
    for (id, entity, _, _) in entities {
        let [x, y, z] = entity.position;
        info!(
            "{id:>6}  {:?}  model 0x{:08x}  position {x:.2} {y:.2} {z:.2}  heading {:.2}",
            entity.kind, entity.model, entity.rotation[2]
        );
    }
}

/// `state [bag]`: lists the state bags, or prints the keys of one bag with
/// their values and whether they are replicated.
fn state(manager: &ResourceManager, args: &[String]) {
    let Some(bag) = args.first() else {
        info!("State bags: {}", manager.state_bag_names().join(", "));
        return;
    };
    match manager.state_bag_entries(bag) {
        Some(entries) if entries.is_empty() => info!("State bag {bag} has no keys"),
        Some(entries) => {
            for (key, value, replicated) in entries {
                let flag = if replicated { " (replicated)" } else { "" };
                info!("{bag}.{key} = {value:?}{flag}");
            }
        }
        None => warn!("No such state bag {bag}."),
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

/// `exec <file>`: runs each line of a file, as the source that runs `exec`. The
/// path is relative to the working directory.
fn exec(manager: &ResourceManager, args: &[String], context: &CommandContext) {
    let Some(file) = args.first() else {
        warn!("usage: exec <file>");
        return;
    };
    if context.depth >= MAX_EXEC_DEPTH {
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
            Ok(words) => run_words(manager, &words, line.trim(), &context.nested()),
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

/// The `N` words of `args`, which must all be non-empty. Logs `usage` and
/// returns `None` otherwise.
fn names<'a, const N: usize>(args: &'a [String], usage: &str) -> Option<[&'a str; N]> {
    let words = <[&str; N]>::try_from(args.iter().map(String::as_str).collect::<Vec<_>>()).ok();
    match words {
        Some(words) if words.iter().all(|word| !word.is_empty()) => Some(words),
        _ => {
            warn!("usage: {usage}");
            None
        }
    }
}

/// Reads the `allow` or `deny` word of an access command. Logs `usage` when it is neither.
fn permission_arg(word: &str, usage: &str) -> Option<Permission> {
    let permission = Permission::parse(word);
    if permission.is_none() {
        warn!("usage: {usage}");
    }
    permission
}

/// `add_ace <principal> <object> <allow|deny>`: sets the entry of a principal
/// for an object. An earlier entry for the same pair is replaced.
fn add_ace(manager: &ResourceManager, args: &[String]) {
    const USAGE: &str = "add_ace <principal> <object> <allow|deny>";
    let Some([principal, object, word]) = names::<3>(args, USAGE) else {
        return;
    };
    let Some(permission) = permission_arg(word, USAGE) else {
        return;
    };
    manager
        .access
        .borrow_mut()
        .add_ace(principal, object, permission);
    info!("ace {principal} {object} {}", permission.as_str());
}

/// `remove_ace <principal> <object> <allow|deny>`: removes an entry, when it
/// has that permission.
fn remove_ace(manager: &ResourceManager, args: &[String]) {
    const USAGE: &str = "remove_ace <principal> <object> <allow|deny>";
    let Some([principal, object, word]) = names::<3>(args, USAGE) else {
        return;
    };
    let Some(permission) = permission_arg(word, USAGE) else {
        return;
    };
    let removed = manager
        .access
        .borrow_mut()
        .remove_ace(principal, object, permission);
    if removed {
        info!("removed ace {principal} {object} {}", permission.as_str());
    } else {
        warn!("no ace {principal} {object} {}", permission.as_str());
    }
}

/// `add_principal <child> <parent>`: makes `child` inherit from `parent`.
fn add_principal(manager: &ResourceManager, args: &[String]) {
    let Some([child, parent]) = names::<2>(args, "add_principal <child> <parent>") else {
        return;
    };
    manager.access.borrow_mut().add_principal(child, parent);
    info!("principal {child} inherits from {parent}");
}

/// `remove_principal <child> <parent>`: removes an inheritance link.
fn remove_principal(manager: &ResourceManager, args: &[String]) {
    let Some([child, parent]) = names::<2>(args, "remove_principal <child> <parent>") else {
        return;
    };
    let removed = manager.access.borrow_mut().remove_principal(child, parent);
    if removed {
        info!("principal {child} no longer inherits from {parent}");
    } else {
        warn!("no link from {child} to {parent}");
    }
}

/// `list_aces`: lists every entry as `ace <principal> <object> <allow|deny>`.
fn list_aces(manager: &ResourceManager, args: &[String]) {
    if !args.is_empty() {
        warn!("usage: list_aces");
        return;
    }
    let access = manager.access.borrow();
    let mut count = 0;
    for (principal, object, permission) in access.aces() {
        info!("ace {principal} {object} {}", permission.as_str());
        count += 1;
    }
    if count == 0 {
        info!("No access entries");
    }
}

/// `list_principals`: lists every inheritance link as `principal <child> <parent>`.
fn list_principals(manager: &ResourceManager, args: &[String]) {
    if !args.is_empty() {
        warn!("usage: list_principals");
        return;
    }
    let access = manager.access.borrow();
    let mut count = 0;
    for (child, parent) in access.links() {
        info!("principal {child} inherits from {parent}");
        count += 1;
    }
    if count == 0 {
        info!("No principal links");
    }
}

/// `test_ace <principal> <object>`: tells whether the principal may use the
/// object, with the same rules that the server applies to players.
fn test_ace(manager: &ResourceManager, args: &[String]) {
    let Some([principal, object]) = names::<2>(args, "test_ace <principal> <object>") else {
        return;
    };
    let verdict = if manager.ace_allowed(principal, object) {
        "allowed"
    } else {
        "denied"
    };
    info!("{principal} is {verdict} {object}");
}
