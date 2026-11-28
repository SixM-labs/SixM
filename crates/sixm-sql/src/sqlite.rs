//! Dedicated thread-pool SQLite backend using bundled `rusqlite`.

use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use rusqlite::types::ValueRef;
use rusqlite::{Connection, ErrorCode, params_from_iter};
use tracing::warn;

use crate::config::{SqliteConfig, SqliteTarget};
use crate::params::{BoundQuery, Dialect, ParsedSql};
use crate::{
    DmlSummary, LimitKind, ParamSet, PrepareParams, QueryOutcome, QueryResultSet, SqlError,
    SqlLimits, SqlOperation, SqlRow, SqlValue, TransactionItem,
};

type DoneCallback = Box<dyn FnOnce(Result<QueryOutcome, SqlError>) + Send>;

struct SqliteJob {
    op: SqlOperation,
    cancelled: Option<Arc<AtomicBool>>,
    on_done: DoneCallback,
}

#[derive(Debug)]
pub(crate) struct SqlitePool {
    sender: Option<SyncSender<SqliteJob>>,
    workers: Vec<JoinHandle<()>>,
    closed: Arc<AtomicBool>,
    limits: SqlLimits,
}

impl SqlitePool {
    pub(crate) fn connect(cfg: &SqliteConfig, limits: SqlLimits) -> Result<Self, SqlError> {
        let is_memory = matches!(cfg.target, SqliteTarget::Memory);
        if let SqliteTarget::File(ref path) = cfg.target
            && let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|err| {
                SqlError::Backend(format!(
                    "failed to create SQLite directory `{}`: {err}",
                    parent.display()
                ))
            })?;
        }

        // In-memory SQLite connections are isolated per Connection; keep a single
        // worker for `:memory:` so every query observes the same database.
        let worker_count = if is_memory { 1 } else { limits.pool_size };
        let (sender, receiver) = mpsc::sync_channel::<SqliteJob>(limits.queue_capacity);
        let receiver = Arc::new(Mutex::new(receiver));
        let mut workers = Vec::with_capacity(worker_count);

        for idx in 0..worker_count {
            let target = cfg.target.clone();
            let receiver = Arc::clone(&receiver);
            let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), SqlError>>(1);
            let handle = thread::Builder::new()
                .name(format!("sixm-sql-sqlite-{idx}"))
                .spawn(move || worker_loop(&target, limits, &init_tx, &receiver))
                .map_err(|err| {
                    SqlError::Backend(format!("failed to spawn SQLite worker thread: {err}"))
                })?;
            workers.push(handle);

            match init_rx.recv() {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    drop(sender);
                    for worker in workers {
                        let _ = worker.join();
                    }
                    return Err(err);
                }
                Err(_) => {
                    drop(sender);
                    for worker in workers {
                        let _ = worker.join();
                    }
                    return Err(SqlError::Backend(
                        "SQLite worker exited during initialization".to_owned(),
                    ));
                }
            }
        }

        Ok(Self {
            sender: Some(sender),
            workers,
            closed: Arc::new(AtomicBool::new(false)),
            limits,
        })
    }

    pub(crate) fn is_ready(&self) -> bool {
        !self.closed.load(Ordering::Acquire) && self.sender.is_some()
    }

    pub(crate) fn ping(&self) -> Result<(), SqlError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.execute_async(
            SqlOperation::Scalar {
                sql: "SELECT 1".to_owned(),
                params: ParamSet::Empty,
            },
            None,
            Box::new(move |res| {
                let _ = tx.send(res.map(|_| ()));
            }),
        )?;
        rx.recv().map_err(|_| SqlError::PoolClosed)?
    }

    pub(crate) fn execute_async(
        &self,
        op: SqlOperation,
        cancelled: Option<Arc<AtomicBool>>,
        on_done: DoneCallback,
    ) -> Result<(), SqlError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(SqlError::PoolClosed);
        }
        let Some(ref sender) = self.sender else {
            return Err(SqlError::PoolClosed);
        };
        sender
            .try_send(SqliteJob {
                op,
                cancelled,
                on_done,
            })
            .map_err(|err| match err {
                TrySendError::Full(_) => SqlError::QueueFull(self.limits.queue_capacity),
                TrySendError::Disconnected(_) => SqlError::PoolClosed,
            })
    }

    pub(crate) fn shutdown(&mut self) {
        self.closed.store(true, Ordering::Release);
        drop(self.sender.take());
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

impl Drop for SqlitePool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn open_connection(target: &SqliteTarget, limits: SqlLimits) -> Result<Connection, SqlError> {
    let conn = match target {
        SqliteTarget::Memory => Connection::open_in_memory(),
        SqliteTarget::File(path) => Connection::open(path),
    }
    .map_err(|err| SqlError::Backend(format!("SQLite open failed: {err}")))?;

    conn.busy_timeout(limits.timeout)
        .map_err(|err| SqlError::Backend(format!("SQLite busy_timeout failed: {err}")))?;

    let pragmas = match target {
        SqliteTarget::Memory => "PRAGMA foreign_keys = ON;",
        SqliteTarget::File(_) => {
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;"
        }
    };
    conn.execute_batch(pragmas)
        .map_err(|err| SqlError::Backend(format!("SQLite PRAGMA setup failed: {err}")))?;

    Ok(conn)
}

fn next_job(receiver: &Mutex<Receiver<SqliteJob>>) -> Option<SqliteJob> {
    let guard = receiver.lock().unwrap_or_else(PoisonError::into_inner);
    guard.recv().ok()
}

fn worker_loop(
    target: &SqliteTarget,
    limits: SqlLimits,
    init_tx: &SyncSender<Result<(), SqlError>>,
    receiver: &Mutex<Receiver<SqliteJob>>,
) {
    let mut conn = match open_connection(target, limits) {
        Ok(conn) => {
            let _ = init_tx.send(Ok(()));
            conn
        }
        Err(err) => {
            let _ = init_tx.send(Err(err));
            return;
        }
    };

    while let Some(job) = next_job(receiver) {
        if job
            .cancelled
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            (job.on_done)(Err(SqlError::Cancelled));
            continue;
        }

        let started = Instant::now();
        let outcome = run_with_deadline(&mut conn, job.op, limits, job.cancelled.as_ref());
        let elapsed = started.elapsed();

        if let Some(warn_after) = limits.slow_query_warn
            && elapsed >= warn_after
        {
            warn!(
                target: "sixm::sql",
                elapsed_ms = elapsed.as_millis(),
                backend = "sqlite",
                "slow SQL query"
            );
        }

        (job.on_done)(outcome);
    }
}

fn run_with_deadline(
    conn: &mut Connection,
    op: SqlOperation,
    limits: SqlLimits,
    cancelled: Option<&Arc<AtomicBool>>,
) -> Result<QueryOutcome, SqlError> {
    let deadline = Instant::now() + limits.timeout;
    let interrupted = Arc::new(AtomicBool::new(false));
    let interrupted_hook = Arc::clone(&interrupted);
    let cancel_hook = cancelled.cloned();

    conn.progress_handler(
        1000,
        Some(move || {
            let expired = Instant::now() >= deadline;
            let is_cancelled = cancel_hook
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::Acquire));
            if expired || is_cancelled {
                interrupted_hook.store(true, Ordering::Release);
                true
            } else {
                false
            }
        }),
    )
    .map_err(|e| map_sqlite_err(&e))?;

    let result = execute_op(conn, op, limits);
    let _ = conn.progress_handler(0, None::<fn() -> bool>);

    match result {
        Err(err) if interrupted.load(Ordering::Acquire) => {
            if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                Err(SqlError::Cancelled)
            } else if matches!(err, SqlError::Backend(_)) {
                Err(SqlError::Timeout(limits.timeout))
            } else {
                Err(err)
            }
        }
        other => other,
    }
}

fn execute_op(
    conn: &mut Connection,
    op: SqlOperation,
    limits: SqlLimits,
) -> Result<QueryOutcome, SqlError> {
    match op {
        SqlOperation::Query { sql, params } | SqlOperation::RawExecute { sql, params } => {
            let bound =
                ParsedSql::parse_for(&sql, Dialect::Sqlite)?.bind(&params, Dialect::Sqlite)?;
            let mut total_bytes = 0;
            let res = execute_bound(conn, &bound, limits, None, &mut total_bytes)?;
            Ok(QueryOutcome::Query(res))
        }
        SqlOperation::Single { sql, params } => {
            let bound =
                ParsedSql::parse_for(&sql, Dialect::Sqlite)?.bind(&params, Dialect::Sqlite)?;
            let mut total_bytes = 0;
            let res = execute_bound(conn, &bound, limits, Some(1), &mut total_bytes)?;
            let first = match res {
                QueryResultSet::Rows(mut rows) => rows.drain(..).next(),
                QueryResultSet::Dml(_) => None,
            };
            Ok(QueryOutcome::Single(first))
        }
        SqlOperation::Scalar { sql, params } => {
            let bound =
                ParsedSql::parse_for(&sql, Dialect::Sqlite)?.bind(&params, Dialect::Sqlite)?;
            let mut total_bytes = 0;
            let res = execute_bound(conn, &bound, limits, Some(1), &mut total_bytes)?;
            let scalar = match res {
                QueryResultSet::Rows(mut rows) => rows
                    .drain(..)
                    .next()
                    .and_then(|mut row| row.columns.drain(..).next().map(|(_, val)| val)),
                QueryResultSet::Dml(_) => None,
            };
            Ok(QueryOutcome::Scalar(scalar))
        }
        SqlOperation::Insert { sql, params } => {
            let bound =
                ParsedSql::parse_for(&sql, Dialect::Sqlite)?.bind(&params, Dialect::Sqlite)?;
            let mut total_bytes = 0;
            let res = execute_bound(conn, &bound, limits, Some(1), &mut total_bytes)?;
            let insert_id = match res {
                QueryResultSet::Dml(summary) => summary.insert_id,
                QueryResultSet::Rows(rows) => first_int_cell(&rows)
                    .unwrap_or_else(|| u64::try_from(conn.last_insert_rowid()).unwrap_or(0)),
            };
            Ok(QueryOutcome::Insert(insert_id))
        }
        SqlOperation::Update { sql, params } => {
            let bound =
                ParsedSql::parse_for(&sql, Dialect::Sqlite)?.bind(&params, Dialect::Sqlite)?;
            let mut total_bytes = 0;
            let res = execute_bound(conn, &bound, limits, None, &mut total_bytes)?;
            let affected = match res {
                QueryResultSet::Dml(summary) => summary.affected_rows,
                QueryResultSet::Rows(rows) => u64::try_from(rows.len()).unwrap_or(u64::MAX),
            };
            Ok(QueryOutcome::Update(affected))
        }
        SqlOperation::Prepare { sql, params } => execute_prepare(conn, &sql, params, limits),
        SqlOperation::Transaction { items } => execute_transaction(conn, &items, limits),
    }
}

fn execute_prepare(
    conn: &mut Connection,
    sql: &str,
    params: PrepareParams,
    limits: SqlLimits,
) -> Result<QueryOutcome, SqlError> {
    let parsed = ParsedSql::parse_for(sql, Dialect::Sqlite)?;
    let mut total_bytes = 0;
    match params {
        PrepareParams::Single(set) => {
            let bound = parsed.bind(&set, Dialect::Sqlite)?;
            let res = execute_bound(conn, &bound, limits, None, &mut total_bytes)?;
            Ok(QueryOutcome::Query(res))
        }
        PrepareParams::Batch(sets) => {
            let mut results = Vec::with_capacity(sets.len());
            for set in sets {
                let bound = parsed.bind(&set, Dialect::Sqlite)?;
                results.push(execute_bound(conn, &bound, limits, None, &mut total_bytes)?);
            }
            Ok(QueryOutcome::Batch(results))
        }
    }
}

fn execute_transaction(
    conn: &mut Connection,
    items: &[TransactionItem],
    limits: SqlLimits,
) -> Result<QueryOutcome, SqlError> {
    let mut bound_items = Vec::with_capacity(items.len());
    for item in items {
        bound_items.push(
            ParsedSql::parse_for(&item.sql, Dialect::Sqlite)?
                .bind(&item.params, Dialect::Sqlite)?,
        );
    }

    conn.execute_batch("BEGIN IMMEDIATE")
        .map_err(|e| map_sqlite_err(&e))?;

    let mut total_bytes = 0;
    for bound in &bound_items {
        if let Err(err) = execute_bound(conn, bound, limits, None, &mut total_bytes) {
            // Disarm any deadline/cancel progress hook before rolling back so
            // ROLLBACK itself cannot be interrupted.
            let _ = conn.progress_handler(0, None::<fn() -> bool>);
            let _ = conn.execute_batch("ROLLBACK");
            return Err(err);
        }
    }

    if let Err(err) = conn.execute_batch("COMMIT") {
        let _ = conn.progress_handler(0, None::<fn() -> bool>);
        let _ = conn.execute_batch("ROLLBACK");
        return Err(map_sqlite_err(&err));
    }

    Ok(QueryOutcome::Transaction(true))
}

fn execute_bound(
    conn: &Connection,
    bound: &BoundQuery,
    limits: SqlLimits,
    row_cap_override: Option<usize>,
    total_bytes: &mut usize,
) -> Result<QueryResultSet, SqlError> {
    let mut sqlite_params = Vec::with_capacity(bound.params.len());
    for value in &bound.params {
        sqlite_params.push(sql_to_sqlite(value)?);
    }

    let mut stmt = conn
        .prepare_cached(&bound.sql)
        .map_err(|e| map_sqlite_err(&e))?;
    let col_count = stmt.column_count();

    if col_count == 0 {
        let affected = stmt
            .execute(params_from_iter(sqlite_params))
            .map_err(|e| map_sqlite_err(&e))?;
        let affected_u64 = u64::try_from(affected).unwrap_or(u64::MAX);
        let insert_id = u64::try_from(conn.last_insert_rowid()).unwrap_or(0);
        return Ok(QueryResultSet::Dml(DmlSummary {
            affected_rows: affected_u64,
            insert_id,
            changed_rows: affected_u64,
        }));
    }

    let col_names: Vec<String> = (0..col_count)
        .map(|idx| stmt.column_name(idx).unwrap_or("").to_owned())
        .collect();

    let mut rows = stmt
        .query(params_from_iter(sqlite_params))
        .map_err(|e| map_sqlite_err(&e))?;
    let mut out = Vec::new();
    let effective_cap = row_cap_override.unwrap_or(limits.max_rows);

    while let Some(row) = rows.next().map_err(|e| map_sqlite_err(&e))? {
        if row_cap_override.is_some() && out.len() >= effective_cap {
            break;
        }
        if out.len() >= limits.max_rows {
            return Err(SqlError::ResultTooLarge {
                kind: LimitKind::Rows,
                limit: limits.max_rows,
            });
        }

        let mut columns = Vec::with_capacity(col_count);
        for (idx, name) in col_names.iter().enumerate() {
            let val_ref = row.get_ref(idx).map_err(|e| map_sqlite_err(&e))?;
            let val = sqlite_ref_to_sql(val_ref);
            *total_bytes = total_bytes
                .saturating_add(name.len())
                .saturating_add(val.encoded_len());
            if *total_bytes > limits.max_result_bytes {
                return Err(SqlError::ResultTooLarge {
                    kind: LimitKind::Bytes,
                    limit: limits.max_result_bytes,
                });
            }
            columns.push((name.clone(), val));
        }
        out.push(SqlRow { columns });
        if row_cap_override.is_some() && out.len() >= effective_cap {
            break;
        }
    }

    Ok(QueryResultSet::Rows(out))
}

fn first_int_cell(rows: &[SqlRow]) -> Option<u64> {
    let (_, val) = rows.first()?.columns.first()?;
    match val {
        SqlValue::Int(i) => u64::try_from(*i).ok(),
        SqlValue::UInt(u) => Some(*u),
        _ => None,
    }
}

fn sql_to_sqlite(value: &SqlValue) -> Result<rusqlite::types::Value, SqlError> {
    match value {
        SqlValue::Null => Ok(rusqlite::types::Value::Null),
        SqlValue::Bool(b) => Ok(rusqlite::types::Value::Integer(i64::from(*b))),
        SqlValue::Int(i) => Ok(rusqlite::types::Value::Integer(*i)),
        SqlValue::UInt(u) => i64::try_from(*u)
            .map(rusqlite::types::Value::Integer)
            .map_err(|_| {
                SqlError::InvalidParam(format!(
                    "unsigned integer `{u}` exceeds SQLite signed 64-bit range"
                ))
            }),
        SqlValue::Float(f) => Ok(rusqlite::types::Value::Real(*f)),
        SqlValue::Text(s) => Ok(rusqlite::types::Value::Text(s.clone())),
        SqlValue::Bytes(b) => Ok(rusqlite::types::Value::Blob(b.clone())),
    }
}

fn sqlite_ref_to_sql(value: ValueRef<'_>) -> SqlValue {
    match value {
        ValueRef::Null => SqlValue::Null,
        ValueRef::Integer(i) => SqlValue::Int(i),
        ValueRef::Real(f) => SqlValue::Float(f),
        ValueRef::Text(bytes) => match std::str::from_utf8(bytes) {
            Ok(s) => SqlValue::Text(s.to_owned()),
            Err(_) => SqlValue::Bytes(bytes.to_vec()),
        },
        ValueRef::Blob(bytes) => SqlValue::Bytes(bytes.to_vec()),
    }
}

fn map_sqlite_err(err: &rusqlite::Error) -> SqlError {
    if let rusqlite::Error::SqliteFailure(ffi_err, _) = err
        && ffi_err.code == ErrorCode::OperationInterrupted
    {
        return SqlError::Backend("SQLite operation interrupted".to_owned());
    }
    SqlError::Backend(err.to_string())
}
