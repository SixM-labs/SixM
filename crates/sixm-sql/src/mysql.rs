//! Async MySQL / MariaDB backend powered by `mysql_async` (`rustls` + `ring`).

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use mysql_async::prelude::{Protocol, Queryable};
use mysql_async::{
    Conn, Error as MysqlError, Opts, OptsBuilder, Pool, PoolConstraints, PoolOpts, QueryResult,
    SslOpts, TxOpts, Value as MysqlValue,
};
use tokio::runtime::{Builder, Runtime};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};
use tracing::{debug, warn};

use crate::config::{MysqlConfig, SslMode};
use crate::params::{BoundQuery, Dialect, ParsedSql};
use crate::{
    DmlSummary, LimitKind, PrepareParams, QueryOutcome, QueryResultSet, SqlError, SqlLimits,
    SqlOperation, SqlRow, SqlValue, TransactionItem,
};

type DoneCallback = Box<dyn FnOnce(Result<QueryOutcome, SqlError>) + Send>;

const ER_UNSUPPORTED_PS: u16 = 1295;
const STMT_CACHE_SIZE: usize = 64;

#[derive(Debug)]
pub(crate) struct MysqlPool {
    runtime: Option<Runtime>,
    pool: Pool,
    in_flight: Arc<Semaphore>,
    closed: Arc<AtomicBool>,
    limits: SqlLimits,
    connect_timeout: Duration,
    date_strings: bool,
}

impl MysqlPool {
    pub(crate) fn connect(cfg: &MysqlConfig, limits: SqlLimits) -> Result<Self, SqlError> {
        let worker_threads = limits.pool_size.clamp(1, 8);
        let runtime = Builder::new_multi_thread()
            .worker_threads(worker_threads)
            .thread_name("sixm-sql-mysql")
            .enable_all()
            .build()
            .map_err(|err| {
                SqlError::Backend(format!("failed to start MySQL async runtime: {err}"))
            })?;

        let constraints = PoolConstraints::new(1, limits.pool_size).ok_or_else(|| {
            SqlError::InvalidConfig(format!("invalid MySQL pool size `{}`", limits.pool_size))
        })?;
        let pool_opts = PoolOpts::default().with_constraints(constraints);

        let mut builder = OptsBuilder::default()
            .ip_or_hostname(cfg.host.clone())
            .tcp_port(cfg.port)
            .user(cfg.user.clone())
            .pass(cfg.password.clone())
            .db_name(cfg.database.clone())
            .prefer_socket(false)
            .stmt_cache_size(STMT_CACHE_SIZE)
            .pool_opts(pool_opts)
            .ssl_opts(build_ssl_opts(cfg.ssl_mode));

        if !cfg.charset.eq_ignore_ascii_case("utf8mb4") {
            builder = builder.init(vec![format!("SET NAMES {}", cfg.charset)]);
        }

        let pool = Pool::new(Opts::from(builder));

        Ok(Self {
            runtime: Some(runtime),
            pool,
            in_flight: Arc::new(Semaphore::new(limits.queue_capacity)),
            closed: Arc::new(AtomicBool::new(false)),
            limits,
            connect_timeout: cfg.connect_timeout.unwrap_or(limits.timeout),
            date_strings: cfg.date_strings,
        })
    }

    pub(crate) fn is_ready(&self) -> bool {
        !self.closed.load(Ordering::Acquire) && self.runtime.is_some()
    }

    pub(crate) fn ping(&self) -> Result<(), SqlError> {
        let (tx, rx) = mpsc::sync_channel(1);
        let Some(ref runtime) = self.runtime else {
            return Err(SqlError::PoolClosed);
        };
        if self.closed.load(Ordering::Acquire) {
            return Err(SqlError::PoolClosed);
        }
        let permit = Arc::clone(&self.in_flight)
            .try_acquire_owned()
            .map_err(|err| match err {
                TryAcquireError::NoPermits => SqlError::QueueFull(self.limits.queue_capacity),
                TryAcquireError::Closed => SqlError::PoolClosed,
            })?;
        let pool = self.pool.clone();
        let timeout = self.connect_timeout;
        runtime.spawn(async move {
            let _permit = permit;
            let res = tokio::time::timeout(timeout, async {
                let mut conn = pool.get_conn().await.map_err(|e| map_mysql_err(&e))?;
                conn.ping().await.map_err(|e| map_mysql_err(&e))?;
                Ok::<(), SqlError>(())
            })
            .await;
            let mapped = match res {
                Ok(inner) => inner,
                Err(_) => Err(SqlError::Timeout(timeout)),
            };
            let _ = tx.send(mapped);
        });
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
        let Some(ref runtime) = self.runtime else {
            return Err(SqlError::PoolClosed);
        };
        let permit = Arc::clone(&self.in_flight)
            .try_acquire_owned()
            .map_err(|err| match err {
                TryAcquireError::NoPermits => SqlError::QueueFull(self.limits.queue_capacity),
                TryAcquireError::Closed => SqlError::PoolClosed,
            })?;

        let pool = self.pool.clone();
        let limits = self.limits;
        let connect_timeout = self.connect_timeout;
        let date_strings = self.date_strings;

        runtime.spawn(async move {
            let _permit: OwnedSemaphorePermit = permit;
            if cancelled
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::Acquire))
            {
                on_done(Err(SqlError::Cancelled));
                return;
            }

            let started = Instant::now();
            let outcome =
                run_op_with_timeout(pool, op, limits, connect_timeout, date_strings, cancelled)
                    .await;
            let elapsed = started.elapsed();

            if let Some(warn_after) = limits.slow_query_warn
                && elapsed >= warn_after
            {
                warn!(
                    target: "sixm::sql",
                    elapsed_ms = elapsed.as_millis(),
                    backend = "mysql",
                    "slow SQL query"
                );
            }

            on_done(outcome);
        });

        Ok(())
    }

    pub(crate) fn shutdown(&mut self) {
        self.closed.store(true, Ordering::Release);
        self.in_flight.close();
        let Some(runtime) = self.runtime.take() else {
            return;
        };
        let pool = self.pool.clone();
        let handle = runtime.handle().clone();
        let (tx, rx) = mpsc::sync_channel(1);
        handle.spawn(async move {
            let _ = tokio::time::timeout(Duration::from_secs(2), pool.disconnect()).await;
            let _ = tx.send(());
        });
        let _ = rx.recv_timeout(Duration::from_secs(3));
        // Shut down runtime on a plain thread so callers inside an ambient tokio runtime never panic.
        let _ = thread::Builder::new()
            .name("sixm-sql-mysql-shutdown".to_owned())
            .spawn(move || runtime.shutdown_timeout(Duration::from_secs(2)))
            .map(thread::JoinHandle::join);
    }
}

impl Drop for MysqlPool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn build_ssl_opts(mode: SslMode) -> Option<SslOpts> {
    match mode {
        SslMode::Disabled => None,
        SslMode::Preferred => {
            // `mysql_async` does not support opportunistic plaintext fallback on TLS failure.
            debug!(
                target: "sixm::sql",
                "sslmode=preferred falls back to plaintext in mysql_async; use sslmode=required or verify_ca/verify_identity to enforce TLS"
            );
            None
        }
        SslMode::Required => Some(
            SslOpts::default()
                .with_danger_accept_invalid_certs(true)
                .with_danger_skip_domain_validation(true),
        ),
        SslMode::VerifyCa => Some(SslOpts::default().with_danger_skip_domain_validation(true)),
        SslMode::VerifyIdentity => Some(SslOpts::default()),
    }
}

async fn run_op_with_timeout(
    pool: Pool,
    op: SqlOperation,
    limits: SqlLimits,
    connect_timeout: Duration,
    date_strings: bool,
    cancelled: Option<Arc<AtomicBool>>,
) -> Result<QueryOutcome, SqlError> {
    let deadline = Instant::now() + limits.timeout;
    let remaining_connect = deadline
        .checked_duration_since(Instant::now())
        .unwrap_or(Duration::ZERO)
        .min(connect_timeout);

    if remaining_connect.is_zero() {
        return Err(SqlError::Timeout(limits.timeout));
    }

    let mut conn = match tokio::time::timeout(remaining_connect, pool.get_conn()).await {
        Ok(Ok(conn)) => conn,
        Ok(Err(err)) => return Err(map_mysql_err(&err)),
        Err(_) => return Err(SqlError::Timeout(limits.timeout)),
    };

    if cancelled
        .as_ref()
        .is_some_and(|flag| flag.load(Ordering::Acquire))
    {
        return Err(SqlError::Cancelled);
    }

    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
        return Err(SqlError::Timeout(limits.timeout));
    };

    match tokio::time::timeout(remaining, execute_op(&mut conn, op, limits, date_strings)).await {
        Ok(Ok(outcome)) => Ok(outcome),
        Ok(Err(err @ SqlError::ResultTooLarge { .. })) => {
            // Disconnect rather than draining an unbounded server result set or rolling back over an active stream.
            let _ = conn.disconnect().await;
            Err(err)
        }
        Ok(Err(err)) => Err(err),
        Err(_) => {
            let _ = conn.disconnect().await;
            Err(SqlError::Timeout(limits.timeout))
        }
    }
}

async fn execute_op(
    conn: &mut Conn,
    op: SqlOperation,
    limits: SqlLimits,
    date_strings: bool,
) -> Result<QueryOutcome, SqlError> {
    match op {
        SqlOperation::Query { sql, params } | SqlOperation::RawExecute { sql, params } => {
            let bound =
                ParsedSql::parse_for(&sql, Dialect::Mysql)?.bind(&params, Dialect::Mysql)?;
            let mut total_bytes = 0;
            let res =
                exec_bound(conn, &bound, limits, None, date_strings, &mut total_bytes).await?;
            Ok(QueryOutcome::Query(res))
        }
        SqlOperation::Single { sql, params } => {
            let bound =
                ParsedSql::parse_for(&sql, Dialect::Mysql)?.bind(&params, Dialect::Mysql)?;
            let mut total_bytes = 0;
            let res = exec_bound(
                conn,
                &bound,
                limits,
                Some(1),
                date_strings,
                &mut total_bytes,
            )
            .await?;
            let first = match res {
                QueryResultSet::Rows(mut rows) => rows.drain(..).next(),
                QueryResultSet::Dml(_) => None,
            };
            Ok(QueryOutcome::Single(first))
        }
        SqlOperation::Scalar { sql, params } => {
            let bound =
                ParsedSql::parse_for(&sql, Dialect::Mysql)?.bind(&params, Dialect::Mysql)?;
            let mut total_bytes = 0;
            let res = exec_bound(
                conn,
                &bound,
                limits,
                Some(1),
                date_strings,
                &mut total_bytes,
            )
            .await?;
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
                ParsedSql::parse_for(&sql, Dialect::Mysql)?.bind(&params, Dialect::Mysql)?;
            let mut total_bytes = 0;
            let res = exec_bound(
                conn,
                &bound,
                limits,
                Some(1),
                date_strings,
                &mut total_bytes,
            )
            .await?;
            let insert_id = match res {
                QueryResultSet::Dml(summary) => summary.insert_id,
                QueryResultSet::Rows(rows) => first_int_cell(&rows).unwrap_or(0),
            };
            Ok(QueryOutcome::Insert(insert_id))
        }
        SqlOperation::Update { sql, params } => {
            let bound =
                ParsedSql::parse_for(&sql, Dialect::Mysql)?.bind(&params, Dialect::Mysql)?;
            let mut total_bytes = 0;
            let res =
                exec_bound(conn, &bound, limits, None, date_strings, &mut total_bytes).await?;
            let affected = match res {
                QueryResultSet::Dml(summary) => summary.affected_rows,
                QueryResultSet::Rows(rows) => u64::try_from(rows.len()).unwrap_or(u64::MAX),
            };
            Ok(QueryOutcome::Update(affected))
        }
        SqlOperation::Prepare { sql, params } => {
            execute_prepare(conn, &sql, params, limits, date_strings).await
        }
        SqlOperation::Transaction { items } => {
            execute_transaction(conn, &items, limits, date_strings).await
        }
    }
}

async fn execute_prepare(
    conn: &mut Conn,
    sql: &str,
    params: PrepareParams,
    limits: SqlLimits,
    date_strings: bool,
) -> Result<QueryOutcome, SqlError> {
    let parsed = ParsedSql::parse_for(sql, Dialect::Mysql)?;
    let mut total_bytes = 0;
    match params {
        PrepareParams::Single(set) => {
            let bound = parsed.bind(&set, Dialect::Mysql)?;
            let res =
                exec_bound(conn, &bound, limits, None, date_strings, &mut total_bytes).await?;
            Ok(QueryOutcome::Query(res))
        }
        PrepareParams::Batch(sets) => {
            let mut results = Vec::with_capacity(sets.len());
            for set in sets {
                let bound = parsed.bind(&set, Dialect::Mysql)?;
                results.push(
                    exec_bound(conn, &bound, limits, None, date_strings, &mut total_bytes).await?,
                );
            }
            Ok(QueryOutcome::Batch(results))
        }
    }
}

async fn execute_transaction(
    conn: &mut Conn,
    items: &[TransactionItem],
    limits: SqlLimits,
    date_strings: bool,
) -> Result<QueryOutcome, SqlError> {
    let mut bound_items = Vec::with_capacity(items.len());
    for item in items {
        bound_items.push(
            ParsedSql::parse_for(&item.sql, Dialect::Mysql)?.bind(&item.params, Dialect::Mysql)?,
        );
    }

    let mut tx = conn
        .start_transaction(TxOpts::default())
        .await
        .map_err(|e| map_mysql_err(&e))?;

    let mut total_bytes = 0;
    for bound in &bound_items {
        if let Err(err) =
            exec_bound(&mut tx, bound, limits, None, date_strings, &mut total_bytes).await
        {
            if !matches!(err, SqlError::ResultTooLarge { .. }) {
                let _ = tx.rollback().await;
            }
            return Err(err);
        }
    }

    tx.commit().await.map_err(|e| map_mysql_err(&e))?;
    Ok(QueryOutcome::Transaction(true))
}

async fn exec_bound<Q: Queryable>(
    queryable: &mut Q,
    bound: &BoundQuery,
    limits: SqlLimits,
    row_cap_override: Option<usize>,
    date_strings: bool,
    total_bytes: &mut usize,
) -> Result<QueryResultSet, SqlError> {
    let mysql_params: Vec<MysqlValue> = bound.params.iter().map(sql_to_mysql).collect();
    let had_params = !mysql_params.is_empty();

    match queryable.exec_iter(&bound.sql, mysql_params).await {
        Ok(result) => {
            collect_query_result(result, limits, row_cap_override, date_strings, total_bytes).await
        }
        Err(MysqlError::Server(se)) if se.code == ER_UNSUPPORTED_PS && !had_params => {
            let result = queryable
                .query_iter(&bound.sql)
                .await
                .map_err(|e| map_mysql_err(&e))?;
            collect_query_result(result, limits, row_cap_override, date_strings, total_bytes).await
        }
        Err(err) => Err(map_mysql_err(&err)),
    }
}

async fn collect_query_result<P: Protocol>(
    mut result: QueryResult<'_, '_, P>,
    limits: SqlLimits,
    row_cap_override: Option<usize>,
    date_strings: bool,
    total_bytes: &mut usize,
) -> Result<QueryResultSet, SqlError> {
    if result.columns_ref().is_empty() {
        let affected_rows = result.affected_rows();
        let insert_id = result.last_insert_id().unwrap_or(0);
        let changed_rows = parse_changed_rows(&result.info(), affected_rows);
        result.drop_result().await.map_err(|e| map_mysql_err(&e))?;
        return Ok(QueryResultSet::Dml(DmlSummary {
            affected_rows,
            insert_id,
            changed_rows,
        }));
    }

    let mut rows = Vec::new();
    let effective_cap = row_cap_override.unwrap_or(limits.max_rows);

    while let Some(row) = result.next().await.map_err(|e| map_mysql_err(&e))? {
        if row_cap_override.is_some() && rows.len() >= effective_cap {
            break;
        }
        if rows.len() >= limits.max_rows {
            return Err(SqlError::ResultTooLarge {
                kind: LimitKind::Rows,
                limit: limits.max_rows,
            });
        }

        let col_names: Vec<String> = row
            .columns_ref()
            .iter()
            .map(|col| col.name_str().into_owned())
            .collect();
        let values = row.unwrap();
        let mut columns = Vec::with_capacity(col_names.len());
        for (name, val) in col_names.into_iter().zip(values) {
            let sql_val = mysql_to_sql(val, date_strings);
            *total_bytes = total_bytes
                .saturating_add(name.len())
                .saturating_add(sql_val.encoded_len());
            if *total_bytes > limits.max_result_bytes {
                return Err(SqlError::ResultTooLarge {
                    kind: LimitKind::Bytes,
                    limit: limits.max_result_bytes,
                });
            }
            columns.push((name, sql_val));
        }
        rows.push(SqlRow { columns });
        if row_cap_override.is_some() && rows.len() >= effective_cap {
            break;
        }
    }

    result.drop_result().await.map_err(|e| map_mysql_err(&e))?;
    Ok(QueryResultSet::Rows(rows))
}

fn parse_changed_rows(info: &str, fallback: u64) -> u64 {
    let Some((_, after)) = info.split_once("Changed:") else {
        return fallback;
    };
    after
        .split_whitespace()
        .next()
        .and_then(|tok| tok.parse::<u64>().ok())
        .unwrap_or(fallback)
}

fn first_int_cell(rows: &[SqlRow]) -> Option<u64> {
    let (_, val) = rows.first()?.columns.first()?;
    match val {
        SqlValue::Int(i) => u64::try_from(*i).ok(),
        SqlValue::UInt(u) => Some(*u),
        _ => None,
    }
}

fn sql_to_mysql(value: &SqlValue) -> MysqlValue {
    match value {
        SqlValue::Null => MysqlValue::NULL,
        SqlValue::Bool(b) => MysqlValue::Int(i64::from(*b)),
        SqlValue::Int(i) => MysqlValue::Int(*i),
        SqlValue::UInt(u) => MysqlValue::UInt(*u),
        SqlValue::Float(f) => MysqlValue::Double(*f),
        SqlValue::Text(s) => MysqlValue::Bytes(s.as_bytes().to_vec()),
        SqlValue::Bytes(b) => MysqlValue::Bytes(b.clone()),
    }
}

fn days_from_civil(year: i64, month: u8, day: u8) -> i64 {
    let m = i64::from(month);
    let y = year - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn mysql_to_sql(value: MysqlValue, date_strings: bool) -> SqlValue {
    match value {
        MysqlValue::NULL => SqlValue::Null,
        MysqlValue::Int(i) => SqlValue::Int(i),
        MysqlValue::UInt(u) => SqlValue::UInt(u),
        MysqlValue::Float(f) => SqlValue::Float(f64::from(f)),
        MysqlValue::Double(d) => SqlValue::Float(d),
        MysqlValue::Bytes(bytes) => match String::from_utf8(bytes) {
            Ok(text) => SqlValue::Text(text),
            Err(err) => SqlValue::Bytes(err.into_bytes()),
        },
        MysqlValue::Date(year, month, day, hour, min, sec, usec) => {
            if !date_strings && (1..=12).contains(&month) && (1..=31).contains(&day) {
                let days = days_from_civil(i64::from(year), month, day);
                let ms = days
                    .saturating_mul(86_400_000)
                    .saturating_add(i64::from(hour) * 3_600_000)
                    .saturating_add(i64::from(min) * 60_000)
                    .saturating_add(i64::from(sec) * 1_000)
                    .saturating_add(i64::from(usec / 1000));
                return SqlValue::Int(ms);
            }
            let mut out = String::with_capacity(26);
            if hour == 0 && min == 0 && sec == 0 && usec == 0 {
                let _ = write!(out, "{year:04}-{month:02}-{day:02}");
            } else if usec == 0 {
                let _ = write!(
                    out,
                    "{year:04}-{month:02}-{day:02} {hour:02}:{min:02}:{sec:02}"
                );
            } else {
                let _ = write!(
                    out,
                    "{year:04}-{month:02}-{day:02} {hour:02}:{min:02}:{sec:02}.{usec:06}"
                );
            }
            SqlValue::Text(out)
        }
        MysqlValue::Time(neg, days, hours, mins, secs, usec) => {
            let total_hours = days.saturating_mul(24).saturating_add(u32::from(hours));
            let sign = if neg { "-" } else { "" };
            let mut out = String::with_capacity(20);
            if usec == 0 {
                let _ = write!(out, "{sign}{total_hours:02}:{mins:02}:{secs:02}");
            } else {
                let _ = write!(out, "{sign}{total_hours:02}:{mins:02}:{secs:02}.{usec:06}");
            }
            SqlValue::Text(out)
        }
    }
}

fn map_mysql_err(err: &MysqlError) -> SqlError {
    SqlError::Backend(err.to_string())
}
