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
use std::time::Instant;

use serde_json::{Map, Value as Json, json};
use sixm_core::{
    FuncRef, Host, ResourceName, ScriptError, ScriptRuntime, WebReply, WebRequest, WebResponse,
};
use sixm_net::{WebExchange, WebResponder};
use tracing::warn;

use crate::manager::ResourceManager;

/// Largest response body a handler may build. Beyond it the response fails with `500`.
const MAX_RESPONSE: usize = 16 * 1024 * 1024;

/// The HTTP state of the server: the handlers and the open requests.
#[derive(Debug, Default)]
pub(crate) struct WebState {
    /// The `SetHttpHandler` function of each resource.
    handlers: RefCell<HashMap<ResourceName, FuncRef>>,
    /// Requests handed to a handler and not yet completed, by id.
    pending: RefCell<HashMap<u64, Pending>>,
    /// The id of the last request handed to a handler.
    last_id: Cell<u64>,
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
        let document: fn(&ResourceManager) -> Json = match request.path.as_str() {
            "/info.json" => Self::info_document,
            "/players.json" => Self::players_document,
            "/dynamic.json" => Self::dynamic_document,
            _ => return None,
        };
        if request.method != "GET" && request.method != "HEAD" {
            return Some(WebResponse {
                status: 405,
                headers: vec![
                    ("allow".to_owned(), "GET, HEAD".to_owned()),
                    (
                        "content-type".to_owned(),
                        "text/plain; charset=utf-8".to_owned(),
                    ),
                ],
                body: b"Method Not Allowed".to_vec(),
            });
        }
        let body = serde_json::to_vec(&document(self)).unwrap_or_default();
        Some(WebResponse {
            status: 200,
            headers: vec![
                ("content-type".to_owned(), "application/json".to_owned()),
                ("cache-control".to_owned(), "no-cache".to_owned()),
            ],
            body,
        })
    }

    /// `/info.json`: the server, its running resources and its server-info convars.
    fn info_document(&self) -> Json {
        let resources: Vec<String> = self
            .snapshot()
            .iter()
            .map(|runtime| runtime.resource().to_string())
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
    fn dynamic_document(&self) -> Json {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_resource_is_the_first_path_segment() {
        assert_eq!(resource_of("/res"), Some("res"));
        assert_eq!(resource_of("/res/a/b"), Some("res"));
        assert_eq!(resource_of("/"), None);
        assert_eq!(resource_of("//x"), None);
        assert_eq!(resource_of("res/x"), None);
    }
}
