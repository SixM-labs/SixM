//! Command-line argument splitting. The console tokenizer lives in `sixm-core`,
//! shared with the client, and is re-exported here.

pub use sixm_core::{TokenError, tokenize};

/// The command line could not be read as `+command` groups.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArgError {
    /// An argument appeared before the first `+command`.
    #[error("argument `{0}` does not belong to a command; commands start with `+`")]
    NotACommand(String),
    /// A `+` with no command name after it.
    #[error("a bare `+` is not a command")]
    EmptyCommand,
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
