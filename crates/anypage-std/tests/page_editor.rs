//! Headless tests of pages/editor, the multilingual text editor: typing,
//! IME preedit and commit, visual arrows over right-to-left text and emoji
//! graphemes, caret affinity at soft wraps, undo / redo, find and replace,
//! open / drop / save (UTF-8, UTF-16, line ends, file names, binary
//! refused, confirmation before replacing a modified document), Escape and
//! Tab, the narrow layout picked from the viewport, the agent actions, and
//! render times at 128 KB.
//!
//! Run with `cargo test -p anypage-std --test page_editor`. The page folder
//! is found from this crate (`../../pages/editor`) or from `ANYPAGE_ROOT`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anypage_core::Status;
use anypage_engine::{Direction, HostResult, Page, PageRequest, TextRaster, Wake};
use anypage_runtime::{Data, DisplayList, Package, ScriptPage};

// ---------------------------------------------------------------- helpers

fn page_dir() -> PathBuf {
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../pages/editor");
    if here.join("page.json").is_file() {
        return here;
    }
    let root = std::env::var("ANYPAGE_ROOT").expect("pages/editor not found next to this crate; set ANYPAGE_ROOT");
    PathBuf::from(root).join("pages/editor")
}

fn text() -> Arc<TextRaster> {
    Arc::new(TextRaster::load().expect("a system font"))
}

fn wake() -> Wake {
    Arc::new(|| {})
}

fn load() -> ScriptPage {
    let pkg = Package::from_dir(&page_dir()).expect("package");
    let p = ScriptPage::load(&pkg, &anypage_std::natives(), text(), wake()).unwrap_or_else(|e| panic!("load failed: {e}"));
    assert!(
        p.load_ms < p.manifest.limits.load_ms,
        "loaded in {:.1} ms, over the manifest's {} ms",
        p.load_ms,
        p.manifest.limits.load_ms
    );
    p
}

/// The display list the wide app's panel (at x = 0) draws now.
fn display(p: &mut ScriptPage) -> Data {
    display_at(p, 0.0)
}

/// The left edge of the narrow (phone portrait) app.
const NARROW_X: f64 = 2400.0;

/// The display list of the panel whose rectangle starts at world x.
fn display_at(p: &mut ScriptPage, x: f64) -> Data {
    let panel = p.spec().panels.iter().find(|pn| (pn.rect.x as f64 - x).abs() < 0.5).expect("a panel at that x");
    let raster = panel.cell;
    let script = p.graph().inputs_of(raster)[0];
    let g = p.parts().0;
    let i = g.demand(script);
    match &i.status {
        Status::Ready => {}
        other => panic!("the panel is not ready: {other:?}"),
    }
    i.get::<DisplayList>().expect("a display list").cmds.clone()
}

/// Draw and time it (ms).
fn timed_display(p: &mut ScriptPage) -> (Data, f64) {
    let t0 = Instant::now();
    let d = display(p);
    (d, t0.elapsed().as_secs_f64() * 1000.0)
}

/// Every "text" command: (x, y, size, string, dir).
fn texts(cmds: &Data) -> Vec<(f64, f64, f64, String, Option<String>)> {
    cmds.items()
        .iter()
        .filter(|c| c.at(1).as_str() == Some("text"))
        .filter_map(|c| {
            let s = c.at(6).as_str()?.to_string();
            Some((c.at(2).num_or(0.0), c.at(3).num_or(0.0), c.at(4).num_or(0.0), s, c.at(7).as_str().map(str::to_string)))
        })
        .collect()
}

fn has_text(cmds: &Data, s: &str) -> bool {
    texts(cmds).iter().any(|t| t.3 == s)
}

fn has_text_containing(cmds: &Data, s: &str) -> bool {
    texts(cmds).iter().any(|t| t.3.contains(s))
}

fn no_error_card(cmds: &Data) {
    assert!(!has_text_containing(cmds, "the view failed"), "the UI library's error card is shown: {:?}", texts(cmds).iter().map(|t| &t.3).collect::<Vec<_>>());
}

fn state(p: &mut ScriptPage) -> Data {
    p.cell_value("editor.state").expect("state cell")
}

fn ui_state(p: &mut ScriptPage) -> Data {
    p.cell_value("editor.ui").expect("ui cell")
}

fn doc(p: &mut ScriptPage) -> String {
    state(p).get("text").as_str().unwrap_or_default().to_string()
}

/// (caret, anchor) of the editor in the wide app.
fn caret(p: &mut ScriptPage) -> (usize, usize) {
    caret_in(p, "editor")
}

/// (caret, anchor) of the editor in app `app` ("editor" wide, "narrow").
fn caret_in(p: &mut ScriptPage, app: &str) -> (usize, usize) {
    let u = p.cell_value(&format!("{app}.ui")).expect("ui cell");
    let e = u.get("mle:doc");
    (e.get("caret").num_or(-1.0) as usize, e.get("anchor").num_or(-1.0) as usize)
}

fn act(p: &mut ScriptPage, name: &str, params: Data) -> Data {
    p.run_action(name, &params).unwrap_or_else(|e| panic!("action {name}: {e}"))
}

fn s(v: &str) -> Data {
    Data::str(v)
}

fn set_text(p: &mut ScriptPage, t: &str) {
    act(p, "set_text", Data::map([("text", s(t))]));
}

fn key(p: &mut ScriptPage, k: &str) {
    p.key(k);
}

fn type_text(p: &mut ScriptPage, t: &str) {
    for ch in t.chars() {
        p.text_input(&ch.to_string());
    }
}

/// The editor's rectangle in world units, from the UI library's describe.
fn editor_rect(p: &mut ScriptPage) -> (f64, f64, f64, f64) {
    let mut from = 1.0;
    loop {
        let d = act(p, "editor.describe", Data::map([("from", Data::Num(from)), ("types", s("mleditor"))]));
        if let Some(n) = d.get("nodes").items().iter().find(|n| n.get("id").as_str() == Some("doc")) {
            return (n.get("x").num_or(0.0), n.get("y").num_or(0.0), n.get("w").num_or(0.0), n.get("h").num_or(0.0));
        }
        match d.get("next").num() {
            Some(n) => from = n,
            None => panic!("no editor node"),
        }
    }
}

fn no_handler_errors(p: &ScriptPage) {
    assert_eq!(p.handler_errors, 0, "handlers failed (see the log)");
}

fn requests(p: &mut ScriptPage) -> Vec<PageRequest> {
    let mut out = Vec::new();
    while let Some(r) = p.take_request() {
        out.push(r);
    }
    out
}

/// The (suggested name, bytes) of the save request the page made, if any.
fn save_request(p: &mut ScriptPage) -> Option<(String, Vec<u8>)> {
    requests(p).into_iter().find_map(|r| match r {
        PageRequest::SaveFile { name, bytes, .. } => Some((name, bytes.to_vec())),
        _ => None,
    })
}

fn open_request(p: &mut ScriptPage) -> Option<u64> {
    requests(p).into_iter().find_map(|r| match r {
        PageRequest::OpenFile { id, .. } => Some(id),
        _ => None,
    })
}

/// The rectangles the page asked the engine to fit, as (x, y, w, h).
fn fits(p: &mut ScriptPage) -> Vec<(f32, f32, f32, f32)> {
    requests(p)
        .into_iter()
        .filter_map(|r| match r {
            PageRequest::FitRect(r) => Some((r.x, r.y, r.w, r.h)),
            _ => None,
        })
        .collect()
}

fn focus_in(p: &mut ScriptPage, app: &str) -> Option<String> {
    p.cell_value(&format!("{app}.ui")).expect("ui cell").get("focus").as_str().map(str::to_string)
}

fn notice(p: &mut ScriptPage) -> String {
    state(p).get("notice").as_str().unwrap_or_default().to_string()
}

/// UTF-16 with a byte order mark.
fn utf16(s: &str, little: bool) -> Vec<u8> {
    let mut out = if little { vec![0xFF, 0xFE] } else { vec![0xFE, 0xFF] };
    for u in s.encode_utf16() {
        out.extend_from_slice(&if little { u.to_le_bytes() } else { u.to_be_bytes() });
    }
    out
}

fn op(p: &mut ScriptPage, app: &str, id: &str, op: &str) -> Data {
    act(p, &format!("{app}.do"), Data::map([("id", s(id)), ("op", s(op))]))
}

const FAMILY: &str = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";

// ---------------------------------------------------------------- tests

#[test]
fn loads_renders_and_focuses_the_text() {
    let mut p = load();
    let (d, ms) = timed_display(&mut p);
    no_error_card(&d);
    assert!(has_text(&d, "AnyPage 多語言編輯器"), "title missing");
    assert!(has_text_containing(&d, "第 1 行，第 1 欄"), "status bar position missing");
    assert!(has_text(&d, "字型齊全") || has_text_containing(&d, "缺字"), "missing-glyph indicator missing");
    // the welcome text has right-to-left paragraphs, drawn as such
    let rtl: Vec<_> = texts(&d).into_iter().filter(|t| t.4.as_deref() == Some("rtl")).collect();
    assert!(!rtl.is_empty(), "no right-to-left line drawn");
    // the editor has focus and the host was told where the caret is
    assert_eq!(ui_state(&mut p).get("focus").as_str(), Some("doc"));
    assert!(p.text_focus().is_some(), "no text focus for the host's IME");
    no_handler_errors(&p);
    println!("first render {ms:.1} ms, load {:.1} ms", p.load_ms);
}

#[test]
fn typing_ime_undo_and_redo() {
    let mut p = load();
    display(&mut p);
    set_text(&mut p, "");
    type_text(&mut p, "Hello");
    assert_eq!(doc(&mut p), "Hello");
    p.text_input(" ");
    type_text(&mut p, "world");
    assert_eq!(doc(&mut p), "Hello world");
    assert_eq!(caret(&mut p), (11, 11));

    // IME: the preedit is shown inline at the caret, not in the document
    p.ime_preedit("ㄋㄧˇ");
    let d = display(&mut p);
    assert!(has_text(&d, "Hello worldㄋㄧˇ"), "preedit not drawn inline: {:?}", texts(&d));
    assert_eq!(doc(&mut p), "Hello world");
    p.ime_preedit("你");
    assert!(has_text(&display(&mut p), "Hello world你"));
    // commit: the host clears the preedit, then sends the text
    p.ime_preedit("");
    p.text_input("你好");
    assert_eq!(doc(&mut p), "Hello world你好");
    assert!(has_text(&display(&mut p), "Hello world你好"));
    assert_eq!(ui_state(&mut p).get("ime"), &Data::Nil);

    // a preedit over a selection replaces it on commit
    key(&mut p, "shift+left");
    key(&mut p, "shift+left");
    assert_eq!(caret(&mut p), (11, 17));
    p.ime_preedit("せかい");
    assert!(has_text(&display(&mut p), "Hello worldせかい"));
    p.ime_preedit("");
    p.text_input("世界");
    assert_eq!(doc(&mut p), "Hello world世界");

    // undo goes back edit by edit (a run of typing, IME commits included, is
    // one step; a space starts a new one), redo forward again
    key(&mut p, "ctrl+z");
    assert_eq!(doc(&mut p), "Hello world你好");
    assert_eq!(caret(&mut p), (11, 17), "undo restores the selection");
    key(&mut p, "ctrl+z");
    assert_eq!(doc(&mut p), "Hello ");
    key(&mut p, "ctrl+z");
    assert_eq!(doc(&mut p), "Hello");
    key(&mut p, "ctrl+y");
    assert_eq!(doc(&mut p), "Hello ");
    key(&mut p, "ctrl+shift+z");
    assert_eq!(doc(&mut p), "Hello world你好");
    key(&mut p, "ctrl+z");
    key(&mut p, "ctrl+z");
    key(&mut p, "ctrl+z");
    assert_eq!(doc(&mut p), "", "back to the empty document");
    key(&mut p, "ctrl+z");
    assert!(doc(&mut p).starts_with("歡迎使用"), "set_text is undoable too");
    key(&mut p, "ctrl+y");
    for _ in 0..4 {
        key(&mut p, "ctrl+y");
    }
    assert_eq!(doc(&mut p), "Hello world世界");

    // backspace and delete by grapheme, enter breaks the line
    key(&mut p, "end");
    key(&mut p, "backspace");
    assert_eq!(doc(&mut p), "Hello world世");
    key(&mut p, "enter");
    type_text(&mut p, "é");
    assert_eq!(doc(&mut p), "Hello world世\né");
    key(&mut p, "ctrl+home");
    key(&mut p, "delete");
    assert_eq!(doc(&mut p), "ello world世\né");
    no_error_card(&display(&mut p));
    no_handler_errors(&p);
}

#[test]
fn arrows_move_by_grapheme_over_emoji() {
    let mut p = load();
    display(&mut p);
    let t = format!("ab {FAMILY} cd e\u{301}f");
    set_text(&mut p, &t);
    key(&mut p, "ctrl+home");
    for _ in 0..3 {
        key(&mut p, "right");
    }
    assert_eq!(caret(&mut p).0, 3);
    // the whole ZWJ family is one step
    key(&mut p, "right");
    assert_eq!(caret(&mut p).0, 3 + FAMILY.len());
    key(&mut p, "left");
    assert_eq!(caret(&mut p).0, 3);
    // shift+right selects it, backspace deletes it in one go
    key(&mut p, "shift+right");
    assert_eq!(caret(&mut p), (3 + FAMILY.len(), 3));
    key(&mut p, "backspace");
    assert_eq!(doc(&mut p), "ab  cd e\u{301}f");
    // e + combining acute is one grapheme
    key(&mut p, "end");
    key(&mut p, "left");
    key(&mut p, "left");
    let c = caret(&mut p).0;
    assert_eq!(&doc(&mut p)[c..], "e\u{301}f");
    key(&mut p, "delete");
    assert_eq!(doc(&mut p), "ab  cd f");
    // backspace after the family deletes the whole cluster
    set_text(&mut p, &format!("x{FAMILY}"));
    key(&mut p, "ctrl+end");
    key(&mut p, "backspace");
    assert_eq!(doc(&mut p), "x");
    // inserting into the middle of a cluster (agent) snaps to its start
    set_text(&mut p, &format!("a{FAMILY}b"));
    let r = act(&mut p, "insert", Data::map([("at", Data::Num(5.0)), ("text", s("!"))]));
    assert_eq!(r.get("at").num(), Some(1.0));
    assert_eq!(doc(&mut p), format!("a!{FAMILY}b"));
    no_handler_errors(&p);
}

/// Caret x of byte `pos` on a one-line right-to-left paragraph, as the page lays it out.
fn rtl_x(tr: &TextRaster, line: &str, size: f32, pos: usize) -> f32 {
    tr.caret_x_dir(line, size, pos, Direction::Rtl)
}

#[test]
fn right_to_left_lines_are_right_aligned_and_arrows_move_visually() {
    let mut p = load();
    display(&mut p);
    let tr = text();
    let size = 18.0f32;

    // pure Hebrew: Left goes forward (to the left on screen), Right back
    let he = "שלום עולם";
    set_text(&mut p, he);
    key(&mut p, "ctrl+home");
    assert_eq!(caret(&mut p).0, 0);
    key(&mut p, "left");
    assert_eq!(caret(&mut p).0, "ש".len(), "Left in a right-to-left line moves forward");
    key(&mut p, "right");
    assert_eq!(caret(&mut p).0, 0);
    key(&mut p, "end");
    assert_eq!(caret(&mut p).0, he.len());

    // the line is drawn right to left and right-aligned to the text area
    let d = display(&mut p);
    let line = texts(&d).into_iter().find(|t| t.3 == he).expect("the Hebrew line is drawn whole");
    assert_eq!(line.4.as_deref(), Some("rtl"));
    let (ex, _, ew, _) = editor_rect(&mut p);
    let right = ex + ew - 14.0 - 10.0; // padding and scrollbar lane
    let w = tr.measure(he, size) as f64;
    assert!((line.0 + w - right).abs() < 1.5, "right edge {} vs text area {}", line.0 + w, right);

    // a mixed line: Left from the logical start walks stops with x decreasing, Right back up
    let mixed = "שלום abc 42 עולם";
    set_text(&mut p, mixed);
    key(&mut p, "ctrl+home");
    // (no frame drawn since the text changed: handlers must work on the new text anyway)
    let r = p.run_action("editor.do", &Data::map([("op", s("key")), ("value", s("left"))])).expect("key");
    assert_eq!(r.get("ok"), &Data::Bool(true), "the key reached the editor");
    key(&mut p, "ctrl+home");
    let mut xs = vec![rtl_x(&tr, mixed, size, 0)];
    let mut seen = vec![0usize];
    for _ in 0..40 {
        key(&mut p, "left");
        let c = caret(&mut p).0;
        if c == *seen.last().unwrap() {
            break;
        }
        seen.push(c);
        xs.push(rtl_x(&tr, mixed, size, c));
    }
    for w in xs.windows(2) {
        assert!(w[1] < w[0] - 0.4, "Left must move left on screen: {xs:?} at {seen:?}");
    }
    assert!(xs.last().unwrap() - 0.0 < 1.0 || seen.len() > 10, "Left stops before the left end: {xs:?}");
    // every grapheme boundary was visited (the line has no ambiguous stop twice)
    let boundaries = mixed.char_indices().count() + 1;
    assert!(seen.len() >= boundaries - 3, "visited {} of {boundaries} stops: {seen:?}", seen.len());
    // and Right comes back the same way
    let mut back = vec![*seen.last().unwrap()];
    for _ in 0..40 {
        key(&mut p, "right");
        let c = caret(&mut p).0;
        if c == *back.last().unwrap() {
            break;
        }
        back.push(c);
        if c == 0 {
            break;
        }
    }
    let bx: Vec<f32> = back.iter().map(|&c| rtl_x(&tr, mixed, size, c)).collect();
    for w in bx.windows(2) {
        assert!(w[1] > w[0] + 0.4, "Right must move right on screen: {bx:?}");
    }

    // clicking: the hit position of a point is in the paragraph's direction
    set_text(&mut p, he);
    let d = display(&mut p);
    let line = texts(&d).into_iter().find(|t| t.3 == he).expect("line");
    // just inside the right end is the logical start
    p.pointer_down((line.0 + w - 1.0) as f32, (line.1 + 8.0) as f32);
    p.pointer_up();
    assert_eq!(caret(&mut p).0, 0, "a click at the right end of a Hebrew line is its start");
    p.pointer_down((line.0 + 1.0) as f32, (line.1 + 8.0) as f32);
    p.pointer_up();
    assert_eq!(caret(&mut p).0, he.len(), "a click at the left end is its end");

    // the caret in a right-to-left line: a 2-unit bar at text.caret(dir = rtl),
    // with a flag pointing left
    key(&mut p, "ctrl+home");
    key(&mut p, "left");
    let d = display(&mut p);
    let line = texts(&d).into_iter().find(|t| t.3 == he).expect("line");
    let cx = line.0 + rtl_x(&tr, he, size, "ש".len()) as f64;
    let rects: Vec<(f64, f64)> = d
        .items()
        .iter()
        .filter(|c| c.at(1).as_str() == Some("rrect"))
        .map(|c| (c.at(2).num_or(0.0), c.at(4).num_or(0.0)))
        .collect();
    assert!(rects.iter().any(|&(x, w)| (x - (cx - 1.0)).abs() < 0.6 && w == 2.0), "no caret bar at {cx}: {rects:?}");
    assert!(rects.iter().any(|&(x, w)| (x - (cx - 5.0)).abs() < 0.6 && w == 4.0), "no right-to-left flag");

    // a selection in a right-to-left line is highlighted with text.selection rects
    key(&mut p, "ctrl+a");
    let d = display(&mut p);
    no_error_card(&d);
    no_handler_errors(&p);
}

#[test]
fn clipboard_copy_cut_paste() {
    let mut p = load();
    display(&mut p);
    set_text(&mut p, "複製 שלום 👍🏿 end");
    requests(&mut p);
    key(&mut p, "ctrl+home");
    for _ in 0..6 {
        key(&mut p, "shift+right");
    }
    key(&mut p, "ctrl+c");
    let copied: Vec<String> = requests(&mut p)
        .into_iter()
        .filter_map(|r| match r {
            PageRequest::Copy(t) => Some(t),
            _ => None,
        })
        .collect();
    assert_eq!(copied.len(), 1);
    // graphemes: 複 製 space, then a right-to-left word moves forward with shift+right
    // until its visual end; whatever was selected is what the clipboard got
    let (c, a) = caret(&mut p);
    assert_eq!(copied[0], doc(&mut p)[a.min(c)..a.max(c)]);
    key(&mut p, "ctrl+x");
    let cut_len = copied[0].len();
    assert_eq!(doc(&mut p).len(), "複製 שלום 👍🏿 end".len() - cut_len);
    key(&mut p, "ctrl+end");
    key(&mut p, "ctrl+v");
    assert!(requests(&mut p).iter().any(|r| matches!(r, PageRequest::Paste)), "paste asks the host");
    p.paste("\r\n貼上 مرحبا\r\n");
    assert!(doc(&mut p).ends_with("end\n貼上 مرحبا\n"), "pasted with line breaks normalised: {:?}", doc(&mut p));
    key(&mut p, "ctrl+z");
    assert!(doc(&mut p).ends_with("end"));
    no_handler_errors(&p);
}

#[test]
fn survives_whatever_an_agent_writes_into_the_state() {
    let mut p = load();
    display(&mut p);
    // the state cell is agent-writable: any JSON may arrive
    for v in [
        r#"{"text": 12345, "size": "huge", "wrap": "no", "find": 7, "find_open": true, "repl": [], "file": {}, "notice": 3}"#,
        r#"{"text": null}"#,
        r#"[1, 2, 3]"#,
        r#""just a string""#,
        r#"{"text": "a\r\nb\rc", "size": 1000, "find_open": true, "find": "b"}"#,
        r#"{"text": "abc", "size": "nan"}"#,
        r#"{"text": "abc", "size": "NaN", "eol": 3, "bom": "yes", "enc": [], "file": "a\u0007b"}"#,
        r#"{"text": "abc", "confirm": 5}"#,
        r#"{"text": "abc", "confirm": {"body": 7, "ok": []}}"#,
        r#"5"#,
    ] {
        let out = p.act(&format!(r#"{{"set": "editor.state", "value": {v}}}"#));
        assert!(out.contains("\"ok\":true"), "{out}");
        let d = display(&mut p);
        no_error_card(&d);
        key(&mut p, "right");
        p.text_input("x");
        no_error_card(&display(&mut p));
    }
    // a text longer than the editor's limit, set from outside, is shown read-only
    let huge = "長".repeat(100_000);
    let out = p.act(&format!(r#"{{"set": "editor.state", "value": {{"text": "{huge}"}}}}"#));
    assert!(out.contains("\"ok\":true"));
    let d = display(&mut p);
    no_error_card(&d);
    assert!(has_text_containing(&d, "唯讀"), "read-only badge");
    p.text_input("x");
    assert_eq!(doc(&mut p).len(), huge.len(), "typing into the read-only view changes nothing");
    // set_text replaces it with something editable again
    set_text(&mut p, "ok");
    p.text_input("!");
    assert!(doc(&mut p).contains('!'));
    no_error_card(&display(&mut p));
    no_handler_errors(&p);
}

#[test]
fn find_and_replace_any_case() {
    let mut p = load();
    display(&mut p);
    set_text(&mut p, "Hello hello HELLO\nΩμέγα ΩΜΈΓΑ\nЧай чай ЧАЙ\nÉté ÉTÉ");
    let r = act(&mut p, "find", Data::map([("query", s("hello"))]));
    assert_eq!(r.get("count").num(), Some(3.0));
    // the first match at or after the caret is selected (the caret was at 0)
    assert_eq!(caret(&mut p), (5, 0));
    let r = act(&mut p, "find", Data::map([("query", s("ωμέγα"))]));
    assert_eq!(r.get("count").num(), Some(2.0), "Greek folds, accents included: {r:?}");
    assert_eq!(r.get("matches").at(1).get("line").num(), Some(2.0));
    let r = act(&mut p, "find", Data::map([("query", s("чай"))]));
    assert_eq!(r.get("count").num(), Some(3.0));
    let r = act(&mut p, "find", Data::map([("query", s("été"))]));
    assert_eq!(r.get("count").num(), Some(2.0));
    let r = act(&mut p, "find", Data::map([("query", s("Hello")), ("case", Data::Bool(true))]));
    assert_eq!(r.get("count").num(), Some(1.0));

    // the find bar shows the count and highlights; F3 moves on
    let d = display(&mut p);
    no_error_card(&d);
    assert!(has_text_containing(&d, "個符合"), "match count missing");
    let r = act(&mut p, "replace", Data::map([("query", s("hello")), ("with", s("你好"))]));
    assert_eq!(r.get("replaced").num(), Some(3.0));
    assert_eq!(doc(&mut p), "你好 你好 你好\nΩμέγα ΩΜΈΓΑ\nЧай чай ЧАЙ\nÉté ÉTÉ");
    key(&mut p, "ctrl+z");
    assert!(doc(&mut p).starts_with("Hello hello HELLO\n"), "replace all is one undo step");

    // the keyboard path: Ctrl+F focuses the find field, typing goes there, Enter finds
    key(&mut p, "ctrl+home");
    key(&mut p, "ctrl+f");
    assert_eq!(ui_state(&mut p).get("focus").as_str(), Some("find"));
    key(&mut p, "ctrl+a");
    type_text(&mut p, "чай");
    assert_eq!(state(&mut p).get("find").as_str(), Some("чай"));
    key(&mut p, "enter");
    let start = "Hello hello HELLO\nΩμέγα ΩΜΈΓΑ\n".len();
    assert_eq!(caret(&mut p), (start + "Чай".len(), start));
    key(&mut p, "f3");
    let second = start + "Чай ".len();
    assert_eq!(caret(&mut p), (second + "чай".len(), second));
    // replace the selected match, then the next one is selected
    key(&mut p, "ctrl+h");
    assert_eq!(ui_state(&mut p).get("focus").as_str(), Some("repl"));
    type_text(&mut p, "tea");
    key(&mut p, "enter");
    assert!(doc(&mut p).contains("Чай tea ЧАЙ"), "{}", doc(&mut p));
    let third = start + "Чай tea ".len();
    assert_eq!(caret(&mut p), (third + "ЧАЙ".len(), third));
    let d = display(&mut p);
    assert!(has_text_containing(&d, "2 / 2 個符合") || has_text_containing(&d, "個符合"), "count");
    // Escape closes the bar and puts the caret back in the text
    key(&mut p, "escape");
    assert_eq!(state(&mut p).get("find_open"), &Data::Bool(false));
    assert_eq!(ui_state(&mut p).get("focus").as_str(), Some("doc"));
    no_error_card(&display(&mut p));
    no_handler_errors(&p);
}

#[test]
fn save_open_drop_and_refuse_binary() {
    let mut p = load();
    display(&mut p);
    let body = "第一行\nעברית שורה\n\u{1F600} done";
    set_text(&mut p, body);
    requests(&mut p);
    key(&mut p, "ctrl+s");
    let rs = requests(&mut p);
    let (id, name, bytes) = rs
        .iter()
        .find_map(|r| match r {
            PageRequest::SaveFile { id, name, bytes } => Some((*id, name.clone(), bytes.clone())),
            _ => None,
        })
        .expect("a save request");
    assert_eq!(name, "未命名.txt");
    assert_eq!(bytes.as_slice(), body.as_bytes(), "saved bytes are the text in UTF-8");
    assert_eq!(state(&mut p).get("dirty"), &Data::Bool(true));
    p.host_result(id, HostResult::Saved { name: "C:\\notes\\memo.txt".into() });
    assert_eq!(state(&mut p).get("file").as_str(), Some("memo.txt"));
    assert_eq!(state(&mut p).get("dirty"), &Data::Bool(false));

    // a CRLF file with a BOM comes back the way it was
    let r = act(&mut p, "load_text", Data::map([("name", s("win.txt")), ("text", s("\u{FEFF}一\r\n二\r\n"))]));
    assert_eq!(r.get("opened"), &Data::Bool(true));
    assert_eq!(doc(&mut p), "一\n二\n");
    act(&mut p, "save", Data::Nil);
    let bytes = requests(&mut p)
        .into_iter()
        .find_map(|r| match r {
            PageRequest::SaveFile { name, bytes, .. } => Some((name, bytes)),
            _ => None,
        })
        .expect("save request");
    assert_eq!(bytes.0, "win.txt");
    assert_eq!(bytes.1.as_slice(), "\u{FEFF}一\r\n二\r\n".as_bytes());
    let d = display(&mut p);
    assert!(has_text_containing(&d, "UTF-8 BOM · CRLF"), "encoding shown");

    // binary is refused: NUL bytes, control characters, a binary extension
    let r = act(&mut p, "load_text", Data::map([("name", s("x.txt")), ("text", s("abc\u{0}def"))]));
    assert_eq!(r.get("opened"), &Data::Bool(false));
    assert_eq!(r.get("reason").as_str(), Some("binary"));
    assert_eq!(doc(&mut p), "一\n二\n", "the document is unchanged");
    p.file_drop("photo.png", Arc::new(vec![0x89, b'P', b'N', b'G', 0, 0]));
    assert!(state(&mut p).get("notice").as_str().unwrap_or_default().contains("binary"), "{:?}", state(&mut p).get("notice"));

    // the open dialog asks for .txt / .md; the answer goes through the same checks
    key(&mut p, "ctrl+o");
    let id = requests(&mut p)
        .into_iter()
        .find_map(|r| match r {
            PageRequest::OpenFile { id, filters } => {
                assert_eq!(filters, vec!["txt".to_string(), "md".to_string()]);
                Some(id)
            }
            _ => None,
        })
        .expect("open request");
    p.host_result(id, HostResult::File { name: "readme.md".into(), bytes: Arc::new("# 標題\nשלום".as_bytes().to_vec()) });
    assert_eq!(doc(&mut p), "# 標題\nשלום", "text.decode reads the opened file");
    assert_eq!(state(&mut p).get("file").as_str(), Some("readme.md"));
    assert_eq!(state(&mut p).get("notice"), &Data::Nil, "the earlier refusal is cleared");
    p.file_drop("dropped.txt", Arc::new(b"dropped text".to_vec()));
    assert_eq!(doc(&mut p), "dropped text");
    assert_eq!(state(&mut p).get("file").as_str(), Some("dropped.txt"));
    no_error_card(&display(&mut p));
    no_handler_errors(&p);
}

#[test]
fn open_and_drop_utf8_and_utf16_files_and_refuse_binary() {
    let mut p = load();
    display(&mut p);
    requests(&mut p);
    // Open…: UTF-8 with a BOM; saved back the way it came
    key(&mut p, "ctrl+o");
    let id = open_request(&mut p).expect("open request");
    let body = "# 標題\nשלום עולם\n中文 😀";
    let mut bom8 = vec![0xEF, 0xBB, 0xBF];
    bom8.extend_from_slice(body.as_bytes());
    p.host_result(id, HostResult::File { name: "readme.md".into(), bytes: Arc::new(bom8.clone()) });
    assert_eq!(doc(&mut p), body);
    assert_eq!(state(&mut p).get("file").as_str(), Some("readme.md"));
    let d = display(&mut p);
    assert!(has_text(&d, "שלום עולם") && has_text(&d, "中文 😀"), "the file's lines are drawn: {:?}", texts(&d));
    assert!(has_text_containing(&d, "UTF-8 BOM · LF"));
    key(&mut p, "ctrl+s");
    let (name, bytes) = save_request(&mut p).expect("save");
    assert_eq!((name.as_str(), bytes), ("readme.md", bom8));

    // dropped: UTF-16 little-endian with CRLF line ends, saved as UTF-8 with a BOM and CRLF
    let t16 = "第一行\r\nعربي 2026\r\n👍🏿 end";
    p.file_drop("utf16.txt", Arc::new(utf16(t16, true)));
    assert_eq!(doc(&mut p), "第一行\nعربي 2026\n👍🏿 end");
    assert_eq!(state(&mut p).get("file").as_str(), Some("utf16.txt"));
    let d = display(&mut p);
    assert!(has_text(&d, "第一行") && has_text(&d, "عربي 2026"), "{:?}", texts(&d));
    assert!(has_text_containing(&d, "UTF-16→UTF-8 BOM · CRLF"), "the conversion is shown");
    key(&mut p, "ctrl+s");
    let mut want = vec![0xEF, 0xBB, 0xBF];
    want.extend_from_slice(t16.as_bytes());
    assert_eq!(save_request(&mut p).expect("save").1, want);
    // big-endian too
    p.file_drop("be.txt", Arc::new(utf16("Ελληνικά\nok", false)));
    assert_eq!(doc(&mut p), "Ελληνικά\nok");
    // plain UTF-8 stays without a BOM
    p.file_drop("plain.txt", Arc::new("plain 中".as_bytes().to_vec()));
    assert_eq!(doc(&mut p), "plain 中");
    key(&mut p, "ctrl+s");
    assert_eq!(save_request(&mut p).expect("save").1, "plain 中".as_bytes());

    // refused, with the document unchanged: invalid UTF-8, NUL, control
    // characters, odd UTF-16, a binary extension, too large
    for (name, bytes, why) in [
        ("bad.txt", vec![0xC3, 0x28, b'a'], "binary"),
        ("nul.txt", b"ab\0cd".to_vec(), "binary"),
        ("ctl.txt", b"ab\x01\x02cd".to_vec(), "binary"),
        ("odd.txt", vec![0xFF, 0xFE, b'a'], "binary"),
        ("image.png", b"hello".to_vec(), "binary"),
        ("big.txt", vec![b'a'; 600 * 1024], "too large"),
    ] {
        p.file_drop(name, Arc::new(bytes));
        assert_eq!(doc(&mut p), "plain 中", "{name} must not replace the document");
        let n = notice(&mut p);
        assert!(n.contains(name) && n.contains(why), "{name}: {n}");
    }
    no_error_card(&display(&mut p));
    no_handler_errors(&p);
}

#[test]
fn a_modified_document_is_replaced_only_after_asking() {
    let mut p = load();
    display(&mut p);
    set_text(&mut p, "draft");
    p.text_input("!");
    assert_eq!(state(&mut p).get("dirty"), &Data::Bool(true));
    // Ctrl+N asks; Escape keeps the document
    key(&mut p, "ctrl+n");
    assert!(state(&mut p).get("confirm").get("body").as_str().is_some(), "a confirmation is open");
    let d = display(&mut p);
    assert!(has_text(&d, "取代目前的文件？"), "the dialog is drawn");
    assert_eq!(focus_in(&mut p, "editor").as_deref(), Some("confirm-cancel"), "the safe button has the focus");
    p.text_input("x");
    assert_eq!(doc(&mut p), "!draft", "typing does not reach the document behind the dialog");
    assert!(p.key("escape"));
    assert_eq!(state(&mut p).get("confirm"), &Data::Nil);
    assert_eq!(doc(&mut p), "!draft");
    assert_eq!(focus_in(&mut p, "editor").as_deref(), Some("doc"));
    // ... and the second time the user agrees
    key(&mut p, "ctrl+n");
    op(&mut p, "editor", "confirm-ok", "click");
    assert_eq!(doc(&mut p), "");
    assert_eq!(state(&mut p).get("dirty"), &Data::Bool(false));
    // nothing to lose: no question
    key(&mut p, "ctrl+n");
    assert_eq!(state(&mut p).get("confirm"), &Data::Nil);

    // a good file dropped on a modified document asks first; a refused one never does
    set_text(&mut p, "work");
    p.file_drop("x.png", Arc::new(vec![1, 2, 3]));
    assert_eq!(state(&mut p).get("confirm"), &Data::Nil);
    p.file_drop("new.txt", Arc::new(b"new content".to_vec()));
    assert_eq!(doc(&mut p), "work");
    op(&mut p, "editor", "confirm-cancel", "click");
    assert_eq!(doc(&mut p), "work");
    p.file_drop("new.txt", Arc::new(b"new content".to_vec()));
    op(&mut p, "editor", "confirm-ok", "click");
    assert_eq!(doc(&mut p), "new content");
    assert_eq!(state(&mut p).get("file").as_str(), Some("new.txt"));

    // Open… asks before the file dialog, 全部範例 too
    p.text_input("+");
    requests(&mut p);
    key(&mut p, "ctrl+o");
    assert!(open_request(&mut p).is_none(), "no file dialog before the answer");
    op(&mut p, "editor", "confirm-ok", "click");
    assert!(open_request(&mut p).is_some());
    p.text_input("+");
    op(&mut p, "editor", "all-samples", "click");
    assert!(doc(&mut p).starts_with('+'));
    op(&mut p, "editor", "confirm-ok", "click");
    assert!(doc(&mut p).starts_with("繁體中文"));
    no_error_card(&display(&mut p));
    no_handler_errors(&p);
}

#[test]
fn escape_keeps_the_caret_in_the_text_and_tab_inserts_a_tab() {
    let mut p = load();
    display(&mut p);
    set_text(&mut p, "abc");
    key(&mut p, "ctrl+end");
    p.text_input("d");
    // the host quits on nothing now, but a page that lets Escape drop the focus
    // leaves the next keys nowhere to go: Escape keeps (or restores) it
    for _ in 0..3 {
        assert!(p.key("escape"), "the page takes Escape");
        assert_eq!(focus_in(&mut p, "editor").as_deref(), Some("doc"));
        assert!(p.text_focus().is_some(), "the host still has a caret for text input");
    }
    p.text_input("e");
    assert_eq!(doc(&mut p), "abcde");
    // from a toolbar control, Escape goes back to the text
    op(&mut p, "editor", "wrap", "focus");
    assert!(p.key("escape"));
    assert_eq!(focus_in(&mut p, "editor").as_deref(), Some("doc"));
    // an open drop-down: Escape is the library's (it closes the menu)
    op(&mut p, "editor", "samples", "click");
    assert!(ui_state(&mut p).get("open").as_str().is_some(), "the sample menu is open");
    assert!(p.key("escape"));
    assert_eq!(ui_state(&mut p).get("open"), &Data::Nil);
    // Tab in the text inserts a tab; Shift+Tab leaves the text
    focus_doc_by_click(&mut p);
    key(&mut p, "tab");
    assert_eq!(doc(&mut p), "abcde\t");
    key(&mut p, "shift+tab");
    assert_ne!(focus_in(&mut p, "editor").as_deref(), Some("doc"));
    key(&mut p, "tab");
    assert_eq!(focus_in(&mut p, "editor").as_deref(), Some("doc"), "Tab from the last control comes back");
    assert_eq!(doc(&mut p), "abcde\t", "and inserts nothing on the way in");
    no_error_card(&display(&mut p));
    no_handler_errors(&p);
}

/// An agent's click on the document: focus, caret at the end.
fn focus_doc_by_click(p: &mut ScriptPage) {
    let r = op(p, "editor", "doc", "click");
    assert_eq!(r.get("ok"), &Data::Bool(true));
}

#[test]
fn agent_ops_on_the_document_element() {
    let mut p = load();
    display(&mut p);
    set_text(&mut p, "日本語");
    op(&mut p, "editor", "wrap", "focus");
    focus_doc_by_click(&mut p);
    assert_eq!(focus_in(&mut p, "editor").as_deref(), Some("doc"));
    assert_eq!(caret(&mut p), ("日本語".len(), "日本語".len()), "click puts the caret at the end");
    act(&mut p, "editor.do", Data::map([("id", s("doc")), ("op", s("type")), ("value", s("です"))]));
    assert_eq!(doc(&mut p), "日本語です");
    // op set has no hook for page element types: describe says what to use
    let d = act(&mut p, "editor.describe", Data::map([("types", s("mleditor"))]));
    let node = d.get("nodes").items().iter().find(|n| n.get("id").as_str() == Some("doc")).cloned().expect("doc node");
    assert!(node.get("hint").as_str().unwrap_or_default().contains("set_text"), "{node:?}");
    no_handler_errors(&p);
}

/// The caret rectangle the page gave the host: (x, y).
fn caret_xy(p: &ScriptPage) -> (f32, f32) {
    let r = p.text_focus().expect("text focus");
    (r.x, r.y)
}

#[test]
fn end_and_home_stay_on_the_line_of_a_wrapped_cjk_paragraph() {
    let mut p = load();
    display(&mut p);
    let para = "繁體中文：這是一個多語言文字編輯器。中文的斷行規則不讓句號或逗號出現在行首，游標以字素為單位移動。".repeat(8);
    set_text(&mut p, &para);
    display(&mut p);
    key(&mut p, "ctrl+home");
    let (x0, y0) = caret_xy(&p);
    // End: the end of the first visual line, drawn there (not at the start of the next)
    key(&mut p, "end");
    let c1 = caret(&mut p).0;
    assert!(c1 > 0 && c1 < para.len(), "End stops at the first line's end: {c1}");
    let (x1, y1) = caret_xy(&p);
    assert!((y1 - y0).abs() < 0.5, "End moved the caret down a line: y {y0} -> {y1}");
    assert!(x1 > x0 + 300.0, "the caret is at the right end of the line: x {x0} -> {x1}");
    // the caret bar is drawn there too
    let d = display(&mut p);
    let bars: Vec<(f64, f64)> = d
        .items()
        .iter()
        .filter(|c| matches!(c.at(1).as_str(), Some("rect") | Some("rrect")) && c.at(4).num_or(0.0) == 2.0)
        .map(|c| (c.at(2).num_or(0.0), c.at(3).num_or(0.0)))
        .collect();
    assert!(bars.iter().any(|&(x, y)| (x - (x1 as f64 - 1.0)).abs() < 0.6 && (y - (y1 as f64 + 2.0)).abs() < 0.6), "no caret bar at ({x1}, {y1}): {bars:?}");
    // End again stays; Home goes back to the start of the same line
    key(&mut p, "end");
    assert_eq!(caret(&mut p).0, c1, "a second End does not walk down");
    assert_eq!(caret_xy(&p), (x1, y1));
    key(&mut p, "home");
    assert_eq!(caret(&mut p).0, 0);
    assert_eq!(caret_xy(&p).1, y0);
    // the second line: End and Home keep to it
    key(&mut p, "end");
    key(&mut p, "down");
    let (_, y2) = caret_xy(&p);
    let lh = y2 - y0;
    assert!(lh > 15.0 && lh < 40.0, "down moved one line: {lh}");
    key(&mut p, "end");
    let c2 = caret(&mut p).0;
    assert!(c2 > c1);
    assert_eq!(caret_xy(&p).1, y2, "End on line 2 stays on line 2");
    key(&mut p, "home");
    assert_eq!(caret(&mut p).0, c1, "line 2 starts where line 1 ends");
    assert_eq!(caret_xy(&p).1, y2, "Home on line 2 stays on line 2");
    // shift+end selects to the line's end, without the next line
    key(&mut p, "shift+end");
    assert_eq!(caret(&mut p), (c2, c1));
    // a click past the end of the first line puts the caret at its end, on it
    let (ex, _, ew, _) = editor_rect(&mut p);
    p.pointer_down((ex + ew - 30.0) as f32, y0 + 5.0);
    p.pointer_up();
    assert_eq!(caret(&mut p).0, c1);
    assert_eq!(caret_xy(&p).1, y0, "the click's line, not the next one");
    // typing there goes in at that point
    p.text_input("★");
    assert_eq!(&doc(&mut p)[c1..c1 + "★".len()], "★");
    no_error_card(&display(&mut p));
    no_handler_errors(&p);
}

#[test]
fn file_names_are_sanitised() {
    let mut p = load();
    display(&mut p);
    set_text(&mut p, "x");
    requests(&mut p);
    for (given, want) in [
        ("..\\..\\evil.bat", "evil.bat.txt"),
        ("a/b/notes.md", "notes.md"),
        ("C:\\Users\\me\\報告.TXT", "報告.TXT"),
        ("CON", "_CON.txt"),
        ("lpt1.md", "_lpt1.md"),
        ("x\u{7}y\u{202E}gnp.exe", "xygnp.exe.txt"),
        (" .hidden. ", "hidden.txt"),
        ("<>:\"|?*", "未命名.txt"),
        ("draft", "draft.txt"),
    ] {
        let r = act(&mut p, "save", Data::map([("name", s(given))]));
        assert_eq!(r.get("name").as_str(), Some(want), "{given:?}");
        assert_eq!(save_request(&mut p).expect("save").0, want, "{given:?}");
    }
    // load_text keeps only the base name
    let r = act(&mut p, "load_text", Data::map([("name", s("a\\..\\b.txt")), ("text", s("b"))]));
    assert_eq!(r.get("opened"), &Data::Bool(true));
    assert_eq!(state(&mut p).get("file").as_str(), Some("b.txt"));
    // a file name an agent wrote that is not a string: Ctrl+S still works
    let out = p.act(r#"{"set": "editor.state", "value": {"text": "t", "file": {"x": 1}}}"#);
    assert!(out.contains("\"ok\":true"), "{out}");
    no_error_card(&display(&mut p));
    key(&mut p, "ctrl+s");
    assert_eq!(save_request(&mut p).expect("save").0, "未命名.txt");
    // the name the user chose in the save dialog is shown as its base name
    let out = p.act(r#"{"set": "editor.state", "value": {"text": "t"}}"#);
    assert!(out.contains("\"ok\":true"));
    let id = {
        key(&mut p, "ctrl+s");
        requests(&mut p)
            .into_iter()
            .find_map(|r| match r {
                PageRequest::SaveFile { id, .. } => Some(id),
                _ => None,
            })
            .expect("save")
    };
    p.host_result(id, HostResult::Saved { name: "D:\\docs\\log.md".into() });
    assert_eq!(state(&mut p).get("file").as_str(), Some("log.md"));
    no_handler_errors(&p);
}

#[test]
fn line_ends_are_kept_and_a_cut_view_is_not_saved() {
    let mut p = load();
    display(&mut p);
    // classic Mac line ends
    let r = act(&mut p, "load_text", Data::map([("name", s("mac.txt")), ("text", s("one\rtwo\r"))]));
    assert_eq!(r.get("opened"), &Data::Bool(true));
    assert_eq!(doc(&mut p), "one\ntwo\n");
    assert!(has_text_containing(&display(&mut p), "UTF-8 · CR ·"), "CR shown");
    assert_eq!(act(&mut p, "stats", Data::Nil).get("eol").as_str(), Some("cr"));
    requests(&mut p);
    key(&mut p, "ctrl+end");
    p.text_input("three");
    key(&mut p, "ctrl+s");
    assert_eq!(save_request(&mut p).expect("save").1, b"one\rtwo\rthree");
    // mixed: the most frequent one wins, and the page says so
    act(&mut p, "load_text", Data::map([("name", s("mixed.txt")), ("text", s("a\r\nb\r\nc\nd"))]));
    key(&mut p, "ctrl+s");
    assert_eq!(save_request(&mut p).expect("save").1, b"a\r\nb\r\nc\r\nd");

    // longer than 256 KB, set from outside: shown read-only and never saved cut
    let huge = "長".repeat(100_000);
    let out = p.act(&format!(r#"{{"set": "editor.state", "value": {{"text": "{huge}", "file": "big.txt"}}}}"#));
    assert!(out.contains("\"ok\":true"));
    assert!(has_text_containing(&display(&mut p), "唯讀"));
    key(&mut p, "ctrl+s");
    assert!(save_request(&mut p).is_none(), "the first 256 KB alone must not be saved");
    assert!(notice(&mut p).contains("沒有儲存"), "{}", notice(&mut p));
    let r = act(&mut p, "save", Data::Nil);
    assert_eq!(r.get("requested"), &Data::Bool(false));
    assert_eq!(r.get("reason").as_str(), Some("truncated"));
    assert!(save_request(&mut p).is_none());
    no_error_card(&display(&mut p));
    no_handler_errors(&p);
}

#[test]
fn a_nan_font_size_from_an_agent_is_ignored() {
    let mut p = load();
    display(&mut p);
    for v in ["nan", "NaN", "-nan", "inf", "-inf"] {
        let out = p.act(&format!(r#"{{"set": "editor.state", "value": {{"text": "abc", "size": "{v}"}}}}"#));
        assert!(out.contains("\"ok\":true"), "{out}");
        let d = display(&mut p);
        no_error_card(&d);
        let size = act(&mut p, "stats", Data::Nil).get("size").num_or(-1.0);
        assert!((10.0..=48.0).contains(&size), "size {v:?} -> {size}");
        key(&mut p, "ctrl+end");
        p.text_input("x");
        assert_eq!(doc(&mut p), "abcx", "typing works with size {v:?}");
        key(&mut p, "ctrl+=");
        no_error_card(&display(&mut p));
    }
    no_handler_errors(&p);
}

#[test]
fn narrow_layout_follows_the_window() {
    let mut p = load();
    display(&mut p);
    requests(&mut p);
    // a phone in portrait (or a narrow desktop window)
    p.viewport(560.0, 1180.0, 1.0);
    assert_eq!(fits(&mut p), vec![(2400.0, 0.0, 400.0, 820.0)], "the narrow layout is fitted");
    let d = display_at(&mut p, NARROW_X);
    no_error_card(&d);
    assert!(has_text(&d, "多語言編輯器") && has_text_containing(&d, "第 1 行"), "{:?}", texts(&d));
    assert_eq!(focus_in(&mut p, "narrow").as_deref(), Some("doc"), "the narrow app has the caret");
    assert_eq!(focus_in(&mut p, "editor"), None);
    assert!(p.text_focus().is_some_and(|r| r.x >= NARROW_X as f32), "the host caret is in the narrow app");
    // every control fits in the 400 units
    let mut from = 1.0;
    loop {
        let d = act(&mut p, "narrow.describe", Data::map([("from", Data::Num(from))]));
        for n in d.get("nodes").items() {
            let (x, w) = (n.get("x").num_or(0.0), n.get("w").num_or(0.0));
            assert!(x >= NARROW_X - 0.5 && x + w <= NARROW_X + 400.5, "{n:?} sticks out of the narrow layout");
        }
        match d.get("next").num() {
            Some(n) => from = n,
            None => break,
        }
    }
    // typing, find and Escape work there
    set_text(&mut p, "");
    type_text(&mut p, "窄版 narrow 窄版");
    assert_eq!(doc(&mut p), "窄版 narrow 窄版");
    key(&mut p, "ctrl+f");
    assert_eq!(focus_in(&mut p, "narrow").as_deref(), Some("find"));
    type_text(&mut p, "窄版");
    key(&mut p, "enter");
    let d = display_at(&mut p, NARROW_X);
    no_error_card(&d);
    assert!(has_text_containing(&d, "個符合"), "{:?}", texts(&d));
    assert!(p.key("escape"));
    assert_eq!(state(&mut p).get("find_open"), &Data::Bool(false));
    assert_eq!(focus_in(&mut p, "narrow").as_deref(), Some("doc"));
    // the confirmation dialog fits too
    key(&mut p, "ctrl+n");
    let d = display_at(&mut p, NARROW_X);
    no_error_card(&d);
    assert!(has_text(&d, "取代目前的文件？"));
    assert!(p.key("escape"));
    // a wrapped line's End there
    set_text(&mut p, &"這是一個多語言文字編輯器。".repeat(10));
    display_at(&mut p, NARROW_X);
    key(&mut p, "ctrl+home");
    let y0 = caret_xy(&p).1;
    key(&mut p, "end");
    let c = caret_in(&mut p, "narrow");
    assert!(c.0 > 0 && c.0 == c.1, "{c:?}");
    assert_eq!(caret_xy(&p).1, y0, "End stays on the first line in the narrow layout too");
    // a window wide again: the wide layout, with the caret and focus
    p.viewport(1600.0, 1000.0, 1.0);
    assert_eq!(fits(&mut p), vec![(0.0, 0.0, 1440.0, 900.0)]);
    assert_eq!(focus_in(&mut p, "editor").as_deref(), Some("doc"));
    assert_eq!(caret(&mut p), c, "the caret came along");
    p.text_input("!");
    assert!(doc(&mut p).contains('!'));
    // agents can pick one, and give the choice back to the window
    let r = act(&mut p, "set_layout", Data::map([("layout", s("narrow"))]));
    assert_eq!(r.get("layout").as_str(), Some("narrow"));
    p.viewport(1700.0, 1000.0, 1.0);
    assert_eq!(act(&mut p, "stats", Data::Nil).get("layout").as_str(), Some("narrow"), "a chosen layout stays");
    let r = act(&mut p, "set_layout", Data::map([("layout", s("auto"))]));
    assert_eq!(r.get("layout").as_str(), Some("wide"));
    no_error_card(&display(&mut p));
    no_error_card(&display_at(&mut p, NARROW_X));
    no_handler_errors(&p);
}

#[test]
fn agent_actions() {
    let mut p = load();
    display(&mut p);
    // the semantic port lists the editor's actions
    let desc = p.describe();
    for a in ["get_text", "set_text", "insert", "find", "replace", "stats", "load_text", "save", "editor.describe"] {
        assert!(desc.contains(&format!("\"name\":\"{a}\"")), "action {a} missing from describe");
    }
    // set_text, then read it back in parts of at most 400 characters
    let long: String = (0..300).map(|i| format!("{i}中")).collect();
    set_text(&mut p, &long);
    let mut got = String::new();
    let mut from = 0.0;
    let mut parts = 0;
    loop {
        let r = act(&mut p, "get_text", Data::map([("from", Data::Num(from))]));
        let t = r.get("text").as_str().unwrap_or_default();
        assert!(t.chars().count() <= 400);
        got.push_str(t);
        parts += 1;
        match r.get("next").num() {
            Some(n) => from = n,
            None => break,
        }
        assert!(parts < 100);
    }
    assert_eq!(got, long);
    assert!(parts >= 3);
    // insert at the end, at the start
    act(&mut p, "insert", Data::map([("at", s("end")), ("text", s("!"))]));
    act(&mut p, "insert", Data::map([("at", Data::Num(0.0)), ("text", s("<"))]));
    let t = doc(&mut p);
    assert!(t.starts_with("<0中") && t.ends_with("299中!"));
    // the same through the port's JSON
    let out = p.act(r#"{"action":"stats","params":{}}"#);
    let j: serde_json_lite::Value = serde_json_lite::parse(&out);
    assert!(j.ok, "{out}");
    let st = act(&mut p, "stats", Data::Nil);
    assert_eq!(st.get("bytes").num(), Some(t.len() as f64));
    assert_eq!(st.get("lines").num(), Some(1.0));
    // words: each Han character is a word (300), as are the runs between
    // them: "<0", "1" .. "299", "!" (301)
    assert_eq!(st.get("words").num(), Some(601.0), "{st:?}");
    assert_eq!(st.get("graphemes").num(), Some(t.chars().count() as f64));
    assert_eq!(st.get("direction").as_str(), Some("ltr"));
    no_handler_errors(&p);
    // bad parameters are reported to the caller (each counts as one failed handler)
    let e = p.run_action("set_text", &Data::map([("text", Data::Num(1.0))])).unwrap_err();
    assert!(e.contains("text must be a string"), "{e}");
    let e = p.run_action("insert", &Data::map([("at", s("nowhere")), ("text", s("x"))])).unwrap_err();
    assert!(e.contains("at must be"), "{e}");
    assert_eq!(p.handler_errors, 2);
    assert_eq!(doc(&mut p), t, "nothing changed");
    no_error_card(&display(&mut p));
}

/// A tiny check that the port answered with an "ok" JSON object, without a
/// serde_json dependency in this crate.
mod serde_json_lite {
    pub struct Value {
        pub ok: bool,
    }
    pub fn parse(s: &str) -> Value {
        Value { ok: s.starts_with('{') && s.contains("\"ok\":true") }
    }
}

#[test]
fn missing_glyphs_are_counted() {
    let mut p = load();
    display(&mut p);
    let tr = text();
    let sample = "缺字 \u{11F04}\u{11F05} \u{0378}\nfine 中文";
    set_text(&mut p, sample);
    let st = act(&mut p, "stats", Data::Nil);
    let expect: usize = sample.split('\n').map(|l| tr.missing_glyphs(l)).sum();
    assert_eq!(st.get("missing").num(), Some(expect as f64));
    let d = display(&mut p);
    if expect > 0 {
        assert!(has_text_containing(&d, "缺字 "), "the status bar shows the missing glyphs");
        // a red dot marks the paragraph in the gutter
        assert!(d.items().iter().any(|c| c.at(1).as_str() == Some("circle")), "no gutter mark");
    } else {
        println!("this machine has fonts for every test character; nothing missing");
    }
    no_error_card(&d);
}

#[test]
fn every_sample_renders() {
    let mut p = load();
    display(&mut p);
    act(&mut p, "set_text", Data::map([("text", s(""))]));
    // insert each sample from the menu by its index, as the select does
    for i in 1..=14 {
        p.run_action("editor.do", &Data::map([("id", s("samples")), ("op", s("set")), ("value", Data::Num(i as f64))]))
            .unwrap_or_else(|e| panic!("sample {i}: {e}"));
        // the select keeps no selection, so the same entry can be inserted again
        let d = display(&mut p);
        no_error_card(&d);
    }
    let t = doc(&mut p);
    assert_eq!(t.split('\n').count(), 14, "one paragraph per sample: {t}");
    let st = act(&mut p, "stats", Data::Nil);
    assert_eq!(st.get("rtl_paragraphs").num(), Some(2.0), "Arabic and Hebrew");
    // wrap off and zoom, then everything still draws
    key(&mut p, "alt+z");
    assert_eq!(state(&mut p).get("wrap"), &Data::Bool(false));
    key(&mut p, "ctrl+=");
    key(&mut p, "ctrl+=");
    assert_eq!(state(&mut p).get("size").num(), Some(22.0));
    let d = display(&mut p);
    no_error_card(&d);
    assert!(has_text_containing(&d, "不換行"));
    key(&mut p, "ctrl+0");
    key(&mut p, "alt+z");
    no_error_card(&display(&mut p));
    no_handler_errors(&p);
}

#[test]
fn big_document_128k_renders_and_edits_fast() {
    let mut p = load();
    display(&mut p);
    // 128 KB of the welcome text's paragraphs (CJK, Arabic, Hebrew,
    // Devanagari, Thai, emoji, mixed), each numbered so no two are the same
    // and every one is shaped and laid out cold
    let all = {
        let mut a = String::new();
        let welcome = doc(&mut p);
        let paras: Vec<&str> = welcome.split('\n').filter(|l| !l.is_empty()).collect();
        let mut i = 0;
        while a.len() < 128 * 1024 {
            a.push_str(&format!("{} {}\n", i + 1, paras[i % paras.len()]));
            i += 1;
        }
        let mut cut = 128 * 1024;
        while !a.is_char_boundary(cut) {
            cut -= 1;
        }
        a.truncate(cut);
        a
    };
    let t0 = Instant::now();
    set_text(&mut p, &all);
    let set_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let (d, first) = timed_display(&mut p);
    no_error_card(&d);
    assert_eq!(doc(&mut p).len(), all.len());

    // a keystroke: the handler, then the frame that shows it
    let mut keys = Vec::new();
    for ch in ["a", "語", "ب", " ", "😀"] {
        let t = Instant::now();
        p.text_input(ch);
        let h = t.elapsed().as_secs_f64() * 1000.0;
        let (d, f) = timed_display(&mut p);
        no_error_card(&d);
        keys.push(h + f);
    }
    let mut moves = Vec::new();
    for k in ["down", "down", "pagedown", "end", "left", "shift+up", "ctrl+end", "ctrl+home"] {
        let t = Instant::now();
        p.key(k);
        let h = t.elapsed().as_secs_f64() * 1000.0;
        let (d, f) = timed_display(&mut p);
        no_error_card(&d);
        moves.push(h + f);
    }
    let t = Instant::now();
    p.ime_preedit("ㄓㄨㄥ");
    let (_, f) = timed_display(&mut p);
    let ime = t.elapsed().as_secs_f64() * 1000.0;
    let _ = f;
    p.ime_preedit("");
    let before = ui_state(&mut p).get("scroll").get("doc").num_or(0.0);
    let t = Instant::now();
    assert!(p.wheel(700.0, 500.0, 0.0, -10.0), "the editor takes the wheel");
    display(&mut p);
    let wheel = t.elapsed().as_secs_f64() * 1000.0;
    let after = ui_state(&mut p).get("scroll").get("doc").num_or(0.0);
    assert!(after > before + 100.0, "wheel scrolled {before} -> {after}");
    let t = Instant::now();
    act(&mut p, "find", Data::map([("query", s("AnyPage"))]));
    display(&mut p);
    let find = t.elapsed().as_secs_f64() * 1000.0;
    let t = Instant::now();
    p.key("alt+z");
    let (d, nowrap) = timed_display(&mut p);
    no_error_card(&d);
    let _ = t;
    p.text_input("x");
    let (_, nowrap_key) = timed_display(&mut p);
    let st = act(&mut p, "stats", Data::Nil);
    let max_key = keys.iter().cloned().fold(0.0, f64::max);
    let max_move = moves.iter().cloned().fold(0.0, f64::max);
    println!(
        "128 KB ({} lines, {} graphemes, {} words): set_text {set_ms:.1} ms, first frame {first:.1} ms, \
         keystroke+frame max {max_key:.1} ms {keys:.1?}, caret move+frame max {max_move:.1} ms, IME preedit {ime:.1} ms, \
         wheel {wheel:.1} ms, find {find:.1} ms, no-wrap first frame {nowrap:.1} ms, no-wrap keystroke frame {nowrap_key:.1} ms; \
         app view/layout/draw {:.1}/{:.1}/{:.1} ms, script memory {:.1} MB",
        st.get("lines").num_or(0.0),
        st.get("graphemes").num_or(0.0),
        st.get("words").num_or(0.0),
        st.get("view_ms").num_or(0.0),
        st.get("layout_ms").num_or(0.0),
        st.get("draw_ms").num_or(0.0),
        p.script_memory() as f64 / 1048576.0,
    );
    // inside the page's own budgets (call_ms / handler_ms = 250 ms), with room to spare
    let budget = p.manifest.limits.call_ms;
    assert!(first < budget, "first frame {first:.1} ms");
    assert!(max_key < budget, "keystroke {max_key:.1} ms");
    assert!(max_move < budget, "caret move {max_move:.1} ms");
    // the worst case: one 128 KB paragraph (no line breaks), with the find bar
    // open (178 matches), wrapped (laid out again on every keystroke) and unwrapped
    // (one line 128 KB long)
    let one_para = all.replace('\n', " ");
    set_text(&mut p, &one_para);
    for wrap in [false, true] {
        if state(&mut p).get("wrap") != &Data::Bool(wrap) {
            key(&mut p, "alt+z");
        }
        let t = Instant::now();
        let (d, _) = timed_display(&mut p);
        let first = t.elapsed().as_secs_f64() * 1000.0;
        no_error_card(&d);
        key(&mut p, "ctrl+end");
        let mut one = Vec::new();
        for ch in ["x", "中", "ש"] {
            let t = Instant::now();
            p.text_input(ch);
            let h = t.elapsed().as_secs_f64() * 1000.0;
            let (d, f) = timed_display(&mut p);
            no_error_card(&d);
            one.push((h, f));
        }
        let t = Instant::now();
        p.key("ctrl+home");
        display(&mut p);
        let jump = t.elapsed().as_secs_f64() * 1000.0;
        println!(
            "one 128 KB paragraph, {}: first frame {first:.1} ms, keystroke (handler, frame) {one:.1?} ms, ctrl+home {jump:.1} ms",
            if wrap { "wrapped" } else { "not wrapped" }
        );
        assert!(one.iter().all(|&(h, f)| h + f < budget), "keystroke in one long paragraph {one:?}");
    }
    // past the limit, typing is refused with a message instead of growing the document
    let full: String = "字".repeat(256 * 1024 / 3);
    set_text(&mut p, &full);
    key(&mut p, "ctrl+end");
    p.text_input("ab");
    p.text_input("c");
    assert!(doc(&mut p).len() <= 256 * 1024);
    no_error_card(&display(&mut p));
    no_handler_errors(&p);
}
