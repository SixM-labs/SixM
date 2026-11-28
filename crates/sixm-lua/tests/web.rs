//! `SetHttpHandler` and the `req` and `res` objects a handler is given.

mod common;

use common::{new_client_host, new_host, start};
use sixm_core::{ScriptRuntime, WebReply, WebRequest};
use sixm_lua::LuaRuntime;

/// A `GET /app/page?x=1` request with one header and a body.
fn request(body: &str) -> WebRequest {
    WebRequest {
        method: "GET".to_owned(),
        path: "/app/page".to_owned(),
        query: "x=1".to_owned(),
        headers: vec![("x-token".to_owned(), "abc".to_owned())],
        body: body.as_bytes().to_vec(),
        address: "127.0.0.1".to_owned(),
    }
}

/// Runs the handler the resource registered for request `id`.
fn serve(host: &common::TestHost, runtime: &LuaRuntime, id: u64, data: &WebRequest) {
    let handler = host.web_handlers.borrow()[0].1.clone();
    runtime
        .call_web_handler(handler.id, id, data)
        .expect("handler call");
}

#[test]
fn the_handler_sees_the_request_and_completes_the_response() {
    let host = new_host();
    let runtime = start(
        &host,
        "app",
        &[],
        &[(
            "main.lua",
            "SetHttpHandler(function(req, res)\n\
               res.writeHead(201, { ['Content-Type'] = 'text/plain' })\n\
               res.write(req.method .. ' ' .. req.path .. ' ' .. req.query .. ' ')\n\
               res.send(req.headers['x-token'])\n\
             end)\n",
        )],
    );

    serve(&host, &runtime, 7, &request(""));

    let replies = host.web_replies.borrow();
    assert_eq!(replies.len(), 3);
    assert_eq!(replies[0].0, 7);
    assert_eq!(
        replies[0].1,
        WebReply::Head {
            status: 201,
            headers: vec![("Content-Type".to_owned(), "text/plain".to_owned())],
        }
    );
    assert_eq!(
        replies[1].1,
        WebReply::Write(b"GET /app/page x=1 ".to_vec())
    );
    assert_eq!(replies[2].1, WebReply::End(b"abc".to_vec()));
}

#[test]
fn the_colon_call_syntax_works_too() {
    let host = new_host();
    let runtime = start(
        &host,
        "app",
        &[],
        &[(
            "main.lua",
            "SetHttpHandler(function(req, res)\n\
               res:writeHead(404)\n\
               res:send('nothing here')\n\
             end)\n",
        )],
    );

    serve(&host, &runtime, 3, &request(""));

    let replies = host.web_replies.borrow();
    assert_eq!(
        replies[0].1,
        WebReply::Head {
            status: 404,
            headers: Vec::new()
        }
    );
    assert_eq!(replies[1].1, WebReply::End(b"nothing here".to_vec()));
}

#[test]
fn the_body_handler_gets_the_whole_body_after_the_handler_returns() {
    let host = new_host();
    let runtime = start(
        &host,
        "app",
        &[],
        &[(
            "main.lua",
            "SetHttpHandler(function(req, res)\n\
               req.setDataHandler(function(data)\n\
                 res.send('got ' .. data)\n\
               end)\n\
             end)\n",
        )],
    );

    serve(&host, &runtime, 9, &request("payload"));

    let replies = host.web_replies.borrow();
    assert_eq!(*replies, vec![(9, WebReply::End(b"got payload".to_vec()))]);
}

#[test]
fn a_handler_that_never_completes_sends_nothing() {
    let host = new_host();
    let runtime = start(
        &host,
        "app",
        &[],
        &[(
            "main.lua",
            "SetHttpHandler(function(req, res) res.write('partial') end)\n",
        )],
    );

    serve(&host, &runtime, 4, &request(""));

    let replies = host.web_replies.borrow();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].1, WebReply::Write(b"partial".to_vec()));
}

#[test]
fn a_client_has_no_http_handlers() {
    let host = new_client_host(1);
    let _runtime = start(
        &host,
        "app",
        &[],
        &[("main.lua", "SetHttpHandler(function(req, res) end)\n")],
    );
    assert!(host.web_handlers.borrow().is_empty());
}

#[test]
fn handler_sees_address_and_cancel_handler_lifecycle() {
    let host = new_host();
    let runtime = start(
        &host,
        "app",
        &["state", "bind_late"],
        &[(
            "main.lua",
            "CANCELED = {}\n\
             ADDR = ''\n\
             LATE_REQ = nil\n\
             function state() return ADDR .. ':' .. table.concat(CANCELED, ',') end\n\
             function bind_late() if LATE_REQ then LATE_REQ.setCancelHandler(function() CANCELED[#CANCELED + 1] = 'late' end) end end\n\
             SetHttpHandler(function(req, res)\n\
               ADDR = req.address\n\
               if req.path == '/app/answered' then\n\
                 req.setCancelHandler(function() CANCELED[#CANCELED + 1] = 'answered' end)\n\
                 res.send('done')\n\
                 req.setCancelHandler(function() CANCELED[#CANCELED + 1] = 'after_send' end)\n\
               elseif req.path == '/app/cancel' then\n\
                 req:setCancelHandler(function() CANCELED[#CANCELED + 1] = 'canceled' end)\n\
               elseif req.path == '/app/late' then\n\
                 LATE_REQ = req\n\
               elseif req.path == '/app/bad' then\n\
                 local ok, err = pcall(req.setCancelHandler, 123)\n\
                 if not ok then CANCELED[#CANCELED + 1] = 'err' end\n\
               elseif req.path == '/app/throw_cancel' then\n\
                 req.setCancelHandler(function() error('cancel boom') end)\n\
               elseif req.path == '/app/retain' then\n\
                 req.setCancelHandler(function() CANCELED[#CANCELED + 1] = 'retained' end)\n\
               end\n\
             end)\n",
        )],
    );

    let mut req = request("");
    req.address = "203.0.113.9".to_owned();
    req.path = "/app/answered".to_owned();
    serve(&host, &runtime, 1, &req);
    // Cancel after res.send must be a no-op.
    runtime.cancel_http_request(1);

    req.path = "/app/cancel".to_owned();
    serve(&host, &runtime, 2, &req);
    runtime.cancel_http_request(2);
    // Second cancel call is a no-op.
    runtime.cancel_http_request(2);

    req.path = "/app/bad".to_owned();
    serve(&host, &runtime, 3, &req);

    req.path = "/app/throw_cancel".to_owned();
    serve(&host, &runtime, 4, &req);
    // Error inside cancel handler must be logged and not panic.
    runtime.cancel_http_request(4);

    // Request canceled before setCancelHandler is called must ignore late registration.
    req.path = "/app/late".to_owned();
    serve(&host, &runtime, 6, &req);
    runtime.cancel_http_request(6);
    runtime
        .call_export("bind_late", &[], None)
        .expect("bind_late");
    runtime.cancel_http_request(6);

    let out = runtime.call_export("state", &[], None).expect("state");
    assert_eq!(
        out,
        vec![sixm_core::Value::from("203.0.113.9:canceled,err")]
    );

    // Retained cancel handler on open request drops cleanly on shutdown.
    req.path = "/app/retain".to_owned();
    serve(&host, &runtime, 5, &req);
    runtime.shutdown();
    runtime.cancel_http_request(5);
}
