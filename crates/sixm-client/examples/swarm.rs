//! Connects a swarm of bots to a server. Every bot is a full client: it syncs
//! the resources into its own cache, runs their client scripts, and quits when
//! the run ends. The summary lists what each bot did.
//!
//! Usage: `cargo run --release -p sixm-client --example swarm -- <addr> <count> [--seconds N] [--cache DIR]`
//!
//! Bots are named `bot1` to `botN`, start 50 ms apart and use an ephemeral
//! certificate, so each one is a new player. The run lasts `--seconds` (60 by
//! default) after the last bot starts. Without `--cache`, the bots keep their
//! caches in a temporary folder that is removed at the end. Logs are shown at
//! warn level unless `RUST_LOG` says otherwise.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use sixm_client::{Client, ClientRuntime, Input};
use sixm_net::Identity;
use tracing::warn;
use tracing_subscriber::EnvFilter;

/// Usage line printed on bad arguments.
const USAGE: &str = "usage: swarm <addr> <count> [--seconds N] [--cache DIR]";
/// Run length when `--seconds` is not given.
const DEFAULT_SECONDS: u64 = 60;
/// Time between the starts of two bots.
const STAGGER: Duration = Duration::from_millis(50);
/// Longest a bot waits for the handshake and the resource list.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Folder inside a bot's cache that holds the data folders, as the client uses it.
const DATA_FOLDER: &str = "data";

/// Parsed command line.
#[derive(Debug)]
struct Options {
    addr: SocketAddr,
    count: usize,
    seconds: u64,
    cache: Option<PathBuf>,
}

/// What one bot did during the run.
#[derive(Debug)]
struct Report {
    name: String,
    /// Time from the start of the connection to the resource list, when it connected.
    connect: Option<Duration>,
    /// The failure that stopped the bot before or during the run.
    error: Option<String>,
    resources: usize,
    downloaded: usize,
    cached: usize,
    /// Whether the server ended the connection before the run did.
    ended_early: bool,
}

impl Report {
    fn failed(name: String, error: String) -> Self {
        Self {
            name,
            connect: None,
            error: Some(error),
            resources: 0,
            downloaded: 0,
            cached: 0,
            ended_early: false,
        }
    }
}

fn main() -> ExitCode {
    init_logging();
    let options = match parse_args(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("swarm: {message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let (root, _temporary) = match options.cache {
        Some(cache) => (cache, None),
        None => match tempfile::tempdir() {
            Ok(dir) => (dir.path().to_path_buf(), Some(dir)),
            Err(err) => {
                eprintln!("swarm: cannot create a temporary cache folder: {err}");
                return ExitCode::FAILURE;
            }
        },
    };

    let run_over = Arc::new(AtomicBool::new(false));
    let mut bots = Vec::with_capacity(options.count);
    for index in 1..=options.count {
        let name = format!("bot{index}");
        let cache = root.join(&name);
        let (sender, input) = mpsc::channel();
        let thread_run_over = Arc::clone(&run_over);
        let thread_name = name.clone();
        let addr = options.addr;
        match thread::Builder::new()
            .name(name.clone())
            .spawn(move || run_bot(thread_name, addr, &cache, &input, &thread_run_over))
        {
            Ok(handle) => bots.push((name, sender, handle)),
            Err(err) => {
                eprintln!("swarm: cannot start {name}: {err}");
                break;
            }
        }
        thread::sleep(STAGGER);
    }
    println!(
        "swarm: {} bots started against {}, running for {} s",
        bots.len(),
        options.addr,
        options.seconds
    );

    thread::sleep(Duration::from_secs(options.seconds));
    run_over.store(true, Ordering::SeqCst);
    let mut reports = Vec::with_capacity(bots.len());
    for (name, sender, handle) in bots {
        let _ = sender.send(Input::Quit("swarm finished".to_owned()));
        reports.push(match handle.join() {
            Ok(report) => report,
            Err(_) => Report::failed(name, "the bot thread panicked".to_owned()),
        });
    }
    print_summary(&reports);
    if reports.len() == options.count
        && reports
            .iter()
            .all(|report| report.error.is_none() && !report.ended_early)
    {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Runs one bot until it quits or the server ends its connection.
fn run_bot(
    name: String,
    addr: SocketAddr,
    cache: &Path,
    input: &Receiver<Input>,
    run_over: &AtomicBool,
) -> Report {
    let started = Instant::now();
    let client = match Client::connect(
        addr,
        &name,
        None,
        &Identity::Ephemeral,
        cache,
        CONNECT_TIMEOUT,
    ) {
        Ok(client) => client,
        Err(err) => {
            warn!("{name}: cannot connect: {err}");
            return Report::failed(name, err.to_string());
        }
    };
    let connect = started.elapsed();
    let stats = client.stats();
    let resources = client.resources().len();
    let runtime = ClientRuntime::new(client, cache.join(DATA_FOLDER));
    runtime.start_synced();
    runtime.run(input);
    Report {
        name,
        connect: Some(connect),
        error: None,
        resources,
        downloaded: stats.downloaded,
        cached: stats.cached,
        ended_early: !run_over.load(Ordering::SeqCst),
    }
}

/// Prints one line per bot, then the totals.
fn print_summary(reports: &[Report]) {
    println!();
    println!(
        "{:<8} {:>12} {:>10} {:>11} {:>8}  status",
        "bot", "connect", "resources", "downloaded", "cached"
    );
    for report in reports {
        let connect = report
            .connect
            .map_or_else(|| "-".to_owned(), |time| format!("{} ms", time.as_millis()));
        let status = match (&report.error, report.ended_early) {
            (Some(error), _) => format!("failed: {error}"),
            (None, true) => "disconnected before the end".to_owned(),
            (None, false) => "ok".to_owned(),
        };
        println!(
            "{:<8} {:>12} {:>10} {:>11} {:>8}  {status}",
            report.name, connect, report.resources, report.downloaded, report.cached
        );
    }

    let connected: Vec<Duration> = reports.iter().filter_map(|report| report.connect).collect();
    let failed = reports
        .iter()
        .filter(|report| report.error.is_some())
        .count();
    let ended_early = reports.iter().filter(|report| report.ended_early).count();
    println!();
    println!(
        "totals: {} bots, {} connected, {} failed, {} disconnected before the end",
        reports.len(),
        connected.len(),
        failed,
        ended_early
    );
    if let (Some(min), Some(max)) = (connected.iter().min(), connected.iter().max()) {
        let total: Duration = connected.iter().sum();
        let average = total / u32::try_from(connected.len()).unwrap_or(u32::MAX);
        println!(
            "connect time: min {} ms, average {} ms, max {} ms",
            min.as_millis(),
            average.as_millis(),
            max.as_millis()
        );
    }
    let resources: usize = reports.iter().map(|report| report.resources).sum();
    let downloaded: usize = reports.iter().map(|report| report.downloaded).sum();
    let cached: usize = reports.iter().map(|report| report.cached).sum();
    println!(
        "resources synced: {resources} in total, {downloaded} files downloaded, {cached} files from the cache"
    );
}

/// Parses `<addr> <count>` with optional `--seconds N` and `--cache DIR`, in any order.
fn parse_args(args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut args = args;
    let mut positional = Vec::new();
    let mut seconds = DEFAULT_SECONDS;
    let mut cache = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--seconds" => {
                let value = args.next().ok_or("--seconds needs a number")?;
                seconds = value
                    .parse::<u64>()
                    .map_err(|err| format!("--seconds: {err}"))?;
            }
            "--cache" => {
                cache = Some(PathBuf::from(args.next().ok_or("--cache needs a folder")?));
            }
            _ => positional.push(arg),
        }
    }
    let [addr, count] = <[String; 2]>::try_from(positional)
        .map_err(|_| "expected a server address and a bot count".to_owned())?;
    let addr: SocketAddr = addr
        .parse()
        .map_err(|err| format!("invalid address `{addr}`: {err}"))?;
    let count = count
        .parse::<usize>()
        .map_err(|err| format!("invalid bot count `{count}`: {err}"))?;
    if count == 0 {
        return Err("the bot count must be at least 1".to_owned());
    }
    Ok(Options {
        addr,
        count,
        seconds,
        cache,
    })
}

/// Logs to standard output with `RUST_LOG` as the filter, defaulting to `warn`.
fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}

#[cfg(test)]
mod tests {
    use super::parse_args;

    fn args<'a>(list: &'a [&'a str]) -> impl Iterator<Item = String> + 'a {
        list.iter().map(|arg| (*arg).to_owned())
    }

    #[test]
    fn flags_may_come_before_or_after_the_positional_arguments() {
        let options = parse_args(args(&[
            "--seconds",
            "30",
            "127.0.0.1:30120",
            "--cache",
            "x",
            "50",
        ]))
        .expect("valid arguments");
        assert_eq!(options.count, 50);
        assert_eq!(options.seconds, 30);
        assert_eq!(options.cache.unwrap().to_str(), Some("x"));
    }

    #[test]
    fn bad_arguments_are_rejected() {
        assert!(parse_args(args(&["127.0.0.1:30120"])).is_err());
        assert!(parse_args(args(&["nonsense", "5"])).is_err());
        assert!(parse_args(args(&["127.0.0.1:30120", "0"])).is_err());
        assert!(parse_args(args(&["127.0.0.1:30120", "5", "--seconds"])).is_err());
    }
}
