//! `PerformHttpRequest`: argument conversion, the callback, and the server-only binding.

mod common;

use std::time::Instant;

use common::{call, first, new_client_host, new_host, start, tick};
use sixm_core::{Host, Value};

#[test]
fn the_request_carries_every_argument_converted() {
    let host = new_host();
    let _runtime = start(
        &host,
        "fetcher",
        &[],
        &[(
            "main.lua",
            "PerformHttpRequest('https://example.test/a?b=1', function() end, 'POST', 'payload',\n\
             { ['X-Token'] = 'abc', ['X-Count'] = 3 }, { timeout = 1500 })\n",
        )],
    );

    let requests = host.http_requests.borrow();
    assert_eq!(requests.len(), 1);
    let (resource, request, _callback) = &requests[0];
    assert_eq!(resource.as_ref(), "fetcher");
    assert_eq!(request.url, "https://example.test/a?b=1");
    assert_eq!(request.method, "POST");
    assert_eq!(request.body.as_deref(), Some(&b"payload"[..]));
    assert_eq!(request.timeout.map(|t| t.as_millis()), Some(1500));
    let mut headers = request.headers.clone();
    headers.sort();
    assert_eq!(
        headers,
        vec![
            ("X-Count".to_owned(), "3".to_owned()),
            ("X-Token".to_owned(), "abc".to_owned()),
        ]
    );
}

#[test]
fn optional_arguments_default_to_a_get_without_body_headers_or_timeout() {
    let host = new_host();
    let _runtime = start(
        &host,
        "fetcher",
        &[],
        &[(
            "main.lua",
            "PerformHttpRequest('http://example.test/', function() end)\n",
        )],
    );

    let requests = host.http_requests.borrow();
    let (_, request, _) = &requests[0];
    assert_eq!(request.method, "GET");
    assert_eq!(request.body, None);
    assert_eq!(request.headers, Vec::<(String, String)>::new());
    assert_eq!(request.timeout, None);
}

#[test]
fn the_callback_receives_the_outcome_and_is_released_afterwards() {
    let host = new_host();
    let runtime = start(
        &host,
        "fetcher",
        &["seen"],
        &[(
            "main.lua",
            "local seen = 'none'\n\
             PerformHttpRequest('http://example.test/', function(status, body, headers, err)\n\
               seen = status .. ':' .. body .. ':' .. headers['content-type'] .. ':' .. err\n\
             end)\n\
             exports('seen', function() return seen end)\n",
        )],
    );

    let (_, _, callback) = host.http_requests.borrow()[0].clone();
    let outcome = [
        Value::Int(200),
        Value::from("hi"),
        Value::Map(vec![(
            Value::from("content-type"),
            Value::from("text/plain"),
        )]),
        Value::from(""),
    ];
    host.call_ref(&callback, &outcome).expect("callback");
    assert_eq!(first(&runtime, "seen"), Value::from("200:hi:text/plain:"));
}

#[test]
fn a_non_positive_timeout_is_a_script_error() {
    let host = new_host();
    let runtime = start(
        &host,
        "fetcher",
        &["err"],
        &[(
            "main.lua",
            "local err = 'none'\n\
             local ok, e = pcall(PerformHttpRequest, 'http://example.test/', function() end, 'GET', nil, nil, { timeout = 0 })\n\
             if not ok then err = tostring(e) end\n\
             exports('err', function() return err end)\n",
        )],
    );

    assert!(host.http_requests.borrow().is_empty());
    let Value::String(text) = first(&runtime, "err") else {
        panic!("err is not a string");
    };
    assert!(String::from_utf8_lossy(&text).contains("positive number of milliseconds"));
}

#[test]
fn a_client_has_no_perform_http_request() {
    let host = new_client_host(1);
    let runtime = start(
        &host,
        "client_side",
        &["present"],
        &[(
            "main.lua",
            "exports('present', function() return PerformHttpRequest ~= nil end)\n",
        )],
    );

    assert_eq!(first(&runtime, "present"), Value::Bool(false));
}

#[test]
fn perform_http_request_await_returns_the_outcome_from_a_thread() {
    let host = new_host();
    let runtime = start(
        &host,
        "fetcher",
        &["result"],
        &[(
            "main.lua",
            "local result = nil\n\
             exports('result', function() return result end)\n\
             CreateThreadNow(function()\n\
               local s, b, h, e = PerformHttpRequestAwait('http://example.test/', 'POST', 'x', { A = '1' }, { timeout = 500 })\n\
               result = s .. ':' .. b .. ':' .. h['content-type'] .. ':' .. e\n\
             end)\n",
        )],
    );

    let (_, request, callback) = host.http_requests.borrow()[0].clone();
    assert_eq!(request.method, "POST");
    assert_eq!(request.body.as_deref(), Some(&b"x"[..]));
    assert_eq!(request.headers, vec![("A".to_owned(), "1".to_owned())]);
    assert_eq!(request.timeout.map(|t| t.as_millis()), Some(500));
    assert_eq!(first(&runtime, "result"), Value::Nil);

    let outcome = [
        Value::Int(200),
        Value::from("hi"),
        Value::Map(vec![(
            Value::from("content-type"),
            Value::from("text/plain"),
        )]),
        Value::from(""),
    ];
    host.call_ref(&callback, &outcome).expect("callback");
    tick(&host, Instant::now());
    assert_eq!(first(&runtime, "result"), Value::from("200:hi:text/plain:"));
}

#[test]
fn perform_http_request_await_outside_a_thread_is_an_error() {
    let host = new_host();
    let runtime = start(
        &host,
        "fetcher",
        &["try"],
        &[(
            "main.lua",
            "exports('try', function() return pcall(PerformHttpRequestAwait, 'http://example.test/') end)\n",
        )],
    );

    let outcome = call(&runtime, "try", &[]);
    assert!(host.http_requests.borrow().is_empty());
    let [Value::Bool(false), Value::String(text)] = outcome.as_slice() else {
        panic!("expected a failed call, got {outcome:?}");
    };
    assert!(String::from_utf8_lossy(text).contains("can only be called from a thread"));
}

#[test]
fn a_client_has_no_perform_http_request_await() {
    let host = new_client_host(1);
    let runtime = start(
        &host,
        "client_side",
        &["present"],
        &[(
            "main.lua",
            "exports('present', function() return PerformHttpRequestAwait ~= nil end)\n",
        )],
    );

    assert_eq!(first(&runtime, "present"), Value::Bool(false));
}
