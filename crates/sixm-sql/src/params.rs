//! Tokenizer-safe SQL placeholder parsing and parameter binding.
//!
//! No string interpolation is ever performed. Placeholders (`?`, `@name`,
//! `:name`) are recognized strictly outside string literals, quoted identifiers,
//! and SQL comments, and lowered to driver-native parameter bindings.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

use crate::{ParamSet, SqlError, SqlValue};

/// Target SQL dialect for lowered placeholder syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Mysql,
    Sqlite,
}

/// Kind of placeholder found in a scanned SQL statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaceholderKind {
    None,
    Positional,
    Named,
}

/// One scanned placeholder occurrence inside a SQL string.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TokenPlaceholder {
    Positional,
    Named(String),
}

/// A scanned, single-statement SQL template with extracted placeholders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSql {
    /// Text chunks around placeholders (len == `placeholders.len() + 1`).
    chunks: Vec<String>,
    placeholders: Vec<TokenPlaceholder>,
    kind: PlaceholderKind,
}

/// A single SQL statement paired with ordered parameter values ready for execution.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundQuery {
    pub sql: String,
    pub params: Vec<SqlValue>,
}

impl ParsedSql {
    /// Scans `sql` into a single-statement template using MySQL backslash-escape rules.
    ///
    /// Rejects empty statements, unterminated string/comment delimiters,
    /// stacked multi-statements, mixed positional/named placeholders, and
    /// numbered `?1` placeholders.
    pub fn parse(sql: &str) -> Result<Self, SqlError> {
        Self::parse_for(sql, Dialect::Mysql)
    }

    /// Scans `sql` into a single-statement template for a specific SQL [`Dialect`].
    ///
    /// MySQL treats `\` as an escape inside quoted strings, whereas SQLite treats
    /// `\` as a literal character (only doubled quotes `''` / `""` escape).
    pub fn parse_for(sql: &str, dialect: Dialect) -> Result<Self, SqlError> {
        let backslash_escapes = dialect == Dialect::Mysql;
        let bytes = sql.as_bytes();
        let mut chunks = Vec::new();
        let mut placeholders = Vec::new();
        let mut kind = PlaceholderKind::None;
        let mut chunk_start = 0;
        let mut end_limit = bytes.len();
        let mut saw_content = false;
        let mut i = 0;

        while i < bytes.len() {
            let b = bytes[i];
            if b.is_ascii_whitespace() {
                i += 1;
                continue;
            }
            if let Some(next_i) = skip_comment(bytes, i, dialect)? {
                i = next_i;
                continue;
            }
            if let Some(next_i) = skip_quoted(bytes, i, backslash_escapes)? {
                saw_content = true;
                i = next_i;
                continue;
            }
            if b == b';' {
                if !saw_content {
                    return Err(SqlError::InvalidSql(
                        "empty SQL statement before `;`".to_owned(),
                    ));
                }
                end_limit = i;
                verify_only_trailing_comments(bytes, i + 1, dialect)?;
                break;
            }

            saw_content = true;

            if let Some((next_i, ph_kind, ph)) = scan_placeholder(sql, bytes, i)? {
                kind = merge_placeholder_kind(kind, ph_kind)?;
                chunks.push(sql[chunk_start..i].to_owned());
                placeholders.push(ph);
                i = next_i;
                chunk_start = i;
                continue;
            }

            i = skip_non_placeholder_sigil(bytes, i);
        }

        if !saw_content {
            return Err(SqlError::InvalidSql("SQL query is empty".to_owned()));
        }

        chunks.push(sql[chunk_start..end_limit].trim_end().to_owned());

        Ok(Self {
            chunks,
            placeholders,
            kind,
        })
    }

    /// Returns the number of placeholder occurrences in this query.
    pub fn placeholder_count(&self) -> usize {
        self.placeholders.len()
    }

    /// Binds `params` to this parsed SQL statement for `dialect`.
    pub fn bind(&self, params: &ParamSet, dialect: Dialect) -> Result<BoundQuery, SqlError> {
        match self.kind {
            PlaceholderKind::None => {
                match params {
                    ParamSet::Empty | ParamSet::Named(_) => {}
                    ParamSet::Positional(values) if values.is_empty() => {}
                    ParamSet::Positional(values) => {
                        return Err(SqlError::InvalidParam(format!(
                            "query has 0 placeholders, but {} positional parameters were supplied",
                            values.len()
                        )));
                    }
                }
                Ok(BoundQuery {
                    sql: self.chunks[0].trim().to_owned(),
                    params: Vec::new(),
                })
            }
            PlaceholderKind::Positional => self.bind_positional(params),
            PlaceholderKind::Named => self.bind_named(params, dialect),
        }
    }

    fn bind_positional(&self, params: &ParamSet) -> Result<BoundQuery, SqlError> {
        let expected = self.placeholders.len();
        let values = match params {
            ParamSet::Positional(values) => values,
            ParamSet::Empty => {
                return Err(SqlError::InvalidParam(format!(
                    "expected {expected} positional parameters, got 0"
                )));
            }
            ParamSet::Named(map) => {
                if map.is_empty() {
                    return Err(SqlError::InvalidParam(format!(
                        "expected {expected} positional parameters, got 0"
                    )));
                }
                return Err(SqlError::InvalidParam(
                    "query uses positional `?` placeholders, but named parameters were provided"
                        .to_owned(),
                ));
            }
        };

        if values.len() != expected {
            return Err(SqlError::InvalidParam(format!(
                "expected {expected} positional parameters, got {}",
                values.len()
            )));
        }

        let mut sql =
            String::with_capacity(self.chunks.iter().map(String::len).sum::<usize>() + expected);
        for (idx, chunk) in self.chunks.iter().enumerate() {
            if idx > 0 {
                sql.push('?');
            }
            sql.push_str(chunk);
        }

        Ok(BoundQuery {
            sql: sql.trim().to_owned(),
            params: values.clone(),
        })
    }

    fn bind_named(&self, params: &ParamSet, dialect: Dialect) -> Result<BoundQuery, SqlError> {
        let empty_map = BTreeMap::new();
        let map = match params {
            ParamSet::Named(map) => map,
            ParamSet::Empty => &empty_map,
            ParamSet::Positional(values) if values.is_empty() => &empty_map,
            ParamSet::Positional(_) => {
                return Err(SqlError::InvalidParam(
                    "query uses named `@name`/`:name` placeholders, but positional parameters were provided".to_owned(),
                ));
            }
        };

        let mut sql = String::with_capacity(
            self.chunks.iter().map(String::len).sum::<usize>() + self.placeholders.len() * 3,
        );
        let mut bound = Vec::new();
        let mut sqlite_indices: HashMap<&str, usize> = HashMap::new();

        for (idx, chunk) in self.chunks.iter().enumerate() {
            if idx > 0 {
                let TokenPlaceholder::Named(name) = &self.placeholders[idx - 1] else {
                    return Err(SqlError::InvalidParam(
                        "unexpected positional placeholder in named query".to_owned(),
                    ));
                };
                let value = map.get(name.as_str()).ok_or_else(|| {
                    SqlError::InvalidParam(format!("missing named parameter `{name}`"))
                })?;

                match dialect {
                    Dialect::Mysql => {
                        sql.push('?');
                        bound.push(value.clone());
                    }
                    Dialect::Sqlite => {
                        let slot = if let Some(&existing) = sqlite_indices.get(name.as_str()) {
                            existing
                        } else {
                            let next = bound.len() + 1;
                            sqlite_indices.insert(name.as_str(), next);
                            bound.push(value.clone());
                            next
                        };
                        let _ = write!(sql, "?{slot}");
                    }
                }
            }
            sql.push_str(chunk);
        }

        Ok(BoundQuery {
            sql: sql.trim().to_owned(),
            params: bound,
        })
    }
}

/// Convenience helper to parse and bind a SQL query in one call.
pub fn bind_query(sql: &str, params: &ParamSet, dialect: Dialect) -> Result<BoundQuery, SqlError> {
    ParsedSql::parse_for(sql, dialect)?.bind(params, dialect)
}

/// Restrict transaction items to queries and row changes. The pool owns the
/// transaction boundaries, and MySQL schema changes can implicitly commit.
pub(crate) fn bind_transaction_item(
    sql: &str,
    params: &ParamSet,
    dialect: Dialect,
) -> Result<BoundQuery, SqlError> {
    let bytes = sql.as_bytes();
    let mut start = 0;
    while let Some(byte) = bytes.get(start) {
        if byte.is_ascii_whitespace() {
            start += 1;
        } else if let Some(next) = skip_comment(bytes, start, dialect)? {
            start = next;
        } else {
            break;
        }
    }
    let keyword = &bytes[start..scan_ident_end(bytes, start)];
    let allowed: [&[u8]; 7] = [
        b"SELECT", b"WITH", b"INSERT", b"UPDATE", b"DELETE", b"REPLACE", b"VALUES",
    ];
    if !allowed
        .iter()
        .any(|verb| keyword.eq_ignore_ascii_case(verb))
    {
        return Err(SqlError::InvalidSql(
            "transactions only accept row queries and data changes".to_owned(),
        ));
    }
    bind_query(sql, params, dialect)
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn skip_comment(bytes: &[u8], i: usize, dialect: Dialect) -> Result<Option<usize>, SqlError> {
    let b = bytes[i];
    if b == b'-'
        && bytes.get(i + 1) == Some(&b'-')
        && (dialect == Dialect::Sqlite
            || bytes
                .get(i + 2)
                .is_none_or(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control()))
    {
        return Ok(Some(skip_line_comment(bytes, i + 2)));
    }
    if b == b'#' && dialect == Dialect::Mysql {
        return Ok(Some(skip_line_comment(bytes, i + 1)));
    }
    if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
        if dialect == Dialect::Mysql
            && (bytes[i + 2..].starts_with(b"!")
                || bytes
                    .get(i + 2..i + 4)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"M!")))
        {
            return Err(SqlError::InvalidSql(
                "MySQL executable comments are not supported".to_owned(),
            ));
        }
        return skip_block_comment(bytes, i + 2).map(Some);
    }
    Ok(None)
}

fn skip_quoted(bytes: &[u8], i: usize, backslash_escapes: bool) -> Result<Option<usize>, SqlError> {
    match bytes[i] {
        b'\'' => skip_single_quoted(bytes, i + 1, backslash_escapes).map(Some),
        b'"' => skip_double_quoted(bytes, i + 1, backslash_escapes).map(Some),
        b'`' => skip_backtick_quoted(bytes, i + 1).map(Some),
        _ => Ok(None),
    }
}

fn verify_only_trailing_comments(
    bytes: &[u8],
    mut i: usize,
    dialect: Dialect,
) -> Result<(), SqlError> {
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
        } else if let Some(next_i) = skip_comment(bytes, i, dialect)? {
            i = next_i;
        } else {
            return Err(SqlError::InvalidSql(
                "multiple SQL statements are not allowed".to_owned(),
            ));
        }
    }
    Ok(())
}

fn merge_placeholder_kind(
    current: PlaceholderKind,
    next: PlaceholderKind,
) -> Result<PlaceholderKind, SqlError> {
    if current != PlaceholderKind::None && current != next {
        return Err(SqlError::InvalidParam(
            "cannot mix positional `?` and named `@name`/`:name` placeholders".to_owned(),
        ));
    }
    Ok(next)
}

fn scan_placeholder(
    sql: &str,
    bytes: &[u8],
    i: usize,
) -> Result<Option<(usize, PlaceholderKind, TokenPlaceholder)>, SqlError> {
    match bytes[i] {
        b'?' => {
            if bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
                return Err(SqlError::InvalidParam(
                    "numbered `?N` placeholders are not supported; use `?` or `@name`".to_owned(),
                ));
            }
            Ok(Some((
                i + 1,
                PlaceholderKind::Positional,
                TokenPlaceholder::Positional,
            )))
        }
        b'@' if bytes.get(i + 1) != Some(&b'@')
            && bytes.get(i + 1).is_some_and(|c| is_ident_start(*c)) =>
        {
            let name_end = scan_ident_end(bytes, i + 2);
            let name = sql[i + 1..name_end].to_owned();
            Ok(Some((
                name_end,
                PlaceholderKind::Named,
                TokenPlaceholder::Named(name),
            )))
        }
        b':' if !matches!(bytes.get(i + 1), Some(b'=' | b':'))
            && bytes.get(i + 1).is_some_and(|c| is_ident_start(*c)) =>
        {
            let name_end = scan_ident_end(bytes, i + 2);
            let name = sql[i + 1..name_end].to_owned();
            Ok(Some((
                name_end,
                PlaceholderKind::Named,
                TokenPlaceholder::Named(name),
            )))
        }
        _ => Ok(None),
    }
}

fn scan_ident_end(bytes: &[u8], mut i: usize) -> usize {
    while bytes.get(i).is_some_and(|c| is_ident_continue(*c)) {
        i += 1;
    }
    i
}

fn skip_non_placeholder_sigil(bytes: &[u8], i: usize) -> usize {
    if bytes[i] == b'@' && bytes.get(i + 1) == Some(&b'@') {
        return scan_ident_end(bytes, i + 2);
    }
    if bytes[i] == b':' && matches!(bytes.get(i + 1), Some(b'=' | b':')) {
        return i + 2;
    }
    i + 1
}

fn skip_line_comment(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }
    i
}

fn skip_block_comment(bytes: &[u8], mut i: usize) -> Result<usize, SqlError> {
    while i + 1 < bytes.len() {
        if bytes[i] == b'*' && bytes[i + 1] == b'/' {
            return Ok(i + 2);
        }
        i += 1;
    }
    Err(SqlError::InvalidSql(
        "unterminated `/* ... */` block comment".to_owned(),
    ))
}

fn skip_single_quoted(
    bytes: &[u8],
    mut i: usize,
    backslash_escapes: bool,
) -> Result<usize, SqlError> {
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if backslash_escapes => {
                if i + 1 >= bytes.len() {
                    return Err(SqlError::InvalidSql(
                        "unterminated single-quoted string literal".to_owned(),
                    ));
                }
                i += 2;
            }
            b'\'' => {
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                } else {
                    return Ok(i + 1);
                }
            }
            _ => i += 1,
        }
    }
    Err(SqlError::InvalidSql(
        "unterminated single-quoted string literal".to_owned(),
    ))
}

fn skip_double_quoted(
    bytes: &[u8],
    mut i: usize,
    backslash_escapes: bool,
) -> Result<usize, SqlError> {
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if backslash_escapes => {
                if i + 1 >= bytes.len() {
                    return Err(SqlError::InvalidSql(
                        "unterminated double-quoted string or identifier".to_owned(),
                    ));
                }
                i += 2;
            }
            b'"' => {
                if bytes.get(i + 1) == Some(&b'"') {
                    i += 2;
                } else {
                    return Ok(i + 1);
                }
            }
            _ => i += 1,
        }
    }
    Err(SqlError::InvalidSql(
        "unterminated double-quoted string or identifier".to_owned(),
    ))
}

fn skip_backtick_quoted(bytes: &[u8], mut i: usize) -> Result<usize, SqlError> {
    while i < bytes.len() {
        if bytes[i] == b'`' {
            if bytes.get(i + 1) == Some(&b'`') {
                i += 2;
            } else {
                return Ok(i + 1);
            }
        } else {
            i += 1;
        }
    }
    Err(SqlError::InvalidSql(
        "unterminated backtick identifier".to_owned(),
    ))
}
