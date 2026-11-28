//! Connection string parsing and redacted database configuration.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use crate::SqlError;

/// Supported SQL database backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlBackend {
    Mysql,
    Sqlite,
}

/// TLS / SSL verification mode for MySQL / MariaDB connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SslMode {
    #[default]
    Disabled,
    Preferred,
    Required,
    VerifyCa,
    VerifyIdentity,
}

/// Target storage location for SQLite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqliteTarget {
    Memory,
    File(PathBuf),
}

/// Configuration for the embedded SQLite backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteConfig {
    pub target: SqliteTarget,
}

/// Configuration for the MySQL / MariaDB backend.
///
/// Credentials are redacted in [`fmt::Debug`].
#[derive(Clone, PartialEq, Eq)]
pub struct MysqlConfig {
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
    pub password: Option<String>,
    pub database: Option<String>,
    pub charset: String,
    pub connect_timeout: Option<Duration>,
    pub ssl_mode: SslMode,
    pub date_strings: bool,
}

impl Default for MysqlConfig {
    fn default() -> Self {
        Self {
            host: String::from("127.0.0.1"),
            port: 3306,
            user: None,
            password: None,
            database: None,
            charset: String::from("utf8mb4"),
            connect_timeout: None,
            ssl_mode: SslMode::Disabled,
            date_strings: false,
        }
    }
}

impl fmt::Debug for MysqlConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MysqlConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .field("database", &self.database)
            .field("charset", &self.charset)
            .field("connect_timeout", &self.connect_timeout)
            .field("ssl_mode", &self.ssl_mode)
            .field("date_strings", &self.date_strings)
            .finish()
    }
}

/// Parsed database configuration from `mysql_connection_string`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DatabaseConfig {
    Mysql(MysqlConfig),
    Sqlite(SqliteConfig),
}

impl DatabaseConfig {
    /// Parses a `mysql_connection_string` in either URI (`mysql://`, `mysql2://`,
    /// `mariadb://`, `sqlite://`, `sqlite:`, `sqlite::memory:`) or semicolon
    /// `key=value;` form.
    ///
    /// Returns `Ok(None)` when `raw` is empty or whitespace-only so an
    /// unconfigured server leaves the database subsystem inactive.
    pub fn parse(raw: &str) -> Result<Option<Self>, SqlError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }

        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("mysql://")
            || lower.starts_with("mysql2://")
            || lower.starts_with("mariadb://")
        {
            return parse_mysql_uri(trimmed).map(Some);
        }
        if lower.starts_with("sqlite:") {
            return parse_sqlite_uri(trimmed).map(Some);
        }
        if let Some((scheme, _)) = trimmed.split_once("://")
            && !scheme.contains('=')
            && !scheme.contains(';')
        {
            return Err(SqlError::InvalidConfig(format!(
                "unsupported database URI scheme `{scheme}`"
            )));
        }

        parse_key_value(trimmed).map(Some)
    }

    /// Returns the backend selected by this configuration.
    pub fn backend(&self) -> SqlBackend {
        match self {
            Self::Mysql(_) => SqlBackend::Mysql,
            Self::Sqlite(_) => SqlBackend::Sqlite,
        }
    }
}

fn parse_mysql_uri(raw: &str) -> Result<DatabaseConfig, SqlError> {
    let (_, rest) = raw
        .split_once("://")
        .ok_or_else(|| SqlError::InvalidConfig("missing `://` in MySQL URI".to_owned()))?;

    let (authority_and_path, query) = match rest.split_once('?') {
        Some((before, q)) => (before, Some(q)),
        None => (rest, None),
    };

    let (authority, path) = match authority_and_path.split_once('/') {
        Some((auth, db)) => (auth, Some(db)),
        None => (authority_and_path, None),
    };

    if authority.is_empty() {
        return Err(SqlError::InvalidConfig(
            "MySQL URI is missing host authority".to_owned(),
        ));
    }

    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((ui, hp)) => (Some(ui), hp),
        None => (None, authority),
    };

    let mut cfg = MysqlConfig::default();

    if let Some(ui) = userinfo {
        match ui.split_once(':') {
            Some((u, p)) => {
                if !u.is_empty() {
                    cfg.user = Some(percent_decode_utf8(u, "username")?);
                }
                if !p.is_empty() {
                    cfg.password = Some(percent_decode_utf8(p, "password")?);
                }
            }
            None => {
                if !ui.is_empty() {
                    cfg.user = Some(percent_decode_utf8(ui, "username")?);
                }
            }
        }
    }

    let (host, port) = parse_host_port(hostport)?;
    cfg.host = host;
    if let Some(port) = port {
        cfg.port = port;
    }

    if let Some(db) = path
        && !db.is_empty()
    {
        cfg.database = Some(percent_decode_utf8(db, "database")?);
    }

    if let Some(query) = query {
        for pair in query.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (raw_key, raw_val) = match pair.split_once('=') {
                Some((k, v)) => (k, v),
                None => (pair, ""),
            };
            let key = percent_decode_utf8(raw_key, "query option key")?;
            let val = percent_decode_utf8(raw_val, "query option value")?;
            apply_mysql_option(&mut cfg, &key, &val)?;
        }
    }

    Ok(DatabaseConfig::Mysql(cfg))
}

fn parse_host_port(hostport: &str) -> Result<(String, Option<u16>), SqlError> {
    if hostport.is_empty() {
        return Err(SqlError::InvalidConfig(
            "MySQL connection string has an empty host".to_owned(),
        ));
    }

    if let Some(after_bracket) = hostport.strip_prefix('[') {
        let Some((ipv6, rest)) = after_bracket.split_once(']') else {
            return Err(SqlError::InvalidConfig(
                "unterminated IPv6 host bracket in MySQL URI".to_owned(),
            ));
        };
        if ipv6.is_empty() {
            return Err(SqlError::InvalidConfig(
                "empty IPv6 host in MySQL URI".to_owned(),
            ));
        }
        let port = if rest.is_empty() {
            None
        } else if let Some(port_str) = rest.strip_prefix(':') {
            Some(parse_port(port_str)?)
        } else {
            return Err(SqlError::InvalidConfig(format!(
                "unexpected characters `{rest}` after IPv6 host"
            )));
        };
        return Ok((ipv6.to_owned(), port));
    }

    if let Some((host, port_str)) = hostport.rsplit_once(':') {
        if host.contains(':') {
            return Err(SqlError::InvalidConfig(
                "IPv6 hosts in URIs must be wrapped in `[...]`".to_owned(),
            ));
        }
        let host = percent_decode_utf8(host, "host")?;
        if host.is_empty() {
            return Err(SqlError::InvalidConfig(
                "MySQL connection string has an empty host".to_owned(),
            ));
        }
        let port = if port_str.is_empty() {
            None
        } else {
            Some(parse_port(port_str)?)
        };
        Ok((host, port))
    } else {
        let host = percent_decode_utf8(hostport, "host")?;
        Ok((host, None))
    }
}

fn parse_sqlite_uri(raw: &str) -> Result<DatabaseConfig, SqlError> {
    let lower = raw.to_ascii_lowercase();
    if lower == "sqlite::memory:" || lower == "sqlite://:memory:" || lower == "sqlite:///:memory:" {
        return Ok(DatabaseConfig::Sqlite(SqliteConfig {
            target: SqliteTarget::Memory,
        }));
    }

    let (rest, had_slashes) = if let Some(after) = raw.get(9..)
        && lower.starts_with("sqlite://")
    {
        (after, true)
    } else if let Some(after) = raw.get(7..)
        && lower.starts_with("sqlite:")
    {
        (after, false)
    } else {
        return Err(SqlError::InvalidConfig(
            "invalid SQLite connection URI".to_owned(),
        ));
    };

    let (path_part, query) = match rest.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (rest, None),
    };

    if let Some(q) = query {
        for pair in q.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (raw_k, raw_v) = pair.split_once('=').unwrap_or((pair, ""));
            let key = percent_decode_utf8(raw_k, "sqlite query key")?;
            let val = percent_decode_utf8(raw_v, "sqlite query value")?;
            if key.eq_ignore_ascii_case("mode") && val.eq_ignore_ascii_case("memory") {
                return Ok(DatabaseConfig::Sqlite(SqliteConfig {
                    target: SqliteTarget::Memory,
                }));
            }
        }
    }

    let decoded = percent_decode_utf8(path_part, "sqlite path")?;
    if decoded == ":memory:" || decoded == "/:memory:" {
        return Ok(DatabaseConfig::Sqlite(SqliteConfig {
            target: SqliteTarget::Memory,
        }));
    }

    let normalized = normalize_sqlite_uri_path(&decoded, had_slashes);
    if normalized.is_empty() {
        return Err(SqlError::InvalidConfig(
            "SQLite URI is missing a database path".to_owned(),
        ));
    }

    Ok(DatabaseConfig::Sqlite(SqliteConfig {
        target: SqliteTarget::File(PathBuf::from(normalized)),
    }))
}

fn normalize_sqlite_uri_path(decoded: &str, _had_slashes: bool) -> &str {
    // On Windows, `sqlite:///C:/path/to.db` yields `/C:/path/to.db`.
    #[cfg(windows)]
    if let Some(after_slash) = decoded.strip_prefix('/') {
        let bytes = after_slash.as_bytes();
        if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            return after_slash;
        }
    }
    decoded
}

fn split_key_value_segments(raw: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let bytes = raw.as_bytes();
    let mut start = 0;
    let mut quote: Option<u8> = None;

    for (i, &b) in bytes.iter().enumerate() {
        match quote {
            Some(q) if b == q => quote = None,
            None if b == b'\'' || b == b'"' => quote = Some(b),
            None if b == b';' => {
                segments.push(&raw[start..i]);
                start = i + 1;
            }
            Some(_) | None => {}
        }
    }
    if start <= raw.len() {
        segments.push(&raw[start..]);
    }
    segments
}

fn parse_key_value(raw: &str) -> Result<DatabaseConfig, SqlError> {
    let mut driver: Option<SqlBackend> = None;
    let mut sqlite_path: Option<String> = None;
    let mut host: Option<String> = None;
    let mut mysql_cfg = MysqlConfig::default();
    let mut saw_mysql_field = false;

    for segment in split_key_value_segments(raw) {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        let Some((raw_key, raw_val)) = segment.split_once('=') else {
            return Err(SqlError::InvalidConfig(
                "invalid key=value segment in connection string (missing `=`)".to_owned(),
            ));
        };
        let key = raw_key.trim();
        let val = unquote(raw_val.trim());
        if key.is_empty() {
            return Err(SqlError::InvalidConfig(
                "empty key in connection string".to_owned(),
            ));
        }

        match key.to_ascii_lowercase().as_str() {
            "driver" | "provider" => match val.to_ascii_lowercase().as_str() {
                "mysql" | "mysql2" | "mariadb" => driver = Some(SqlBackend::Mysql),
                "sqlite" | "sqlite3" => driver = Some(SqlBackend::Sqlite),
                other => {
                    return Err(SqlError::InvalidConfig(format!(
                        "unsupported database driver `{other}`"
                    )));
                }
            },
            "filename" | "data source" | "filepath" => {
                sqlite_path = Some(val.to_owned());
            }
            "host" | "server" | "addr" | "address" => {
                if val.is_empty() {
                    return Err(SqlError::InvalidConfig(
                        "connection string host cannot be empty".to_owned(),
                    ));
                }
                host = Some(val.to_owned());
                saw_mysql_field = true;
            }
            "port" => {
                mysql_cfg.port = parse_port(val)?;
                saw_mysql_field = true;
            }
            "user" | "uid" | "username" | "userid" => {
                if !val.is_empty() {
                    mysql_cfg.user = Some(val.to_owned());
                }
                saw_mysql_field = true;
            }
            "password" | "pwd" => {
                if !val.is_empty() {
                    mysql_cfg.password = Some(val.to_owned());
                }
                saw_mysql_field = true;
            }
            "database" | "db" | "initial catalog" => {
                if !val.is_empty() {
                    mysql_cfg.database = Some(val.to_owned());
                }
                saw_mysql_field = true;
            }
            _ => {
                apply_mysql_option(&mut mysql_cfg, key, val)?;
            }
        }
    }

    let resolved_backend = match driver {
        Some(backend) => backend,
        None if sqlite_path.is_some() && !saw_mysql_field => SqlBackend::Sqlite,
        None => SqlBackend::Mysql,
    };

    finalize_key_value_config(resolved_backend, sqlite_path, host, mysql_cfg)
}

fn finalize_key_value_config(
    backend: SqlBackend,
    sqlite_path: Option<String>,
    host: Option<String>,
    mut mysql_cfg: MysqlConfig,
) -> Result<DatabaseConfig, SqlError> {
    match backend {
        SqlBackend::Sqlite => {
            let path = sqlite_path.ok_or_else(|| {
                SqlError::InvalidConfig(
                    "SQLite connection string requires `filename` / `data source`".to_owned(),
                )
            })?;
            if path.is_empty() {
                return Err(SqlError::InvalidConfig(
                    "SQLite filename cannot be empty".to_owned(),
                ));
            }
            let target = if path == ":memory:" {
                SqliteTarget::Memory
            } else {
                SqliteTarget::File(PathBuf::from(path))
            };
            Ok(DatabaseConfig::Sqlite(SqliteConfig { target }))
        }
        SqlBackend::Mysql => {
            let host = host.ok_or_else(|| {
                SqlError::InvalidConfig(
                    "MySQL connection string requires `host` or `server`".to_owned(),
                )
            })?;
            mysql_cfg.host = host;
            Ok(DatabaseConfig::Mysql(mysql_cfg))
        }
    }
}

fn apply_mysql_option(cfg: &mut MysqlConfig, key: &str, val: &str) -> Result<(), SqlError> {
    match key.to_ascii_lowercase().as_str() {
        "charset" => {
            if val.is_empty() || !val.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                return Err(SqlError::InvalidConfig(format!(
                    "invalid MySQL charset `{val}`"
                )));
            }
            val.clone_into(&mut cfg.charset);
        }
        "connecttimeout" | "connect_timeout" | "connection timeout" => {
            let ms = val.parse::<u64>().map_err(|_| {
                SqlError::InvalidConfig(format!("invalid connectTimeout value `{val}`"))
            })?;
            cfg.connect_timeout = (ms > 0).then(|| Duration::from_millis(ms));
        }
        "multiplestatements" | "multiple_statements" => {
            if parse_bool(val, key)? {
                return Err(SqlError::InvalidConfig(
                    "multipleStatements=true is disabled for security; stacked queries are not permitted".to_owned(),
                ));
            }
        }
        "datestrings" | "date_strings" => {
            cfg.date_strings = parse_bool(val, key)?;
        }
        "ssl" | "sslmode" | "ssl-mode" | "ssl mode" | "require_ssl" => {
            cfg.ssl_mode = parse_ssl_mode(val)?;
        }
        _ => {}
    }
    Ok(())
}

fn parse_port(raw: &str) -> Result<u16, SqlError> {
    let port = raw
        .parse::<u16>()
        .map_err(|_| SqlError::InvalidConfig(format!("invalid port `{raw}`")))?;
    if port == 0 {
        return Err(SqlError::InvalidConfig(
            "database port must be greater than 0".to_owned(),
        ));
    }
    Ok(port)
}

fn parse_bool(raw: &str, key: &str) -> Result<bool, SqlError> {
    match raw.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" | "" => Ok(false),
        _ => Err(SqlError::InvalidConfig(format!(
            "invalid boolean value `{raw}` for `{key}`"
        ))),
    }
}

fn parse_ssl_mode(raw: &str) -> Result<SslMode, SqlError> {
    match raw.to_ascii_lowercase().as_str() {
        "false" | "0" | "no" | "off" | "disable" | "disabled" | "none" => Ok(SslMode::Disabled),
        "preferred" | "prefer" => Ok(SslMode::Preferred),
        "true" | "1" | "yes" | "on" | "require" | "required" => Ok(SslMode::Required),
        "verify_ca" | "verify-ca" => Ok(SslMode::VerifyCa),
        "verify_identity" | "verify-identity" | "verify_full" | "verify-full" => {
            Ok(SslMode::VerifyIdentity)
        }
        _ => Err(SqlError::InvalidConfig(format!(
            "unsupported ssl/sslmode value `{raw}`"
        ))),
    }
}

fn unquote(raw: &str) -> &str {
    if raw.len() >= 2
        && ((raw.starts_with('\'') && raw.ends_with('\''))
            || (raw.starts_with('"') && raw.ends_with('"')))
    {
        &raw[1..raw.len() - 1]
    } else {
        raw
    }
}

fn percent_decode_utf8(input: &str, field: &str) -> Result<String, SqlError> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let Some(&hi) = bytes.get(i + 1) else {
                return Err(SqlError::InvalidConfig(format!(
                    "incomplete percent-encoding in {field}"
                )));
            };
            let Some(&lo) = bytes.get(i + 2) else {
                return Err(SqlError::InvalidConfig(format!(
                    "incomplete percent-encoding in {field}"
                )));
            };
            let hi_val = hex_nibble(hi).ok_or_else(|| {
                SqlError::InvalidConfig(format!("invalid percent-encoding in {field}"))
            })?;
            let lo_val = hex_nibble(lo).ok_or_else(|| {
                SqlError::InvalidConfig(format!("invalid percent-encoding in {field}"))
            })?;
            out.push((hi_val << 4) | lo_val);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out)
        .map_err(|_| SqlError::InvalidConfig(format!("{field} is not valid UTF-8")))
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
