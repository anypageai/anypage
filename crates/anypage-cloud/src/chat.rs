//! OpenAI-compatible chat completions, streamed (`stream: true`, SSE).
//!
//! Works with OpenAI, Azure OpenAI (`api-key` header, deployment URLs with
//! `?api-version=`), Ollama, vLLM, LM Studio and other servers speaking the
//! same protocol. Also accepts a plain JSON (non-streamed) answer and
//! Ollama's native NDJSON stream.
//!
//! The source `cloud.chat` serves requests from the page's request cell: a
//! new request cancels the one in flight, `nil` cancels. Each item it pushes
//! is the whole accumulated state so far (use `latest = true`).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anypage_runtime::{Data, Grants, SourceCtx, SourceGuard, Sink};
use serde_json::{json, Map, Value as Json};

use crate::auth;
use crate::cancel::{self, Token};
use crate::net::{self, Redirects, Request, Timeouts};
use crate::sse;
use crate::stream::{next_request, Gate, Guard, Next, Workers};
use crate::util::Scrubber;

/// Most text a reply may accumulate (content + reasoning + arguments).
pub const MAX_REPLY: usize = 8 << 20;
/// Most bytes a whole streamed response may have.
pub const MAX_STREAM: usize = 64 << 20;
/// Largest request body.
pub const MAX_REQUEST_JSON: usize = 32 << 20;
/// Kept conversation (`keep_history`) is trimmed to this many JSON bytes.
pub const MAX_HISTORY: usize = 16 << 20;
const MAX_TOOL_CALLS: usize = 256;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolCall {
    pub index: u64,
    pub id: String,
    pub kind: String,
    pub name: String,
    pub arguments: String,
}

/// The accumulated state of one completion.
#[derive(Clone, Debug, Default)]
pub struct ChatState {
    pub text: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    pub usage: Option<Json>,
    pub model: Option<String>,
    pub id: Option<String>,
    pub done: bool,
    pub cancelled: bool,
    pub error: Option<String>,
    pub status: Option<u16>,
    pub first_token_ms: Option<f64>,
    pub elapsed_ms: f64,
    pub chunks: u64,
    size: usize,
    /// The request's credentials, kept out of everything the page sees.
    scrubber: Option<Arc<Scrubber>>,
    scanned_text: usize,
    scanned_reasoning: usize,
    scanned_args: Vec<usize>,
}

fn str_of(j: &Json) -> Option<&str> {
    j.as_str()
}

impl ChatState {
    /// Copy what pages see from `o`, reusing this state's allocations.
    fn copy_from(&mut self, o: &ChatState) {
        self.text.clone_from(&o.text);
        self.reasoning.clone_from(&o.reasoning);
        self.tool_calls.clone_from(&o.tool_calls);
        self.finish_reason.clone_from(&o.finish_reason);
        self.usage.clone_from(&o.usage);
        self.model.clone_from(&o.model);
        self.id.clone_from(&o.id);
        self.done = o.done;
        self.cancelled = o.cancelled;
        self.error.clone_from(&o.error);
        self.status = o.status;
        self.first_token_ms = o.first_token_ms;
        self.elapsed_ms = o.elapsed_ms;
        self.chunks = o.chunks;
    }

    /// Remove the request's credentials from everything the server sent so
    /// far. Text, reasoning and arguments are scanned only where they grew.
    pub fn scrub(&mut self) {
        let Some(sc) = self.scrubber.clone() else { return };
        sc.tail(&mut self.text, &mut self.scanned_text);
        sc.tail(&mut self.reasoning, &mut self.scanned_reasoning);
        self.scanned_args.resize(self.tool_calls.len(), 0);
        for (t, scanned) in self.tool_calls.iter_mut().zip(self.scanned_args.iter_mut()) {
            sc.tail(&mut t.arguments, scanned);
            for s in [&mut t.id, &mut t.name, &mut t.kind] {
                if let Some(x) = sc.text(s) {
                    *s = x;
                }
            }
        }
        for s in [&mut self.finish_reason, &mut self.model, &mut self.id, &mut self.error].into_iter().flatten() {
            if let Some(x) = sc.text(s) {
                *s = x;
            }
        }
        if let Some(u) = &self.usage {
            if let Some(x) = sc.text(&u.to_string()) {
                self.usage = serde_json::from_str(&x).ok();
            }
        }
    }

    fn grow(&mut self, n: usize) -> Result<(), String> {
        self.size += n;
        if self.size > MAX_REPLY {
            return Err(format!("reply larger than {}", net::size_text(MAX_REPLY)));
        }
        Ok(())
    }

    fn tool_mut(&mut self, index: Option<u64>, id: Option<&str>) -> Result<&mut ToolCall, String> {
        let pos = match index {
            Some(i) => self.tool_calls.iter().position(|t| t.index == i),
            None => match id.filter(|s| !s.is_empty()) {
                Some(id) => self.tool_calls.iter().position(|t| t.id == id),
                None => self.tool_calls.len().checked_sub(1),
            },
        };
        let pos = match pos {
            Some(p) => p,
            None => {
                if self.tool_calls.len() >= MAX_TOOL_CALLS {
                    return Err(format!("more than {MAX_TOOL_CALLS} tool calls"));
                }
                let index = index.unwrap_or(self.tool_calls.len() as u64);
                self.tool_calls.push(ToolCall { index, kind: "function".into(), ..ToolCall::default() });
                self.tool_calls.len() - 1
            }
        };
        Ok(&mut self.tool_calls[pos])
    }

    /// Merge one tool-call delta (`{index, id, type, function: {name, arguments}}`).
    fn apply_tool_delta(&mut self, d: &Json, position: usize) -> Result<(), String> {
        let index = d.get("index").and_then(Json::as_u64).or_else(|| if d.get("id").and_then(str_of).is_some_and(|s| !s.is_empty()) { None } else { Some(position as u64) });
        let id = d.get("id").and_then(str_of);
        let f = d.get("function").cloned().unwrap_or(Json::Null);
        let name = f.get("name").and_then(str_of).unwrap_or("");
        let args = match f.get("arguments") {
            Some(Json::String(s)) => s.clone(),
            Some(Json::Null) | None => String::new(),
            // some servers send the arguments as an object
            Some(other) => other.to_string(),
        };
        self.grow(name.len() + args.len())?;
        let t = self.tool_mut(index, id)?;
        if let Some(id) = id.filter(|s| !s.is_empty()) {
            t.id = id.to_string();
        }
        if let Some(k) = d.get("type").and_then(str_of).filter(|s| !s.is_empty()) {
            t.kind = k.to_string();
        }
        if !name.is_empty() && t.name != name {
            // a full name once (OpenAI), the same name repeated (some
            // proxies), or the name in pieces
            t.name.push_str(name);
        }
        t.arguments.push_str(&args);
        Ok(())
    }

    /// Apply one streamed chunk (or a whole non-streamed response).
    pub fn apply(&mut self, j: &Json) -> Result<(), String> {
        self.chunks += 1;
        // some gateways send `"error": null` (or `{}`) on every chunk
        let error = j.get("error").filter(|e| !e.is_null() && !e.as_object().is_some_and(Map::is_empty) && e.as_str() != Some(""));
        if let Some(e) = error {
            let msg = e.get("message").and_then(str_of).map(str::to_string).or_else(|| e.as_str().map(str::to_string)).unwrap_or_else(|| e.to_string());
            self.error = Some(crate::util::clip(&msg, 2000));
            return Ok(());
        }
        if let Some(id) = j.get("id").and_then(str_of) {
            self.id = Some(id.to_string());
        }
        if let Some(m) = j.get("model").and_then(str_of) {
            self.model = Some(m.to_string());
        }
        if let Some(u) = j.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(u.clone());
        }
        if let Some(choices) = j.get("choices").and_then(Json::as_array) {
            // n > 1 is not supported: the first choice is the reply
            let first = choices.iter().find(|c| c.get("index").and_then(Json::as_u64).unwrap_or(0) == 0);
            if let Some(c) = first {
                for key in ["delta", "message"] {
                    if let Some(d) = c.get(key) {
                        self.apply_delta(d)?;
                    }
                }
                if let Some(t) = c.get("text").and_then(str_of) {
                    // legacy completions
                    self.grow(t.len())?;
                    self.text.push_str(t);
                }
                if let Some(r) = c.get("finish_reason").and_then(str_of) {
                    self.finish_reason = Some(r.to_string());
                }
            }
        }
        // Ollama's native /api/chat stream
        if let Some(m) = j.get("message").filter(|m| m.is_object()) {
            self.apply_delta(m)?;
        }
        if j.get("done").and_then(Json::as_bool) == Some(true) {
            self.finish_reason = j.get("done_reason").and_then(str_of).map(str::to_string).or_else(|| self.finish_reason.clone()).or(Some("stop".into()));
            if let (Some(p), Some(c)) = (j.get("prompt_eval_count").and_then(Json::as_u64), j.get("eval_count").and_then(Json::as_u64)) {
                self.usage = Some(json!({ "prompt_tokens": p, "completion_tokens": c, "total_tokens": p + c }));
            }
        }
        Ok(())
    }

    fn apply_delta(&mut self, d: &Json) -> Result<(), String> {
        if let Some(t) = d.get("content").and_then(str_of) {
            self.grow(t.len())?;
            self.text.push_str(t);
        } else if let Some(parts) = d.get("content").and_then(Json::as_array) {
            // content as a list of parts: [{type: "text", text: "..."}]
            for p in parts {
                if let Some(t) = p.get("text").and_then(str_of) {
                    self.grow(t.len())?;
                    self.text.push_str(t);
                }
            }
        }
        for key in ["reasoning_content", "reasoning", "thinking"] {
            if let Some(t) = d.get(key).and_then(str_of) {
                self.grow(t.len())?;
                self.reasoning.push_str(t);
            }
        }
        if let Some(calls) = d.get("tool_calls").and_then(Json::as_array) {
            for (pos, c) in calls.iter().enumerate() {
                self.apply_tool_delta(c, pos)?;
            }
        }
        if let Some(fc) = d.get("function_call").filter(|f| f.is_object()) {
            // the legacy single function call
            self.apply_tool_delta(&json!({ "index": 0, "type": "function", "function": fc }), 0)?;
        }
        Ok(())
    }

    /// The item pages see.
    pub fn to_data(&self, seq: u64, messages: Option<&Data>) -> Data {
        let calls: Vec<Data> = self
            .tool_calls
            .iter()
            .map(|t| {
                let mut m = vec![
                    ("index", Data::Num(t.index as f64)),
                    ("id", Data::str(&t.id)),
                    ("type", Data::str(&t.kind)),
                    ("name", Data::str(&t.name)),
                    ("arguments", Data::str(&t.arguments)),
                ];
                if let Ok(args) = serde_json::from_str::<Json>(&t.arguments) {
                    m.push(("args", Data::from_json(&args)));
                }
                Data::map(m)
            })
            .collect();
        let mut m: Vec<(&str, Data)> = vec![
            ("seq", Data::Num(seq as f64)),
            ("text", Data::str(&self.text)),
            ("tool_calls", Data::list(calls)),
            ("done", Data::Bool(self.done)),
            ("elapsed_ms", Data::Num((self.elapsed_ms * 10.0).round() / 10.0)),
        ];
        let opt = |s: &Option<String>| s.as_deref().map(Data::str).unwrap_or_default();
        m.push(("finish_reason", opt(&self.finish_reason)));
        m.push(("error", opt(&self.error)));
        m.push(("model", opt(&self.model)));
        m.push(("id", opt(&self.id)));
        if !self.reasoning.is_empty() {
            m.push(("reasoning", Data::str(&self.reasoning)));
        }
        if let Some(u) = &self.usage {
            m.push(("usage", Data::from_json(u)));
        }
        if self.cancelled {
            m.push(("cancelled", Data::Bool(true)));
        }
        if let Some(s) = self.status {
            m.push(("status", Data::Num(s as f64)));
        }
        if let Some(t) = self.first_token_ms {
            m.push(("first_token_ms", Data::Num((t * 10.0).round() / 10.0)));
        }
        if let Some(msgs) = messages {
            m.push(("messages", msgs.clone()));
        }
        Data::map(m)
    }

    /// The assistant message to append to a conversation.
    pub fn assistant_message(&self) -> Json {
        let mut m = Map::new();
        m.insert("role".into(), json!("assistant"));
        m.insert("content".into(), json!(self.text));
        if !self.tool_calls.is_empty() {
            let calls: Vec<Json> = self.tool_calls.iter().map(|t| json!({ "id": t.id, "type": t.kind, "function": { "name": t.name, "arguments": t.arguments } })).collect();
            m.insert("tool_calls".into(), Json::Array(calls));
        }
        Json::Object(m)
    }
}

/// A chat request, validated.
#[derive(Clone, Debug)]
pub struct ChatRequest {
    pub url: String,
    pub body: Map<String, Json>,
    pub messages: Vec<Json>,
    pub secret: Option<String>,
    pub auth: String,
    pub headers: Vec<(String, String)>,
    pub keep_history: bool,
    pub reset: bool,
    pub idle_timeout: Option<Duration>,
}

/// `{ base_url | url, model, messages, tools, tool_choice, temperature,
/// max_tokens, top_p, stop, seed, response_format, extra, secret, auth,
/// headers, keep_history, reset, usage, timeout }`
pub fn parse_request(req: &Data) -> Result<ChatRequest, String> {
    if !matches!(req, Data::Map(_)) {
        return Err("a chat request is a table { base_url, model, messages, ... }".into());
    }
    let url = match (req.get("url").as_str(), req.get("base_url").as_str()) {
        (Some(u), _) => u.to_string(),
        (None, Some(b)) => endpoint(b),
        (None, None) => return Err("a chat request needs base_url (e.g. \"https://api.openai.com/v1\") or url".into()),
    };
    let mut body = Map::new();
    if let Some(m) = req.get("model").as_str() {
        body.insert("model".into(), json!(m));
    }
    let messages: Vec<Json> = match req.get("messages") {
        Data::Nil => Vec::new(),
        Data::List(l) => l.iter().map(Data::to_json).collect(),
        Data::Str(s) => vec![json!({ "role": "user", "content": &**s })],
        _ => return Err("messages must be a list of { role, content }".into()),
    };
    for (i, m) in messages.iter().enumerate() {
        if m.get("role").and_then(Json::as_str).is_none() {
            return Err(format!("messages[{}] has no role", i + 1));
        }
    }
    if let Data::List(tools) = req.get("tools") {
        let mut t: Vec<Json> = tools.iter().map(Data::to_json).collect();
        t.iter_mut().for_each(fix_schema);
        if !t.is_empty() {
            body.insert("tools".into(), Json::Array(t));
        }
    }
    for key in ["tool_choice", "temperature", "max_tokens", "max_completion_tokens", "top_p", "stop", "seed", "response_format", "presence_penalty", "frequency_penalty", "user", "parallel_tool_calls", "reasoning_effort"] {
        let v = req.get(key);
        if !v.is_nil() {
            body.insert(key.into(), v.to_json());
        }
    }
    if let Data::Map(_) = req.get("extra") {
        if let Json::Object(extra) = req.get("extra").to_json() {
            for (k, v) in extra {
                body.insert(k, v);
            }
        }
    }
    body.insert("stream".into(), json!(true));
    if req.get("usage").as_bool() != Some(false) {
        body.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    let headers = auth::page_headers(req.get("headers"))?;
    let idle = req.get("timeout").num().filter(|t| t.is_finite() && *t > 0.0).map(|t| Duration::from_secs_f64(t.min(3600.0)));
    Ok(ChatRequest {
        url,
        body,
        messages,
        secret: req.get("secret").as_str().map(str::to_string),
        auth: req.get("auth").as_str().unwrap_or("").to_string(),
        headers,
        keep_history: req.get("keep_history").truthy(),
        reset: req.get("reset").truthy(),
        idle_timeout: idle,
    })
}

/// `base_url` + `/chat/completions`, keeping a query string (Azure's
/// `?api-version=`); a URL already ending in `/chat/completions` is kept.
pub fn endpoint(base: &str) -> String {
    let (path, query) = match base.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (base, None),
    };
    let path = path.trim_end_matches('/');
    let mut url = if path.ends_with("/chat/completions") { path.to_string() } else { format!("{path}/chat/completions") };
    if let Some(q) = query {
        url.push('?');
        url.push_str(q);
    }
    url
}

/// Luau turns `{}` into an empty list; JSON schemas need objects there.
fn fix_schema(v: &mut Json) {
    if let Json::Object(m) = v {
        for (k, x) in m.iter_mut() {
            let empty_list = matches!(x, Json::Array(a) if a.is_empty());
            if empty_list && matches!(k.as_str(), "properties" | "parameters" | "function" | "definitions" | "$defs" | "patternProperties") {
                *x = if k == "parameters" { json!({ "type": "object", "properties": {} }) } else { json!({}) };
            } else {
                fix_schema(x);
            }
        }
    } else if let Json::Array(a) = v {
        a.iter_mut().for_each(fix_schema);
    }
}

/// The secret a request uses: the one it names, else `"openai"` when the
/// request goes to api.openai.com and the page declares that secret (the
/// spec's default). Any other host gets credentials only when the request
/// names a secret: secrets are not bound to hosts yet, so a key is never
/// attached to a server the page merely points at.
pub fn effective_secret(grants: &Grants, req: &ChatRequest) -> Option<String> {
    if req.secret.is_some() || req.auth == "none" {
        return req.secret.clone();
    }
    let official = anypage_runtime::natives::url_host(&req.url).is_some_and(|(h, _)| h.eq_ignore_ascii_case("api.openai.com")) && req.url.to_ascii_lowercase().starts_with("https://");
    (official && grants.caps.secrets.iter().any(|s| s == "openai")).then(|| "openai".to_string())
}

/// Run one completion to the end (or until `cancel`), calling `emit` with
/// the accumulated state after every chunk of input that changed it. Run it
/// inside [`cancel::scope`] with the same token so that cancelling also
/// interrupts a read blocked on the server.
pub fn run(grants: &Grants, req: &ChatRequest, messages: &[Json], cancel: &Token, emit: &mut dyn FnMut(&ChatState)) -> ChatState {
    let t0 = Instant::now();
    let mut st = ChatState::default();
    let secret = effective_secret(grants, req);
    let r = stream(grants, req, secret.as_deref(), messages, cancel, &mut st, t0, emit);
    st.elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
    if cancel.is_cancelled() {
        st.cancelled = true;
    } else if let Err(e) = r {
        st.error = Some(e);
    } else if st.error.is_none() && st.finish_reason.is_none() && st.chunks == 0 {
        st.error = Some("the server closed the stream without sending a reply".into());
    }
    // servers echo keys in error messages ("Incorrect API key provided: sk-...")
    st.error = st.error.take().map(|e| auth::scrub(grants, secret.as_deref(), e));
    st.scrub();
    st.done = true;
    st
}

#[allow(clippy::too_many_arguments)]
fn stream(grants: &Grants, req: &ChatRequest, secret: Option<&str>, messages: &[Json], cancel: &Token, st: &mut ChatState, t0: Instant, emit: &mut dyn FnMut(&ChatState)) -> Result<(), String> {
    let mut body = req.body.clone();
    body.insert("messages".into(), Json::Array(messages.to_vec()));
    let bytes = serde_json::to_vec(&Json::Object(body)).map_err(|e| format!("cannot encode the request: {e}"))?;
    if bytes.len() > MAX_REQUEST_JSON {
        return Err(format!("chat request larger than {}", net::size_text(MAX_REQUEST_JSON)));
    }
    let mut r = Request::new("POST", &req.url).header("Content-Type", "application/json").header("Accept", "text/event-stream, application/json, application/x-ndjson");
    r.headers.extend(req.headers.iter().cloned());
    r.body = Some(Arc::new(bytes));
    r.redirects = Redirects::Follow;
    r.timeouts = Timeouts::stream();
    if let Some(t) = req.idle_timeout {
        r.timeouts.idle = t;
    }
    let mode = if req.auth.is_empty() { None } else { Some(req.auth.as_str()) };
    let is_cancelled = || cancel.is_cancelled();
    auth::apply(grants, &mut r, secret, mode, &Data::Nil, &is_cancelled)?;
    let sc = Scrubber::new(r.secrets());
    if !sc.is_empty() {
        st.scrubber = Some(Arc::new(sc));
    }
    let resp = net::send(grants, &r, &is_cancelled)?;
    st.status = Some(resp.status);
    let ctype = resp.content_type();
    if !resp.ok() {
        let shown = resp.url.display();
        let status = resp.status;
        let b = resp.read_all(1 << 20, &is_cancelled, &|_| {}).unwrap_or_default();
        return Err(format!("HTTP {status} from {shown}: {}", net::error_text(&b)));
    }
    let ndjson = ctype.contains("ndjson");
    let whole_json = ctype.contains("application/json");
    let mut rd = resp.reader();
    if whole_json {
        let mut all = Vec::new();
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            if is_cancelled() {
                return Ok(());
            }
            let n = rd.read(&mut buf)?;
            if n == 0 {
                break;
            }
            if all.len() + n > MAX_STREAM {
                return Err(format!("response larger than {}", net::size_text(MAX_STREAM)));
            }
            all.extend_from_slice(&buf[..n]);
        }
        let j: Json = serde_json::from_slice(&all).map_err(|e| format!("the server sent JSON that does not parse: {e}"))?;
        st.apply(&j)?;
        mark_first(st, t0);
        if st.finish_reason.is_none() && st.error.is_none() {
            st.finish_reason = Some("stop".into());
        }
        return Ok(());
    }
    let mut parser = sse::Parser::default();
    let mut lines = LineSplitter::default();
    let mut events = Vec::new();
    let mut buf = vec![0u8; 16 * 1024];
    let mut total = 0usize;
    let mut finished = false;
    while !finished {
        if is_cancelled() {
            return Ok(());
        }
        let n = rd.read(&mut buf)?;
        if is_cancelled() {
            return Ok(());
        }
        let eof = n == 0;
        total += n;
        if total > MAX_STREAM {
            return Err(format!("response larger than {}", net::size_text(MAX_STREAM)));
        }
        let before = (st.text.len(), st.reasoning.len(), st.chunks, st.error.is_some());
        events.clear();
        if ndjson {
            for line in lines.push(&buf[..n], eof)? {
                if line.trim().is_empty() {
                    continue;
                }
                events.push(sse::Event { event: "message".into(), data: line, id: None });
            }
        } else {
            parser.push(&buf[..n], &mut events)?;
            if eof {
                parser.finish(&mut events)?;
            }
        }
        for ev in &events {
            let data = ev.data.trim();
            if data == "[DONE]" {
                finished = true;
                break;
            }
            if data.is_empty() {
                continue;
            }
            match serde_json::from_str::<Json>(data) {
                Ok(j) => st.apply(&j)?,
                Err(e) if ev.event == "error" => st.error = Some(crate::util::clip(&format!("{data} ({e})"), 2000)),
                Err(e) => return Err(format!("the server sent an event that is not JSON: {e}: {}", crate::util::clip(data, 200))),
            }
            if ev.event == "error" && st.error.is_none() {
                st.error = Some(crate::util::clip(data, 2000));
            }
            if st.error.is_some() {
                finished = true;
                break;
            }
        }
        if st.text.len() + st.reasoning.len() > 0 || !st.tool_calls.is_empty() {
            mark_first(st, t0);
        }
        let after = (st.text.len(), st.reasoning.len(), st.chunks, st.error.is_some());
        if after != before && !finished {
            st.elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
            st.scrub();
            emit(st);
        }
        if eof {
            break;
        }
    }
    Ok(())
}

fn mark_first(st: &mut ChatState, t0: Instant) {
    if st.first_token_ms.is_none() {
        st.first_token_ms = Some(t0.elapsed().as_secs_f64() * 1000.0);
    }
}

/// Splits a byte stream into `\n`-terminated UTF-8 lines (NDJSON).
#[derive(Default)]
struct LineSplitter {
    buf: Vec<u8>,
}

impl LineSplitter {
    fn push(&mut self, chunk: &[u8], eof: bool) -> Result<Vec<String>, String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=i).collect();
            out.push(String::from_utf8_lossy(&line).trim_end_matches(['\r', '\n']).to_string());
        }
        if eof && !self.buf.is_empty() {
            out.push(String::from_utf8_lossy(&std::mem::take(&mut self.buf)).to_string());
        }
        if self.buf.len() > 8 << 20 {
            return Err("NDJSON line longer than 8 MB".into());
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------- the source

/// Trim the oldest non-system messages until the conversation fits.
fn trim_history(h: &mut Vec<Json>) {
    let size = |h: &Vec<Json>| h.iter().map(|m| m.to_string().len()).sum::<usize>();
    while size(h) > MAX_HISTORY {
        match h.iter().position(|m| m.get("role").and_then(Json::as_str) != Some("system")) {
            Some(i) => {
                h.remove(i);
            }
            None => break,
        }
    }
}

/// `cloud.chat`: see the module docs.
pub fn source(ctx: SourceCtx) -> Result<SourceGuard, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let guard = Guard { stop: stop.clone() };
    let label = ctx.label.clone();
    std::thread::Builder::new().name(format!("cloud.chat {label}")).spawn(move || controller(ctx, stop)).map_err(|e| format!("cannot start the chat thread: {e}"))?;
    Ok(Box::new(guard))
}

/// Least time between two intermediate items of one request.
pub const MIN_GAP: Duration = Duration::from_millis(8);

/// Delivers a request's intermediate states from a thread of its own: the
/// newest state always reaches the page, at most one item per [`MIN_GAP`].
/// (Throttling in the reading thread would hold back the last update until
/// the next read, which can be seconds away when the model pauses.)
#[derive(Default)]
struct Emitter {
    slot: Mutex<Pending>,
    cv: Condvar,
}

#[derive(Default)]
struct Pending {
    state: Option<ChatState>,
    dirty: bool,
    closed: bool,
}

struct Target {
    gen: u64,
    gate: Arc<Gate>,
    sink: Sink,
    dead: Arc<AtomicBool>,
    shown: Option<Data>,
}

impl Emitter {
    fn lock(&self) -> std::sync::MutexGuard<'_, Pending> {
        self.slot.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The reading thread: a newer state (cheap: a copy into the slot).
    fn offer(&self, s: &ChatState) {
        let mut g = self.lock();
        match &mut g.state {
            Some(x) => x.copy_from(s),
            None => g.state = Some(s.clone()),
        }
        g.dirty = true;
        self.cv.notify_one();
    }

    /// The request ended: the final item is pushed by the reading thread.
    fn close(&self) {
        self.lock().closed = true;
        self.cv.notify_one();
    }

    fn run(&self, t: Target) {
        let mut last: Option<Instant> = None;
        loop {
            let mut g = self.lock();
            while !g.dirty && !g.closed {
                g = self.cv.wait(g).unwrap_or_else(|p| p.into_inner());
            }
            if g.closed {
                return;
            }
            let wait = last.map_or(Duration::ZERO, |l| MIN_GAP.saturating_sub(l.elapsed()));
            if !wait.is_zero() {
                drop(g);
                std::thread::sleep(wait);
                continue;
            }
            let item = g.state.as_ref().map(|s| s.to_data(t.gen, t.shown.as_ref()));
            g.dirty = false;
            drop(g);
            let Some(item) = item else { continue };
            last = Some(Instant::now());
            if !t.gate.push(&t.sink, t.gen, item, false, &t.dead) {
                if !t.gate.is_current(t.gen) || t.dead.load(Ordering::Relaxed) {
                    return;
                }
                // the page's buffer is full: retry with whatever is newest then
                self.lock().dirty = true;
                std::thread::sleep(Duration::from_millis(15));
            }
        }
    }
}

fn controller(ctx: SourceCtx, stop: Arc<AtomicBool>) {
    let gate = Arc::new(Gate::default());
    let history: Arc<Mutex<Vec<Json>>> = Arc::new(Mutex::new(Vec::new()));
    // set once the page or this source goes away: workers stop pushing
    let dead = Arc::new(AtomicBool::new(false));
    let workers = Workers::default();
    let mut current: Option<Arc<Token>> = None;
    loop {
        let req = match next_request(&ctx, &stop, &mut current) {
            Next::Quit => {
                dead.store(true, Ordering::Relaxed);
                return;
            }
            // the running request (if any) was cancelled; it pushes its
            // final state itself, marked cancelled
            Next::Cancel => continue,
            Next::Request(r) => r,
        };
        let gen = gate.next();
        let parsed = match parse_request(&req) {
            Ok(p) => p,
            Err(e) => {
                let st = ChatState { done: true, error: Some(e), ..ChatState::default() };
                gate.push(&ctx.sink, gen, st.to_data(gen, None), true, &dead);
                continue;
            }
        };
        if parsed.reset {
            history.lock().unwrap_or_else(|p| p.into_inner()).clear();
        }
        let mut messages: Vec<Json> = if parsed.keep_history { history.lock().unwrap_or_else(|p| p.into_inner()).clone() } else { Vec::new() };
        messages.extend(parsed.messages.iter().cloned());
        let shown = if parsed.keep_history { Some(Data::list(messages.iter().map(Data::from_json).collect())) } else { None };
        if messages.is_empty() {
            // a reset with nothing to send
            let st = ChatState { done: true, ..ChatState::default() };
            gate.push(&ctx.sink, gen, st.to_data(gen, shown.as_ref()), true, &dead);
            continue;
        }
        let slot = match workers.admit(&stop) {
            Ok(s) => s,
            Err(e) => {
                let st = ChatState { done: true, error: Some(e), ..ChatState::default() };
                gate.push(&ctx.sink, gen, st.to_data(gen, shown.as_ref()), true, &dead);
                continue;
            }
        };
        let token = Token::new();
        current = Some(token.clone());
        let (sink, gate2, grants, hist, dead2) = (ctx.sink.clone(), gate.clone(), ctx.grants.clone(), history.clone(), dead.clone());
        let spawned = std::thread::Builder::new().name(format!("cloud.chat #{gen}")).spawn(move || {
            let _slot = slot;
            // show the request as running at once, before the first token
            gate2.push(&sink, gen, ChatState::default().to_data(gen, shown.as_ref()), false, &dead2);
            let emitter = Arc::new(Emitter::default());
            let target = Target { gen, gate: gate2.clone(), sink: sink.clone(), dead: dead2.clone(), shown: shown.clone() };
            let em = emitter.clone();
            let pusher = std::thread::Builder::new().name(format!("cloud.chat #{gen} items")).spawn(move || em.run(target)).ok();
            // the controller cancels `token` on a new request, on nil and on
            // quit; inside the scope that also shuts the socket down
            let st = cancel::scope(&token, || {
                crate::guarded("cloud.chat", || Ok(run(&grants, &parsed, &messages, &token, &mut |s: &ChatState| emitter.offer(s)))).unwrap_or_else(|e| ChatState { done: true, error: Some(e), ..ChatState::default() })
            });
            emitter.close();
            if let Some(p) = pusher {
                let _ = p.join();
            }
            if dead2.load(Ordering::Relaxed) {
                return;
            }
            // keep the turn before the page can see `done` and send the next one
            let has_reply = !st.text.is_empty() || !st.tool_calls.is_empty();
            if parsed.keep_history && st.error.is_none() && (has_reply || !st.cancelled) {
                gate2.if_current(gen, || {
                    let mut h = hist.lock().unwrap_or_else(|p| p.into_inner());
                    *h = messages;
                    h.push(st.assistant_message());
                    trim_history(&mut h);
                });
            }
            gate2.push(&sink, gen, st.to_data(gen, shown.as_ref()), true, &dead2);
        });
        if let Err(e) = spawned {
            let st = ChatState { done: true, error: Some(format!("cannot start a request thread: {e}")), ..ChatState::default() };
            gate.push(&ctx.sink, gen, st.to_data(gen, None), true, &dead);
        }
    }
}

/// Parse an SSE body into `{ {event, data, id} }` (for `cloud.parse_sse`).
pub fn events_data(body: &[u8]) -> Result<Data, String> {
    let evs = sse::parse_all(body)?;
    Ok(Data::list(
        evs.into_iter()
            .map(|e| {
                let mut m = BTreeMap::new();
                m.insert("event", Data::str(&e.event));
                m.insert("data", Data::str(&e.data));
                if let Some(id) = e.id {
                    m.insert("id", Data::str(&id));
                }
                Data::map(m)
            })
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(chunks: &[&str]) -> ChatState {
        let mut st = ChatState::default();
        for c in chunks {
            st.apply(&serde_json::from_str(c).unwrap()).unwrap();
        }
        st
    }

    #[test]
    fn text_and_usage() {
        let st = feed(&[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"Hel"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"lo","reasoning_content":"hmm"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}"#,
        ]);
        assert_eq!(st.text, "Hello");
        assert_eq!(st.reasoning, "hmm");
        assert_eq!(st.finish_reason.as_deref(), Some("stop"));
        assert_eq!(st.usage.as_ref().and_then(|u| u["total_tokens"].as_u64()), Some(5));
        assert_eq!(st.model.as_deref(), Some("m"));
    }

    #[test]
    fn tool_call_deltas_by_index() {
        let st = feed(&[
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"get_weather","arguments":""}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"call_b","type":"function","function":{"name":"get_time","arguments":""}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{\"tz\":\"UTC\"}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"Paris\"}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);
        assert_eq!(st.tool_calls.len(), 2);
        assert_eq!(st.tool_calls[0], ToolCall { index: 0, id: "call_a".into(), kind: "function".into(), name: "get_weather".into(), arguments: "{\"city\":\"Paris\"}".into() });
        assert_eq!(st.tool_calls[1].arguments, "{\"tz\":\"UTC\"}");
        let d = st.to_data(1, None);
        let calls = d.get("tool_calls");
        assert_eq!(calls.at(1).get("args").get("city").as_str(), Some("Paris"));
        assert_eq!(d.get("finish_reason").as_str(), Some("tool_calls"));
        let msg = st.assistant_message();
        assert_eq!(msg["tool_calls"][1]["function"]["name"], "get_time");
    }

    #[test]
    fn tool_calls_without_index_and_repeated_names() {
        // servers that omit the index and repeat the name on every delta
        let st = feed(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"id":"x1","function":{"name":"f","arguments":"{\"a\""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"id":"x1","function":{"name":"f","arguments":":1}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"id":"x2","function":{"name":"g","arguments":{"b":2}}}]}}]}"#,
        ]);
        assert_eq!(st.tool_calls.len(), 2);
        assert_eq!(st.tool_calls[0].name, "f");
        assert_eq!(st.tool_calls[0].arguments, "{\"a\":1}");
        assert_eq!(st.tool_calls[1].arguments, "{\"b\":2}");
    }

    #[test]
    fn non_streamed_ollama_and_errors() {
        let st = feed(&[r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"whole"},"finish_reason":"stop"}]}"#]);
        assert_eq!(st.text, "whole");
        let st = feed(&[r#"{"message":{"role":"assistant","content":"a"},"done":false}"#, r#"{"message":{"content":"b"},"done":true,"done_reason":"stop","prompt_eval_count":4,"eval_count":2}"#]);
        assert_eq!((st.text.as_str(), st.finish_reason.as_deref()), ("ab", Some("stop")));
        assert_eq!(st.usage.as_ref().and_then(|u| u["total_tokens"].as_u64()), Some(6));
        let st = feed(&[r#"{"error":{"message":"rate limited","type":"x"}}"#]);
        assert_eq!(st.error.as_deref(), Some("rate limited"));
    }

    #[test]
    fn null_or_empty_error_fields_are_not_errors() {
        let st = feed(&[
            r#"{"choices":[{"delta":{"content":"hi"}}],"error":null}"#,
            r#"{"choices":[{"delta":{"content":" there"}}],"error":{}}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"error":""}"#,
        ]);
        assert_eq!((st.text.as_str(), st.error.as_deref(), st.finish_reason.as_deref()), ("hi there", None, Some("stop")));
        let st = feed(&[r#"{"error":"quota exceeded"}"#]);
        assert_eq!(st.error.as_deref(), Some("quota exceeded"));
    }

    #[test]
    fn default_secret_only_for_the_official_endpoint() {
        use anypage_runtime::Caps;
        let g = Grants::new("t", Caps { secrets: vec!["openai".into()], ..Caps::default() });
        let req = |url: &str, extra: Vec<(&'static str, Data)>| {
            let mut v = vec![("url", Data::str(url)), ("messages", Data::str("hi"))];
            v.extend(extra);
            parse_request(&Data::map(v)).unwrap()
        };
        assert_eq!(effective_secret(&g, &req("https://api.openai.com/v1/chat/completions", vec![])).as_deref(), Some("openai"));
        assert_eq!(effective_secret(&g, &req("https://API.OpenAI.com/v1/chat/completions", vec![])).as_deref(), Some("openai"));
        // a lookalike path, another host, plain http, auth = none: no key
        assert_eq!(effective_secret(&g, &req("http://127.0.0.1:9/api.openai.com/v1/chat/completions", vec![])), None);
        assert_eq!(effective_secret(&g, &req("https://api.openai.com.evil.example/v1", vec![])), None);
        assert_eq!(effective_secret(&g, &req("http://api.openai.com/v1", vec![])), None);
        assert_eq!(effective_secret(&g, &req("https://api.openai.com/v1", vec![("auth", Data::str("none"))])), None);
        // named explicitly: used as named
        assert_eq!(effective_secret(&g, &req("http://localhost:1/v1", vec![("secret", Data::str("other"))])).as_deref(), Some("other"));
        // not declared: nothing by default
        let g2 = Grants::new("t", Caps::default());
        assert_eq!(effective_secret(&g2, &req("https://api.openai.com/v1", vec![])), None);
    }

    #[test]
    fn scrubbing_streamed_state() {
        let mut st = ChatState { scrubber: Some(Arc::new(Scrubber::new(["sk-live-0123456789"]))), ..ChatState::default() };
        st.text.push_str("key: sk-live-01234");
        st.scrub();
        // a prefix alone is not the secret
        assert_eq!(st.text, "key: sk-live-01234");
        st.text.push_str("56789 end");
        st.scrub();
        assert_eq!(st.text, "key: [redacted] end");
        st.model = Some("sk-live-0123456789".into());
        st.scrub();
        assert_eq!(st.model.as_deref(), Some("[redacted]"));
    }

    #[test]
    fn requests() {
        assert_eq!(endpoint("https://api.openai.com/v1/"), "https://api.openai.com/v1/chat/completions");
        assert_eq!(endpoint("https://r.openai.azure.com/openai/deployments/d?api-version=2024-06-01"), "https://r.openai.azure.com/openai/deployments/d/chat/completions?api-version=2024-06-01");
        assert_eq!(endpoint("http://localhost:11434/v1/chat/completions"), "http://localhost:11434/v1/chat/completions");
        let lua_like = Data::map([
            ("base_url", Data::str("http://localhost:11434/v1")),
            ("model", Data::str("llama3.2")),
            ("messages", Data::list(vec![Data::map([("role", Data::str("user")), ("content", Data::str("hi"))])])),
            ("temperature", Data::Num(0.5)),
            ("tools", Data::list(vec![Data::map([("type", Data::str("function")), ("function", Data::map([("name", Data::str("f")), ("parameters", Data::list(vec![]))]))])])),
            ("extra", Data::map([("options", Data::map([("num_ctx", Data::Num(4096.0))]))])),
        ]);
        let r = parse_request(&lua_like).unwrap();
        assert_eq!(r.url, "http://localhost:11434/v1/chat/completions");
        assert_eq!(r.body["temperature"], json!(0.5));
        assert_eq!(r.body["stream"], json!(true));
        assert_eq!(r.body["tools"][0]["function"]["parameters"], json!({ "type": "object", "properties": {} }));
        assert_eq!(r.body["options"]["num_ctx"], json!(4096));
        assert!(parse_request(&Data::str("x")).is_err());
        assert!(parse_request(&Data::map([("model", Data::str("m"))])).unwrap_err().contains("base_url"));
        let bad = Data::map([("base_url", Data::str("http://x")), ("messages", Data::list(vec![Data::map([("content", Data::str("x"))])]))]);
        assert!(parse_request(&bad).unwrap_err().contains("role"));
    }

    #[test]
    fn size_cap() {
        let mut st = ChatState::default();
        let big = "x".repeat(MAX_REPLY / 2 + 1);
        let chunk = json!({ "choices": [{ "delta": { "content": big } }] });
        st.apply(&chunk).unwrap();
        assert!(st.apply(&chunk).unwrap_err().contains("larger"));
    }

    #[test]
    fn history_trim() {
        let mut h = vec![json!({"role": "system", "content": "s"})];
        let big = "y".repeat(MAX_HISTORY / 3);
        for _ in 0..4 {
            h.push(json!({"role": "user", "content": big}));
        }
        trim_history(&mut h);
        assert_eq!(h[0]["role"], "system");
        assert!(h.len() < 5);
    }
}
