//! The HTTP/1.1 listener on the game port's TCP address.
//!
//! A small `hyper` server on its own thread and a current-thread tokio runtime.
//! It reads each request in full, applies the limits below, and hands the
//! request to the owner through a callback together with a [`WebResponder`].
//! The owner answers from its own thread, so the scripts never run here and
//! this thread never waits on them. A connection that gets no answer within
//! [`REQUEST_TIMEOUT`] is closed with `503`.
//!
//! Limits: a body of [`MAX_BODY`] bytes, [`MAX_CONNECTIONS`] open connections,
//! [`HEADER_TIMEOUT`] for the request head, and a token bucket of
//! [`RATE_PER_SECOND`] requests per second per client address, with bursts up to
//! [`RATE_BURST`]. Connections close after one response.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr, TcpListener as StdListener};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::Incoming;
use hyper::header::{CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, HeaderName, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use sixm_core::{WebRequest, WebResponse};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, oneshot};
use tracing::{debug, error};

use crate::error::NetError;

/// Largest request body accepted. A larger one is refused with `413`.
const MAX_BODY: usize = 1024 * 1024;
/// Connections served at once. Further ones are closed as they arrive.
const MAX_CONNECTIONS: usize = 128;
/// Longest a client may take to send the request head.
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest a request may take from its first byte to its answer. A safety net
/// above the longest handler timeout the server allows (120 seconds), so that
/// the server's own `504` is what a client normally sees.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(130);
/// Largest request head buffered by the parser.
const MAX_HEAD: usize = 64 * 1024;
/// Requests per second one client address may sustain.
const RATE_PER_SECOND: f64 = 20.0;
/// Requests one client address may send at once after a quiet period.
const RATE_BURST: f64 = 60.0;
/// Client addresses tracked at once. A new address beyond this is refused.
const MAX_TRACKED_ADDRESSES: usize = 4096;
/// Pause after a failed `accept`, such as running out of file descriptors.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// A request handed to the owner, with the way to answer it.
#[derive(Debug)]
pub struct WebExchange {
    /// The request, with its body read in full.
    pub request: WebRequest,
    /// Sends the answer. Dropping it without sending closes the request with `503`.
    pub responder: WebResponder,
}

/// Sends the answer to one request. Consumed by the send, so one request is
/// answered at most once.
#[derive(Debug)]
pub struct WebResponder {
    sender: oneshot::Sender<WebResponse>,
    peer_ip: IpAddr,
}

impl WebResponder {
    /// The TCP peer's address, as observed by the listener.
    pub fn peer_ip(&self) -> IpAddr {
        self.peer_ip
    }

    /// Sends `response`. Does nothing when the client has already gone.
    pub fn respond(self, response: WebResponse) {
        let _ = self.sender.send(response);
    }
}

/// The HTTP listener. Dropping it closes the listening socket, ends the open
/// connections and joins its thread.
#[derive(Debug)]
pub struct WebServer {
    addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl WebServer {
    /// Binds `addr` for TCP and serves requests on a new thread. `on_request`
    /// runs on that thread for each request and must not block. Fails when the
    /// address cannot be bound.
    pub fn bind(
        addr: SocketAddr,
        on_request: impl Fn(WebExchange) + Send + Sync + 'static,
    ) -> Result<Self, NetError> {
        let listener = StdListener::bind(addr).map_err(|source| NetError::Bind { addr, source })?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let (stop, stop_rx) = oneshot::channel();
        let on_request: Arc<dyn Fn(WebExchange) + Send + Sync> = Arc::new(on_request);
        let thread = thread::Builder::new()
            .name("sixm-web".to_owned())
            .spawn(move || run(listener, &on_request, stop_rx))?;
        Ok(Self {
            addr,
            stop: Some(stop),
            thread: Some(thread),
        })
    }

    /// The address the listener is bound to, with the port resolved.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for WebServer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The listener thread: accepts until stopped.
fn run(
    listener: StdListener,
    on_request: &Arc<dyn Fn(WebExchange) + Send + Sync>,
    stop: oneshot::Receiver<()>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            error!("cannot start the HTTP listener: {err}");
            return;
        }
    };
    runtime.block_on(async {
        let listener = match TcpListener::from_std(listener) {
            Ok(listener) => listener,
            Err(err) => {
                error!("cannot start the HTTP listener: {err}");
                return;
            }
        };
        let limits = Arc::new(RateLimits::default());
        let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        let accept = async {
            loop {
                let (stream, peer) = match listener.accept().await {
                    Ok(accepted) => accepted,
                    Err(err) => {
                        debug!("HTTP accept failed: {err}");
                        tokio::time::sleep(ACCEPT_BACKOFF).await;
                        continue;
                    }
                };
                let Ok(permit) = Arc::clone(&slots).try_acquire_owned() else {
                    drop(stream);
                    continue;
                };
                let on_request = Arc::clone(on_request);
                let limits = Arc::clone(&limits);
                tokio::spawn(async move {
                    let _permit = permit;
                    serve_connection(stream, peer.ip(), on_request, limits).await;
                });
            }
        };
        tokio::select! {
            () = accept => {}
            _ = stop => {}
        }
    });
}

/// Serves one connection: one request, then the connection closes.
async fn serve_connection(
    stream: TcpStream,
    ip: IpAddr,
    on_request: Arc<dyn Fn(WebExchange) + Send + Sync>,
    limits: Arc<RateLimits>,
) {
    let service = service_fn(move |request: Request<Incoming>| {
        let on_request = Arc::clone(&on_request);
        let limits = Arc::clone(&limits);
        async move { Ok::<_, Infallible>(answer_with_timeout(request, ip, &on_request, &limits).await) }
    });
    let mut builder = http1::Builder::new();
    builder.keep_alive(false);
    builder.header_read_timeout(HEADER_TIMEOUT);
    builder.timer(TokioTimer::new());
    builder.max_buf_size(MAX_HEAD);
    if let Err(err) = builder
        .serve_connection(TokioIo::new(stream), service)
        .await
    {
        debug!("HTTP connection from {ip} ended: {err}");
    }
}

async fn answer_with_timeout(
    request: Request<Incoming>,
    ip: IpAddr,
    on_request: &Arc<dyn Fn(WebExchange) + Send + Sync>,
    limits: &RateLimits,
) -> Response<Full<Bytes>> {
    let answer = answer(request, ip, on_request, limits);
    match tokio::time::timeout(REQUEST_TIMEOUT, answer).await {
        Ok(response) => response,
        Err(_) => text_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "The server did not answer in time",
        ),
    }
}

/// Reads the request, hands it to the owner and waits for the answer.
async fn answer(
    request: Request<Incoming>,
    ip: IpAddr,
    on_request: &Arc<dyn Fn(WebExchange) + Send + Sync>,
    limits: &RateLimits,
) -> Response<Full<Bytes>> {
    if !limits.admit(ip, Instant::now()) {
        return text_response(StatusCode::TOO_MANY_REQUESTS, "Too many requests");
    }
    let (parts, body) = request.into_parts();
    if declared_length(&parts.headers).is_some_and(|length| length > MAX_BODY) {
        return text_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "The request body is too large",
        );
    }
    let body = match Limited::new(body, MAX_BODY).collect().await {
        Ok(collected) => collected.to_bytes().to_vec(),
        Err(err) if err.downcast_ref::<LengthLimitError>().is_some() => {
            return text_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "The request body is too large",
            );
        }
        Err(_) => {
            return text_response(
                StatusCode::BAD_REQUEST,
                "The request body could not be read",
            );
        }
    };
    let request = WebRequest {
        method: parts.method.as_str().to_owned(),
        path: parts.uri.path().to_owned(),
        query: parts.uri.query().unwrap_or_default().to_owned(),
        headers: parts
            .headers
            .iter()
            .filter_map(|(name, value)| {
                Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
            })
            .collect(),
        body,
    };
    let (sender, receiver) = oneshot::channel();
    on_request(WebExchange {
        request,
        responder: WebResponder {
            sender,
            peer_ip: ip,
        },
    });
    match receiver.await {
        Ok(response) => into_response(response),
        Err(_) => text_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "The request could not be answered",
        ),
    }
}

/// The `Content-Length` a request declares, when it is valid.
fn declared_length(headers: &hyper::HeaderMap) -> Option<usize> {
    headers.get(CONTENT_LENGTH)?.to_str().ok()?.parse().ok()
}

/// Turns the owner's answer into a response. Headers the listener manages
/// itself are dropped, and an invalid status or header gives `500`.
fn into_response(response: WebResponse) -> Response<Full<Bytes>> {
    let Ok(status) = StatusCode::from_u16(response.status) else {
        return text_response(StatusCode::INTERNAL_SERVER_ERROR, "Invalid status code");
    };
    let mut builder = Response::builder().status(status);
    for (name, value) in &response.headers {
        if matches!(
            name.to_ascii_lowercase().as_str(),
            "content-length" | "transfer-encoding" | "connection"
        ) {
            continue;
        }
        let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) else {
            return text_response(StatusCode::INTERNAL_SERVER_ERROR, "Invalid response header");
        };
        builder = builder.header(name, value);
    }
    builder
        .body(Full::new(Bytes::from(response.body)))
        .unwrap_or_else(|_| text_response(StatusCode::INTERNAL_SERVER_ERROR, "Invalid response"))
}

/// A short plain-text response built by the listener itself.
fn text_response(status: StatusCode, message: &'static str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(CONNECTION, "close")
        .body(Full::new(Bytes::from_static(message.as_bytes())))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
}

/// Token buckets by client address.
#[derive(Debug, Default)]
struct RateLimits {
    buckets: Mutex<HashMap<IpAddr, Bucket>>,
}

/// One client address's bucket: its tokens at `at`.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    at: Instant,
}

impl Bucket {
    /// The tokens the bucket holds at `now`, without changing it.
    fn refilled(self, now: Instant) -> f64 {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        (self.tokens + elapsed * RATE_PER_SECOND).min(RATE_BURST)
    }
}

impl RateLimits {
    /// Takes one token for `ip`. Returns `false` when it has none, or when the
    /// address is new and the table is full of addresses still in use.
    fn admit(&self, ip: IpAddr, now: Instant) -> bool {
        let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
        if !buckets.contains_key(&ip) && buckets.len() >= MAX_TRACKED_ADDRESSES {
            // A bucket that has refilled to the burst is the same as a new one.
            buckets.retain(|_, bucket| bucket.refilled(now) < RATE_BURST);
            if buckets.len() >= MAX_TRACKED_ADDRESSES {
                return false;
            }
        }
        let bucket = buckets.entry(ip).or_insert(Bucket {
            tokens: RATE_BURST,
            at: now,
        });
        let tokens = bucket.refilled(now);
        *bucket = Bucket { tokens, at: now };
        if tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_is_admitted_then_refused_until_refilled() {
        let limits = RateLimits::default();
        let ip: IpAddr = "192.0.2.1".parse().expect("address");
        let start = Instant::now();
        let admitted = (0..100).filter(|_| limits.admit(ip, start)).count();
        // The burst of 60 and the 20 tokens a second, as the constants say.
        assert_eq!(admitted, 60);
        let later = start + Duration::from_secs(1);
        let refilled = (0..100).filter(|_| limits.admit(ip, later)).count();
        assert_eq!(refilled, 20);
    }

    #[test]
    fn clients_have_separate_buckets() {
        let limits = RateLimits::default();
        let start = Instant::now();
        let first: IpAddr = "192.0.2.1".parse().expect("address");
        let second: IpAddr = "192.0.2.2".parse().expect("address");
        while limits.admit(first, start) {}
        assert!(limits.admit(second, start));
    }

    fn address(index: u32) -> IpAddr {
        IpAddr::V4(std::net::Ipv4Addr::from(index))
    }

    #[test]
    fn idle_addresses_expire_so_the_table_stays_bounded() {
        let limits = RateLimits::default();
        let start = Instant::now();
        for index in 0..MAX_TRACKED_ADDRESSES {
            assert!(limits.admit(address(u32::try_from(index).expect("index")), start));
        }
        // Every address is still in use, so a new one is refused.
        assert!(!limits.admit(address(u32::MAX), start));
        // Once they have refilled they are idle, and the new address fits.
        let later = start + Duration::from_secs(10);
        assert!(limits.admit(address(u32::MAX), later));
        let tracked = limits.buckets.lock().expect("lock").len();
        assert_eq!(tracked, 1);
    }

    #[test]
    fn invalid_response_headers_become_a_server_error() {
        let response = into_response(WebResponse {
            status: 200,
            headers: vec![("bad name".to_owned(), "x".to_owned())],
            body: Vec::new(),
        });
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
