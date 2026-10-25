//! The native library a page can use by name. Heavy work stays compiled into
//! the app; a page only names it, wires it into its graph, and declares the
//! module in its capabilities.
//!
//! A module ("pdf", "sheet", "cloud", ...) registers any of:
//!
//! | kind | runs | script side |
//! |---|---|---|
//! | function | inline, on the calling thread | `pdf.page_count(doc)` or `doc:page_count()` |
//! | job | on the worker pool, cancellable, with progress | `page.job(name, "pdf.render", {inputs})` |
//! | source | its own thread(s), pushing items | `page.stream(name, "cloud.openai_chat", {request = cell})` |
//! | tiles | tile workers | `page.tiled{source = "pdf.page", params = cell}` |
//! | layer | the GPU, into an offscreen texture | `page.layer{kind = "3d.scene", inputs = {...}}` |
//! | Luau library | the page's realm | `local ui = import("ui")` |
//!
//! Functions marked [`Effect::Pure`] may run anywhere, including inside a
//! cell's function. [`Effect::Effect`] functions (they change the outside
//! world or a resource) run only at load and in event handlers.

use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::thread::JoinHandle;

use anypage_core::{CellId, EvalCtx, Graph, Op, Rect, StreamProducer, Value};
use anypage_engine::{GpuRenderer, TileSource};

use crate::data::Data;
use crate::package::Caps;

/// Secrets the host keeps for pages (API keys, cloud credentials). A page
/// never reads them; native modules use them on its behalf.
pub trait SecretStore: Send + Sync {
    fn get(&self, page_id: &str, name: &str) -> Option<String>;
}

/// Secrets from environment variables `ANYPAGE_SECRET_<NAME>` (upper case,
/// '-' and '.' as '_'). For development and CI.
pub struct EnvSecrets;

impl SecretStore for EnvSecrets {
    fn get(&self, _page_id: &str, name: &str) -> Option<String> {
        let key = format!("ANYPAGE_SECRET_{}", name.to_uppercase().replace(['-', '.'], "_"));
        std::env::var(key).ok().filter(|s| !s.is_empty())
    }
}

/// What one page is allowed to do, handed to every native call it makes.
pub struct Grants {
    pub page_id: String,
    pub caps: Caps,
    pub secrets: Option<Arc<dyn SecretStore>>,
}

/// Run native code so that a panic becomes an error string instead of
/// unwinding through the script VM or killing a worker thread.
pub fn guarded<T>(what: &str, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(p) => {
            let msg = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
            log::error!(target: "anypage_page", "{what} panicked: {msg}");
            Err(format!("{what}: internal error"))
        }
    }
}

/// A host pattern from `capabilities.net`: "host", "*.host", "host:port",
/// "[v6]", "[v6]:port". Returns (host pattern, port).
fn split_pattern(p: &str) -> (String, Option<u16>) {
    if let Some(v6) = p.strip_prefix('[') {
        if let Some((h, rest)) = v6.split_once(']') {
            return (h.to_string(), rest.strip_prefix(':').and_then(|x| x.parse().ok()));
        }
    }
    match p.rsplit_once(':') {
        Some((h, port)) if !h.contains(':') && port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() => (h.to_string(), port.parse().ok()),
        _ => (p.to_string(), None),
    }
}

fn host_matches(pattern: &str, host: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let host = host.to_ascii_lowercase();
    match pattern.strip_prefix("*.") {
        Some(suffix) => host == suffix || host.ends_with(&format!(".{suffix}")),
        None => pattern == host,
    }
}

/// "https://api.example.com:8443/v1/x" → ("api.example.com", Some(8443)).
pub fn url_host(url: &str) -> Option<(String, Option<u16>)> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map(|(_, a)| a).unwrap_or(authority);
    if let Some(v6) = authority.strip_prefix('[') {
        let (h, port) = v6.split_once(']')?;
        return Some((h.to_string(), port.strip_prefix(':').and_then(|p| p.parse().ok())));
    }
    match authority.rsplit_once(':') {
        Some((h, p)) => Some((h.to_string(), p.parse().ok())),
        None => Some((authority.to_string(), None)),
    }
}

impl Grants {
    pub fn new(page_id: &str, caps: Caps) -> Self {
        Self { page_id: page_id.to_string(), caps, secrets: None }
    }

    /// May this page talk to `url`? Patterns in `capabilities.net`:
    /// "api.openai.com", "*.amazonaws.com", "localhost:11434".
    pub fn check_url(&self, url: &str) -> Result<(), String> {
        let (host, port) = url_host(url).ok_or_else(|| format!("not a URL: {url}"))?;
        let ok = self.caps.net.iter().any(|p| {
            let (h, pp) = split_pattern(p);
            host_matches(&h, &host) && (pp.is_none() || pp == port)
        });
        if ok {
            Ok(())
        } else {
            Err(format!("{host} is not declared in page.json capabilities.net"))
        }
    }

    /// A secret for a request to `url`: the URL must be allowed, and when
    /// page.json binds the secret to hosts (`{"name": "openai", "hosts":
    /// ["api.openai.com"]}`), the URL's host must be one of them.
    pub fn secret_for(&self, name: &str, url: &str) -> Result<String, String> {
        self.check_url(url)?;
        self.check_secret_url(name, url)?;
        self.secret(name)
    }

    /// May the secret `name` be sent to `url`? (Host binding only.)
    pub fn check_secret_url(&self, name: &str, url: &str) -> Result<(), String> {
        let Some(hosts) = self.caps.secret_hosts.get(name) else { return Ok(()) };
        let (host, port) = url_host(url).ok_or_else(|| format!("not a URL: {url}"))?;
        let ok = hosts.iter().any(|p| {
            let (h, pp) = split_pattern(p);
            host_matches(&h, &host) && (pp.is_none() || pp == port)
        });
        if ok {
            Ok(())
        } else {
            Err(format!("secret \"{name}\" is bound to {} in page.json and may not be sent to {host}", hosts.join(", ")))
        }
    }

    /// A secret by name; it must be declared in `capabilities.secrets`.
    pub fn secret(&self, name: &str) -> Result<String, String> {
        if !self.caps.secrets.iter().any(|s| s == name) {
            return Err(format!("secret \"{name}\" is not declared in page.json capabilities.secrets"));
        }
        self.secrets
            .as_ref()
            .and_then(|s| s.get(&self.page_id, name))
            .ok_or_else(|| format!("secret \"{name}\" is not set on this device"))
    }
}

/// An inline native function call.
pub struct Call<'a> {
    pub args: &'a [Data],
    pub grants: &'a Grants,
}

impl Call<'_> {
    pub fn arg(&self, i: usize) -> &Data {
        static NIL: Data = Data::Nil;
        self.args.get(i).unwrap_or(&NIL)
    }
}

/// An async job on the worker pool.
pub struct JobCtx<'a> {
    /// Input cell values; not-ready inputs never reach a job (it stays pending).
    pub inputs: &'a [Data],
    pub opts: &'a Data,
    pub grants: &'a Grants,
    pub(crate) eval: &'a EvalCtx,
}

impl JobCtx<'_> {
    pub fn input(&self, i: usize) -> &Data {
        static NIL: Data = Data::Nil;
        self.inputs.get(i).unwrap_or(&NIL)
    }
    pub fn cancelled(&self) -> bool {
        self.eval.cancelled()
    }
    pub fn progress(&self, p: f32) {
        self.eval.set_progress(p)
    }
}

/// Producer side of a source stream; clone it into threads.
#[derive(Clone)]
pub struct Sink(pub(crate) StreamProducer<(Data, std::time::Instant)>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pushed {
    Ok,
    /// The buffer between frames is full (the page is slow); the item was dropped.
    Full,
    /// The page is gone: stop producing.
    Closed,
}

impl Sink {
    /// Non-blocking; false if the buffer is full or the page is gone. The
    /// push time is kept for item-to-screen latency.
    pub fn push(&self, item: Data) -> bool {
        self.0.push((item, std::time::Instant::now()))
    }
    /// Like `push`, but says whether the page is gone or just busy.
    pub fn try_push(&self, item: Data) -> Pushed {
        let before = self.0.dropped();
        if self.0.push((item, std::time::Instant::now())) {
            Pushed::Ok
        } else if self.0.dropped() > before {
            Pushed::Full
        } else {
            Pushed::Closed
        }
    }
}

/// What a source gets when a page creates it.
pub struct SourceCtx {
    pub label: String,
    pub opts: Data,
    pub sink: Sink,
    pub grants: Arc<Grants>,
    /// New values of the `request` input cell the page passed, starting with
    /// its initial value. A source that serves requests (a chat completion,
    /// a device connection) reads them here.
    pub requests: Receiver<Data>,
    on: Arc<AtomicBool>,
    quit: Arc<AtomicBool>,
    has_request: bool,
}

impl SourceCtx {
    /// False while the page has the stream switched off (`enabled` cell).
    pub fn on(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }
    /// True once the page is shutting down: threads should exit.
    pub fn quit(&self) -> bool {
        self.quit.load(Ordering::Relaxed)
    }
    pub fn flags(&self) -> (Arc<AtomicBool>, Arc<AtomicBool>) {
        (self.on.clone(), self.quit.clone())
    }
    /// Did the page pass a `request` cell? (Without one, nothing ever arrives
    /// on `requests`.)
    pub fn has_request(&self) -> bool {
        self.has_request
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(label: String, opts: Data, sink: Sink, grants: Arc<Grants>, requests: Receiver<Data>, on: Arc<AtomicBool>, quit: Arc<AtomicBool>, has_request: bool) -> Self {
        Self { label, opts, sink, grants, requests, on, quit, has_request }
    }
}

/// Kept alive while the page lives; dropped at shutdown (after `quit` is set).
pub type SourceGuard = Box<dyn Any + Send>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    /// No side effects: safe inside a cell's function.
    Pure,
    /// Changes the world or a resource: only at load and in handlers.
    Effect,
}

/// What the page's graph sees of a stream it declared (legacy, low level).
pub struct StreamSpec<'a> {
    pub label: &'a str,
    pub opts: &'a Data,
    pub on: Arc<AtomicBool>,
    pub quit: Arc<AtomicBool>,
}

pub struct StreamHandle {
    /// The cell scripts read. Its value should be [`crate::Data`] or [`crate::Streamed`].
    pub cell: CellId,
    pub threads: Vec<JoinHandle<()>>,
}

pub type FunctionFn = Arc<dyn Fn(&Call) -> Result<Data, String> + Send + Sync>;
pub type JobFnData = Arc<dyn Fn(&JobCtx) -> Result<Data, String> + Send + Sync>;
pub type SourceFn = Arc<dyn Fn(SourceCtx) -> Result<SourceGuard, String> + Send + Sync>;
pub type StreamFn = Arc<dyn Fn(&mut Graph, StreamSpec) -> Result<StreamHandle, String> + Send + Sync>;
/// Builds an async operator; its inputs are the cells the page passes.
pub type JobFn = Arc<dyn Fn(&Data) -> Result<Arc<dyn Op>, String> + Send + Sync>;
/// Builds a tile source for `rect`; it receives the page's params cell value (a [`crate::Data`]).
pub type TileFn = Arc<dyn Fn(Rect, &Data) -> Result<Arc<dyn TileSource>, String> + Send + Sync>;
/// Turns a layer's input values into the value its renderer draws.
pub type LayerConvert = Arc<dyn Fn(&[Data]) -> Result<Value, String> + Send + Sync>;

#[derive(Clone)]
pub struct LayerKind {
    pub renderer: Arc<dyn GpuRenderer>,
    pub convert: LayerConvert,
}

#[derive(Clone, Default)]
pub struct Natives {
    pub(crate) functions: HashMap<String, (Effect, FunctionFn)>,
    pub(crate) jobs_data: HashMap<String, JobFnData>,
    pub(crate) sources: HashMap<String, SourceFn>,
    pub(crate) streams: HashMap<String, StreamFn>,
    pub(crate) jobs: HashMap<String, JobFn>,
    pub(crate) tiles: HashMap<String, TileFn>,
    pub(crate) layers: HashMap<String, LayerKind>,
    pub(crate) lua_modules: HashMap<String, &'static str>,
    /// Stream defaults a source declares: (latest wins, cap).
    pub(crate) source_defaults: HashMap<String, (bool, usize)>,
    /// Modules this build provides, for `capabilities.modules` checks.
    pub(crate) modules: std::collections::BTreeSet<String>,
}

fn module_of(name: &str) -> String {
    name.split('.').next().unwrap_or(name).to_string()
}

impl Natives {
    pub fn new() -> Self {
        Self::default()
    }

    /// An inline function, `module.name`, visible to scripts as a field of
    /// the global table `module`.
    pub fn function(&mut self, name: &str, effect: Effect, f: impl Fn(&Call) -> Result<Data, String> + Send + Sync + 'static) {
        self.modules.insert(module_of(name));
        self.functions.insert(name.to_string(), (effect, Arc::new(f)));
    }

    /// An async job over `Data` inputs. The job is pending while any input
    /// is; it is cancelled when an input changes or the page cancels it.
    pub fn job_fn(&mut self, name: &str, f: impl Fn(&JobCtx) -> Result<Data, String> + Send + Sync + 'static) {
        self.modules.insert(module_of(name));
        self.jobs_data.insert(name.to_string(), Arc::new(f));
    }

    /// A stream of `Data` items produced on the source's own threads.
    pub fn source(&mut self, name: &str, f: impl Fn(SourceCtx) -> Result<SourceGuard, String> + Send + Sync + 'static) {
        self.modules.insert(module_of(name));
        self.sources.insert(name.to_string(), Arc::new(f));
    }

    /// Defaults for a source's stream when the page does not say: frame
    /// sources want `latest = true` (only the newest item matters).
    pub fn source_defaults(&mut self, name: &str, latest: bool, cap: usize) {
        self.source_defaults.insert(name.to_string(), (latest, cap.max(1)));
    }

    /// Low-level stream: builds its own cells (typed items, custom adapters).
    pub fn stream(&mut self, name: &str, f: impl Fn(&mut Graph, StreamSpec) -> Result<StreamHandle, String> + Send + Sync + 'static) {
        self.modules.insert(module_of(name));
        self.streams.insert(name.to_string(), Arc::new(f));
    }

    /// Low-level job: returns its own operator.
    pub fn job(&mut self, name: &str, f: impl Fn(&Data) -> Result<Arc<dyn Op>, String> + Send + Sync + 'static) {
        self.modules.insert(module_of(name));
        self.jobs.insert(name.to_string(), Arc::new(f));
    }

    pub fn tile(&mut self, name: &str, f: impl Fn(Rect, &Data) -> Result<Arc<dyn TileSource>, String> + Send + Sync + 'static) {
        self.modules.insert(module_of(name));
        self.tiles.insert(name.to_string(), Arc::new(f));
    }

    /// A GPU layer kind: `convert` turns the layer's inputs into the value
    /// `renderer` draws.
    pub fn layer(&mut self, name: &str, renderer: Arc<dyn GpuRenderer>, convert: impl Fn(&[Data]) -> Result<Value, String> + Send + Sync + 'static) {
        self.modules.insert(module_of(name));
        self.layers.insert(name.to_string(), LayerKind { renderer, convert: Arc::new(convert) });
    }

    /// A Luau library pages load with `import(name)`. Libraries are ordinary
    /// page code: sandboxed, quota-bound, no extra powers.
    pub fn lua_module(&mut self, name: &str, source: &'static str) {
        self.lua_modules.insert(name.to_string(), source);
    }

    /// Add everything from another registry (a module crate's `register`).
    pub fn extend(&mut self, other: Natives) {
        self.functions.extend(other.functions);
        self.jobs_data.extend(other.jobs_data);
        self.sources.extend(other.sources);
        self.streams.extend(other.streams);
        self.jobs.extend(other.jobs);
        self.tiles.extend(other.tiles);
        self.layers.extend(other.layers);
        self.lua_modules.extend(other.lua_modules);
        self.source_defaults.extend(other.source_defaults);
        self.modules.extend(other.modules);
    }

    pub fn has_module(&self, m: &str) -> bool {
        self.modules.contains(m)
    }

    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .functions
            .keys()
            .map(|k| format!("fn:{k}"))
            .chain(self.jobs_data.keys().chain(self.jobs.keys()).map(|k| format!("job:{k}")))
            .chain(self.sources.keys().chain(self.streams.keys()).map(|k| format!("stream:{k}")))
            .chain(self.tiles.keys().map(|k| format!("tiles:{k}")))
            .chain(self.layers.keys().map(|k| format!("layer:{k}")))
            .chain(self.lua_modules.keys().map(|k| format!("lua:{k}")))
            .collect();
        v.sort();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grants(net: &[&str], secrets: &[&str]) -> Grants {
        let caps = Caps { net: net.iter().map(|s| s.to_string()).collect(), secrets: secrets.iter().map(|s| s.to_string()).collect(), ..Caps::default() };
        Grants::new("t", caps)
    }

    #[test]
    fn hosts_and_ports() {
        let g = grants(&["api.openai.com", "*.amazonaws.com", "localhost:11434"], &[]);
        assert!(g.check_url("https://api.openai.com/v1/chat/completions").is_ok());
        assert!(g.check_url("https://s3.us-east-1.amazonaws.com/b/k").is_ok());
        assert!(g.check_url("https://amazonaws.com").is_ok());
        assert!(g.check_url("https://evil-amazonaws.com").is_err());
        assert!(g.check_url("http://localhost:11434/v1").is_ok());
        assert!(g.check_url("http://localhost:8080/v1").is_err());
        assert!(g.check_url("https://user@api.openai.com.evil.com/").is_err());
        assert_eq!(url_host("http://[::1]:9000/x"), Some(("::1".into(), Some(9000))));
    }

    #[test]
    fn ipv6_and_bound_secrets() {
        let mut g = grants(&["[::1]:8080", "api.openai.com", "*.example.com"], &["openai"]);
        g.caps.secret_hosts.insert("openai".into(), vec!["api.openai.com".into()]);
        assert!(g.check_url("http://[::1]:8080/x").is_ok());
        assert!(g.check_url("http://[::1]:9090/x").is_err());
        assert!(g.check_secret_url("openai", "https://api.openai.com/v1").is_ok());
        assert!(g.check_secret_url("openai", "https://evil.example.com/v1").unwrap_err().contains("bound"));
        assert!(g.secret_for("openai", "https://evil.example.com/v1").is_err());
        assert!(guarded("x", || -> Result<(), String> { panic!("boom") }).unwrap_err().contains("internal error"));
    }

    #[test]
    fn secrets_must_be_declared() {
        let g = grants(&[], &["openai"]);
        assert!(g.secret("aws").unwrap_err().contains("not declared"));
        assert!(g.secret("openai").unwrap_err().contains("not set"));
    }
}
