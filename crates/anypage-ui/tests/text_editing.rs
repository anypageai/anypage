//! Text fields and text areas: grapheme-correct carets, words, selection,
//! clipboard, IME preedit, undo, wrapping and scrolling, host text focus.

mod common;

use anypage_engine::{Page, PageRequest, PointerResponse};
use anypage_runtime::{Data, ScriptPage};
use common::*;

const PAGE: &str = r#"
local ui = import("ui")
local app
app = ui.app({
    name = "t", rect = { 50, 40, 700, 600 }, agent = true,
    state = { f = "", pw = "", area = "", ro = "fixed", short = "", submitted = "" },
    view = function(s)
        local function set(k) return function(v) app:set(k, v) end end
        return ui.column({ gap = 10, padding = 20 }, {
            ui.textfield({ id = "f", value = s.f, width = 300, fill = false, placeholder = "type here", on_change = set("f"),
                on_submit = function(v) app:set("submitted", v) end }),
            ui.textfield({ id = "pw", value = s.pw, password = true, on_change = set("pw") }),
            ui.textarea({ id = "area", value = s.area, rows = 3, width = 260, fill = false, on_change = set("area") }),
            ui.textfield({ id = "ro", value = s.ro, readonly = true, on_change = set("ro") }),
            ui.textfield({ id = "short", value = s.short, max_length = 5, on_change = set("short") }),
            ui.button("after", { id = "after" }),
        })
    end,
})
"#;

/// Under the runtime's default limits (50 ms per call and handler, 64 MB).
fn page() -> ScriptPage {
    load_pkg(&default_manifest(1000.0, 800.0), PAGE).unwrap_or_else(|e| panic!("{e}"))
}

fn val(p: &mut ScriptPage, key: &str) -> String {
    state(p, "t").get(key).as_str().unwrap_or("").to_string()
}

fn edit(p: &mut ScriptPage) -> (usize, usize) {
    let u = ui_state(p, "t");
    let e = u.get("edit");
    (e.get("caret").num_or(-1.0) as usize, e.get("anchor").num_or(-1.0) as usize)
}

fn focus(p: &mut ScriptPage, id: &str) {
    let d = describe(p, "t");
    let n = node(&d, id);
    // click near the right end so the caret lands after any text
    let x = (n.get("x").num_or(0.0) + n.get("w").num_or(0.0) - 6.0) as f32;
    let y = (n.get("y").num_or(0.0) + n.get("h").num_or(0.0) - 6.0) as f32;
    assert_eq!(p.pointer_down(x, y), PointerResponse::Captured);
    p.pointer_up();
    assert_eq!(ui_state(p, "t").get("focus").as_str(), Some(id));
}

fn typing(p: &mut ScriptPage, s: &str) {
    p.text_input(s);
}

#[test]
fn typing_graphemes_and_host_focus() {
    let mut p = page();
    assert!(p.text_focus().is_none());
    focus(&mut p, "f");
    // the host gets the caret rectangle inside the field (world coordinates)
    let f = node(&describe(&mut p, "t"), "f");
    let r = p.text_focus().expect("text focus while editing");
    let (fx, fy, fw, fh) = (f.get("x").num_or(0.0) as f32, f.get("y").num_or(0.0) as f32, f.get("w").num_or(0.0) as f32, f.get("h").num_or(0.0) as f32);
    assert!(r.x >= fx && r.x <= fx + fw && r.y >= fy && r.y + r.h <= fy + fh + 0.5, "{r:?} not in field");

    typing(&mut p, "a");
    typing(&mut p, "e\u{301}"); // e + combining acute: one grapheme, 3 bytes
    typing(&mut p, "字");
    typing(&mut p, "👍🏽"); // emoji + skin tone: one grapheme, 8 bytes
    let v = val(&mut p, "f");
    assert_eq!(v, "ae\u{301}字👍🏽");
    assert_eq!(edit(&mut p), (v.len(), v.len()));
    // the caret moved right as text was typed
    let r2 = p.text_focus().unwrap();
    assert!(r2.x > r.x + 20.0, "caret did not advance: {} -> {}", r.x, r2.x);

    // left steps over whole graphemes
    assert!(p.key("left"));
    assert_eq!(edit(&mut p).0, 1 + 3 + 3);
    assert!(p.key("left"));
    assert_eq!(edit(&mut p).0, 1 + 3);
    assert!(p.key("left"));
    assert_eq!(edit(&mut p).0, 1);
    // backspace deletes "a"; delete removes "e + accent" as one
    assert!(p.key("backspace"));
    assert_eq!(val(&mut p, "f"), "e\u{301}字👍🏽");
    assert!(p.key("delete"));
    assert_eq!(val(&mut p, "f"), "字👍🏽");
    assert!(p.key("end"));
    assert!(p.key("backspace"));
    assert_eq!(val(&mut p, "f"), "字");
    assert!(p.key("home"));
    assert_eq!(edit(&mut p).0, 0);
    // the drawn text is the value
    assert!(has_text(&display(&mut p, 0), "字"));
    // Escape leaves the field and the host hides the keyboard
    assert!(p.key("escape"));
    assert!(p.text_focus().is_none());
    assert_eq!(p.handler_errors, 0);
}

#[test]
fn words_selection_and_clipboard() {
    let mut p = page();
    focus(&mut p, "f");
    typing(&mut p, "hello brave world");
    assert!(p.key("ctrl+left"));
    assert_eq!(edit(&mut p).0, 12);
    assert!(p.key("ctrl+left"));
    assert_eq!(edit(&mut p).0, 6);
    assert!(p.key("ctrl+right"));
    assert_eq!(edit(&mut p).0, 11);
    // select "brave" backwards with shift
    for _ in 0..5 {
        assert!(p.key("shift+left"));
    }
    assert_eq!(edit(&mut p), (6, 11));
    // the selection is drawn
    let sel = rgba(92, 152, 255, 96);
    assert!(!rects_of_color(&display(&mut p, 0), sel).is_empty() || commands(&display(&mut p, 0), "rrect").iter().any(|c| c.at(7).num() == Some(sel)));
    assert!(p.key("ctrl+c"));
    assert_eq!(p.take_request(), Some(PageRequest::Copy("brave".into())));
    assert!(p.key("ctrl+x"));
    assert_eq!(p.take_request(), Some(PageRequest::Copy("brave".into())));
    assert_eq!(val(&mut p, "f"), "hello  world");
    assert!(p.key("ctrl+v"));
    assert_eq!(p.take_request(), Some(PageRequest::Paste));
    p.paste("bold");
    assert_eq!(val(&mut p, "f"), "hello bold world");
    // ctrl+backspace deletes the word before the caret
    assert!(p.key("ctrl+backspace"));
    assert_eq!(val(&mut p, "f"), "hello  world");
    // select all, then typing replaces everything
    assert!(p.key("ctrl+a"));
    assert_eq!(edit(&mut p), (12, 0));
    typing(&mut p, "x");
    assert_eq!(val(&mut p, "f"), "x");
    // a selection collapses to its edge on a plain arrow
    typing(&mut p, "yz");
    assert!(p.key("shift+home"));
    assert!(p.key("right"));
    assert_eq!(edit(&mut p), (3, 3));
    // Enter submits
    assert!(p.key("enter"));
    assert_eq!(val(&mut p, "submitted"), "xyz");
    assert_eq!(p.handler_errors, 0);
}

#[test]
fn ime_preedit_is_shown_inline_then_committed() {
    let mut p = page();
    focus(&mut p, "f");
    typing(&mut p, "ab");
    p.ime_preedit("ㄋㄧ");
    assert_eq!(ui_state(&mut p, "t").get("ime").as_str(), Some("ㄋㄧ"));
    // shown in the field, not yet in the value
    assert!(has_text(&display(&mut p, 0), "abㄋㄧ"));
    assert_eq!(val(&mut p, "f"), "ab");
    // underline under the preedit, in the accent colour
    let underline = commands(&display(&mut p, 0), "rrect").iter().filter(|c| c.at(7).num() == Some(rgba(92, 152, 255, 255)) && c.at(5).num() == Some(1.5)).count();
    assert!(underline >= 1, "preedit underline");
    p.ime_preedit("");
    p.text_input("你");
    assert_eq!(val(&mut p, "f"), "ab你");
    assert!(ui_state(&mut p, "t").get("ime").is_nil());
}

#[test]
fn undo_redo() {
    let mut p = page();
    focus(&mut p, "f");
    for c in ["a", "b", "c"] {
        typing(&mut p, c);
    }
    typing(&mut p, " ");
    for c in ["d", "e"] {
        typing(&mut p, c);
    }
    assert_eq!(val(&mut p, "f"), "abc de");
    assert!(p.key("ctrl+z"));
    assert_eq!(val(&mut p, "f"), "abc ");
    assert!(p.key("ctrl+z"));
    assert_eq!(val(&mut p, "f"), "abc");
    assert!(p.key("ctrl+z"));
    assert_eq!(val(&mut p, "f"), "");
    assert!(p.key("ctrl+shift+z"));
    assert_eq!(val(&mut p, "f"), "abc");
    assert!(p.key("ctrl+y"));
    assert_eq!(val(&mut p, "f"), "abc ");
}

#[test]
fn password_readonly_and_length_limit() {
    let mut p = page();
    focus(&mut p, "pw");
    typing(&mut p, "s3cr");
    typing(&mut p, "字");
    assert_eq!(val(&mut p, "pw"), "s3cr字");
    let cmds = display(&mut p, 0);
    assert!(!has_text_containing(&cmds, "s3cr"), "the password leaked into the display list");
    assert!(has_text(&cmds, "•••••"), "{:?}", texts(&cmds));
    // never described to agents, never copied
    assert!(node(&describe(&mut p, "t"), "pw").get("value").is_nil());
    assert!(p.key("ctrl+a"));
    assert!(p.key("ctrl+c"));
    assert_eq!(p.take_request(), None);
    // caret positions map through the mask: left skips "字" (3 bytes)
    assert!(p.key("end"));
    assert!(p.key("left"));
    assert_eq!(edit(&mut p).0, 4);

    focus(&mut p, "ro");
    typing(&mut p, "zzz");
    assert!(p.key("backspace"));
    assert_eq!(val(&mut p, "ro"), "fixed");

    focus(&mut p, "short");
    typing(&mut p, "abcdefgh");
    assert_eq!(val(&mut p, "short"), "abcde");
    p.paste("xyz");
    assert_eq!(val(&mut p, "short"), "abcde");
}

#[test]
fn tab_selects_and_mouse_selects() {
    let mut p = page();
    focus(&mut p, "f");
    typing(&mut p, "one two three");
    // Tab away and back: a tabbed-into field selects everything
    assert!(p.key("tab"));
    assert_eq!(ui_state(&mut p, "t").get("focus").as_str(), Some("pw"));
    assert!(p.key("shift+tab"));
    assert_eq!(edit(&mut p), (13, 0));
    // drag from the left edge to the right selects text
    let n = node(&describe(&mut p, "t"), "f");
    let (x0, y) = ((n.get("x").num_or(0.0) + 4.0) as f32, center(&n).1);
    tick(&mut p, 5.0);
    p.pointer_down(x0, y);
    assert_eq!(edit(&mut p), (0, 0));
    p.pointer_drag(x0 + 280.0, y);
    p.pointer_up();
    assert_eq!(edit(&mut p), (13, 0));
    // a double click selects a word
    tick(&mut p, 10.0);
    let wx = x0 + metrics().measure("one tw", 15.0);
    click(&mut p, wx, y);
    click(&mut p, wx, y);
    assert_eq!(edit(&mut p), (7, 4));
}

#[test]
fn long_field_scrolls_to_keep_the_caret_visible() {
    let mut p = page();
    focus(&mut p, "f");
    let long = "The quick brown fox jumps over the lazy dog again and again";
    typing(&mut p, long);
    let n = node(&describe(&mut p, "t"), "f");
    let (fx, fw) = (n.get("x").num_or(0.0) - 50.0, n.get("w").num_or(0.0));
    let sx = ui_state(&mut p, "t").get("edit").get("sx").num_or(0.0);
    assert!(sx > 0.0, "field did not scroll");
    // the caret stays inside the field
    let r = p.text_focus().unwrap();
    assert!(r.x <= (fx + 50.0 + fw) as f32 && r.x >= (fx + 50.0) as f32, "caret {} outside the field", r.x);
    // what is drawn of the text stays inside the field (app-local coordinates)
    let cmds = display(&mut p, 0);
    for t in commands(&cmds, "text") {
        if long.contains(t.at(6).as_str().unwrap_or("#")) && t.at(6).as_str() != Some("") {
            let x = t.at(2).num_or(0.0);
            let w = metrics().measure(t.at(6).as_str().unwrap(), 15.0) as f64;
            assert!(x >= fx - 0.5 && x + w <= fx + fw + 0.5, "text {x}+{w} outside {fx}..{}", fx + fw);
        }
    }
    assert!(p.key("home"));
    assert!(ui_state(&mut p, "t").get("edit").get("sx").is_nil());
}

#[test]
fn text_area_lines_wrapping_and_scrolling() {
    let mut p = page();
    focus(&mut p, "area");
    typing(&mut p, "first");
    assert!(p.key("enter"));
    typing(&mut p, "second line");
    assert!(p.key("enter"));
    typing(&mut p, "3");
    assert_eq!(val(&mut p, "area"), "first\nsecond line\n3");
    // up keeps the x position: from after "3" to after "s" of "second"
    assert!(p.key("up"));
    assert_eq!(edit(&mut p).0, 6 + 1);
    assert!(p.key("end"));
    assert_eq!(edit(&mut p).0, 6 + 11);
    assert!(p.key("home"));
    assert_eq!(edit(&mut p).0, 6);
    assert!(p.key("up"));
    assert_eq!(edit(&mut p).0, 0);
    assert!(p.key("ctrl+end"));
    assert_eq!(edit(&mut p).0, val(&mut p, "area").len());
    // more lines than fit: the area scrolls to follow the caret
    for i in 0..6 {
        assert!(p.key("enter"));
        typing(&mut p, &format!("line {i}"));
    }
    let sy = ui_state(&mut p, "t").get("scroll").get("area").num_or(0.0);
    assert!(sy > 0.0, "area did not scroll");
    let cmds = display(&mut p, 0);
    assert!(has_text(&cmds, "line 5"));
    assert!(!has_text(&cmds, "first"), "scrolled-out line still drawn");
    // the wheel scrolls it back up
    let n = node(&describe(&mut p, "t"), "area");
    let (cx, cy) = center(&n);
    assert!(p.wheel(cx, cy, 0.0, 10.0));
    assert!(ui_state(&mut p, "t").get("scroll").get("area").is_nil());
    assert!(has_text(&display(&mut p, 0), "first"));
    // a long line wraps inside the width
    assert!(p.key("ctrl+a"));
    typing(&mut p, "a long line of words that cannot possibly fit into two hundred and sixty units");
    let cmds = display(&mut p, 0);
    let parts: Vec<String> = texts(&cmds).into_iter().filter(|t| t.contains("words") || t.contains("possibly") || t.contains("units")).collect();
    assert!(parts.len() >= 2, "no wrapping: {parts:?}");
    assert_eq!(p.handler_errors, 0);
}

#[test]
fn caret_blinks_by_declared_time() {
    let mut p = page();
    tick(&mut p, 3.0);
    focus(&mut p, "f");
    let sc = script_cell(&p, 0);
    display(&mut p, 0);
    // a focused field redraws at the next blink, not every frame
    let e = p.graph().expires(sc);
    assert!((3.53..3.6).contains(&e), "{e}");
    // blur: nothing time-dependent remains
    let (ax, ay) = center(&node(&describe(&mut p, "t"), "after"));
    click(&mut p, ax, ay);
    display(&mut p, 0);
    assert_eq!(p.graph().expires(sc), f64::INFINITY);
    let _ = Data::Nil;
}
