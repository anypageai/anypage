//! HTTP for HLS: a fetcher that checks every URL (and every redirect) against
//! the page's grants, and a small HTTP/1.1 server that hands playlists and
//! segments to the local network only.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anypage_runtime::natives::url_host;
use anypage_runtime::Grants;

/// A segment or playlist larger than this is refused.
pub const MAX_BODY: usize = 64 << 20;
const MAX_REDIRECTS: usize = 5;

/// Loopback, private (RFC 1918), link-local or unique-local.
///
/// Carrier-grade NAT space (100.64.0.0/10, RFC 6598) is *not* local: on
/// mobile networks it is shared with other subscribers of the carrier. It is
/// accepted only where a request opts in with `tailnet = true` (overlay
/// networks such as Tailscale use that range); see [`Peers`].
pub fn is_local_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_local_ip(IpAddr::V4(v4));
            }
            let s = v6.segments();
            v6.is_loopback() || (s[0] & 0xFE00) == 0xFC00 || (s[0] & 0xFFC0) == 0xFE80
        }
    }
}

/// 100.64.0.0/10, shared address space (carrier-grade NAT, Tailscale).
pub fn is_cgnat_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (o[1] & 0xC0) == 64
        }
        IpAddr::V6(v6) => v6.to_ipv4_mapped().is_some_and(|v4| is_cgnat_ip(IpAddr::V4(v4))),
    }
}

/// Which addresses count as "the local network".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Peers {
    /// Also accept 100.64.0.0/10 (an overlay network such as Tailscale).
    pub tailnet: bool,
}

impl Peers {
    pub fn allows(self, ip: IpAddr) -> bool {
        is_local_ip(ip) || (self.tailnet && is_cgnat_ip(ip))
    }
}

/// Is `host` (from a URL) on this machine or the local network?
pub fn is_local_host(host: &str) -> bool {
    is_local_host_for(host, Peers::default())
}

fn is_local_host_for(host: &str, peers: Peers) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    if h == "localhost" || h.ends_with(".local") || h.ends_with(".localhost") {
        return true;
    }
    h.parse::<IpAddr>().map(|ip| peers.allows(ip)).unwrap_or(false)
}

/// The checks every outgoing request passes: http(s) only, HTTPS unless the
/// host is local, and the page's `capabilities.net`.
pub fn check_url(grants: &Grants, url: &str) -> Result<(), String> {
    check_url_for(grants, url, Peers::default())
}

/// [`check_url`] with an explicit local-network policy.
pub fn check_url_for(grants: &Grants, url: &str, peers: Peers) -> Result<(), String> {
    let lower = url.to_ascii_lowercase();
    let https = lower.starts_with("https://");
    if !https && !lower.starts_with("http://") {
        return Err(format!("only http and https URLs can be played: {}", short(url)));
    }
    let (host, _) = url_host(url).ok_or_else(|| format!("not a URL: {}", short(url)))?;
    if !https && !is_local_host_for(&host, peers) {
        let hint = match host.parse::<IpAddr>() {
            Ok(ip) if is_cgnat_ip(ip) => " (100.64.0.0/10 is shared carrier space; add tailnet = true for an overlay network)",
            _ => "",
        };
        return Err(format!("{host}: plain http is only allowed on the local network; use https{hint}"));
    }
    grants.check_url(url)
}

pub fn short(s: &str) -> String {
    if s.chars().count() > 120 {
        format!("{}...", s.chars().take(120).collect::<String>())
    } else {
        s.to_string()
    }
}

/// Outgoing GETs for one playback session.
///
/// A call blocks its thread until the server answers or a timeout passes
/// (5 s to connect, 10 s for the response head, 30 s for the body); `cancelled`
/// is polled between body reads. Callers run it on threads they never join
/// from an engine or UI thread.
pub struct Fetcher {
    agent: ureq::Agent,
    grants: Arc<Grants>,
    pub max_body: usize,
    /// Bytes received so far.
    pub bytes: AtomicU64,
    pub peers: Peers,
}

impl Fetcher {
    pub fn new(grants: Arc<Grants>) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(5)))
            .timeout_recv_response(Some(Duration::from_secs(10)))
            .timeout_recv_body(Some(Duration::from_secs(30)))
            .max_redirects(0)
            .http_status_as_error(false)
            .user_agent("AnyPage-media/0.1")
            .build();
        Self { agent: ureq::Agent::new_with_config(config), grants, max_body: MAX_BODY, bytes: AtomicU64::new(0), peers: Peers::default() }
    }

    /// GET `url` (optionally a byte range), following redirects that pass the
    /// same checks. Returns the final URL and the body. `cancelled` is polled
    /// while the body streams in.
    pub fn get(&self, url: &str, range: Option<(u64, u64)>, cancelled: &dyn Fn() -> bool) -> Result<(String, Vec<u8>), String> {
        let mut url = url.to_string();
        let last_byte = match range {
            Some((off, len)) => Some(off.checked_add(len.max(1) - 1).ok_or_else(|| format!("byte range {len}@{off} is out of range"))?),
            None => None,
        };
        for _ in 0..=MAX_REDIRECTS {
            check_url_for(&self.grants, &url, self.peers)?;
            let mut req = self.agent.get(&url);
            if let (Some((off, _)), Some(last)) = (range, last_byte) {
                req = req.header("Range", format!("bytes={off}-{last}"));
            }
            let resp = req.call().map_err(|e| format!("{}: {e}", short(&url)))?;
            let status = resp.status().as_u16();
            if (300..400).contains(&status) {
                let loc = resp.headers().get("location").and_then(|v| v.to_str().ok()).ok_or_else(|| format!("{}: redirect without a location", short(&url)))?;
                url = crate::hls::resolve(&url, loc);
                continue;
            }
            if !(200..300).contains(&status) {
                return Err(format!("{}: HTTP {status}", short(&url)));
            }
            if let Some(len) = resp.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|s| s.parse::<u64>().ok()) {
                if len > self.max_body as u64 {
                    return Err(format!("{}: {} MB is larger than the {} MB limit", short(&url), len >> 20, self.max_body >> 20));
                }
            }
            let mut reader = resp.into_body().into_reader();
            let mut body = Vec::new();
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                if cancelled() {
                    return Err("cancelled".into());
                }
                let n = reader.read(&mut buf).map_err(|e| format!("{}: {e}", short(&url)))?;
                if n == 0 {
                    break;
                }
                if body.len() + n > self.max_body {
                    return Err(format!("{}: larger than the {} MB limit", short(&url), self.max_body >> 20));
                }
                body.extend_from_slice(&buf[..n]);
                self.bytes.fetch_add(n as u64, Ordering::Relaxed);
            }
            if let (Some((off, len)), 200) = (range, status) {
                // the server ignored the range: cut it ourselves
                let off = usize::try_from(off).unwrap_or(usize::MAX);
                let end = off.saturating_add(usize::try_from(len).unwrap_or(usize::MAX)).min(body.len());
                body = body.get(off..end).map(<[u8]>::to_vec).unwrap_or_default();
            }
            return Ok((url, body));
        }
        Err(format!("{}: too many redirects", short(&url)))
    }
}

// ---------------------------------------------------------------- server

/// What a server answers for a path: (content type, body, cache control).
pub type Reply = (&'static str, Arc<Vec<u8>>, &'static str);

pub trait Handler: Send + Sync + 'static {
    fn get(&self, path: &str) -> Option<Reply>;
}

impl<F: Fn(&str) -> Option<Reply> + Send + Sync + 'static> Handler for F {
    fn get(&self, path: &str) -> Option<Reply> {
        self(path)
    }
}

const MAX_CONNECTIONS: usize = 32;
const MAX_HEAD: usize = 8 * 1024;

/// A local-network HTTP/1.1 server (GET and HEAD, keep-alive).
pub struct Server {
    pub addr: SocketAddr,
    quit: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pub requests: Arc<AtomicU64>,
    pub refused: Arc<AtomicU64>,
    pub active: Arc<AtomicUsize>,
}

/// How long [`Server::stop`] waits for open connections to close before it
/// leaves them to finish on their own (they hold only shared data).
const CONNECTION_GRACE: Duration = Duration::from_millis(300);

impl Server {
    /// Listen on `bind:port` (port 0 picks a free one). Peers outside the
    /// local network are disconnected without an answer.
    pub fn start(bind: IpAddr, port: u16, handler: Arc<dyn Handler>) -> Result<Server, String> {
        Self::start_with(bind, port, handler, Peers::default())
    }

    /// [`Server::start`] with an explicit policy for which peers are local.
    pub fn start_with(bind: IpAddr, port: u16, handler: Arc<dyn Handler>, peers: Peers) -> Result<Server, String> {
        // a server on the same fixed port that just stopped may still be
        // releasing it: retry briefly
        let mut tries = 0;
        let listener = loop {
            match TcpListener::bind((bind, port)) {
                Ok(l) => break l,
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && port != 0 && tries < 10 => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => return Err(format!("cannot listen on {bind}:{port}: {e}")),
            }
        };
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        let quit = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicU64::new(0));
        let refused = Arc::new(AtomicU64::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let (q, rq, rf, ac) = (quit.clone(), requests.clone(), refused.clone(), active.clone());
        let thread = std::thread::Builder::new()
            .name("media-http".into())
            .spawn(move || accept_loop(listener, handler, peers, q, rq, rf, ac))
            .map_err(|e| e.to_string())?;
        Ok(Server { addr, quit, thread: Some(thread), requests, refused, active })
    }

    /// Stop accepting and close the listening socket at once; open
    /// connections get [`CONNECTION_GRACE`] to finish, then are left to end
    /// on their own (a client that stops reading cannot hold this up).
    pub fn stop(&mut self) {
        self.quit.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

#[allow(clippy::too_many_arguments)]
fn accept_loop(listener: TcpListener, handler: Arc<dyn Handler>, peers: Peers, quit: Arc<AtomicBool>, requests: Arc<AtomicU64>, refused: Arc<AtomicU64>, active: Arc<AtomicUsize>) {
    let mut workers: Vec<JoinHandle<()>> = Vec::new();
    while !quit.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, peer)) => {
                if !peers.allows(peer.ip()) {
                    refused.fetch_add(1, Ordering::Relaxed);
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                }
                workers.retain(|w| !w.is_finished());
                if active.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
                    let mut s = stream;
                    let _ = s.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    continue;
                }
                active.fetch_add(1, Ordering::Relaxed);
                let (h, q, rq, ac) = (handler.clone(), quit.clone(), requests.clone(), active.clone());
                match std::thread::Builder::new().name("media-http-conn".into()).spawn(move || {
                    // a panicking handler must not leak a connection slot
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| serve_connection(stream, &*h, &q, &rq)));
                    ac.fetch_sub(1, Ordering::Relaxed);
                }) {
                    Ok(w) => workers.push(w),
                    Err(_) => {
                        active.fetch_sub(1, Ordering::Relaxed);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    drop(listener);
    crate::join_within(workers, CONNECTION_GRACE);
}

/// Body bytes written between checks of the stop flag.
const WRITE_CHUNK: usize = 64 * 1024;

fn serve_connection(mut stream: TcpStream, handler: &dyn Handler, quit: &AtomicBool, requests: &AtomicU64) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let write_body = |stream: &mut TcpStream, body: &[u8]| -> bool {
        for chunk in body.chunks(WRITE_CHUNK) {
            if quit.load(Ordering::Relaxed) || stream.write_all(chunk).is_err() {
                return false;
            }
        }
        true
    };
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut idle = Instant::now();
    for _ in 0..1000 {
        // read one request head
        let head_end = loop {
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break Some(i + 4);
            }
            if buf.len() > MAX_HEAD || quit.load(Ordering::Relaxed) || idle.elapsed() > Duration::from_secs(15) {
                break None;
            }
            let mut chunk = [0u8; 2048];
            match stream.read(&mut chunk) {
                Ok(0) => break None,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
                Err(_) => break None,
            }
        };
        let Some(end) = head_end else { return };
        let head = String::from_utf8_lossy(&buf[..end]).to_string();
        buf.drain(..end);
        idle = Instant::now();
        requests.fetch_add(1, Ordering::Relaxed);
        let mut lines = head.split("\r\n");
        let first = lines.next().unwrap_or("");
        let mut parts = first.split_whitespace();
        let (method, target, version) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"), parts.next().unwrap_or("HTTP/1.0"));
        let close_requested = lines.any(|l| {
            let l = l.to_ascii_lowercase();
            l.starts_with("connection:") && l.contains("close")
        });
        let keep = version == "HTTP/1.1" && !close_requested;
        let path = target.split('?').next().unwrap_or("/");
        let conn = if keep { "keep-alive" } else { "close" };
        let ok = match method {
            "GET" | "HEAD" => match handler.get(path) {
                Some((ctype, body, cache)) => {
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: {cache}\r\nAccess-Control-Allow-Origin: *\r\nConnection: {conn}\r\n\r\n",
                        body.len()
                    );
                    stream.write_all(head.as_bytes()).is_ok() && (method == "HEAD" || write_body(&mut stream, &body))
                }
                None => stream.write_all(format!("HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 10\r\nConnection: {conn}\r\n\r\nnot found\n").as_bytes()).is_ok(),
            },
            _ => {
                let _ = stream.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nAllow: GET, HEAD\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                false
            }
        };
        if !ok || !keep {
            let _ = stream.flush();
            let _ = stream.shutdown(Shutdown::Write);
            return;
        }
    }
}

/// This machine's address on the local network (no packet is sent: a UDP
/// socket is only connected so the OS picks the outgoing interface).
pub fn lan_ip() -> Option<IpAddr> {
    let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    s.connect((Ipv4Addr::new(10, 254, 254, 254), 9)).ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_unspecified() && is_local_ip(ip)).then_some(ip)
}

pub fn loopback() -> IpAddr {
    IpAddr::V4(Ipv4Addr::LOCALHOST)
}

pub fn unspecified() -> IpAddr {
    IpAddr::V4(Ipv4Addr::UNSPECIFIED)
}

pub fn v6_loopback() -> IpAddr {
    IpAddr::V6(Ipv6Addr::LOCALHOST)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anypage_runtime::Caps;

    fn grants(net: &[&str]) -> Arc<Grants> {
        Arc::new(Grants::new("t", Caps { net: net.iter().map(|s| s.to_string()).collect(), ..Caps::default() }))
    }

    #[test]
    fn local_addresses() {
        for ip in ["127.0.0.1", "10.1.2.3", "192.168.0.9", "172.16.5.5", "169.254.1.1", "::1", "fd00::1", "fe80::1", "::ffff:192.168.1.2"] {
            assert!(is_local_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "172.32.0.1", "2001:4860::8888", "::ffff:8.8.8.8", "100.64.0.1", "100.127.255.254", "::ffff:100.100.1.1"] {
            assert!(!is_local_ip(ip.parse().unwrap()), "{ip}");
        }
        assert!(is_local_host("localhost") && is_local_host("cam.local") && is_local_host("[::1]"));
        assert!(!is_local_host("example.com"));
        // carrier-grade NAT space is local only for an overlay network the request names
        let cgnat: IpAddr = "100.100.1.1".parse().unwrap();
        assert!(is_cgnat_ip(cgnat) && !is_cgnat_ip("100.128.0.1".parse().unwrap()) && !is_cgnat_ip("100.63.255.255".parse().unwrap()));
        assert!(!Peers::default().allows(cgnat));
        assert!(Peers { tailnet: true }.allows(cgnat));
        assert!(!Peers { tailnet: true }.allows("8.8.8.8".parse().unwrap()));
        let g = grants(&["100.100.1.1"]);
        let e = check_url(&g, "http://100.100.1.1:8080/live.m3u8").unwrap_err();
        assert!(e.contains("https") && e.contains("tailnet"), "{e}");
        assert!(check_url_for(&g, "http://100.100.1.1:8080/live.m3u8", Peers { tailnet: true }).is_ok());
    }

    #[test]
    fn stop_does_not_wait_for_a_client_that_stopped_reading() {
        let big = Arc::new(vec![7u8; 32 << 20]);
        let handler: Arc<dyn Handler> = Arc::new(move |_p: &str| -> Option<Reply> { Some(("video/mp2t", big.clone(), "no-cache")) });
        let mut server = Server::start(loopback(), 0, handler).unwrap();
        let mut c = TcpStream::connect(("127.0.0.1", server.addr.port())).unwrap();
        write!(c, "GET /seg0.ts HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        // the client never reads: the server's write blocks once the socket buffers fill
        std::thread::sleep(Duration::from_millis(300));
        let t = Instant::now();
        server.stop();
        let took = t.elapsed();
        assert!(took < Duration::from_millis(800), "stop took {took:?}");
        // the port is free again at once
        let again = TcpListener::bind(server.addr);
        assert!(again.is_ok(), "{again:?}");
        drop(c);
    }

    #[test]
    fn url_rules() {
        let g = grants(&["127.0.0.1", "cdn.example.com"]);
        assert!(check_url(&g, "http://127.0.0.1:8080/live.m3u8").is_ok());
        assert!(check_url(&g, "https://cdn.example.com/a.m3u8").is_ok());
        assert!(check_url(&g, "http://cdn.example.com/a.m3u8").unwrap_err().contains("https"));
        assert!(check_url(&g, "https://evil.example.org/a.m3u8").unwrap_err().contains("capabilities.net"));
        assert!(check_url(&g, "file:///c:/x.m3u8").unwrap_err().contains("only http"));
        assert!(check_url(&g, "http://192.168.1.4/a.m3u8").unwrap_err().contains("capabilities.net"));
    }

    #[test]
    fn serves_fetches_and_follows_checked_redirects() {
        let body = Arc::new(b"hello segment".to_vec());
        let b2 = body.clone();
        let handler: Arc<dyn Handler> = Arc::new(move |path: &str| -> Option<Reply> {
            match path {
                "/a.ts" => Some(("video/mp2t", b2.clone(), "no-cache")),
                _ => None,
            }
        });
        let mut server = Server::start(loopback(), 0, handler).unwrap();
        let base = format!("http://127.0.0.1:{}", server.addr.port());
        let f = Fetcher::new(grants(&["127.0.0.1"]));
        let (u, got) = f.get(&format!("{base}/a.ts"), None, &|| false).unwrap();
        assert_eq!(got, *body);
        assert!(u.ends_with("/a.ts"));
        // keep-alive: a second request on the pooled connection
        let (_, again) = f.get(&format!("{base}/a.ts"), Some((6, 7)), &|| false).unwrap();
        assert_eq!(again, b"segment");
        let e = f.get(&format!("{base}/missing.ts"), None, &|| false).unwrap_err();
        assert!(e.contains("404"), "{e}");
        assert!(f.get(&format!("{base}/a.ts"), None, &|| true).unwrap_err().contains("cancelled"));
        // not granted
        let g = Fetcher::new(grants(&["example.com"]));
        assert!(g.get(&format!("{base}/a.ts"), None, &|| false).unwrap_err().contains("capabilities.net"));
        // size limit
        let mut small = Fetcher::new(grants(&["127.0.0.1"]));
        small.max_body = 4;
        assert!(small.get(&format!("{base}/a.ts"), None, &|| false).unwrap_err().contains("limit"));
        assert!(server.requests.load(Ordering::Relaxed) >= 4);
        server.stop();
    }

    #[test]
    fn redirects_are_rechecked() {
        // a server that redirects to a host the page did not declare
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let t = std::thread::spawn(move || {
            for _ in 0..2 {
                let Ok((mut s, _)) = listener.accept() else { return };
                let mut b = [0u8; 1024];
                let n = s.read(&mut b).unwrap_or(0);
                let req = String::from_utf8_lossy(&b[..n]).to_string();
                let to = if req.contains("/hop") { "http://127.0.0.1:1/x".to_string() } else { "https://elsewhere.example.net/x".into() };
                let _ = s.write_all(format!("HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes());
            }
        });
        let f = Fetcher::new(grants(&[&format!("127.0.0.1:{port}")]));
        let e = f.get(&format!("http://127.0.0.1:{port}/start"), None, &|| false).unwrap_err();
        assert!(e.contains("elsewhere.example.net"), "{e}");
        let e = f.get(&format!("http://127.0.0.1:{port}/hop"), None, &|| false).unwrap_err();
        assert!(e.contains("capabilities.net"), "redirect to another port of the same host is checked too: {e}");
        let _ = t.join();
    }
}
