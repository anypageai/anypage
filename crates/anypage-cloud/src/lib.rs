//! Cloud module (`cloud`): HTTPS with the page's capabilities enforced,
//! OpenAI-compatible chat streaming, event streams, AWS S3 (SigV4) and
//! Google Cloud Storage. See docs/MODULES.md for the module contract.
//!
//! A page declares the module, every host it talks to and every secret it
//! uses:
//!
//! ```json
//! "capabilities": {
//!   "modules": ["cloud"],
//!   "net": ["api.openai.com", "localhost:11434", "*.amazonaws.com", "storage.googleapis.com", "oauth2.googleapis.com"],
//!   "secrets": ["openai", "aws", "gcp"]
//! }
//! ```
//!
//! # Luau API
//!
//! | name | kind | what |
//! |---|---|---|
//! | `cloud.chat` | source (use `latest = true`) | OpenAI-compatible `/chat/completions` streaming |
//! | `cloud.sse` | source | any `text/event-stream` endpoint, one item per event |
//! | `cloud.http` | job | one HTTP request → `{ status, ok, headers, body, text, json, url }` |
//! | `cloud.s3` | job | S3 `get`/`put`/`list`/`delete`/`head` with SigV4 |
//! | `cloud.gcs` | job | Cloud Storage `get`/`put`/`list`/`delete`/`head` |
//! | `cloud.s3_presign{...}` | function (effect) | presigned S3 URL |
//! | `cloud.check_url(url)` | function (pure) | `{ ok, error }`: would the page be allowed to call it |
//! | `cloud.parse_sse(text)` | function (pure) | `{ {event, data, id} }`, input up to 4 MB |
//! | `cloud.url_encode(s)` | function (pure) | RFC 3986 percent-encoding |
//! | `cloud.query(map)` | function (pure) | `"a=1&b=x%20y"`, keys sorted |
//! | `cloud.sha256(bytes)` | function (pure) | hex digest, input up to 16 MB |
//!
//! ## `cloud.chat`
//!
//! ```lua
//! local req = page.input("chat.req", nil)
//! local chat = page.stream("chat", "cloud.chat", { request = req, latest = true })
//! page.set(req, { base_url = "http://localhost:11434/v1", model = "llama3.2",
//!                 messages = { { role = "user", content = "Hi" } },
//!                 tools = {...}, temperature = 0.7, max_tokens = 512, secret = "openai" })
//! -- chat value: { items, total, last = { seq, text, reasoning, tool_calls = { {id, name, arguments, args} },
//! --               done, finish_reason, usage, error, cancelled, model, status, first_token_ms, elapsed_ms } }
//! ```
//!
//! A new request cancels the one in flight; `nil` cancels (the cancelled
//! request ends with `cancelled = true`). Tool-call deltas are accumulated
//! by index. The newest state always reaches the page (at most one item per
//! 8 ms), also when the model pauses mid-reply.
//!
//! `secret`: the secret whose key is sent. When the request names none, the
//! `"openai"` secret is used only for `https://api.openai.com` (and only if
//! the page declares it); any other server gets a key only when the request
//! names one. This differs from `cloud.s3`/`cloud.gcs`, whose default secret
//! names apply to any endpoint: a chat page is typically pointed at local or
//! agent-chosen servers, and secrets are not bound to hosts by the runtime.
//!
//! Other request fields: `url` (full endpoint instead of `base_url`),
//! `tool_choice`, `top_p`, `stop`, `seed`, `response_format`,
//! `reasoning_effort`, `extra` (merged into the JSON body), `headers`,
//! `auth` (`"bearer"`, `"api-key"` (Azure, the default for `*.openai.azure.com`),
//! `"none"`, ...), `usage = false` (omit `stream_options`), `timeout` (idle
//! seconds). With `keep_history = true` the source keeps the conversation:
//! send only new messages, items carry `messages` (the whole conversation),
//! `reset = true` starts over.
//!
//! ## `cloud.http`
//!
//! ```lua
//! local r = page.job("r", "cloud.http", { req_cell })
//! -- req: { method, url, headers, body | json | form, secret, auth, timeout, idle_timeout,
//! --        max_bytes, follow_redirects, region, service (auth = "aws"), scope (auth = "gcp") }
//! ```
//!
//! `auth`: `"bearer"` (default), `"api-key"`, `"x-api-key"`, `"basic"`,
//! `"header:<Name>"`, `"aws"` (SigV4 for any AWS API), `"gcp"` (OAuth token).
//!
//! ## Job timeouts (`cloud.http`, `cloud.s3`, `cloud.gcs`)
//!
//! `timeout`: seconds for the whole exchange (up to 24 h; it also bounds
//! connecting and the wait for the response headers). `idle_timeout`:
//! seconds of silence while sending or receiving (default 60). Without
//! `timeout`, a job gets 15 s to connect, 120 s for the response headers, and
//! a total of 120 s plus its size (request body + `max_bytes`) at 64 KB/s, so
//! a 256 MB upload may take about 70 minutes on a slow link.
//!
//! ## `cloud.s3`, `cloud.s3_presign`
//!
//! `{ op = "get"|"put"|"list"|"delete"|"head", bucket, key, prefix, delimiter,
//! continuation_token, max_keys, body, content_type, metadata, range, region,
//! endpoint, path_style, payload = "unsigned", timeout, idle_timeout,
//! max_bytes, secret = "aws" }`. The secret is JSON
//! `{ access_key_id, secret_access_key, session_token }`.
//! `get` → `{ status, body, text, size, etag, content_type, last_modified, metadata }`;
//! `list` → `{ objects = { {key, size, etag, last_modified, storage_class} }, prefixes, truncated, next_token, count }`;
//! `head` → `{ exists, size, etag, ... }`; `put` → `{ status, etag }`.
//! `cloud.s3_presign{ bucket, key, method = "GET", expires = 3600, region, endpoint }` → URL.
//! Requests and URLs are always signed with the device clock.
//!
//! ## `cloud.gcs`
//!
//! `{ op, bucket, object, prefix, delimiter, page_token, max, body,
//! content_type, endpoint, scope, subject, timeout, idle_timeout, max_bytes,
//! secret = "gcp" }`. The secret is a service-account key file (RS256 JWT →
//! OAuth token, cached until expiry), an `authorized_user` file, or a raw
//! access token (`"ya29..."` or `{"access_token": "...", "hosts": [...]}`).
//! The token endpoint (`oauth2.googleapis.com`) must be in
//! `capabilities.net` too. Google tokens go only to `*.googleapis.com` and
//! to the key file's own token host (an emulator), unless the secret names
//! its `"hosts"`.
//!
//! ## `cloud.sse`
//!
//! `{ url, method, headers, body | json, secret, auth, timeout, idle_timeout,
//! max_event, max_bytes }`. Each event may be up to `max_event` bytes
//! (default 8 MB, at most 64 MB); a stream has no total limit unless
//! `max_bytes` sets one, so live feeds can run for hours.
//!
//! # Security
//!
//! * Every URL and every redirect hop passes `Grants::check_url`; only https
//!   is allowed, except plain http to localhost/127.0.0.1/`[::1]`; a URL with
//!   a bad port (or port 0) is refused. The URL is checked before any secret
//!   is read, signed with or exchanged for a token.
//! * Credentials are sent only to the origin a request started at.
//! * A secret's owner can bind it to hosts by storing it as JSON with
//!   `"hosts": [...]` (an opaque key then goes in `"value"`); see `auth`.
//! * Secrets never reach the script: they are redacted from error messages,
//!   and the request's credentials (and their base64 forms) are scrubbed
//!   from everything a response hands back: body, text, json, headers, url,
//!   streamed chat text, reasoning and tool calls, event data.
//! * Responses are size-capped (32 MB by default, `max_bytes` up to 512 MB;
//!   chat replies 8 MB); every phase has a timeout.
//! * Cancellation is immediate: a cancelled job returns at once and frees the
//!   page's worker (the request runs on its own thread, see `jobs`), and the
//!   cancelled request's socket is shut down, also while it waits for a
//!   silent server (see `cancel`). At most 32 request threads per page and 4
//!   per stream exist at a time.
//!
//! Known limits (see `CORE_REQUESTS.md`): a secret without `"hosts"` may be
//! sent to any host the page declares (the runtime does not bind secrets to
//! hosts yet), so pages that declare a secret plus hosts they control are
//! not safe to run untrusted. A request still resolving the host name or
//! opening the TCP connection when cancelled ends only at the connect
//! timeout (15 s). An intermediate chat item may show the first part of an
//! echoed key before the rest arrives (whole values are scrubbed).

pub mod auth;
pub mod cancel;
pub mod chat;
pub mod gcp;
pub mod http;
pub mod jobs;
pub mod net;
pub mod s3;
pub mod sigv4;
pub mod sse;
pub mod stream;
pub mod util;
pub mod xml;

use std::panic::{catch_unwind, AssertUnwindSafe};

use anypage_runtime::{Call, Data, Effect, JobCtx, Natives};

/// Largest input of the pure `cloud.parse_sse` (it may run in a cell
/// function on the engine thread).
pub const MAX_PARSE_SSE: usize = 4 << 20;
/// Largest input of the pure `cloud.sha256`.
pub const MAX_SHA256: usize = 16 << 20;

/// Run native code so that a bug becomes an error string, never a panic
/// across the page boundary.
pub(crate) fn guarded<T>(what: &str, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(p) => {
            let msg = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
            log::error!(target: "anypage_cloud", "{what} panicked: {msg}");
            Err(format!("{what}: internal error ({msg})"))
        }
    }
}

/// The job's request: its first input, or the options when it has none.
fn job_request(j: &JobCtx) -> Data {
    let i = j.input(0);
    if i.is_nil() {
        j.opts.clone()
    } else {
        i.clone()
    }
}

fn arg_table<'a>(c: &'a Call, name: &str) -> Result<&'a Data, String> {
    match c.arg(0) {
        d @ Data::Map(_) => Ok(d),
        _ => Err(format!("{name} takes a table")),
    }
}

/// Add this module's functions, jobs and sources.
pub fn register(n: &mut Natives) {
    n.source("cloud.chat", |ctx| guarded("cloud.chat", || chat::source(ctx)));
    n.source("cloud.sse", |ctx| guarded("cloud.sse", || http::sse_source(ctx)));

    // network jobs run on threads of their own (see `jobs`): a cancelled job
    // frees the page's worker at once
    n.job_fn("cloud.http", |j| {
        let req = job_request(j);
        if req.is_nil() {
            return Ok(Data::Nil);
        }
        let name = req.get("secret").as_str().map(str::to_string);
        let r = req.clone();
        jobs::run(j, "cloud.http", move |g, cancel, progress| http::job(g, &r, cancel, progress)).map_err(|e| auth::scrub(j.grants, name.as_deref(), e))
    });
    n.job_fn("cloud.s3", |j| {
        let req = job_request(j);
        if req.is_nil() {
            return Ok(Data::Nil);
        }
        let name = req.get("secret").as_str().unwrap_or("aws").to_string();
        let r = req.clone();
        jobs::run(j, "cloud.s3", move |g, cancel, progress| s3::run(g, &r, http::max_bytes(&r)?, cancel, progress)).map_err(|e| auth::scrub(j.grants, Some(&name), e))
    });
    n.job_fn("cloud.gcs", |j| {
        let req = job_request(j);
        if req.is_nil() {
            return Ok(Data::Nil);
        }
        let name = req.get("secret").as_str().unwrap_or("gcp").to_string();
        let r = req.clone();
        jobs::run(j, "cloud.gcs", move |g, cancel, progress| gcp::run(g, &r, http::max_bytes(&r)?, cancel, progress)).map_err(|e| auth::scrub(j.grants, Some(&name), e))
    });

    // uses the current time and a secret: load time and handlers only.
    // Always the device clock: a page cannot date a URL into the future.
    n.function("cloud.s3_presign", Effect::Effect, |c| {
        let req = arg_table(c, "s3_presign")?;
        let name = req.get("secret").as_str().unwrap_or("aws");
        guarded("cloud.s3_presign", || {
            let secret = c.grants.secret(name)?;
            let m = auth::material(&secret)?;
            let creds = sigv4::Credentials::from_secret(&secret)?;
            let url = s3::presign(c.grants, req, &creds, util::unix_now())?;
            auth::check_binding(name, &m, &net::check(c.grants, &url)?)?;
            c.grants.check_secret_url(name, &url)?;
            Ok(Data::str(&url))
        })
        .map_err(|e| auth::scrub(c.grants, Some(name), e))
    });
    n.function("cloud.check_url", Effect::Pure, |c| {
        let url = c.arg(0).as_str().ok_or("check_url takes a URL string")?;
        Ok(match net::check(c.grants, url) {
            Ok(_) => Data::map([("ok", Data::Bool(true))]),
            Err(e) => Data::map([("ok", Data::Bool(false)), ("error", Data::str(&e))]),
        })
    });
    n.function("cloud.parse_sse", Effect::Pure, |c| {
        let b = c.arg(0).as_bytes().ok_or("parse_sse takes a string or bytes")?;
        if b.len() > MAX_PARSE_SSE {
            return Err(format!("parse_sse: input larger than {}; stream it with cloud.sse instead", net::size_text(MAX_PARSE_SSE)));
        }
        guarded("cloud.parse_sse", || chat::events_data(b))
    });
    n.function("cloud.url_encode", Effect::Pure, |c| Ok(Data::str(&util::encode_strict(c.arg(0).as_str().ok_or("url_encode takes a string")?))));
    n.function("cloud.query", Effect::Pure, |c| {
        let mut pairs: Vec<(String, String)> = c
            .arg(0)
            .entries()
            .map(|(k, v)| {
                let v = match v {
                    Data::Str(s) => s.to_string(),
                    other => other.to_json().to_string(),
                };
                (k.to_string(), v)
            })
            .collect();
        pairs.sort();
        Ok(Data::str(&util::build_query(&pairs)))
    });
    n.function("cloud.sha256", Effect::Pure, |c| {
        let b = c.arg(0).as_bytes().ok_or("sha256 takes a string or bytes")?;
        if b.len() > MAX_SHA256 {
            return Err(format!("sha256: input larger than {}", net::size_text(MAX_SHA256)));
        }
        Ok(Data::str(&util::sha256_hex(b)))
    });
}
