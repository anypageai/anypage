//! Frame cost of view + layout + draw for a 100 000-row list and a
//! 200-element form, measured on the panel's script cell (what the engine
//! evaluates per change) and inside Luau (app:stats()). Results are printed
//! and written to target/test-output/perf-*.txt.
//!
//! The bounds are loose (they catch regressions of an order of magnitude,
//! not noise); the measured numbers are the report. Take them from a
//! single-threaded run (`cargo test --test perf -- --test-threads=1`):
//! tests running side by side compete for cores and memory bandwidth.

mod common;

use std::fmt::Write as _;
use std::time::Instant;

use anypage_engine::Page;
use anypage_runtime::{Data, ScriptPage};
use common::*;

struct Sample {
    name: String,
    ms: Vec<f64>,
}

impl Sample {
    fn median(&self) -> f64 {
        let mut v = self.ms.clone();
        v.sort_by(|a, b| a.total_cmp(b));
        v[v.len() / 2]
    }
    fn min(&self) -> f64 {
        self.ms.iter().cloned().fold(f64::INFINITY, f64::min)
    }
    fn max(&self) -> f64 {
        self.ms.iter().cloned().fold(0.0, f64::max)
    }
}

fn report(lines: &mut String, s: &Sample) {
    let line = format!(
        "{:<58} min {:7.3}   median {:7.3}   max {:7.3} ms   ({} frames)",
        s.name,
        s.min(),
        s.median(),
        s.max(),
        s.ms.len()
    );
    println!("{line}");
    let _ = writeln!(lines, "{line}");
}

fn save(name: &str, text: &str) {
    let path = out_dir().join(name);
    let _ = std::fs::write(path, text);
}

/// Time one evaluation of the panel's display list (view + layout + draw +
/// conversion of inputs and of the display list).
fn frame_ms(p: &mut ScriptPage) -> f64 {
    let sc = script_cell(p, 0);
    let t = Instant::now();
    let g = p.parts().0;
    let i = g.demand(sc);
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    assert!(matches!(i.status, anypage_core::Status::Ready), "{:?}", i.status);
    ms
}

fn lua_stats(p: &mut ScriptPage) -> Data {
    act(p, "stats", Data::Nil)
}

const LIST_LAZY: &str = r#"
local ui = import("ui")
local app
app = ui.app({ name = "big", rect = { 0, 0, 800, 900 }, agent = true, state = {},
    view = function(s)
        return ui.column({ padding = 10, gap = 8 }, {
            ui.text("100 000 rows"),
            ui.list({ id = "list", count = 100000, grow = 1, row_height = 28, selected = s.sel,
                item = function(i) return i end,
                row = function(item, i, selected)
                    return ui.row({ gap = 8 }, { ui.text("Row " .. i), ui.spacer(), ui.text(tostring(i * 7 % 1000), { tone = "dim" }) })
                end,
                on_select = function(item, i) app:set("sel", i) end }),
        })
    end })
page.action({ name = "stats", run = function() return app:stats() end })
"#;

// Items that live in a cell reach the view converted into Luau tables on every
// frame (the runtime's input conversion), so this case measures that cost on
// 20 000 items; 100 000 took about 0.7 s per frame on the development machine.
const CELL_ITEMS: usize = 20_000;
const LIST_CELL: &str = r#"
local ui = import("ui")
local t = {}
for i = 1, 20000 do t[i] = "Item " .. i end
local items = page.input("items", t)
local app
app = ui.app({ name = "big", rect = { 0, 0, 800, 900 }, agent = true, state = {}, inputs = { items },
    view = function(s, items)
        return ui.column({ padding = 10 }, {
            ui.list({ id = "list", items = items, grow = 1, row_height = 28, selected = s.sel,
                on_select = function(item, i) app:set("sel", i) end }),
        })
    end })
page.action({ name = "stats", run = function() return app:stats() end })
"#;

fn list_frames(src: &str, label: &str, rows: usize, frames: usize, lines: &mut String) -> (Sample, Sample) {
    let mut p = load_pkg(&manifest(800.0, 900.0, ""), src).unwrap_or_else(|e| panic!("{e}"));
    let first = frame_ms(&mut p);
    let line = format!("{label}: first frame {first:.3} ms");
    println!("{line}");
    let _ = writeln!(lines, "{line}");
    let d = describe(&mut p, "big");
    let (cx, cy) = center(&node(&d, "list"));
    let mut scroll = Sample { name: format!("{label}: wheel scroll (view+layout+draw)"), ms: Vec::new() };
    let mut lua = Sample { name: format!("{label}:   of which in Luau (view+layout+draw)"), ms: Vec::new() };
    for k in 0..frames {
        assert!(p.wheel(cx, cy, 0.0, if k < frames * 2 / 3 { -5.0 } else { 3.0 }));
        scroll.ms.push(frame_ms(&mut p));
        let st = lua_stats(&mut p);
        lua.ms.push(st.get("view_ms").num_or(0.0) + st.get("layout_ms").num_or(0.0) + st.get("draw_ms").num_or(0.0));
    }
    let st = lua_stats(&mut p);
    let line = format!(
        "{label}: last frame in Luau: view {:.3} ms, layout {:.3} ms, draw {:.3} ms, {} elements, {} commands",
        st.get("view_ms").num_or(0.0),
        st.get("layout_ms").num_or(0.0),
        st.get("draw_ms").num_or(0.0),
        st.get("elements").num_or(0.0),
        st.get("commands").num_or(0.0)
    );
    println!("{line}");
    let _ = writeln!(lines, "{line}");
    // keyboard navigation to the far end is one frame, not a walk over the rows
    assert!(p.run_action("big.do", &Data::map([("id", Data::str("list")), ("op", Data::str("focus"))])).is_ok());
    let t = Instant::now();
    assert!(p.key("end"));
    let key_ms = t.elapsed().as_secs_f64() * 1000.0;
    let end_ms = frame_ms(&mut p);
    let line = format!("{label}: End key handler {key_ms:.3} ms, frame after it {end_ms:.3} ms");
    println!("{line}");
    let _ = writeln!(lines, "{line}");
    assert!(has_text_containing(&display(&mut p, 0), &rows.to_string()));
    assert!(st.get("elements").num_or(1e9) < 200.0, "the list built more than its visible rows");
    assert_eq!(p.handler_errors, 0);
    // rasterising is the runtime's work; report it once
    let rc = raster_cell(&p, 0);
    let t = Instant::now();
    p.parts().0.demand(rc);
    let line = format!("{label}: rasterise 800x900 panel {:.3} ms", t.elapsed().as_secs_f64() * 1000.0);
    println!("{line}");
    let _ = writeln!(lines, "{line}");
    (scroll, lua)
}

#[test]
fn list_of_100k_rows() {
    let mut lines = String::new();
    let (scroll, lua) = list_frames(LIST_LAZY, "100k list, rows on demand", 100_000, 60, &mut lines);
    report(&mut lines, &scroll);
    report(&mut lines, &lua);
    assert!(scroll.median() < 25.0, "100k list frame {} ms", scroll.median());
    let label = format!("{}k list, items from a cell", CELL_ITEMS / 1000);
    let (scroll2, lua2) = list_frames(LIST_CELL, &label, CELL_ITEMS, 12, &mut lines);
    report(&mut lines, &scroll2);
    report(&mut lines, &lua2);
    let per_item_us = (scroll2.median() - lua2.median()).max(0.0) * 1000.0 / CELL_ITEMS as f64;
    let line = format!("{label}: input conversion outside Luau ~{per_item_us:.2} us per item per frame");
    println!("{line}");
    lines.push_str(&line);
    lines.push('\n');
    // converting every item into the script for every frame is the runtime's cost
    assert!(scroll2.median() < 400.0, "cell list frame {} ms", scroll2.median());
    save("perf-list.txt", &lines);
}

#[test]
fn gallery_frames() {
    let mut lines = String::new();
    let pkg = anypage_ui::gallery().expect("gallery");
    let mut p = ScriptPage::load(&pkg, &natives(), text(), wake()).unwrap_or_else(|e| panic!("{e}"));
    let line = format!("gallery: load {:.3} ms", p.load_ms);
    println!("{line}");
    lines.push_str(&line);
    lines.push('\n');
    // the page has its own text system (cold glyph cache), as at app start
    let first = frame_ms(&mut p);
    let mut s = Sample { name: "gallery (dark app): frame after a click".into(), ms: Vec::new() };
    let d = describe(&mut p, "dark");
    let (x, y) = center(&node(&d, "primary"));
    for _ in 0..20 {
        click(&mut p, x, y);
        s.ms.push(frame_ms(&mut p));
    }
    let line = format!("gallery (dark app): first frame {first:.3} ms");
    println!("{line}");
    lines.push_str(&line);
    lines.push('\n');
    report(&mut lines, &s);
    assert!(s.median() < 60.0, "gallery frame {} ms", s.median());
    save("perf-gallery.txt", &lines);
}

const FORM: &str = r#"
local ui = import("ui")
local app
app = ui.app({ name = "form", rect = { 0, 0, 1000, 3000 }, agent = true, state = { v = {}, c = {} },
    view = function(s)
        local rows = {}
        for i = 1, 50 do
            local k = tostring(i)
            rows[i] = ui.row({ gap = 8 }, {
                ui.text("Field " .. i, { width = 90 }),
                ui.textfield({ id = "f" .. i, value = s.v[k] or "", grow = 1, placeholder = "value " .. i,
                    on_change = function(v) app:update(function(st) st.v[k] = v end) end }),
                ui.checkbox("on", { id = "c" .. i, checked = s.c[k] == true,
                    on_change = function(v) app:update(function(st) st.c[k] = v end) end }),
                ui.button("Go", { id = "b" .. i, size = "small" }),
            })
        end
        return ui.column({ padding = 16, gap = 6 }, rows)
    end })
page.action({ name = "stats", run = function() return app:stats() end })
"#;

#[test]
fn form_of_200_elements() {
    let mut lines = String::new();
    let mut p = load_pkg(&manifest(1000.0, 3000.0, ""), FORM).unwrap_or_else(|e| panic!("{e}"));
    let first = frame_ms(&mut p);
    let st = lua_stats(&mut p);
    let line = format!(
        "form: first frame {first:.3} ms; {} elements ({} controls), {} commands",
        st.get("elements").num_or(0.0),
        200,
        st.get("commands").num_or(0.0)
    );
    println!("{line}");
    let _ = writeln!(lines, "{line}");
    assert!(st.get("elements").num_or(0.0) >= 250.0);
    // focus field 1 and type: each keystroke is a handler + one frame
    let d = describe(&mut p, "form");
    let (x, y) = center(&node(&d, "f1"));
    p.pointer_down(x, y);
    p.pointer_up();
    frame_ms(&mut p);
    let mut handler = Sample { name: "form: keystroke handler (text event)".into(), ms: Vec::new() };
    let mut frame = Sample { name: "form: frame after a keystroke (view+layout+draw)".into(), ms: Vec::new() };
    let mut lua = Sample { name: "form:   of which in Luau (view+layout+draw)".into(), ms: Vec::new() };
    for ch in "the quick brown fox jumps over".chars() {
        let t = Instant::now();
        p.text_input(&ch.to_string());
        handler.ms.push(t.elapsed().as_secs_f64() * 1000.0);
        frame.ms.push(frame_ms(&mut p));
        let st = lua_stats(&mut p);
        lua.ms.push(st.get("view_ms").num_or(0.0) + st.get("layout_ms").num_or(0.0) + st.get("draw_ms").num_or(0.0));
    }
    assert_eq!(state(&mut p, "form").get("v").get("1").as_str(), Some("the quick brown fox jumps over"));
    // hover across many controls
    let mut hover = Sample { name: "form: hover change (handler + frame)".into(), ms: Vec::new() };
    for i in 1..=30 {
        let (bx, by) = center(&node(&d, &format!("b{i}")));
        let t = Instant::now();
        p.pointer_move(bx, by);
        frame_ms(&mut p);
        hover.ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    for s in [&handler, &frame, &lua, &hover] {
        report(&mut lines, s);
    }
    let st = lua_stats(&mut p);
    let line = format!(
        "form: last frame in Luau: view {:.3} ms, layout {:.3} ms, draw {:.3} ms",
        st.get("view_ms").num_or(0.0),
        st.get("layout_ms").num_or(0.0),
        st.get("draw_ms").num_or(0.0)
    );
    println!("{line}");
    let _ = writeln!(lines, "{line}");
    assert!(frame.median() < 40.0, "form frame {} ms", frame.median());
    assert!(handler.median() < 40.0, "keystroke handler {} ms", handler.median());
    assert_eq!(p.handler_errors, 0);
    save("perf-form.txt", &lines);
}
