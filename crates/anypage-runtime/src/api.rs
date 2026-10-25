//! The globals a page script sees: `page`, the native module tables it
//! declared (`pdf`, `sheet`, ...), the built-in `text` and `json` modules,
//! `import`, and the prelude's `color` and `Draw`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Instant;

use anypage_core::{level_scale, op_inline, op_memo, val, CellId, Graph, OpClass, Rect, Status, StreamPolicy, StreamSnapshot, Value};
use anypage_engine::{
    Canvas, Class, EffectLayer, EffectParams, GpuLayer, Image, PageRequest, Panel, TextRaster, TiledLayer,
};
use mlua::chunk::ChunkMode;
use mlua::{AnyUserData, FromLuaMulti, Function, IntoLuaMulti, Lua, RegistryKey, Table, UserData, Value as LuaValue, Variadic};

use crate::data::{input_to_data, value_to_data, Data, Res, Streamed};
use crate::draw::{error_image, render, DisplayList};
use crate::natives::{guarded, Call, Effect, Grants, Natives, Sink, SourceCtx, SourceGuard, StreamSpec};
use crate::ops::{DataJob, ScriptKind, ScriptOp, PHASE_EVAL};
use crate::package::{Caps, Limits};
use crate::realm::{describe, pause_deadline, Realm};

pub(crate) const CLASSES: [(&str, Class); 5] = [
    ("static", Class::Static),
    ("pull", Class::PullStream),
    ("push", Class::PushStream),
    ("async", Class::Async),
    ("ops", Class::Operations),
];
pub(crate) const EVENTS: [&str; 13] = [
    "viewport",
    "pointer_down",
    "pointer_drag",
    "pointer_up",
    "pointer_cancel",
    "pointer_move",
    "wheel",
    "key",
    "text",
    "ime",
    "paste",
    "file_drop",
    "control",
];

thread_local! {
    /// The page's graph while a handler or action runs on this thread, for
    /// `page.peek` (handlers never change the graph directly; their commands
    /// are applied afterwards).
    static PEEK: std::cell::Cell<*const Graph> = const { std::cell::Cell::new(std::ptr::null()) };
}

/// Makes the graph readable by `page.peek` for the lifetime of the guard.
pub(crate) struct PeekGuard;

impl PeekGuard {
    pub fn new(g: &Graph) -> Self {
        PEEK.with(|p| p.set(g as *const Graph));
        PeekGuard
    }
}

impl Drop for PeekGuard {
    fn drop(&mut self) {
        PEEK.with(|p| p.set(std::ptr::null()));
    }
}
/// Modules every page gets without declaring them: pure, no outside access.
pub(crate) const BUILTIN_MODULES: [&str; 2] = ["text", "json"];

/// A cell as scripts hold it.
#[derive(Clone, Copy)]
pub(crate) struct CellRef {
    pub id: CellId,
    pub input: bool,
}

impl UserData for CellRef {}

pub(crate) enum Cmd {
    Set(CellId, Data),
    Cancel(CellId),
    Request(PageRequest),
}

pub(crate) struct SourceRec {
    pub on: Arc<AtomicBool>,
    pub enabled: Option<CellId>,
    pub request: Option<(CellId, Sender<Data>)>,
}

pub(crate) struct ActionRec {
    pub name: String,
    pub description: String,
    pub params: Data,
    pub func: RegistryKey,
}

/// Exists only while the entry script runs.
pub(crate) struct Build {
    pub graph: Graph,
    pub panels: Vec<Panel>,
    pub tiled: Vec<TiledLayer>,
    pub effects: Vec<EffectLayer>,
    pub layers: Vec<GpuLayer>,
    pub sources: Vec<SourceRec>,
    pub threads: Vec<std::thread::JoinHandle<()>>,
    pub guards: Vec<SourceGuard>,
    pub cam_level: CellId,
    pub panel_ids: HashSet<u32>,
    pub layer_ids: HashSet<u32>,
}

pub(crate) struct Shared {
    pub build: Option<Build>,
    pub cmds: Vec<Cmd>,
    /// Current values of input cells, so `page.get` sees `page.set` at once.
    pub mirror: HashMap<CellId, Data>,
    pub handlers: HashMap<String, Vec<RegistryKey>>,
    pub actions: Vec<ActionRec>,
    /// Named cells for agents: name -> (cell, kind, agent may set it).
    pub names: BTreeMap<String, (CellId, &'static str, bool)>,
    pub callbacks: HashMap<u64, RegistryKey>,
    /// `page.job(..., {done = fn})`: called with the job's value when it finishes.
    pub job_done: HashMap<CellId, RegistryKey>,
    /// `page.job(..., {eager = true})`: run whenever inputs change, read or not.
    pub eager: Vec<CellId>,
    pub imports: HashMap<String, RegistryKey>,
    /// `page.viewport()`: the input cell the window size goes into.
    pub viewport: Option<CellId>,
    pub next_id: u64,
    pub now: f64,
    pub text_focus: Option<Rect>,
}

impl Shared {
    pub fn new(build: Build) -> Self {
        Self {
            build: Some(build),
            cmds: Vec::new(),
            mirror: HashMap::new(),
            handlers: HashMap::new(),
            actions: Vec::new(),
            names: BTreeMap::new(),
            callbacks: HashMap::new(),
            job_done: HashMap::new(),
            eager: Vec::new(),
            imports: HashMap::new(),
            viewport: None,
            next_id: 1,
            now: 0.0,
            text_focus: None,
        }
    }
}

/// Fixed context the API functions need.
pub(crate) struct Env {
    pub id: String,
    pub caps: Caps,
    pub limits: Limits,
    pub natives: Natives,
    pub files: Arc<BTreeMap<String, String>>,
    pub assets: Arc<BTreeMap<String, Arc<Vec<u8>>>>,
    pub text: Arc<TextRaster>,
    pub realm: Weak<Realm>,
    pub quit: Arc<AtomicBool>,
    pub grants: Arc<Grants>,
    pub phase: Arc<AtomicU8>,
    /// The realm's call deadline; native calls stop its clock.
    pub deadline: Arc<std::sync::atomic::AtomicU64>,
}

pub(crate) fn rt(msg: impl Into<String>) -> mlua::Error {
    mlua::Error::runtime(msg.into())
}

pub(crate) fn lock(s: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    s.lock().unwrap_or_else(|p| p.into_inner())
}

/// Cell names are how agents and `describe` find cells: two cells may not share one.
fn name_cell(sh: &mut Shared, name: String, entry: (CellId, &'static str, bool)) -> mlua::Result<()> {
    if sh.names.contains_key(&name) {
        return Err(rt(format!("a cell named \"{name}\" already exists; give each cell its own name")));
    }
    sh.names.insert(name, entry);
    Ok(())
}

fn building<'a>(s: &'a mut Shared, what: &str) -> mlua::Result<&'a mut Build> {
    s.build.as_mut().ok_or_else(|| rt(format!("{what} is only available while the page loads; the graph is fixed afterwards")))
}

fn cell_of(v: &LuaValue) -> mlua::Result<CellRef> {
    match v {
        LuaValue::UserData(ud) => Ok(*ud.borrow::<CellRef>()?),
        _ => Err(rt("expected a cell (from page.input, page.derive, page.stream, ...)")),
    }
}

fn cells_of(t: Option<Table>) -> mlua::Result<Vec<CellId>> {
    let Some(t) = t else { return Ok(Vec::new()) };
    let mut v = Vec::with_capacity(t.raw_len());
    for i in 1..=t.raw_len() {
        v.push(cell_of(&t.raw_get::<LuaValue>(i)?)?.id);
    }
    Ok(v)
}

pub(crate) fn rect_of(v: LuaValue) -> mlua::Result<Rect> {
    let d = Data::from_lua(&v)?;
    let n: Vec<f64> = d.items().iter().filter_map(Data::num).collect();
    if n.len() != 4 || n[2] <= 0.0 || n[3] <= 0.0 || n.iter().any(|x| !x.is_finite() || x.abs() > 1.0e9) {
        return Err(rt("rect must be { x, y, w, h } with w, h > 0 and |values| <= 1e9"));
    }
    Ok(Rect::new(n[0] as f32, n[1] as f32, n[2] as f32, n[3] as f32))
}

/// Options table to `Data`, pulling out cell-valued and function-valued keys.
type Opts = (Data, HashMap<String, CellRef>, HashMap<String, Function>);

fn opts_of(t: Option<Table>) -> mlua::Result<Opts> {
    let mut cells = HashMap::new();
    let mut funcs = HashMap::new();
    let Some(t) = t else { return Ok((Data::Nil, cells, funcs)) };
    let mut plain = Vec::new();
    for pair in t.pairs::<String, LuaValue>() {
        let (k, v) = pair?;
        match &v {
            LuaValue::UserData(ud) if ud.borrow::<CellRef>().is_ok() => {
                cells.insert(k, cell_of(&v)?);
            }
            LuaValue::Function(f) => {
                funcs.insert(k, f.clone());
            }
            _ => plain.push((k, Data::from_lua(&v)?)),
        }
    }
    Ok((Data::map(plain.iter().map(|(k, v)| (k.as_str(), v.clone()))), cells, funcs))
}

fn class_of(name: Option<String>, default: &str) -> mlua::Result<Class> {
    let name = name.unwrap_or_else(|| default.into());
    CLASSES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, c)| *c)
        .ok_or_else(|| rt(format!("unknown panel class \"{name}\" (static, pull, push, async, ops)")))
}

/// "main.luau:12: " for the script line that called into the host.
fn caller(lua: &Lua) -> String {
    for level in 1..4 {
        let found = lua.inspect_stack(level, |d| {
            let s = d.source();
            if s.what == "C" {
                return None;
            }
            let line = d.current_line()?;
            let src = s.short_src.as_deref().unwrap_or("?").to_string();
            Some(format!("{src}:{line}: "))
        });
        match found {
            Some(Some(at)) => return at,
            Some(None) => continue,
            None => break,
        }
    }
    String::new()
}

/// A host function: errors carry the script line that called it.
pub(crate) fn func<A, R>(lua: &Lua, f: impl Fn(&Lua, A) -> mlua::Result<R> + Send + 'static) -> mlua::Result<Function>
where
    A: FromLuaMulti,
    R: IntoLuaMulti,
{
    lua.create_function(move |lua, args: A| f(lua, args).map_err(|e| rt(format!("{}{}", caller(lua), describe(&e)))))
}

/// Parse and validate a page shader before any GPU sees it, against the
/// effect contract (vertex `vs_main`, fragment `fs_main`).
pub fn check_wgsl(name: &str, src: &str) -> Result<(), String> {
    use anypage_engine::wgpu::naga;
    let first = |s: String| s.lines().filter(|l| !l.trim().is_empty()).take(4).collect::<Vec<_>>().join(" | ");
    let module = naga::front::wgsl::parse_str(src).map_err(|e| format!("{name}: {}", first(e.emit_to_string(src))))?;
    naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::empty())
        .validate(&module)
        .map_err(|e| format!("{name}: {}", first(e.emit_to_string(src))))?;
    for (entry, stage) in [("vs_main", naga::ShaderStage::Vertex), ("fs_main", naga::ShaderStage::Fragment)] {
        if !module.entry_points.iter().any(|ep| ep.name == entry && ep.stage == stage) {
            return Err(format!("{name}: needs a {stage:?} entry point named {entry}"));
        }
    }
    Ok(())
}

fn packed(c: [u8; 4]) -> f64 {
    u32::from_be_bytes(c) as f64
}

fn file_table(lua: &Lua, name: &str, bytes: Arc<Vec<u8>>) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("name", name)?;
    t.set("size", bytes.len() as f64)?;
    let res = Res::from_arc("fs.bytes", bytes);
    t.set("data", Data::Res(res).to_lua(lua)?)?;
    Ok(t)
}

pub(crate) fn host_result_table(lua: &Lua, r: &anypage_engine::HostResult) -> mlua::Result<Table> {
    use anypage_engine::HostResult;
    match r {
        HostResult::File { name, bytes } => file_table(lua, name, bytes.clone()),
        HostResult::Saved { name } => {
            let t = lua.create_table()?;
            t.set("saved", name.as_str())?;
            Ok(t)
        }
        HostResult::Cancelled => {
            let t = lua.create_table()?;
            t.set("cancelled", true)?;
            Ok(t)
        }
        HostResult::Failed(e) => {
            let t = lua.create_table()?;
            t.set("error", e.as_str())?;
            Ok(t)
        }
    }
}

pub(crate) fn dropped_file_table(lua: &Lua, name: &str, bytes: Arc<Vec<u8>>) -> mlua::Result<Table> {
    file_table(lua, name, bytes)
}

fn blank() -> Value {
    val(Image { w: 1, h: 1, rgba: vec![0; 4], newest: None })
}

/// The image inside a value: a `Res` holding an `Image`, or a stream value
/// whose `last` item is one (video frames).
fn image_value(d: &Data) -> Option<Value> {
    let r = match d {
        Data::Res(r) => r,
        Data::Map(_) => d.get("last").as_res()?,
        _ => return None,
    };
    r.get::<Image>()?;
    Some(r.arc().clone())
}

// ---------------------------------------------------------------- install

pub(crate) fn install(lua: &Lua, shared: &Arc<Mutex<Shared>>, env: &Arc<Env>) -> mlua::Result<()> {
    let page = lua.create_table()?;

    let classes = lua.create_table()?;
    for (name, class) in CLASSES {
        let t = lua.create_table()?;
        t.set("label", class.label())?;
        t.set("color", packed(class.color()))?;
        classes.set(name, t)?;
    }
    lua.globals().set("classes", classes)?;

    let info = lua.create_table()?;
    info.set("id", env.id.as_str())?;
    info.set("platform", std::env::consts::OS)?;
    page.set("info", info)?;

    install_graph(lua, &page, shared, env)?;
    install_layers(lua, &page, shared, env)?;
    install_handlers(lua, &page, shared, env)?;
    install_actions(lua, &page, shared)?;
    install_modules(lua, shared, env)?;

    lua.globals().set("page", page)?;
    Ok(())
}

fn install_graph(lua: &Lua, page: &Table, shared: &Arc<Mutex<Shared>>, env: &Arc<Env>) -> mlua::Result<()> {
    // page.viewport(): an input cell { w, h, scale, portrait } with the window
    // size in logical pixels; the "viewport" event fires when it changes
    let s = shared.clone();
    page.set(
        "viewport",
        func(lua, move |lua, ()| {
            let mut sh = lock(&s);
            if let Some(id) = sh.viewport {
                return lua.create_userdata(CellRef { id, input: true });
            }
            let initial = Data::map([("w", Data::Num(1280.0)), ("h", Data::Num(800.0)), ("scale", Data::Num(1.0)), ("portrait", Data::Bool(false))]);
            let id = building(&mut sh, "page.viewport")?.graph.input("viewport", val(initial.clone()));
            sh.mirror.insert(id, initial);
            sh.viewport = Some(id);
            name_cell(&mut sh, "viewport".into(), (id, "input", false))?;
            lua.create_userdata(CellRef { id, input: true })
        })?,
    )?;

    let s = shared.clone();
    page.set(
        "clock",
        func(lua, move |lua, ()| {
            let mut sh = lock(&s);
            let b = building(&mut sh, "page.clock")?;
            let id = match b.graph.clock_id() {
                Some(id) => id,
                None => b.graph.clock("t"),
            };
            lua.create_userdata(CellRef { id, input: false })
        })?,
    )?;

    // page.input(name, initial[, {agent = true}])
    let s = shared.clone();
    page.set(
        "input",
        func(lua, move |lua, (name, initial, opts): (String, LuaValue, Option<Table>)| {
            let data = Data::from_lua(&initial)?;
            let agent = match &opts {
                Some(t) => t.get::<Option<bool>>("agent")?.unwrap_or(false),
                None => false,
            };
            let mut sh = lock(&s);
            let id = building(&mut sh, "page.input")?.graph.input(&name, val(data.clone()));
            sh.mirror.insert(id, data);
            name_cell(&mut sh, name, (id, "input", agent))?;
            lua.create_userdata(CellRef { id, input: true })
        })?,
    )?;

    let (s, e) = (shared.clone(), env.clone());
    page.set(
        "derive",
        func(lua, move |lua, (name, inputs, f): (String, Option<Table>, Function)| {
            let ids = cells_of(inputs)?;
            let realm = e.realm.upgrade().ok_or_else(|| rt("page is shutting down"))?;
            let op = Arc::new(ScriptOp {
                name: name.clone(),
                realm,
                func: lua.create_registry_value(f)?,
                kind: ScriptKind::Derive,
                phase: e.phase.clone(),
                last_error: Mutex::new(None),
            });
            let mut sh = lock(&s);
            let id = building(&mut sh, "page.derive")?.graph.derive(&name, op, &ids);
            name_cell(&mut sh, name, (id, "derived", false))?;
            lua.create_userdata(CellRef { id, input: false })
        })?,
    )?;

    // page.stream(name, source, {enabled = cell, request = cell, cap = n, latest = bool, ...})
    let (s, e) = (shared.clone(), env.clone());
    page.set(
        "stream",
        func(lua, move |lua, (name, source, opts): (String, String, Option<Table>)| {
            if !e.caps.allows(&source) {
                return Err(rt(format!("stream source \"{source}\" is not declared in page.json capabilities (modules or streams)")));
            }
            let (data, cells, _) = opts_of(opts)?;
            let enabled = cells.get("enabled").map(|c| c.id);
            let on = Arc::new(AtomicBool::new(true));
            let mut sh = lock(&s);
            let mirror_req = cells.get("request").map(|c| (c.id, sh.mirror.get(&c.id).cloned().unwrap_or_default()));
            let b = building(&mut sh, "page.stream")?;
            let cell = if let Some(f) = e.natives.sources.get(&source) {
                let (def_latest, def_cap) = e.natives.source_defaults.get(&source).copied().unwrap_or((false, 256));
                let cap = data.get("cap").num_or(def_cap as f64).clamp(1.0, 100_000.0) as usize;
                let latest = match data.get("latest") {
                    Data::Nil => def_latest,
                    d => d.truthy(),
                };
                let policy = if latest { StreamPolicy::LatestWins } else { StreamPolicy::Append };
                let (raw, producer) = b.graph.stream::<(Data, Instant)>(&name, cap, 8192, policy);
                let (tx, rx) = channel();
                let mut request = None;
                if let Some((id, initial)) = mirror_req {
                    let _ = tx.send(initial);
                    request = Some((id, tx));
                }
                let has_request = request.is_some();
                let ctx = SourceCtx::new(name.clone(), data.clone(), Sink(producer), e.grants.clone(), rx, on.clone(), e.quit.clone(), has_request);
                let guard = guarded(&source, || f(ctx)).map_err(rt)?;
                b.guards.push(guard);
                b.sources.push(SourceRec { on, enabled, request });
                let items = op_inline(&format!("{name}.items"), OpClass::Native, |i, _| {
                    let Some(snap) = i[0].get::<StreamSnapshot<(Data, Instant)>>().filter(|s| !s.items.is_empty()) else {
                        return Ok(val(Data::Nil));
                    };
                    let items: Vec<Data> = snap.items.iter().map(|x| x.0.clone()).collect();
                    let last = items.last().cloned().unwrap_or_default();
                    let data = Data::map([("items", Data::list(items)), ("total", Data::Num(snap.total as f64)), ("last", last)]);
                    Ok(val(Streamed { data, newest: snap.items.last().map(|x| x.1) }))
                });
                b.graph.derive(&format!("{name}.items"), items, &[raw])
            } else if let Some(f) = e.natives.streams.get(&source) {
                let h = guarded(&source, || f(&mut b.graph, StreamSpec { label: &name, opts: &data, on: on.clone(), quit: e.quit.clone() })).map_err(rt)?;
                b.threads.extend(h.threads);
                b.sources.push(SourceRec { on, enabled, request: None });
                h.cell
            } else {
                return Err(rt(format!("this app has no stream source \"{source}\"")));
            };
            name_cell(&mut sh, name, (cell, "stream", false))?;
            lua.create_userdata(CellRef { id: cell, input: false })
        })?,
    )?;

    // page.job(name, source, {inputs}, opts)
    let (s, e) = (shared.clone(), env.clone());
    page.set(
        "job",
        func(lua, move |lua, (name, source, inputs, opts): (String, String, Option<Table>, Option<Table>)| {
            if !e.caps.allows(&source) {
                return Err(rt(format!("job \"{source}\" is not declared in page.json capabilities (modules or jobs)")));
            }
            let ids = cells_of(inputs)?;
            let (data, _, funcs) = opts_of(opts)?;
            let eager = data.get("eager").truthy();
            let op: Arc<dyn anypage_core::Op> = if let Some(f) = e.natives.jobs_data.get(&source) {
                Arc::new(DataJob { name: name.clone(), f: f.clone(), opts: data, grants: e.grants.clone() })
            } else if let Some(f) = e.natives.jobs.get(&source) {
                guarded(&source, || f(&data)).map_err(rt)?
            } else {
                return Err(rt(format!("this app has no job \"{source}\"")));
            };
            let done = match funcs.get("done") {
                Some(f) => Some(lua.create_registry_value(f.clone())?),
                None => None,
            };
            let mut sh = lock(&s);
            let id = building(&mut sh, "page.job")?.graph.derive(&name, op, &ids);
            name_cell(&mut sh, name, (id, "job", false))?;
            if let Some(k) = done {
                sh.job_done.insert(id, k);
            }
            if eager {
                sh.eager.push(id);
            }
            lua.create_userdata(CellRef { id, input: false })
        })?,
    )?;
    Ok(())
}

fn install_layers(lua: &Lua, page: &Table, shared: &Arc<Mutex<Shared>>, env: &Arc<Env>) -> mlua::Result<()> {
    let (s, e) = (shared.clone(), env.clone());
    page.set(
        "panel",
        func(lua, move |lua, spec: Table| {
            let rect = rect_of(spec.get("rect")?)?;
            let class = class_of(spec.get("class")?, "static")?;
            let priority = spec.get::<Option<f64>>("priority")?.unwrap_or(4.0).clamp(0.0, 255.0) as u8;
            let over = spec.get::<Option<bool>>("over")?.unwrap_or(false);
            let ids = cells_of(spec.get("inputs")?)?;
            let draw: Function = spec.get::<Option<Function>>("draw")?.ok_or_else(|| rt("panel needs a draw function"))?;
            let want_id = spec.get::<Option<f64>>("id")?;
            let name: Option<String> = spec.get("name")?;
            let realm = e.realm.upgrade().ok_or_else(|| rt("page is shutting down"))?;
            let func = lua.create_registry_value(draw)?;

            let mut sh = lock(&s);
            let b = building(&mut sh, "page.panel")?;
            let id = next_id(&mut b.panel_ids, want_id)?;
            let label = name.unwrap_or_else(|| format!("panel.{id}"));
            let op = Arc::new(ScriptOp {
                name: format!("{label}.draw"),
                realm,
                func,
                kind: ScriptKind::Draw { w: rect.w, h: rect.h },
                phase: e.phase.clone(),
                last_error: Mutex::new(None),
            });
            let script = b.graph.derive(&format!("{label}.draw"), op, &ids);
            let tr = e.text.clone();
            let raster = op_inline(&format!("{label}.raster"), OpClass::Native, move |i, _| {
                let scale = level_scale(*i[1].get::<i32>().unwrap_or(&0)).min(8.0);
                let img = match (&i[0].status, i[0].get::<DisplayList>()) {
                    (Status::Error(msg), _) => error_image(rect.w, rect.h, scale, &tr, msg),
                    (_, Some(list)) => render(list, rect.w, rect.h, scale, &tr),
                    _ => Canvas::new(rect.w, rect.h, scale).into_image(None),
                };
                Ok(val(img))
            });
            let cam = b.cam_level;
            let cell = b.graph.derive(&format!("{label}.raster"), raster, &[script, cam]);
            b.panels.push(Panel { id, rect, cell, class, priority, over_effects: over });
            Ok(())
        })?,
    )?;

    // page.image{rect, source = cell}: an image value shown as is (video
    // frames, rendered pages, camera); no rasterising on the CPU
    let (s, e) = (shared.clone(), env.clone());
    page.set(
        "image",
        func(lua, move |_, spec: Table| {
            let rect = rect_of(spec.get("rect")?)?;
            let class = class_of(spec.get("class")?, "pull")?;
            let priority = spec.get::<Option<f64>>("priority")?.unwrap_or(2.0).clamp(0.0, 255.0) as u8;
            let over = spec.get::<Option<bool>>("over")?.unwrap_or(false);
            let source = cell_of(&spec.get("source")?)?.id;
            let want_id = spec.get::<Option<f64>>("id")?;
            let name: Option<String> = spec.get("name")?;
            let tr = e.text.clone();
            let mut sh = lock(&s);
            let b = building(&mut sh, "page.image")?;
            let id = next_id(&mut b.panel_ids, want_id)?;
            let label = name.unwrap_or_else(|| format!("image.{id}"));
            let adapter = op_inline(&format!("{label}.image"), OpClass::Native, move |i, _| {
                Ok(match &i[0].status {
                    Status::Error(msg) => val(error_image(rect.w, rect.h, 1.0, &tr, msg)),
                    _ => i[0].value.as_deref().map(value_to_data).as_ref().and_then(image_value).unwrap_or_else(blank),
                })
            });
            let cell = b.graph.derive(&format!("{label}.image"), adapter, &[source]);
            b.panels.push(Panel { id, rect, cell, class, priority, over_effects: over });
            Ok(())
        })?,
    )?;

    // page.layer{rect, kind = "3d.scene", inputs = {...}}: native GPU drawing
    let (s, e) = (shared.clone(), env.clone());
    page.set(
        "layer",
        func(lua, move |_, spec: Table| {
            let rect = rect_of(spec.get("rect")?)?;
            let kind: String = spec.get("kind")?;
            if !e.caps.allows(&kind) {
                return Err(rt(format!("layer \"{kind}\" is not declared in page.json capabilities.modules")));
            }
            let lk = e.natives.layers.get(&kind).ok_or_else(|| rt(format!("this app has no GPU layer \"{kind}\"")))?.clone();
            let ids = cells_of(spec.get("inputs")?)?;
            let want_id = spec.get::<Option<f64>>("id")?;
            let name: Option<String> = spec.get("name")?;
            let mut sh = lock(&s);
            let b = building(&mut sh, "page.layer")?;
            let id = next_id(&mut b.layer_ids, want_id)?;
            let label = name.unwrap_or_else(|| format!("layer.{id}"));
            let convert = lk.convert.clone();
            let adapter = op_inline(&format!("{label}.value"), OpClass::Gpu, move |i, _| {
                let data: Vec<Data> = i.iter().map(input_to_data).collect();
                guarded("layer", || convert(&data))
            });
            let cell = b.graph.derive(&format!("{label}.value"), adapter, &ids);
            b.layers.push(GpuLayer { id, rect, cell, renderer: lk.renderer.clone() });
            Ok(())
        })?,
    )?;

    let (s, e) = (shared.clone(), env.clone());
    page.set(
        "tiled",
        func(lua, move |_, spec: Table| {
            let rect = rect_of(spec.get("rect")?)?;
            let source: String = spec.get("source")?;
            if !e.caps.allows(&source) {
                return Err(rt(format!("tile source \"{source}\" is not declared in page.json capabilities (modules or tiles)")));
            }
            let f = e.natives.tiles.get(&source).ok_or_else(|| rt(format!("this app has no tile source \"{source}\"")))?;
            let params = cell_of(&spec.get("params")?)?.id;
            let opts = Data::from_lua(&spec.get("options")?)?;
            let src = guarded(&source, || f(rect, &opts)).map_err(rt)?;
            let mut sh = lock(&s);
            building(&mut sh, "page.tiled")?.tiled.push(TiledLayer { rect, params, source: src });
            Ok(())
        })?,
    )?;

    let (s, e) = (shared.clone(), env.clone());
    page.set(
        "effect",
        func(lua, move |_, spec: Table| {
            if !e.caps.gpu {
                return Err(rt("GPU operators need \"gpu\": true in page.json capabilities"));
            }
            let rect = rect_of(spec.get("rect")?)?;
            let shader: String = spec.get("shader")?;
            if !shader.ends_with(".wgsl") {
                return Err(rt("shader must be a .wgsl file in the package"));
            }
            let wgsl = e.files.get(&shader).ok_or_else(|| rt(format!("{shader} is not in the package")))?.clone();
            check_wgsl(&shader, &wgsl).map_err(rt)?;
            let params = cell_of(&spec.get("params")?)?.id;
            let mut sh = lock(&s);
            let b = building(&mut sh, "page.effect")?;
            let to_gpu = op_memo(&format!("{shader}.params"), OpClass::Gpu, |i, _| {
                let mut p = [0f32; 8];
                if let Some(d) = i[0].get::<Data>() {
                    for (k, x) in d.items().iter().take(8).enumerate() {
                        p[k] = x.num_or(0.0) as f32;
                    }
                }
                Ok(EffectParams(p))
            });
            let cell = b.graph.derive(&format!("{shader}.params"), to_gpu, &[params]);
            b.effects.push(EffectLayer { rect, wgsl, params: cell });
            Ok(())
        })?,
    )?;
    Ok(())
}

fn next_id(used: &mut HashSet<u32>, want: Option<f64>) -> mlua::Result<u32> {
    let id = match want {
        Some(n) if n >= 0.0 && n < u32::MAX as f64 => n as u32,
        Some(_) => return Err(rt("id must be a non-negative number")),
        None => (0..).find(|k| !used.contains(k)).unwrap_or(0),
    };
    if !used.insert(id) {
        return Err(rt(format!("id {id} is used twice")));
    }
    Ok(id)
}

fn install_handlers(lua: &Lua, page: &Table, shared: &Arc<Mutex<Shared>>, env: &Arc<Env>) -> mlua::Result<()> {
    // page.on(event, fn): several handlers per event, called in order until
    // one returns a true value
    let s = shared.clone();
    page.set(
        "on",
        func(lua, move |lua, (event, f): (String, Function)| {
            if !EVENTS.contains(&event.as_str()) {
                return Err(rt(format!("unknown event \"{event}\" ({})", EVENTS.join(", "))));
            }
            let key = lua.create_registry_value(f)?;
            lock(&s).handlers.entry(event).or_default().push(key);
            Ok(())
        })?,
    )?;

    let s = shared.clone();
    page.set(
        "set",
        func(lua, move |_, (cell, v): (AnyUserData, LuaValue)| {
            let c = *cell.borrow::<CellRef>()?;
            if !c.input {
                return Err(rt("page.set needs an input cell (from page.input)"));
            }
            let data = Data::from_lua(&v)?;
            let mut sh = lock(&s);
            if sh.mirror.get(&c.id) != Some(&data) {
                sh.mirror.insert(c.id, data.clone());
                sh.cmds.push(Cmd::Set(c.id, data));
            }
            Ok(())
        })?,
    )?;

    let s = shared.clone();
    page.set(
        "get",
        func(lua, move |lua, cell: AnyUserData| {
            let c = *cell.borrow::<CellRef>()?;
            if !c.input {
                return Err(rt("page.get reads input cells; pass other cells to a function as inputs"));
            }
            let d = lock(&s).mirror.get(&c.id).cloned().unwrap_or_default();
            d.to_lua(lua)
        })?,
    )?;

    let s = shared.clone();
    page.set(
        "cancel",
        func(lua, move |_, cell: AnyUserData| {
            let c = *cell.borrow::<CellRef>()?;
            lock(&s).cmds.push(Cmd::Cancel(c.id));
            Ok(())
        })?,
    )?;

    let s = shared.clone();
    page.set(
        "request",
        func(lua, move |_, what: String| {
            let r = match what.as_str() {
                "fit" => PageRequest::FitPage,
                _ => return Err(rt(format!("unknown request \"{what}\" (fit)"))),
            };
            lock(&s).cmds.push(Cmd::Request(r));
            Ok(())
        })?,
    )?;

    // page.peek(cell): the last computed value of any cell, from a handler or
    // at load. Inside a cell's function, pass the cell as an input instead.
    let s = shared.clone();
    page.set(
        "peek",
        func(lua, move |lua, cell: AnyUserData| {
            let c = *cell.borrow::<CellRef>()?;
            let ptr = PEEK.with(|p| p.get());
            if !ptr.is_null() {
                // SAFETY: set by a PeekGuard for the duration of a handler on
                // this thread, while the graph is borrowed and not mutated.
                let g = unsafe { &*ptr };
                return input_to_data(&g.view(c.id)).to_lua(lua);
            }
            let sh = lock(&s);
            match &sh.build {
                Some(b) => input_to_data(&b.graph.view(c.id)).to_lua(lua),
                None => Err(rt("page.peek works in event handlers, actions and at load; inside a cell's function pass the cell as an input")),
            }
        })?,
    )?;

    // page.asset(name): a binary file shipped in the package's assets folder
    let e = env.clone();
    page.set(
        "asset",
        func(lua, move |lua, name: String| match e.assets.get(&name) {
            Some(bytes) => Data::Res(Res::from_arc("fs.bytes", bytes.clone())).to_lua(lua),
            None => Err(rt(format!("no asset \"{name}\" in the package's assets folder"))),
        })?,
    )?;

    // page.navigate(name): open another page package this app ships
    let s = shared.clone();
    page.set(
        "navigate",
        func(lua, move |_, name: String| {
            if name.is_empty() || name.len() > 64 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                return Err(rt("page.navigate takes a package folder name (letters, digits, '-', '_')"));
            }
            lock(&s).cmds.push(Cmd::Request(PageRequest::Navigate(name)));
            Ok(())
        })?,
    )?;

    let s = shared.clone();
    page.set(
        "fit_rect",
        func(lua, move |_, r: LuaValue| {
            let rect = rect_of(r)?;
            lock(&s).cmds.push(Cmd::Request(PageRequest::FitRect(rect)));
            Ok(())
        })?,
    )?;

    let s = shared.clone();
    page.set("now", func(lua, move |_, ()| Ok(lock(&s).now))?)?;

    let e = env.clone();
    page.set(
        "measure",
        func(lua, move |_, (text, size): (String, f64)| {
            let t0 = Instant::now();
            let w = e.text.measure(&text, size.clamp(1.0, 400.0) as f32) as f64;
            pause_deadline(&e.deadline, t0.elapsed());
            Ok(w)
        })?,
    )?;

    // page.focus_text(rect | nil): the caret's rectangle while editing text
    let s = shared.clone();
    page.set(
        "focus_text",
        func(lua, move |_, r: LuaValue| {
            let focus = if matches!(r, LuaValue::Nil | LuaValue::Boolean(false)) { None } else { Some(rect_of(r)?) };
            lock(&s).text_focus = focus;
            Ok(())
        })?,
    )?;

    let (s, e) = (shared.clone(), env.clone());
    page.set(
        "copy",
        func(lua, move |_, text: String| {
            if !e.caps.clipboard {
                return Err(rt("the clipboard needs \"clipboard\": true in page.json capabilities"));
            }
            lock(&s).cmds.push(Cmd::Request(PageRequest::Copy(text)));
            Ok(())
        })?,
    )?;

    let (s, e) = (shared.clone(), env.clone());
    page.set(
        "paste",
        func(lua, move |_, ()| {
            if !e.caps.clipboard {
                return Err(rt("the clipboard needs \"clipboard\": true in page.json capabilities"));
            }
            lock(&s).cmds.push(Cmd::Request(PageRequest::Paste));
            Ok(())
        })?,
    )?;

    // page.open_file{filters = {"pdf"}, done = function(file) end}
    let (s, e) = (shared.clone(), env.clone());
    page.set(
        "open_file",
        func(lua, move |lua, spec: Table| {
            if !e.caps.files {
                return Err(rt("file dialogs need \"files\": true in page.json capabilities"));
            }
            let filters: Vec<String> = Data::from_lua(&spec.get("filters")?)?.items().iter().filter_map(|d| d.as_str().map(str::to_string)).collect();
            let done: Function = spec.get::<Option<Function>>("done")?.ok_or_else(|| rt("open_file needs a done function"))?;
            let key = lua.create_registry_value(done)?;
            let mut sh = lock(&s);
            let id = sh.next_id;
            sh.next_id += 1;
            sh.callbacks.insert(id, key);
            sh.cmds.push(Cmd::Request(PageRequest::OpenFile { id, filters }));
            Ok(id as f64)
        })?,
    )?;

    // page.save_file{name = "x.pdf", data = bytes | string | resource, done = fn}
    let (s, e) = (shared.clone(), env.clone());
    page.set(
        "save_file",
        func(lua, move |lua, spec: Table| {
            if !e.caps.files {
                return Err(rt("file dialogs need \"files\": true in page.json capabilities"));
            }
            let name: String = spec.get("name")?;
            let data = Data::from_lua(&spec.get("data")?)?;
            let bytes: Arc<Vec<u8>> = match &data {
                Data::Bytes(b) => b.clone(),
                Data::Res(r) => r.downcast_arc::<Vec<u8>>().ok_or_else(|| rt("data must be bytes, a string or a byte resource"))?,
                other => Arc::new(other.as_bytes().ok_or_else(|| rt("data must be bytes, a string or a byte resource"))?.to_vec()),
            };
            let mut sh = lock(&s);
            let id = sh.next_id;
            sh.next_id += 1;
            if let Some(done) = spec.get::<Option<Function>>("done")? {
                sh.callbacks.insert(id, lua.create_registry_value(done)?);
            }
            sh.cmds.push(Cmd::Request(PageRequest::SaveFile { id, name, bytes }));
            Ok(id as f64)
        })?,
    )?;

    let e = env.clone();
    page.set(
        "log",
        func(lua, move |_, args: Variadic<LuaValue>| {
            let parts: Vec<String> = args
                .iter()
                .map(|v| match v {
                    LuaValue::String(s) => s.to_string_lossy(),
                    LuaValue::Number(n) => n.to_string(),
                    LuaValue::Integer(i) => i.to_string(),
                    LuaValue::Boolean(b) => b.to_string(),
                    LuaValue::Nil => "nil".into(),
                    other => other.type_name().to_string(),
                })
                .collect();
            log::info!(target: "anypage_page", "{}: {}", e.id, parts.join(" "));
            Ok(())
        })?,
    )?;
    Ok(())
}

/// The semantic port inside the page: actions agents (and the page itself) can run.
fn install_actions(lua: &Lua, page: &Table, shared: &Arc<Mutex<Shared>>) -> mlua::Result<()> {
    // page.action{name, description, params = {...}, run = function(params) ... end}
    let s = shared.clone();
    page.set(
        "action",
        func(lua, move |lua, spec: Table| {
            let name: String = spec.get("name")?;
            let description: String = spec.get::<Option<String>>("description")?.unwrap_or_default();
            let params = Data::from_lua(&spec.get("params")?)?;
            let run: Function = spec.get::<Option<Function>>("run")?.ok_or_else(|| rt("action needs a run function"))?;
            let func = lua.create_registry_value(run)?;
            let mut sh = lock(&s);
            sh.actions.retain(|a| a.name != name);
            sh.actions.push(ActionRec { name, description, params, func });
            Ok(())
        })?,
    )?;

    let s = shared.clone();
    page.set(
        "actions",
        func(lua, move |lua, ()| {
            let list = action_list(&lock(&s));
            list.to_lua(lua)
        })?,
    )?;

    let s = shared.clone();
    page.set(
        "act",
        func(lua, move |lua, (name, params): (String, LuaValue)| {
            let f: Function = {
                let sh = lock(&s);
                let a = sh.actions.iter().find(|a| a.name == name).ok_or_else(|| rt(format!("no action \"{name}\"")))?;
                lua.registry_value(&a.func)?
            };
            f.call::<LuaValue>(params)
        })?,
    )?;
    Ok(())
}

pub(crate) fn action_list(sh: &Shared) -> Data {
    Data::list(
        sh.actions
            .iter()
            .map(|a| Data::map([("name", Data::str(&a.name)), ("description", Data::str(&a.description)), ("params", a.params.clone())]))
            .collect(),
    )
}

/// Native module tables, built-in `text`/`json`, and `import`.
fn install_modules(lua: &Lua, shared: &Arc<Mutex<Shared>>, env: &Arc<Env>) -> mlua::Result<()> {
    let mut tables: HashMap<String, Table> = HashMap::new();
    let mut names: Vec<&String> = env.natives.functions.keys().collect();
    names.sort();
    for name in names {
        let (effect, f) = env.natives.functions[name].clone();
        let Some((module, short)) = name.split_once('.') else { continue };
        if !BUILTIN_MODULES.contains(&module) && !env.caps.allows(name) {
            continue;
        }
        let t = match tables.get(module) {
            Some(t) => t.clone(),
            None => {
                let t = lua.create_table()?;
                tables.insert(module.to_string(), t.clone());
                t
            }
        };
        let e = env.clone();
        let full = name.clone();
        t.set(
            short,
            func(lua, move |lua, args: Variadic<LuaValue>| {
                if effect == Effect::Effect && e.phase.load(Ordering::Relaxed) == PHASE_EVAL {
                    return Err(rt(format!("{full} changes state; call it from an event handler, not inside a cell's function")));
                }
                let data: Vec<Data> = args.iter().map(Data::from_lua).collect::<mlua::Result<_>>()?;
                let t0 = Instant::now();
                let out = guarded(&full, || f(&Call { args: &data, grants: &e.grants }));
                pause_deadline(&e.deadline, t0.elapsed());
                out.map_err(|m| rt(format!("{full}: {m}")))?.to_lua(lua)
            })?,
        )?;
    }
    // modules this app has but the page did not declare: a clear error on use
    for module in env.natives.modules.iter() {
        if tables.contains_key(module) || BUILTIN_MODULES.contains(&module.as_str()) {
            continue;
        }
        let stub = lua.create_table()?;
        let meta = lua.create_table()?;
        let m = module.clone();
        meta.set(
            "__index",
            lua.create_function(move |_, (_t, key): (LuaValue, LuaValue)| -> mlua::Result<LuaValue> {
                let key = match key {
                    LuaValue::String(s) => s.to_string_lossy(),
                    _ => "?".into(),
                };
                Err(rt(format!("{m}.{key}: the \"{m}\" module is not declared in page.json capabilities.modules")))
            })?,
        )?;
        stub.set_metatable(Some(meta))?;
        tables.insert(module.clone(), stub);
    }
    for (module, t) in tables {
        lua.globals().set(module, t)?;
    }

    // import(name): a Luau library from the app ("ui") or a file in the package
    let (s, e) = (shared.clone(), env.clone());
    lua.globals().set(
        "import",
        func(lua, move |lua, name: String| {
            if let Some(k) = lock(&s).imports.get(&name) {
                return lua.registry_value::<LuaValue>(k);
            }
            let (chunk, src) = if let Some(src) = e.natives.lua_modules.get(&name) {
                (format!("@{name}.luau"), src.to_string())
            } else {
                let file = if name.ends_with(".luau") { name.clone() } else { format!("{name}.luau") };
                let src = e.files.get(&file).ok_or_else(|| rt(format!("import: no library or package file \"{name}\"")))?;
                (format!("@{file}"), src.clone())
            };
            let v: LuaValue = lua.load(src).set_name(chunk).set_mode(ChunkMode::Text).eval()?;
            let key = lua.create_registry_value(v.clone())?;
            lock(&s).imports.insert(name, key);
            Ok(v)
        })?,
    )?;
    Ok(())
}

// ---------------------------------------------------------------- page building

/// Everything the entry script created, ready to become a [`crate::ScriptPage`].
pub(crate) fn empty_build(graph: Graph, cam_level: CellId) -> Build {
    Build {
        graph,
        panels: Vec::new(),
        tiled: Vec::new(),
        effects: Vec::new(),
        layers: Vec::new(),
        sources: Vec::new(),
        threads: Vec::new(),
        guards: Vec::new(),
        cam_level,
        panel_ids: HashSet::new(),
        layer_ids: HashSet::new(),
    }
}
