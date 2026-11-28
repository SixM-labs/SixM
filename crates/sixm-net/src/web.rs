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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
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

/// A parsed IPv4 or IPv6 CIDR range for trusted reverse-proxy matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpCidr {
    /// IPv4 network prefix (`0..=32`).
    V4 { network: u32, mask: u32 },
    /// IPv6 network prefix (`0..=128`).
    V6 { network: u128, mask: u128 },
}

impl IpCidr {
    /// Parses a CIDR (`10.0.0.0/8`, `fd00::/8`) or bare IP (`/32`, `/128`).
    pub fn parse(spec: &str) -> Result<Self, &'static str> {
        let trimmed = spec.trim();
        if trimmed.is_empty() {
            return Err("empty CIDR");
        }
        let (addr_str, prefix_opt) = match trimmed.split_once('/') {
            Some((addr, prefix)) => (addr, Some(prefix)),
            None => (trimmed, None),
        };
        let ip: IpAddr = addr_str.parse().map_err(|_| "invalid IP address")?;
        match ip {
            IpAddr::V4(v4) => {
                let prefix = match prefix_opt {
                    Some(raw) => raw.parse::<u32>().map_err(|_| "invalid prefix length")?,
                    None => 32,
                };
                if prefix > 32 {
                    return Err("IPv4 prefix exceeds 32");
                }
                let mask = if prefix == 0 {
                    0
                } else {
                    u32::MAX << (32 - prefix)
                };
                Ok(Self::V4 {
                    network: u32::from_be_bytes(v4.octets()) & mask,
                    mask,
                })
            }
            IpAddr::V6(v6) => {
                let prefix = match prefix_opt {
                    Some(raw) => raw.parse::<u32>().map_err(|_| "invalid prefix length")?,
                    None => 128,
                };
                if prefix > 128 {
                    return Err("IPv6 prefix exceeds 128");
                }
                let mask = if prefix == 0 {
                    0
                } else {
                    u128::MAX << (128 - prefix)
                };
                Ok(Self::V6 {
                    network: u128::from_be_bytes(v6.octets()) & mask,
                    mask,
                })
            }
        }
    }

    /// Returns whether `ip` belongs to this CIDR range.
    pub fn contains(&self, ip: IpAddr) -> bool {
        let normalized = normalize_ip(ip);
        match (self, normalized) {
            (Self::V4 { network, mask }, IpAddr::V4(v4)) => {
                (u32::from_be_bytes(v4.octets()) & *mask) == *network
            }
            (Self::V6 { network, mask }, IpAddr::V6(v6)) => {
                (u128::from_be_bytes(v6.octets()) & *mask) == *network
            }
            (Self::V6 { network, mask }, IpAddr::V4(v4)) => {
                let mapped = v4.to_ipv6_mapped();
                (u128::from_be_bytes(mapped.octets()) & *mask) == *network
            }
            _ => false,
        }
    }
}

fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        IpAddr::V4(v4) => IpAddr::V4(v4),
    }
}

/// Shared store of trusted reverse-proxy CIDRs (`sv_proxyIPRanges`).
///
/// Cloning shares the same underlying rule set. When no CIDRs are configured,
/// [`resolve_ip`](Self::resolve_ip) skips locking and returns the TCP peer IP.
#[derive(Debug, Clone, Default)]
pub struct TrustedProxies {
    has_ranges: Arc<AtomicBool>,
    ranges: Arc<RwLock<Vec<IpCidr>>>,
}

impl TrustedProxies {
    /// Creates an empty trusted-proxy store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the trusted ranges with `ranges`.
    pub fn set_ranges(&self, ranges: Vec<IpCidr>) {
        let mut guard = self.ranges.write().unwrap_or_else(PoisonError::into_inner);
        *guard = ranges;
        self.has_ranges.store(!guard.is_empty(), Ordering::Release);
    }

    /// Parses comma/whitespace-separated CIDR tokens, applies valid entries, and
    /// returns any invalid tokens.
    pub fn parse_and_set(&self, raw: &str) -> Vec<String> {
        let mut valid = Vec::new();
        let mut invalid = Vec::new();
        for token in raw.split(|c: char| c == ',' || c.is_ascii_whitespace()) {
            let trimmed = token.trim();
            if trimmed.is_empty() {
                continue;
            }
            match IpCidr::parse(trimmed) {
                Ok(cidr) => valid.push(cidr),
                Err(_) => invalid.push(trimmed.to_owned()),
            }
        }
        self.set_ranges(valid);
        invalid
    }

    /// Resolves the effective client IP for an HTTP request from `peer_ip`.
    ///
    /// Proxy headers (`X-Forwarded-For`, `X-Real-IP`) are honored only when
    /// `peer_ip` matches a configured CIDR.
    pub fn resolve_ip<'a>(
        &self,
        peer_ip: IpAddr,
        headers: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> IpAddr {
        let peer_norm = normalize_ip(peer_ip);
        if !self.has_ranges.load(Ordering::Acquire) {
            return peer_norm;
        }
        let ranges = self.ranges.read().unwrap_or_else(PoisonError::into_inner);
        if !ranges.iter().any(|cidr| cidr.contains(peer_norm)) {
            return peer_norm;
        }

        let mut xff_tokens: Vec<&str> = Vec::new();
        let mut real_ip: Option<&str> = None;
        for (name, value) in headers {
            if name.eq_ignore_ascii_case("x-forwarded-for") {
                for part in value.split(',') {
                    let trimmed = part.trim();
                    if !trimmed.is_empty() {
                        xff_tokens.push(trimmed);
                    }
                }
            } else if name.eq_ignore_ascii_case("x-real-ip") && real_ip.is_none() {
                let trimmed = value.trim();
                if !trimmed.is_empty() {
                    real_ip = Some(trimmed);
                }
            }
        }

        // Walk X-Forwarded-For right-to-left: stop on malformed entries and select
        // the rightmost valid IP not matching a trusted proxy CIDR.
        let mut leftmost_trusted: Option<IpAddr> = None;
        for token in xff_tokens.iter().rev() {
            let Ok(parsed) = token.parse::<IpAddr>() else {
                leftmost_trusted = None;
                break;
            };
            let ip = normalize_ip(parsed);
            if ranges.iter().any(|cidr| cidr.contains(ip)) {
                leftmost_trusted = Some(ip);
            } else {
                return ip;
            }
        }
        if let Some(ip) = leftmost_trusted {
            return ip;
        }

        if let Some(raw_real) = real_ip
            && let Ok(parsed) = raw_real.parse::<IpAddr>()
        {
            return normalize_ip(parsed);
        }

        peer_norm
    }
}

/// Sends the answer to one request. Consumed by the send, so one request is
/// answered at most once.
#[derive(Debug)]
pub struct WebResponder {
    sender: oneshot::Sender<WebResponse>,
    peer_ip: IpAddr,
    client_ip: IpAddr,
}

impl WebResponder {
    /// The TCP peer's address, as observed by the listener.
    pub fn peer_ip(&self) -> IpAddr {
        self.peer_ip
    }

    /// The resolved client IP address (honoring [`TrustedProxies`] when configured).
    pub fn client_ip(&self) -> IpAddr {
        self.client_ip
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
    proxies: TrustedProxies,
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
        Self::bind_with_proxies(addr, TrustedProxies::new(), on_request)
    }

    /// Binds `addr` for TCP using shared [`TrustedProxies`] for client IP attribution.
    pub fn bind_with_proxies(
        addr: SocketAddr,
        proxies: TrustedProxies,
        on_request: impl Fn(WebExchange) + Send + Sync + 'static,
    ) -> Result<Self, NetError> {
        let listener = StdListener::bind(addr).map_err(|source| NetError::Bind { addr, source })?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let (stop, stop_rx) = oneshot::channel();
        let on_request: Arc<dyn Fn(WebExchange) + Send + Sync> = Arc::new(on_request);
        let thread_proxies = proxies.clone();
        let thread = thread::Builder::new()
            .name("sixm-web".to_owned())
            .spawn(move || run(listener, &on_request, &thread_proxies, stop_rx))?;
        Ok(Self {
            addr,
            proxies,
            stop: Some(stop),
            thread: Some(thread),
        })
    }

    /// The trusted-proxy store consulted for client IP attribution.
    pub fn trusted_proxies(&self) -> &TrustedProxies {
        &self.proxies
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
    proxies: &TrustedProxies,
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
                let proxies = proxies.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    serve_connection(stream, peer.ip(), on_request, limits, proxies).await;
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
    proxies: TrustedProxies,
) {
    let service = service_fn(move |request: Request<Incoming>| {
        let on_request = Arc::clone(&on_request);
        let limits = Arc::clone(&limits);
        let proxies = proxies.clone();
        async move {
            Ok::<_, Infallible>(
                answer_with_timeout(request, ip, &on_request, &limits, &proxies).await,
            )
        }
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
    proxies: &TrustedProxies,
) -> Response<Full<Bytes>> {
    let answer = answer(request, ip, on_request, limits, proxies);
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
    proxies: &TrustedProxies,
) -> Response<Full<Bytes>> {
    let (parts, body) = request.into_parts();
    let client_ip = proxies.resolve_ip(
        ip,
        parts
            .headers
            .iter()
            .filter_map(|(name, value)| Some((name.as_str(), value.to_str().ok()?))),
    );
    if !limits.admit(client_ip, Instant::now()) {
        return text_response(StatusCode::TOO_MANY_REQUESTS, "Too many requests");
    }
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
            client_ip,
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

    #[test]
    fn ip_cidr_parses_masks_and_normalizes_ipv4_mapped_v6() {
        let net4 = IpCidr::parse("10.20.30.40/8").expect("v4 cidr");
        assert!(net4.contains("10.0.0.1".parse().unwrap()));
        assert!(net4.contains("::ffff:10.99.1.2".parse().unwrap()));
        assert!(!net4.contains("11.0.0.1".parse().unwrap()));

        let bare = IpCidr::parse("127.0.0.1").expect("bare v4");
        assert!(bare.contains("127.0.0.1".parse().unwrap()));
        assert!(!bare.contains("127.0.0.2".parse().unwrap()));

        let net6 = IpCidr::parse("fd00::/8").expect("v6 cidr");
        assert!(net6.contains("fd12:3456::1".parse().unwrap()));
        assert!(!net6.contains("fe80::1".parse().unwrap()));

        assert!(IpCidr::parse("10.0.0.0/33").is_err());
        assert!(IpCidr::parse("::1/129").is_err());
        assert!(IpCidr::parse("not-an-ip").is_err());
    }

    #[test]
    fn trusted_proxies_honors_rightmost_untrusted_hop_only_from_listed_cidrs() {
        let proxies = TrustedProxies::new();
        let peer: IpAddr = "127.0.0.1".parse().unwrap();
        let untrusted_peer: IpAddr = "198.51.100.5".parse().unwrap();

        // Disabled by default: ignores forwarded headers.
        assert_eq!(
            proxies.resolve_ip(peer, [("x-forwarded-for", "203.0.113.9")]),
            peer
        );

        let invalid = proxies.parse_and_set("127.0.0.1/32, bogus/99, 10.0.0.0/8");
        assert_eq!(invalid, vec!["bogus/99"]);

        // Untrusted peer still cannot spoof headers.
        assert_eq!(
            proxies.resolve_ip(untrusted_peer, [("x-forwarded-for", "203.0.113.9")]),
            untrusted_peer
        );

        // Trusted peer selects rightmost untrusted hop, ignoring spoofed left hops.
        assert_eq!(
            proxies.resolve_ip(
                peer,
                [("x-forwarded-for", "198.51.100.1, 203.0.113.42, 10.1.2.3")]
            ),
            "203.0.113.42".parse::<IpAddr>().unwrap()
        );

        // Malformed entry aborts XFF walk cleanly and falls back safely.
        assert_eq!(
            proxies.resolve_ip(
                peer,
                [
                    ("x-forwarded-for", "203.0.113.1, evil, 10.1.2.3"),
                    ("x-real-ip", "203.0.113.77"),
                ]
            ),
            "203.0.113.77".parse::<IpAddr>().unwrap()
        );
    }
}
