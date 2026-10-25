//! The semantic port over a local socket, for agents outside the app.
//!
//! `anypage --agent-port 7700` listens on 127.0.0.1 only and prints a token.
//! A client sends the token as its first line, then one JSON request per
//! line; each gets one JSON line back:
//!
//! ```text
//! <token>
//! {"describe": true}
//! {"action": "pdf.search", "params": {"query": "invoice"}}
//! {"get": "page_count"}
//! {"set": "zoom", "value": 2}
//! ```
//!
//! Requests run on the UI thread between frames, exactly like user input.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};

/// A request waiting for the UI thread, and where to send the answer.
pub struct AgentCall {
    pub request: String,
    pub reply: Sender<String>,
}

/// A random hex token from the OS-seeded hasher (no extra dependency).
pub fn token() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::new();
    for k in 0..2u64 {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(k ^ std::process::id() as u64);
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

fn serve(stream: TcpStream, token: &str, calls: &Sender<AgentCall>, wake: &(dyn Fn() + Send + Sync)) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut out = stream;
    let mut first = String::new();
    reader.read_line(&mut first)?;
    if first.trim() != token {
        out.write_all(b"{\"error\":\"bad token\"}\n")?;
        return Ok(());
    }
    out.write_all(b"{\"ok\":true}\n")?;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let request = line.trim().to_string();
        if request.is_empty() {
            continue;
        }
        if request.len() > 1 << 20 {
            out.write_all(b"{\"error\":\"request too large\"}\n")?;
            continue;
        }
        let (tx, rx) = channel();
        if calls.send(AgentCall { request, reply: tx }).is_err() {
            return Ok(());
        }
        wake();
        let answer = rx.recv().unwrap_or_else(|_| "{\"error\":\"page closed\"}".into());
        out.write_all(answer.as_bytes())?;
        out.write_all(b"\n")?;
    }
}

/// Start listening; requests arrive on the returned receiver after `wake` is called.
pub fn start(port: u16, token: String, wake: Arc<dyn Fn() + Send + Sync>) -> std::io::Result<std::sync::mpsc::Receiver<AgentCall>> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let (tx, rx) = channel::<AgentCall>();
    let tx = Arc::new(Mutex::new(tx));
    std::thread::Builder::new().name("agent-port".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let (tx, token, wake) = (tx.lock().unwrap().clone(), token.clone(), wake.clone());
            std::thread::spawn(move || {
                let _ = serve(stream, &token, &tx, wake.as_ref());
            });
        }
    })?;
    Ok(rx)
}
