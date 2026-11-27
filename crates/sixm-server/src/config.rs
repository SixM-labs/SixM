//! Command-line argument splitting and CLI option parsing. The console
//! tokenizer lives in `sixm-core`, shared with the client, and is re-exported
//! here.

use std::path::PathBuf;

pub use sixm_core::{TokenError, tokenize};

/// The command line could not be read as CLI options and `+command` groups.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArgError {
    /// An argument appeared before the first `+command`.
    #[error("argument `{0}` does not belong to a command; commands start with `+`")]
    NotACommand(String),
    /// A `+` with no command name after it.
    #[error("a bare `+` is not a command")]
    EmptyCommand,
    /// A CLI option expected a value argument that was missing.
    #[error("option `{0}` requires a value")]
    MissingValue(String),
}

/// Process-level CLI flags parsed before or alongside `+command` groups.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CliOptions {
    /// Optional path to also write server logs to.
    pub log_file: Option<PathBuf>,
    /// Emit logs as JSON lines instead of compact text.
    pub log_json: bool,
    /// Enable the development resource hot-reload watcher.
    pub watch: bool,
}

/// What the command line asks the server to do (including CLI options).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliInvocation {
    /// `--help` was given: print usage and exit.
    Help,
    /// `--version` was given: print the version and exit.
    Version,
    /// Run the server with `options` and startup console `commands`.
    Run {
        /// Parsed CLI flags.
        options: CliOptions,
        /// Console commands to run in order. Each one is its words, name first.
        commands: Vec<Vec<String>>,
    },
}

/// What the command line asks the server to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// `--help` was given: print usage and exit.
    Help,
    /// `--version` was given: print the version and exit.
    Version,
    /// Console commands to run in order. Each one is its words, name first.
    Run(Vec<Vec<String>>),
}

/// Reads the command-line arguments, including CLI options (`--log-file`,
/// `--log-json`, `--watch`) and `+command` groups.
///
/// `--help` and `--version` win over everything else.
///
/// # Errors
///
/// Returns [`ArgError::NotACommand`] for an unknown argument before the first
/// `+command`, [`ArgError::EmptyCommand`] for a bare `+`, and
/// [`ArgError::MissingValue`] when `--log-file` has no path.
pub fn parse_cli<I>(args: I) -> Result<CliInvocation, ArgError>
where
    I: IntoIterator<Item = String>,
{
    let args: Vec<String> = args.into_iter().collect();
    if args.iter().any(|arg| arg == "--help") {
        return Ok(CliInvocation::Help);
    }
    if args.iter().any(|arg| arg == "--version") {
        return Ok(CliInvocation::Version);
    }

    let mut options = CliOptions::default();
    let mut commands: Vec<Vec<String>> = Vec::new();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        if arg == "--watch" {
            options.watch = true;
        } else if arg == "--log-json" {
            options.log_json = true;
        } else if arg == "--log-file" {
            let value = iter
                .next()
                .filter(|path| !path.is_empty())
                .ok_or_else(|| ArgError::MissingValue("--log-file".to_owned()))?;
            options.log_file = Some(PathBuf::from(value));
        } else if let Some(value) = arg.strip_prefix("--log-file=") {
            if value.is_empty() {
                return Err(ArgError::MissingValue("--log-file".to_owned()));
            }
            options.log_file = Some(PathBuf::from(value));
        } else if let Some(name) = arg.strip_prefix('+') {
            if name.is_empty() {
                return Err(ArgError::EmptyCommand);
            }
            commands.push(vec![name.to_owned()]);
        } else {
            match commands.last_mut() {
                Some(command) => command.push(arg),
                None => return Err(ArgError::NotACommand(arg)),
            }
        }
    }
    Ok(CliInvocation::Run { options, commands })
}

/// Reads the command-line arguments.
///
/// `--help` and `--version` win over everything else. Otherwise each `+name`
/// starts a command, and the arguments after it up to the next `+name` belong
/// to it. The words are kept exactly as the shell passed them, so
/// `+set motd "a # b"` sets `a # b` with no comment handling.
///
/// # Errors
///
/// Returns [`ArgError::NotACommand`] for an argument before the first
/// `+command`, and [`ArgError::EmptyCommand`] for a bare `+`.
pub fn parse_args<I>(args: I) -> Result<Invocation, ArgError>
where
    I: IntoIterator<Item = String>,
{
    let args: Vec<String> = args.into_iter().collect();
    if args.iter().any(|arg| arg == "--help") {
        return Ok(Invocation::Help);
    }
    if args.iter().any(|arg| arg == "--version") {
        return Ok(Invocation::Version);
    }

    let mut commands: Vec<Vec<String>> = Vec::new();
    for arg in args {
        if let Some(name) = arg.strip_prefix('+') {
            if name.is_empty() {
                return Err(ArgError::EmptyCommand);
            }
            commands.push(vec![name.to_owned()]);
        } else {
            match commands.last_mut() {
                Some(command) => command.push(arg),
                None => return Err(ArgError::NotACommand(arg)),
            }
        }
    }
    Ok(Invocation::Run(commands))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cli_extracts_options_and_commands() {
        let args = [
            "--watch",
            "--log-json",
            "--log-file",
            "logs/server.log",
            "+exec",
            "server.cfg",
        ]
        .map(str::to_owned);
        assert_eq!(
            parse_cli(args),
            Ok(CliInvocation::Run {
                options: CliOptions {
                    log_file: Some(PathBuf::from("logs/server.log")),
                    log_json: true,
                    watch: true,
                },
                commands: vec![vec!["exec".to_owned(), "server.cfg".to_owned()]],
            })
        );
    }

    #[test]
    fn parse_cli_supports_equals_log_file_and_rejects_missing_value() {
        let ok = parse_cli(["--log-file=out.log".to_owned()]);
        assert_eq!(
            ok,
            Ok(CliInvocation::Run {
                options: CliOptions {
                    log_file: Some(PathBuf::from("out.log")),
                    log_json: false,
                    watch: false,
                },
                commands: Vec::new(),
            })
        );
        assert_eq!(
            parse_cli(["--log-file".to_owned()]),
            Err(ArgError::MissingValue("--log-file".to_owned()))
        );
        assert_eq!(
            parse_cli(["--log-file=".to_owned()]),
            Err(ArgError::MissingValue("--log-file".to_owned()))
        );
    }
}
