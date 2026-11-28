//! Command-line entry point of the SixM client: connects to a server, syncs its
//! resources and runs their client scripts until Ctrl+C, `quit` or a disconnect.

use std::io::{self, BufRead, IsTerminal};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use sixm_client::{Client, ClientRuntime, Input, SimAdapter};
use sixm_net::{Fingerprint, Identity};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

/// Usage line printed on bad arguments.
const USAGE: &str = "usage: sixm-client <addr> <name> [--fingerprint HEX] [--cache DIR] [--identity FILE] [--sim] [+command args...]";
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
    /// Attach the built-in simulated [`GameAdapter`].
    sim: bool,
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1).peekable();
    if args.len() == 1 && args.peek().is_some_and(|arg| arg == "--version") {
        println!("sixm-client {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    init_logging();
    let options = match parse_args(args) {
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

    let (card_tx, card_rx) = mpsc::sync_channel::<(String, mpsc::SyncSender<Option<String>>)>(1);
    let addr = options.addr;
    let name = options.name.clone();
    let fingerprint = options.fingerprint;
    let identity = options.identity;
    let cache = options.cache.clone();
    let connect_handle = thread::spawn(move || {
        Client::connect_with_card_handler(
            addr,
            &name,
            fingerprint,
            &identity,
            &cache,
            CONNECT_TIMEOUT,
            |_| {},
            move |card_json| {
                let (reply_tx, reply_rx) = mpsc::sync_channel(1);
                if card_tx.send((card_json.to_owned(), reply_tx)).is_err() {
                    return None;
                }
                reply_rx.recv().unwrap_or(None)
            },
        )
    });

    while !connect_handle.is_finished() {
        match card_rx.recv_timeout(Duration::from_millis(25)) {
            Ok((card_json, reply_tx)) => {
                let answer = prompt_card(&card_json, &input);
                let _ = reply_tx.send(answer);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let client = match connect_handle.join() {
        Ok(Ok(client)) => client,
        Ok(Err(err)) => {
            error!("cannot connect to {}: {err}", options.addr);
            return ExitCode::FAILURE;
        }
        Err(_) => {
            error!("client connect thread panicked");
            return ExitCode::FAILURE;
        }
    };
    info!("Client fingerprint: {}", client.fingerprint());
    log_sync(&client);

    let runtime = ClientRuntime::new(client, options.cache.join(DATA_FOLDER));
    if options.sim {
        runtime.set_adapter(Box::new(SimAdapter::new()));
    }
    if let Err(err) = runtime.try_start_synced() {
        error!("cannot attach game adapter: {err}");
        return ExitCode::FAILURE;
    }
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
    let mut sim = false;
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
            "--sim" => sim = true,
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
        sim,
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

/// Extracts a concise human-readable summary from an Adaptive Card JSON value.
fn card_summary(value: &serde_json::Value) -> Option<String> {
    let obj = value.as_object()?;
    let mut parts = Vec::new();
    if let Some(title) = obj.get("title").and_then(serde_json::Value::as_str)
        && !title.is_empty()
    {
        parts.push(title.to_owned());
    }
    if let Some(body) = obj.get("body").and_then(serde_json::Value::as_array) {
        for item in body {
            if item.get("type").and_then(serde_json::Value::as_str) == Some("TextBlock")
                && let Some(text) = item.get("text").and_then(serde_json::Value::as_str)
                && !text.is_empty()
            {
                parts.push(text.to_owned());
            }
        }
    }
    (!parts.is_empty()).then(|| parts.join(" | "))
}

/// Logs an Adaptive Card presented during deferral and waits for the user to
/// enter a JSON response, press Enter for `{}`, or type `cancel`.
fn prompt_card(card_json: &str, input: &Receiver<Input>) -> Option<String> {
    if let Some(summary) = serde_json::from_str::<serde_json::Value>(card_json)
        .ok()
        .as_ref()
        .and_then(card_summary)
    {
        info!("Adaptive Card ({summary}): {card_json}");
    } else {
        info!("Adaptive Card: {card_json}");
    }
    info!("Enter JSON response, 'cancel', or press Enter for {{}}:");

    let deadline = std::time::Instant::now() + CONNECT_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            warn!("Adaptive Card prompt timed out");
            return None;
        }
        match input.recv_timeout(remaining) {
            Ok(Input::Line(line)) => {
                let trimmed = line.trim();
                if trimmed.eq_ignore_ascii_case("cancel") {
                    return None;
                }
                if trimmed.is_empty() {
                    return Some("{}".to_owned());
                }
                if trimmed.len() > sixm_net::MAX_CARD_RESPONSE_BYTES {
                    warn!(
                        "Card response exceeds {} bytes; enter shorter JSON or 'cancel':",
                        sixm_net::MAX_CARD_RESPONSE_BYTES
                    );
                    continue;
                }
                match serde_json::from_str::<serde_json::Value>(trimmed) {
                    Ok(_) => return Some(trimmed.to_owned()),
                    Err(err) => {
                        warn!(
                            "Invalid JSON ({err}); enter valid JSON, 'cancel', or press Enter for {{}}:"
                        );
                    }
                }
            }
            Ok(Input::Quit(_))
            | Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {
                return None;
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_card_accepts_json_empty_cancel_and_retries_invalid() {
        let (tx, rx) = mpsc::channel();
        tx.send(Input::Line("not-json".to_owned())).unwrap();
        tx.send(Input::Line(r#"{"accepted":true}"#.to_owned()))
            .unwrap();
        assert_eq!(
            prompt_card(
                r#"{"title":"Rules","body":[{"type":"TextBlock","text":"Accept?"}]}"#,
                &rx
            )
            .as_deref(),
            Some(r#"{"accepted":true}"#)
        );

        tx.send(Input::Line("   ".to_owned())).unwrap();
        assert_eq!(prompt_card("{}", &rx).as_deref(), Some("{}"));

        tx.send(Input::Line("CANCEL".to_owned())).unwrap();
        assert_eq!(prompt_card("{}", &rx), None);

        tx.send(Input::Quit("bye".to_owned())).unwrap();
        assert_eq!(prompt_card("{}", &rx), None);
    }
}
