//! A page described by a package. Its entry script runs once at load and
//! builds the graph through `page.*`; after that the graph is fixed and the
//! script's functions run only as operators (when an input changed or a
//! declared time passed), as event handlers, or as actions an agent runs.
//!
//! Script API, all on the global `page` unless noted:
//!
//! | load time | |
//! |---|---|
//! | `clock()` | the page clock cell (seconds) |
//! | `input(name, initial[, {agent = true}])` | an input cell, changed by `page.set` |
//! | `derive(name, {inputs}, fn)` | `fn(values...) -> value[, valid_until]` |
//! | `stream(name, source, {enabled, request, cap, latest, ...})` | a native stream; value `{items, total, last}` |
//! | `job(name, source, {inputs}, opts)` | a native async job |
//! | `panel{rect, class, priority, over, inputs, draw}` | `draw(d, values...) -> [valid_until]` |
//! | `image{rect, source}` | an image value shown without CPU rasterising |
//! | `layer{rect, kind, inputs}` | native GPU drawing (3D scenes, point clouds) |
//! | `tiled{rect, source, params, options}` | native tiles keyed by a params cell |
//! | `effect{rect, shader, params}` | a page WGSL operator; params cell -> up to 8 numbers |
//!
//! | any time | |
//! |---|---|
//! | `on(event, fn)` | events: pointer_down/drag/up/move, wheel, key, text, ime, paste, file_drop, control |
//! | `set(cell, v)`, `get(cell)` | write / read an input cell (applied after the handler) |
//! | `cancel(cell)`, `request("fit")`, `fit_rect(r)` | |
//! | `focus_text(rect or nil)` | the caret while editing: shows the keyboard / places the IME |
//! | `copy(text)`, `paste()` | clipboard (`capabilities.clipboard`) |
//! | `open_file{filters, done}`, `save_file{name, data, done}` | file dialogs (`capabilities.files`) |
//! | `action{name, description, params, run}`, `actions()`, `act(name, params)` | the semantic port |
//! | `now()`, `measure(text, size)`, `log(...)` | |
//! | global `import(name)` | an app library (`"ui"`) or a `.luau` file in the package |

use std::collections::VecDeque;
use std::mem::take;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anypage_core::{val, Graph, ThreadPool, Wake};
use anypage_engine::{HostResult, Page, PageRequest, PageSpec, PointerResponse, TextRaster};
use mlua::{Function, IntoLuaMulti, MultiValue, Value as LuaValue};
use serde_json::{json, Value as Json};

use crate::api::{action_list, dropped_file_table, empty_build, host_result_table, install, lock, Cmd, Env, PeekGuard, Shared, SourceRec, BUILTIN_MODULES};
use crate::data::{value_to_data, Data};
use crate::natives::{Grants, Natives, SecretStore};
use crate::ops::{PhaseGuard, PHASE_HANDLER, PHASE_LOAD};
use crate::package::{Manifest, Package};
use crate::realm::Realm;

pub use crate::api::check_wgsl;

pub struct ScriptPage {
    graph: Graph,
    spec: PageSpec,
    realm: Arc<Realm>,
    shared: Arc<Mutex<Shared>>,
    sources: Vec<SourceRec>,
    quit: Arc<AtomicBool>,
    phase: Arc<AtomicU8>,
    requests: VecDeque<PageRequest>,
    _threads: Vec<std::thread::JoinHandle<()>>,
    _guards: Vec<crate::natives::SourceGuard>,
    pub manifest: Manifest,
    pub origin: String,
    /// Time to build the realm and run the entry script.
    pub load_ms: f64,
    /// Event handlers that failed.
    pub handler_errors: u64,
}

/// Optional things a host passes when loading a page.
#[derive(Default, Clone)]
pub struct LoadOptions {
    pub secrets: Option<Arc<dyn SecretStore>>,
}

type Built = (Graph, PageSpec, Arc<Realm>, Arc<Mutex<Shared>>, Vec<SourceRec>, Vec<std::thread::JoinHandle<()>>, Vec<crate::natives::SourceGuard>);

impl ScriptPage {
    pub fn load(pkg: &Package, natives: &Natives, text: Arc<TextRaster>, wake: Wake) -> Result<Self, String> {
        Self::load_with(pkg, natives, text, wake, LoadOptions::default())
    }

    pub fn load_with(pkg: &Package, natives: &Natives, text: Arc<TextRaster>, wake: Wake, opts: LoadOptions) -> Result<Self, String> {
        let t0 = Instant::now();
        let m = pkg.manifest.clone();
        for module in &m.caps.modules {
            if !natives.has_module(module) && !BUILTIN_MODULES.contains(&module.as_str()) {
                return Err(format!("this page needs the \"{module}\" module, which this app or platform does not provide"));
            }
        }
        let quit = Arc::new(AtomicBool::new(false));
        let phase = Arc::new(AtomicU8::new(PHASE_LOAD));
        let r = Self::build(pkg, natives, text, wake, quit.clone(), phase.clone(), opts);
        if r.is_err() {
            quit.store(true, Ordering::Relaxed); // stop any producers already started
        }
        let (graph, spec, realm, shared, sources, threads, guards) = r?;
        let mut page = ScriptPage {
            graph,
            spec,
            realm,
            shared,
            sources,
            quit,
            phase,
            requests: VecDeque::new(),
            _threads: threads,
            _guards: guards,
            manifest: m,
            origin: pkg.origin.clone(),
            load_ms: 0.0,
            handler_errors: 0,
        };
        let cmds = take(&mut lock(&page.shared).cmds);
        page.apply(cmds);
        page.sync_streams();
        page.demand_eager();
        page.load_ms = t0.elapsed().as_secs_f64() * 1000.0;
        Ok(page)
    }

    fn build(pkg: &Package, natives: &Natives, text: Arc<TextRaster>, wake: Wake, quit: Arc<AtomicBool>, phase: Arc<AtomicU8>, opts: LoadOptions) -> Result<Built, String> {
        let m = &pkg.manifest;
        let mut graph = Graph::new(Arc::new(ThreadPool::new(m.limits.job_workers, "page-job")));
        graph.set_wake(wake);
        let cam_level = graph.input("camera.level", val(0i32));
        let realm = Arc::new(Realm::new(&m.limits)?);
        let shared = Arc::new(Mutex::new(Shared::new(empty_build(graph, cam_level))));
        let mut all = natives.clone();
        crate::text::register(&mut all, text.clone());
        let mut grants = Grants::new(&m.id, m.caps.clone());
        grants.secrets = opts.secrets;
        let env = Arc::new(Env {
            id: m.id.clone(),
            caps: m.caps.clone(),
            limits: m.limits,
            natives: all,
            files: Arc::new(pkg.files.clone()),
            assets: Arc::new(pkg.assets.clone()),
            text,
            realm: Arc::downgrade(&realm),
            quit,
            grants: Arc::new(grants),
            phase: phase.clone(),
            deadline: realm.deadline_handle(),
        });
        realm.setup(|lua| install(lua, &shared, &env))?;
        realm.seal()?;
        let src = pkg.file(&m.entry).ok_or_else(|| format!("{} missing", m.entry))?;
        let ran = {
            let _p = PhaseGuard::enter(&phase, PHASE_LOAD);
            realm.exec(&m.entry, src, env.limits.load_ms)
        };
        let b = lock(&shared).build.take().ok_or("page build state lost")?;
        ran.map_err(|e| if e.starts_with(&m.entry) { e } else { format!("{}: {e}", m.entry) })?;
        if b.panels.is_empty() && b.tiled.is_empty() && b.effects.is_empty() && b.layers.is_empty() {
            return Err(format!("{}: the page declared nothing to show (page.panel, page.image, page.layer, page.tiled, page.effect)", m.entry));
        }
        let spec = PageSpec { bounds: m.bounds, panels: b.panels, tiled: b.tiled, effects: b.effects, layers: b.layers, camera_level: Some(cam_level) };
        Ok((b.graph, spec, realm, shared, b.sources, b.threads, b.guards))
    }

    fn apply(&mut self, cmds: Vec<Cmd>) {
        let mut set = false;
        for c in cmds {
            match c {
                Cmd::Set(id, d) => {
                    for s in &self.sources {
                        if let Some((cell, tx)) = &s.request {
                            if *cell == id {
                                let _ = tx.send(d.clone());
                            }
                        }
                    }
                    self.graph.set(id, val(d));
                    set = true;
                }
                Cmd::Cancel(id) => {
                    self.graph.cancel(id);
                }
                Cmd::Request(r) => self.requests.push_back(r),
            }
        }
        if set {
            self.sync_streams();
            self.demand_eager();
        }
    }

    /// Start eager jobs whose inputs changed (`page.job(..., {eager = true})`).
    fn demand_eager(&mut self) {
        let eager = lock(&self.shared).eager.clone();
        for id in eager {
            if self.graph.is_dirty(id) {
                self.graph.demand(id);
            }
        }
    }

    /// Commit what other threads produced and react to it, as the engine does
    /// each frame. For hosts without an engine and for tests.
    pub fn pump(&mut self) {
        let rep = self.graph.commit();
        let ids: Vec<anypage_core::CellId> = rep.completed.iter().map(|c| c.0).collect();
        if !ids.is_empty() || !rep.streams_changed.is_empty() {
            self.after_commit(&ids);
        } else {
            self.demand_eager();
        }
    }

    /// Producers pause while the input cell named as their `enabled` is off.
    fn sync_streams(&self) {
        let sh = lock(&self.shared);
        for s in &self.sources {
            if let Some(c) = s.enabled {
                s.on.store(sh.mirror.get(&c).is_none_or(Data::truthy), Ordering::Relaxed);
            }
        }
    }

    /// Run the page's handlers for `event` in order until one returns a true
    /// value, then apply what they asked for. Returns that value.
    fn dispatch<A: IntoLuaMulti + Clone>(&mut self, event: &str, args: A) -> Data {
        lock(&self.shared).now = self.graph.clock_now();
        let shared = self.shared.clone();
        let r = {
            let _p = PhaseGuard::enter(&self.phase, PHASE_HANDLER);
            let _peek = PeekGuard::new(&self.graph);
            self.realm.call(self.realm.limits.handler_ms, |lua| {
                let fs: Vec<Function> = match lock(&shared).handlers.get(event) {
                    Some(keys) => keys.iter().map(|k| lua.registry_value(k)).collect::<mlua::Result<_>>()?,
                    None => return Ok(Data::Nil),
                };
                for f in fs {
                    let rets: MultiValue = f.call(args.clone())?;
                    let d = rets.front().map(|v| Data::from_lua(v).unwrap_or_default()).unwrap_or_default();
                    if d.truthy() {
                        return Ok(d);
                    }
                }
                Ok(Data::Nil)
            })
        };
        let out = r.unwrap_or_else(|e| {
            self.handler_errors += 1;
            log::warn!(target: "anypage_page", "{} handler {event}: {e}", self.manifest.id);
            Data::Nil
        });
        let cmds = take(&mut lock(&self.shared).cmds);
        self.apply(cmds);
        out
    }

    /// Call a stored Luau function (a callback or an action) as a handler.
    fn call_stored(&mut self, what: &str, get: impl FnOnce(&mlua::Lua) -> mlua::Result<Option<(Function, LuaValue)>>) -> Result<Data, String> {
        lock(&self.shared).now = self.graph.clock_now();
        let r = {
            let _p = PhaseGuard::enter(&self.phase, PHASE_HANDLER);
            let _peek = PeekGuard::new(&self.graph);
            self.realm.call(self.realm.limits.handler_ms, |lua| match get(lua)? {
                Some((f, arg)) => {
                    let v: LuaValue = f.call(arg)?;
                    Data::from_lua(&v)
                }
                None => Ok(Data::Nil),
            })
        };
        let cmds = take(&mut lock(&self.shared).cmds);
        self.apply(cmds);
        r.map_err(|e| {
            self.handler_errors += 1;
            log::warn!(target: "anypage_page", "{} {what}: {e}", self.manifest.id);
            e
        })
    }

    /// Run a page action by name, as an agent would.
    pub fn run_action(&mut self, name: &str, params: &Data) -> Result<Data, String> {
        let shared = self.shared.clone();
        let params = params.clone();
        let exists = lock(&shared).actions.iter().any(|a| a.name == name);
        if !exists {
            return Err(format!("no action \"{name}\""));
        }
        self.call_stored(&format!("action {name}"), |lua| {
            let sh = lock(&shared);
            let a = sh.actions.iter().find(|a| a.name == name).expect("checked above");
            Ok(Some((lua.registry_value(&a.func)?, params.to_lua(lua)?)))
        })
    }

    /// A cell by the name the script gave it.
    pub fn cell_id(&self, name: &str) -> Option<anypage_core::CellId> {
        lock(&self.shared).names.get(name).map(|n| n.0)
    }

    /// The current value of a named cell (evaluating it if needed).
    pub fn cell_value(&mut self, name: &str) -> Option<Data> {
        let id = lock(&self.shared).names.get(name).map(|n| n.0)?;
        let input = self.graph.demand(id);
        Some(crate::data::input_to_data(&input))
    }

    pub fn script_memory(&self) -> usize {
        self.realm.used_memory()
    }

    pub fn summary(&self) -> String {
        format!(
            "page {} {} ({}) from {}: {} cells, {} panels, {} tiled, {} effects, {} GPU layers, loaded in {:.1} ms, script memory {:.1} MB",
            self.manifest.id,
            self.manifest.version,
            self.manifest.name,
            self.origin,
            self.graph.len(),
            self.spec.panels.len(),
            self.spec.tiled.len(),
            self.spec.effects.len(),
            self.spec.layers.len(),
            self.load_ms,
            self.script_memory() as f64 / 1048576.0
        )
    }
}

/// JSON for agents: long strings and lists are cut so a page can always be described.
fn summarize(d: &Data, depth: u32) -> Json {
    match d {
        Data::Str(s) if s.chars().count() > 400 => {
            let cut: String = s.chars().take(400).collect();
            json!({ "$text": cut, "$chars": s.chars().count() })
        }
        Data::List(l) if depth >= 3 || l.len() > 40 => {
            if depth >= 3 {
                json!({ "$list": l.len() })
            } else {
                let head: Vec<Json> = l.iter().take(40).map(|x| summarize(x, depth + 1)).collect();
                json!({ "$head": head, "$len": l.len() })
            }
        }
        Data::List(l) => Json::Array(l.iter().map(|x| summarize(x, depth + 1)).collect()),
        Data::Map(m) if depth >= 3 => json!({ "$map": m.len() }),
        Data::Map(m) => Json::Object(m.iter().map(|(k, v)| (k.to_string(), summarize(v, depth + 1))).collect()),
        other => other.to_json(),
    }
}

impl Page for ScriptPage {
    fn parts(&mut self) -> (&mut Graph, &PageSpec) {
        (&mut self.graph, &self.spec)
    }
    fn graph(&self) -> &Graph {
        &self.graph
    }
    fn spec(&self) -> &PageSpec {
        &self.spec
    }
    fn pointer_down(&mut self, wx: f32, wy: f32) -> PointerResponse {
        match self.dispatch("pointer_down", (wx, wy)) {
            Data::Str(s) if &*s == "captured" => PointerResponse::Captured,
            Data::Str(s) if &*s == "handled" => PointerResponse::Handled,
            Data::Bool(true) => PointerResponse::Handled,
            _ => PointerResponse::Ignored,
        }
    }
    fn pointer_drag(&mut self, wx: f32, wy: f32) {
        self.dispatch("pointer_drag", (wx, wy));
    }
    fn pointer_up(&mut self) {
        self.dispatch("pointer_up", ());
    }
    fn pointer_cancel(&mut self) {
        // pages that do not listen for cancel get pointer_up, as before
        let has_cancel = lock(&self.shared).handlers.get("pointer_cancel").is_some_and(|h| !h.is_empty());
        self.dispatch(if has_cancel { "pointer_cancel" } else { "pointer_up" }, ());
    }
    fn after_commit(&mut self, completed: &[anypage_core::CellId]) {
        for id in completed {
            let has = lock(&self.shared).job_done.contains_key(id);
            if !has {
                continue;
            }
            let value = crate::data::input_to_data(&self.graph.view(*id));
            let shared = self.shared.clone();
            let id = *id;
            let _ = self.call_stored("job done", |lua| {
                let sh = lock(&shared);
                match sh.job_done.get(&id) {
                    Some(k) => Ok(Some((lua.registry_value(k)?, value.to_lua(lua)?))),
                    None => Ok(None),
                }
            });
        }
        self.demand_eager();
    }
    fn pointer_move(&mut self, wx: f32, wy: f32) {
        self.dispatch("pointer_move", (wx, wy));
    }
    fn wheel(&mut self, wx: f32, wy: f32, dx: f32, dy: f32) -> bool {
        self.dispatch("wheel", (wx, wy, dx, dy)).truthy()
    }
    fn key(&mut self, key: &str) -> bool {
        self.dispatch("key", key.to_string()).truthy()
    }
    fn text_input(&mut self, text: &str) {
        self.dispatch("text", text.to_string());
    }
    fn ime_preedit(&mut self, text: &str) {
        self.dispatch("ime", text.to_string());
    }
    fn paste(&mut self, text: &str) {
        self.dispatch("paste", text.to_string());
    }
    fn file_drop(&mut self, name: &str, bytes: Arc<Vec<u8>>) {
        let shared = self.shared.clone();
        let name = name.to_string();
        let _ = self.call_stored("file_drop", |lua| {
            let t = dropped_file_table(lua, &name, bytes)?;
            let fs: Vec<Function> = match lock(&shared).handlers.get("file_drop") {
                Some(keys) => keys.iter().map(|k| lua.registry_value(k)).collect::<mlua::Result<_>>()?,
                None => return Ok(None),
            };
            for f in fs {
                let r: LuaValue = f.call(t.clone())?;
                if !matches!(r, LuaValue::Nil | LuaValue::Boolean(false)) {
                    break;
                }
            }
            Ok(None)
        });
    }
    fn host_result(&mut self, id: u64, result: HostResult) {
        let shared = self.shared.clone();
        let _ = self.call_stored("file callback", |lua| {
            let key = lock(&shared).callbacks.remove(&id);
            match key {
                Some(k) => Ok(Some((lua.registry_value(&k)?, LuaValue::Table(host_result_table(lua, &result)?)))),
                None => Ok(None),
            }
        });
    }
    fn text_focus(&self) -> Option<anypage_core::Rect> {
        lock(&self.shared).text_focus
    }
    fn viewport(&mut self, w: f32, h: f32, scale: f32) {
        let v = Data::map([
            ("w", Data::Num(w.round() as f64)),
            ("h", Data::Num(h.round() as f64)),
            ("scale", Data::Num(scale as f64)),
            ("portrait", Data::Bool(h > w)),
        ]);
        let cell = {
            let mut sh = lock(&self.shared);
            match sh.viewport {
                Some(id) if sh.mirror.get(&id) != Some(&v) => {
                    sh.mirror.insert(id, v.clone());
                    Some(id)
                }
                _ => None,
            }
        };
        if let Some(id) = cell {
            self.apply(vec![Cmd::Set(id, v)]);
            self.dispatch("viewport", (w.round(), h.round()));
        }
    }
    fn take_request(&mut self) -> Option<PageRequest> {
        self.requests.pop_front()
    }
    fn control(&mut self, name: &str, value: f64) -> bool {
        self.dispatch("control", (name.to_string(), value)).truthy()
    }
    fn describe(&self) -> String {
        let sh = lock(&self.shared);
        let cells: Vec<Json> = sh
            .names
            .iter()
            .map(|(name, (id, kind, agent))| {
                let v = self.graph.view(*id);
                let value = v.value.as_deref().map(value_to_data).unwrap_or_default();
                let mut c = json!({ "name": name, "kind": kind, "value": summarize(&value, 0) });
                if *agent {
                    c["agent_writable"] = Json::Bool(true);
                }
                c
            })
            .collect();
        let doc = json!({
            "page": { "id": self.manifest.id, "name": self.manifest.name, "version": self.manifest.version },
            "actions": action_list(&sh).to_json(),
            "cells": cells,
            "requests": [
                { "action": "<name>", "params": {} },
                { "get": "<cell name>" },
                { "set": "<agent-writable input>", "value": null },
                { "describe": true }
            ],
        });
        doc.to_string()
    }
    fn act(&mut self, request: &str) -> String {
        let req: Json = match serde_json::from_str(request) {
            Ok(j) => j,
            Err(e) => return json!({ "error": format!("bad JSON: {e}") }).to_string(),
        };
        if req.get("describe").is_some() {
            return self.describe();
        }
        if let Some(name) = req.get("action").and_then(Json::as_str) {
            let params = req.get("params").map(Data::from_json).unwrap_or_default();
            return match self.run_action(name, &params) {
                Ok(d) => json!({ "ok": true, "result": summarize(&d, 0) }).to_string(),
                Err(e) => json!({ "error": e }).to_string(),
            };
        }
        if let Some(name) = req.get("get").and_then(Json::as_str) {
            return match self.cell_value(name) {
                Some(d) => json!({ "value": summarize(&d, 0) }).to_string(),
                None => json!({ "error": format!("no cell \"{name}\"") }).to_string(),
            };
        }
        if let Some(name) = req.get("set").and_then(Json::as_str) {
            let entry = lock(&self.shared).names.get(name).copied();
            return match entry {
                Some((id, "input", true)) => {
                    let d = req.get("value").map(Data::from_json).unwrap_or_default();
                    lock(&self.shared).mirror.insert(id, d.clone());
                    self.apply(vec![Cmd::Set(id, d)]);
                    json!({ "ok": true }).to_string()
                }
                Some(_) => json!({ "error": format!("\"{name}\" is not an agent-writable input") }).to_string(),
                None => json!({ "error": format!("no cell \"{name}\"") }).to_string(),
            };
        }
        json!({ "error": "expected one of: action, get, set, describe" }).to_string()
    }
    fn shutdown(&mut self) {
        self.quit.store(true, Ordering::Relaxed);
        self.graph.cancel_all();
    }
}

impl Drop for ScriptPage {
    fn drop(&mut self) {
        self.quit.store(true, Ordering::Relaxed);
        // running jobs end now instead of holding devices and connections
        self.graph.cancel_all();
    }
}

