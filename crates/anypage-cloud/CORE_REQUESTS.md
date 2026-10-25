# Core requests from the cloud module

Changes to the runtime the cloud module would benefit from. None blocks the
module itself, but #2 blocks giving secrets to untrusted pages; each lists
the workaround in place.

## 1. Handlers cannot read stream or derived cells (`page.peek`)

**Why.** A chat page needs the finished assistant reply when the user sends
the next message (to build the next request). `page.get` only reads input
cells, and a handler has no way to read a stream cell's current value.

**Workaround.** `cloud.chat` keeps the conversation itself when the request
has `keep_history = true`: the page sends only the new messages, the source
prepends the kept history, and items carry `messages` (the whole
conversation) for display. This is useful anyway, but pages that manage
history themselves (editing a past turn, tool-result turns built from the
reply's `tool_calls`) still need the reply in the handler.

**Proposal.** A read-only `page.peek(cell)` usable in handlers and actions
(not inside cell functions, where inputs must be declared), returning the
cell's last committed value without forcing evaluation:

```rust
// api.rs, install_handlers
page.set("peek", func(lua, move |lua, cell: AnyUserData| { /* graph.view(id).value -> Data */ }))
```

It needs the graph reachable from `Shared` after load (today the graph moves
into `ScriptPage`); a snapshot map `CellId -> Data` refreshed on commit would
also do.

## 2. Secrets bound to hosts

**Why.** Today a page declaring `"secrets": ["openai"]` and
`"net": ["api.openai.com", "my-server.example"]` can send the OpenAI key to
`my-server.example` (e.g. `cloud.http{ url = "https://my-server.example", secret = "openai" }`).
The module enforces both lists but cannot know which secret belongs to which
host. Users approve the two lists separately, so the combination is not what
they think they approved.

**Integration blocker for untrusted pages.** Until this lands, a downloaded
or AI-written page that declares a secret and a host it controls can send
the secret there. Do not grant secrets to untrusted pages before it is done.

**Interim in the module.**
- The URL is checked against `capabilities.net` before any secret is read,
  signed with or exchanged for a token.
- A secret's owner (whoever fills the device's secret store, not the page)
  can bind it: a JSON value with `"hosts": ["api.openai.com", ...]` is only
  sent, signed with, or exchanged for requests to those hosts; an opaque key
  then goes in `"value"`. Enforced for every auth mode, `cloud.s3`,
  `cloud.s3_presign` and `cloud.gcs`.
- Google tokens are bound by default to `*.googleapis.com` and the key
  file's own token endpoint host (so a page cannot point `cloud.gcs`'s
  `endpoint` at a host it controls and receive the OAuth token).
- `cloud.chat` attaches the `openai` secret by default only for
  `https://api.openai.com`; other servers need the request to name a secret.
- Credentials are never sent across origins on redirects.
- The request's credentials, and their base64 forms, are scrubbed from
  everything handed back to the page (bodies, text, json, headers, streamed
  chat text and tool calls, event data) and from error messages, so an
  echoing server cannot give them to the script. This does not stop the
  server itself from keeping what it received.

**Proposal.** Let `capabilities.secrets` entries name their hosts, and add a
check to `Grants`:

```json
"secrets": [{ "name": "openai", "hosts": ["api.openai.com"] }, "legacy-name"]
```

```rust
impl Grants {
    /// The secret, only if `url`'s host is one it is bound to (or it is unbound).
    pub fn secret_for(&self, name: &str, url: &str) -> Result<String, String>;
}
```

The cloud module would switch every `grants.secret(name)` to
`grants.secret_for(name, url)` (a one-line change per call site: chat, http,
s3, gcs, presign, and the GCP token endpoint).

## 3. Interrupting a blocked network read (resolved in the module)

No runtime change needed. Done in the module: network jobs run on threads of
their own, so a cancelled job returns `"cancelled"` within ~20 ms and frees
the page's worker; every connection goes through a ureq connector wrapper
(`src/cancel.rs`) whose sockets are shut down when the request's token is
cancelled, so a read blocked on a silent server ends at once. Measured: with
`job_workers = 1`, the next job's result arrives 16–27 ms after the inputs
change, while the abandoned request was waiting on a server that never
answers. Remaining limit: a request still resolving the host name or in the
TCP connect ends only at the connect timeout (15 s); at most 32 request
threads per page and 4 per stream exist at a time.

## 4. Engine-level item wake for `latest = true` streams

Not a request, an observation: `Sink::push` returns `false` both when the
page's buffer is full and when the page is gone. The module retries final
items for up to 5 s; a distinct "disconnected" result would let it stop at
once.

```rust
pub enum Pushed { Ok, Full, Closed }
impl Sink { pub fn try_push(&self, item: Data) -> Pushed; }
```

## 5. Observation: IPv6 hosts cannot be declared

`Grants::check_url` splits patterns at the last `:` and `url_host` strips
the brackets, so neither `"[::1]"` nor `"::1"` in `capabilities.net`
matches `http://[::1]:8080/`. The module accepts `[::1]` as loopback, but a
page cannot declare it. Proposal: in `check_url`, parse a bracketed pattern
(`[::1]`, `[::1]:8080`) as host plus optional port and compare it with the
bracket-less host `url_host` returns.

## 6. License decision: webpki-roots (CDLA-Permissive-2.0)

Not a code request, a decision for the project owner. The task requires the
webpki root store, and `webpki-roots` 1.x is licensed CDLA-Permissive-2.0,
which is outside the MIT/Apache/BSD/ISC/Zlib/MPL list. Every other
dependency of this crate is inside the list. Options: accept
CDLA-Permissive-2.0 for the root store (it is a permissive data license), or
switch to the operating system's roots (`rustls-platform-verifier`, Apache-2.0
OR MIT; on Android it needs the host to register its Java component). The
module does not change until this is decided.
