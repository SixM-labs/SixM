//! Command-line entry point of the SixM server.

use std::env;
use std::io::{self, IsTerminal};
use std::path::Path;
use std::process::ExitCode;
use std::sync::mpsc;

use sixm_server::{Input, Invocation, Server, parse_args, spawn_stdin_reader};
use tracing::{error, warn};
use tracing_subscriber::EnvFilter;

/// Text printed by `--help`.
const USAGE: &str = "\
usage: sixm-server [+command [args...]]...

Each +command runs a console command at startup, in order. With no commands,
server.cfg in the working directory is executed if it exists.

Options:
  --help       print this help and exit
  --version    print the version and exit";

fn main() -> ExitCode {
    let mut commands = match parse_args(env::args().skip(1)) {
        Ok(Invocation::Help) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Invocation::Version) => {
            println!("sixm-server {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Ok(Invocation::Run(commands)) => commands,
        Err(err) => {
            eprintln!("sixm-server: {err}\ntry `sixm-server --help`");
            return ExitCode::from(2);
        }
    };

    init_logging();

    if commands.is_empty() {
        if Path::new("server.cfg").is_file() {
            commands.push(vec!["exec".to_owned(), "server.cfg".to_owned()]);
        } else {
            warn!("no server.cfg in the working directory and no +commands given; nothing to run");
        }
    }

    let (sender, input) = mpsc::channel();
    if let Err(err) = spawn_stdin_reader(sender.clone()) {
        error!("cannot read the console: {err}");
    }
    let interrupt = sender;
    if let Err(err) = ctrlc::set_handler(move || {
        let _ = interrupt.send(Input::Quit("interrupted".to_owned()));
    }) {
        error!("cannot install the Ctrl+C handler: {err}");
        return ExitCode::FAILURE;
    }

    let server = Server::new();
    for command in &commands {
        server.execute_words(command);
    }
    server.run(&input);
    ExitCode::SUCCESS
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
