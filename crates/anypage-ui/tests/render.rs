//! When apps render (only on their own cells and inputs), inputs reaching
//! the view, failures staying contained, and limits.

mod common;

use anypage_engine::Page;
use anypage_runtime::{Data, ScriptPage};
use common::*;

const PAGE: &str = r#"
local ui = import("ui")
local other = page.input("other", 0)
local data = page.input("data", { "a", "b" })
local doubled = page.derive("doubled", { data }, function(d)
    local out = {}
    for i, v in d do out[i] = v .. v end
    return out
end)
local app
app = ui.app({ name = "r", rect = { 0, 0, 400, 300 }, agent = true, state = { n = 1 }, inputs = { doubled },
    view = function(s, items)
        return ui.column({}, { ui.text("n=" .. s.n), ui.text(table.concat(items, ",")) })
    end })
page.action({ name = "stats", run = function() return app:stats() end })
page.on("key", function(k)
    if k == "o" then page.set(other, page.get(other) + 1) return true end
    if k == "d" then page.set(data, { "x", "y", "z" }) return true end
    if k == "n" then app:update(function(st) st.n += 1 end) return true end
    return false
end)
"#;

fn frames(p: &mut ScriptPage) -> f64 {
    act(p, "stats", Data::Nil).get("frames").num_or(-1.0)
}

#[test]
fn renders_only_when_its_cells_or_inputs_change() {
    let mut p = load(PAGE);
    assert!(has_text(&display(&mut p, 0), "aa,bb"));
    assert_eq!(frames(&mut p), 1.0);
    let sc = script_cell(&p, 0);
    let v = p.graph().version(sc);
    // an unrelated input: nothing runs
    assert!(p.key("o"));
    display(&mut p, 0);
    assert_eq!(frames(&mut p), 1.0);
    assert_eq!(p.graph().version(sc), v);
    // the app's input changed: one render with the new value
    assert!(p.key("d"));
    assert!(has_text(&display(&mut p, 0), "xx,yy,zz"));
    assert_eq!(frames(&mut p), 2.0);
    // its state changed: one render
    assert!(p.key("n"));
    assert!(has_text(&display(&mut p, 0), "n=2"));
    assert_eq!(frames(&mut p), 3.0);
    // the clock ticking does not run it (nothing declared a time)
    tick(&mut p, 5.0);
    display(&mut p, 0);
    assert_eq!(frames(&mut p), 3.0);
    assert_eq!(p.graph().expires(sc), f64::INFINITY);
}

#[test]
fn a_failing_view_is_contained_and_recovers() {
    let src = r#"
local ui = import("ui")
ui.app({ name = "e", rect = { 0, 0, 300, 200 }, agent = true, state = { bad = false },
    view = function(s)
        if s.bad then error("boom in the view") end
        return ui.column({ padding = 10 }, { ui.button("ok", { id = "ok" }), ui.text("fine") })
    end })
"#;
    let mut p = load(src);
    assert!(has_text(&display(&mut p, 0), "fine"));
    let r = p.act(r#"{"set": "e.state", "value": {"bad": true}}"#);
    assert!(r.contains("\"ok\":true"), "{r}");
    let sc = script_cell(&p, 0);
    // the panel draws an error card naming the failure
    let cmds = display(&mut p, 0);
    assert!(has_text_containing(&cmds, "the view failed"), "{:?}", texts(&cmds));
    assert!(has_text_containing(&cmds, "boom in the view"), "{:?}", texts(&cmds));
    assert!(!has_text(&cmds, "fine"));
    // and declares no time: idle frames do not run the failing view again
    assert_eq!(p.graph().expires(sc), f64::INFINITY);
    let v = p.graph().version(sc);
    let frames = Frames::new();
    for _ in 0..10 {
        frames.frame(&mut p);
    }
    assert_eq!(p.graph().version(sc), v, "a broken view re-ran on idle frames");
    let img = raster(&mut p, 0);
    assert!(img.w > 0);
    // events meet an empty tree: nothing happens, nothing fails
    let before = p.handler_errors;
    assert!(!p.key("tab"));
    p.pointer_down(20.0, 20.0);
    p.pointer_up();
    p.pointer_move(30.0, 30.0);
    assert_eq!(p.handler_errors, before);
    // fixing the state brings it back
    p.act(r#"{"set": "e.state", "value": {"bad": false}}"#);
    assert!(has_text(&display(&mut p, 0), "fine"));
}

/// The message a failing view shows on its error card.
fn card_text(p: &mut ScriptPage) -> String {
    let t = texts(&display(p, 0)).join(" ");
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn element_limit_and_bad_specs() {
    let src = r#"
local ui = import("ui")
ui.app({ name = "big", rect = { 0, 0, 300, 200 }, max_elements = 1000,
    view = function()
        local t = {}
        for i = 1, 2000 do t[i] = ui.text("x") end
        return ui.column({}, t)
    end })
"#;
    let mut p = load(src);
    let e = card_text(&mut p);
    assert!(e.contains("more than 1000 elements"), "{e}");
    let cases = [
        ("ui.app({ rect = { 0, 0, 10, 10 } })", "view must be a function"),
        ("ui.app({ rect = { 0, 0, 0, 10 }, view = function() end })", "w, h > 0"),
        ("ui.app({ view = function() end })", "rect must be"),
        ("ui.app({ name = 'a', rect = { 0, 0, 10, 10 }, view = function() end }) ui.app({ name = 'a', rect = { 0, 0, 10, 10 }, view = function() end })", "already an app named"),
        ("ui.app({ rect = { 0, 0, 10, 10 }, state = 5, view = function() end })", "state must be a table"),
    ];
    for (body, want) in cases {
        let e = load_pkg(&manifest(100.0, 100.0, ""), &format!("local ui = import('ui') {body}")).err().unwrap_or_default();
        assert!(e.contains(want), "{body}: {e}");
    }
    // unknown element types are reported with their id
    let src = r#"
local ui = import("ui")
ui.app({ rect = { 0, 0, 600, 200 }, view = function() return ui.column({}, { { type = "nope", id = "x" } }) end })
"#;
    let mut p = load(src);
    let e = card_text(&mut p);
    assert!(e.contains("unknown element type \"nope\""), "{e}");
    // wrong prop types name the element and the prop
    let props = [
        (r#"ui.select({ id = "s", options = "abc" })"#, r#"ui.select "s": options must be a list"#),
        (r#"ui.radio({ id = "r", options = 5 })"#, r#"ui.radio "r": options must be a list"#),
        (r#"ui.tabs({ id = "t", tabs = "x" })"#, r#"ui.tabs "t": tabs must be a list"#),
        (r#"ui.button("b", { id = "b", width = "abc" })"#, r#"ui.button "b": width must be a number, got string "abc""#),
        (r#"ui.column({ id = "c", padding = "8" }, {})"#, r#"ui.column "c": padding must be a number"#),
        (r#"ui.column({ id = "c", padding = { 4, "x" } }, {})"#, r#"ui.column "c": padding.2 must be a number"#),
        (r#"ui.list({ id = "l", count = "many" })"#, r#"ui.list "l": count must be a number"#),
        (r#"ui.list({ id = "l", items = "abc" })"#, r#"ui.list "l": items must be a list"#),
        (r#"ui.table({ id = "tb", data = { columns = "A", rows = {} } })"#, r#"ui.table "tb": data.columns must be a list"#),
        (r#"ui.table({ id = "tb", columns = { "A" }, row = function(i) return { i } end })"#, r#"ui.table "tb": row(i) needs count"#),
        (r#"ui.slider({ id = "sl", value = "1" })"#, r#"ui.slider "sl": value must be a number"#),
        (r#"ui.button("b", { id = "b", on_click = "go" })"#, r#"ui.button "b": on_click must be a function"#),
        (r#"ui.textfield({ id = "f", max_length = "9" })"#, r#"ui.textfield "f": max_length must be a number"#),
    ];
    for (el, want) in props {
        let src = format!("local ui = import('ui') ui.app({{ rect = {{ 0, 0, 600, 200 }}, view = function() return ui.column({{}}, {{ {el} }}) end }})");
        let mut p = load(&src);
        let e = card_text(&mut p);
        assert!(e.contains(want), "{el}: {e}");
        assert_eq!(p.handler_errors, 0);
    }
}

/// The example from docs/MODULES.md. The only change: `local app` is declared
/// before `ui.app(...)`, since a Lua local is not in scope inside its own
/// initialiser (the callbacks would otherwise see a nil global `app`).
#[test]
fn the_spec_example_runs() {
    let src = r#"
local ui = import("ui")
local cell_a = page.input("a", { { title = "first" }, { title = "second" }, { title = "third" } })
local cell_b = page.input("b", 0)
local app
app = ui.app({
    rect = { 0, 0, 600, 500 }, name = "settings",
    inputs = { cell_a, cell_b },
    state = { count = 0, name = "" },
    theme = ui.themes.dark,
    view = function(s, a, b)
        return ui.column({ gap = 8, padding = 16 }, {
            ui.text("Hello " .. s.name, { size = 20 }),
            ui.row({ gap = 8 }, {
                ui.button("Add", { id = "add", on_click = function() app:update(function(st) st.count += 1 end) end }),
                ui.button("Reset", { id = "reset", variant = "secondary", on_click = function() app:set("count", 0) end }),
            }),
            ui.textfield({ id = "name", value = s.name, placeholder = "Name", on_change = function(v) app:set("name", v) end }),
            ui.list({ id = "list", items = a, height = 300, row_height = 28, selected = s.sel,
                      row = function(item, i, selected) return ui.text(item.title) end,
                      on_select = function(item, i) app:set("sel", i) end }),
        })
    end,
})
page.action({ name = "settings.describe", run = function() return app:describe() end })
"#;
    let mut p = load(src);
    assert!(has_text(&display(&mut p, 0), "second"));
    click_id(&mut p, "settings", "add");
    click_id(&mut p, "settings", "add");
    assert_eq!(state(&mut p, "settings").get("count").num(), Some(2.0));
    click_id(&mut p, "settings", "reset");
    assert_eq!(state(&mut p, "settings").get("count").num(), Some(0.0));
    click_id(&mut p, "settings", "name");
    p.text_input("Ada");
    assert!(has_text(&display(&mut p, 0), "Hello Ada"));
    click_id(&mut p, "settings", "list#2");
    assert_eq!(state(&mut p, "settings").get("sel").num(), Some(2.0));
    assert_eq!(p.handler_errors, 0);
}

#[test]
fn images_from_native_modules_are_drawn() {
    let mut n = natives();
    // a 2 x 2 image: red, green / green, red (premultiplied, opaque)
    n.function("img.checker", anypage_runtime::Effect::Pure, |_| {
        let px = vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 255, 0, 255, 255, 0, 0, 255];
        Ok(Data::res("img.image", anypage_engine::Image { w: 2, h: 2, rgba: px, newest: None }))
    });
    let manifest = r#"{ "format": 1, "id": "img.test", "bounds": [0, 0, 300, 300], "capabilities": { "modules": ["img"] } }"#;
    let src = r#"
local ui = import("ui")
ui.app({ name = "im", rect = { 0, 0, 300, 300 }, agent = true,
    view = function()
        return ui.column({}, {
            ui.image(img.checker(), { id = "a", width = 200, height = 200, fit = "fill", radius = 0 }),
            ui.image(img.checker(), { id = "b", width = 100, height = 50, aspect = 1 }),
        })
    end })
"#;
    let pkg = anypage_runtime::Package::from_files("test", [("page.json", manifest), ("main.luau", src)]).unwrap();
    let mut p = ScriptPage::load(&pkg, &n, text(), wake()).unwrap_or_else(|e| panic!("{e}"));
    let cmds = display(&mut p, 0);
    assert_eq!(commands(&cmds, "image").len(), 2);
    // letterboxed: a square image in a 100 x 50 box is 50 x 50, centred
    let b = commands(&cmds, "image")[1].clone();
    assert_eq!((b.at(3).num(), b.at(5).num(), b.at(6).num()), (Some(25.0), Some(50.0), Some(50.0)));
    let img = raster(&mut p, 0);
    save_png("image-element.png", &img);
    let tl = pixel(&img, 40, 40);
    let tr = pixel(&img, 160, 40);
    assert!(tl[0] > 200 && tl[1] < 60, "top-left should be red: {tl:?}");
    assert!(tr[1] > 200 && tr[0] < 60, "top-right should be green: {tr:?}");
}

#[test]
fn view_shapes_and_reused_elements() {
    // a view may return a plain list; an element value may appear twice
    let src = r#"
local ui = import("ui")
local SEP = ui.divider()
local TITLE = ui.text("constant title")
ui.app({ name = "v", rect = { 0, 0, 300, 300 }, agent = true, state = { k = 0 },
    view = function(s)
        return { TITLE, SEP, ui.text("k=" .. s.k), SEP, false, nil, "a plain string", { ui.text("nested") } }
    end })
page.on("key", function() return true end)
"#;
    let mut p = load(src);
    let cmds = display(&mut p, 0);
    for t in ["constant title", "k=0", "a plain string", "nested"] {
        assert!(has_text(&cmds, t), "{t}");
    }
    let d = describe(&mut p, "v");
    let dividers = d.get("nodes").items().iter().filter(|n| n.get("type").as_str() == Some("divider")).count();
    assert_eq!(dividers, 2);
    // rebuilding (as handlers do) gives the same tree again
    let r = p.act(r#"{"set": "v.state", "value": {"k": 1}}"#);
    assert!(r.contains("ok"));
    let d2 = describe(&mut p, "v");
    assert_eq!(d2.get("nodes").items().len(), d.get("nodes").items().len());
    assert!(has_text(&display(&mut p, 0), "k=1"));
}
