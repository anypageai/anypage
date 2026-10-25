//! Time in handlers when frames are made the way the engine makes them: the
//! page clock advances only when something visible is due, so after a quiet
//! spell `page.now()` is seconds old. Toasts, tooltip delays, caret blink
//! and double clicks must still follow real time. These tests wait for real
//! (about two seconds in all) instead of setting the clock by hand.

mod common;

use anypage_engine::Page;
use anypage_runtime::ScriptPage;
use common::*;

const PAGE: &str = r#"
local ui = import("ui")
local app
app = ui.app({ name = "c", rect = { 0, 0, 600, 500 }, agent = true, state = { acts = 0, f = "" },
    theme = ui.theme({ tooltip_delay = 0.3 }),
    view = function(s)
        return ui.column({ padding = 10, gap = 8 }, {
            ui.button("Toast", { id = "toast", on_click = function(a) a:toast("hello toast", { duration = 0.4 }) end }),
            ui.button("Tip", { id = "tip", tooltip = "a tooltip" }),
            ui.textfield({ id = "f", value = s.f, on_change = function(v) app:set("f", v) end }),
            ui.list({ id = "list", count = 50, height = 200, selected = s.sel,
                on_select = function(_, i) app:set("sel", i) end,
                on_activate = function() app:update(function(st) st.acts += 1 end) end }),
        })
    end })
"#;

fn page() -> ScriptPage {
    load_pkg(&default_manifest(600.0, 500.0), PAGE).unwrap_or_else(|e| panic!("{e}"))
}

/// The page clock the handlers see: the last tick.
fn page_clock(p: &ScriptPage) -> f64 {
    p.graph().clock_now()
}

#[test]
fn a_toast_after_a_quiet_spell_lasts_its_duration() {
    let mut p = page();
    let f = Frames::new();
    f.frame(&mut p);
    let toast = center(&node(&describe(&mut p, "c"), "toast"));
    f.wait(1.0); // nothing due: no frames, the page clock stays put
    assert!(f.now() - page_clock(&p) > 0.9);
    click(&mut p, toast.0, toast.1);
    f.frame(&mut p);
    assert!(has_text(&display(&mut p, 0), "hello toast"));
    // the next frames keep it: it ends 0.4 s after the click, not 0.4 s after the old clock
    f.frame(&mut p);
    assert!(has_text(&display(&mut p, 0), "hello toast"), "the toast vanished on the frame after it appeared");
    f.wait(0.15);
    f.frame(&mut p);
    assert!(has_text(&display(&mut p, 0), "hello toast"));
    f.wait(0.35);
    f.frame(&mut p);
    assert!(!has_text(&display(&mut p, 0), "hello toast"));
    assert_eq!(p.graph().expires(script_cell(&p, 0)), f64::INFINITY);
    assert_eq!(p.handler_errors, 0);
}

#[test]
fn a_tooltip_waits_its_delay_after_a_quiet_spell() {
    let mut p = page();
    let f = Frames::new();
    f.frame(&mut p);
    let tip = center(&node(&describe(&mut p, "c"), "tip"));
    f.wait(1.0);
    p.pointer_move(tip.0, tip.1);
    f.frame(&mut p);
    assert!(!has_text(&display(&mut p, 0), "a tooltip"));
    f.frame(&mut p);
    assert!(!has_text(&display(&mut p, 0), "a tooltip"), "the tooltip skipped its delay");
    f.wait(0.4);
    f.frame(&mut p);
    assert!(has_text(&display(&mut p, 0), "a tooltip"));
}

#[test]
fn the_caret_is_on_right_after_focus_after_a_quiet_spell() {
    let mut p = page();
    let f = Frames::new();
    f.frame(&mut p);
    let field = center(&node(&describe(&mut p, "c"), "f"));
    f.wait(1.0);
    click(&mut p, field.0, field.1);
    for _ in 0..2 {
        f.frame(&mut p);
        let cmds = display(&mut p, 0);
        let carets = commands(&cmds, "rrect").into_iter().filter(|r| r.at(4).num() == Some(1.5)).count();
        assert_eq!(carets, 1, "the caret is off right after focusing");
    }
    // the next blink is half a period from now, not from the old clock
    let e = p.graph().expires(script_cell(&p, 0));
    assert!(e > f.now() + 0.3, "next blink at {e}, now {}", f.now());
}

#[test]
fn clicks_seconds_apart_are_not_a_double_click() {
    let mut p = page();
    let f = Frames::new();
    f.frame(&mut p);
    let d = describe(&mut p, "c");
    let n = node(&d, "list");
    let (x, y) = ((n.get("x").num_or(0.0) + 40.0) as f32, (n.get("y").num_or(0.0) + 40.0) as f32);
    click(&mut p, x, y);
    f.frame(&mut p);
    f.wait(0.6); // longer than the theme's double-click time (0.4 s)
    click(&mut p, x, y);
    f.frame(&mut p);
    assert_eq!(state(&mut p, "c").get("acts").num(), Some(0.0));
    // two quick clicks are
    click(&mut p, x, y);
    assert_eq!(state(&mut p, "c").get("acts").num(), Some(1.0));
    assert_eq!(p.handler_errors, 0);
}
