//! What an agent sees and does through the real semantic port (`Page::act`,
//! JSON in and out), which cuts lists after 40 entries and turns lists or
//! maps three levels deep into counts: paged descriptions, option labels as
//! JSON text, items of lists and tables, `set` under the same rules as
//! typing, and secrets kept out of what agents can read.

mod common;

use std::collections::HashSet;

use anypage_engine::Page;
use anypage_runtime::{Data, ScriptPage};
use common::*;
use serde_json::{json, Value as Json};

fn gallery() -> ScriptPage {
    let pkg = anypage_ui::gallery().expect("gallery package");
    ScriptPage::load(&pkg, &natives(), text(), wake()).unwrap_or_else(|e| panic!("gallery: {e}"))
}

fn port(p: &mut ScriptPage, req: Json) -> Json {
    let out = p.act(&req.to_string());
    serde_json::from_str(&out).unwrap_or_else(|e| panic!("bad JSON from the port ({e}): {out}"))
}

fn ok(p: &mut ScriptPage, req: Json) -> Json {
    let r = port(p, req.clone());
    assert_eq!(r["ok"], true, "{req} -> {r}");
    r["result"].clone()
}

/// Every node of an app's description, paging with `next`.
fn all_nodes(p: &mut ScriptPage, app: &str, params: Json) -> Vec<Json> {
    let mut nodes = Vec::new();
    let mut params = params;
    let mut total = None;
    loop {
        let r = ok(p, json!({ "action": format!("{app}.describe"), "params": params.clone() }));
        let page = r["nodes"].as_array().unwrap_or_else(|| panic!("nodes are not a plain list: {r}")).clone();
        assert!(page.len() <= 40, "{} nodes in one page", page.len());
        total = total.or(r["total"].as_u64());
        nodes.extend(page);
        match r["next"].as_u64() {
            Some(n) => params["from"] = json!(n),
            None => break,
        }
    }
    assert_eq!(Some(nodes.len() as u64), total, "pages do not add up to the total");
    nodes
}

fn find<'a>(nodes: &'a [Json], id: &str) -> &'a Json {
    nodes.iter().find(|n| n["id"] == id).unwrap_or_else(|| panic!("no node {id}"))
}

/// A list-valued field, sent as JSON text.
fn json_list(n: &Json, key: &str) -> Vec<String> {
    let s = n[key].as_str().unwrap_or_else(|| panic!("{key} is not JSON text: {n}"));
    serde_json::from_str(s).unwrap_or_else(|e| panic!("{key}: {e}: {s}"))
}

#[test]
fn the_whole_gallery_is_reachable_through_the_port() {
    let mut p = gallery();
    let nodes = all_nodes(&mut p, "dark", json!({}));
    assert!(nodes.len() > 100, "only {} nodes", nodes.len());
    let ids: HashSet<&str> = nodes.iter().filter_map(|n| n["id"].as_str()).collect();
    assert_eq!(ids.len(), nodes.len(), "duplicate ids across pages");
    for id in ["primary", "fruit", "size", "tabs", "list", "table", "notes", "password", "scroll", "open-dialog", "volume"] {
        assert!(ids.contains(id), "{id} unreachable");
    }
    // nothing the port summarised away
    for n in &nodes {
        let s = n.to_string();
        assert!(!s.contains("\"$list\"") && !s.contains("\"$map\"") && !s.contains("\"$head\""), "{s}");
    }
    // option labels arrive whole
    let fruit = json_list(find(&nodes, "fruit"), "options");
    assert_eq!(fruit.len(), 11);
    assert_eq!(fruit[2], "Cherry");
    assert_eq!(json_list(find(&nodes, "size"), "options"), ["Small", "Medium", "Large"]);
    assert_eq!(json_list(find(&nodes, "tabs"), "tabs"), ["Overview", "Details", "語言"]);
    assert_eq!(json_list(find(&nodes, "table"), "columns"), ["ID", "Name", "City", "Score", "Active"]);
    assert_eq!(find(&nodes, "fruit")["selected"], 3);

    // filters
    let buttons = all_nodes(&mut p, "dark", json!({ "types": "button" }));
    assert_eq!(buttons.len(), 9, "{buttons:?}");
    assert!(buttons.iter().all(|n| n["type"] == "button"));
    let controls = all_nodes(&mut p, "dark", json!({ "interactive_only": true }));
    assert!(controls.len() < nodes.len(), "{} of {}", controls.len(), nodes.len());
    let passive = ["text", "column", "row", "card", "grid", "stack", "spacer", "divider", "badge", "progress", "image"];
    for n in &controls {
        assert!(!passive.contains(&n["type"].as_str().unwrap_or("")), "not a control: {n}");
    }
    let in_list = all_nodes(&mut p, "dark", json!({ "within": "list", "limit": 3 }));
    assert_eq!(in_list[0]["id"], "list");
    assert!(in_list.iter().skip(1).any(|n| n["type"] == "row_item"));
    assert!(in_list.iter().all(|n| n["id"].as_str().is_some_and(|s| s.starts_with("list"))));
    let bad = port(&mut p, json!({ "action": "dark.describe", "params": { "within": "nope" } }));
    assert!(bad["error"].as_str().unwrap_or("").contains("no element \"nope\""), "{bad}");

    // the dialog's buttons, once it is open
    ok(&mut p, json!({ "action": "dark.do", "params": { "id": "open-dialog", "op": "click" } }));
    let dialog = all_nodes(&mut p, "dark", json!({ "within": "dialog/panel" }));
    let dids: Vec<&str> = dialog.iter().filter_map(|n| n["id"].as_str()).collect();
    assert!(dids.contains(&"cancel") && dids.contains(&"delete"), "{dids:?}");
    assert_eq!(dialog[0]["type"], "dialog");
    ok(&mut p, json!({ "action": "dark.do", "params": { "id": "cancel", "op": "click" } }));
    assert_eq!(state(&mut p, "dark").get("dialog").as_bool(), Some(false));
    // only the refused `within: "nope"` (action errors count as handler errors)
    assert_eq!(p.handler_errors, 1);
}

#[test]
fn items_of_selects_lists_and_tables_page_through_the_port() {
    let mut p = gallery();
    let r = ok(&mut p, json!({ "action": "dark.items", "params": { "id": "fruit" } }));
    assert_eq!(r["count"], 11);
    assert_eq!(r["items"].as_array().map(Vec::len), Some(11));
    assert_eq!(r["items"][10], "Mango");
    assert!(r["next"].is_null());
    // the 10 000-row list, a page from the end
    let r = ok(&mut p, json!({ "action": "dark.items", "params": { "id": "list", "from": 9990 } }));
    assert_eq!(r["count"], 10000);
    assert_eq!(r["items"].as_array().map(Vec::len), Some(11));
    assert!(r["next"].is_null());
    let r = ok(&mut p, json!({ "action": "dark.items", "params": { "id": "list", "limit": 500 } }));
    assert_eq!(r["items"].as_array().map(Vec::len), Some(40), "limit above 40 is capped");
    assert_eq!(r["next"], 41);
    // table rows keep their cell values
    let r = ok(&mut p, json!({ "action": "dark.items", "params": { "id": "table", "from": 2, "limit": 3 } }));
    assert_eq!(serde_json::from_str::<Vec<String>>(r["columns"].as_str().expect("columns text")).map(|c| c.len()).ok(), Some(5));
    let rows = r["items"].as_array().expect("rows");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][0], 2);
    assert_eq!(rows[0][2], "Taipei");
    assert_eq!(r["next"], 5);
    let bad = port(&mut p, json!({ "action": "dark.items", "params": { "id": "primary" } }));
    assert!(bad["error"].as_str().unwrap_or("").contains("has no items"), "{bad}");
}

const SETS: &str = r#"
local ui = import("ui")
local app
app = ui.app({ name = "s", rect = { 0, 0, 600, 600 }, agent = true,
    state = { one = "", ro = "fixed", short = "", area = "", pw = "", v = 0, on = false },
    view = function(s)
        local function set(k) return function(v) app:set(k, v) end end
        return ui.column({ padding = 10, gap = 8 }, {
            ui.textfield({ id = "one", value = s.one, on_change = set("one") }),
            ui.textfield({ id = "ro", value = s.ro, readonly = true, on_change = set("ro") }),
            ui.textfield({ id = "short", value = s.short, max_length = 5, on_change = set("short") }),
            ui.textfield({ id = "pw", value = s.pw, password = true, on_change = set("pw") }),
            ui.textarea({ id = "area", value = s.area, on_change = set("area") }),
            ui.slider({ id = "v", value = s.v, min = 0, max = 100, step = 5, on_change = set("v") }),
            ui.checkbox("on", { id = "on", checked = s.on, on_change = set("on") }),
            ui.select({ id = "sel", options = { "a", "b" }, selected = s.sel, on_change = set("sel") }),
        })
    end })
"#;

#[test]
fn agent_set_follows_the_rules_of_typing() {
    let mut p = load_pkg(&default_manifest(600.0, 600.0), SETS).unwrap_or_else(|e| panic!("{e}"));
    let set = |p: &mut ScriptPage, id: &str, v: Json| port(p, json!({ "action": "s.do", "params": { "id": id, "op": "set", "value": v } }));
    // a one-line field gets no line breaks
    let r = set(&mut p, "one", json!("first\nsecond\r\nthird"));
    assert_eq!(r["result"]["value"], "first second third", "{r}");
    assert_eq!(state(&mut p, "s").get("one").as_str(), Some("first second third"));
    // read-only refuses
    let r = set(&mut p, "ro", json!("changed"));
    assert!(r["error"].as_str().unwrap_or("").contains("read-only"), "{r}");
    assert_eq!(state(&mut p, "s").get("ro").as_str(), Some("fixed"));
    // max_length cuts (at a character boundary)
    let r = set(&mut p, "short", json!("abcd字ef"));
    assert_eq!(r["result"]["value"], "abcd", "{r}");
    // a text area keeps its lines, with \r\n made \n
    let r = set(&mut p, "area", json!("one\r\ntwo"));
    assert_eq!(r["result"]["value"], "one\ntwo");
    // a password is set but not echoed
    let r = set(&mut p, "pw", json!("hunter2"));
    assert_eq!(r["ok"], true);
    assert!(r["result"]["value"].is_null(), "{r}");
    assert_eq!(state(&mut p, "s").get("pw").as_str(), Some("hunter2"));
    // sliders snap to their step and stay in range
    let r = set(&mut p, "v", json!(43));
    assert_eq!(r["result"]["value"], 45);
    assert_eq!(state(&mut p, "s").get("v").num(), Some(45.0));
    set(&mut p, "v", json!(1000));
    assert_eq!(state(&mut p, "s").get("v").num(), Some(100.0));
    let r = set(&mut p, "v", json!("loud"));
    assert!(r["error"].as_str().unwrap_or("").contains("must be a number"), "{r}");
    // toggles take booleans, choices take indexes in range
    let r = set(&mut p, "on", json!("yes"));
    assert!(r["error"].as_str().unwrap_or("").contains("true or false"), "{r}");
    set(&mut p, "on", json!(true));
    assert_eq!(state(&mut p, "s").get("on").as_bool(), Some(true));
    let r = set(&mut p, "sel", json!(3));
    assert!(r["error"].as_str().unwrap_or("").contains("out of range 1..2"), "{r}");
    set(&mut p, "sel", json!(2));
    assert_eq!(state(&mut p, "s").get("sel").num(), Some(2.0));
    // an agent's set is one undo step: focus the field, Ctrl+Z
    ok(&mut p, json!({ "action": "s.do", "params": { "id": "one", "op": "focus" } }));
    assert!(p.key("ctrl+z"));
    assert_eq!(state(&mut p, "s").get("one").as_str(), Some(""));
    // the four refusals above (read-only, "loud", "yes", index 3) are action
    // errors, which the runtime counts with handler errors; nothing else failed
    assert_eq!(p.handler_errors, 4);
}

#[test]
fn secrets_stay_out_of_what_agents_can_read() {
    let mut p = gallery();
    let d = describe(&mut p, "dark");
    let (x, y) = center(&node(&d, "password"));
    p.pointer_down(x, y);
    p.pointer_up();
    p.text_input("hunter2");
    // an IME composition in the password field
    p.ime_preedit("zebra");
    let shown = texts(&display(&mut p, 0));
    assert!(shown.iter().any(|t| t.contains("•••••••")), "the field shows dots: {shown:?}");
    assert!(!shown.iter().any(|t| t.contains("hunter2") || t.contains("zebra")));
    for req in [json!({ "describe": true }), json!({ "get": "dark.state" }), json!({ "get": "dark.ui" })] {
        let out = p.act(&req.to_string());
        assert!(!out.contains("hunter2"), "{req} shows the password: {out}");
        assert!(!out.contains("zebra"), "{req} shows the preedit: {out}");
    }
    let nodes = all_nodes(&mut p, "dark", json!({ "types": "textfield" }));
    let pw = find(&nodes, "password");
    assert!(pw["value"].is_null() && pw["password"] == true, "{pw}");
    // the page still has the value it needs (it keeps it in its script)
    assert_eq!(state(&mut p, "dark").get("pw_edits").num(), Some(1.0));
    assert_eq!(p.handler_errors, 0);
}

const LAZY: &str = r#"
local ui = import("ui")
local app
local CITIES = { "Taipei", "Tainan", "Hsinchu" }
app = ui.app({ name = "lz", rect = { 0, 0, 700, 500 }, agent = true, state = {},
    view = function(s)
        return ui.column({ padding = 10 }, {
            ui.table({ id = "t", columns = { "N", "City", "Even" }, count = 100000, grow = 1,
                row = function(i) return { i, CITIES[i % 3 + 1], i % 2 == 0 } end,
                selected = s.sel,
                on_select = function(row, i) app:set("sel", i) app:set("picked", row[2]) end,
                on_activate = function(row, i) app:set("opened", row[1]) end }),
        })
    end })
page.action({ name = "stats", run = function() return app:stats() end })
"#;

#[test]
fn a_lazy_table_builds_only_the_rows_it_shows() {
    let mut p = load_pkg(&default_manifest(700.0, 500.0), LAZY).unwrap_or_else(|e| panic!("{e}"));
    let cmds = display(&mut p, 0);
    assert!(has_text(&cmds, "Taipei") || has_text(&cmds, "Tainan"));
    let drawn = texts(&cmds).iter().filter(|t| ["Taipei", "Tainan", "Hsinchu"].contains(&t.as_str())).count();
    assert!((5..40).contains(&drawn), "{drawn} rows drawn");
    let n = node(&describe(&mut p, "lz"), "t");
    assert_eq!(n.get("rows").num(), Some(100000.0));
    // keyboard to the far end: on_select gets row(100000)
    ok(&mut p, json!({ "action": "lz.do", "params": { "id": "t", "op": "focus" } }));
    assert!(p.key("end"));
    assert_eq!(state(&mut p, "lz").get("sel").num(), Some(100000.0));
    assert_eq!(state(&mut p, "lz").get("picked").as_str(), Some("Tainan"));
    assert!(has_text(&display(&mut p, 0), "100000"));
    assert!(p.key("enter"));
    assert_eq!(state(&mut p, "lz").get("opened").num(), Some(100000.0));
    // an agent reads rows a page at a time, and sets the selection
    let r = ok(&mut p, json!({ "action": "lz.items", "params": { "id": "t", "from": 50000, "limit": 2 } }));
    assert_eq!(r["items"][0], json!([50000, "Hsinchu", true]));
    assert_eq!(r["next"], 50002);
    ok(&mut p, json!({ "action": "lz.do", "params": { "id": "t", "op": "set", "value": 8 } }));
    assert_eq!(state(&mut p, "lz").get("picked").as_str(), Some("Hsinchu"));
    let st = act(&mut p, "stats", Data::Nil);
    assert!(st.get("elements").num_or(1e9) < 20.0, "a table is one element: {st:?}");
    assert_eq!(p.handler_errors, 0);
}
