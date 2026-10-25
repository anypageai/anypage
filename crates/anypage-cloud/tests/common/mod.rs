//! Test helpers: a tiny HTTP/1.1 server on 127.0.0.1 and headless pages.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anypage_core::{CellId, Status};
use anypage_engine::{Page, TextRaster, Wake};
use anypage_runtime::{Data, LoadOptions, Natives, Package, ScriptPage, SecretStore};

#[derive(Clone, Debug, Default)]
pub struct MockRequest {
    pub method: String,
    /// Path and query as sent.
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl MockRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("")
    }
    pub fn query(&self) -> &str {
        self.target.split_once('?').map(|(_, q)| q).unwrap_or("")
    }
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

pub enum Reply {
    Full { status: u16, headers: Vec<(String, String)>, body: Vec<u8> },
    /// Chunked transfer encoding, a pause before each chunk.
    Chunks { status: u16, headers: Vec<(String, String)>, chunks: Vec<Vec<u8>>, delay: Duration },
    /// Accept the request and say nothing for this long (or until the
    /// client goes away, counted in `hangups`).
    Hang(Duration),
    /// Chunked, each chunk after its own pause.
    Timed { status: u16, headers: Vec<(String, String)>, chunks: Vec<(Duration, Vec<u8>)> },
    /// Send `chunk` every `delay` until the client goes away.
    Endless { headers: Vec<(String, String)>, chunk: Vec<u8>, delay: Duration },
}

impl Reply {
    pub fn json(status: u16, v: serde_json::Value) -> Reply {
        Reply::Full { status, headers: vec![("Content-Type".into(), "application/json".into())], body: v.to_string().into_bytes() }
    }
    pub fn text(status: u16, ctype: &str, body: impl Into<Vec<u8>>) -> Reply {
        Reply::Full { status, headers: vec![("Content-Type".into(), ctype.into())], body: body.into() }
    }
    pub fn redirect(status: u16, location: &str) -> Reply {
        Reply::Full { status, headers: vec![("Location".into(), location.into())], body: Vec::new() }
    }
    /// An SSE body cut into pieces of `piece` bytes, sent chunked.
    pub fn sse(body: &str, piece: usize, delay: Duration) -> Reply {
        let chunks = body.as_bytes().chunks(piece.max(1)).map(<[u8]>::to_vec).collect();
        Reply::Chunks { status: 200, headers: vec![("Content-Type".into(), "text/event-stream".into()), ("Cache-Control".into(), "no-cache".into())], chunks, delay }
    }
}

type Handler = dyn Fn(&MockRequest) -> Reply + Send + Sync;

pub struct Mock {
    pub port: u16,
    requests: Arc<Mutex<Vec<MockRequest>>>,
    stop: Arc<AtomicBool>,
    /// Connections that ended because the client went away mid-stream.
    pub hangups: Arc<AtomicUsize>,
    /// Connections being served right now.
    pub open: Arc<AtomicUsize>,
}

struct OpenGuard(Arc<AtomicUsize>);

impl Drop for OpenGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Mock {
    pub fn start(handler: impl Fn(&MockRequest) -> Reply + Send + Sync + 'static) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1");
        let port = listener.local_addr().expect("addr").port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let hangups = Arc::new(AtomicUsize::new(0));
        let open = Arc::new(AtomicUsize::new(0));
        let handler: Arc<Handler> = Arc::new(handler);
        let (r2, s2, h2, o2) = (requests.clone(), stop.clone(), hangups.clone(), open.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if s2.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(conn) = conn else { continue };
                let (handler, requests, stop, hangups) = (handler.clone(), r2.clone(), s2.clone(), h2.clone());
                o2.fetch_add(1, Ordering::SeqCst);
                let guard = OpenGuard(o2.clone());
                std::thread::spawn(move || {
                    let _guard = guard;
                    serve(conn, &*handler, &requests, &stop, &hangups)
                });
            }
        });
        Mock { port, requests, stop, hangups, open }
    }

    /// Connections being served right now.
    pub fn open_now(&self) -> usize {
        self.open.load(Ordering::SeqCst)
    }

    /// Wait up to `secs` for `f` to hold.
    pub fn eventually(&self, secs: f64, f: impl Fn(&Mock) -> bool) -> bool {
        let t0 = Instant::now();
        while t0.elapsed().as_secs_f64() < secs {
            if f(self) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        f(self)
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    pub fn requests(&self) -> Vec<MockRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub fn count(&self, pred: impl Fn(&MockRequest) -> bool) -> usize {
        self.requests.lock().unwrap().iter().filter(|r| pred(r)).count()
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

fn read_request(conn: &TcpStream) -> Option<MockRequest> {
    conn.set_read_timeout(Some(Duration::from_secs(10))).ok()?;
    let mut r = BufReader::new(conn.try_clone().ok()?);
    let mut line = String::new();
    r.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        r.read_line(&mut h).ok()?;
        let h = h.trim_end_matches(['\r', '\n']);
        if h.is_empty() {
            break;
        }
        let (k, v) = h.split_once(':')?;
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }
    let get = |n: &str| headers.iter().find(|(k, _): &&(String, String)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v.clone());
    let mut body = Vec::new();
    if let Some(n) = get("content-length").and_then(|v| v.parse::<usize>().ok()) {
        body.resize(n, 0);
        r.read_exact(&mut body).ok()?;
    } else if get("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        loop {
            let mut size = String::new();
            r.read_line(&mut size).ok()?;
            let n = usize::from_str_radix(size.trim(), 16).ok()?;
            let mut chunk = vec![0; n + 2];
            r.read_exact(&mut chunk).ok()?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
    }
    Some(MockRequest { method, target, headers, body })
}

fn head(status: u16, headers: &[(String, String)], extra: &str) -> String {
    let mut s = format!("HTTP/1.1 {status} {}\r\nConnection: close\r\n", reason(status));
    for (k, v) in headers {
        s.push_str(&format!("{k}: {v}\r\n"));
    }
    s.push_str(extra);
    s.push_str("\r\n");
    s
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        307 => "Temporary Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

fn serve(mut conn: TcpStream, handler: &Handler, requests: &Mutex<Vec<MockRequest>>, stop: &AtomicBool, hangups: &AtomicUsize) {
    let Some(req) = read_request(&conn) else { return };
    requests.lock().unwrap().push(req.clone());
    let _ = conn.set_write_timeout(Some(Duration::from_secs(10)));
    // tiny streamed chunks must not wait for Nagle / delayed ACKs
    let _ = conn.set_nodelay(true);
    match handler(&req) {
        Reply::Full { status, headers, body } => {
            let h = head(status, &headers, &format!("Content-Length: {}\r\n", body.len()));
            let _ = conn.write_all(h.as_bytes());
            if req.method != "HEAD" {
                let _ = conn.write_all(&body);
            }
        }
        Reply::Chunks { status, headers, chunks, delay } => {
            let h = head(status, &headers, "Transfer-Encoding: chunked\r\n");
            let _ = conn.write_all(h.as_bytes());
            let _ = conn.flush();
            for c in chunks {
                if !delay.is_zero() {
                    std::thread::sleep(delay);
                }
                let mut out = format!("{:x}\r\n", c.len()).into_bytes();
                out.extend_from_slice(&c);
                out.extend_from_slice(b"\r\n");
                if conn.write_all(&out).is_err() || conn.flush().is_err() {
                    hangups.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            }
            let _ = conn.write_all(b"0\r\n\r\n");
        }
        Reply::Hang(d) => {
            // the request was read whole: any read now ends only when the
            // client closes (0 bytes) or resets the connection
            let _ = conn.set_read_timeout(Some(Duration::from_millis(20)));
            let t0 = Instant::now();
            let mut b = [0u8; 64];
            while t0.elapsed() < d && !stop.load(Ordering::Relaxed) {
                match conn.read(&mut b) {
                    Ok(0) => {
                        hangups.fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                    Ok(_) => {}
                    Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                    Err(_) => {
                        hangups.fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                }
            }
        }
        Reply::Timed { status, headers, chunks } => {
            let h = head(status, &headers, "Transfer-Encoding: chunked\r\n");
            let _ = conn.write_all(h.as_bytes());
            let _ = conn.flush();
            for (pause, c) in chunks {
                std::thread::sleep(pause);
                let mut out = format!("{:x}\r\n", c.len()).into_bytes();
                out.extend_from_slice(&c);
                out.extend_from_slice(b"\r\n");
                if conn.write_all(&out).is_err() || conn.flush().is_err() {
                    hangups.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            }
            let _ = conn.write_all(b"0\r\n\r\n");
        }
        Reply::Endless { headers, chunk, delay } => {
            let h = head(200, &headers, "Transfer-Encoding: chunked\r\n");
            let _ = conn.write_all(h.as_bytes());
            let t0 = Instant::now();
            while !stop.load(Ordering::Relaxed) && t0.elapsed() < Duration::from_secs(30) {
                let mut out = format!("{:x}\r\n", chunk.len()).into_bytes();
                out.extend_from_slice(&chunk);
                out.extend_from_slice(b"\r\n");
                if conn.write_all(&out).is_err() || conn.flush().is_err() {
                    hangups.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                std::thread::sleep(delay);
            }
        }
    }
    let _ = conn.flush();
    let _ = conn.shutdown(std::net::Shutdown::Write);
    // let the client read everything before the socket closes
    let mut sink = [0u8; 256];
    let _ = conn.set_read_timeout(Some(Duration::from_millis(500)));
    while let Ok(n) = conn.read(&mut sink) {
        if n == 0 {
            break;
        }
    }
}

// ---------------------------------------------------------------- pages

pub struct Secrets(pub HashMap<String, String>);

impl SecretStore for Secrets {
    fn get(&self, _page: &str, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }
}

pub fn secrets(pairs: &[(&str, &str)]) -> Arc<dyn SecretStore> {
    Arc::new(Secrets(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()))
}

pub fn natives() -> Natives {
    let mut n = Natives::new();
    anypage_cloud::register(&mut n);
    n
}

pub fn text() -> Arc<TextRaster> {
    static T: std::sync::OnceLock<Arc<TextRaster>> = std::sync::OnceLock::new();
    T.get_or_init(|| Arc::new(TextRaster::load().expect("a system font"))).clone()
}

pub fn wake() -> Wake {
    Arc::new(|| {})
}

pub const PANEL: &str = "page.panel({ rect = { 0, 0, 10, 10 }, draw = function(d) end })\n";

/// Load a one-file page with `capabilities` (a JSON object) and secrets.
pub fn load(caps: &str, src: &str, secrets: Arc<dyn SecretStore>) -> Result<ScriptPage, String> {
    load_with(caps, src, secrets, "cloud-test", 4)
}

/// [`load`] with a page id and a number of job workers.
pub fn load_with(caps: &str, src: &str, secrets: Arc<dyn SecretStore>, id: &str, workers: usize) -> Result<ScriptPage, String> {
    let manifest = format!(r#"{{ "format": 1, "id": "{id}", "bounds": [0, 0, 400, 300], "capabilities": {caps}, "limits": {{ "job_workers": {workers} }} }}"#);
    let src = format!("{PANEL}{src}");
    let pkg = Package::from_files("test", [("page.json", manifest.as_str()), ("main.luau", src.as_str())])?;
    ScriptPage::load_with(&pkg, &natives(), text(), wake(), LoadOptions { secrets: Some(secrets) })
}

pub fn cell(p: &ScriptPage, name: &str) -> CellId {
    p.cell_id(name).unwrap_or_else(|| panic!("no cell {name}"))
}

/// Commit and demand `name` until `ok` holds (ready values only).
pub fn wait_for(p: &mut ScriptPage, name: &str, secs: u64, mut ok: impl FnMut(&Data) -> bool) -> Data {
    let id = cell(p, name);
    let t0 = Instant::now();
    loop {
        let g = p.parts().0;
        g.commit();
        let i = g.demand(id);
        let d = anypage_runtime::data::input_to_data(&i);
        if matches!(i.status, Status::Ready) && ok(&d) {
            return d;
        }
        if let Status::Error(e) = &i.status {
            if ok(&Data::map([("job_error", Data::str(e))])) {
                return Data::map([("job_error", Data::str(e))]);
            }
        }
        assert!(t0.elapsed() < Duration::from_secs(secs), "timed out waiting for {name}; last {d:?} {:?}", i.status);
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Wait for a job cell to finish: Ok(value) or Err(message).
pub fn job_result(p: &mut ScriptPage, name: &str) -> Result<Data, String> {
    let d = wait_for(p, name, 20, |d| !d.get("job_error").is_nil() || !d.is_nil());
    match d.get("job_error").as_str() {
        Some(e) => Err(e.to_string()),
        None => Ok(d),
    }
}

/// The `last` item of a stream cell once `ok` holds for it.
pub fn last_item(p: &mut ScriptPage, name: &str, secs: u64, mut ok: impl FnMut(&Data) -> bool) -> Data {
    wait_for(p, name, secs, |d| ok(d.get("last"))).get("last").clone()
}

/// Set an agent-writable input through the semantic port.
pub fn set(p: &mut ScriptPage, name: &str, v: serde_json::Value) {
    let r = p.act(&serde_json::json!({ "set": name, "value": v }).to_string());
    assert!(r.contains("\"ok\":true"), "{r}");
}
