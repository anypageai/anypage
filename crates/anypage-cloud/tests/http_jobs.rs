//! `cloud.http` as a page job, and the network rules every request follows:
//! capabilities on each redirect hop, credentials only to the first origin,
//! size caps, timeouts, cancellation.

mod common;

use std::time::{Duration, Instant};

use anypage_cloud::{http, net};
use anypage_engine::Page;
use anypage_runtime::{Caps, Data, Grants};
use common::*;
use serde_json::json;

const JOB: &str = r#"
Req = page.input("req", nil, { agent = true })
Res = page.job("res", "cloud.http", { Req })
"#;

fn caps(net: &[String], secrets: &[&str]) -> String {
    json!({ "modules": ["cloud"], "net": net, "secrets": secrets }).to_string()
}

#[test]
fn get_json_with_bearer_secret() {
    let mock = Mock::start(|r| {
        assert_eq!(r.target, "/v1/items?q=a%20b");
        assert_eq!(r.header("authorization"), Some("Bearer tok-abcdef"));
        assert_eq!(r.header("x-trace"), Some("7"));
        Reply::Full { status: 200, headers: vec![("Content-Type".into(), "application/json".into()), ("X-Rate".into(), "9".into())], body: json!({ "items": [1, 2, 3], "name": "中" }).to_string().into_bytes() }
    });
    let mut p = load(&caps(&[format!("127.0.0.1:{}", mock.port)], &["api"]), JOB, secrets(&[("api", "tok-abcdef")])).unwrap();
    set(&mut p, "req", json!({ "url": mock.url("/v1/items?q=a%20b"), "secret": "api", "headers": { "X-Trace": "7" } }));
    let r = job_result(&mut p, "res").unwrap();
    assert_eq!(r.get("status").num(), Some(200.0));
    assert_eq!(r.get("ok").as_bool(), Some(true));
    assert_eq!(r.get("json").get("items").items().len(), 3);
    assert_eq!(r.get("json").get("name").as_str(), Some("中"));
    assert_eq!(r.get("headers").get("x-rate").as_str(), Some("9"));
    assert!(r.get("body").as_bytes().is_some_and(|b| b.starts_with(b"{\"items\"")));
}

#[test]
fn post_json_and_status_codes_are_values() {
    let mock = Mock::start(|r| {
        let j = r.json();
        Reply::json(if j["fail"] == true { 422 } else { 201 }, json!({ "echo": j["v"] }))
    });
    let mut p = load(&caps(&[format!("127.0.0.1:{}", mock.port)], &[]), JOB, secrets(&[])).unwrap();
    set(&mut p, "req", json!({ "url": mock.url("/x"), "json": { "v": [1, "二"] } }));
    let r = job_result(&mut p, "res").unwrap();
    assert_eq!(r.get("status").num(), Some(201.0));
    assert_eq!(r.get("json").get("echo").at(2).as_str(), Some("二"));
    assert_eq!(mock.requests()[0].method, "POST");
    set(&mut p, "req", json!({ "url": mock.url("/x"), "json": { "fail": true } }));
    let r = wait_for(&mut p, "res", 10, |d| d.get("status").num() == Some(422.0));
    assert_eq!(r.get("ok").as_bool(), Some(false));
}

#[test]
fn redirects_are_rechecked_and_drop_credentials_across_origins() {
    let other = Mock::start(|r| Reply::json(200, json!({ "auth": r.header("authorization"), "path": r.target })));
    let other_port = other.port;
    let first = Mock::start(move |r| match r.path() {
        "/same" => Reply::redirect(302, "/landed?x=1"),
        "/landed" => Reply::json(200, json!({ "auth": r.header("authorization"), "path": r.target })),
        "/cross" => Reply::redirect(307, &format!("http://127.0.0.1:{other_port}/elsewhere")),
        "/post303" => Reply::redirect(303, "/landed"),
        "/loop" => Reply::redirect(302, "/loop"),
        _ => Reply::redirect(301, "https://not-declared.example/x"),
    });
    let net = [format!("127.0.0.1:{}", first.port), format!("127.0.0.1:{}", other.port)];
    let mut p = load(&caps(&net, &["api"]), JOB, secrets(&[("api", "tok-abcdef")])).unwrap();
    // same origin: followed, credentials kept (and the echo of the key is
    // scrubbed before the page sees it)
    set(&mut p, "req", json!({ "url": first.url("/same"), "secret": "api" }));
    let r = job_result(&mut p, "res").unwrap();
    assert_eq!(r.get("json").get("auth").as_str(), Some("[redacted]"));
    assert!(!r.get("text").as_str().unwrap_or("").contains("tok-abcdef"));
    let landed = first.requests().into_iter().find(|r| r.path() == "/landed").unwrap();
    assert_eq!(landed.header("authorization"), Some("Bearer tok-abcdef"));
    assert_eq!(r.get("json").get("path").as_str(), Some("/landed?x=1"));
    assert_eq!(r.get("url").as_str(), Some(first.url("/landed").as_str()));
    // another origin: followed (declared), credentials dropped
    set(&mut p, "req", json!({ "url": first.url("/cross"), "secret": "api" }));
    let r = wait_for(&mut p, "res", 10, |d| d.get("json").get("path").as_str() == Some("/elsewhere"));
    assert!(r.get("json").get("auth").is_nil(), "credentials crossed origins: {r:?}");
    // 303 after POST becomes GET without the body
    set(&mut p, "req", json!({ "url": first.url("/post303"), "body": "payload" }));
    wait_for(&mut p, "res", 10, |d| d.get("json").get("path").as_str() == Some("/landed") && d.get("json").get("auth").is_nil());
    let last = first.requests().into_iter().rev().find(|r| r.path() == "/landed").unwrap();
    assert_eq!(last.method, "GET");
    assert!(last.body.is_empty());
    // undeclared target: refused before any request is sent there
    set(&mut p, "req", json!({ "url": first.url("/away") }));
    let e = job_result(&mut p, "res").unwrap_err();
    assert!(e.contains("redirected to a URL this page may not use") && e.contains("not-declared.example"), "{e}");
    // loops end
    set(&mut p, "req", json!({ "url": first.url("/loop") }));
    let e = job_result(&mut p, "res").unwrap_err();
    assert!(e.contains("more than 5 redirects"), "{e}");
    // not following: the 3xx is the answer
    set(&mut p, "req", json!({ "url": first.url("/same"), "follow_redirects": false }));
    let r = wait_for(&mut p, "res", 10, |d| d.get("status").num() == Some(302.0));
    assert_eq!(r.get("headers").get("location").as_str(), Some("/landed?x=1"));
}

#[test]
fn port_specific_capabilities() {
    let a = Mock::start(|_| Reply::redirect(302, "/"));
    let b = Mock::start(|_| Reply::json(200, json!({})));
    let target = b.url("/");
    let a2 = Mock::start(move |_| Reply::redirect(302, &target));
    // only a and a2 are declared, with their ports
    let net = [format!("127.0.0.1:{}", a.port), format!("127.0.0.1:{}", a2.port)];
    let mut p = load(&caps(&net, &[]), JOB, secrets(&[])).unwrap();
    set(&mut p, "req", json!({ "url": b.url("/") }));
    assert!(job_result(&mut p, "res").unwrap_err().contains("not declared"));
    set(&mut p, "req", json!({ "url": a2.url("/") }));
    let e = job_result(&mut p, "res").unwrap_err();
    assert!(e.contains("redirected"), "{e}");
    assert_eq!(b.requests().len(), 0);
}

#[test]
fn size_limits() {
    let mock = Mock::start(|r| {
        let body = vec![b'x'; 5000];
        if r.path() == "/chunked" {
            Reply::Chunks { status: 200, headers: vec![], chunks: body.chunks(1000).map(<[u8]>::to_vec).collect(), delay: Duration::ZERO }
        } else {
            Reply::text(200, "text/plain", body)
        }
    });
    let mut p = load(&caps(&[format!("127.0.0.1:{}", mock.port)], &[]), JOB, secrets(&[])).unwrap();
    set(&mut p, "req", json!({ "url": mock.url("/known"), "max_bytes": 1000 }));
    let e = job_result(&mut p, "res").unwrap_err();
    assert!(e.contains("5000 bytes, more than the limit of 1000 bytes"), "{e}");
    set(&mut p, "req", json!({ "url": mock.url("/chunked"), "max_bytes": 1000 }));
    let e = wait_for(&mut p, "res", 10, |d| d.get("job_error").as_str().is_some_and(|e| e.contains("larger than the limit")));
    assert!(e.get("job_error").as_str().is_some());
    set(&mut p, "req", json!({ "url": mock.url("/chunked"), "max_bytes": 5000 }));
    let r = wait_for(&mut p, "res", 10, |d| d.get("status").num() == Some(200.0));
    assert_eq!(r.get("text").as_str().map(str::len), Some(5000));
}

#[test]
fn timeouts() {
    let mock = Mock::start(|_| Reply::Hang(Duration::from_secs(5)));
    let mut p = load(&caps(&[format!("127.0.0.1:{}", mock.port)], &[]), JOB, secrets(&[])).unwrap();
    let t0 = Instant::now();
    set(&mut p, "req", json!({ "url": mock.url("/slow"), "timeout": 0.4 }));
    let e = job_result(&mut p, "res").unwrap_err();
    assert!(e.contains("timed out"), "{e}");
    assert!(t0.elapsed() < Duration::from_secs(3), "took {:?}", t0.elapsed());
}

#[test]
fn cancellation_stops_a_download_between_chunks() {
    let mock = Mock::start(|_| Reply::Endless { headers: vec![], chunk: vec![b'y'; 4096], delay: Duration::from_millis(5) });
    let g = Grants::new("t", Caps { net: vec![format!("127.0.0.1:{}", mock.port)], ..Caps::default() });
    let req = Data::map([("url", Data::str(&mock.url("/big")))]);
    let t0 = Instant::now();
    let e = http::job(&g, &req, &|| t0.elapsed() > Duration::from_millis(300), &|_| {}).unwrap_err();
    assert_eq!(e, "cancelled");
    assert!(t0.elapsed() < Duration::from_secs(3));
}

/// https really goes through rustls (ring provider, webpki roots): talking
/// TLS to a plain-HTTP server fails in the handshake, as an error, not a
/// panic (rustls panics when no crypto provider is installed).
#[test]
fn https_goes_through_rustls() {
    use std::io::Write;
    // a server that answers anything with plain HTTP at once
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Ok((mut c, _)) = listener.accept() {
            let _ = c.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n");
            std::thread::sleep(Duration::from_millis(300));
        }
    });
    let g = Grants::new("t", Caps { net: vec![format!("127.0.0.1:{port}")], ..Caps::default() });
    let req = Data::map([("url", Data::str(&format!("https://127.0.0.1:{port}/"))), ("timeout", Data::Num(5.0))]);
    let t0 = Instant::now();
    let e = http::job(&g, &req, &|| false, &|_| {}).unwrap_err();
    assert!(t0.elapsed() < Duration::from_secs(5));
    assert!(e.starts_with(&format!("GET https://127.0.0.1:{port}/:")), "{e}");
    // rustls itself rejects the plain-text answer
    assert!(e.contains("corrupt message"), "the handshake should fail in rustls: {e}");
}

#[test]
fn refusals_without_network() {
    let g = Grants::new("t", Caps { net: vec!["api.example.com".into()], secrets: vec!["s".into()], ..Caps::default() });
    let run = |req: serde_json::Value| http::job(&g, &Data::from_json(&req), &|| false, &|_| {}).unwrap_err();
    assert!(run(json!({ "url": "https://other.example.com/" })).contains("not declared"));
    assert!(run(json!({ "url": "http://api.example.com/" })).contains("plain http"));
    assert!(run(json!({ "url": "https://user:pw@api.example.com/" })).contains("user:password"));
    assert!(run(json!({ "url": "https://api.example.com/", "headers": { "Host": "evil" } })).contains("cannot be given"));
    assert!(run(json!({ "url": "https://api.example.com/", "secret": "s" })).contains("not set on this device"));
    assert!(run(json!({ "url": "https://api.example.com/", "secret": "t" })).contains("not declared"));
    assert!(run(json!({ "url": "https://api.example.com/", "method": "GET\r\nX: y" })).contains("not an HTTP method"));
    assert!(net::check(&g, "https://api.example.com.evil.com/").is_err());
}

#[test]
fn check_url_and_pure_helpers_from_luau() {
    let src = r#"
local ok = cloud.check_url("https://api.example.com/v1")
assert(ok.ok == true, "declared host")
local no = cloud.check_url("https://evil.example.org/")
assert(no.ok == false and string.find(no.error, "not declared", 1, true), "undeclared host")
assert(cloud.url_encode("a b/字") == "a%20b%2F%E5%AD%97", "url_encode")
assert(cloud.query({ b = "2", a = "x y" }) == "a=x%20y&b=2", "query")
assert(cloud.sha256("") == "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855", "sha256")
local evs = cloud.parse_sse("data: a\r\ndata: b\r\n\r\n: ping\n\nevent: x\ndata: [DONE]\n\n")
assert(#evs == 2 and evs[1].data == "a\nb" and evs[2].event == "x", "parse_sse")
Out = page.derive("out", {}, function() return cloud.check_url("https://api.example.com/").ok end)
"#;
    let mut p = load(r#"{ "modules": ["cloud"], "net": ["api.example.com"] }"#, src, secrets(&[])).unwrap();
    assert_eq!(p.cell_value("out"), Some(Data::Bool(true)));
    // without the module declared, nothing of it is reachable
    let e = load(r#"{ "net": ["api.example.com"] }"#, "cloud.check_url('https://api.example.com/')", secrets(&[])).err().unwrap();
    assert!(e.contains("\"cloud\" module is not declared"), "{e}");
    let e = load(r#"{ "net": ["api.example.com"] }"#, "page.job('j', 'cloud.http', {})", secrets(&[])).err().unwrap();
    assert!(e.contains("not declared"), "{e}");
}

#[test]
fn aws_signed_generic_request() {
    let mock = Mock::start(|r| {
        let auth = r.header("authorization").unwrap_or("").to_string();
        Reply::json(200, json!({ "auth": auth, "date": r.header("x-amz-date"), "token": r.header("x-amz-security-token") }))
    });
    let caps = caps(&[format!("127.0.0.1:{}", mock.port)], &["aws"]);
    let mut p = load(&caps, JOB, secrets(&[("aws", r#"{"access_key_id":"AKIDTEST","secret_access_key":"SECRETSECRET","session_token":"SESSIONTOKEN"}"#)])).unwrap();
    set(&mut p, "req", json!({ "url": mock.url("/model/x/invoke"), "json": { "prompt": "hi" }, "secret": "aws", "auth": "aws", "service": "bedrock", "region": "us-west-2" }));
    let r = job_result(&mut p, "res").unwrap();
    let got = &mock.requests()[0];
    let auth = got.header("authorization").unwrap();
    assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDTEST/"), "{auth}");
    assert!(auth.contains("/us-west-2/bedrock/aws4_request, SignedHeaders=content-type;host;x-amz-date;x-amz-security-token, Signature="), "{auth}");
    assert_eq!(got.header("x-amz-security-token"), Some("SESSIONTOKEN"));
    // the server echoed the session token: the page gets it scrubbed
    assert_eq!(r.get("json").get("token").as_str(), Some("[redacted]"));
    assert!(r.get("json").get("date").as_str().is_some());
}

/// A cancelled job (its input changed) gives the page's worker back at once:
/// with a single worker, the next request is not stuck behind a server that
/// never answers. The abandoned connection is closed right away too.
#[test]
fn cancelled_job_frees_the_worker_at_once() {
    let mock = Mock::start(|r| if r.path() == "/hang" { Reply::Hang(Duration::from_secs(8)) } else { Reply::json(200, json!({ "fast": true })) });
    let page_id = "cloud-test-one-worker";
    let mut p = load_with(&caps(&[format!("127.0.0.1:{}", mock.port)], &[]), JOB, secrets(&[]), page_id, 1).unwrap();
    set(&mut p, "req", json!({ "url": mock.url("/hang") }));
    // run the job until the request is waiting at the server
    let id = cell(&p, "res");
    let t0 = Instant::now();
    while mock.count(|r| r.path() == "/hang") == 0 {
        let g = p.parts().0;
        g.commit();
        g.demand(id);
        assert!(t0.elapsed() < Duration::from_secs(5), "the first request never reached the server");
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(100));
    let t1 = Instant::now();
    set(&mut p, "req", json!({ "url": mock.url("/fast") }));
    let r = wait_for(&mut p, "res", 5, |d| d.get("json").get("fast").truthy());
    let waited = t1.elapsed();
    assert_eq!(r.get("status").num(), Some(200.0));
    eprintln!("cloud.http, 1 worker: the next result came {:.1} ms after cancelling a request blocked on a silent server", waited.as_secs_f64() * 1000.0);
    assert!(waited < Duration::from_millis(1500), "the worker stayed blocked: {waited:?}");
    // the cancelled request's socket was shut down, its thread ended
    assert!(mock.eventually(2.0, |m| m.hangups.load(std::sync::atomic::Ordering::Relaxed) >= 1), "the abandoned connection stayed open");
    let t2 = Instant::now();
    while anypage_cloud::jobs::live_requests(page_id) > 0 && t2.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(anypage_cloud::jobs::live_requests(page_id), 0, "a request thread lingers");
}

#[test]
fn idle_timeout_ends_a_silent_exchange() {
    let mock = Mock::start(|_| Reply::Hang(Duration::from_secs(5)));
    let mut p = load(&caps(&[format!("127.0.0.1:{}", mock.port)], &[]), JOB, secrets(&[])).unwrap();
    let t0 = Instant::now();
    set(&mut p, "req", json!({ "url": mock.url("/slow"), "idle_timeout": 0.3 }));
    let e = job_result(&mut p, "res").unwrap_err();
    assert!(e.contains("timed out"), "{e}");
    assert!(t0.elapsed() < Duration::from_secs(3), "took {:?}", t0.elapsed());
}

/// A secret whose owner bound it to hosts (`"hosts"` in its JSON value) is
/// only sent there, even to another host the page declares.
#[test]
fn secrets_bound_to_hosts() {
    let a = Mock::start(|r| Reply::json(200, json!({ "got": r.header("authorization").is_some() })));
    let b = Mock::start(|_| Reply::json(200, json!({})));
    let bound = format!(r#"{{"value": "tok-bound-123456", "hosts": ["127.0.0.1:{}"]}}"#, a.port);
    let net = [format!("127.0.0.1:{}", a.port), format!("127.0.0.1:{}", b.port)];
    let mut p = load(&caps(&net, &["k"]), JOB, secrets(&[("k", &bound)])).unwrap();
    set(&mut p, "req", json!({ "url": a.url("/x"), "secret": "k" }));
    let r = job_result(&mut p, "res").unwrap();
    assert_eq!(r.get("json").get("got").as_bool(), Some(true));
    assert_eq!(a.requests()[0].header("authorization"), Some("Bearer tok-bound-123456"));
    set(&mut p, "req", json!({ "url": b.url("/x"), "secret": "k", "n": 2 }));
    let e = job_result(&mut p, "res").unwrap_err();
    assert!(e.contains("secret \"k\" is bound to") && e.contains("may not be sent to 127.0.0.1"), "{e}");
    assert!(!e.contains("tok-bound"), "{e}");
    assert_eq!(b.requests().len(), 0, "the bound secret's request reached another host");
}

/// Credentials echoed back (in the body, in a header, base64 encoded) never
/// reach the script.
#[test]
fn echoed_credentials_are_scrubbed_from_responses() {
    use base64::Engine;
    let mock = Mock::start(|r| {
        let auth = r.header("authorization").unwrap_or("").to_string();
        let basic = auth.strip_prefix("Basic ").unwrap_or("").to_string();
        let decoded = String::from_utf8(base64::engine::general_purpose::STANDARD.decode(&basic).unwrap_or_default()).unwrap_or_default();
        Reply::Full {
            status: 200,
            headers: vec![("Content-Type".into(), "application/json".into()), ("X-Echo".into(), auth.clone())],
            body: json!({ "auth": auth, "b64": basic, "plain": decoded }).to_string().into_bytes(),
        }
    });
    let mut p = load(&caps(&[format!("127.0.0.1:{}", mock.port)], &["login"]), JOB, secrets(&[("login", "me:pa55word-xyz")])).unwrap();
    set(&mut p, "req", json!({ "url": mock.url("/whoami"), "secret": "login", "auth": "basic" }));
    let r = job_result(&mut p, "res").unwrap();
    let b64 = base64::engine::general_purpose::STANDARD.encode("me:pa55word-xyz");
    assert_eq!(mock.requests()[0].header("authorization"), Some(format!("Basic {b64}").as_str()));
    let j = r.get("json");
    assert_eq!(j.get("auth").as_str(), Some("[redacted]"));
    assert_eq!(j.get("b64").as_str(), Some("[redacted]"));
    assert_eq!(j.get("plain").as_str(), Some("[redacted]"));
    assert_eq!(r.get("headers").get("x-echo").as_str(), Some("[redacted]"));
    let text = r.get("text").as_str().unwrap();
    let body = String::from_utf8_lossy(r.get("body").as_bytes().unwrap()).into_owned();
    for s in [text, body.as_str()] {
        assert!(!s.contains("pa55word") && !s.contains(&b64), "{s}");
    }
}
