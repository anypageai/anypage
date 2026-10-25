//! Virtualised lists and tables, scroll containers: wheel, drag, scrollbar,
//! keyboard navigation, selection.

mod common;

use anypage_engine::{Page, PointerResponse};
use anypage_runtime::ScriptPage;
use common::*;

const LIST: &str = r#"
local ui = import("ui")
local app
app = ui.app({
    name = "l", rect = { 0, 0, 600, 500 }, agent = true, state = {},
    view = function(s)
        return ui.column({ padding = 10, gap = 10 }, {
            ui.list({ id = "list", count = 100000, height = 280, row_height = 28, selected = s.sel,
                item = function(i) return { title = "Row " .. i } end,
                key = function(item, i) return "k" .. i end,
                row = function(item, i, selected, hovered)
                    return ui.row({}, { ui.text(item.title), ui.spacer(), ui.text(if selected then "*" else "") })
                end,
                on_select = function(item, i) app:set("sel", i) end,
                on_activate = function(item, i) app:set("opened", item.title) end }),
            ui.button("below", { id = "below" }),
        })
    end,
})
"#;

fn sel(p: &mut ScriptPage) -> f64 {
    state(p, "l").get("sel").num().unwrap_or(0.0)
}

fn scroll_of(p: &mut ScriptPage, app: &str, id: &str) -> f64 {
    ui_state(p, app).get("scroll").get(id).num().unwrap_or(0.0)
}

fn rows_drawn(p: &mut ScriptPage) -> Vec<String> {
    texts(&display(p, 0)).into_iter().filter(|t| t.starts_with("Row ")).collect()
}

#[test]
fn list_renders_only_visible_rows_and_scrolls() {
    let mut p = load(LIST);
    let rows = rows_drawn(&mut p);
    // 280 / 28 = 10 rows in view, of 100 000
    assert_eq!(rows.len(), 10, "{rows:?}");
    assert_eq!(rows[0], "Row 1");
    let d = describe(&mut p, "l");
    assert_eq!(node(&d, "list").get("count").num(), Some(100000.0));
    // rows have stable keyed ids
    assert!(try_node(&d, "list#k3").is_some());

    // wheel down 3 lines = 120 units = row 5 at the top (partly row 5)
    let (cx, cy) = center(&node(&d, "list"));
    assert!(p.wheel(cx, cy, 0.0, -3.0));
    assert_eq!(scroll_of(&mut p, "l", "list"), 120.0);
    let rows = rows_drawn(&mut p);
    assert_eq!(rows[0], "Row 5");
    assert_eq!(rows.len(), 11); // partial rows at both edges
    // the wheel at the top edge is still consumed (no zooming under a list)
    assert!(p.wheel(cx, cy, 0.0, 50.0));
    assert_eq!(scroll_of(&mut p, "l", "list"), 0.0);
    assert!(p.wheel(cx, cy, 0.0, 1.0));
    // outside the list (on the app background) the wheel is not consumed
    assert!(!p.wheel(590.0, 490.0, 0.0, -1.0));
    assert_eq!(p.handler_errors, 0);
}

#[test]
fn list_selection_by_pointer_and_keys() {
    let mut p = load(LIST);
    let d = describe(&mut p, "l");
    let (x, y) = center(&node(&d, "list#k3"));
    assert_eq!(p.pointer_down(x, y), PointerResponse::Captured);
    p.pointer_up();
    assert_eq!(sel(&mut p), 3.0);
    assert!(has_text(&display(&mut p, 0), "*"));
    // focused by the click: keys navigate
    assert_eq!(ui_state(&mut p, "l").get("focus").as_str(), Some("list"));
    assert!(p.key("down"));
    assert_eq!(sel(&mut p), 4.0);
    assert!(p.key("pagedown"));
    assert_eq!(sel(&mut p), 13.0);
    // the selection is kept in view
    assert!(scroll_of(&mut p, "l", "list") > 0.0);
    assert!(rows_drawn(&mut p).contains(&"Row 13".to_string()));
    assert!(p.key("end"));
    assert_eq!(sel(&mut p), 100000.0);
    assert_eq!(scroll_of(&mut p, "l", "list"), 100000.0 * 28.0 - 280.0);
    assert_eq!(rows_drawn(&mut p).last().map(String::as_str), Some("Row 100000"));
    assert!(p.key("home"));
    assert_eq!(sel(&mut p), 1.0);
    assert_eq!(scroll_of(&mut p, "l", "list"), 0.0);
    assert!(p.key("enter"));
    assert_eq!(state(&mut p, "l").get("opened").as_str(), Some("Row 1"));
    // hovering a row highlights that row
    let (hx, hy) = center(&node(&describe(&mut p, "l"), "list#k5"));
    p.pointer_move(hx, hy);
    assert_eq!(ui_state(&mut p, "l").get("hover").as_str(), Some("list#k5"));
    assert!(!rects_of_color(&display(&mut p, 0), rgba(255, 255, 255, 14)).is_empty(), "row hover colour");
    assert_eq!(p.handler_errors, 0);
}

#[test]
fn list_touch_drag_and_scrollbar() {
    let mut p = load(LIST);
    let d = describe(&mut p, "l");
    let (x, y) = center(&node(&d, "list#k5"));
    p.pointer_down(x, y);
    p.pointer_drag(x, y - 12.0); // past the slop: becomes a scroll drag
    p.pointer_drag(x, y - 112.0);
    p.pointer_up();
    assert_eq!(scroll_of(&mut p, "l", "list"), 100.0);
    // a drag is not a click: nothing got selected
    assert_eq!(sel(&mut p), 0.0);
    // the scrollbar along the right edge: dragging its thumb to the bottom reaches the end
    let n = node(&d, "list");
    let (rx, top, h) = ((n.get("x").num_or(0.0) + n.get("w").num_or(0.0) - 4.0) as f32, n.get("y").num_or(0.0) as f32, n.get("h").num_or(0.0) as f32);
    p.pointer_down(rx, top + 10.0);
    p.pointer_drag(rx, top + h + 400.0);
    p.pointer_up();
    assert_eq!(scroll_of(&mut p, "l", "list"), 100000.0 * 28.0 - 280.0);
}

const TABLE: &str = r#"
local ui = import("ui")
local ROWS = {}
for i = 1, 500 do ROWS[i] = { i, "name " .. (501 - i), i * 1.5, i % 2 == 0, "c5", "c6", "c7", "c8" } end
local app
app = ui.app({
    name = "tb", rect = { 0, 0, 700, 400 }, agent = true, state = {},
    view = function(s)
        local rows = ROWS
        if s.sort then
            rows = table.clone(ROWS)
            local c, desc = s.sort.col, s.sort.desc
            table.sort(rows, function(a, b) if desc then return a[c] > b[c] end return a[c] < b[c] end)
        end
        return ui.column({ padding = 10 }, {
            ui.table({ id = "t", height = 300, widths = { 80, 200, 120, 100, 120, 120, 120, 120 },
                data = { columns = { "ID", "Name", "Value", "Even", "E", "F", "G", "H" }, rows = rows },
                selected = s.sel, sort = s.sort,
                on_sort = function(c, desc) app:set("sort", { col = c, desc = desc }) end,
                on_select = function(row, i) app:set("sel", i) app:set("picked", row[2]) end }),
        })
    end,
})
"#;

#[test]
fn table_header_rows_sort_select_and_scroll() {
    let mut p = load(TABLE);
    let cmds = display(&mut p, 0);
    for h in ["ID", "Name", "Value", "Even"] {
        assert!(has_text(&cmds, h), "header {h}");
    }
    assert!(has_text(&cmds, "name 500"));
    assert!(has_text(&cmds, "1.5"));
    assert!(has_text(&cmds, "TRUE"));
    // only the visible rows are drawn
    let names = texts(&cmds).into_iter().filter(|t| t.starts_with("name ")).count();
    assert!(names <= 10, "{names} rows drawn");
    let d = describe(&mut p, "tb");
    let t = node(&d, "t");
    let (tx, ty) = (t.get("x").num_or(0.0) as f32, t.get("y").num_or(0.0) as f32);
    // click the "Name" header (second column, x 80..280): sort ascending, then descending
    click(&mut p, tx + 150.0, ty + 10.0);
    let sort = state(&mut p, "tb").get("sort").clone();
    assert_eq!((sort.get("col").num(), sort.get("desc").as_bool()), (Some(2.0), Some(false)));
    assert!(has_text(&display(&mut p, 0), "name 1"));
    click(&mut p, tx + 150.0, ty + 10.0);
    assert_eq!(state(&mut p, "tb").get("sort").get("desc").as_bool(), Some(true));
    // select the second row (header 36, rows 32)
    click(&mut p, tx + 40.0, ty + 36.0 + 32.0 + 16.0);
    assert_eq!(state(&mut p, "tb").get("sel").num(), Some(2.0));
    assert_eq!(state(&mut p, "tb").get("picked").as_str(), Some("name 98"));
    assert!(p.key("down"));
    assert_eq!(state(&mut p, "tb").get("sel").num(), Some(3.0));
    // horizontal scrolling: the columns are wider than the table
    assert!(p.wheel(tx + 100.0, ty + 100.0, -2.0, 0.0));
    assert_eq!(ui_state(&mut p, "tb").get("scrollx").get("t").num(), Some(80.0));
    assert!(p.key("left"));
    assert_eq!(ui_state(&mut p, "tb").get("scrollx").get("t").num(), Some(40.0));
    // vertical
    assert!(p.wheel(tx + 100.0, ty + 100.0, 0.0, -1.0));
    assert_eq!(ui_state(&mut p, "tb").get("scroll").get("t").num(), Some(40.0));
    assert_eq!(p.handler_errors, 0);
}

const SCROLL: &str = r#"
local ui = import("ui")
ui.app({
    name = "s", rect = { 0, 0, 400, 300 }, agent = true, state = {},
    view = function(s)
        local items = {}
        for i = 1, 40 do
            items[i] = ui.button("Item " .. i, { id = "b" .. i, variant = "secondary" })
        end
        return ui.column({ padding = 10, gap = 10 }, {
            ui.text("header", { id = "header" }),
            ui.scroll({ id = "sc", grow = 1 }, ui.column({ gap = 4 }, items)),
        })
    end,
})
"#;

#[test]
fn scroll_container_wheel_drag_and_focus_reveal() {
    let mut p = load(SCROLL);
    let d = describe(&mut p, "s");
    let sc = node(&d, "sc");
    // the scroll fills the space below the header and clips its content
    let (sy0, sh) = (sc.get("y").num_or(0.0), sc.get("h").num_or(0.0));
    assert!((sy0 + sh - 290.0).abs() < 0.5, "scroll ends at the bottom padding: {sy0} + {sh}");
    assert_eq!(sc.get("content_h").num().map(|h| h > sh), Some(true));
    let cmds = display(&mut p, 0);
    assert!(has_text(&cmds, "Item 1"));
    assert!(!has_text(&cmds, "Item 40"), "content outside the viewport was drawn");
    let (cx, cy) = center(&sc);
    assert!(p.wheel(cx, cy, 0.0, -2.0));
    assert_eq!(scroll_of(&mut p, "s", "sc"), 80.0);
    // over the header (no scroller) the wheel is not consumed
    let (hx, hy) = center(&node(&d, "header"));
    assert!(!p.wheel(hx, hy, 0.0, -1.0));
    // a drag that starts on a button scrolls instead of clicking it; the
    // first 8 units are slop, then the content follows the pointer
    let (bx, by) = center(&node(&describe(&mut p, "s"), "b5"));
    p.pointer_down(bx, by);
    assert_eq!(ui_state(&mut p, "s").get("press").as_str(), Some("b5"));
    p.pointer_drag(bx, by - 20.0);
    assert!(ui_state(&mut p, "s").get("press").is_nil());
    p.pointer_drag(bx, by - 70.0);
    p.pointer_up();
    assert_eq!(scroll_of(&mut p, "s", "sc"), 130.0);
    // a drag on the content between items scrolls at once
    let (gx, gy) = (bx + 150.0, by);
    p.pointer_down(gx, gy);
    p.pointer_drag(gx, gy - 10.0);
    p.pointer_up();
    let after = scroll_of(&mut p, "s", "sc");
    assert_eq!(after, 140.0);
    // Tab to the last button scrolls it into view
    assert!(p.run_action("s.do", &anypage_runtime::Data::map([("id", anypage_runtime::Data::str("b40")), ("op", anypage_runtime::Data::str("focus"))])).is_ok());
    let r = p.run_action("s.do", &anypage_runtime::Data::map([("op", anypage_runtime::Data::str("key")), ("value", anypage_runtime::Data::str("shift+tab"))]));
    assert!(r.is_ok());
    assert!(p.key("tab"));
    assert_eq!(ui_state(&mut p, "s").get("focus").as_str(), Some("b40"));
    let b = node(&describe(&mut p, "s"), "b40");
    let (by, bh) = (b.get("y").num_or(0.0), b.get("h").num_or(0.0));
    assert!(by >= sy0 - 0.5 && by + bh <= sy0 + sh + 0.5, "b40 at {by} not inside {sy0}..{}", sy0 + sh);
    assert!(has_text(&display(&mut p, 0), "Item 40"));
    assert_eq!(p.handler_errors, 0);
}
