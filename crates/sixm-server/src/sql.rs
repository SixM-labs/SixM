//! Background database pool lifecycle and instance-scoped script callbacks.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use sixm_core::{FuncRef, ResourceName, ScriptError, SqlQueryKind, Value};
use sixm_sql::{
    DEFAULT_QUEUE_CAPACITY, DatabaseConfig, ParamSet, PrepareParams, QueryOutcome, QueryResultSet,
    SqlError, SqlLimits, SqlOperation, SqlPool, TransactionItem,
};
use tracing::warn;

const MAX_IN_FLIGHT: usize = 256;
const RETRY_DELAY: Duration = Duration::from_secs(5);

/// Requests stay in script value form until the worker validates and binds them.
pub(crate) enum Request {
    Execute(SqlQueryKind, String, Value),
    Prepare(String, Value),
    Transaction(Value, Value),
}

impl Request {
    fn into_operation(self) -> Result<(SqlOperation, bool), SqlError> {
        match self {
            Self::Execute(kind, sql, params) => {
                let bind = || ParamSet::from_core_value(&params);
                let operation = match kind {
                    SqlQueryKind::Query => SqlOperation::Query {
                        sql,
                        params: bind()?,
                    },
                    SqlQueryKind::Single => SqlOperation::Single {
                        sql,
                        params: bind()?,
                    },
                    SqlQueryKind::Scalar => SqlOperation::Scalar {
                        sql,
                        params: bind()?,
                    },
                    SqlQueryKind::Insert => SqlOperation::Insert {
                        sql,
                        params: bind()?,
                    },
                    SqlQueryKind::Update => SqlOperation::Update {
                        sql,
                        params: bind()?,
                    },
                    SqlQueryKind::RawExecute => prepared(sql, &params)?,
                };
                Ok((operation, false))
            }
            Self::Prepare(sql, params) => Ok((prepared(sql, &params)?, true)),
            Self::Transaction(queries, params) => {
                let items = transaction_items(&queries, &params)?;
                Ok((SqlOperation::Transaction { items }, false))
            }
        }
    }
}

fn prepared(sql: String, params: &Value) -> Result<SqlOperation, SqlError> {
    let params = PrepareParams::from_core_value(params)?;
    let sets = match &params {
        PrepareParams::Single(set) => std::slice::from_ref(set),
        PrepareParams::Batch(sets) => sets,
    };
    for set in sets {
        if matches!(set, ParamSet::Named(map) if !map.is_empty()) {
            return Err(SqlError::InvalidParam(
                "prepare and rawExecute require positional parameters".to_owned(),
            ));
        }
    }
    Ok(SqlOperation::Prepare { sql, params })
}

fn transaction_items(queries: &Value, params: &Value) -> Result<Vec<TransactionItem>, SqlError> {
    let Value::Array(queries) = queries else {
        return Err(SqlError::InvalidParam(
            "a transaction expects an array of SQL strings or statements".to_owned(),
        ));
    };
    let shared = ParamSet::from_core_value(params)?;
    queries
        .iter()
        .map(|entry| {
            let (query, params) = match entry {
                Value::String(_) => (Some(entry), None),
                Value::Array(pair) if (1..=2).contains(&pair.len()) => (pair.first(), pair.get(1)),
                Value::Map(fields) => {
                    let get = |name| {
                        fields
                            .iter()
                            .find(|(key, _)| key.as_str() == Some(name))
                            .map(|(_, value)| value)
                    };
                    (get("query"), get("values").or_else(|| get("parameters")))
                }
                _ => (None, None),
            };
            let sql = query
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    SqlError::InvalidParam("a transaction statement needs a SQL string".to_owned())
                })?
                .to_owned();
            let params = match params {
                None | Some(Value::Nil) => shared.clone(),
                Some(params) => ParamSet::from_core_value(params)?,
            };
            Ok(TransactionItem { sql, params })
        })
        .collect()
}

fn prepared_result(result: QueryResultSet) -> Value {
    match result {
        QueryResultSet::Rows(mut rows) if rows.len() == 1 => {
            let row = rows.remove(0);
            if row.columns.len() == 1 {
                let (_, value) = row.columns.into_iter().next().expect("one column");
                return Value::from(value);
            }
            row.into_core_value()
        }
        other => other.into_core_value(),
    }
}

fn into_result(outcome: QueryOutcome, collapse_rows: bool) -> Value {
    if collapse_rows {
        return match outcome {
            QueryOutcome::Query(result) => prepared_result(result),
            QueryOutcome::Batch(results) => {
                Value::Array(results.into_iter().map(prepared_result).collect())
            }
            other => other.into_core_value(),
        };
    }
    outcome.into_core_value()
}

#[derive(Clone)]
struct Configuration {
    connection: Arc<str>,
    limits: SqlLimits,
    generation: u64,
}

impl Default for Configuration {
    fn default() -> Self {
        Self {
            connection: Arc::from(""),
            limits: SqlLimits::default(),
            generation: 1,
        }
    }
}

/// Tracks callback ownership separately from background work. A canceled job still
/// uses one global slot until the worker finishes it, even after its callback is freed.
struct Pending {
    origin: ResourceName,
    callback: Option<FuncRef>,
    cancelled: Arc<AtomicBool>,
}

#[derive(Default)]
struct ServiceState {
    config: Configuration,
    worker: Option<Worker>,
    last_id: u64,
    pending: HashMap<u64, Pending>,
    per_resource: HashMap<ResourceName, usize>,
}

fn reduce_in_flight(resources: &mut HashMap<ResourceName, usize>, origin: &str) {
    if let Some(count) = resources.get_mut(origin) {
        *count -= 1;
        if *count == 0 {
            resources.remove(origin);
        }
    }
}

/// Main-thread facade. Only the supervisor thread constructs, pings or drops a pool.
#[derive(Default)]
pub(crate) struct SqlService(RefCell<ServiceState>);

impl fmt::Debug for SqlService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The configuration contains the database credentials.
        formatter.debug_struct("SqlService").finish_non_exhaustive()
    }
}

impl SqlService {
    pub(crate) fn set_connection(&self, connection: &str) {
        let mut state = self.0.borrow_mut();
        let connection = connection.trim();
        if state.config.connection.as_ref() != connection {
            state.config.connection = connection.into();
            Self::reconfigure(&mut state);
        }
    }

    pub(crate) fn set_limits(&self, limits: SqlLimits) {
        let mut state = self.0.borrow_mut();
        if state.config.limits != limits {
            state.config.limits = limits;
            Self::reconfigure(&mut state);
        }
    }

    fn reconfigure(state: &mut ServiceState) {
        state.config.generation += 1;
        Self::cancel_pending(state);
        if let Some(worker) = &state.worker {
            worker.configure(state.config.clone());
        }
    }

    fn worker(state: &mut ServiceState) -> Result<&Worker, ScriptError> {
        if state.config.connection.is_empty() {
            return Err(ScriptError::Runtime("SQL is not configured".to_owned()));
        }
        if state.worker.is_none() {
            let worker = Worker::new().map_err(|_| {
                ScriptError::Runtime("cannot start the SQL supervisor thread".to_owned())
            })?;
            worker.configure(state.config.clone());
            state.worker = Some(worker);
        }
        Ok(state.worker.as_ref().expect("SQL worker initialized"))
    }

    pub(crate) fn is_ready(&self) -> bool {
        let mut state = self.0.borrow_mut();
        let generation = state.config.generation;
        Self::worker(&mut state).is_ok_and(|worker| {
            !worker.thread.is_finished() && worker.ready.load(Ordering::Acquire) == generation
        })
    }

    pub(crate) fn start(
        &self,
        origin: &ResourceName,
        callback: FuncRef,
        request: Request,
    ) -> Result<(), ScriptError> {
        let mut state = self.0.borrow_mut();
        if state.pending.len() >= DEFAULT_QUEUE_CAPACITY {
            return Err(ScriptError::Runtime("SQL job queue is full".to_owned()));
        }
        if state.per_resource.get(origin).copied().unwrap_or(0) >= MAX_IN_FLIGHT {
            return Err(ScriptError::Runtime(format!(
                "a resource can have at most {MAX_IN_FLIGHT} SQL jobs in flight"
            )));
        }
        let generation = state.config.generation;
        let id = state.last_id + 1;
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker = Self::worker(&mut state)?;
        let completion = Arc::new(Reply {
            tx: worker.results.clone(),
            id,
            sent: AtomicBool::new(false),
        });
        let job = Job {
            generation,
            request,
            cancelled: Arc::clone(&cancelled),
            completion: Arc::clone(&completion),
        };
        if worker.sender.send(Message::Execute(job)).is_err() {
            completion.sent.store(true, Ordering::Release);
            return Err(ScriptError::Runtime("SQL worker is unavailable".to_owned()));
        }
        state.last_id = id;
        state.pending.insert(
            id,
            Pending {
                origin: origin.clone(),
                callback: Some(callback),
                cancelled,
            },
        );
        *state.per_resource.entry(origin.clone()).or_default() += 1;
        Ok(())
    }

    /// Cancel pending jobs of a stopped resource, including jobs forwarded by a
    /// provider whose callback belongs to the provider itself.
    pub(crate) fn cancel_resource(&self, resource: &str) -> Vec<FuncRef> {
        let mut state = self.0.borrow_mut();
        let mut callbacks = Vec::new();
        let ServiceState {
            pending,
            per_resource,
            ..
        } = &mut *state;
        for job in pending.values_mut() {
            if job.callback.as_ref().is_some_and(|callback| {
                job.origin.as_ref() == resource || callback.owner.as_ref() == resource
            }) {
                job.cancelled.store(true, Ordering::Release);
                if let Some(callback) = job.callback.take() {
                    callbacks.push(callback);
                }
                reduce_in_flight(per_resource, &job.origin);
            }
        }
        callbacks
    }

    /// Drain once per frame, before entering any runtimes. An idle drain allocates nothing.
    pub(crate) fn take_finished(&self) -> Vec<Completion> {
        self.0
            .borrow()
            .worker
            .as_ref()
            .map_or_else(Vec::new, |worker| worker.finished.try_iter().collect())
    }

    /// Take ownership of one callback just before calling it. An earlier
    /// callback in the same frame can synchronously stop another resource.
    pub(crate) fn complete(&self, completion: Completion) -> Option<(FuncRef, [Value; 2])> {
        let mut state = self.0.borrow_mut();
        let job = state.pending.remove(&completion.id)?;
        let callback = job.callback?;
        reduce_in_flight(&mut state.per_resource, &job.origin);
        let args = match completion.outcome {
            Ok(value) => [Value::Nil, value],
            Err(error) => [Value::from(error), Value::Nil],
        };
        Some((callback, args))
    }

    /// Joins only during explicit server shutdown, outside the frame loop.
    pub(crate) fn shutdown(&self) {
        let worker = {
            let mut state = self.0.borrow_mut();
            Self::cancel_pending(&state);
            state.pending.clear();
            state.per_resource.clear();
            state.worker.take()
        };
        if let Some(worker) = worker {
            drop(worker.sender);
            let _ = worker.thread.join();
        }
    }

    fn cancel_pending(state: &ServiceState) {
        for job in state.pending.values() {
            job.cancelled.store(true, Ordering::Release);
        }
    }
}

impl Drop for SqlService {
    fn drop(&mut self) {
        Self::cancel_pending(self.0.get_mut());
    }
}

struct Job {
    generation: u64,
    request: Request,
    cancelled: Arc<AtomicBool>,
    completion: Arc<Reply>,
}

pub(crate) struct Completion {
    id: u64,
    outcome: Result<Value, String>,
}

/// Also reports a task dropped by a pool during reconfiguration, which otherwise
/// leaves the main thread holding a job slot and a pinned callback forever.
struct Reply {
    tx: Sender<Completion>,
    id: u64,
    sent: AtomicBool,
}

impl Reply {
    fn send(&self, outcome: Result<Value, String>) {
        if !self.sent.swap(true, Ordering::AcqRel) {
            let _ = self.tx.send(Completion {
                id: self.id,
                outcome,
            });
        }
    }
}

impl Drop for Reply {
    fn drop(&mut self) {
        if !self.sent.load(Ordering::Acquire) {
            self.send(Err(
                "SQL worker closed before the operation finished".to_owned()
            ));
        }
    }
}

enum Message {
    Configure,
    Execute(Job),
}

struct Worker {
    sender: Sender<Message>,
    settings: Arc<Mutex<Option<Configuration>>>,
    ready: Arc<AtomicU64>,
    results: Sender<Completion>,
    finished: Receiver<Completion>,
    thread: JoinHandle<()>,
}

impl Worker {
    fn new() -> std::io::Result<Self> {
        let (sender, jobs) = mpsc::channel();
        let (results, finished) = mpsc::channel();
        let settings = Arc::new(Mutex::new(None));
        let ready = Arc::new(AtomicU64::new(0));
        let worker_settings = Arc::clone(&settings);
        let worker_ready = Arc::clone(&ready);
        let thread = thread::Builder::new()
            .name("sixm-sql-supervisor".to_owned())
            .spawn(move || supervise(&jobs, &worker_settings, &worker_ready))?;
        Ok(Self {
            sender,
            settings,
            ready,
            results,
            finished,
            thread,
        })
    }

    /// Keep only the newest pending configuration, regardless of how many set
    /// commands arrive while the worker is waiting on an old database.
    fn configure(&self, config: Configuration) {
        let old = self
            .settings
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(config);
        if old.is_none() {
            let _ = self.sender.send(Message::Configure);
        }
    }
}

#[derive(Default)]
struct ActivePool {
    pool: Option<SqlPool>,
    generation: u64,
    retry_at: Option<Instant>,
}

impl ActivePool {
    fn configure(&mut self, config: &Configuration, ready: &AtomicU64) {
        self.pool = None;
        self.generation = config.generation;
        self.retry_at = None;
        let parsed = DatabaseConfig::parse(&config.connection);
        let result = parsed.and_then(|database| {
            database
                .map(|mut database| {
                    if let DatabaseConfig::Mysql(configured) = &mut database
                        && let Some(timeout) = configured.connect_timeout.as_mut()
                    {
                        *timeout = (*timeout).min(config.limits.timeout);
                    }
                    SqlPool::connect(&database, config.limits)
                })
                .transpose()
        });
        match result {
            Ok(pool) => {
                self.pool = pool;
                self.ping(ready, true);
            }
            Err(error) => {
                let reason = match error {
                    SqlError::InvalidConfig(_) => "invalid database configuration",
                    _ => "cannot open a database pool",
                };
                warn!(target: "sixm::sql", "{reason}; check mysql_connection_string and database settings");
            }
        }
    }

    fn ping(&mut self, ready: &AtomicU64, first: bool) {
        if let Some(pool) = &self.pool {
            if pool.ping().is_ok() {
                ready.store(self.generation, Ordering::Release);
                self.retry_at = None;
            } else {
                if first {
                    warn!(target: "sixm::sql", "cannot reach the configured database; retrying");
                }
                self.retry_at = Some(Instant::now() + RETRY_DELAY);
            }
        }
    }

    fn execute(&self, job: Job) {
        if job.generation != self.generation || job.cancelled.load(Ordering::Acquire) {
            job.completion.send(Err(SqlError::Cancelled.to_string()));
            return;
        }
        let Some(pool) = self.pool.as_ref().filter(|_| self.retry_at.is_none()) else {
            job.completion
                .send(Err("SQL database is unavailable".to_owned()));
            return;
        };
        let (operation, collapse_rows) = match job.request.into_operation() {
            Ok(operation) => operation,
            Err(error) => {
                job.completion.send(Err(error.to_string()));
                return;
            }
        };
        let reply = Arc::clone(&job.completion);
        let started = pool.execute_async_cancellable(operation, job.cancelled, move |outcome| {
            reply.send(
                outcome
                    .map(|value| into_result(value, collapse_rows))
                    .map_err(|error| error.to_string()),
            );
        });
        if let Err(error) = started {
            job.completion.send(Err(error.to_string()));
        }
    }
}

fn supervise(jobs: &Receiver<Message>, settings: &Mutex<Option<Configuration>>, ready: &AtomicU64) {
    let mut active = ActivePool::default();
    loop {
        let message = match active.retry_at {
            Some(at) => jobs.recv_timeout(at.saturating_duration_since(Instant::now())),
            None => jobs.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match message {
            Ok(Message::Configure) => {
                let config = settings
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take();
                if let Some(config) = config {
                    active.configure(&config, ready);
                }
            }
            Ok(Message::Execute(job)) => active.execute(job),
            Err(RecvTimeoutError::Timeout) => active.ping(ready, false),
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}
