//! Command-line client: connects to a server, syncs its resource files and
//! logs what the server does until Ctrl+C or a disconnect.

use std::io::{self, IsTerminal};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sixm_client::{Client, ClientNotice};
use sixm_net::Fingerprint;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

/// Usage line printed on bad arguments.
const USAGE: &str = "usage: sixm-client <addr> <name> [--fingerprint HEX] [--cache DIR]";
/// Cache folder used when `--cache` is not given.
const DEFAULT_CACHE: &str = "sixm-cache";
/// Longest the handshake and the resource list may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the loop checks for Ctrl+C when the server is quiet.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Parsed command line.
#[derive(Debug)]
struct Options {
    addr: SocketAddr,
    name: String,
    fingerprint: Option<Fingerprint>,
    cache: PathBuf,
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

    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    if let Err(err) = ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst)) {
        error!("cannot install the Ctrl+C handler: {err}");
        return ExitCode::FAILURE;
    }

    let mut client = match Client::connect(
        options.addr,
        &options.name,
        options.fingerprint,
        &options.cache,
        CONNECT_TIMEOUT,
    ) {
        Ok(client) => client,
        Err(err) => {
            error!("cannot connect to {}: {err}", options.addr);
            return ExitCode::FAILURE;
        }
    };
    log_sync(&client);

    while !stop.load(Ordering::SeqCst) {
        let Some(notice) = client.poll(POLL_INTERVAL) else {
            continue;
        };
        log_notice(&notice);
        if matches!(
            notice,
            ClientNotice::Kicked { .. } | ClientNotice::Disconnected { .. }
        ) {
            break;
        }
    }
    ExitCode::SUCCESS
}

/// Parses `<addr> <name>` with optional `--fingerprint HEX` and `--cache DIR`,
/// in any order.
fn parse_args(args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut args = args;
    let mut positional = Vec::new();
    let mut fingerprint = None;
    let mut cache = PathBuf::from(DEFAULT_CACHE);
    while let Some(arg) = args.next() {
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
            _ => positional.push(arg),
        }
    }
    let [addr, name] = <[String; 2]>::try_from(positional)
        .map_err(|_| "expected a server address and a player name".to_owned())?;
    let addr = addr
        .parse()
        .map_err(|err| format!("invalid address `{addr}`: {err}"))?;
    Ok(Options {
        addr,
        name,
        fingerprint,
        cache,
    })
}

/// Logs every synced resource and file, then the download totals.
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

fn log_notice(notice: &ClientNotice) {
    match notice {
        ClientNotice::ResourceStarted {
            name,
            downloaded,
            cached,
        } => info!("resource {name} started: {downloaded} downloaded, {cached} cached"),
        ClientNotice::ResourceFailed { name, reason } => {
            error!("resource {name} could not be synced: {reason}");
        }
        ClientNotice::ResourceStopped { name } => info!("resource {name} stopped"),
        ClientNotice::Event { name, args } => info!("event {name} ({} arguments)", args.len()),
        ClientNotice::StateBag { bag, key, .. } => info!("state {bag}.{key} changed"),
        ClientNotice::StateBagRemoved { bag } => info!("state bag {bag} removed"),
        ClientNotice::Kicked { reason } => info!("kicked: {reason}"),
        ClientNotice::Disconnected { reason } => info!("disconnected: {reason}"),
    }
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
