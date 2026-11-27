//! Outgoing HTTP requests for `PerformHttpRequest`.
//!
//! A fixed pool of worker threads runs the requests, with a blocking client
//! (ureq on rustls with ring and the Mozilla root certificates). The pool is
//! built when the first request starts, with `sixm_httpWorkers` threads, and is
//! fed through a bounded queue, so a burst of requests costs no extra threads
//! and a flood is refused instead of buffered without limit. A worker queues
//! each outcome, and the frame loop delivers it to the script's callback on the
//! main thread. A resource may have [`MAX_IN_FLIGHT`] requests open at once.
//!
//! When a resource stops, its requests are cancelled: those still waiting in the
//! queue are skipped, and the outcomes of those already running are dropped
//! without touching the stopped runtime.
//!
//! Completions are queued rather than sent through the loop's input channel,
//! because scripts run before the loop starts and so before that channel exists.

use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use sixm_core::{FuncRef, HttpRequest, ResourceName, ScriptError, Value};
use tracing::debug;
use ureq::Agent;
use ureq::config::Config;
use ureq::http::{self, Method, Uri};
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};

/// Most requests one resource may have open at once.
pub(crate) const MAX_IN_FLIGHT: usize = 64;
/// Most requests waiting for a free worker, over all resources.
const QUEUE_CAPACITY: usize = 256;
/// Largest response body kept. A larger response fails the request.
pub(crate) const MAX_BODY: u64 = 16 * 1024 * 1024;
/// Most redirects followed before a request fails.
const MAX_REDIRECTS: u32 = 5;

/// The outcome of one request, waiting to be delivered to its callback.
#[derive(Debug)]
pub(crate) struct Completion {
    pub(crate) resource: ResourceName,
    pub(crate) callback: FuncRef,
    pub(crate) outcome: Outcome,
}

/// How a request ended.
#[derive(Debug)]
pub(crate) enum Outcome {
    /// The server answered, whatever the status code.
    Response {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    /// The request failed before a complete response arrived.
    Failed(String),
}

impl Outcome {
    /// The callback arguments: `(statusCode, body, headers, errorData)`.
    pub(crate) fn into_args(self) -> Vec<Value> {
        let (status, body, headers, error) = match self {
            Self::Response {
                status,
                headers,
                body,
            } => (i64::from(status), body, headers, String::new()),
            Self::Failed(message) => (0, Vec::new(), Vec::new(), message),
        };
        let headers = headers
            .into_iter()
            .map(|(name, value)| {
                (
                    Value::String(name.into_bytes().into_boxed_slice()),
                    Value::String(value.into_bytes().into_boxed_slice()),
                )
            })
            .collect();
        vec![
            Value::Int(status),
            Value::String(body.into_boxed_slice()),
            Value::Map(headers),
            Value::String(error.into_bytes().into_boxed_slice()),
        ]
    }
}

/// Where a completed HTTP request reports.
#[derive(Debug)]
enum JobTarget {
    Resource {
        owner: ResourceName,
        callback: FuncRef,
        /// Set when the owner stops, so a worker skips the request.
        cancelled: Arc<AtomicBool>,
    },
    FireAndForget,
}

/// One request waiting for a worker.
#[derive(Debug)]
struct Job {
    method: Method,
    uri: Uri,
    request: HttpRequest,
    timeout: Duration,
    allow_private: bool,
    target: JobTarget,
}

/// The requests one start of a resource has open.
#[derive(Debug)]
struct Slots {
    open: usize,
    cancelled: Arc<AtomicBool>,
}

/// The pool's side of the queue. Dropping it ends the workers.
#[derive(Debug)]
struct Pool {
    queue: SyncSender<Job>,
}

/// Where workers put finished requests.
type Finished = Arc<Mutex<Vec<Completion>>>;

/// The HTTP client of a server: the open requests and the finished ones.
#[derive(Debug, Default)]
pub(crate) struct HttpClient {
    /// Built on first use.
    pool: OnceCell<Pool>,
    /// Open requests by resource and start of that resource (`FuncRef::instance`).
    in_flight: RefCell<HashMap<(ResourceName, u64), Slots>>,
    /// Finished requests, filled by the workers.
    finished: Finished,
}

impl HttpClient {
    /// Starts `request` for `resource`. `callback` receives its outcome from a
    /// later frame. `timeout` applies unless the request has its own, and
    /// `workers` sizes the pool if this is the first request. When
    /// `allow_private` is `false`, loopback, link-local, and private IP
    /// targets (including after DNS resolution or redirects) fail safely. Fails,
    /// without starting anything, when the URL is not an `http` or `https`
    /// address, the method is not valid, the resource already has
    /// [`MAX_IN_FLIGHT`] requests open, or [`QUEUE_CAPACITY`] requests already
    /// wait for a worker.
    pub(crate) fn start(
        &self,
        resource: &ResourceName,
        request: HttpRequest,
        callback: FuncRef,
        timeout: Duration,
        workers: usize,
        allow_private: bool,
    ) -> Result<(), ScriptError> {
        let uri = parse_http_uri(&request.url)?;
        let method = Method::from_bytes(request.method.as_bytes()).map_err(|_| {
            ScriptError::Runtime(format!("`{}` is not an HTTP method", request.method))
        })?;
        let timeout = request.timeout.unwrap_or(timeout);
        let instance = callback.instance;
        let cancelled = self.reserve(resource, instance)?;

        let queued = self.enqueue(
            workers,
            Job {
                method,
                uri,
                request,
                timeout,
                allow_private,
                target: JobTarget::Resource {
                    owner: resource.clone(),
                    callback,
                    cancelled,
                },
            },
        );
        if queued.is_err() {
            self.release(resource, instance);
        }
        queued
    }

    /// Queues a fire-and-forget JSON `POST` to `url` without needing a script
    /// callback. Failures are logged at `debug` level by the worker.
    pub(crate) fn post_fire_and_forget(
        &self,
        url: &str,
        body: Vec<u8>,
        timeout: Duration,
        workers: usize,
        allow_private: bool,
    ) -> Result<(), ScriptError> {
        let uri = parse_http_uri(url)?;
        let request = HttpRequest {
            url: url.to_owned(),
            method: "POST".to_owned(),
            headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
            body: Some(body),
            timeout: Some(timeout),
        };
        self.enqueue(
            workers,
            Job {
                method: Method::POST,
                uri,
                request,
                timeout,
                allow_private,
                target: JobTarget::FireAndForget,
            },
        )
    }

    fn enqueue(&self, workers: usize, job: Job) -> Result<(), ScriptError> {
        self.pool(workers).and_then(|pool| {
            pool.queue.try_send(job).map_err(|err| match err {
                TrySendError::Full(_) => ScriptError::Runtime(format!(
                    "{QUEUE_CAPACITY} HTTP requests are already waiting for a worker"
                )),
                TrySendError::Disconnected(_) => {
                    ScriptError::Runtime("the HTTP workers have stopped".to_owned())
                }
            })
        })
    }

    /// Takes every finished request, oldest first. Empty when none finished,
    /// and then it does not allocate.
    pub(crate) fn take_finished(&self) -> Vec<Completion> {
        std::mem::take(&mut *self.finished.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Marks one request of `resource`, started as `instance`, as delivered or
    /// dropped, freeing its slot.
    pub(crate) fn finish(&self, resource: &ResourceName, instance: u64) {
        self.release(resource, instance);
    }

    /// Cancels every request of `resource` started as `instance`, because that
    /// start has stopped. Requests still waiting for a worker never run; the
    /// ones already running finish, and delivery drops their outcome.
    pub(crate) fn discard(&self, resource: &ResourceName, instance: u64) {
        let removed = self
            .in_flight
            .borrow_mut()
            .remove(&(resource.clone(), instance));
        if let Some(slots) = removed {
            slots.cancelled.store(true, Ordering::Release);
        }
    }

    /// The pool, built with `workers` threads on first use.
    fn pool(&self, workers: usize) -> Result<&Pool, ScriptError> {
        if let Some(pool) = self.pool.get() {
            return Ok(pool);
        }
        let (queue, jobs) = mpsc::sync_channel(QUEUE_CAPACITY);
        let jobs = Arc::new(Mutex::new(jobs));
        let agents = Agents::new();
        let mut last_error = None;
        let mut started = 0_usize;
        for index in 0..workers.max(1) {
            let jobs = Arc::clone(&jobs);
            let agents = agents.clone();
            let finished = Arc::clone(&self.finished);
            let spawned = thread::Builder::new()
                .name(format!("sixm-http-{index}"))
                .spawn(move || work(&jobs, &agents, &finished));
            match spawned {
                Ok(_) => started += 1,
                Err(err) => last_error = Some(err),
            }
        }
        if let (0, Some(err)) = (started, last_error) {
            return Err(ScriptError::Runtime(format!(
                "cannot start the HTTP workers: {err}"
            )));
        }
        Ok(self.pool.get_or_init(|| Pool { queue }))
    }

    /// Counts one more open request of `resource` and returns its cancel flag.
    fn reserve(
        &self,
        resource: &ResourceName,
        instance: u64,
    ) -> Result<Arc<AtomicBool>, ScriptError> {
        let mut all = self.in_flight.borrow_mut();
        let slots = all
            .entry((resource.clone(), instance))
            .or_insert_with(|| Slots {
                open: 0,
                cancelled: Arc::new(AtomicBool::new(false)),
            });
        if slots.open >= MAX_IN_FLIGHT {
            return Err(ScriptError::Runtime(format!(
                "resource `{resource}` already has {MAX_IN_FLIGHT} HTTP requests in flight"
            )));
        }
        slots.open += 1;
        Ok(Arc::clone(&slots.cancelled))
    }

    fn release(&self, resource: &ResourceName, instance: u64) {
        let mut all = self.in_flight.borrow_mut();
        let key = (resource.clone(), instance);
        if let Some(slots) = all.get_mut(&key) {
            slots.open = slots.open.saturating_sub(1);
            if slots.open == 0 {
                all.remove(&key);
            }
        }
    }
}

/// A worker: takes jobs until the pool is dropped, skipping cancelled ones.
fn work(jobs: &Mutex<Receiver<Job>>, agents: &Agents, finished: &Mutex<Vec<Completion>>) {
    while let Some(job) = next_job(jobs) {
        if let JobTarget::Resource { ref cancelled, .. } = job.target
            && cancelled.load(Ordering::Acquire)
        {
            continue;
        }
        let url_for_log = match job.target {
            JobTarget::FireAndForget => Some(job.request.url.clone()),
            JobTarget::Resource { .. } => None,
        };
        let outcome = perform(
            agents,
            &job.method,
            &job.uri,
            job.request,
            job.timeout,
            job.allow_private,
        );
        match job.target {
            JobTarget::Resource {
                owner, callback, ..
            } => {
                finished
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(Completion {
                        resource: owner,
                        callback,
                        outcome,
                    });
            }
            JobTarget::FireAndForget => match outcome {
                Outcome::Response { status, .. } if status >= 400 => {
                    debug!(
                        "heartbeat POST `{}` returned HTTP {status}",
                        url_for_log.as_deref().unwrap_or("")
                    );
                }
                Outcome::Response { .. } => {}
                Outcome::Failed(err) => {
                    debug!(
                        "heartbeat POST `{}` failed: {err}",
                        url_for_log.as_deref().unwrap_or("")
                    );
                }
            },
        }
    }
}

fn parse_http_uri(url: &str) -> Result<Uri, ScriptError> {
    let uri: Uri = url.parse().map_err(|_| invalid_url(url))?;
    if !matches!(uri.scheme_str(), Some("http" | "https")) || uri.host().is_none() {
        return Err(invalid_url(url));
    }
    Ok(uri)
}

/// Waits for the next job. `None` when the pool has been dropped. The queue's
/// lock is held only while waiting, so the other workers wait behind it.
fn next_job(jobs: &Mutex<Receiver<Job>>) -> Option<Job> {
    let receiver = jobs.lock().unwrap_or_else(PoisonError::into_inner);
    receiver.recv().ok()
}

fn invalid_url(url: &str) -> ScriptError {
    ScriptError::Runtime(format!("`{url}` is not an absolute http or https URL"))
}

/// Shared worker agents: `public` pins DNS resolution so forbidden private or
/// loopback IPs can never be connected to (closing DNS rebinding), while
/// `private` allows local targets when `sv_httpAllowPrivate` is enabled.
#[derive(Clone)]
struct Agents {
    public: Agent,
    private: Agent,
}

impl Agents {
    fn new() -> Self {
        let config = || {
            Agent::config_builder()
                .http_status_as_error(false)
                .max_redirects(0)
                .build()
        };
        Self {
            public: Agent::with_parts(
                config(),
                DefaultConnector::default(),
                SafePublicResolver::default(),
            ),
            private: Agent::new_with_config(config()),
        }
    }
}

#[derive(Debug, Default)]
struct SafePublicResolver(DefaultResolver);

impl Resolver for SafePublicResolver {
    fn resolve(
        &self,
        uri: &Uri,
        config: &Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        check_target_syntax(uri, false).map_err(|message| {
            ureq::Error::Io(io::Error::new(io::ErrorKind::PermissionDenied, message))
        })?;
        let addrs = self.0.resolve(uri, config, timeout)?;
        let host = uri.host().unwrap_or("");
        for addr in &addrs {
            if is_forbidden_ip(addr.ip()) {
                return Err(ureq::Error::Io(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("refused private or loopback HTTP target `{host}`"),
                )));
            }
        }
        Ok(addrs)
    }
}

/// Runs one request to completion on the calling thread, validating initial and
/// redirected targets when `allow_private` is `false`.
fn perform(
    agents: &Agents,
    method: &Method,
    uri: &Uri,
    request: HttpRequest,
    timeout: Duration,
    allow_private: bool,
) -> Outcome {
    let deadline = Instant::now() + timeout;
    let agent = if allow_private {
        &agents.private
    } else {
        &agents.public
    };
    let mut method = method.clone();
    let mut uri = uri.clone();
    let mut headers = request.headers;
    let mut body = request.body.unwrap_or_default();

    for hop in 0..=MAX_REDIRECTS {
        if let Err(msg) = check_target_syntax(&uri, allow_private) {
            return Outcome::Failed(msg);
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Outcome::Failed("request timed out".to_owned());
        };
        if remaining.is_zero() {
            return Outcome::Failed("request timed out".to_owned());
        }

        let mut builder = http::Request::builder()
            .method(method.clone())
            .uri(uri.clone());
        for (name, value) in &headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let built = match builder.body(body.clone()) {
            Ok(built) => built,
            Err(err) => return Outcome::Failed(format!("invalid request: {err}")),
        };
        let built = agent
            .configure_request(built)
            .timeout_global(Some(remaining))
            .build();
        let mut response = match agent.run(built) {
            Ok(response) => response,
            Err(err) => return Outcome::Failed(err.to_string()),
        };

        let status = response.status().as_u16();
        if is_redirect(status)
            && let Some(location) = response.headers().get(http::header::LOCATION)
        {
            let Ok(location) = location.to_str() else {
                return Outcome::Failed("redirect target is not valid UTF-8".to_owned());
            };
            if hop == MAX_REDIRECTS {
                return Outcome::Failed(format!("too many redirects (>{MAX_REDIRECTS})"));
            }
            let next_uri = match resolve_redirect(&uri, location) {
                Ok(next) => next,
                Err(msg) => return Outcome::Failed(msg),
            };
            let cross_origin = !same_origin(&uri, &next_uri);
            apply_redirect_semantics(status, &mut method, &mut body, &mut headers, cross_origin);
            uri = next_uri;
            continue;
        }

        let response_headers = collect_headers(&response);
        return match response
            .body_mut()
            .with_config()
            .limit(MAX_BODY)
            .read_to_vec()
        {
            Ok(body) => Outcome::Response {
                status,
                headers: response_headers,
                body,
            },
            Err(err) => Outcome::Failed(err.to_string()),
        };
    }

    Outcome::Failed(format!("too many redirects (>{MAX_REDIRECTS})"))
}

fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn collect_headers(response: &http::Response<ureq::Body>) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = Vec::new();
    for (name, value) in response.headers() {
        let Ok(value) = value.to_str() else {
            continue;
        };
        match headers.iter_mut().find(|(known, _)| known == name.as_str()) {
            Some((_, joined)) => {
                joined.push_str(", ");
                joined.push_str(value);
            }
            None => headers.push((name.as_str().to_owned(), value.to_owned())),
        }
    }
    headers
}

fn check_target_syntax(uri: &Uri, allow_private: bool) -> Result<(), String> {
    if !matches!(uri.scheme_str(), Some("http" | "https")) {
        return Err(format!("`{uri}` is not an absolute http or https URL"));
    }
    let Some(host) = uri.host() else {
        return Err(format!("`{uri}` has no host"));
    };
    if allow_private {
        return Ok(());
    }
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let lower = host.to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".localhost") {
        return Err(format!("refused private or loopback HTTP target `{host}`"));
    }
    if let Ok(ip) = host.parse::<IpAddr>()
        && is_forbidden_ip(ip)
    {
        return Err(format!("refused private or loopback HTTP target `{host}`"));
    }
    Ok(())
}

fn is_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 0
                || v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_documentation()
                || (o[0] == 100 && (o[1] & 0b1100_0000) == 64)
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                || (o[0] == 198 && (o[1] & 0b1111_1110) == 18)
                || o[0] >= 240
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_forbidden_ip(IpAddr::V4(v4));
            }
            let seg = v6.segments();
            if seg[..6] == [0; 6] && !v6.is_unspecified() && !v6.is_loopback() {
                let [a, b] = seg[6].to_be_bytes();
                let [c, d] = seg[7].to_be_bytes();
                return is_forbidden_ip(IpAddr::V4(Ipv4Addr::new(a, b, c, d)));
            }
            v6.is_unspecified()
                || v6.is_loopback()
                || v6.is_unicast_link_local()
                || v6.is_unique_local()
                || v6.is_multicast()
                || (seg[0] == 0x2001 && seg[1] == 0x0db8)
        }
    }
}

fn default_port(uri: &Uri) -> u16 {
    if uri.scheme_str() == Some("https") {
        443
    } else {
        80
    }
}

fn same_origin(a: &Uri, b: &Uri) -> bool {
    a.scheme_str() == b.scheme_str()
        && a.host()
            .zip(b.host())
            .is_some_and(|(ah, bh)| ah.eq_ignore_ascii_case(bh))
        && a.port_u16().unwrap_or_else(|| default_port(a))
            == b.port_u16().unwrap_or_else(|| default_port(b))
}

fn apply_redirect_semantics(
    status: u16,
    method: &mut Method,
    body: &mut Vec<u8>,
    headers: &mut Vec<(String, String)>,
    cross_origin: bool,
) {
    if status == 303 && *method != Method::GET && *method != Method::HEAD
        || matches!(status, 301 | 302) && *method == Method::POST
    {
        *method = Method::GET;
        body.clear();
        headers.retain(|(name, _)| {
            !name.eq_ignore_ascii_case("content-length")
                && !name.eq_ignore_ascii_case("content-type")
        });
    }
    if cross_origin {
        headers.retain(|(name, _)| {
            !name.eq_ignore_ascii_case("authorization")
                && !name.eq_ignore_ascii_case("proxy-authorization")
                && !name.eq_ignore_ascii_case("cookie")
        });
    }
}

fn resolve_redirect(base: &Uri, location: &str) -> Result<Uri, String> {
    let trimmed = location.trim().split('#').next().unwrap_or("").trim();
    if trimmed.is_empty() {
        return Err("redirect target is empty".to_owned());
    }
    if trimmed.starts_with("//") {
        let scheme = base.scheme_str().unwrap_or("http");
        let parsed: Uri = format!("{scheme}:{trimmed}")
            .parse()
            .map_err(|_| format!("invalid redirect URL `{location}`"))?;
        return validate_redirect_uri(parsed);
    }
    if let Ok(parsed) = trimmed.parse::<Uri>()
        && parsed.scheme_str().is_some()
    {
        return validate_redirect_uri(parsed);
    }
    let scheme = base.scheme_str().unwrap_or("http");
    let authority = base
        .authority()
        .ok_or_else(|| "base URL has no authority".to_owned())?
        .as_str();
    let path_and_query = if trimmed.starts_with('/') {
        trimmed.to_owned()
    } else if trimmed.starts_with('?') {
        format!("{}{trimmed}", base.path())
    } else {
        let prefix = match base.path().rfind('/') {
            Some(index) => &base.path()[..=index],
            None => "/",
        };
        format!("{prefix}{trimmed}")
    };
    let parsed: Uri = format!("{scheme}://{authority}{path_and_query}")
        .parse()
        .map_err(|_| format!("invalid redirect URL `{location}`"))?;
    validate_redirect_uri(parsed)
}

fn validate_redirect_uri(uri: Uri) -> Result<Uri, String> {
    if !matches!(uri.scheme_str(), Some("http" | "https")) || uri.host().is_none() {
        return Err(format!("`{uri}` is not an absolute http or https URL"));
    }
    Ok(uri)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Instant;

    use super::*;

    /// Pool size used by the tests.
    const WORKERS: usize = 2;

    /// Serves one connection with `response` after reading the request head.
    /// `delay` keeps the connection silent first. Returns the URL of the server.
    fn serve(response: Vec<u8>, delay: Duration) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut head = Vec::new();
            let mut chunk = [0_u8; 1024];
            while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => return,
                    Ok(read) => head.extend_from_slice(&chunk[..read]),
                }
            }
            thread::sleep(delay);
            let _ = stream.write_all(&response);
        });
        format!("http://127.0.0.1:{port}/")
    }

    fn ok_response(body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 201 Created\r\nContent-Type: text/plain\r\nX-Multi: a\r\nX-Multi: b\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn callback() -> FuncRef {
        FuncRef {
            owner: Arc::from("test"),
            instance: 1,
            id: 1,
        }
    }

    fn request(url: String) -> HttpRequest {
        HttpRequest {
            url,
            method: "GET".to_owned(),
            headers: Vec::new(),
            body: None,
            timeout: None,
        }
    }

    /// Waits until a request finishes and returns what finished.
    fn wait_for(client: &HttpClient) -> Vec<Completion> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let done = client.take_finished();
            if !done.is_empty() {
                return done;
            }
            assert!(Instant::now() < deadline, "the request never finished");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn resource() -> ResourceName {
        Arc::from("fetcher")
    }

    #[test]
    fn a_response_keeps_its_status_headers_and_body() {
        let client = HttpClient::default();
        let url = serve(ok_response("hello"), Duration::ZERO);
        client
            .start(
                &resource(),
                request(url),
                callback(),
                Duration::from_secs(10),
                WORKERS,
                true,
            )
            .expect("start");

        let done = wait_for(&client);
        let Outcome::Response {
            status,
            headers,
            body,
        } = &done[0].outcome
        else {
            panic!("expected a response, got {:?}", done[0].outcome);
        };
        assert_eq!(*status, 201);
        assert_eq!(body, b"hello");
        assert!(headers.contains(&("content-type".to_owned(), "text/plain".to_owned())));
        assert!(headers.contains(&("x-multi".to_owned(), "a, b".to_owned())));
    }

    #[test]
    fn an_error_status_is_a_response_not_a_failure() {
        let client = HttpClient::default();
        let response =
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnope"
                .to_vec();
        client
            .start(
                &resource(),
                request(serve(response, Duration::ZERO)),
                callback(),
                Duration::from_secs(10),
                WORKERS,
                true,
            )
            .expect("start");

        let done = wait_for(&client);
        let args = done.into_iter().next().expect("one").outcome.into_args();
        assert_eq!(args[0], Value::Int(404));
        assert_eq!(args[1], Value::from("nope"));
        assert_eq!(args[3], Value::from(""));
    }

    #[test]
    fn a_silent_server_times_out() {
        let client = HttpClient::default();
        client
            .start(
                &resource(),
                request(serve(ok_response("late"), Duration::from_secs(5))),
                callback(),
                Duration::from_millis(300),
                WORKERS,
                true,
            )
            .expect("start");

        let done = wait_for(&client);
        let Outcome::Failed(message) = &done[0].outcome else {
            panic!("expected a timeout, got {:?}", done[0].outcome);
        };
        assert_ne!(message.as_str(), "");
        let args = done.into_iter().next().expect("one").outcome.into_args();
        assert_eq!(args[0], Value::Int(0));
    }

    #[test]
    fn a_body_over_the_limit_fails() {
        let client = HttpClient::default();
        let size = MAX_BODY + 1;
        let head =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {size}\r\nConnection: close\r\n\r\n");
        let mut response = head.into_bytes();
        response.resize(response.len() + usize::try_from(size).expect("fits"), b'x');
        client
            .start(
                &resource(),
                request(serve(response, Duration::ZERO)),
                callback(),
                Duration::from_secs(20),
                WORKERS,
                true,
            )
            .expect("start");

        let done = wait_for(&client);
        assert!(matches!(done[0].outcome, Outcome::Failed(_)));
    }

    #[test]
    fn only_http_and_https_are_accepted() {
        let client = HttpClient::default();
        for url in ["ftp://example.test/", "example.test/relative", "not a url"] {
            let result = client.start(
                &resource(),
                request(url.to_owned()),
                callback(),
                Duration::from_secs(1),
                WORKERS,
                false,
            );
            assert!(result.is_err(), "{url} was accepted");
        }
        assert!(client.in_flight.borrow().is_empty());
    }

    #[test]
    fn an_invalid_method_is_refused() {
        let client = HttpClient::default();
        let mut bad = request("http://127.0.0.1:1/".to_owned());
        bad.method = "BAD METHOD".to_owned();
        assert!(
            client
                .start(
                    &resource(),
                    bad,
                    callback(),
                    Duration::from_secs(1),
                    WORKERS,
                    false,
                )
                .is_err()
        );
    }

    #[test]
    fn a_resource_has_at_most_sixty_four_requests_in_flight() {
        let client = HttpClient::default();
        let name = resource();
        for _ in 0..MAX_IN_FLIGHT {
            client.reserve(&name, 1).expect("within the limit");
        }
        assert!(client.reserve(&name, 1).is_err());
        assert!(client.reserve(&Arc::from("other"), 1).is_ok());
        assert!(client.reserve(&name, 2).is_ok());
        client.release(&name, 1);
        assert!(client.reserve(&name, 1).is_ok());
    }

    #[test]
    fn a_delivered_request_frees_its_slot() {
        let client = HttpClient::default();
        let name = resource();
        client
            .start(
                &name,
                request(serve(ok_response("x"), Duration::ZERO)),
                callback(),
                Duration::from_secs(10),
                WORKERS,
                true,
            )
            .expect("start");
        let done = wait_for(&client);
        for completion in done {
            client.finish(&completion.resource, completion.callback.instance);
        }
        assert!(client.in_flight.borrow().is_empty());
    }

    #[test]
    fn more_requests_than_workers_all_finish() {
        let client = HttpClient::default();
        let name = resource();
        for index in 0..6 {
            client
                .start(
                    &name,
                    request(serve(ok_response(&index.to_string()), Duration::ZERO)),
                    callback(),
                    Duration::from_secs(10),
                    1,
                    true,
                )
                .expect("start");
        }
        let mut bodies = Vec::new();
        while bodies.len() < 6 {
            for completion in wait_for(&client) {
                let Outcome::Response { body, .. } = completion.outcome else {
                    panic!("a request failed");
                };
                bodies.push(String::from_utf8(body).expect("text"));
            }
        }
        bodies.sort();
        assert_eq!(bodies, ["0", "1", "2", "3", "4", "5"]);
    }

    #[test]
    fn a_full_queue_refuses_new_requests() {
        let client = HttpClient::default();
        let jammed = serve(ok_response("late"), Duration::from_secs(5));
        let mut refused = 0;
        for index in 0..(QUEUE_CAPACITY + MAX_IN_FLIGHT + 8) {
            let owner: ResourceName = Arc::from(format!("flood{}", index / MAX_IN_FLIGHT));
            let result = client.start(
                &owner,
                request(jammed.clone()),
                callback(),
                Duration::from_millis(200),
                1,
                true,
            );
            if result.is_err() {
                refused += 1;
            }
        }
        assert!(refused > 0, "nothing was refused");
    }

    #[test]
    fn a_discarded_resource_never_delivers() {
        let client = HttpClient::default();
        let name = resource();
        let url = serve(ok_response("late"), Duration::from_millis(200));
        for _ in 0..3 {
            client
                .start(
                    &name,
                    request(url.clone()),
                    callback(),
                    Duration::from_secs(10),
                    1,
                    true,
                )
                .expect("start");
        }
        client.discard(&name, callback().instance);
        assert!(client.in_flight.borrow().is_empty());
        thread::sleep(Duration::from_millis(600));
        // The one running request may finish; the queued ones were skipped.
        assert!(client.take_finished().len() <= 1);
    }

    #[test]
    fn private_and_loopback_targets_are_blocked_by_default() {
        let client = HttpClient::default();
        for url in [
            "http://127.0.0.1:1/",
            "http://localhost:1/",
            "http://[::1]:1/",
            "http://[::ffff:127.0.0.1]:1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.0.1/",
            "http://192.168.1.1/",
            "http://100.64.0.1/",
            "http://0.0.0.0/",
        ] {
            client
                .start(
                    &resource(),
                    request(url.to_owned()),
                    callback(),
                    Duration::from_secs(2),
                    WORKERS,
                    false,
                )
                .expect("start");
            let done = wait_for(&client);
            let Outcome::Failed(message) = &done[0].outcome else {
                panic!("expected `{url}` to be blocked, got {:?}", done[0].outcome);
            };
            assert!(message.contains("private or loopback"), "{url}: {message}");
            client.finish(&done[0].resource, done[0].callback.instance);
        }

        assert!(!is_forbidden_ip("1.1.1.1".parse().expect("ip")));
        assert!(!is_forbidden_ip("172.15.255.255".parse().expect("ip")));
        assert!(is_forbidden_ip("172.16.0.1".parse().expect("ip")));
        assert!(is_forbidden_ip("fe80::1".parse().expect("ip")));
        assert!(is_forbidden_ip("fc00::1".parse().expect("ip")));
        assert!(!is_forbidden_ip(
            "2606:4700:4700::1111".parse().expect("ip")
        ));
    }

    #[test]
    fn redirects_are_validated_and_followed_safely() {
        let base: Uri = "https://public.example.test/api/v1/start"
            .parse()
            .expect("uri");
        let blocked = resolve_redirect(&base, "http://169.254.169.254/latest").expect("uri");
        assert!(check_target_syntax(&blocked, false).is_err());

        let resolver = SafePublicResolver::default();
        let cfg = Config::default();
        let localhost_uri: Uri = "http://localhost:80/".parse().expect("uri");
        assert!(
            resolver
                .resolve(&localhost_uri, &cfg, NextTimeout::default())
                .is_err()
        );
        let loopback_uri: Uri = "http://127.0.0.1:80/".parse().expect("uri");
        assert!(
            resolver
                .resolve(&loopback_uri, &cfg, NextTimeout::default())
                .is_err()
        );

        let relative = resolve_redirect(&base, "next?ok=1#frag").expect("uri");
        assert_eq!(
            relative.to_string(),
            "https://public.example.test/api/v1/next?ok=1"
        );
        assert!(same_origin(&base, &relative));

        let other = resolve_redirect(&base, "https://other.example.test/hook").expect("uri");
        assert!(!same_origin(&base, &other));
        let mut method = Method::POST;
        let mut body = b"secret".to_vec();
        let mut headers = vec![
            ("Authorization".to_owned(), "Bearer tok".to_owned()),
            ("Cookie".to_owned(), "a=b".to_owned()),
            ("X-Trace".to_owned(), "1".to_owned()),
        ];
        apply_redirect_semantics(302, &mut method, &mut body, &mut headers, true);
        assert_eq!(method, Method::GET);
        assert_eq!(body, b"");
        assert_eq!(headers, [("X-Trace".to_owned(), "1".to_owned())]);

        let client = HttpClient::default();
        let final_url = serve(ok_response("final"), Duration::ZERO);
        let redirect = format!(
            "HTTP/1.1 302 Found\r\nLocation: {final_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .into_bytes();
        client
            .start(
                &resource(),
                request(serve(redirect, Duration::ZERO)),
                callback(),
                Duration::from_secs(10),
                WORKERS,
                true,
            )
            .expect("start redirect");
        let done = wait_for(&client);
        let Outcome::Response { status, body, .. } = &done[0].outcome else {
            panic!("expected redirected response, got {:?}", done[0].outcome);
        };
        assert_eq!(*status, 201);
        assert_eq!(body, b"final");
    }
}
