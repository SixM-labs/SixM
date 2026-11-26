//! Console line tokenizer and command-line argument splitting.
//!
//! The same tokenizer reads `.cfg` files, console input and `ExecuteCommand`
//! lines, so every source follows the same quoting and comment rules.

/// A console line could not be split into words.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    /// A double quote was opened and never closed on the same line.
    #[error("unterminated quoted string")]
    UnterminatedQuote,
}

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

/// Splits a console line into words.
///
/// Words are separated by whitespace. A double-quoted string is one word and
/// may contain whitespace, and `\"` inside quotes stands for a quote. A `#` or
/// `//` outside quotes starts a comment that runs to the end of the line. A
/// line with no words, or only a comment, gives an empty list.
///
/// # Errors
///
/// Returns [`TokenError::UnterminatedQuote`] when a quote is never closed.
pub fn tokenize(line: &str) -> Result<Vec<String>, TokenError> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '"' => in_quotes = false,
                '\\' if chars.peek() == Some(&'"') => {
                    chars.next();
                    word.get_or_insert_with(String::new).push('"');
                }
                _ => word.get_or_insert_with(String::new).push(c),
            }
            continue;
        }
        match c {
            '"' => {
                in_quotes = true;
                word.get_or_insert_with(String::new);
            }
            c if c.is_whitespace() => {
                if let Some(finished) = word.take() {
                    words.push(finished);
                }
            }
            '#' => break,
            '/' if chars.peek() == Some(&'/') => break,
            _ => word.get_or_insert_with(String::new).push(c),
        }
    }

    if in_quotes {
        return Err(TokenError::UnterminatedQuote);
    }
    if let Some(finished) = word {
        words.push(finished);
    }
    Ok(words)
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
