//! Generic requests from pages: the `cloud.http` job and the `cloud.sse`
//! event-stream source.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anypage_runtime::{Data, Grants, SourceCtx, SourceGuard};
use serde_json::Value as Json;

use crate::auth;
use crate::cancel::{self, Token};
use crate::net::{self, Redirects, Request, Timeouts};
use crate::sse;
use crate::stream::{next_request, Gate, Guard, Next, Workers};
use crate::util::{build_query, Scrubber};

/// Largest event a `cloud.sse` stream accepts by default (`max_event`).
pub const DEFAULT_MAX_EVENT: usize = 8 << 20;
/// Largest `max_event` a page may ask for.
pub const HARD_MAX_EVENT: usize = 64 << 20;

/// Largest response a job may ask for with `max_bytes`.
pub fn max_bytes(req: &Data) -> Result<usize, String> {
    match req.get("max_bytes").num() {
        None => Ok(net::DEFAULT_MAX_RESPONSE),
        Some(n) if n.is_finite() && n >= 1.0 => Ok((n as usize).min(net::HARD_MAX_RESPONSE)),
        Some(_) => Err("max_bytes must be a positive number".into()),
    }
}

/// `{ method, url, headers, body | json | form, secret, auth, timeout,
/// follow_redirects, region, service, scope }` to a request with
/// credentials applied.
pub fn build(grants: &Grants, req: &Data, cancel: &dyn Fn() -> bool) -> Result<Request, String> {
    if !matches!(req, Data::Map(_)) {
        return Err("a request is a table { url, method, headers, body, ... }".into());
    }
    let url = req.get("url").as_str().ok_or("the request needs a url")?;
    let has_body = !req.get("body").is_nil() || !req.get("json").is_nil() || !req.get("form").is_nil();
    let method = req.get("method").as_str().unwrap_or(if has_body { "POST" } else { "GET" }).to_ascii_uppercase();
    let mut r = Request::new(&method, url);
    r.headers = auth::page_headers(req.get("headers"))?;
    let has_header = |r: &Request, n: &str| r.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(n));
    if !req.get("json").is_nil() {
        let j = req.get("json").to_json();
        r.body = Some(Arc::new(serde_json::to_vec(&j).map_err(|e| format!("json: {e}"))?));
        if !has_header(&r, "content-type") {
            r.headers.push(("Content-Type".into(), "application/json".into()));
        }
    } else if let Data::Map(_) = req.get("form") {
        let pairs: Vec<(String, String)> = req
            .get("form")
            .entries()
            .map(|(k, v)| {
                let v = match v {
                    Data::Str(s) => s.to_string(),
                    other => other.to_json().to_string(),
                };
                (k.to_string(), v)
            })
            .collect();
        r.body = Some(Arc::new(build_query(&pairs).into_bytes()));
        if !has_header(&r, "content-type") {
            r.headers.push(("Content-Type".into(), "application/x-www-form-urlencoded".into()));
        }
    } else if !req.get("body").is_nil() {
        let b = req.get("body").as_bytes().ok_or("body must be a string, bytes or a byte resource")?;
        if b.len() > net::MAX_REQUEST_BODY {
            return Err(format!("request body larger than {}", net::size_text(net::MAX_REQUEST_BODY)));
        }
        r.body = Some(Arc::new(b.to_vec()));
    }
    let body_len = r.body.as_ref().map_or(0, |b| b.len());
    r.timeouts = Timeouts::for_job(req, max_bytes(req)?.saturating_add(body_len))?;
    if req.get("follow_redirects").as_bool() == Some(false) {
        r.redirects = Redirects::Return;
    }
    auth::apply(grants, &mut r, req.get("secret").as_str(), req.get("auth").as_str(), req, cancel)?;
    Ok(r)
}

fn headers_data(h: &[(String, String)], scrub: &Scrubber) -> Data {
    let mut m: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for (k, v) in h {
        let v = scrub.text(v).unwrap_or_else(|| v.clone());
        m.entry(k.to_ascii_lowercase()).and_modify(|e| {
            e.push_str(", ");
            e.push_str(&v);
        }).or_insert(v);
    }
    Data::map(m.iter().map(|(k, v)| (k.as_str(), Data::str(v))))
}

/// The `cloud.http` job body. Everything returned (headers, body, text,
/// json, url) is scrubbed of the request's credentials.
pub fn job(grants: &Grants, req: &Data, cancel: &dyn Fn() -> bool, progress: &dyn Fn(f32)) -> Result<Data, String> {
    if req.is_nil() {
        return Ok(Data::Nil);
    }
    let max = max_bytes(req)?;
    let r = build(grants, req, cancel)?;
    let resp = net::send(grants, &r, cancel)?;
    let scrub = resp.scrubber();
    let status = resp.status;
    let headers = headers_data(&resp.headers, &scrub);
    let ctype = resp.content_type();
    let url = resp.url.display();
    let url = scrub.text(&url).unwrap_or(url);
    let body = scrub.bytes(resp.read_all(max, cancel, progress)?);
    let mut m = vec![("status", Data::Num(status as f64)), ("ok", Data::Bool((200..300).contains(&status))), ("headers", headers), ("url", Data::str(&url))];
    if let Ok(t) = std::str::from_utf8(&body) {
        m.push(("text", Data::str(t)));
        let looks_json = ctype.contains("json") || matches!(t.trim_start().as_bytes().first(), Some(b'{' | b'['));
        if looks_json && body.len() <= 16 << 20 {
            if let Ok(j) = serde_json::from_str::<Json>(t) {
                m.push(("json", Data::from_json(&j)));
            }
        }
    }
    m.push(("body", Data::bytes(body)));
    Ok(Data::map(m))
}

// ---------------------------------------------------------------- cloud.sse

/// `cloud.sse`: request `{ url, method, headers, body | json, secret, auth,
/// timeout, idle_timeout, max_event, max_bytes }` (or nil to stop) → one item
/// per event `{ seq, event, data, id, json }`, then `{ seq, done = true,
/// status, error, cancelled }`.
pub fn sse_source(ctx: SourceCtx) -> Result<SourceGuard, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let guard = Guard { stop: stop.clone() };
    std::thread::Builder::new().name(format!("cloud.sse {}", ctx.label)).spawn(move || sse_controller(ctx, stop)).map_err(|e| format!("cannot start the event-stream thread: {e}"))?;
    Ok(Box::new(guard))
}

fn end_item(seq: u64, status: Option<u16>, error: Option<String>, cancelled: bool) -> Data {
    let mut m = vec![("seq", Data::Num(seq as f64)), ("done", Data::Bool(true))];
    if let Some(s) = status {
        m.push(("status", Data::Num(s as f64)));
    }
    if let Some(e) = error {
        m.push(("error", Data::str(&e)));
    }
    if cancelled {
        m.push(("cancelled", Data::Bool(true)));
    }
    Data::map(m)
}

/// Timeouts for an event stream: no total; `timeout` caps connecting, the
/// wait for the headers and each silence; `idle_timeout` sets the silence.
fn stream_timeouts(req: &Data) -> Result<Timeouts, String> {
    let job = Timeouts::for_job(req, 0)?;
    let mut t = Timeouts::stream();
    if !req.get("timeout").is_nil() {
        t.connect = job.connect;
        t.response = job.response;
        t.idle = t.idle.min(job.response);
    }
    if !req.get("idle_timeout").is_nil() {
        t.idle = job.idle;
    }
    Ok(t)
}

/// `max_event` (bytes per event) and `max_bytes` (bytes over the stream's
/// whole life, none by default).
fn stream_limits(req: &Data) -> Result<(usize, Option<usize>), String> {
    let positive = |name: &str| -> Result<Option<usize>, String> {
        match req.get(name) {
            Data::Nil => Ok(None),
            Data::Num(n) if n.is_finite() && *n >= 1.0 => Ok(Some(*n as usize)),
            _ => Err(format!("{name} must be a positive number of bytes")),
        }
    };
    let max_event = positive("max_event")?.unwrap_or(DEFAULT_MAX_EVENT).min(HARD_MAX_EVENT);
    Ok((max_event, positive("max_bytes")?))
}

/// One event as the item pages see, scrubbed of the request's credentials.
fn event_item(gen: u64, e: &sse::Event, scrub: &Scrubber) -> Data {
    let clean = |s: &str| scrub.text(s).unwrap_or_else(|| s.to_string());
    let data = clean(&e.data);
    let mut m = vec![("seq", Data::Num(gen as f64)), ("event", Data::str(&clean(&e.event)))];
    if let Some(id) = &e.id {
        m.push(("id", Data::str(&clean(id))));
    }
    if let Ok(j) = serde_json::from_str::<Json>(&data) {
        if j.is_object() || j.is_array() {
            m.push(("json", Data::from_json(&j)));
        }
    }
    m.push(("data", Data::str(&data)));
    Data::map(m)
}

fn sse_controller(ctx: SourceCtx, stop: Arc<AtomicBool>) {
    let gate = Arc::new(Gate::default());
    let dead = Arc::new(AtomicBool::new(false));
    let workers = Workers::default();
    let mut current: Option<Arc<Token>> = None;
    loop {
        let req = match next_request(&ctx, &stop, &mut current) {
            Next::Quit => {
                dead.store(true, Ordering::Relaxed);
                return;
            }
            Next::Cancel => continue,
            Next::Request(r) => r,
        };
        let gen = gate.next();
        let slot = match workers.admit(&stop) {
            Ok(s) => s,
            Err(e) => {
                gate.push(&ctx.sink, gen, end_item(gen, None, Some(e), false), true, &dead);
                continue;
            }
        };
        let token = Token::new();
        current = Some(token.clone());
        let (sink, gate2, grants, dead2) = (ctx.sink.clone(), gate.clone(), ctx.grants.clone(), dead.clone());
        let spawned = std::thread::Builder::new().name(format!("cloud.sse #{gen}")).spawn(move || {
            let _slot = slot;
            let is_cancelled = || token.is_cancelled();
            let mut status = None;
            let r = cancel::scope(&token, || crate::guarded("cloud.sse", || -> Result<(), String> {
                let mut r = build(&grants, &req, &is_cancelled)?;
                r.timeouts = stream_timeouts(&req)?;
                let (max_event, max_bytes) = stream_limits(&req)?;
                if !r.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("accept")) {
                    r.headers.push(("Accept".into(), "text/event-stream".into()));
                }
                let resp = net::send(&grants, &r, &is_cancelled)?;
                status = Some(resp.status);
                let scrub = resp.scrubber();
                if !resp.ok() {
                    let (s, shown) = (resp.status, resp.url.display());
                    let b = resp.read_all(1 << 20, &is_cancelled, &|_| {}).unwrap_or_default();
                    return Err(format!("HTTP {s} from {shown}: {}", net::error_text(&b)));
                }
                let mut rd = resp.reader();
                // lines and events are capped; the stream as a whole is not
                // (unless max_bytes asks): live feeds run for hours
                let mut p = sse::Parser::with_limits(max_event, max_event);
                let mut evs = Vec::new();
                let mut buf = vec![0u8; 16 * 1024];
                let mut total = 0usize;
                loop {
                    if is_cancelled() {
                        return Ok(());
                    }
                    let n = rd.read(&mut buf)?;
                    total = total.saturating_add(n);
                    if let Some(max) = max_bytes.filter(|m| total > *m) {
                        return Err(format!("event stream longer than max_bytes ({})", net::size_text(max)));
                    }
                    evs.clear();
                    p.push(&buf[..n], &mut evs)?;
                    if n == 0 {
                        p.finish(&mut evs)?;
                    }
                    for e in evs.drain(..) {
                        if is_cancelled() {
                            return Ok(());
                        }
                        gate2.push(&sink, gen, event_item(gen, &e, &scrub), true, &dead2);
                    }
                    if n == 0 {
                        return Ok(());
                    }
                }
            }));
            if dead2.load(Ordering::Relaxed) {
                return;
            }
            let cancelled = token.is_cancelled();
            let err = if cancelled { None } else { r.err().map(|e| auth::scrub(&grants, req.get("secret").as_str(), e)) };
            gate2.push(&sink, gen, end_item(gen, status, err, cancelled), true, &dead2);
        });
        if let Err(e) = spawned {
            gate.push(&ctx.sink, gen, end_item(gen, None, Some(format!("cannot start a request thread: {e}")), false), true, &dead);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anypage_runtime::Caps;

    #[test]
    fn builds() {
        let g = Grants::new("t", Caps { net: vec!["api.x.com".into()], ..Caps::default() });
        let req = Data::map([("url", Data::str("https://api.x.com/a")), ("json", Data::map([("k", Data::Num(1.0))]))]);
        let r = build(&g, &req, &|| false).unwrap();
        assert_eq!(r.method, "POST");
        assert_eq!(r.body.as_deref().map(Vec::as_slice), Some(&b"{\"k\":1}"[..]));
        assert!(r.headers.contains(&("Content-Type".to_string(), "application/json".to_string())));
        let req = Data::map([("url", Data::str("https://api.x.com/a")), ("form", Data::map([("a b", Data::str("c&d"))]))]);
        assert_eq!(build(&g, &req, &|| false).unwrap().body.as_deref().map(Vec::as_slice), Some(&b"a%20b=c%26d"[..]));
        assert!(build(&g, &Data::map([("method", Data::str("GET"))]), &|| false).is_err());
        assert!(build(&g, &Data::map([("url", Data::str("https://api.x.com")), ("timeout", Data::Num(-1.0))]), &|| false).is_err());
        assert_eq!(max_bytes(&Data::map([("max_bytes", Data::Num(1e12))])).unwrap(), net::HARD_MAX_RESPONSE);
        let h = headers_data(&[("Set-Cookie".into(), "a".into()), ("set-cookie".into(), "b".into())], &Scrubber::default());
        assert_eq!(h.get("set-cookie").as_str(), Some("a, b"));
        let h = headers_data(&[("X-Echo".into(), "Bearer tok-secret-123".into())], &Scrubber::new(["tok-secret-123"]));
        assert_eq!(h.get("x-echo").as_str(), Some("Bearer [redacted]"));
        // sse: per-event cap and an optional lifetime cap; timeouts without a total
        assert_eq!(stream_limits(&Data::Nil).unwrap(), (DEFAULT_MAX_EVENT, None));
        assert_eq!(stream_limits(&Data::map([("max_event", Data::Num(1e12)), ("max_bytes", Data::Num(5.0))])).unwrap(), (HARD_MAX_EVENT, Some(5)));
        assert!(stream_limits(&Data::map([("max_event", Data::Num(0.0))])).is_err());
        let t = stream_timeouts(&Data::map([("idle_timeout", Data::Num(2.0))])).unwrap();
        assert_eq!((t.idle.as_secs(), t.total), (2, None));
        let t = stream_timeouts(&Data::map([("timeout", Data::Num(3.0))])).unwrap();
        assert_eq!((t.connect.as_secs(), t.response.as_secs(), t.idle.as_secs(), t.total), (3, 3, 3, None));
    }
}
