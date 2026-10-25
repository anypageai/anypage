//! `cloud.chat` end to end: a real page, a 127.0.0.1 mock server speaking
//! the OpenAI streaming protocol.

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use anypage_engine::Page;
use common::*;
use serde_json::json;

const PAGE: &str = r#"
Req = page.input("req", nil, { agent = true })
Chat = page.stream("chat", "cloud.chat", { request = Req, latest = true })
"#;

fn caps(port: u16) -> String {
    format!(r#"{{ "modules": ["cloud"], "net": ["127.0.0.1:{port}"], "secrets": ["openai"] }}"#)
}

fn sse_chunks(chunks: &[serde_json::Value]) -> String {
    let mut s = String::from(": connected\r\n\r\n");
    for c in chunks {
        s.push_str(&format!("data: {c}\r\n\r\n"));
        s.push_str(": keep-alive\r\n\r\n");
    }
    s.push_str("data: [DONE]\r\n\r\n");
    s
}

fn delta(d: serde_json::Value) -> serde_json::Value {
    json!({ "id": "chatcmpl-1", "object": "chat.completion.chunk", "model": "mock-1", "choices": [{ "index": 0, "delta": d, "finish_reason": null }] })
}

fn tool_stream() -> String {
    sse_chunks(&[
        delta(json!({ "role": "assistant", "content": "" })),
        delta(json!({ "content": "Checking " })),
        delta(json!({ "content": "the weather…" })),
        delta(json!({ "tool_calls": [{ "index": 0, "id": "call_1", "type": "function", "function": { "name": "get_weather", "arguments": "" } }] })),
        delta(json!({ "tool_calls": [{ "index": 0, "function": { "arguments": "{\"ci" } }] })),
        delta(json!({ "tool_calls": [{ "index": 1, "id": "call_2", "type": "function", "function": { "name": "get_time", "arguments": "{}" } }] })),
        delta(json!({ "tool_calls": [{ "index": 0, "function": { "arguments": "ty\":\"台北\"}" } }] })),
        json!({ "id": "chatcmpl-1", "model": "mock-1", "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }] }),
        json!({ "id": "chatcmpl-1", "model": "mock-1", "choices": [], "usage": { "prompt_tokens": 12, "completion_tokens": 7, "total_tokens": 19 } }),
    ])
}

#[test]
fn streams_text_and_tool_calls() {
    let mock = Mock::start(|r| {
        assert_eq!(r.path(), "/v1/chat/completions");
        // 7-byte pieces: events, CRLF pairs and UTF-8 sequences split across reads
        Reply::sse(&tool_stream(), 7, Duration::from_millis(0))
    });
    let mut p = load(&caps(mock.port), PAGE, secrets(&[("openai", "sk-test-123456")])).unwrap();
    set(
        &mut p,
        "req",
        json!({
            "base_url": mock.url("/v1"), "model": "mock-1", "secret": "openai", "temperature": 0.2,
            "messages": [{ "role": "user", "content": "台北天氣?" }],
            "tools": [{ "type": "function", "function": { "name": "get_weather", "parameters": { "type": "object", "properties": { "city": { "type": "string" } } } } }]
        }),
    );
    let last = last_item(&mut p, "chat", 10, |l| l.get("done").truthy());
    assert_eq!(last.get("error"), &anypage_runtime::Data::Nil, "{last:?}");
    assert_eq!(last.get("text").as_str(), Some("Checking the weather…"));
    assert_eq!(last.get("finish_reason").as_str(), Some("tool_calls"));
    let calls = last.get("tool_calls");
    assert_eq!(calls.items().len(), 2);
    assert_eq!(calls.at(1).get("id").as_str(), Some("call_1"));
    assert_eq!(calls.at(1).get("name").as_str(), Some("get_weather"));
    assert_eq!(calls.at(1).get("arguments").as_str(), Some("{\"city\":\"台北\"}"));
    assert_eq!(calls.at(1).get("args").get("city").as_str(), Some("台北"));
    assert_eq!(calls.at(2).get("name").as_str(), Some("get_time"));
    assert_eq!(last.get("usage").get("total_tokens").num(), Some(19.0));
    assert_eq!(last.get("model").as_str(), Some("mock-1"));
    assert_eq!(last.get("status").num(), Some(200.0));
    let first_token = last.get("first_token_ms").num().expect("first_token_ms");
    eprintln!("chat via 127.0.0.1, 7-byte pieces: first token {first_token:.1} ms, whole reply {:.1} ms", last.get("elapsed_ms").num().unwrap_or(0.0));
    // what the server got
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1);
    let r = &reqs[0];
    assert_eq!(r.method, "POST");
    assert_eq!(r.header("authorization"), Some("Bearer sk-test-123456"));
    assert_eq!(r.header("content-type"), Some("application/json"));
    let body = r.json();
    assert_eq!(body["stream"], true);
    assert_eq!(body["model"], "mock-1");
    assert_eq!(body["temperature"], 0.2);
    assert_eq!(body["messages"][0]["content"], "台北天氣?");
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert_eq!(body["tools"][0]["function"]["name"], "get_weather");
    p.shutdown();
}

#[test]
fn intermediate_items_accumulate() {
    // slow chunks: the page sees the text grow before `done`
    let body = sse_chunks(&[delta(json!({ "content": "one " })), delta(json!({ "content": "two " })), delta(json!({ "content": "three" }))]);
    let mock = Mock::start(move |_| Reply::sse(&body, 64, Duration::from_millis(60)));
    let mut p = load(&caps(mock.port), PAGE, secrets(&[])).unwrap();
    set(&mut p, "req", json!({ "base_url": mock.url("/v1"), "model": "m", "messages": [{ "role": "user", "content": "count" }] }));
    let partial = last_item(&mut p, "chat", 10, |l| l.get("text").as_str().is_some_and(|t| !t.is_empty()) && !l.get("done").truthy());
    assert!(partial.get("text").as_str().unwrap().starts_with("one"));
    let done = last_item(&mut p, "chat", 10, |l| l.get("done").truthy());
    assert_eq!(done.get("text").as_str(), Some("one two three"));
    // no secret was named: no Authorization header
    assert_eq!(mock.requests()[0].header("authorization"), None);
}

#[test]
fn new_request_cancels_and_nil_cancels() {
    let mock = Mock::start(|r| {
        let prompt = r.json()["messages"][0]["content"].as_str().unwrap_or("").to_string();
        if prompt == "slow" {
            let chunk = format!("data: {}\n\n", delta(json!({ "content": "x" }))).into_bytes();
            Reply::Endless { headers: vec![("Content-Type".into(), "text/event-stream".into())], chunk, delay: Duration::from_millis(20) }
        } else {
            Reply::sse(&sse_chunks(&[delta(json!({ "content": format!("answer to {prompt}") }))]), 1000, Duration::ZERO)
        }
    });
    let mut p = load(&caps(mock.port), PAGE, secrets(&[])).unwrap();
    let req = |prompt: &str| json!({ "base_url": mock.url("/v1"), "model": "m", "messages": [{ "role": "user", "content": prompt }] });
    set(&mut p, "req", req("slow"));
    let first = last_item(&mut p, "chat", 10, |l| l.get("text").as_str().is_some_and(|t| t.len() >= 3));
    assert_eq!(first.get("seq").num(), Some(1.0));
    // a new request replaces the running one
    set(&mut p, "req", req("fast"));
    let second = last_item(&mut p, "chat", 10, |l| l.get("seq").num() == Some(2.0) && l.get("done").truthy());
    assert_eq!(second.get("text").as_str(), Some("answer to fast"));
    // the superseded stream never shows up again
    std::thread::sleep(Duration::from_millis(200));
    let still = last_item(&mut p, "chat", 1, |_| true);
    assert_eq!(still.get("seq").num(), Some(2.0));
    // and its connection was dropped
    let t0 = std::time::Instant::now();
    while mock.hangups.load(Ordering::Relaxed) < 1 && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(mock.hangups.load(Ordering::Relaxed) >= 1, "the slow stream was not closed");
    // nil cancels: the cancelled request ends marked as such
    set(&mut p, "req", req("slow"));
    last_item(&mut p, "chat", 10, |l| l.get("seq").num() == Some(3.0) && l.get("text").as_str().is_some_and(|t| !t.is_empty()));
    set(&mut p, "req", serde_json::Value::Null);
    let stopped = last_item(&mut p, "chat", 10, |l| l.get("seq").num() == Some(3.0) && l.get("done").truthy());
    assert_eq!(stopped.get("cancelled").as_bool(), Some(true));
    assert!(stopped.get("error").is_nil());
    p.shutdown();
}

#[test]
fn keeps_history_when_asked() {
    let mock = Mock::start(|r| {
        let n = r.json()["messages"].as_array().map(|a| a.len()).unwrap_or(0);
        Reply::sse(&sse_chunks(&[delta(json!({ "content": format!("reply {n}") }))]), 1000, Duration::ZERO)
    });
    let mut p = load(&caps(mock.port), PAGE, secrets(&[])).unwrap();
    let turn = |text: &str, k: u32| json!({ "base_url": mock.url("/v1"), "model": "m", "keep_history": true, "turn": k, "messages": [{ "role": "user", "content": text }] });
    set(&mut p, "req", json!({ "base_url": mock.url("/v1"), "model": "m", "keep_history": true, "messages": [{ "role": "system", "content": "be brief" }, { "role": "user", "content": "hi" }] }));
    let a = last_item(&mut p, "chat", 10, |l| l.get("seq").num() == Some(1.0) && l.get("done").truthy());
    assert_eq!(a.get("text").as_str(), Some("reply 2"));
    set(&mut p, "req", turn("again", 2));
    let b = last_item(&mut p, "chat", 10, |l| l.get("seq").num() == Some(2.0) && l.get("done").truthy());
    assert_eq!(b.get("text").as_str(), Some("reply 4"));
    // the item carries the conversation sent
    assert_eq!(b.get("messages").items().len(), 4);
    let sent = mock.requests()[1].json();
    assert_eq!(sent["messages"][2], json!({ "role": "assistant", "content": "reply 2" }));
    assert_eq!(sent["messages"][3]["content"], "again");
    // reset
    set(&mut p, "req", json!({ "base_url": mock.url("/v1"), "keep_history": true, "reset": true }));
    let c = last_item(&mut p, "chat", 10, |l| l.get("seq").num() == Some(3.0));
    assert!(c.get("done").truthy());
    assert_eq!(c.get("messages").items().len(), 0);
}

#[test]
fn server_errors_are_reported_without_secrets() {
    let mock = Mock::start(|r| {
        if r.path().ends_with("/bad/chat/completions") {
            Reply::json(401, json!({ "error": { "message": "Incorrect API key provided: sk-test-SECRET99", "type": "invalid_request_error" } }))
        } else if r.path().ends_with("/midstream/chat/completions") {
            Reply::sse(&format!("data: {}\n\nevent: error\ndata: {{\"error\":{{\"message\":\"overloaded\"}}}}\n\n", delta(json!({ "content": "par" }))), 1000, Duration::ZERO)
        } else {
            Reply::text(200, "text/event-stream", "data: {not json\n\n")
        }
    });
    let mut p = load(&caps(mock.port), PAGE, secrets(&[("openai", "sk-test-SECRET99")])).unwrap();
    set(&mut p, "req", json!({ "base_url": mock.url("/bad"), "model": "m", "secret": "openai", "messages": [{ "role": "user", "content": "x" }] }));
    let e = last_item(&mut p, "chat", 10, |l| l.get("done").truthy());
    let msg = e.get("error").as_str().unwrap();
    assert!(msg.contains("HTTP 401"), "{msg}");
    assert!(!msg.contains("SECRET99"), "secret leaked: {msg}");
    assert!(msg.contains("[redacted]"), "{msg}");
    set(&mut p, "req", json!({ "base_url": mock.url("/midstream"), "model": "m", "messages": [{ "role": "user", "content": "x" }] }));
    let e = last_item(&mut p, "chat", 10, |l| l.get("seq").num() == Some(2.0) && l.get("done").truthy());
    assert_eq!(e.get("error").as_str(), Some("overloaded"));
    assert_eq!(e.get("text").as_str(), Some("par"));
    set(&mut p, "req", json!({ "base_url": mock.url("/garbage"), "model": "m", "messages": [{ "role": "user", "content": "x" }] }));
    let e = last_item(&mut p, "chat", 10, |l| l.get("seq").num() == Some(3.0) && l.get("done").truthy());
    assert!(e.get("error").as_str().unwrap().contains("not JSON"), "{e:?}");
}

#[test]
fn capability_refusals() {
    let mock = Mock::start(|_| Reply::redirect(307, "https://evil.example/v1/chat/completions"));
    let mut p = load(&caps(mock.port), PAGE, secrets(&[("openai", "sk-x"), ("other", "sk-y")])).unwrap();
    let cases = [
        (json!({ "base_url": "https://api.openai.com/v1", "model": "m", "messages": [{ "role": "user", "content": "x" }] }), "not declared"),
        (json!({ "base_url": format!("http://127.0.0.1:{}/v1", mock.port.wrapping_add(1)), "model": "m", "messages": [{ "role": "user", "content": "x" }] }), "not declared"),
        (json!({ "base_url": "http://example.com/v1", "model": "m", "messages": [{ "role": "user", "content": "x" }] }), "plain http"),
        (json!({ "base_url": "file:///etc/passwd", "model": "m", "messages": [{ "role": "user", "content": "x" }] }), "only https"),
        (json!({ "base_url": mock.url("/v1"), "model": "m", "secret": "other", "messages": [{ "role": "user", "content": "x" }] }), "not declared in page.json capabilities.secrets"),
        (json!({ "base_url": mock.url("/v1"), "model": "m", "messages": [{ "role": "user", "content": "x" }] }), "redirected to a URL this page may not use"),
        (json!({ "model": "m", "messages": [] }), "base_url"),
    ];
    for (k, (req, want)) in cases.iter().enumerate() {
        set(&mut p, "req", req.clone());
        let seq = (k + 1) as f64;
        let e = last_item(&mut p, "chat", 10, |l| l.get("seq").num() == Some(seq) && l.get("done").truthy());
        let msg = e.get("error").as_str().unwrap_or("");
        assert!(msg.contains(want), "case {k}: wanted \"{want}\" in {msg:?}");
    }
    // the redirect was the only request that reached a server
    assert_eq!(mock.requests().len(), 1);
}

#[test]
fn non_streamed_json_and_ndjson_answers() {
    let mock = Mock::start(|r| {
        if r.path().starts_with("/json") {
            Reply::json(200, json!({ "id": "x", "model": "m", "choices": [{ "index": 0, "message": { "role": "assistant", "content": "whole answer" }, "finish_reason": "stop" }], "usage": { "total_tokens": 3 } }))
        } else {
            let lines = "{\"message\":{\"role\":\"assistant\",\"content\":\"nd\"},\"done\":false}\n{\"message\":{\"content\":\"json\"},\"done\":true,\"done_reason\":\"stop\",\"prompt_eval_count\":1,\"eval_count\":2}\n";
            Reply::Chunks { status: 200, headers: vec![("Content-Type".into(), "application/x-ndjson".into())], chunks: lines.as_bytes().chunks(5).map(<[u8]>::to_vec).collect(), delay: Duration::ZERO }
        }
    });
    let mut p = load(&caps(mock.port), PAGE, secrets(&[])).unwrap();
    set(&mut p, "req", json!({ "url": mock.url("/json"), "messages": [{ "role": "user", "content": "x" }] }));
    let a = last_item(&mut p, "chat", 10, |l| l.get("done").truthy());
    assert_eq!(a.get("text").as_str(), Some("whole answer"));
    set(&mut p, "req", json!({ "url": mock.url("/nd"), "messages": [{ "role": "user", "content": "x" }] }));
    let b = last_item(&mut p, "chat", 10, |l| l.get("seq").num() == Some(2.0) && l.get("done").truthy());
    assert_eq!(b.get("text").as_str(), Some("ndjson"));
    assert_eq!(b.get("usage").get("total_tokens").num(), Some(3.0));
}

/// "a", then "b" 2 ms later, then a 1.5 s pause: the page must show "ab"
/// during the pause, not "a" (a throttled update must not be held back
/// until the next read).
#[test]
fn the_newest_text_arrives_during_a_pause() {
    let piece = |s: &str| format!("data: {}\n\n", delta(json!({ "content": s }))).into_bytes();
    let mock = Mock::start(move |_| Reply::Timed {
        status: 200,
        headers: vec![("Content-Type".into(), "text/event-stream".into())],
        chunks: vec![
            (Duration::ZERO, piece("a")),
            (Duration::from_millis(2), piece("b")),
            (Duration::from_millis(1500), [piece("c"), b"data: [DONE]\n\n".to_vec()].concat()),
        ],
    });
    for run in 0..3 {
        let mut p = load(&caps(mock.port), PAGE, secrets(&[])).unwrap();
        let t0 = std::time::Instant::now();
        set(&mut p, "req", json!({ "base_url": mock.url("/v1"), "model": "m", "messages": [{ "role": "user", "content": "x" }] }));
        // well inside the pause
        let mid = last_item(&mut p, "chat", 2, |l| l.get("text").as_str() == Some("ab"));
        let at = t0.elapsed();
        assert!(!mid.get("done").truthy(), "run {run}");
        assert!(at < Duration::from_millis(900), "run {run}: \"ab\" only after {at:?}");
        let done = last_item(&mut p, "chat", 5, |l| l.get("done").truthy());
        assert_eq!(done.get("text").as_str(), Some("abc"), "run {run}");
        p.shutdown();
    }
}

/// Requests replaced in quick succession while the server says nothing:
/// each superseded request's connection is closed at once, not left open
/// until its timeout.
#[test]
fn superseded_requests_close_their_connections() {
    let mock = Mock::start(|r| {
        if r.json()["messages"][0]["content"] == "last" {
            Reply::sse(&sse_chunks(&[delta(json!({ "content": "done" }))]), 1000, Duration::ZERO)
        } else {
            Reply::Hang(Duration::from_secs(20))
        }
    });
    let mut p = load(&caps(mock.port), PAGE, secrets(&[])).unwrap();
    for i in 0..12 {
        set(&mut p, "req", json!({ "base_url": mock.url("/v1"), "model": "m", "messages": [{ "role": "user", "content": format!("q{i}") }] }));
        p.parts().0.commit();
        std::thread::sleep(Duration::from_millis(40));
    }
    let reached = mock.requests().len();
    assert!(reached >= 3, "only {reached} requests reached the server");
    // at most the newest is still open
    assert!(mock.eventually(2.0, |m| m.open_now() <= 1), "{} connections still open", mock.open_now());
    assert!(mock.hangups.load(Ordering::Relaxed) >= reached - 1, "{} of {reached} closed", mock.hangups.load(Ordering::Relaxed));
    // and the stream keeps working
    set(&mut p, "req", json!({ "base_url": mock.url("/v1"), "model": "m", "messages": [{ "role": "user", "content": "last" }] }));
    let last = last_item(&mut p, "chat", 10, |l| l.get("done").truthy() && l.get("seq").num() == Some(13.0));
    assert_eq!(last.get("text").as_str(), Some("done"));
    assert!(mock.eventually(2.0, |m| m.open_now() == 0));
    p.shutdown();
}

/// A server that echoes the key in the reply (even split across chunks)
/// never shows it to the page.
#[test]
fn echoed_keys_are_scrubbed_from_replies() {
    let mock = Mock::start(|r| {
        let key = r.header("authorization").unwrap_or("").trim_start_matches("Bearer ").to_string();
        let (a, b) = key.split_at(key.len() / 2);
        Reply::sse(
            &sse_chunks(&[
                delta(json!({ "content": format!("your key is {a}") })),
                delta(json!({ "content": format!("{b}, ok") })),
                delta(json!({ "tool_calls": [{ "index": 0, "id": "c1", "type": "function", "function": { "name": "leak", "arguments": format!("{{\"k\":\"{key}\"}}") } }] })),
            ]),
            5,
            Duration::ZERO,
        )
    });
    let mut p = load(&caps(mock.port), PAGE, secrets(&[("openai", "sk-echo-SECRET-424242")])).unwrap();
    set(&mut p, "req", json!({ "base_url": mock.url("/v1"), "model": "m", "secret": "openai", "messages": [{ "role": "user", "content": "x" }] }));
    let last = last_item(&mut p, "chat", 10, |l| l.get("done").truthy());
    assert_eq!(mock.requests()[0].header("authorization"), Some("Bearer sk-echo-SECRET-424242"));
    assert_eq!(last.get("text").as_str(), Some("your key is [redacted], ok"));
    assert_eq!(last.get("tool_calls").at(1).get("arguments").as_str(), Some("{\"k\":\"[redacted]\"}"));
    assert_eq!(last.get("tool_calls").at(1).get("args").get("k").as_str(), Some("[redacted]"));
    p.shutdown();
}

#[test]
fn generic_event_stream_source() {
    let mock = Mock::start(|r| {
        assert_eq!(r.header("x-api-key"), Some("k-123456"));
        Reply::sse("event: message_start\ndata: {\"type\":\"start\"}\n\n: ping\n\nid: 9\ndata: line1\ndata: line2\n\n", 3, Duration::ZERO)
    });
    let src = r#"
Req = page.input("req", nil, { agent = true })
Ev = page.stream("ev", "cloud.sse", { request = Req, cap = 100 })
"#;
    let caps = format!(r#"{{ "modules": ["cloud"], "net": ["127.0.0.1:{}"], "secrets": ["anthropic"] }}"#, mock.port);
    let mut p = load(&caps, src, secrets(&[("anthropic", "k-123456")])).unwrap();
    set(&mut p, "req", json!({ "url": mock.url("/v1/messages"), "json": { "stream": true }, "secret": "anthropic", "auth": "x-api-key" }));
    let v = wait_for(&mut p, "ev", 10, |d| d.get("last").get("done").truthy());
    let items = v.get("items").items();
    assert_eq!(items.len(), 3, "{items:?}");
    assert_eq!(items[0].get("event").as_str(), Some("message_start"));
    assert_eq!(items[0].get("json").get("type").as_str(), Some("start"));
    assert_eq!(items[1].get("data").as_str(), Some("line1\nline2"));
    assert_eq!(items[1].get("id").as_str(), Some("9"));
    assert_eq!(items[2].get("status").num(), Some(200.0));
    assert!(items[2].get("error").is_nil());
}
