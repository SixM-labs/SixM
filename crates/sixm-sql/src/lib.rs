//! Asynchronous SQL database pool supporting MySQL/MariaDB and SQLite.
//!
//! Queries always execute on background worker threads (`rusqlite`) or a
//! dedicated `tokio` runtime (`mysql_async`) so the caller's main tick loop
//! never blocks. All SQL parameter binding is strictly parameterized with zero
//! string interpolation.

pub mod config;
pub mod params;

mod mysql;
mod sqlite;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::time::Duration;

use sixm_core::Value;
use thiserror::Error;

pub use config::{DatabaseConfig, MysqlConfig, SqlBackend, SqliteConfig, SqliteTarget, SslMode};
use mysql::MysqlPool;
use sqlite::SqlitePool;

/// Default connection pool size (`sixm_dbPoolSize`).
pub const DEFAULT_POOL_SIZE: usize = 10;
/// Maximum allowed connection pool size.
pub const MAX_POOL_SIZE: usize = 64;
/// Default per-query / transaction timeout (`sixm_dbTimeoutMs`).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// Minimum allowed query timeout.
pub const MIN_TIMEOUT: Duration = Duration::from_millis(100);
/// Maximum allowed query timeout.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(300);
/// Default maximum rows materialized per query (`sixm_dbMaxRows`).
pub const DEFAULT_MAX_ROWS: usize = 10_000;
/// Maximum allowed row cap.
pub const MAX_MAX_ROWS: usize = 100_000;
/// Default maximum materialized result payload bytes (`sixm_dbMaxResultBytes`, 16 MiB).
pub const DEFAULT_MAX_RESULT_BYTES: usize = 16 * 1024 * 1024;
/// Minimum allowed result byte cap (64 KiB).
pub const MIN_MAX_RESULT_BYTES: usize = 64 * 1024;
/// Maximum allowed result byte cap (64 MiB).
pub const MAX_MAX_RESULT_BYTES: usize = 64 * 1024 * 1024;
/// Default slow-query warning threshold (`sixm_dbSlowQueryWarnMs`).
pub const DEFAULT_SLOW_QUERY_WARN: Duration = Duration::from_millis(200);
/// Default bounded background job queue capacity.
pub const DEFAULT_QUEUE_CAPACITY: usize = 1024;

/// Which result size cap was exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitKind {
    Rows,
    Bytes,
}

impl std::fmt::Display for LimitKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rows => f.write_str("row count"),
            Self::Bytes => f.write_str("result byte size"),
        }
    }
}

/// Errors produced by configuration parsing, parameter binding, limits, or backends.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SqlError {
    #[error("invalid database configuration: {0}")]
    InvalidConfig(String),
    #[error("invalid SQL statement: {0}")]
    InvalidSql(String),
    #[error("invalid SQL parameter: {0}")]
    InvalidParam(String),
    #[error("query {kind} exceeded configured limit of {limit}")]
    ResultTooLarge { kind: LimitKind, limit: usize },
    #[error("SQL operation timed out after {0:?}")]
    Timeout(Duration),
    #[error("SQL job queue is full (capacity {0})")]
    QueueFull(usize),
    #[error("SQL pool is closed")]
    PoolClosed,
    #[error("SQL operation was cancelled")]
    Cancelled,
    #[error("SQL backend error: {0}")]
    Backend(String),
}

/// Operational limits governing connection pools, timeouts, and result caps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqlLimits {
    pub max_rows: usize,
    pub max_result_bytes: usize,
    pub timeout: Duration,
    pub pool_size: usize,
    pub slow_query_warn: Option<Duration>,
    pub queue_capacity: usize,
}

impl Default for SqlLimits {
    fn default() -> Self {
        Self {
            max_rows: DEFAULT_MAX_ROWS,
            max_result_bytes: DEFAULT_MAX_RESULT_BYTES,
            timeout: DEFAULT_TIMEOUT,
            pool_size: DEFAULT_POOL_SIZE,
            slow_query_warn: Some(DEFAULT_SLOW_QUERY_WARN),
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
        }
    }
}

impl SqlLimits {
    /// Returns a copy of `self` with every limit clamped to its documented safe bounds.
    #[must_use]
    pub fn clamped(self) -> Self {
        Self {
            max_rows: self.max_rows.clamp(1, MAX_MAX_ROWS),
            max_result_bytes: self
                .max_result_bytes
                .clamp(MIN_MAX_RESULT_BYTES, MAX_MAX_RESULT_BYTES),
            timeout: self.timeout.clamp(MIN_TIMEOUT, MAX_TIMEOUT),
            pool_size: self.pool_size.clamp(1, MAX_POOL_SIZE),
            slow_query_warn: self.slow_query_warn.filter(|d| !d.is_zero()),
            queue_capacity: self.queue_capacity.max(1),
        }
    }
}

/// A typed SQL parameter or cell value.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlValue {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
}

impl SqlValue {
    /// Approximate serialized payload footprint in bytes for result-cap enforcement.
    pub fn encoded_len(&self) -> usize {
        match self {
            Self::Null | Self::Bool(_) => 1,
            Self::Int(_) | Self::UInt(_) | Self::Float(_) => 8,
            Self::Text(s) => s.len(),
            Self::Bytes(b) => b.len(),
        }
    }
}

impl TryFrom<&Value> for SqlValue {
    type Error = SqlError;

    fn try_from(value: &Value) -> Result<Self, Self::Error> {
        match value {
            Value::Nil => Ok(Self::Null),
            Value::Bool(b) => Ok(Self::Bool(*b)),
            Value::Int(i) => Ok(Self::Int(*i)),
            Value::Float(f) => {
                if !f.is_finite() {
                    return Err(SqlError::InvalidParam(
                        "non-finite floating point value cannot be bound to SQL".to_owned(),
                    ));
                }
                Ok(Self::Float(*f))
            }
            Value::String(bytes) => match std::str::from_utf8(bytes) {
                Ok(s) => Ok(Self::Text(s.to_owned())),
                Err(_) => Ok(Self::Bytes(bytes.to_vec())),
            },
            Value::FuncRef(_) => Err(SqlError::InvalidParam(
                "function references cannot be bound as SQL parameters".to_owned(),
            )),
            Value::Array(_) => Err(SqlError::InvalidParam(
                "nested arrays cannot be bound as scalar SQL parameters".to_owned(),
            )),
            Value::Map(_) => Err(SqlError::InvalidParam(
                "nested maps/tables cannot be bound as scalar SQL parameters".to_owned(),
            )),
            Value::Vector2(_) | Value::Vector3(_) | Value::Vector4(_) | Value::Quat(_) => {
                Err(SqlError::InvalidParam(
                    "vector/quaternion values cannot be bound as SQL parameters".to_owned(),
                ))
            }
        }
    }
}

impl TryFrom<Value> for SqlValue {
    type Error = SqlError;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        Self::try_from(&value)
    }
}

impl From<SqlValue> for Value {
    fn from(value: SqlValue) -> Self {
        match value {
            SqlValue::Null => Self::Nil,
            SqlValue::Bool(b) => Self::Bool(b),
            SqlValue::Int(i) => Self::Int(i),
            SqlValue::UInt(u) => match i64::try_from(u) {
                Ok(i) => Self::Int(i),
                Err(_) => Self::from(u.to_string()),
            },
            SqlValue::Float(f) => Self::Float(f),
            SqlValue::Text(s) => Self::from(s),
            SqlValue::Bytes(b) => Self::String(b.into_boxed_slice()),
        }
    }
}

/// Parameters supplied for a single SQL statement.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum ParamSet {
    #[default]
    Empty,
    Positional(Vec<SqlValue>),
    Named(BTreeMap<String, SqlValue>),
}

impl ParamSet {
    /// Converts a [`sixm_core::Value`] (`Nil`, `Array`, or `Map`) into a [`ParamSet`].
    ///
    /// Named map keys strip an optional leading `@` or `:` prefix to match `oxmysql`
    /// conventions.
    pub fn from_core_value(value: &Value) -> Result<Self, SqlError> {
        match value {
            Value::Nil => Ok(Self::Empty),
            Value::Array(items) => {
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    values.push(SqlValue::try_from(item)?);
                }
                Ok(Self::Positional(values))
            }
            Value::Map(pairs) => {
                let mut map = BTreeMap::new();
                for (raw_key, raw_val) in pairs {
                    let key_str = raw_key.as_str().ok_or_else(|| {
                        SqlError::InvalidParam(
                            "named SQL parameter map keys must be UTF-8 strings".to_owned(),
                        )
                    })?;
                    let normalized = key_str
                        .strip_prefix('@')
                        .or_else(|| key_str.strip_prefix(':'))
                        .unwrap_or(key_str);
                    if normalized.is_empty() {
                        return Err(SqlError::InvalidParam(
                            "named SQL parameter key cannot be empty".to_owned(),
                        ));
                    }
                    map.insert(normalized.to_owned(), SqlValue::try_from(raw_val)?);
                }
                Ok(Self::Named(map))
            }
            _ => Err(SqlError::InvalidParam(
                "SQL parameters must be nil, an array, or a key-value map".to_owned(),
            )),
        }
    }
}

/// Parameters for [`SqlOperation::Prepare`], supporting single or batch execution.
#[derive(Debug, Clone, PartialEq)]
pub enum PrepareParams {
    Single(ParamSet),
    Batch(Vec<ParamSet>),
}

impl PrepareParams {
    /// Parses a [`sixm_core::Value`] as either a single parameter set or a batch of
    /// parameter sets (`[[...], [...]]` or `[{...}, {...}]`).
    pub fn from_core_value(value: &Value) -> Result<Self, SqlError> {
        if let Value::Array(items) = value
            && let Some(first) = items.first()
            && matches!(first, Value::Array(_) | Value::Map(_))
        {
            let mut batch = Vec::with_capacity(items.len());
            for item in items {
                batch.push(ParamSet::from_core_value(item)?);
            }
            return Ok(Self::Batch(batch));
        }
        ParamSet::from_core_value(value).map(Self::Single)
    }
}

/// One materialized result row.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SqlRow {
    pub columns: Vec<(String, SqlValue)>,
}

impl SqlRow {
    /// Converts this row into a [`sixm_core::Value::Map`].
    pub fn into_core_value(self) -> Value {
        let pairs = self
            .columns
            .into_iter()
            .map(|(col, val)| (Value::from(col), Value::from(val)))
            .collect();
        Value::Map(pairs)
    }
}

/// Summary metadata returned by DML statements (`INSERT`, `UPDATE`, `DELETE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DmlSummary {
    pub affected_rows: u64,
    pub insert_id: u64,
    pub changed_rows: u64,
}

impl DmlSummary {
    /// Converts this DML summary into `oxmysql`-shaped `Value::Map`:
    /// `{ affectedRows, insertId, changedRows }`.
    pub fn into_core_value(self) -> Value {
        Value::Map(vec![
            (
                Value::from("affectedRows"),
                Value::Int(i64::try_from(self.affected_rows).unwrap_or(i64::MAX)),
            ),
            (
                Value::from("insertId"),
                Value::Int(i64::try_from(self.insert_id).unwrap_or(i64::MAX)),
            ),
            (
                Value::from("changedRows"),
                Value::Int(i64::try_from(self.changed_rows).unwrap_or(i64::MAX)),
            ),
        ])
    }
}

/// Outcome of a single statement execution (rows or DML summary).
#[derive(Debug, Clone, PartialEq)]
pub enum QueryResultSet {
    Rows(Vec<SqlRow>),
    Dml(DmlSummary),
}

impl QueryResultSet {
    /// Converts this result set into a [`sixm_core::Value`].
    pub fn into_core_value(self) -> Value {
        match self {
            Self::Rows(rows) => {
                Value::Array(rows.into_iter().map(SqlRow::into_core_value).collect())
            }
            Self::Dml(summary) => summary.into_core_value(),
        }
    }
}

/// One statement inside [`SqlOperation::Transaction`].
#[derive(Debug, Clone, PartialEq)]
pub struct TransactionItem {
    pub sql: String,
    pub params: ParamSet,
}

/// A SQL operation dispatched through [`SqlPool`].
#[derive(Debug, Clone, PartialEq)]
pub enum SqlOperation {
    Query { sql: String, params: ParamSet },
    Single { sql: String, params: ParamSet },
    Scalar { sql: String, params: ParamSet },
    Insert { sql: String, params: ParamSet },
    Update { sql: String, params: ParamSet },
    Prepare { sql: String, params: PrepareParams },
    RawExecute { sql: String, params: ParamSet },
    Transaction { items: Vec<TransactionItem> },
}

/// Alias matching the ADR naming.
pub type QueryOp = SqlOperation;

/// Normalized outcome produced by executing a [`SqlOperation`].
#[derive(Debug, Clone, PartialEq)]
pub enum QueryOutcome {
    Query(QueryResultSet),
    Single(Option<SqlRow>),
    Scalar(Option<SqlValue>),
    Insert(u64),
    Update(u64),
    Batch(Vec<QueryResultSet>),
    Transaction(bool),
}

impl QueryOutcome {
    /// Converts this outcome into a [`sixm_core::Value`] matching `oxmysql` return shapes.
    pub fn into_core_value(self) -> Value {
        match self {
            Self::Query(res) => res.into_core_value(),
            Self::Single(Some(row)) => row.into_core_value(),
            Self::Single(None) | Self::Scalar(None) => Value::Nil,
            Self::Scalar(Some(val)) => Value::from(val),
            Self::Insert(id) => Value::Int(i64::try_from(id).unwrap_or(i64::MAX)),
            Self::Update(affected) => Value::Int(i64::try_from(affected).unwrap_or(i64::MAX)),
            Self::Batch(items) => Value::Array(
                items
                    .into_iter()
                    .map(QueryResultSet::into_core_value)
                    .collect(),
            ),
            Self::Transaction(ok) => Value::Bool(ok),
        }
    }
}

#[derive(Debug)]
enum PoolInner {
    Mysql(MysqlPool),
    Sqlite(SqlitePool),
}

/// Database connection pool supporting both non-blocking callback dispatch and
/// synchronous convenience helpers.
#[derive(Debug)]
pub struct SqlPool {
    inner: PoolInner,
}

impl SqlPool {
    /// Connects (or initializes workers) for `config` with clamped `limits`.
    pub fn connect(config: &DatabaseConfig, limits: SqlLimits) -> Result<Self, SqlError> {
        let clamped = limits.clamped();
        let inner = match config {
            DatabaseConfig::Mysql(cfg) => PoolInner::Mysql(MysqlPool::connect(cfg, clamped)?),
            DatabaseConfig::Sqlite(cfg) => PoolInner::Sqlite(SqlitePool::connect(cfg, clamped)?),
        };
        Ok(Self { inner })
    }

    /// Parses `mysql_connection_string` and creates a [`SqlPool`], returning
    /// `Ok(None)` when `connection_string` is empty or whitespace-only.
    pub fn from_connection_string(
        connection_string: &str,
        limits: SqlLimits,
    ) -> Result<Option<Self>, SqlError> {
        let Some(config) = DatabaseConfig::parse(connection_string)? else {
            return Ok(None);
        };
        Self::connect(&config, limits).map(Some)
    }

    /// Returns which backend this pool targets.
    pub fn backend(&self) -> SqlBackend {
        match self.inner {
            PoolInner::Mysql(_) => SqlBackend::Mysql,
            PoolInner::Sqlite(_) => SqlBackend::Sqlite,
        }
    }

    /// Returns `true` while the pool is open and ready to accept queries.
    pub fn is_ready(&self) -> bool {
        match &self.inner {
            PoolInner::Mysql(pool) => pool.is_ready(),
            PoolInner::Sqlite(pool) => pool.is_ready(),
        }
    }

    /// Verifies live database connectivity by acquiring a connection and pinging it.
    pub fn ping(&self) -> Result<(), SqlError> {
        match &self.inner {
            PoolInner::Mysql(pool) => pool.ping(),
            PoolInner::Sqlite(pool) => pool.ping(),
        }
    }

    /// Dispatches `op` onto background workers/tasks and invokes `on_done` off the
    /// caller thread when execution completes or fails.
    pub fn execute_async<F>(&self, op: SqlOperation, on_done: F) -> Result<(), SqlError>
    where
        F: FnOnce(Result<QueryOutcome, SqlError>) + Send + 'static,
    {
        self.dispatch(op, None, Box::new(on_done))
    }

    /// Dispatches `op` with an optional cooperative cancellation flag (`Arc<AtomicBool>`).
    pub fn execute_async_cancellable<F>(
        &self,
        op: SqlOperation,
        cancelled: Arc<AtomicBool>,
        on_done: F,
    ) -> Result<(), SqlError>
    where
        F: FnOnce(Result<QueryOutcome, SqlError>) + Send + 'static,
    {
        self.dispatch(op, Some(cancelled), Box::new(on_done))
    }

    fn dispatch(
        &self,
        op: SqlOperation,
        cancelled: Option<Arc<AtomicBool>>,
        on_done: Box<dyn FnOnce(Result<QueryOutcome, SqlError>) + Send>,
    ) -> Result<(), SqlError> {
        match &self.inner {
            PoolInner::Mysql(pool) => pool.execute_async(op, cancelled, on_done),
            PoolInner::Sqlite(pool) => pool.execute_async(op, cancelled, on_done),
        }
    }

    /// Executes `op` synchronously, waiting for the background worker to complete.
    pub fn execute(&self, op: SqlOperation) -> Result<QueryOutcome, SqlError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.execute_async(op, move |outcome| {
            let _ = tx.send(outcome);
        })?;
        rx.recv().map_err(|_| SqlError::PoolClosed)?
    }

    /// Executes a `query` operation synchronously.
    pub fn query(
        &self,
        sql: impl Into<String>,
        params: ParamSet,
    ) -> Result<QueryResultSet, SqlError> {
        match self.execute(SqlOperation::Query {
            sql: sql.into(),
            params,
        })? {
            QueryOutcome::Query(res) => Ok(res),
            _ => Err(SqlError::Backend(
                "unexpected outcome for query operation".to_owned(),
            )),
        }
    }

    /// Executes a `single` operation synchronously.
    pub fn single(
        &self,
        sql: impl Into<String>,
        params: ParamSet,
    ) -> Result<Option<SqlRow>, SqlError> {
        match self.execute(SqlOperation::Single {
            sql: sql.into(),
            params,
        })? {
            QueryOutcome::Single(row) => Ok(row),
            _ => Err(SqlError::Backend(
                "unexpected outcome for single operation".to_owned(),
            )),
        }
    }

    /// Executes a `scalar` operation synchronously.
    pub fn scalar(
        &self,
        sql: impl Into<String>,
        params: ParamSet,
    ) -> Result<Option<SqlValue>, SqlError> {
        match self.execute(SqlOperation::Scalar {
            sql: sql.into(),
            params,
        })? {
            QueryOutcome::Scalar(val) => Ok(val),
            _ => Err(SqlError::Backend(
                "unexpected outcome for scalar operation".to_owned(),
            )),
        }
    }

    /// Executes an `insert` operation synchronously, returning the last insert id.
    pub fn insert(&self, sql: impl Into<String>, params: ParamSet) -> Result<u64, SqlError> {
        match self.execute(SqlOperation::Insert {
            sql: sql.into(),
            params,
        })? {
            QueryOutcome::Insert(id) => Ok(id),
            _ => Err(SqlError::Backend(
                "unexpected outcome for insert operation".to_owned(),
            )),
        }
    }

    /// Executes an `update` operation synchronously, returning the affected row count.
    pub fn update(&self, sql: impl Into<String>, params: ParamSet) -> Result<u64, SqlError> {
        match self.execute(SqlOperation::Update {
            sql: sql.into(),
            params,
        })? {
            QueryOutcome::Update(affected) => Ok(affected),
            _ => Err(SqlError::Backend(
                "unexpected outcome for update operation".to_owned(),
            )),
        }
    }

    /// Executes a `prepare` operation synchronously.
    pub fn prepare(
        &self,
        sql: impl Into<String>,
        params: PrepareParams,
    ) -> Result<QueryOutcome, SqlError> {
        self.execute(SqlOperation::Prepare {
            sql: sql.into(),
            params,
        })
    }

    /// Executes a `rawExecute` operation synchronously.
    pub fn raw_execute(
        &self,
        sql: impl Into<String>,
        params: ParamSet,
    ) -> Result<QueryResultSet, SqlError> {
        match self.execute(SqlOperation::RawExecute {
            sql: sql.into(),
            params,
        })? {
            QueryOutcome::Query(res) => Ok(res),
            _ => Err(SqlError::Backend(
                "unexpected outcome for rawExecute operation".to_owned(),
            )),
        }
    }

    /// Executes an atomic `transaction` across `items` synchronously.
    pub fn transaction(&self, items: Vec<TransactionItem>) -> Result<bool, SqlError> {
        match self.execute(SqlOperation::Transaction { items })? {
            QueryOutcome::Transaction(ok) => Ok(ok),
            _ => Err(SqlError::Backend(
                "unexpected outcome for transaction operation".to_owned(),
            )),
        }
    }

    /// Gracefully closes connections and joins worker threads / async runtime.
    pub fn shutdown(&mut self) {
        match &mut self.inner {
            PoolInner::Mysql(pool) => pool.shutdown(),
            PoolInner::Sqlite(pool) => pool.shutdown(),
        }
    }
}
