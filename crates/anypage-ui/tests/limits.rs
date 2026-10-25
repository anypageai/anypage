//! The library under the runtime's default limits (50 ms per call and per
//! handler, 64 MB of script memory): what a real page gets unless its
//! manifest asks for more. Large text areas at the library's size limit,
//! the 100 000-row list and the 200-element form all have to stay inside.
//!
//! Timings are printed (and written to target/test-output/limits-area.txt);
//! the assertions are that nothing failed: no "time budget exceeded", no
//! error card, no handler errors.

mod common;

use std::fmt::Write as _;
use std::time::Instant;

use anypage_core::Status;
use anypage_engine::Page;
use anypage_runtime::{Data, ScriptPage};
use common::*;

const AREA: &str = r#"
local ui = import("ui")
local app
app = ui.app({ name = "t", rect = { 0, 0, 900, 700 }, agent = true, state = { text = "" },
    view = function(s)
        return ui.column({ padding = 10 }, {
            ui.textarea({ id = "area", value = s.text, height = 600, on_change = function(v) app:set("text", v) end }),
        })
    end })
page.action({ name = "stats", run = function() return app:stats() end })
page.action({ name = "limits", run = function() return ui.limits end })
"#;

/// Text of `bytes` bytes in one of four shapes: prose paragraphs that wrap,
/// code-like short lines, tiny lines, or a single paragraph.
/// Every paragraph starts with its own number, so no two are alike (the
/// library caches layout per paragraph string).
fn shaped(shape: &str, bytes: usize) -> String {
    let words = ["lorem", "ipsum", "dolor", "sit", "amet", "consectetur", "adipiscing", "elit", "中文", "測試", "naïve"];
    let para = match shape {
        "prose" => 400,
        "code" => 40,
        "tiny" => 1,
        _ => usize::MAX,
    };
    let mut s = String::with_capacity(bytes + 16);
    let (mut k, mut cur, mut n) = (0usize, 0usize, 0usize);
    while s.len() < bytes {
        if cur >= para {
            s.push('\n');
            cur = 0;
            continue;
        }
        if cur == 0 {
            n += 1;
            let num = n.to_string();
            s.push_str(&num);
            cur += num.len();
            continue;
        }
        let w = words[k % words.len()];
        k += 1;
        s.push(' ');
        s.push_str(w);
        cur += w.len() + 1;
    }
    let mut cut = bytes.min(s.len());
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s
}

/// Evaluate the panel; fail on a script error or the library's error card.
fn frame(p: &mut ScriptPage, what: &str) -> f64 {
    let sc = script_cell(p, 0);
    let t = Instant::now();
    let i = p.parts().0.demand(sc);
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    match &i.status {
        Status::Ready => {}
        other => panic!("{what}: panel failed: {other:?}"),
    }
    let cmds = display(p, 0);
    assert!(!has_text_containing(&cmds, "the view failed"), "{what}: {:?}", texts(&cmds));
    ms
}

fn timed(f: impl FnOnce()) -> f64 {
    let t = Instant::now();
    f();
    t.elapsed().as_secs_f64() * 1000.0
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

#[test]
fn large_text_areas_fit_the_default_budget() {
    let mut p = load_pkg(&default_manifest(900.0, 700.0), AREA).unwrap_or_else(|e| panic!("{e}"));
    let max_area = act(&mut p, "limits", Data::Nil).get("area").num().expect("ui.limits.area") as usize;
    let mut out = String::new();
    let _ = writeln!(out, "text area at the size limit ({max_area} bytes), default limits (50 ms per call / handler)");
    for shape in ["prose", "code", "tiny", "one"] {
        // just under the limit, so typing still has room
        for bytes in [max_area / 4, max_area - 2048] {
            let mut p = load_pkg(&default_manifest(900.0, 700.0), AREA).unwrap_or_else(|e| panic!("{e}"));
            frame(&mut p, "empty");
            let text = shaped(shape, bytes);
            let set = serde_json::json!({ "set": "t.state", "value": { "text": text } }).to_string();
            let r = p.act(&set);
            assert!(r.contains("\"ok\":true"), "{r}");
            let what = format!("{shape} {bytes}");
            let first = frame(&mut p, &format!("{what}: first frame"));
            // focus near the top left, then type, break lines, delete, move
            let n = node(&describe(&mut p, "t"), "area");
            let (x, y) = ((n.get("x").num_or(0.0) + 30.0) as f32, (n.get("y").num_or(0.0) + 20.0) as f32);
            let focus = timed(|| {
                p.pointer_down(x, y);
                p.pointer_up();
            });
            frame(&mut p, &format!("{what}: after focus"));
            let (mut keys, mut frames) = (Vec::new(), Vec::new());
            for k in 0..24 {
                let h = timed(|| match k % 8 {
                    0..=4 => p.text_input("x"),
                    5 => {
                        p.key("enter");
                    }
                    6 => {
                        p.key("backspace");
                    }
                    _ => {
                        p.key("down");
                    }
                });
                keys.push(h);
                frames.push(frame(&mut p, &format!("{what}: after key {k}")));
            }
            let mut far = Vec::new();
            for key in ["pagedown", "ctrl+end", "x", "up", "ctrl+home", "x"] {
                let h = timed(|| {
                    if key == "x" {
                        p.text_input("x");
                    } else {
                        p.key(key);
                    }
                });
                far.push(h + frame(&mut p, &format!("{what}: after {key}")));
            }
            assert_eq!(p.handler_errors, 0, "{what}");
            let len = state(&mut p, "t").get("text").as_str().map(str::len).unwrap_or(0);
            assert!(len > bytes.min(max_area) / 2, "{what}: text lost ({len} bytes)");
            let line = format!(
                "{shape:>5} {bytes:>7} B: first frame {first:6.2} ms, focus {focus:6.2} ms, keystroke handler median {:5.2} max {:5.2} ms, frame after it median {:5.2} max {:5.2} ms, far moves (handler+frame) max {:5.2} ms",
                median(&mut keys.clone()),
                keys.iter().cloned().fold(0.0, f64::max),
                median(&mut frames.clone()),
                frames.iter().cloned().fold(0.0, f64::max),
                far.iter().cloned().fold(0.0, f64::max),
            );
            println!("{line}");
            let _ = writeln!(out, "{line}");
        }
    }
    let _ = std::fs::write(out_dir().join("limits-area.txt"), out);
}

#[test]
fn text_beyond_the_limit_is_cut_and_reported() {
    let mut p = load_pkg(&default_manifest(900.0, 700.0), AREA).unwrap_or_else(|e| panic!("{e}"));
    let max_area = act(&mut p, "limits", Data::Nil).get("area").num().expect("limit") as usize;
    // an agent's set goes through the same limit as typing and pasting
    let big = shaped("prose", max_area + 5000);
    let r = act(
        &mut p,
        "t.do",
        Data::map([("id", Data::str("area")), ("op", Data::str("set")), ("value", Data::str(&big))]),
    );
    let applied = r.get("value").as_str().map(str::len).unwrap_or(0);
    assert!(applied <= max_area && applied > max_area - 8, "{applied}");
    frame(&mut p, "cut text");
    // typing at the limit does nothing
    let n = node(&describe(&mut p, "t"), "area");
    let (x, y) = center(&n);
    p.pointer_down(x, y);
    p.pointer_up();
    p.text_input("y");
    let len = state(&mut p, "t").get("text").as_str().map(str::len).unwrap_or(0);
    assert_eq!(len, applied);
    assert_eq!(p.handler_errors, 0);

    // a page (or an agent writing the state cell) may hold more than an area
    // edits: it shows the beginning, read-only, and the first frame stays cheap
    let huge = shaped("prose", max_area * 8);
    let set = serde_json::json!({ "set": "t.state", "value": { "text": huge } }).to_string();
    assert!(p.act(&set).contains("\"ok\":true"));
    let ms = frame(&mut p, "over the limit");
    assert!(has_text_containing(&display(&mut p, 0), "read-only: the first"), "{:?}", texts(&display(&mut p, 0)));
    let n = node(&describe(&mut p, "t"), "area");
    assert_eq!(n.get("truncated").as_bool(), Some(true));
    assert_eq!(n.get("length").num(), Some(huge.len() as f64));
    assert_eq!(n.get("readonly").as_bool(), Some(true));
    // keys do not cut the real value
    let (x, y) = center(&n);
    p.pointer_down(x, y);
    p.pointer_up();
    p.text_input("z");
    assert!(p.key("backspace"));
    assert_eq!(state(&mut p, "t").get("text").as_str().map(str::len), Some(huge.len()));
    // an agent can still replace it with something that fits
    let r = act(&mut p, "t.do", Data::map([("id", Data::str("area")), ("op", Data::str("set")), ("value", Data::str("short again"))]));
    assert_eq!(r.get("value").as_str(), Some("short again"));
    assert_eq!(state(&mut p, "t").get("text").as_str(), Some("short again"));
    frame(&mut p, "after replacing");
    println!("over-limit value ({} bytes): first frame {ms:.2} ms", huge.len());
    assert_eq!(p.handler_errors, 0);
}

#[test]
fn a_long_text_field_fits_the_default_budget() {
    let src = r#"
local ui = import("ui")
local app
app = ui.app({ name = "t", rect = { 0, 0, 900, 200 }, agent = true, state = { text = "" },
    view = function(s)
        return ui.column({ padding = 10 }, {
            ui.textfield({ id = "f", value = s.text, on_change = function(v) app:set("text", v) end }),
        })
    end })
page.action({ name = "limits", run = function() return ui.limits end })
"#;
    let mut p = load_pkg(&default_manifest(900.0, 200.0), src).unwrap_or_else(|e| panic!("{e}"));
    let max_field = act(&mut p, "limits", Data::Nil).get("field").num().expect("limit") as usize;
    let text: String = shaped("one", max_field - 64);
    let set = serde_json::json!({ "set": "t.state", "value": { "text": text } }).to_string();
    assert!(p.act(&set).contains("\"ok\":true"));
    let first = frame(&mut p, "long field");
    let (x, y) = center(&node(&describe(&mut p, "t"), "f"));
    p.pointer_down(x, y);
    p.pointer_up();
    let mut worst: f64 = 0.0;
    for k in 0..20 {
        let h = timed(|| {
            if k % 5 == 4 {
                p.key("ctrl+left");
            } else {
                p.text_input("q");
            }
        });
        worst = worst.max(h + frame(&mut p, "long field key"));
    }
    println!("text field of {} bytes: first frame {first:.2} ms, worst keystroke (handler + frame) {worst:.2} ms", max_field - 64);
    assert_eq!(p.handler_errors, 0);
}

#[test]
fn gallery_under_default_limits() {
    let pkg = anypage_ui::gallery().expect("gallery");
    assert_eq!(pkg.manifest.limits.call_ms, 50.0);
    assert_eq!(pkg.manifest.limits.handler_ms, 50.0);
    let mut p = ScriptPage::load(&pkg, &natives(), text(), wake()).unwrap_or_else(|e| panic!("{e}"));
    for panel in [0, 1] {
        let c = script_cell(&p, panel);
        assert!(matches!(p.parts().0.demand(c).status, Status::Ready));
    }
    // use the dark app: buttons, fields, select, list, table, scroll, dialog
    for id in ["primary", "agree", "wifi"] {
        click_id(&mut p, "dark", id);
    }
    let d = describe(&mut p, "dark");
    let (x, y) = center(&node(&d, "name"));
    p.pointer_down(x, y);
    p.pointer_up();
    p.text_input("Grace Hopper");
    assert!(p.key("ctrl+a"));
    assert!(p.key("ctrl+c"));
    for id in ["list", "table", "scroll", "notes"] {
        let (x, y) = center(&node(&d, id));
        p.wheel(x, y, 0.0, -3.0);
    }
    click_id(&mut p, "dark", "fruit");
    assert!(p.key("down"));
    assert!(p.key("enter"));
    click_id(&mut p, "dark", "open-dialog");
    assert!(p.key("tab"));
    assert!(p.key("escape"));
    let c = script_cell(&p, 0);
    assert!(matches!(p.parts().0.demand(c).status, Status::Ready));
    assert_eq!(state(&mut p, "dark").get("name").as_str(), Some("Grace Hopper"));
    assert_eq!(state(&mut p, "dark").get("fruit").num(), Some(4.0));
    assert_eq!(p.handler_errors, 0);
}

const LIST: &str = r#"
local ui = import("ui")
local app
app = ui.app({ name = "big", rect = { 0, 0, 800, 900 }, agent = true, state = {},
    view = function(s)
        return ui.column({ padding = 10, gap = 8 }, {
            ui.list({ id = "list", count = 100000, grow = 1, row_height = 28, selected = s.sel,
                row = function(item, i) return ui.row({ gap = 8 }, { ui.text("Row " .. i), ui.spacer(), ui.text(tostring(i * 7 % 1000)) }) end,
                on_select = function(item, i) app:set("sel", i) end }),
        })
    end })
"#;

#[test]
fn list_and_form_under_default_limits() {
    let mut p = load_pkg(&default_manifest(800.0, 900.0), LIST).unwrap_or_else(|e| panic!("{e}"));
    frame(&mut p, "list first frame");
    let (x, y) = center(&node(&describe(&mut p, "big"), "list"));
    for k in 0..30 {
        assert!(p.wheel(x, y, 0.0, -20.0));
        frame(&mut p, &format!("list wheel {k}"));
    }
    click(&mut p, x, y);
    assert!(p.key("end"));
    frame(&mut p, "list end");
    assert_eq!(state(&mut p, "big").get("sel").num(), Some(100000.0));
    assert_eq!(p.handler_errors, 0);

    let form = r#"
local ui = import("ui")
local app
app = ui.app({ name = "form", rect = { 0, 0, 1000, 3000 }, agent = true, state = { v = {} },
    view = function(s)
        local rows = {}
        for i = 1, 50 do
            local k = tostring(i)
            rows[i] = ui.row({ gap = 8 }, {
                ui.text("Field " .. i, { width = 90 }),
                ui.textfield({ id = "f" .. i, value = s.v[k] or "", grow = 1,
                    on_change = function(v) app:update(function(st) st.v[k] = v end) end }),
                ui.checkbox("on", { id = "c" .. i }),
                ui.button("Go", { id = "b" .. i, size = "small" }),
            })
        end
        return ui.column({ padding = 16, gap = 6 }, rows)
    end })
"#;
    let mut p = load_pkg(&default_manifest(1000.0, 3000.0), form).unwrap_or_else(|e| panic!("{e}"));
    frame(&mut p, "form first frame");
    let (x, y) = center(&node(&describe(&mut p, "form"), "f7"));
    click(&mut p, x, y);
    for ch in "default limits".chars() {
        p.text_input(&ch.to_string());
        frame(&mut p, "form keystroke");
    }
    assert_eq!(state(&mut p, "form").get("v").get("7").as_str(), Some("default limits"));
    assert_eq!(p.handler_errors, 0);
}
