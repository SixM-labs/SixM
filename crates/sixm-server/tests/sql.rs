//! Exercise the shipped oxmysql resource, both script runtimes, and the SQLite worker.

mod common;

use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use common::{Fixture, manifest};
use sixm_core::{Host, Value};
use sixm_sql::{DatabaseConfig, ParamSet, SqlLimits, SqlPool, SqliteConfig, SqliteTarget};

const TIMEOUT: Duration = Duration::from_secs(10);
const STEP: Duration = Duration::from_millis(5);

fn fixture() -> Fixture {
    let fx = Fixture::new();
    fx.add_resource(
        "[system]/oxmysql",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/server-data/resources/[system]/oxmysql/fxmanifest.lua"
        )),
        &[
            (
                "server.lua",
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../examples/server-data/resources/[system]/oxmysql/server.lua"
                )),
            ),
            (
                "lib/MySQL.lua",
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../examples/server-data/resources/[system]/oxmysql/lib/MySQL.lua"
                )),
            ),
        ],
    );
    fx
}

fn call(fx: &Fixture, resource: &str, export: &str) -> Vec<Value> {
    fx.manager()
        .call_export(resource, export, &[], None)
        .unwrap_or_else(|err| panic!("{resource}.{export} failed: {err}"))
}

fn tick_until(fx: &Fixture, description: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !done() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {description}"
        );
        fx.server.tick(Instant::now());
        thread::sleep(STEP);
    }
}

fn assert_script_completed(fx: &Fixture, resource: &str) {
    tick_until(fx, resource, || fx.text(resource, "report") != "pending");
    assert_eq!(fx.text(resource, "report"), "ok");
}

#[test]
fn lua_await_callbacks_and_legacy_imports_use_the_same_database() {
    let fx = fixture();
    let app = r#"
        local report, ready = 'pending', false
        exports('report', function() return report end)
        exports('ready', function() return ready end)
        MySQL.ready(function() ready = true end)

        CreateThread(function()
            local ok, err = pcall(function()
                assert(Citizen.Await(exports.oxmysql:ready_async()))
                assert(Await(exports.oxmysql:isReady_async()))
                local ddl = MySQL.query.await('CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT NOT NULL)')
                assert(ddl.affectedRows == 0, 'DDL metadata')

                local name = "a quote: ' is bound as data"
                local insert = exports.oxmysql:insert_async('INSERT INTO items VALUES (?, ?)', {1, name})
                assert(Await(insert) == 1, 'insert ID')
                assert(Await(insert) == 1, 'second await must reuse the first result')
                assert(MySQL.Sync.fetchScalar('SELECT name FROM items WHERE id = @id', {['@id'] = 1}) == name,
                    'legacy named parameter')

                local callback = promise.new()
                exports['ghmattimysql']:execute('SELECT id, name FROM items', function(rows, err)
                    if err then callback:reject(err) else callback:resolve(rows) end
                end)
                local rows = Await(callback)
                assert(#rows == 1 and rows[1].id == 1 and rows[1].name == name, 'legacy export result')

                callback = promise.new()
                MySQL.Async.execute('UPDATE items SET name = ? WHERE id = ?', {'updated', 1}, function(rows, err)
                    if err then callback:reject(err) else callback:resolve(rows) end
                end)
                assert(Await(callback) == 1, 'legacy affected rows')

                callback = promise.new()
                exports['mysql-async']:mysql_fetch_all('SELECT name FROM items', {}, function(rows, err)
                    if err then callback:reject(err) else callback:resolve(rows) end
                end)
                assert(Await(callback)[1].name == 'updated', 'mysql-async alias')
                assert(MySQL.single.await('SELECT name FROM items WHERE id = ?', {1}).name == 'updated')
                assert(MySQL.single.await('SELECT name FROM items WHERE id = ?', {-1}) == nil)

                assert(MySQL.transaction.await({
                    {query = 'INSERT INTO items VALUES (?, ?)', values = {2, 'second'}},
                    {'INSERT INTO items VALUES (?, ?)', {3, 'third'}},
                }), 'transaction commit')
                assert(MySQL.transaction.await({
                    {query = 'INSERT INTO items VALUES (?, ?)', parameters = {4, 'rolled back'}},
                    {query = 'INSERT INTO items VALUES (?, ?)', values = {1, 'duplicate'}},
                }) == false, 'transaction rollback result')
                assert(MySQL.scalar.await('SELECT COUNT(*) FROM items') == 3, 'transaction atomicity')

                MySQL.prepare.await('INSERT INTO items VALUES (?, ?)', {{4, 'fourth'}, {5, 'fifth'}})
                local raw = MySQL.rawExecute.await('SELECT name FROM items WHERE id = ?', {5})
                assert(#raw == 1 and raw[1].name == 'fifth', 'prepared batch and raw row')

                local valid, reason = pcall(function()
                    return MySQL.query.await('SELECT ?', {})
                end)
                assert(not valid and tostring(reason):find('parameter'), 'invalid bind must reject')
            end)
            report = ok and 'ok' or tostring(err)
        end)
    "#;
    fx.add_resource(
        "lua_app",
        &manifest(
            &["@oxmysql/lib/MySQL.lua", "app.lua"],
            &[],
            &["mysql-async", "ghmattimysql"],
        ),
        &[("app.lua", app)],
    );
    fx.manager().start("lua_app").expect("start Lua app");
    assert!(fx.manager().is_running("oxmysql"));
    assert!(fx.manager().is_running("mysql-async"));
    assert!(fx.manager().is_running("ghmattimysql"));
    assert_eq!(call(&fx, "oxmysql", "isReady"), [Value::Bool(false)]);
    fx.server.tick(Instant::now());
    assert_eq!(call(&fx, "lua_app", "ready"), [Value::Bool(false)]);

    fx.server
        .execute("set mysql_connection_string sqlite::memory:");
    assert_script_completed(&fx, "lua_app");
    assert_eq!(call(&fx, "lua_app", "ready"), [Value::Bool(true)]);
    assert_eq!(call(&fx, "oxmysql", "isReady"), [Value::Bool(true)]);

    fx.server.execute("set mysql_connection_string \"\"");
    assert_eq!(call(&fx, "oxmysql", "isReady"), [Value::Bool(false)]);
    fx.server
        .execute("set mysql_connection_string driver=sqlite;filename=:memory:");
    tick_until(&fx, "reconfigured database", || {
        call(&fx, "oxmysql", "isReady") == [Value::Bool(true)]
    });
}

#[test]
fn javascript_await_and_callbacks_cross_the_lua_export_boundary() {
    let fx = fixture();
    fx.server
        .execute("set mysql_connection_string sqlite::memory:");
    fx.add_resource(
        "js_app",
        &manifest(&["app.js"], &[], &["oxmysql"]),
        &[(
            "app.js",
            r#"
            let report = "pending";
            exports("report", () => report);

            function check(condition, message) {
                if (!condition) throw new Error(message);
            }

            async function run() {
                await new Promise((resolve) => exports.oxmysql.ready(resolve));
                await exports.oxmysql.query_async("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)");
                const insert = exports.oxmysql.insert_async("INSERT INTO users VALUES (?, ?)", [1, "Alice"]);
                check(await insert === 1 && await insert === 1, "insert thenable");
                const rows = await exports.oxmysql.query_async("SELECT * FROM users WHERE id = :id", {id: 1});
                check(rows.length === 1 && rows[0].name === "Alice", "Lua rows in JS");
                const name = await new Promise((resolve, reject) => {
                    exports.oxmysql.scalar("SELECT name FROM users WHERE id = 1", (value, error) => {
                        if (error) reject(error); else resolve(value);
                    });
                });
                check(name === "Alice", "async callback argument");
                check(await exports.oxmysql.single_async("SELECT * FROM users WHERE id = 2") === null, "missing row");
                check(await exports.ghmattimysql.scalarSync("SELECT COUNT(*) FROM users") === 1, "legacy thenable");

                const isNull = await new Promise((resolve, reject) => {
                    SixmSqlExecute("scalar", "SELECT ? IS NULL", [null], (error, value) => {
                        if (error) reject(error); else resolve(value);
                    });
                });
                check(isNull === 1, "native SQL NULL parameter");

                await new Promise((resolve, reject) => {
                    let completed = 0;
                    const callback = (error, value) => {
                        if (error || value !== 1) reject(error || new Error("unexpected capped query result"));
                        if (++completed === 256) resolve();
                    };
                    for (let i = 0; i < 256; i++) {
                        SixmSqlExecute("scalar", "SELECT 1", null, callback);
                    }
                    let capped = false;
                    try {
                        SixmSqlExecute("scalar", "SELECT 1", null, callback);
                    } catch (error) {
                        capped = String(error).includes("SQL jobs in flight");
                    }
                    check(capped, "257th job must be refused before the next frame");
                });

                let rejected = false;
                try {
                    await exports.oxmysql.query_async("SELECT * FROM missing_table");
                } catch (error) {
                    rejected = String(error).includes("missing_table");
                }
                check(rejected, "thenable rejection");
            }

            run().then(() => report = "ok", (error) => report = String(error));
            "#,
        )],
    );
    fx.manager().start("js_app").expect("start JS app");
    assert_script_completed(&fx, "js_app");
}

struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("log capture lock").write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn database_connection_string_stays_secret_and_console_only() {
    let fx = fixture();
    fx.add_resource(
        "probe",
        &manifest(&["probe.lua"], &[], &[]),
        &[(
            "probe.lua",
            r"
            local changes = 0
            AddConvarChangeListener(nil, function(name)
                if name:lower() == 'mysql_connection_string' then changes = changes + 1 end
            end)
            exports('read', function()
                return GetConvar('MYSQL_CONNECTION_STRING', 'hidden'), changes
            end)
            exports('attemptWrites', function()
                local write = pcall(SetConvar, 'mysql_connection_string', 'script-secret')
                local replicate = pcall(SetConvarReplicated, 'mysql_connection_string', 'script-secret')
                local publish = pcall(SetConvarServerInfo, 'mysql_connection_string', 'script-secret')
                local configure = pcall(SetConvar, 'sixm_dbPoolSize', '64')
                ExecuteCommand('set MySQL_Connection_String queued-secret')
                return write, replicate, publish, configure
            end)
            ",
        )],
    );
    fx.manager().start("probe").expect("start convar probe");
    fx.manager().start("oxmysql").expect("start oxmysql");
    let captured = Arc::new(Mutex::new(Vec::new()));
    let output = Arc::clone(&captured);
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || LogCapture(Arc::clone(&output)))
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        fx.server
            .execute("add_ace resource.probe command.set allow");
        fx.server
            .execute(r#"set MYSQL_CONNECTION_STRING "mysql://user:console-secret@localhost/db""#);
        assert_eq!(
            call(&fx, "probe", "read"),
            [Value::from("hidden"), Value::Int(0)]
        );
        fx.server.execute("mysql_connection_string");
        fx.server
            .execute("sets mysql_connection_string public-secret");
        fx.server
            .execute("setr mysql_connection_string replicated-secret");
        fx.server
            .execute("set \"MYSQL_CONNECTION_STRING\" \"unterminated-secret");
        assert_eq!(
            call(&fx, "probe", "attemptWrites"),
            vec![Value::Bool(false); 4]
        );
        fx.server
            .execute("set mysql_connection_string sqlite::memory:");
        tick_until(&fx, "protected database", || {
            call(&fx, "oxmysql", "isReady") == [Value::Bool(true)]
        });

        // An authorized command relay still cannot set the database connection string.
        assert_eq!(
            call(&fx, "probe", "attemptWrites"),
            vec![Value::Bool(false); 4]
        );
        fx.server.tick(Instant::now());
        assert_eq!(call(&fx, "oxmysql", "isReady"), [Value::Bool(true)]);
        assert_eq!(
            call(&fx, "probe", "read"),
            [Value::from("hidden"), Value::Int(0)]
        );
        assert_eq!(fx.manager().convar("mysql_connection_string"), None);
        assert_eq!(fx.manager().convar_kind("mysql_connection_string"), None);
    });
    let logs =
        String::from_utf8(captured.lock().expect("log capture lock").clone()).expect("UTF-8 logs");
    assert!(!logs.is_empty(), "expected convar command diagnostics");
    for secret in [
        "console-secret",
        "public-secret",
        "replicated-secret",
        "unterminated-secret",
        "script-secret",
        "queued-secret",
    ] {
        assert!(!logs.contains(secret), "connection string leaked: {logs}");
    }
}

#[test]
fn stopping_the_origin_cancels_sql_even_when_the_observer_and_oxmysql_keep_running() {
    let fx = fixture();
    let database = fx.root().join("cancel.sqlite");
    let config = DatabaseConfig::Sqlite(SqliteConfig {
        target: SqliteTarget::File(database.clone()),
    });
    let lock = SqlPool::connect(
        &config,
        SqlLimits {
            pool_size: 1,
            ..SqlLimits::default()
        },
    )
    .expect("open independent SQLite connection");
    lock.query("CREATE TABLE cancelled (n INTEGER)", ParamSet::Empty)
        .expect("create test table");
    fx.server.execute("set sixm_dbPoolSize 1");
    fx.server.execute_words(&[
        "set".to_owned(),
        "mysql_connection_string".to_owned(),
        format!("driver=sqlite;filename=\"{}\"", database.display()),
    ]);
    fx.add_resource(
        "observer",
        &manifest(&["observer.lua"], &[], &[]),
        &[(
            "observer.lua",
            r"
            local calls = 0
            local function callback() calls = calls + 1 end
            exports('callback', function() return callback end)
            exports('count', function() return calls end)
            ",
        )],
    );
    fx.add_resource(
        "origin",
        &manifest(&["origin.lua"], &[], &["oxmysql", "observer"]),
        &[(
            "origin.lua",
            r"
            local report = 'pending'
            exports('report', function() return report end)
            exports('enqueue', function()
                exports.oxmysql:query([[
                    WITH RECURSIVE numbers(n) AS (
                        VALUES(1) UNION ALL SELECT n + 1 FROM numbers WHERE n < 100000
                    )
                    INSERT INTO cancelled SELECT SUM(n) FROM numbers
                ]], exports.observer:callback())
            end)
            exports('verify', function()
                CreateThread(function()
                    local ok, count = pcall(function()
                        return Await(exports.oxmysql:scalar_async('SELECT COUNT(*) FROM cancelled'))
                    end)
                    report = ok and count == 0 and 'ok' or ('unexpected row count: %s'):format(tostring(count))
                end)
            end)
            ",
        )],
    );
    fx.manager()
        .start("origin")
        .expect("start origin and its dependencies");
    tick_until(&fx, "SQLite readiness", || {
        call(&fx, "oxmysql", "isReady") == [Value::Bool(true)]
    });

    // The query cannot commit before stop. The recursive query also crosses a
    // SQLite progress checkpoint if it was already waiting for this lock.
    lock.query("BEGIN IMMEDIATE", ParamSet::Empty)
        .expect("hold SQLite write lock");
    call(&fx, "origin", "enqueue");
    fx.manager()
        .stop("origin")
        .expect("stop submitting resource");
    lock.query("ROLLBACK", ParamSet::Empty)
        .expect("release SQLite write lock");

    assert!(fx.manager().is_running("observer"));
    assert!(fx.manager().is_running("oxmysql"));
    fx.manager().start("origin").expect("restart origin");
    call(&fx, "origin", "verify");
    assert_script_completed(&fx, "origin");
    assert_eq!(call(&fx, "observer", "count"), [Value::Int(0)]);

    // The identical statement runs and calls its observer while the origin stays started.
    call(&fx, "origin", "enqueue");
    tick_until(&fx, "callback from the restarted resource", || {
        call(&fx, "observer", "count") == [Value::Int(1)]
    });
    assert_eq!(
        lock.scalar("SELECT COUNT(*) FROM cancelled", ParamSet::Empty)
            .expect("read the independently committed row"),
        Some(sixm_sql::SqlValue::Int(1))
    );
}
