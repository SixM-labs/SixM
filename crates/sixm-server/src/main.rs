//! Command-line entry point of the SixM server.

use std::env;
use std::fmt::{self, Write as _};
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal};
use std::path::Path;
use std::process::ExitCode;
use std::sync::{Mutex, mpsc};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value as Json};
use sixm_server::{CliInvocation, CliOptions, Input, Server, parse_cli, spawn_stdin_reader};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber, error, warn};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{self as log_fmt, FmtContext, FormatEvent, FormatFields, MakeWriter};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// Text printed by `--help`.
const USAGE: &str = "\
usage: sixm-server [options] [+command [args...]]...

Each +command runs a console command at startup, in order. With no commands,
server.cfg in the working directory is executed if it exists.

Options:
  --log-file <path>  also write logs to <path>
  --log-json         emit logs as JSON lines
  --watch            watch running resources for file changes
  --help             print this help and exit
  --version          print the version and exit";

fn main() -> ExitCode {
    let (options, mut commands) = match parse_cli(env::args().skip(1)) {
        Ok(CliInvocation::Help) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(CliInvocation::Version) => {
            println!("sixm-server {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Ok(CliInvocation::Run { options, commands }) => (options, commands),
        Err(err) => {
            eprintln!("sixm-server: {err}\ntry `sixm-server --help`");
            return ExitCode::from(2);
        }
    };

    init_logging(&options);

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
    let interrupt = sender.clone();
    if let Err(err) = ctrlc::set_handler(move || {
        let _ = interrupt.send(Input::Quit("interrupted".to_owned()));
    }) {
        error!("cannot install the Ctrl+C handler: {err}");
        return ExitCode::FAILURE;
    }

    let server = Server::new();
    if options.watch {
        server.set_watch(true);
    }
    for command in &commands {
        server.execute_words(command);
    }
    server.run(&sender, &input);
    ExitCode::SUCCESS
}

/// Logs to standard output (and optionally a log file) with `RUST_LOG` as the
/// filter, defaulting to `info`.
fn init_logging(options: &CliOptions) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let stdout_layer = build_layer(options.log_json, io::stdout().is_terminal(), io::stdout);

    let file_layer = options.log_file.as_ref().and_then(|path| {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && let Err(err) = fs::create_dir_all(parent)
        {
            eprintln!(
                "sixm-server: cannot create log directory `{}`: {err}",
                parent.display()
            );
            return None;
        }
        match OpenOptions::new().create(true).append(true).open(path) {
            Ok(file) => Some(build_layer(options.log_json, false, Mutex::new(file))),
            Err(err) => {
                eprintln!(
                    "sixm-server: cannot open log file `{}`: {err}",
                    path.display()
                );
                None
            }
        }
    });

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(stdout_layer)
        .with(file_layer)
        .try_init();
}

fn build_layer<S, W>(json: bool, ansi: bool, writer: W) -> Box<dyn Layer<S> + Send + Sync>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    if json {
        log_fmt::layer()
            .event_format(JsonEventFormatter)
            .with_writer(writer)
            .boxed()
    } else {
        log_fmt::layer()
            .with_target(false)
            .with_ansi(ansi)
            .compact()
            .with_writer(writer)
            .boxed()
    }
}

/// Formats tracing events as single-line JSON objects.
struct JsonEventFormatter;

impl<S, N> FormatEvent<S, N> for JsonEventFormatter
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut fields = Map::new();
        event.record(&mut JsonVisitor(&mut fields));

        let mut root = Map::new();
        root.insert("timestamp".to_owned(), Json::String(utc_timestamp()));
        root.insert(
            "level".to_owned(),
            Json::String(event.metadata().level().as_str().to_owned()),
        );
        root.insert(
            "target".to_owned(),
            Json::String(event.metadata().target().to_owned()),
        );
        if let Some(message) = fields.remove("message") {
            root.insert("message".to_owned(), message);
        }
        if !fields.is_empty() {
            root.insert("fields".to_owned(), Json::Object(fields));
        }

        let line = serde_json::to_string(&Json::Object(root)).map_err(|_| fmt::Error)?;
        writeln!(writer, "{line}")
    }
}

struct JsonVisitor<'a>(&'a mut Map<String, Json>);

impl Visit for JsonVisitor<'_> {
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0
            .insert(field.name().to_owned(), Json::Number(value.into()));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0
            .insert(field.name().to_owned(), Json::Number(value.into()));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().to_owned(), Json::Bool(value));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0
            .insert(field.name().to_owned(), Json::String(value.to_owned()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0
            .insert(field.name().to_owned(), Json::String(format!("{value:?}")));
    }
}

fn utc_timestamp() -> String {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = since_epoch.as_secs();
    let millis = since_epoch.subsec_millis();
    let days = i64::try_from(total_secs / 86_400).unwrap_or(0);
    let secs_of_day = total_secs % 86_400;
    let hour = secs_of_day / 3_600;
    let minute = (secs_of_day % 3_600) / 60;
    let second = secs_of_day % 60;

    // Howard Hinnant's civil-from-days algorithm (UTC).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = u64::try_from(z - era * 146_097).unwrap_or(0);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = i64::try_from(yoe).unwrap_or(0) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + i64::from(month <= 2);

    let mut out = String::with_capacity(24);
    let _ = write!(
        out,
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z"
    );
    out
}
