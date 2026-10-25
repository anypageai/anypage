//! Buttons, toggles, radio, slider, select, tabs, dialogs, toasts, tooltips
//! and keyboard focus, driven through real page events.

mod common;

use anypage_engine::{Page, PointerResponse};
use anypage_runtime::{Data, ScriptPage};
use common::*;

const PAGE: &str = r#"
local ui = import("ui")
local app
app = ui.app({
    name = "c", rect = { 100, 50, 800, 700 }, agent = true,
    state = { count = 0, on = false, sw = false, r = 1, v = 5, tab = 1, dlg = false },
    view = function(s)
        return ui.column({ gap = 8, padding = 16 }, {
            ui.row({ gap = 8 }, {
                ui.button("Add", { id = "add", tooltip = "Add one", on_click = function()
                    app:update(function(st) st.count += 1 end)
                end }),
                ui.button("Off", { id = "off", disabled = true, on_click = function() app:set("count", -100) end }),
                ui.text("count " .. s.count, { id = "count" }),
            }),
            ui.checkbox("Check", { id = "check", checked = s.on, on_change = function(v) app:set("on", v) end }),
            ui.switch("Switch", { id = "switch", checked = s.sw, on_change = function(v, a) a:set("sw", v) end }),
            ui.radio({ id = "radio", options = { "a", "b", "c" }, selected = s.r, on_change = function(i) app:set("r", i) end }),
            ui.slider({ id = "slider", value = s.v, min = 0, max = 10, step = 1, width = 400, fill = false,
                on_change = function(v) app:set("v", v) end }),
            ui.select({ id = "select", options = { "one", "two", "three" }, selected = s.sel, placeholder = "pick",
                width = 300, fill = false, on_change = function(i) app:set("sel", i) end }),
            ui.tabs({ id = "tabs", tabs = { "A", "B", "C" }, selected = s.tab, on_change = function(i) app:set("tab", i) end }),
            ui.row({ gap = 8 }, {
                ui.button("Dialog", { id = "open", on_click = function(a) a:set("dlg", true) end }),
                ui.button("Toast", { id = "toast", on_click = function(a) a:toast("hello toast", { duration = 2 }) end }),
            }),
            ui.dialog({ id = "dlg", open = s.dlg, title = "Sure?", body = "body text",
                actions = {
                    ui.button("No", { id = "no", variant = "secondary", on_click = function(a) a:set("dlg", false) end }),
                    ui.button("Yes", { id = "yes", on_click = function(a)
                        a:update(function(st) st.dlg = false st.count = 1000 end)
                    end }),
                },
                on_close = function(a) a:set("dlg", false) end }),
        })
    end,
})
"#;

fn page() -> ScriptPage {
    load(PAGE)
}

fn num(p: &mut ScriptPage, key: &str) -> f64 {
    state(p, "c").get(key).num().unwrap_or(f64::NAN)
}

// dark theme tokens
fn accent() -> f64 {
    rgba(92, 152, 255, 255)
}
fn accent_press() -> f64 {
    rgba(64, 124, 228, 255)
}

#[test]
fn button_click_press_state_and_disabled() {
    let mut p = page();
    let d = describe(&mut p, "c");
    let add = node(&d, "add");
    assert_eq!(add.get("label").as_str(), Some("Add"));
    let (x, y) = center(&add);
    // the button sits inside the app's rect (world coordinates)
    assert!(x > 100.0 && y > 50.0);
    assert!(!rects_of_color(&display(&mut p, 0), accent()).is_empty(), "primary button colour");

    assert_eq!(p.pointer_down(x, y), PointerResponse::Captured);
    assert_eq!(ui_state(&mut p, "c").get("press").as_str(), Some("add"));
    assert!(!rects_of_color(&display(&mut p, 0), accent_press()).is_empty(), "pressed colour while held");
    p.pointer_up();
    assert_eq!(num(&mut p, "count"), 1.0);
    assert!(ui_state(&mut p, "c").get("press").is_nil());
    assert!(has_text(&display(&mut p, 0), "count 1"));

    // dragging off the button before release cancels the click
    p.pointer_down(x, y);
    p.pointer_drag(x + 500.0, y);
    assert!(ui_state(&mut p, "c").get("press").is_nil());
    p.pointer_up();
    assert_eq!(num(&mut p, "count"), 1.0);

    // a disabled button does nothing
    let (ox, oy) = center(&node(&d, "off"));
    click(&mut p, ox, oy);
    assert_eq!(num(&mut p, "count"), 1.0);
    assert_eq!(node(&d, "off").get("disabled").as_bool(), Some(true));

    // empty space in the app is not consumed (the engine pans)
    assert_eq!(p.pointer_down(100.0 + 790.0, 50.0 + 690.0), PointerResponse::Ignored);
    p.pointer_up();
    assert_eq!(p.handler_errors, 0);
}

#[test]
fn keyboard_focus_and_activation() {
    let mut p = page();
    // nothing focused: Tab enters the app at its first focusable element
    assert!(p.key("tab"));
    let u = ui_state(&mut p, "c");
    assert_eq!(u.get("focus").as_str(), Some("add"));
    assert_eq!(u.get("kb").as_bool(), Some(true));
    // the focus ring is drawn in the theme's focus colour
    let ring = rgba(120, 172, 255, 255);
    assert!(commands(&display(&mut p, 0), "stroke").iter().any(|s| s.at(8).num() == Some(ring)), "focus ring");
    assert!(p.key("enter"));
    assert!(p.key("space"));
    assert_eq!(num(&mut p, "count"), 2.0);
    // the disabled button is skipped
    assert!(p.key("tab"));
    assert_eq!(ui_state(&mut p, "c").get("focus").as_str(), Some("check"));
    assert!(p.key("space"));
    assert_eq!(state(&mut p, "c").get("on").as_bool(), Some(true));
    assert!(p.key("tab"));
    assert!(p.key("enter"));
    assert_eq!(state(&mut p, "c").get("sw").as_bool(), Some(true));
    // Shift+Tab goes back
    assert!(p.key("shift+tab"));
    assert_eq!(ui_state(&mut p, "c").get("focus").as_str(), Some("check"));
    // the order wraps around: from the first back to the last
    assert!(p.key("shift+tab"));
    assert!(p.key("shift+tab"));
    assert_eq!(ui_state(&mut p, "c").get("focus").as_str(), Some("toast"));
    // Escape clears focus; keys are then not consumed
    assert!(p.key("escape"));
    assert!(ui_state(&mut p, "c").get("focus").is_nil());
    assert!(!p.key("x"));
    assert!(!p.key("enter"));
    assert_eq!(p.handler_errors, 0);
}

#[test]
fn toggles_radio_and_slider() {
    let mut p = page();
    click_id(&mut p, "c", "check");
    assert_eq!(state(&mut p, "c").get("on").as_bool(), Some(true));
    click_id(&mut p, "c", "check");
    assert_eq!(state(&mut p, "c").get("on").as_bool(), Some(false));
    click_id(&mut p, "c", "switch");
    assert_eq!(state(&mut p, "c").get("sw").as_bool(), Some(true));

    // radio: the last option is at the bottom of the group
    let d = describe(&mut p, "c");
    let r = node(&d, "radio");
    let (rx, ry) = (r.get("x").num_or(0.0) as f32 + 9.0, (r.get("y").num_or(0.0) + r.get("h").num_or(0.0)) as f32 - 5.0);
    click(&mut p, rx, ry);
    assert_eq!(num(&mut p, "r"), 3.0);
    // focused now (clicked): arrows move the selection and wrap
    assert_eq!(ui_state(&mut p, "c").get("focus").as_str(), Some("radio"));
    assert!(p.key("down"));
    assert_eq!(num(&mut p, "r"), 1.0);
    assert!(p.key("up"));
    assert_eq!(num(&mut p, "r"), 3.0);

    // slider: press at the ends of the track, drag, keys
    let s = node(&d, "slider");
    let (sx, sy, sw) = (s.get("x").num_or(0.0) as f32, center(&s).1, s.get("w").num_or(0.0) as f32);
    assert_eq!(p.pointer_down(sx + sw - 1.0, sy), PointerResponse::Captured);
    assert_eq!(num(&mut p, "v"), 10.0);
    p.pointer_drag(sx + sw / 2.0, sy);
    assert_eq!(num(&mut p, "v"), 5.0);
    p.pointer_drag(sx - 300.0, sy);
    assert_eq!(num(&mut p, "v"), 0.0);
    p.pointer_up();
    assert!(p.key("right"));
    assert!(p.key("right"));
    assert_eq!(num(&mut p, "v"), 2.0);
    assert!(p.key("end"));
    assert_eq!(num(&mut p, "v"), 10.0);
    assert!(p.key("pagedown"));
    assert_eq!(num(&mut p, "v"), 9.0);
    assert!(p.key("home"));
    assert_eq!(num(&mut p, "v"), 0.0);
    assert_eq!(p.handler_errors, 0);
}

#[test]
fn select_menu_by_pointer_and_keys() {
    let mut p = page();
    assert!(has_text(&display(&mut p, 0), "pick"));
    click_id(&mut p, "c", "select");
    assert_eq!(ui_state(&mut p, "c").get("open").as_str(), Some("select"));
    let cmds = display(&mut p, 0);
    for o in ["one", "two", "three"] {
        assert!(has_text(&cmds, o), "menu option {o}");
    }
    // pick "two" from the menu
    let d = describe(&mut p, "c");
    let menu = d.get("nodes").items().iter().find(|n| n.get("type").as_str() == Some("menu")).cloned().expect("menu node");
    let ih = menu.get("item_h").num_or(0.0);
    let (mx, my) = (menu.get("x").num_or(0.0) + 20.0, menu.get("y").num_or(0.0) + 4.0 + ih * 1.5);
    click(&mut p, mx as f32, my as f32);
    assert_eq!(num(&mut p, "sel"), 2.0);
    assert!(ui_state(&mut p, "c").get("open").is_nil());
    assert!(has_text(&display(&mut p, 0), "two"));

    // keyboard: the select is focused; Enter opens, Down + Enter picks
    assert_eq!(ui_state(&mut p, "c").get("focus").as_str(), Some("select"));
    assert!(p.key("enter"));
    assert_eq!(ui_state(&mut p, "c").get("open_hi").num(), Some(2.0));
    assert!(p.key("down"));
    assert!(p.key("enter"));
    assert_eq!(num(&mut p, "sel"), 3.0);
    // closed: Up changes the value directly
    assert!(p.key("up"));
    assert_eq!(num(&mut p, "sel"), 2.0);
    // Escape closes an open menu without choosing
    assert!(p.key("space"));
    assert!(p.key("down"));
    assert!(p.key("escape"));
    assert!(ui_state(&mut p, "c").get("open").is_nil());
    assert_eq!(num(&mut p, "sel"), 2.0);
    // a click outside an open menu closes it and is consumed
    assert!(p.key("enter"));
    assert_eq!(click(&mut p, 880.0, 700.0), PointerResponse::Handled);
    assert!(ui_state(&mut p, "c").get("open").is_nil());
    assert_eq!(p.handler_errors, 0);
}

#[test]
fn tabs_by_keys_and_pointer() {
    let mut p = page();
    assert!(p.run_action("c.do", &Data::map([("id", Data::str("tabs")), ("op", Data::str("focus"))])).is_ok());
    assert!(p.key("right"));
    assert_eq!(num(&mut p, "tab"), 2.0);
    assert!(p.key("end"));
    assert_eq!(num(&mut p, "tab"), 3.0);
    assert!(p.key("right"));
    assert_eq!(num(&mut p, "tab"), 1.0);
    assert!(p.key("left"));
    assert_eq!(num(&mut p, "tab"), 3.0);
    let t = node(&describe(&mut p, "c"), "tabs");
    click(&mut p, t.get("x").num_or(0.0) as f32 + 8.0, center(&t).1);
    assert_eq!(num(&mut p, "tab"), 1.0);
}

#[test]
fn modal_dialog_traps_focus_and_closes() {
    let mut p = page();
    click_id(&mut p, "c", "open");
    assert_eq!(state(&mut p, "c").get("dlg").as_bool(), Some(true));
    let cmds = display(&mut p, 0);
    assert!(has_text(&cmds, "Sure?") && has_text(&cmds, "body text"));
    let scrim = rgba(0, 0, 0, 150);
    assert!(!rects_of_color(&cmds, scrim).is_empty(), "scrim over the app");
    let d = describe(&mut p, "c");
    assert!(d.get("nodes").items().iter().any(|n| n.get("type").as_str() == Some("dialog")));
    // Tab cycles inside the dialog only
    let mut seen = Vec::new();
    for _ in 0..4 {
        assert!(p.key("tab"));
        seen.push(ui_state(&mut p, "c").get("focus").as_str().unwrap_or("").to_string());
    }
    assert_eq!(seen, ["no", "yes", "no", "yes"]);
    // controls behind the dialog are unreachable: a click there hits the scrim and closes it
    let (ax, ay) = center(&node(&d, "add"));
    assert_eq!(click(&mut p, ax, ay), PointerResponse::Handled);
    assert_eq!(state(&mut p, "c").get("dlg").as_bool(), Some(false));
    assert_eq!(num(&mut p, "count"), 0.0);
    // Escape closes it too
    click_id(&mut p, "c", "open");
    assert!(p.key("escape"));
    assert_eq!(state(&mut p, "c").get("dlg").as_bool(), Some(false));
    // its buttons work
    click_id(&mut p, "c", "open");
    click_id(&mut p, "c", "yes");
    assert_eq!(num(&mut p, "count"), 1000.0);
    assert_eq!(state(&mut p, "c").get("dlg").as_bool(), Some(false));
    assert!(!has_text(&display(&mut p, 0), "Sure?"));
    assert_eq!(p.handler_errors, 0);
}

#[test]
fn toast_expires_by_declared_time() {
    let mut p = page();
    tick(&mut p, 1.0);
    click_id(&mut p, "c", "toast");
    assert!(has_text(&display(&mut p, 0), "hello toast"));
    let sc = script_cell(&p, 0);
    // the panel declared it stays valid until the toast ends, and no longer
    // (3 s after the click: the clock read 1.0 plus the real time since then)
    let e = p.graph().expires(sc);
    assert!((3.0..3.25).contains(&e), "{e}");
    tick(&mut p, 2.0);
    assert!(has_text(&display(&mut p, 0), "hello toast"));
    tick(&mut p, 3.5);
    assert!(!has_text(&display(&mut p, 0), "hello toast"));
    assert_eq!(p.graph().expires(sc), f64::INFINITY);
}

#[test]
fn tooltip_after_hover_delay() {
    let mut p = page();
    tick(&mut p, 10.0);
    let (x, y) = center(&node(&describe(&mut p, "c"), "add"));
    p.pointer_move(x, y);
    let u = ui_state(&mut p, "c");
    assert_eq!(u.get("hover").as_str(), Some("add"));
    assert_eq!(u.get("tip").as_str(), Some("add"));
    assert!(!has_text(&display(&mut p, 0), "Add one"));
    let e = p.graph().expires(script_cell(&p, 0));
    assert!((10.5..10.55).contains(&e), "{e}");
    tick(&mut p, 10.6);
    assert!(has_text(&display(&mut p, 0), "Add one"));
    // hovering a darker variant: the hover colour is drawn
    assert!(!rects_of_color(&display(&mut p, 0), rgba(120, 172, 255, 255)).is_empty());
    // leaving the app clears hover and the tooltip
    p.pointer_move(5.0, 5.0);
    assert!(ui_state(&mut p, "c").get("hover").is_nil());
    assert!(!has_text(&display(&mut p, 0), "Add one"));
}

#[test]
fn hover_moves_do_not_redraw_when_nothing_changes() {
    let mut p = page();
    let sc = script_cell(&p, 0);
    display(&mut p, 0);
    let v0 = p.graph().version(sc);
    let d = describe(&mut p, "c");
    let (x, y) = center(&node(&d, "count"));
    // over plain text: no hover target, no state change, no redraw
    p.pointer_move(x, y);
    p.pointer_move(x + 1.0, y);
    display(&mut p, 0);
    assert_eq!(p.graph().version(sc), v0);
    // over a button: one change
    let (bx, by) = center(&node(&d, "add"));
    p.pointer_move(bx, by);
    p.pointer_move(bx + 1.0, by);
    display(&mut p, 0);
    assert_eq!(p.graph().version(sc), v0 + 1);
}

#[test]
fn agent_operations() {
    let mut p = page();
    let r: serde_json::Value = serde_json::from_str(&p.act(r#"{"action": "c.do", "params": {"id": "add", "op": "click"}}"#)).unwrap();
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(num(&mut p, "count"), 1.0);
    p.act(r#"{"action": "c.do", "params": {"id": "slider", "op": "set", "value": 7}}"#);
    assert_eq!(num(&mut p, "v"), 7.0);
    p.act(r#"{"action": "c.do", "params": {"id": "select", "op": "set", "value": 3}}"#);
    assert_eq!(num(&mut p, "sel"), 3.0);
    let r: serde_json::Value = serde_json::from_str(&p.act(r#"{"action": "c.do", "params": {"id": "off", "op": "click"}}"#)).unwrap();
    assert!(r["error"].as_str().unwrap_or("").contains("disabled"), "{r}");
    let r: serde_json::Value = serde_json::from_str(&p.act(r#"{"action": "c.do", "params": {"id": "nope"}}"#)).unwrap();
    assert!(r["error"].as_str().unwrap_or("").contains("no element"), "{r}");
    // the description an agent reads
    let r: serde_json::Value = serde_json::from_str(&p.act(r#"{"action": "c.describe"}"#)).unwrap();
    let nodes = r["result"]["nodes"].as_array().expect("nodes");
    assert!(nodes.iter().any(|n| n["id"] == "slider" && n["value"] == 7));
    // the state cell is agent-writable
    let r: serde_json::Value = serde_json::from_str(&p.act(r#"{"set": "c.state", "value": {"count": 5, "r": 2, "v": 1, "tab": 2}}"#)).unwrap();
    assert_eq!(r["ok"], true);
    assert!(has_text(&display(&mut p, 0), "count 5"));
}

#[test]
fn two_apps_share_the_keyboard_correctly() {
    let src = r#"
local ui = import("ui")
local function make(name, x)
    return ui.app({ name = name, rect = { x, 0, 400, 300 }, agent = true, state = { n = 0 },
        view = function(s)
            return ui.column({ padding = 10 }, {
                ui.button("+" .. s.n, { id = "b", on_click = function(a) a:update(function(st) st.n += 1 end) end }),
            })
        end })
end
make("left", 0)
make("right", 500)
"#;
    let mut p = load(src);
    click_id(&mut p, "right", "b");
    assert_eq!(ui_state(&mut p, "right").get("focus").as_str(), Some("b"));
    // keys go to the app that has focus
    assert!(p.key("enter"));
    assert_eq!(state(&mut p, "right").get("n").num(), Some(2.0));
    assert_eq!(state(&mut p, "left").get("n").num(), Some(0.0));
    // clicking into the other app moves focus there
    click_id(&mut p, "left", "b");
    assert!(ui_state(&mut p, "right").get("focus").is_nil());
    assert!(p.key("enter"));
    assert_eq!(state(&mut p, "left").get("n").num(), Some(2.0));
    assert_eq!(state(&mut p, "right").get("n").num(), Some(2.0));
    // a click outside every app blurs
    assert_eq!(click(&mut p, 450.0, 100.0), PointerResponse::Ignored);
    assert!(ui_state(&mut p, "left").get("focus").is_nil());
    assert!(!p.key("enter"));
    // each app has its own panel and redraws on its own
    assert_eq!(p.spec().panels.len(), 2);
}
