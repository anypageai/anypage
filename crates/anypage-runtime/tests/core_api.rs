//! The runtime's module and page APIs, end to end without a GPU.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anypage_core::{CellId, Status};
use anypage_engine::{GpuRenderer, GpuTarget, HostResult, Image, Page, PageRequest, PointerResponse, TextRaster, Wake};
use anypage_runtime::{Data, Effect, Natives, Package, Res, ScriptPage};

fn text() -> Arc<TextRaster> {
    Arc::new(TextRaster::load().expect("a system font"))
}

fn wake() -> Wake {
    Arc::new(|| {})
}

fn manifest(caps: &str) -> String {
    format!(r#"{{ "format": 1, "id": "test", "bounds": [0, 0, 400, 300], "capabilities": {caps} }}"#)
}

fn load(caps: &str, src: &str, natives: &Natives) -> Result<ScriptPage, String> {
    let m = manifest(caps);
    let pkg = Package::from_files("test", [("page.json", m.as_str()), ("main.luau", src)])?;
    ScriptPage::load(&pkg, natives, text(), wake())
}

fn load_files(caps: &str, files: &[(&str, &str)], natives: &Natives) -> Result<ScriptPage, String> {
    let m = manifest(caps);
    let mut all = vec![("page.json".to_string(), m)];
    all.extend(files.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let pkg = Package::from_files("test", all)?;
    ScriptPage::load(&pkg, natives, text(), wake())
}

const PANEL: &str = "page.panel({ rect = { 0, 0, 10, 10 }, draw = function(d) end })\n";

fn sample_natives() -> Natives {
    let mut n = Natives::new();
    n.function("calc.add", Effect::Pure, |c| Ok(Data::Num(c.arg(0).num_or(0.0) + c.arg(1).num_or(0.0))));
    n.function("calc.poke", Effect::Effect, |_| Ok(Data::Bool(true)));
    n.function("calc.thing", Effect::Pure, |_| Ok(Data::res("calc.thing", 7u32)));
    n.function("calc.thing_value", Effect::Pure, |c| Ok(Data::Num(*c.arg(0).as_res().and_then(|r| r.get::<u32>()).ok_or("not a thing")? as f64)));
    n.job_fn("calc.slow_square", |j| {
        let x = j.input(0).num_or(0.0);
        for k in 0..10 {
            if j.cancelled() {
                return Err("cancelled".into());
            }
            j.progress(k as f32 / 10.0);
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(Data::Num(x * x))
    });
    // echoes every request it gets, with a running count
    n.source("calc.echo", |ctx| {
        let h = std::thread::spawn(move || {
            let mut k = 0.0;
            while let Ok(r) = ctx.requests.recv_timeout(Duration::from_millis(50)).or_else(|e| if ctx.quit() { Err(e) } else { Ok(Data::Nil) }) {
                if r.is_nil() {
                    continue;
                }
                k += 1.0;
                ctx.sink.push(Data::map([("echo", r), ("n", Data::Num(k))]));
            }
        });
        Ok(Box::new(h))
    });
    n.function("calc.image", Effect::Pure, |_| Ok(Data::res("calc.image", Image { w: 2, h: 1, rgba: vec![255, 0, 0, 255, 0, 0, 255, 255], newest: None })));
    n.lua_module("mathx", "local M = {} function M.twice(x) return 2 * x end return M");
    n
}

fn wait_for(p: &mut ScriptPage, cell: CellId, mut ok: impl FnMut(&Data) -> bool) -> Data {
    let t0 = Instant::now();
    loop {
        let g = p.parts().0;
        g.commit();
        let i = g.demand(cell);
        let d = anypage_runtime::data::input_to_data(&i);
        if matches!(i.status, Status::Ready) && ok(&d) {
            return d;
        }
        assert!(t0.elapsed() < Duration::from_secs(5), "timed out; last {d:?} {:?}", i.status);
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn module_functions_resources_and_imports() {
    let src = format!(
        r#"{PANEL}
        local mathx = import("mathx")
        local helper = import("helper")
        local t = calc.thing()
        assert(calc.add(2, 3) == 5, "add")
        assert(t:thing_value() == 7, "method call through the resource")
        assert(t.kind == "calc.thing", "kind")
        assert(mathx.twice(21) == 42, "app library")
        assert(helper.name == "helper", "package file")
        assert(text.next("e\u{{301}}x", 0) == 3, "grapheme")
        assert(json.decode(json.encode({{a = {{1, 2}}}})).a[2] == 2, "json")
        Out = page.derive("out", {{}}, function() return calc.add(1, 1) end)
        "#
    );
    let mut p = load_files(r#"{"modules": ["calc"]}"#, &[("main.luau", &src), ("helper.luau", "return { name = 'helper' }")], &sample_natives()).unwrap();
    let out = p.cell_value("out").unwrap();
    assert_eq!(out, Data::Num(2.0));
}

#[test]
fn undeclared_and_missing_modules() {
    let e = load(r#"{}"#, &format!("{PANEL} calc.add(1, 2)"), &sample_natives()).err().unwrap();
    assert!(e.contains("calc"), "{e}");
    let e = load(r#"{"modules": ["usb"]}"#, PANEL, &sample_natives()).err().unwrap();
    assert!(e.contains("\"usb\" module"), "{e}");
    // optional modules may be missing; the script checks for them
    load(r#"{"optional_modules": ["usb"]}"#, &format!("{PANEL} assert(usb == nil)"), &sample_natives()).unwrap();
}

#[test]
fn effects_only_outside_cell_functions() {
    let src = format!(
        r#"{PANEL}
        calc.poke()
        Bad = page.derive("bad", {{}}, function() return calc.poke() end)
        page.on("key", function(k) return calc.poke() end)
        "#
    );
    let mut p = load(r#"{"modules": ["calc"]}"#, &src, &sample_natives()).unwrap();
    let id = {
        let d = p.describe();
        assert!(d.contains("\"bad\""));
        p.graph().len()
    };
    assert!(id > 0);
    let r = p.act(r#"{"get": "bad"}"#);
    assert!(r.contains("changes state"), "{r}");
    assert!(p.key("x"));
}

#[test]
fn jobs_and_request_driven_sources() {
    let src = format!(
        r#"{PANEL}
        X = page.input("x", 4)
        Sq = page.job("sq", "calc.slow_square", {{X}})
        Req = page.input("req", nil)
        Echo = page.stream("echo", "calc.echo", {{request = Req}})
        page.on("key", function(k) page.set(Req, {{say = k}}) return true end)
        "#
    );
    let mut p = load(r#"{"modules": ["calc"]}"#, &src, &sample_natives()).unwrap();
    // job
    let sq = cell(&p, "sq");
    let v = wait_for(&mut p, sq, |d| d.num().is_some());
    assert_eq!(v, Data::Num(16.0));
    // source fed by requests
    assert!(p.key("hello"));
    let echo = cell(&p, "echo");
    let v = wait_for(&mut p, echo, |d| !d.is_nil());
    assert_eq!(v.get("last").get("echo").get("say").as_str(), Some("hello"));
    assert!(p.key("again"));
    let v = wait_for(&mut p, echo, |d| d.get("total").num() == Some(2.0));
    assert_eq!(v.get("items").items().len(), 2);
    p.shutdown();
}

fn cell(p: &ScriptPage, name: &str) -> CellId {
    let d: serde_json::Value = serde_json::from_str(&p.describe()).unwrap();
    assert!(d["cells"].as_array().unwrap().iter().any(|c| c["name"] == name), "no cell {name}");
    p.cell_id(name).unwrap()
}

#[test]
fn images_layers_and_the_semantic_port() {
    struct Count(std::sync::atomic::AtomicUsize);
    impl GpuRenderer for Count {
        fn render(&self, _t: &GpuTarget, _v: &anypage_core::Value) -> Result<(), String> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
    }
    let mut n = sample_natives();
    n.layer("calc.view", Arc::new(Count(Default::default())), |inputs| Ok(anypage_core::val(inputs.len())));
    let src = r#"
        local img = page.derive("img", {}, function() return calc.image() end)
        page.image({ rect = { 0, 0, 20, 10 }, source = img })
        local zoom = page.input("zoom", 1, { agent = true })
        page.layer({ rect = { 0, 20, 100, 100 }, kind = "calc.view", inputs = { zoom } })
        page.action({ name = "double", description = "Double the zoom", params = { type = "object" },
            run = function(p) local z = page.get(zoom) * (p.by or 2) page.set(zoom, z) return { zoom = z } end })
    "#;
    let mut p = load(r#"{"modules": ["calc"]}"#, src, &n).unwrap();
    assert_eq!(p.spec().layers.len(), 1);
    let panel = p.spec().panels[0].cell;
    let g = p.parts().0;
    let i = g.demand(panel);
    let img = i.get::<Image>().expect("the image value itself, no copy");
    assert_eq!((img.w, img.h), (2, 1));
    // the semantic port
    let d: serde_json::Value = serde_json::from_str(&p.describe()).unwrap();
    assert_eq!(d["actions"][0]["name"], "double");
    let r: serde_json::Value = serde_json::from_str(&p.act(r#"{"action": "double", "params": {"by": 3}}"#)).unwrap();
    assert_eq!(r["result"]["zoom"], 3);
    let r: serde_json::Value = serde_json::from_str(&p.act(r#"{"set": "zoom", "value": 10}"#)).unwrap();
    assert_eq!(r["ok"], true);
    let r: serde_json::Value = serde_json::from_str(&p.act(r#"{"get": "zoom"}"#)).unwrap();
    assert_eq!(r["value"], 10);
    let r: serde_json::Value = serde_json::from_str(&p.act(r#"{"set": "img", "value": 1}"#)).unwrap();
    assert!(r["error"].as_str().unwrap().contains("agent-writable"));
    let r: serde_json::Value = serde_json::from_str(&p.act(r#"{"action": "nope"}"#)).unwrap();
    assert!(r["error"].is_string());
}

#[test]
fn events_focus_clipboard_and_files() {
    let src = r#"
        page.panel({ rect = { 0, 0, 10, 10 }, draw = function(d) end })
        Log = page.input("log", "")
        local function add(s) page.set(Log, page.get(Log) .. s) end
        page.on("key", function(k) add("1:" .. k .. ";") return false end)
        page.on("key", function(k) add("2:" .. k .. ";") return true end)
        page.on("key", function(k) add("3;") return true end)
        page.on("text", function(t) add("t:" .. t .. ";") end)
        page.on("wheel", function(x, y, dx, dy) add("w:" .. dy .. ";") return true end)
        page.on("pointer_down", function(x, y)
            page.focus_text({ x, y, 2, 20 })
            page.copy("copied")
            page.open_file({ filters = { "txt" }, done = function(f) add("f:" .. f.name .. ":" .. f.size .. ";") end })
            return "captured"
        end)
        page.on("file_drop", function(f) add("d:" .. f.name .. ";") return true end)
    "#;
    let mut p = load(r#"{"files": true, "clipboard": true}"#, src, &Natives::new()).unwrap();
    assert!(p.key("k"));
    assert!(p.wheel(0.0, 0.0, 0.0, -2.0));
    assert_eq!(p.pointer_down(5.0, 6.0), PointerResponse::Captured);
    assert_eq!(p.text_focus().map(|r| (r.x, r.y)), Some((5.0, 6.0)));
    assert_eq!(p.take_request(), Some(PageRequest::Copy("copied".into())));
    let id = match p.take_request() {
        Some(PageRequest::OpenFile { id, filters }) => {
            assert_eq!(filters, vec!["txt".to_string()]);
            id
        }
        other => panic!("{other:?}"),
    };
    p.host_result(id, HostResult::File { name: "a.txt".into(), bytes: Arc::new(b"hello".to_vec()) });
    p.text_input("字");
    p.file_drop("b.pdf", Arc::new(vec![1, 2, 3]));
    let log = p.cell_value("log").unwrap();
    assert_eq!(log.as_str(), Some("1:k;2:k;w:-2;f:a.txt:5;t:字;d:b.pdf;"));
    // without the capability
    let e = load("{}", "page.panel({ rect = { 0, 0, 10, 10 }, draw = function(d) end }) page.copy('x')", &Natives::new()).err().unwrap();
    assert!(e.contains("clipboard"), "{e}");
}

#[test]
fn resources_survive_cells_and_compare_by_version() {
    let r = Res::new("calc.thing", 1u8);
    assert_eq!(Data::Res(r.clone()), Data::Res(r.clone()));
    assert_ne!(Data::Res(r.next_version()), Data::Res(r));
}

#[test]
fn peek_job_callbacks_and_eager_jobs() {
    let src = format!(
        r#"{PANEL}
        X = page.input("x", 3)
        Log = page.input("log", "")
        Sq = page.job("sq", "calc.slow_square", {{X}}, {{ eager = true, done = function(v)
            page.set(Log, page.get(Log) .. "done:" .. tostring(v) .. ";")
        end }})
        page.on("key", function(k)
            if k == "peek" then page.set(Log, page.get(Log) .. "peek:" .. tostring(page.peek(Sq)) .. ";") return true end
            page.set(X, tonumber(k)) return true
        end)
        "#
    );
    let mut p = load(r#"{"modules": ["calc"]}"#, &src, &sample_natives()).unwrap();
    // nobody reads "sq", but it is eager: it runs and calls back
    let t0 = Instant::now();
    loop {
        p.pump();
        if p.cell_value("log").unwrap().as_str().unwrap_or("").contains("done:9") {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(5), "eager job never finished: {:?}", p.cell_value("log"));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(p.key("peek"));
    assert!(p.key("5"));
    loop {
        p.pump();
        if p.cell_value("log").unwrap().as_str().unwrap_or("").contains("done:25") {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(5), "rerun never finished: {:?}", p.cell_value("log"));
        std::thread::sleep(Duration::from_millis(10));
    }
    let log = p.cell_value("log").unwrap();
    assert!(log.as_str().unwrap().starts_with("done:9;peek:9;"), "{log:?}");
}

#[test]
fn assets_navigation_cancel_and_bound_secrets() {
    let m = manifest(r#"{"secrets": ["plain", {"name": "key", "hosts": ["api.example.com"]}]}"#);
    let src = r#"
        page.panel({ rect = { 0, 0, 10, 10 }, draw = function(d) end })
        local b = page.asset("data.csv")
        Size = page.input("size", 0)
        page.set(Size, #b)
        Log = page.input("log", "")
        page.on("pointer_up", function() page.set(Log, page.get(Log) .. "up;") end)
        page.on("pointer_cancel", function() page.set(Log, page.get(Log) .. "cancel;") end)
        page.on("key", function(k) page.navigate("other-page") return true end)
    "#;
    let pkg = Package::from_files("test", [("page.json", m.as_str()), ("main.luau", src)]).unwrap().with_asset("data.csv", b"a,b\n1,2\n".to_vec()).unwrap();
    assert!(Package::from_files("t", [("page.json", m.as_str()), ("main.luau", "")]).unwrap().with_asset("x.exe", vec![1]).is_err());
    assert_eq!(pkg.manifest.caps.secret_hosts.get("key").map(|v| v.len()), Some(1));
    assert_eq!(pkg.manifest.caps.secrets, vec!["plain".to_string(), "key".to_string()]);
    let mut p = ScriptPage::load(&pkg, &Natives::new(), text(), wake()).unwrap();
    assert_eq!(p.cell_value("size"), Some(Data::Num(8.0)));
    p.pointer_cancel();
    p.pointer_up();
    assert_eq!(p.cell_value("log").unwrap().as_str(), Some("cancel;up;"));
    assert!(p.key("n"));
    assert_eq!(p.take_request(), Some(PageRequest::Navigate("other-page".into())));
}

#[test]
fn rtl_text_functions_and_panic_guards() {
    let mut n = sample_natives();
    n.function("calc.boom", Effect::Pure, |_| -> Result<Data, String> { panic!("module bug") });
    let src = format!(
        r#"{PANEL}
        assert(text.is_rtl("שלום") == true)
        assert(text.is_rtl("hello") == false)
        local lines = text.layout("שלום עולם", 20, 1000).lines
        assert(lines[1].rtl == true, "rtl line")
        local sel = text.selection("abc שלום", 20, 0, 3)
        assert(#sel >= 1)
        assert(text.caret("שלום", 20, 0) > text.caret("שלום", 20, #"שלום"), "caret 0 is on the right in rtl")
        local ok, err = pcall(calc.boom)
        assert(not ok and string.find(tostring(err), "internal error"), tostring(err))
        "#
    );
    load(r#"{"modules": ["calc"]}"#, &src, &n).unwrap();
}
