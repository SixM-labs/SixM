//! Console line tokenizer, shared by the server and the client.
//!
//! Every console source (the command line, stdin, `.cfg` files and
//! `ExecuteCommand` lines) follows the same quoting and comment rules.

/// A console line could not be split into words.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    /// A double quote was opened and never closed on the same line.
    #[error("unterminated quoted string")]
    UnterminatedQuote,
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
