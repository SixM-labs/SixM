# Database Access: `sixm-sql` and `oxmysql` Parity (ADR / Design)

Status: **accepted** (Phase E, E1 decision approved by maintainer on 2026-11-28).
Implementation target: `crates/sixm-sql` (**E2**), `resources/[system]/oxmysql` (**E3**),
docs and example (**E4**).

Clean room: this design uses only public `oxmysql` / `mysql-async` /
`ghmattimysql` API documentation and standard SQL concepts. It copies no FiveM,
Cfx.re, or third-party resource code.

---

## 1. Context and Goals

FiveM frameworks (`es_extended`, `qb-core`, `ox_inventory`) depend on `oxmysql`
(and legacy `mysql-async` / `ghmattimysql` aliases) via `mysql_connection_string`
to execute asynchronous SQL queries without stalling the server main tick.

Requirements from [FiveM parity plan](fivem-parity/PLAN.md):

1. Separate workspace crate **`crates/sixm-sql`** (`#![forbid(unsafe_code)]`).
2. Support **MySQL / MariaDB** and **SQLite** on both **Linux** and **Windows
   MSVC**, with **no external system libraries** (`libmysqlclient`, `OpenSSL`, or
   pre-installed `sqlite3` DLLs/headers must not be required at build or runtime).
3. Parse **`mysql_connection_string`** in both URI and `key=value;` forms.
4. Execute queries on background worker threads / async tasks without blocking
   the single-threaded server frame loop, delivering outcomes through SixM's
   existing `FuncRef` callback + `promise` / `Await` pipeline.
5. Enforce **parameterized binding only** (no client-side string interpolation).
6. Enforce strict operational limits (row count, result byte size, timeout,
   in-flight queue caps, and credential protection).

---

## 2. Decisions

### 2.1 Driver crates: `mysql_async` + `rusqlite` (`bundled`)

- **MySQL / MariaDB:** `mysql_async` (`default-features = false`, features
  `["minimal", "rustls-tls", "ring"]`).
  - Pure Rust wire protocol + TLS reusing SixM's existing `tokio` (1.54),
    `rustls` (0.23), and `ring` (0.17) dependencies — zero C/OpenSSL system
    library dependencies on Linux or Windows.
  - Supports native `tokio` connection pooling, prepared statements,
    transactions, TLS (`ssl=true` / `sslmode`), and MariaDB/MySQL auth plugins.
- **SQLite:** `rusqlite` (`features = ["bundled"]`).
  - Compiles upstream SQLite C sources statically via `cc` in the same way SixM
    already bundles Lua 5.4 (`mlua` `vendored`) and QuickJS (`rquickjs`), with
    no system packages on Linux or Windows MSVC.
  - WAL mode and busy timeout are configured per connection; queries run on a
    bounded dedicated worker thread pool (never on `tokio` reactor threads or
    the server main loop).
- **Why not `sqlx`:** `sqlx` carries larger compile-time/macro surface area while
  dynamic runtime query execution and heterogeneous JSON/FiveM value binding map
  more directly onto `mysql_async::Value` and `rusqlite::types::Value`.
- **Unsafe lint rule:** Workspace `[workspace.lints.rust] unsafe_code = "forbid"`
  is preserved across `crates/sixm-sql` and all SixM crates.

### 2.2 Default SQLite behavior: compiled in, opt-in via connection string

- SQLite (`rusqlite` `bundled`) is **compiled in by default** (no opt-in Cargo
  feature needed for server binaries or CI).
- **Inactive when unconfigured:** When `mysql_connection_string` is empty or
  unset, the database subsystem stays inactive (`isReady() == false`) and
  creates no unexpected fallback `.db` file in production.
- **Activated via `mysql_connection_string`:** Setting
  `set mysql_connection_string "sqlite://data/server.db"` (or `sqlite::memory:`,
  `sqlite:path.db`, or `driver=sqlite;filename=data/server.db`) opens SQLite
  with zero external database server required.

---

## 3. Configuration and Security Boundaries

### 3.1 Console-only convars

Because `mysql_connection_string` contains database credentials (and/or local
filesystem SQLite paths), it **must** be added to `CONSOLE_ONLY_CONVARS` in
`crates/sixm-server/src/convars.rs` — never readable or writable from scripts
or replicated to clients:

| Convar | Default | Console-only | Notes |
|---|---|---|---|
| `mysql_connection_string` | `""` | Yes | Parsed when DB initializes / reconnects; never exposed to `GetConvar*` in scripts. |
| `sixm_dbPoolSize` | `10` | Yes | Max concurrent pooled connections (clamped `1..=64`). |
| `sixm_dbTimeoutMs` | `10000` | Yes | Per-query / transaction timeout in milliseconds (clamped `100..=300000`). |
| `sixm_dbMaxRows` | `10000` | Yes | Maximum rows materialized per result set (clamped `1..=100000`). |
| `sixm_dbMaxResultBytes` | `16777216` | Yes | Maximum serialized result payload bytes per query (`16 MiB`, clamped `65536..=67108864`). |
| `sixm_dbSlowQueryWarnMs` | `200` | No (console/server) | Logs a warning on `target: "sixm::sql"` when a query exceeds this duration (`0` disables). |

All values remain bounded (`MAX_IN_FLIGHT_PER_RESOURCE = 256`,
`QUEUE_CAPACITY = 1024`) matching the bounded-worker discipline of
`crates/sixm-server/src/http.rs`.

### 3.2 `mysql_connection_string` parsing

Both standard `oxmysql` forms are parsed in `crates/sixm-sql/src/config.rs`:

1. **URI form:**
   - `mysql://[user[:pass]@]host[:port][/database][?query...]`
   - `mysql2://...` and `mariadb://...` accepted as aliases for `mysql://`.
   - `sqlite:///abs/path.db`, `sqlite://rel/path.db`, `sqlite:rel/path.db`,
     `sqlite::memory:`.
   - Query options parsed cleanly: `charset` (default `utf8mb4`),
     `connectTimeout`, `multipleStatements` (ignored/rejected — see §5),
     `dateStrings`, `ssl` / `sslmode`.
2. **Semicolon `key=value;` form:**
   - Case-insensitive keys split on `;` (ignoring empty trailing segments):
     `host`/`server`/`addr`, `port`, `user`/`uid`/`username`,
     `password`/`pwd`, `database`/`db`/`initial catalog`,
     `driver`/`provider` (`mysql` / `mariadb` / `sqlite`),
     `filename`/`data source`/`filepath` (SQLite path), `charset`, `sslmode`.

Never log the raw `mysql_connection_string` or password in tracing output; redact
credentials in error messages (`Debug` implementation redacts passwords).

---

## 4. Parameter Binding and Dialect Rules

**Zero client-side string interpolation.** Queries are always executed via driver
prepared/parameterized statements.

`crates/sixm-sql/src/params.rs` scans the SQL string outside `'...'` single-quoted
strings, `"..."` double-quoted identifiers, `` `...` `` backtick identifiers,
`-- ...` line comments, and `/* ... */` block comments:

1. **Positional `?` placeholders:** Bound sequentially from JSON/script array
   arguments (`[a, b, ...]`).
2. **Named `@name` / `:name` placeholders:** Bound from JSON/script object/map
   keys (stripping optional leading `@` or `:` on parameter keys when matching
   `oxmysql` conventions) and lowered deterministically:
   - MySQL: rewritten to positional `?` placeholders in occurrence order with a
     matching flat `Vec<SqlValue>` (so repeated `@id` references work identically).
   - SQLite: rewritten to positional `?1`, `?2`, ... or matched directly via
     named parameters in SQLite.
3. **`prepare` / `rawExecute` constraints:** Match documented `oxmysql`
   behavior where `prepare` takes positional `?` placeholders (also supporting
   single-row or multi-row parameter batches as arrays of parameter arrays).
4. **Type validation & overflow checks:** Reject mismatch between placeholder
   count and supplied parameter array (unless extra object keys are allowed for
   named bindings, missing named keys return an explicit error), nested
   arrays/objects (except normalized binary blobs / `SqlValue`), and functions.

---

## 5. Query Execution, Async Delivery, and Transactions

### 5.1 Crate API (`crates/sixm-sql`)

```text
crates/sixm-sql/
├── Cargo.toml
└── src/
    ├── lib.rs         # Public DatabaseConfig, SqlPool, QueryKind, QueryResult, SqlError
    ├── config.rs      # mysql_connection_string parser (URI + key=value;), redacted Debug
    ├── params.rs      # Tokenizer-safe placeholder parser (? and @name/:name) & Value mapping
    ├── sqlite.rs      # rusqlite (bundled) worker pool, transactions, row/size limit checks
    └── mysql.rs       # mysql_async tokio pool (rustls/ring), prepared queries, transactions
```

Core operations mapped to `oxmysql` semantics:

| Operation | Return Shape (`Value`) | Notes |
|---|---|---|
| `Query` (`SELECT`) | `Array` of `Map` rows (`[{ col: val, ... }]`) | Returns empty array if zero rows. |
| `Query` (DML `INSERT`/`UPDATE`/`DELETE`) | `Map { affectedRows, insertId, changedRows }` | Matches `oxmysql` DML summary shape. |
| `Single` | `Map` of first row, or `Nil` | `nil` in Lua / `null` in JS if empty. |
| `Scalar` | First column of first row (`Value`), or `Nil` | `nil` / `null` if no row or `NULL`. |
| `Insert` | `Int(last_insert_id)` (or `0` / `Nil`) | `last_insert_rowid()` on SQLite; `last_insert_id()` on MySQL. |
| `Update` | `Int(affected_rows)` | Number of modified rows. |
| `Prepare` | Batch or single-set prepared result | Reuses prepared statement per execution / batch. |
| `RawExecute` | Array of row maps or DML metadata | Same safe binding, bypasses wrapper normalization extras. |
| `Transaction` | `Bool(true)` on commit, error / `Bool(false)` on rollback | Atomic SQL transaction across N statements (`{ query, values }` list or shared parameter map); rolls back on any error or timeout. |

### 5.2 Server Host Integration (Zero Main-Loop Blocking)

Mirroring `HttpClient` (`crates/sixm-server/src/http.rs`):

1. Scripts call SQL through a server-only host bridge on `ServerHost` (`sql_is_ready`,
   `sql_query`, `sql_prepare`, `sql_transaction`), passing `(resource, op, callback: FuncRef)`
   (or a synchronous read for `is_ready`).
2. `SqlService` in `sixm-server` enforces per-resource `MAX_IN_FLIGHT`, attaches an
   `Arc<AtomicBool>` cancellation token tied to the resource instance (`FuncRef::instance`),
   and dispatches work onto `SqlPool`'s background runtime/threads.
3. On completion (or timeout / error), the worker pushes
   `SqlCompletion { resource, callback, outcome }` into `Arc<Mutex<Vec<SqlCompletion>>>`.
4. Once per server frame (`ResourceManager::complete_sql()`), completions are drained:
   if the resource stopped before completion, the result is discarded without invoking
   stale callbacks; otherwise `runtime.call_ref(callback.id, &[err_or_nil, result])`
   runs cleanly and releases `FuncRef`.
5. `resources/[system]/oxmysql` (**E3**) wraps those host calls into all `oxmysql`
   exports (`query`, `single`, `scalar`, `insert`, `update`, `prepare`,
   `rawExecute`, `transaction`, `isReady`, `ready`), `.await` coroutines / JS
   promises, `@oxmysql/lib/MySQL.lua` (`MySQL.Async.*`, `MySQL.Sync.*`,
   `MySQL.ready`), and `provide 'mysql-async'` / `provide 'ghmattimysql'` aliases.

### 5.3 Limits and Safety Invariants

- **Row cap (`sixm_dbMaxRows`):** Iteration stops immediately with an explicit
  `SqlError::ResultTooLarge` if a query produces more than `max_rows`.
- **Byte cap (`sixm_dbMaxResultBytes`):** Track incremental materialized payload
  bytes while decoding columns so huge `BLOB`/`TEXT` results abort before OOM.
- **Query timeout (`sixm_dbTimeoutMs`):** Enforced around `tokio::time::timeout`
  (MySQL) and `sqlite3_progress_handler` deadline / connection busy timeout
  (SQLite), preventing runaway `recursive CTE` / heavy joins from wedging workers.
- **Multi-statement disabled:** Reject or configure MySQL without
  `MULTI_STATEMENTS` so stacked queries (`; DROP TABLE ...`) cannot bypass single-statement
  binding.

---

## 6. Cross-Platform CI and Verification Plan

- **Unit + SQLite integration tests (`E2`):** Always run on both `ubuntu-latest`
  and `windows-latest` (`cargo test -p sixm-sql` and `cargo test --workspace`),
  covering URI/key-value parsing, credential redaction, `?` and `@name` binding,
  SQL injection attempts inside literals/parameters, transactions/rollback, and
  row/byte/timeout caps.
- **MySQL / MariaDB integration tests (`E2`):** Run against a real MySQL/MariaDB
  CI service container on Linux gated by an env var (`SIXM_TEST_MYSQL_URL`) so
  local developers without MySQL and `windows-latest` CI stay green cleanly.
- **End-to-end Lua + JS resource tests (`E3`, `E4`):** Boot `sixm-server` with
  SQLite + `[system]/oxmysql` and exercise `oxmysql`, `mysql-async`, and
  `ghmattimysql` callers across Lua and JS resources.
