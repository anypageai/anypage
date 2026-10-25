//! Bounded memory: undo history has a byte budget per app and is dropped for
//! fields that left the view; scroll offsets of vanished elements are pruned
//! from the UI cell.

mod common;

use anypage_engine::Page;
use anypage_runtime::{Data, ScriptPage};
use common::*;

const FIELDS: &str = r#"
local ui = import("ui")
local app
app = ui.app({ name = "m", rect = { 0, 0, 800, 700 }, agent = true, state = { a = "", b = "", c = "", show_b = true },
    view = function(s)
        local function set(k) return function(v) app:set(k, v) end end
        return ui.column({ padding = 10, gap = 8 }, {
            ui.textarea({ id = "a", value = s.a, rows = 3, on_change = set("a") }),
            ui.when(s.show_b, ui.textarea({ id = "b", value = s.b, rows = 3, on_change = set("b") })),
            ui.textarea({ id = "c", value = s.c, rows = 3, on_change = set("c") }),
        })
    end })
page.action({ name = "stats", run = function() return app:stats() end })
page.action({ name = "limits", run = function() return ui.limits end })
"#;

fn history_bytes(p: &mut ScriptPage) -> f64 {
    act(p, "stats", Data::Nil).get("history_bytes").num().expect("history_bytes")
}

fn set(p: &mut ScriptPage, id: &str, v: &str) {
    act(p, "m.do", Data::map([("id", Data::str(id)), ("op", Data::str("set")), ("value", Data::str(v))]));
}

#[test]
fn undo_history_stays_within_its_budget_and_follows_the_view() {
    let mut p = load_pkg(&default_manifest(800.0, 700.0), FIELDS).unwrap_or_else(|e| panic!("{e}"));
    display(&mut p, 0);
    let budget = act(&mut p, "limits", Data::Nil).get("undo_bytes").num().expect("undo_bytes");
    // 90 whole-value replacements of about 100 KB: 9 MB of snapshots without a budget
    let mut last = String::new();
    for k in 0..90 {
        let id = ["a", "b", "c"][k % 3];
        last = format!("{k} {}", "x".repeat(100_000));
        set(&mut p, id, &last);
        display(&mut p, 0);
        let h = history_bytes(&mut p);
        assert!(h <= budget, "history {h} over the budget {budget} after {k} sets");
    }
    assert!(history_bytes(&mut p) > budget / 2.0, "the budget is used, not thrown away");
    // the newest step of the field edited last still undoes
    let before: String = state(&mut p, "m").get("c").as_str().unwrap_or("").to_string();
    assert_eq!(before, last);
    act(&mut p, "m.do", Data::map([("id", Data::str("c")), ("op", Data::str("focus"))]));
    assert!(p.key("ctrl+z"));
    let undone = state(&mut p, "m").get("c").as_str().unwrap_or("").to_string();
    assert!(undone.starts_with("86 "), "{}", &undone[..10.min(undone.len())]);
    // a field that leaves the view takes its history with it
    let with_b = history_bytes(&mut p);
    let r = p.act(r#"{"set": "m.state", "value": {"a": "", "b": "", "c": "", "show_b": false}}"#);
    assert!(r.contains("\"ok\":true"));
    display(&mut p, 0);
    let without_b = history_bytes(&mut p);
    assert!(without_b < with_b, "{without_b} !< {with_b}");
    assert_eq!(p.handler_errors, 0);
}

const SCROLLS: &str = r#"
local ui = import("ui")
local app
app = ui.app({ name = "s", rect = { 0, 0, 1200, 800 }, agent = true, state = { prefix = "p" },
    view = function(s)
        local cells = {}
        for i = 1, 24 do
            local lines = {}
            for j = 1, 20 do lines[j] = ui.text("line " .. j) end
            cells[i] = ui.scroll({ id = s.prefix .. i, height = 60 }, ui.column({}, lines))
        end
        return ui.grid({ columns = 6, gap = 8, padding = 8 }, cells)
    end })
"#;

#[test]
fn scroll_offsets_of_vanished_elements_are_dropped() {
    let mut p = load_pkg(&default_manifest(1200.0, 800.0), SCROLLS).unwrap_or_else(|e| panic!("{e}"));
    display(&mut p, 0);
    let wheel_all = |p: &mut ScriptPage, prefix: &str| {
        let d = describe(p, "s");
        for i in 1..=24 {
            let (x, y) = center(&node(&d, &format!("{prefix}{i}")));
            assert!(p.wheel(x, y, 0.0, -1.0));
            display(p, 0);
        }
    };
    let ids = |p: &mut ScriptPage| -> Vec<String> {
        ui_state(p, "s").get("scroll").entries().map(|(k, _)| k.to_string()).collect()
    };
    wheel_all(&mut p, "p");
    // every scroller is still there: all 24 offsets are kept
    assert_eq!(ids(&mut p).len(), 24);
    // the view now has 24 other scrollers; scrolling them drops the old ones
    assert!(p.act(r#"{"set": "s.state", "value": {"prefix": "q"}}"#).contains("\"ok\":true"));
    display(&mut p, 0);
    wheel_all(&mut p, "q");
    let left = ids(&mut p);
    assert!(left.iter().all(|k| k.starts_with('q')), "{left:?}");
    assert_eq!(left.len(), 24);
    assert_eq!(p.handler_errors, 0);
}
