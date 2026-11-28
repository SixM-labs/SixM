//! HTTP on the game port: the built-in routes and the `SetHttpHandler` handlers
//! of resources.
//!
//! The listener in `sixm-net` reads each request and passes it to the loop as
//! an exchange. The loop answers it here. The built-in routes are answered at
//! once. A request for `/<resource>` or `/<resource>/...` goes to the handler
//! that resource registered; its answer is sent when the handler completes it,
//! or with `504` when it has not done so within `sixm_httpHandlerTimeout`. When the
//! resource stops, its open requests get `503`.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use ring::digest::{SHA256, digest};
use serde_json::{Map, Value as Json, json};
use sixm_core::{FuncRef, Host, ResourceName, ScriptError, WebReply, WebRequest, WebResponse};
use sixm_net::{WebExchange, WebResponder};
use subtle::ConstantTimeEq;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber, warn};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

use crate::commands::{self, CommandContext, CommandSource};
use crate::filters::ChannelFilters;
use crate::manager::ResourceManager;
use crate::perf::format_millis;

/// Largest response body a handler may build. Beyond it the response fails with `500`.
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
/// Most consecutive failed RCON authentication attempts per TCP peer address.
const MAX_RCON_FAILURES: u32 = 5;
const RCON_LOCKOUT: Duration = Duration::from_secs(30);
const MAX_RCON_TRACKED_ADDRESSES: usize = 4096;

/// Tracks failed `POST /rcon` attempts to slow password guessing.
#[derive(Debug, Default)]
struct RconGuard {
    attempts: HashMap<IpAddr, RconAttempts>,
}

#[derive(Debug)]
struct RconAttempts {
    failures: u32,
    expires: Instant,
}

impl RconGuard {
    fn is_locked(&mut self, ip: IpAddr, now: Instant) -> bool {
        if let Some(attempts) = self.attempts.get(&ip) {
            if now < attempts.expires {
                return attempts.failures >= MAX_RCON_FAILURES;
            }
            self.attempts.remove(&ip);
        }
        if self.attempts.len() < MAX_RCON_TRACKED_ADDRESSES {
            return false;
        }
        self.attempts.retain(|_, attempts| now < attempts.expires);
        self.attempts.len() >= MAX_RCON_TRACKED_ADDRESSES
    }

    fn record_failure(&mut self, ip: IpAddr, now: Instant) {
        let attempts = self.attempts.entry(ip).or_insert_with(|| RconAttempts {
            failures: 0,
            expires: now + RCON_LOCKOUT,
        });
        attempts.failures += 1;
    }

    fn reset(&mut self, ip: IpAddr) {
        self.attempts.remove(&ip);
    }
}

/// The HTTP state of the server: the handlers and the open requests.
#[derive(Debug, Default)]
pub(crate) struct WebState {
    /// The `SetHttpHandler` function of each resource.
    handlers: RefCell<HashMap<ResourceName, FuncRef>>,
    /// Requests handed to a handler and not yet completed, by id.
    pending: RefCell<HashMap<u64, Pending>>,
    /// The id of the last request handed to a handler.
    last_id: Cell<u64>,
    /// Consecutive failed `/rcon` authentication attempts.
    rcon_guard: RefCell<RconGuard>,
}

/// A request a handler is answering.
#[derive(Debug)]
struct Pending {
    resource: ResourceName,
    responder: WebResponder,
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    since: Instant,
}

impl ResourceManager {
    /// Registers `handler` as the HTTP handler of `resource`, replacing the
    /// previous one. The function is retained until it is replaced or the
    /// resource stops.
    pub(crate) fn install_web_handler(&self, resource: &ResourceName, handler: FuncRef) {
        if !self.retain_ref(&handler) {
            warn!("HTTP handler of `{resource}` ignored: its function is no longer valid");
            return;
        }
        let previous = self
            .web
            .handlers
            .borrow_mut()
            .insert(resource.clone(), handler);
        if let Some(previous) = previous {
            self.release_ref(&previous);
        }
    }

    /// Removes the HTTP handler of `resource` and answers its open requests
    /// with `503`. Called when the resource stops.
    pub(crate) fn forget_web(&self, resource: &ResourceName) {
        let handler = self.web.handlers.borrow_mut().remove(resource);
        if let Some(handler) = handler {
            self.release_ref(&handler);
        }
        let stale: Vec<Pending> = {
            let mut pending = self.web.pending.borrow_mut();
            let ids: Vec<u64> = pending
                .iter()
                .filter(|(_, entry)| &entry.resource == resource)
                .map(|(id, _)| *id)
                .collect();
            ids.iter().filter_map(|id| pending.remove(id)).collect()
        };
        for entry in stale {
            entry
                .responder
                .respond(text(503, "The resource stopped before it answered"));
        }
    }

    /// Answers one request from the listener.
    pub(crate) fn handle_web(&self, exchange: WebExchange) {
        let WebExchange { request, responder } = exchange;
        if request.path == "/rcon" {
            let response = self.handle_rcon(&request, responder.client_ip());
            responder.respond(response);
            return;
        }
        if let Some(response) = self.builtin_route(&request) {
            responder.respond(response);
            return;
        }
        let found = resource_of(&request.path).and_then(|name| {
            self.web
                .handlers
                .borrow()
                .get_key_value(name)
                .map(|(resource, handler)| (resource.clone(), handler.clone()))
        });
        let Some((resource, handler)) = found else {
            responder.respond(text(404, "Not Found"));
            return;
        };
        let id = self.web.last_id.get().wrapping_add(1);
        self.web.last_id.set(id);
        self.web.pending.borrow_mut().insert(
            id,
            Pending {
                resource: resource.clone(),
                responder,
                status: 200,
                headers: Vec::new(),
                body: Vec::new(),
                since: Instant::now(),
            },
        );
        let outcome = match self.runtime_of(&handler) {
            Some(runtime) => runtime.call_web_handler(handler.id, id, &request),
            None => Err(ScriptError::StaleRef(handler.clone())),
        };
        if let Err(err) = outcome {
            warn!("HTTP handler of `{resource}` failed: {err}");
            self.fail_web(id, text(500, "The HTTP handler failed"));
        }
    }

    /// Applies one step of a handler's answer to request `request`.
    pub(crate) fn reply_web(&self, request: u64, reply: WebReply) -> Result<(), ScriptError> {
        let mut pending = self.web.pending.borrow_mut();
        let entry = pending.get_mut(&request).ok_or_else(|| {
            ScriptError::Runtime("the HTTP response was already sent or has timed out".to_owned())
        })?;
        let (last, data) = match reply {
            WebReply::Head { status, headers } => {
                if !(100..=999).contains(&status) {
                    return Err(ScriptError::Runtime(format!(
                        "{status} is not an HTTP status code"
                    )));
                }
                entry.status = status;
                entry.headers = headers;
                return Ok(());
            }
            WebReply::Write(data) => (false, data),
            WebReply::End(data) => (true, data),
        };
        if entry.body.len().saturating_add(data.len()) > MAX_RESPONSE {
            let failed = pending.remove(&request);
            drop(pending);
            if let Some(failed) = failed {
                failed
                    .responder
                    .respond(text(500, "The HTTP response is too large"));
            }
            return Err(ScriptError::Runtime(format!(
                "an HTTP response is limited to {MAX_RESPONSE} bytes"
            )));
        }
        entry.body.extend_from_slice(&data);
        if last {
            let done = pending.remove(&request);
            drop(pending);
            if let Some(done) = done {
                done.responder.respond(WebResponse {
                    status: done.status,
                    headers: done.headers,
                    body: done.body,
                });
            }
        }
        Ok(())
    }

    /// Answers the requests that have waited for a handler too long with `504`.
    /// Runs once per frame.
    pub(crate) fn expire_web(&self, now: Instant) {
        let expired: Vec<Pending> = {
            let mut pending = self.web.pending.borrow_mut();
            if pending.is_empty() {
                return;
            }
            let timeout = self.convars.borrow().http_handler_timeout();
            let ids: Vec<u64> = pending
                .iter()
                .filter(|(_, entry)| now.saturating_duration_since(entry.since) >= timeout)
                .map(|(id, _)| *id)
                .collect();
            ids.iter().filter_map(|id| pending.remove(id)).collect()
        };
        for entry in expired {
            entry
                .responder
                .respond(text(504, "The HTTP handler did not respond in time"));
        }
    }

    /// Sends `response` for request `id` and forgets the request, if it is still open.
    fn fail_web(&self, id: u64, response: WebResponse) {
        let entry = self.web.pending.borrow_mut().remove(&id);
        if let Some(entry) = entry {
            entry.responder.respond(response);
        }
    }

    /// The answer to a built-in route, or `None` when the path is not one.
    fn builtin_route(&self, request: &WebRequest) -> Option<WebResponse> {
        match request.path.as_str() {
            "/info.json" => Some(json_route(request, || self.info_document())),
            "/players.json" => Some(json_route(request, || self.players_document())),
            "/dynamic.json" => Some(json_route(request, || self.dynamic_document())),
            "/healthz" => Some(json_route(request, || self.healthz_document())),
            "/metrics" => Some(self.handle_metrics(request)),
            "/admin" => Some(self.handle_admin(request)),
            "/resmon.json" => Some(self.handle_resmon(request)),
            _ => None,
        }
    }

    /// `/info.json`: the server, its running resources and its server-info convars.
    pub(crate) fn info_document(&self) -> Json {
        let resources: Vec<String> = self
            .running_resources()
            .into_iter()
            .map(|name| name.to_string())
            .collect();
        let vars: Map<String, Json> = self
            .convars
            .borrow()
            .server_info()
            .into_iter()
            .map(|(name, value)| (name, Json::String(value)))
            .collect();
        json!({
            "server": concat!("SixM ", env!("CARGO_PKG_VERSION")),
            "resources": resources,
            "vars": vars,
        })
    }

    /// `/players.json`: id, name and ping of each player, and their identifiers
    /// when `sv_exposePlayerIdentifiers` is `true`.
    fn players_document(&self) -> Json {
        let expose = self.convars.borrow().expose_player_identifiers();
        let players: Vec<Json> = self
            .player_ids()
            .into_iter()
            .filter_map(|id| {
                let info = self.player_details(id)?;
                let ping = self
                    .player_round_trip(id)
                    .map(|rtt| u64::try_from(rtt.as_millis()).unwrap_or(u64::MAX));
                let mut player = Map::new();
                player.insert("id".to_owned(), json!(id));
                player.insert("name".to_owned(), json!(info.name));
                player.insert("ping".to_owned(), json!(ping));
                if expose {
                    player.insert("identifiers".to_owned(), json!(self.identifiers_of(id)));
                }
                Some(Json::Object(player))
            })
            .collect();
        Json::Array(players)
    }

    /// `/dynamic.json`: the host name, the player count and the player limit.
    pub(crate) fn dynamic_document(&self) -> Json {
        let (hostname, max_clients) = {
            let convars = self.convars.borrow();
            (convars.server_name().to_owned(), convars.max_clients())
        };
        json!({
            "server": concat!("SixM ", env!("CARGO_PKG_VERSION")),
            "hostname": hostname,
            "clients": self.player_ids().len(),
            "maxclients": max_clients,
        })
    }

    /// `/healthz`: readiness/liveness summary.
    fn healthz_document(&self) -> Json {
        json!({
            "ok": true,
            "resources": self.running_resources().len(),
            "players": self.player_ids().len(),
        })
    }

    /// `GET /metrics`: Prometheus text format when `sv_metricsEnabled` is on.
    fn handle_metrics(&self, request: &WebRequest) -> WebResponse {
        let convars = self.convars.borrow();
        if !convars.metrics_enabled() {
            return text(404, "Not Found");
        }
        if !is_get_or_head(&request.method) {
            return method_not_allowed("GET, HEAD");
        }
        let auth_header = request
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .map(|(_, v)| v.as_str());
        if !convars.check_prometheus_basic_auth(auth_header) {
            return WebResponse {
                status: 401,
                headers: vec![
                    (
                        "www-authenticate".to_owned(),
                        "Basic realm=\"SixM Metrics\"".to_owned(),
                    ),
                    (
                        "content-type".to_owned(),
                        "text/plain; charset=utf-8".to_owned(),
                    ),
                ],
                body: b"Unauthorized".to_vec(),
            };
        }
        drop(convars);
        let players_connected = self.player_ids().len();
        let players_max = self.convars.borrow().max_clients();
        let resources_running = self.running_resources().len();
        let entities_total = self.entity_list().len();
        let summary = self.frames.borrow().summary();
        let (avg_ms, p50_ms, p99_ms, max_ms) = match summary {
            Some(s) => (
                format_millis(s.average),
                format_millis(s.p50),
                format_millis(s.p99),
                format_millis(s.max),
            ),
            None => (
                "0.00".to_owned(),
                "0.00".to_owned(),
                "0.00".to_owned(),
                "0.00".to_owned(),
            ),
        };
        let traffic = self.network_traffic();
        let body = format!(
            "# HELP sixm_players_connected Number of currently connected players.\n\
             # TYPE sixm_players_connected gauge\n\
             sixm_players_connected {players_connected}\n\
             # HELP sixm_players_max Maximum number of allowed players.\n\
             # TYPE sixm_players_max gauge\n\
             sixm_players_max {players_max}\n\
             # HELP sixm_resources_running Number of currently running resources.\n\
             # TYPE sixm_resources_running gauge\n\
             sixm_resources_running {resources_running}\n\
             # HELP sixm_entities_total Total number of active entities.\n\
             # TYPE sixm_entities_total gauge\n\
             sixm_entities_total {entities_total}\n\
             # HELP sixm_tick_duration_ms Server tick duration in milliseconds.\n\
             # TYPE sixm_tick_duration_ms summary\n\
             sixm_tick_duration_ms{{quantile=\"avg\"}} {avg_ms}\n\
             sixm_tick_duration_ms{{quantile=\"p50\"}} {p50_ms}\n\
             sixm_tick_duration_ms{{quantile=\"p99\"}} {p99_ms}\n\
             sixm_tick_duration_ms{{quantile=\"max\"}} {max_ms}\n\
             # HELP sixm_net_rx_bytes_total Total bytes received over the game network.\n\
             # TYPE sixm_net_rx_bytes_total counter\n\
             sixm_net_rx_bytes_total {}\n\
             # HELP sixm_net_tx_bytes_total Total bytes sent over the game network.\n\
             # TYPE sixm_net_tx_bytes_total counter\n\
             sixm_net_tx_bytes_total {}\n",
            traffic.bytes_received, traffic.bytes_sent,
        );
        WebResponse {
            status: 200,
            headers: vec![
                (
                    "content-type".to_owned(),
                    "text/plain; version=0.0.4; charset=utf-8".to_owned(),
                ),
                ("cache-control".to_owned(), "no-cache".to_owned()),
            ],
            body: body.into_bytes(),
        }
    }

    /// `GET /admin`: self-contained admin UI when `sv_adminUiEnabled` and
    /// `rcon_password` are both configured.
    fn handle_admin(&self, request: &WebRequest) -> WebResponse {
        let convars = self.convars.borrow();
        if !convars.admin_ui_enabled() {
            return text(404, "Not Found");
        }
        if convars.rcon_password().is_none() {
            return text(403, "Admin UI requires rcon_password to be configured");
        }
        drop(convars);
        if !is_get_or_head(&request.method) {
            return method_not_allowed("GET, HEAD");
        }
        WebResponse {
            status: 200,
            headers: vec![
                (
                    "content-type".to_owned(),
                    "text/html; charset=utf-8".to_owned(),
                ),
                ("cache-control".to_owned(), "no-store".to_owned()),
            ],
            body: include_str!("admin.html").as_bytes().to_vec(),
        }
    }

    /// `GET /resmon.json`: live per-resource monitor snapshot gated identically to `/admin`.
    fn handle_resmon(&self, request: &WebRequest) -> WebResponse {
        let convars = self.convars.borrow();
        if !convars.admin_ui_enabled() {
            return text(404, "Not Found");
        }
        if convars.rcon_password().is_none() {
            return text(403, "Admin UI requires rcon_password to be configured");
        }
        let hitch_warning_ms = convars.hitch_warning_ms();
        drop(convars);
        if !is_get_or_head(&request.method) {
            return method_not_allowed("GET, HEAD");
        }
        let resources: Vec<Json> = self
            .resmon_snapshot()
            .into_iter()
            .map(|(name, stats)| {
                let avg_u32 = u32::try_from(stats.avg_tick_micros).unwrap_or(u32::MAX);
                json!({
                    "name": name.as_ref(),
                    "samples": stats.samples,
                    "avg_tick_ms": f64::from(avg_u32) / 1000.0,
                    "max_tick_ms": f64::from(stats.max_tick_micros) / 1000.0,
                    "last_tick_ms": f64::from(stats.last_tick_micros) / 1000.0,
                    "heap_bytes": stats.heap_bytes,
                    "window_events": stats.window_events,
                    "total_events": stats.total_events,
                })
            })
            .collect();
        let body = serde_json::to_vec(&json!({
            "hitch_warning_ms": hitch_warning_ms,
            "resources": resources,
        }))
        .unwrap_or_default();
        WebResponse {
            status: 200,
            headers: vec![
                ("content-type".to_owned(), "application/json".to_owned()),
                ("cache-control".to_owned(), "no-store".to_owned()),
            ],
            body,
        }
    }

    /// `POST /rcon`: authenticates with `rcon_password` and runs one console command.
    fn handle_rcon(&self, request: &WebRequest, client_ip: IpAddr) -> WebResponse {
        if request.method != "POST" {
            return WebResponse {
                status: 405,
                headers: vec![
                    ("allow".to_owned(), "POST".to_owned()),
                    (
                        "content-type".to_owned(),
                        "text/plain; charset=utf-8".to_owned(),
                    ),
                ],
                body: b"Method Not Allowed".to_vec(),
            };
        }
        let convars = self.convars.borrow();
        let Some(expected) = convars.rcon_password() else {
            return text(403, "RCON is disabled");
        };
        let now = Instant::now();
        if self.web.rcon_guard.borrow_mut().is_locked(client_ip, now) {
            warn!("locked out RCON requests from ip:{client_ip}");
            return text(429, "Too many failed RCON attempts");
        }
        let Some((supplied, command)) = parse_rcon_payload(request) else {
            return text(
                400,
                "Expected JSON {command, password} or auth header + command",
            );
        };
        let supplied = digest(&SHA256, supplied.as_bytes());
        if !bool::from(expected.as_ref().ct_eq(supplied.as_ref())) {
            tracing::debug!("rejected unauthorized RCON attempt from ip:{client_ip}");
            self.web
                .rcon_guard
                .borrow_mut()
                .record_failure(client_ip, now);
            return text(401, "Unauthorized");
        }
        drop(convars);
        self.web.rcon_guard.borrow_mut().reset(client_ip);

        let output = capture_logs(self.channel_filters(), || {
            commands::run_line(self, &command, &CommandContext::new(CommandSource::Console));
            commands::drain_queued(self);
        });
        let body = serde_json::to_vec(&json!({ "ok": true, "output": output })).unwrap_or_default();
        WebResponse {
            status: 200,
            headers: vec![
                ("content-type".to_owned(), "application/json".to_owned()),
                ("cache-control".to_owned(), "no-store".to_owned()),
            ],
            body,
        }
    }
}

fn parse_rcon_payload(request: &WebRequest) -> Option<(String, String)> {
    let header_pw = request
        .headers
        .iter()
        .find_map(|(k, v)| match k.as_str() {
            "x-rcon-password" => Some(v.as_str()),
            "authorization" => v
                .strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer ")),
            _ => None,
        })
        .map(str::to_owned);

    if let Ok(Json::Object(map)) = serde_json::from_slice::<Json>(&request.body) {
        let command = map.get("command").and_then(Json::as_str)?.trim().to_owned();
        let password = map
            .get("password")
            .and_then(Json::as_str)
            .map(str::to_owned)
            .or(header_pw)?;
        return (!command.is_empty()).then_some((password, command));
    }

    let password = header_pw?;
    let command = std::str::from_utf8(&request.body).ok()?.trim().to_owned();
    (!command.is_empty()).then_some((password, command))
}

struct LogCollector(Arc<Mutex<Vec<String>>>);

impl<S: Subscriber> Layer<S> for LogCollector {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        if !visitor.message.is_empty() {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(visitor.message);
        }
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: String,
}

impl Visit for MessageVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            value.clone_into(&mut self.message);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        }
    }
}

fn capture_logs(filters: &ChannelFilters, run: impl FnOnce()) -> Vec<String> {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry()
        .with(LogCollector(Arc::clone(&lines)).with_filter(filters.tracing_filter()));
    tracing::subscriber::with_default(subscriber, run);
    std::mem::take(&mut *lines.lock().unwrap_or_else(PoisonError::into_inner))
}

/// The resource a path names: `res` for `/res` and `/res/...`.
fn resource_of(path: &str) -> Option<&str> {
    path.strip_prefix('/')?
        .split('/')
        .next()
        .filter(|name| !name.is_empty())
}

/// A plain-text response.
fn text(status: u16, message: &str) -> WebResponse {
    WebResponse {
        status,
        headers: vec![(
            "content-type".to_owned(),
            "text/plain; charset=utf-8".to_owned(),
        )],
        body: message.as_bytes().to_vec(),
    }
}

fn is_get_or_head(method: &str) -> bool {
    method == "GET" || method == "HEAD"
}

fn method_not_allowed(allow: &str) -> WebResponse {
    WebResponse {
        status: 405,
        headers: vec![
            ("allow".to_owned(), allow.to_owned()),
            (
                "content-type".to_owned(),
                "text/plain; charset=utf-8".to_owned(),
            ),
        ],
        body: b"Method Not Allowed".to_vec(),
    }
}

fn json_route(request: &WebRequest, build: impl FnOnce() -> Json) -> WebResponse {
    if !is_get_or_head(&request.method) {
        return method_not_allowed("GET, HEAD");
    }
    let body = serde_json::to_vec(&build()).unwrap_or_default();
    WebResponse {
        status: 200,
        headers: vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            ("cache-control".to_owned(), "no-cache".to_owned()),
        ],
        body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rcon_lockout_is_per_address_and_expires() {
        let mut guard = RconGuard::default();
        let now = Instant::now();
        let first = IpAddr::from([192, 0, 2, 1]);
        let second = IpAddr::from([192, 0, 2, 2]);
        for _ in 0..MAX_RCON_FAILURES {
            assert!(!guard.is_locked(first, now));
            guard.record_failure(first, now);
        }
        assert!(guard.is_locked(first, now));
        assert!(!guard.is_locked(second, now));
        assert!(!guard.is_locked(first, now + RCON_LOCKOUT));
    }

    #[test]
    fn the_resource_is_the_first_path_segment() {
        assert_eq!(resource_of("/res"), Some("res"));
        assert_eq!(resource_of("/res/a/b"), Some("res"));
        assert_eq!(resource_of("/"), None);
        assert_eq!(resource_of("//x"), None);
        assert_eq!(resource_of("res/x"), None);
    }
}
