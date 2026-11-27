//! Command-line entry point of the SixM client: connects to a server, syncs its
//! resources and runs their client scripts until Ctrl+C, `quit` or a disconnect.

use std::io::{self, BufRead, IsTerminal};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use sixm_client::{Client, ClientRuntime, Input};
use sixm_net::{Fingerprint, Identity};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

/// Usage line printed on bad arguments.
const USAGE: &str = "usage: sixm-client <addr> <name> [--fingerprint HEX] [--cache DIR] [--identity FILE] [+command args...]";
/// Cache folder used when `--cache` is not given.
const DEFAULT_CACHE: &str = "sixm-cache";
/// File inside the cache folder that holds the client certificate, unless
/// `--identity` names another one.
const DEFAULT_IDENTITY: &str = "identity.bin";
/// Folder inside the cache that holds each resource's data folder.
const DATA_FOLDER: &str = "data";
/// Longest the handshake and the resource list may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Parsed command line.
#[derive(Debug)]
struct Options {
    addr: SocketAddr,
    name: String,
    fingerprint: Option<Fingerprint>,
    cache: PathBuf,
    /// The client certificate. The server derives the player's identifiers
    /// from it, so the file keeps the player's `license` stable.
    identity: Identity,
    /// Console commands given as `+name args...`, each one its words, name first.
    commands: Vec<Vec<String>>,
}

fn main() -> ExitCode {
    init_logging();
    let options = match parse_args(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("sixm-client: {message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    let (sender, input) = mpsc::channel();
    if let Err(err) = spawn_stdin_reader(sender.clone()) {
        error!("cannot read the console: {err}");
    }
    let interrupt = sender.clone();
    if let Err(err) = ctrlc::set_handler(move || {
        let _ = interrupt.send(Input::Quit("interrupted".to_owned()));
    }) {
        error!("cannot install the Ctrl+C handler: {err}");
        return ExitCode::FAILURE;
    }

    let client = match Client::connect(
        options.addr,
        &options.name,
        options.fingerprint,
        &options.identity,
        &options.cache,
        CONNECT_TIMEOUT,
    ) {
        Ok(client) => client,
        Err(err) => {
            error!("cannot connect to {}: {err}", options.addr);
            return ExitCode::FAILURE;
        }
    };
    info!("Client fingerprint: {}", client.fingerprint());
    log_sync(&client);

    let runtime = ClientRuntime::new(client, options.cache.join(DATA_FOLDER));
    runtime.start_synced();
    for command in &options.commands {
        runtime.execute_words(command);
    }
    runtime.run(&input);
    ExitCode::SUCCESS
}

/// Parses `<addr> <name>` with optional `--fingerprint HEX`, `--cache DIR` and
/// `--identity FILE`, in any order. Arguments after the first `+command` belong
/// to that command.
fn parse_args(args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut args = args;
    let mut positional = Vec::new();
    let mut fingerprint = None;
    let mut cache = PathBuf::from(DEFAULT_CACHE);
    let mut identity = None;
    let mut commands: Vec<Vec<String>> = Vec::new();
    while let Some(arg) = args.next() {
        if let Some(name) = arg.strip_prefix('+') {
            if name.is_empty() {
                return Err("a bare `+` is not a command".to_owned());
            }
            commands.push(vec![name.to_owned()]);
            continue;
        }
        match arg.as_str() {
            "--fingerprint" => {
                let value = args.next().ok_or("--fingerprint needs a value")?;
                let parsed = value
                    .parse::<Fingerprint>()
                    .map_err(|err| format!("--fingerprint: {err}"))?;
                fingerprint = Some(parsed);
            }
            "--cache" => {
                cache = PathBuf::from(args.next().ok_or("--cache needs a folder")?);
            }
            "--identity" => {
                let file = args.next().ok_or("--identity needs a file")?;
                identity = Some(PathBuf::from(file));
            }
            _ => match commands.last_mut() {
                Some(command) => command.push(arg),
                None => positional.push(arg),
            },
        }
    }
    let [addr, name] = <[String; 2]>::try_from(positional)
        .map_err(|_| "expected a server address and a player name".to_owned())?;
    let addr = addr
        .parse()
        .map_err(|err| format!("invalid address `{addr}`: {err}"))?;
    let identity = identity.unwrap_or_else(|| cache.join(DEFAULT_IDENTITY));
    Ok(Options {
        addr,
        name,
        fingerprint,
        cache,
        identity: Identity::Persistent(identity),
        commands,
    })
}

/// Logs the connection, then every synced resource and file, then the download totals.
fn log_sync(client: &Client) {
    info!(
        "connected to {} as player {}",
        client.server_name(),
        client.player()
    );
    for resource in client.resources() {
        info!("resource {}: {} files", resource.name, resource.files.len());
        for file in &resource.files {
            info!("  {:?} {} -> {}", file.kind, file.name, file.path.display());
        }
    }
    let stats = client.stats();
    info!(
        "synced {} resources: {} files downloaded, {} already cached",
        client.resources().len(),
        stats.downloaded,
        stats.cached
    );
}

/// Starts a thread that sends each line of standard input to `sink`. The
/// thread ends at end of input, and the client keeps running without console input.
fn spawn_stdin_reader(sink: Sender<Input>) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("console".to_owned())
        .spawn(move || {
            for line in io::stdin().lock().lines() {
                let line = match line {
                    Ok(line) => line,
                    Err(err) => {
                        warn!("console input closed: {err}");
                        break;
                    }
                };
                if sink.send(Input::Line(line)).is_err() {
                    break;
                }
            }
        })
}

/// Logs to standard output with `RUST_LOG` as the filter, defaulting to `info`.
fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(io::stdout().is_terminal())
        .compact()
        .init();
}
