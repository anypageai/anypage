//! HTTP(S) for page-initiated requests, with the page's capabilities enforced.
//!
//! Every request, and every redirect hop, goes through [`check`]:
//! * only `https`, or plain `http` to `localhost` / `127.0.0.1` / `[::1]`;
//! * no `user:password@` in URLs (credentials come from secrets);
//! * the host (and port, when the pattern names one) must be declared in
//!   `capabilities.net` ([`Grants::check_url`]).
//!
//! The URL is parsed once with the same parser ureq uses (`http::Uri`), and
//! the capability check is made on the normalised form built from that
//! parse, so the host that is checked is the host that is contacted.
//!
//! Redirects are followed here, not by ureq: each `Location` is resolved,
//! re-checked, and credentials (`secret_headers`) are only sent to the
//! origin the request started at. Response bodies are size-capped while they
//! are read, every phase has a timeout, and error messages never contain
//! secrets or query strings.
//!
//! Every connection goes through [`crate::cancel::CancelConnector`]: a
//! request made inside [`crate::cancel::scope`] is interrupted at once when
//! its token is cancelled, even while blocked waiting for the server.

use std::io::Read;
use std::sync::OnceLock;
use std::time::Duration;

use anypage_runtime::{Data, Grants};
use ureq::http::{self, Uri};
use ureq::unversioned::resolver::DefaultResolver;
use ureq::{Agent, Body};

use crate::cancel::CancelConnector;
use crate::util::{redact, Scrubber};

pub const MAX_URL: usize = 16 * 1024;
pub const MAX_REQUEST_BODY: usize = 256 << 20;
pub const DEFAULT_MAX_RESPONSE: usize = 32 << 20;
pub const HARD_MAX_RESPONSE: usize = 512 << 20;
pub const MAX_REDIRECTS: usize = 5;
pub const MAX_HEADERS: usize = 100;

/// A URL that passed the page's capability check, normalised.
#[derive(Clone, Debug)]
pub struct CheckedUrl {
    pub uri: Uri,
    /// "https" or "http"
    pub scheme: &'static str,
    /// Lower case; IPv6 literals keep their brackets.
    pub host: String,
    /// `None` when it is the scheme's default.
    pub port: Option<u16>,
    /// Path and query, at least "/".
    pub path_and_query: String,
}

impl CheckedUrl {
    /// `https://host[:port]`
    pub fn origin(&self) -> String {
        format!("{}://{}", self.scheme, self.authority())
    }
    /// `host[:port]`: the Host header value.
    pub fn authority(&self) -> String {
        match self.port {
            Some(p) => format!("{}:{p}", self.host),
            None => self.host.clone(),
        }
    }
    pub fn path(&self) -> &str {
        self.path_and_query.split('?').next().unwrap_or("/")
    }
    pub fn query(&self) -> Option<&str> {
        self.path_and_query.split_once('?').map(|(_, q)| q)
    }
    /// The full URL.
    pub fn as_string(&self) -> String {
        format!("{}{}", self.origin(), self.path_and_query)
    }
    /// For messages: no query string (it may carry signatures or keys).
    pub fn display(&self) -> String {
        format!("{}{}", self.origin(), self.path())
    }
    pub fn is_loopback(&self) -> bool {
        is_loopback(&self.host)
    }
}

pub fn is_loopback(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    h == "localhost" || h == "127.0.0.1" || h == "::1"
}

/// Validate `url` against the scheme rules and the page's `capabilities.net`.
pub fn check(grants: &Grants, url: &str) -> Result<CheckedUrl, String> {
    if url.len() > MAX_URL {
        return Err(format!("URL longer than {} KB", MAX_URL / 1024));
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control() || !c.is_ascii()) {
        return Err("URL contains spaces, control or non-ASCII characters; percent-encode them".into());
    }
    let shown = url.split(['?', '#']).next().unwrap_or(url);
    if has_scheme(url) {
        let scheme = url.split(':').next().unwrap_or("").to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            return Err(format!("{scheme}: only https (or http to localhost) URLs are allowed"));
        }
    }
    let uri: Uri = url.parse().map_err(|_| format!("not a valid URL: {shown}"))?;
    let scheme = match uri.scheme_str().map(str::to_ascii_lowercase).as_deref() {
        Some("https") => "https",
        Some("http") => "http",
        Some(other) => return Err(format!("{other}: only https (or http to localhost) URLs are allowed")),
        None => return Err(format!("not an absolute URL: {shown}")),
    };
    let auth = uri.authority().ok_or_else(|| format!("URL has no host: {shown}"))?;
    if auth.as_str().contains('@') {
        return Err("URLs with user:password@ are not allowed; use a secret".into());
    }
    let host = auth.host().to_ascii_lowercase();
    if host.is_empty() {
        return Err(format!("URL has no host: {shown}"));
    }
    // `http::Uri` drops a port it cannot parse; refuse it (and port 0)
    // rather than silently contacting the default port
    let a = auth.as_str();
    let after_host = if a.starts_with('[') { a.find(']').map(|i| &a[i + 1..]).unwrap_or("") } else { a.rfind(':').map(|i| &a[i..]).unwrap_or("") };
    if !after_host.is_empty() {
        let ok = after_host.strip_prefix(':').and_then(|p| p.parse::<u16>().ok()).is_some_and(|p| p > 0);
        if !ok || auth.port_u16().is_none() {
            return Err(format!("bad port in {shown}"));
        }
    }
    if scheme == "http" && !is_loopback(&host) {
        return Err(format!("{host}: plain http is only allowed to localhost; use https"));
    }
    let default = if scheme == "https" { 443 } else { 80 };
    let port = auth.port_u16().filter(|p| *p != default);
    let pq = uri.path_and_query().map(|p| p.as_str()).filter(|p| !p.is_empty()).unwrap_or("/");
    let pq = if pq.starts_with('/') { pq.to_string() } else { format!("/{pq}") };
    let c = CheckedUrl { uri: Uri::default(), scheme, host, port, path_and_query: pq };
    let canonical = c.as_string();
    grants.check_url(&canonical)?;
    let uri: Uri = canonical.parse().map_err(|_| format!("not a valid URL: {}", c.display()))?;
    Ok(CheckedUrl { uri, ..c })
}

/// Resolve a `Location` header against the URL it came from (RFC 3986 5.2,
/// for the forms servers send: absolute, scheme-relative, absolute-path and
/// relative-path references).
pub fn resolve(base: &CheckedUrl, location: &str) -> String {
    let loc = location.trim();
    let loc = loc.split('#').next().unwrap_or(loc);
    if has_scheme(loc) {
        return loc.to_string();
    }
    if let Some(rest) = loc.strip_prefix("//") {
        return format!("{}://{rest}", base.scheme);
    }
    if loc.starts_with('/') {
        return format!("{}{}", base.origin(), remove_dot_segments(loc));
    }
    if let Some(q) = loc.strip_prefix('?') {
        return format!("{}{}?{q}", base.origin(), base.path());
    }
    let (rel_path, rel_query) = match loc.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (loc, None),
    };
    let dir = match base.path().rfind('/') {
        Some(i) => &base.path()[..=i],
        None => "/",
    };
    let mut out = format!("{}{}", base.origin(), remove_dot_segments(&format!("{dir}{rel_path}")));
    if let Some(q) = rel_query {
        out.push('?');
        out.push_str(q);
    }
    out
}

/// `scheme:` at the start (RFC 3986: ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )).
fn has_scheme(s: &str) -> bool {
    match s.find(':') {
        Some(i) if i > 0 => {
            let scheme = &s[..i];
            scheme.starts_with(|c: char| c.is_ascii_alphabetic()) && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        }
        _ => false,
    }
}

fn remove_dot_segments(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let segs: Vec<&str> = path.split('/').collect();
    for (i, s) in segs.iter().enumerate() {
        match *s {
            "." => {
                if i == segs.len() - 1 {
                    out.push("");
                }
            }
            ".." => {
                if out.len() > 1 {
                    out.pop();
                }
                if i == segs.len() - 1 {
                    out.push("");
                }
            }
            s => out.push(s),
        }
    }
    let joined = out.join("/");
    if joined.starts_with('/') {
        joined
    } else {
        format!("/{joined}")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Redirects {
    /// Follow up to [`MAX_REDIRECTS`], re-checking each hop.
    Follow,
    /// Return 3xx responses as they are (signed requests).
    Return,
}

#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    pub connect: Duration,
    /// Until the response headers arrive.
    pub response: Duration,
    /// Longest silence while reading the body.
    pub idle: Duration,
    /// The whole exchange, body included; `None` for streams.
    pub total: Option<Duration>,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self { connect: Duration::from_secs(15), response: Duration::from_secs(120), idle: Duration::from_secs(60), total: Some(Duration::from_secs(120)) }
    }
}

/// The slowest average rate a job's default total timeout allows for.
pub const MIN_RATE: usize = 64 * 1024;

impl Timeouts {
    /// For long-lived streams (chat completions, event streams).
    pub fn stream() -> Self {
        Self { connect: Duration::from_secs(15), response: Duration::from_secs(300), idle: Duration::from_secs(180), total: None }
    }

    /// For a job moving up to `bytes` (request body plus the response
    /// limit). Defaults: 15 s to connect, 120 s for the response headers,
    /// 60 s of silence while sending or receiving, and a total of 120 s plus
    /// `bytes` at [`MIN_RATE`] (so a 256 MB upload gets about 70 minutes).
    /// The request may set `timeout` (seconds for the whole exchange; it also
    /// bounds every phase) and `idle_timeout` (seconds of silence).
    pub fn for_job(req: &Data, bytes: usize) -> Result<Self, String> {
        let secs = |name: &str, max: f64| -> Result<Option<Duration>, String> {
            match req.get(name) {
                Data::Nil => Ok(None),
                Data::Num(t) if t.is_finite() && *t > 0.0 => Ok(Some(Duration::from_secs_f64(t.min(max)))),
                _ => Err(format!("{name} must be a positive number of seconds")),
            }
        };
        let total = secs("timeout", 86_400.0)?;
        let idle = secs("idle_timeout", 3_600.0)?;
        let mut t = Timeouts::default();
        match total {
            Some(total) => {
                t.total = Some(total);
                t.connect = t.connect.min(total);
                t.response = total;
                t.idle = t.idle.min(total);
            }
            None => t.total = Some(Duration::from_secs(120 + (bytes / MIN_RATE) as u64)),
        }
        if let Some(i) = idle {
            t.idle = i;
        }
        Ok(t)
    }
}

#[derive(Clone)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// Credentials: sent only to the origin the request started at, and
    /// their values are redacted from every error message.
    pub secret_headers: Vec<(String, String)>,
    /// Other secret values to redact (keys used for signing).
    pub redact: Vec<String>,
    pub body: Option<std::sync::Arc<Vec<u8>>>,
    pub redirects: Redirects,
    pub timeouts: Timeouts,
}

/// Shows no secret header values, no body and no query string.
impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let url = self.url.split(['?', '#']).next().unwrap_or("");
        let secret: Vec<&str> = self.secret_headers.iter().map(|(k, _)| k.as_str()).collect();
        write!(f, "Request({} {url}, headers {:?}, secret headers {secret:?} (values hidden), body {} bytes)", self.method, self.headers, self.body.as_ref().map_or(0, |b| b.len()))
    }
}

impl Request {
    pub fn new(method: &str, url: &str) -> Self {
        Self {
            method: method.to_ascii_uppercase(),
            url: url.to_string(),
            headers: Vec::new(),
            secret_headers: Vec::new(),
            redact: Vec::new(),
            body: None,
            redirects: Redirects::Follow,
            timeouts: Timeouts::default(),
        }
    }
    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.to_string(), v.to_string()));
        self
    }
    pub fn body(mut self, b: Vec<u8>) -> Self {
        self.body = Some(std::sync::Arc::new(b));
        self
    }
    /// Every secret value in this request, for redaction.
    pub fn secrets(&self) -> Vec<&str> {
        self.secret_headers.iter().map(|(_, v)| v.as_str()).chain(self.secret_headers.iter().filter_map(|(_, v)| v.split_once(' ').map(|(_, t)| t))).chain(self.redact.iter().map(String::as_str)).collect()
    }
}

/// Response headers received; the body is still to be read.
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// The URL that answered (after redirects).
    pub url: CheckedUrl,
    body: Body,
    secrets: Vec<String>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
    pub fn content_type(&self) -> String {
        self.header("content-type").unwrap_or("").to_ascii_lowercase()
    }
    pub fn content_length(&self) -> Option<u64> {
        self.header("content-length").and_then(|v| v.trim().parse().ok())
    }

    /// Scrubs this request's credentials (and their base64 forms) out of
    /// whatever the response hands back to the page.
    pub fn scrubber(&self) -> Scrubber {
        Scrubber::new(self.secrets.iter())
    }

    /// A streaming reader over the body (no size cap: the caller counts).
    pub fn reader(self) -> BodyStream {
        BodyStream { reader: Box::new(self.body.into_reader()), secrets: self.secrets, display: self.url.display() }
    }

    /// Read the whole body, at most `max` bytes; `cancel` is polled between
    /// chunks and `progress` gets the fraction when the length is known.
    pub fn read_all(self, max: usize, cancel: &dyn Fn() -> bool, progress: &dyn Fn(f32)) -> Result<Vec<u8>, String> {
        let total = self.content_length();
        if let Some(n) = total {
            if n > max as u64 {
                return Err(format!("{}: response is {} bytes, more than the limit of {}", self.url.display(), n, size_text(max)));
            }
        }
        let mut r = self.reader();
        let mut out = Vec::with_capacity(total.unwrap_or(0).min(8 << 20) as usize);
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            if cancel() {
                return Err("cancelled".into());
            }
            let n = r.read(&mut buf)?;
            if n == 0 {
                break;
            }
            if out.len() + n > max {
                return Err(format!("{}: response larger than the limit of {}", r.display, size_text(max)));
            }
            out.extend_from_slice(&buf[..n]);
            if let Some(t) = total.filter(|t| *t > 0) {
                progress((out.len() as f64 / t as f64).min(1.0) as f32);
            }
        }
        Ok(out)
    }
}

pub fn size_text(n: usize) -> String {
    if n >= 1 << 20 {
        format!("{} MB", n >> 20)
    } else if n >= 1 << 10 {
        format!("{} KB", n >> 10)
    } else {
        format!("{n} bytes")
    }
}

/// The body of a response, read in pieces. Errors are redacted.
pub struct BodyStream {
    reader: Box<dyn Read + Send>,
    secrets: Vec<String>,
    pub display: String,
}

impl BodyStream {
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, String> {
        loop {
            match self.reader.read(buf) {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    let s: Vec<&str> = self.secrets.iter().map(String::as_str).collect();
                    return Err(redact(&format!("{}: reading the response failed: {}", self.display, describe_io(&e)), &s));
                }
            }
        }
    }
}

fn describe_io(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => "timed out waiting for data".into(),
        _ => e.to_string(),
    }
}

fn agent(loopback: bool) -> &'static Agent {
    static REMOTE: OnceLock<Agent> = OnceLock::new();
    static LOCAL: OnceLock<Agent> = OnceLock::new();
    let cell = if loopback { &LOCAL } else { &REMOTE };
    cell.get_or_init(|| {
        let mut b = Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .user_agent(concat!("anypage-cloud/", env!("CARGO_PKG_VERSION")))
            .max_response_header_size(64 * 1024)
            .max_idle_connections_per_host(4);
        if loopback {
            // never route local services through a proxy from the environment
            b = b.proxy(None);
        }
        Agent::with_parts(b.build(), CancelConnector::default(), DefaultResolver::default())
    })
}

const FORBIDDEN_HEADERS: [&str; 6] = ["host", "content-length", "transfer-encoding", "connection", "upgrade", "te"];

/// Check page-supplied header names: no hop-by-hop or framing headers.
pub fn check_header_name(name: &str) -> Result<(), String> {
    if FORBIDDEN_HEADERS.iter().any(|h| h.eq_ignore_ascii_case(name)) {
        return Err(format!("header \"{name}\" is set by the HTTP client and cannot be given"));
    }
    http::HeaderName::from_bytes(name.as_bytes()).map_err(|_| format!("\"{name}\" is not a valid header name"))?;
    Ok(())
}

fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// Send a request; returns once the response headers are in.
pub fn send(grants: &Grants, req: &Request, cancel: &dyn Fn() -> bool) -> Result<Response, String> {
    let secrets: Vec<String> = req.secrets().into_iter().map(str::to_string).collect();
    let red = |m: String| -> String {
        let s: Vec<&str> = secrets.iter().map(String::as_str).collect();
        redact(&m, &s)
    };
    if req.headers.len() + req.secret_headers.len() > MAX_HEADERS {
        return Err(format!("more than {MAX_HEADERS} headers"));
    }
    if let Some(b) = &req.body {
        if b.len() > MAX_REQUEST_BODY {
            return Err(format!("request body larger than {}", size_text(MAX_REQUEST_BODY)));
        }
    }
    let method = http::Method::from_bytes(req.method.as_bytes()).map_err(|_| format!("\"{}\" is not an HTTP method", req.method))?;
    let mut url = check(grants, &req.url).map_err(red)?;
    let first_origin = url.origin();
    let mut method = method;
    let mut body = req.body.clone();
    let mut dropped_body_headers = false;
    for hop in 0..=MAX_REDIRECTS {
        if cancel() {
            return Err("cancelled".into());
        }
        let same_origin = url.origin() == first_origin;
        let mut b = http::Request::builder().method(method.clone()).uri(url.uri.clone());
        for (k, v) in &req.headers {
            if dropped_body_headers && (k.eq_ignore_ascii_case("content-type") || k.eq_ignore_ascii_case("content-encoding")) {
                continue;
            }
            b = b.header(k.as_str(), v.as_str());
        }
        if same_origin {
            for (k, v) in &req.secret_headers {
                b = b.header(k.as_str(), v.as_str());
            }
        }
        let t = req.timeouts;
        let a = agent(url.is_loopback());
        let shown = url.display();
        // connect covers DNS too; idle bounds every read and write (headers,
        // body, TLS), so a stalled upload ends like a stalled download
        let sent = match &body {
            Some(bytes) => {
                let r = b.body(bytes.as_slice()).map_err(|e| red(format!("{shown}: bad request: {e}")))?;
                let r = a
                    .configure_request(r)
                    .timeout_resolve(Some(t.connect))
                    .timeout_connect(Some(t.connect))
                    .timeout_recv_response(Some(t.response))
                    .timeout_per_read(Some(t.idle))
                    .timeout_per_write(Some(t.idle))
                    .timeout_global(t.total)
                    .build();
                a.run(r)
            }
            None => {
                let r = b.body(()).map_err(|e| red(format!("{shown}: bad request: {e}")))?;
                let r = a
                    .configure_request(r)
                    .timeout_resolve(Some(t.connect))
                    .timeout_connect(Some(t.connect))
                    .timeout_recv_response(Some(t.response))
                    .timeout_per_read(Some(t.idle))
                    .timeout_per_write(Some(t.idle))
                    .timeout_global(t.total)
                    .build();
                a.run(r)
            }
        };
        let resp = sent.map_err(|e| red(format!("{} {shown}: {}", method, describe_ureq(&e))))?;
        let status = resp.status().as_u16();
        let headers: Vec<(String, String)> = resp.headers().iter().map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned())).collect();
        let location = headers.iter().find(|(k, _)| k == "location").map(|(_, v)| v.clone());
        if req.redirects == Redirects::Follow && is_redirect(status) {
            if let Some(loc) = location {
                if hop == MAX_REDIRECTS {
                    return Err(format!("{shown}: more than {MAX_REDIRECTS} redirects"));
                }
                let next = resolve(&url, &loc);
                let checked = check(grants, &next).map_err(|e| red(format!("{shown} redirected to a URL this page may not use: {e}")))?;
                if url.scheme == "https" && checked.scheme == "http" && !url.is_loopback() {
                    return Err(format!("{shown}: refusing a redirect from https to http"));
                }
                if status == 303 || (matches!(status, 301 | 302) && method == http::Method::POST) {
                    if method != http::Method::HEAD {
                        method = http::Method::GET;
                    }
                    body = None;
                    dropped_body_headers = true;
                }
                log::debug!(target: "anypage_cloud", "redirect {status} {shown} -> {}", checked.display());
                url = checked;
                continue;
            }
        }
        return Ok(Response { status, headers, url, body: resp.into_body(), secrets });
    }
    Err("too many redirects".into())
}

fn describe_ureq(e: &ureq::Error) -> String {
    match e {
        ureq::Error::Timeout(t) => format!("timed out ({t})"),
        ureq::Error::Io(io) => describe_io(io),
        ureq::Error::HostNotFound => "host not found".into(),
        ureq::Error::ConnectionFailed => "connection failed".into(),
        other => other.to_string(),
    }
}

/// The message inside a JSON error body (`{"error": {"message": ...}}`,
/// `{"error": "..."}`, `{"message": ...}`), else the start of the text.
pub fn error_text(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    if let Ok(j) = serde_json::from_slice::<serde_json::Value>(body) {
        let m = j
            .pointer("/error/message")
            .or_else(|| j.get("error").filter(|e| e.is_string()))
            .or_else(|| j.get("message"))
            .or_else(|| j.pointer("/error/status"))
            .or_else(|| j.get("error_description"))
            .and_then(|v| v.as_str());
        if let Some(m) = m {
            return crate::util::clip(m, 1000);
        }
    }
    crate::util::clip(text.trim(), 500)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anypage_runtime::Caps;

    fn grants(net: &[&str]) -> Grants {
        Grants::new("t", Caps { net: net.iter().map(|s| s.to_string()).collect(), ..Caps::default() })
    }

    #[test]
    fn checks() {
        let g = grants(&["api.openai.com", "*.amazonaws.com", "localhost:11434", "127.0.0.1"]);
        let c = check(&g, "https://API.openai.com:443/v1/chat?x=1").unwrap();
        assert_eq!(c.as_string(), "https://api.openai.com/v1/chat?x=1");
        assert_eq!(c.display(), "https://api.openai.com/v1/chat");
        assert!(check(&g, "https://b.s3.us-east-1.amazonaws.com").unwrap().path_and_query == "/");
        assert!(check(&g, "http://localhost:11434/v1").is_ok());
        assert!(check(&g, "http://127.0.0.1:9/x").is_ok());
        let e = check(&g, "http://api.openai.com/v1").unwrap_err();
        assert!(e.contains("plain http"), "{e}");
        let e = check(&g, "https://evil.com/").unwrap_err();
        assert!(e.contains("not declared"), "{e}");
        let e = check(&g, "https://user:pw@api.openai.com/").unwrap_err();
        assert!(e.contains("user:password"), "{e}");
        assert!(check(&g, "https://api.openai.com@evil.com/").is_err());
        assert!(check(&g, "ftp://api.openai.com/").unwrap_err().contains("only https"));
        assert!(check(&g, "https://api.openai.com/a b").is_err());
        assert!(check(&g, "https://api.openai.com/\u{4e2d}").is_err());
        assert!(check(&g, "/relative").is_err());
        assert!(check(&g, "http://localhost:8080/").unwrap_err().contains("not declared"));
        assert!(check(&g, "https://api.openai.com\\@evil.com/").is_err());
    }

    #[test]
    fn ports() {
        let g = grants(&["api.example.com", "[::1]"]);
        assert_eq!(check(&g, "https://api.example.com:8443/x").unwrap().port, Some(8443));
        assert_eq!(check(&g, "https://api.example.com:443/x").unwrap().port, None);
        for bad in ["https://api.example.com:99999/", "https://api.example.com:0/", "https://api.example.com:/", "https://api.example.com:44a/", "http://[::1]:0/", "http://[::1]:70000/"] {
            let e = check(&g, bad).unwrap_err();
            assert!(e.contains("bad port") || e.contains("not a valid URL"), "{bad}: {e}");
        }
        // IPv6 hosts can be declared ("[::1]" above), with any valid port
        for ok in ["http://[::1]:8080/", "http://[::1]/"] {
            assert!(check(&g, ok).is_ok(), "{ok}");
        }
        assert!(check(&grants(&["api.example.com"]), "http://[::1]/").unwrap_err().contains("not declared"));
    }

    #[test]
    fn job_timeouts() {
        let t = Timeouts::for_job(&Data::Nil, 32 << 20).unwrap();
        assert_eq!((t.connect.as_secs(), t.response.as_secs(), t.idle.as_secs(), t.total.map(|d| d.as_secs())), (15, 120, 60, Some(120 + 512)));
        let t = Timeouts::for_job(&Data::map([("timeout", Data::Num(0.5))]), 1 << 30).unwrap();
        assert_eq!((t.connect, t.response, t.idle, t.total), (Duration::from_millis(500), Duration::from_millis(500), Duration::from_millis(500), Some(Duration::from_millis(500))));
        let t = Timeouts::for_job(&Data::map([("timeout", Data::Num(7200.0)), ("idle_timeout", Data::Num(5.0))]), 0).unwrap();
        assert_eq!((t.response.as_secs(), t.idle.as_secs(), t.total.map(|d| d.as_secs())), (7200, 5, Some(7200)));
        assert!(Timeouts::for_job(&Data::map([("timeout", Data::Num(-1.0))]), 0).is_err());
        assert!(Timeouts::for_job(&Data::map([("idle_timeout", Data::str("x"))]), 0).is_err());
    }

    #[test]
    fn resolves_locations() {
        let g = grants(&["a.com", "b.com"]);
        let base = check(&g, "https://a.com/x/y/z?q=1").unwrap();
        assert_eq!(resolve(&base, "https://b.com/p"), "https://b.com/p");
        assert_eq!(resolve(&base, "//b.com/p"), "https://b.com/p");
        assert_eq!(resolve(&base, "/p?k=v#frag"), "https://a.com/p?k=v");
        assert_eq!(resolve(&base, "w"), "https://a.com/x/y/w");
        assert_eq!(resolve(&base, "../w?k"), "https://a.com/x/w?k");
        assert_eq!(resolve(&base, "./"), "https://a.com/x/y/");
        assert_eq!(resolve(&base, "?k=2"), "https://a.com/x/y/z?k=2");
        assert_eq!(resolve(&base, "/a/../../b"), "https://a.com/b");
    }

    #[test]
    fn error_bodies() {
        assert_eq!(error_text(br#"{"error":{"message":"bad key","type":"auth"}}"#), "bad key");
        assert_eq!(error_text(br#"{"error":"model not found"}"#), "model not found");
        assert_eq!(error_text(b"  plain  "), "plain");
    }

    #[test]
    fn header_names() {
        assert!(check_header_name("X-Custom").is_ok());
        assert!(check_header_name("Host").is_err());
        assert!(check_header_name("bad name").is_err());
    }
}
