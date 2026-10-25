//! Layout geometry, checked numerically: rows, columns, grids, stacks,
//! padding, gap, align, justify, grow, shrink, fixed and min/max sizes, text
//! wrapping. Boxes are fixed-size canvases so the numbers are exact.

mod common;

use anypage_runtime::{Data, ScriptPage};
use common::*;

/// One app per case, each 400 x 300 at (0, 1000 * k); views keyed by case name.
const PAGE: &str = r#"
local ui = import("ui")
local function box(id, w, h, extra)
    local o = { id = id, width = w, height = h }
    for k, v in extra or {} do o[k] = v end
    return ui.canvas(o)
end
local LONG = "the quick brown fox jumps over the lazy dog and keeps on running far away"
local cases = {
    column = function()
        return ui.column({ padding = 10, gap = 5 }, {
            box("a", 100, 20), box("b", 50, 30),
            ui.column({ id = "centered", align = "center", width = 300, height = 60 }, { box("c", 100, 20) }),
        })
    end,
    row = function()
        return ui.column({}, {
            ui.row({ id = "r1", width = 400, height = 40 }, { box("a", 100, 20), ui.spacer(), box("b", 50, 20) }),
            ui.row({ id = "r2", width = 300, height = 40 }, { box("g1", 10, 10, { grow = 1, width = false }), box("g2", 10, 10, { grow = 2, width = false }) }),
            ui.row({ id = "r3", width = 350, justify = "between" }, { box("j1", 50, 10), box("j2", 50, 10), box("j3", 50, 10) }),
            ui.row({ id = "r4", width = 400, height = 80, align = "stretch", gap = 10 }, { box("s1", 50, nil), box("s2", 50, nil) }),
            ui.row({ id = "r5", width = 400, height = 80, align = "end" }, { box("e1", 50, 20) }),
        })
    end,
    justify = function()
        return ui.column({ id = "col", height = 300, justify = "center" }, { box("m", 40, 100) })
    end,
    grid = function()
        return ui.grid({ id = "g", columns = 3, gap = 10, width = 320 }, {
            box("c1", nil, 30), box("c2", nil, 50), box("c3", nil, 20), box("c4", nil, 10),
        })
    end,
    sizes = function()
        return ui.column({ align = "stretch" }, {
            box("max", nil, 10, { max_w = 50 }),
            box("min", nil, 10, { min_w = 500 }),
            box("fixed", 120, 40),
        })
    end,
    text = function()
        return ui.column({ width = 200 }, { ui.text(LONG, { id = "t" }), ui.text("short", { id = "s" }) })
    end,
    shrink = function()
        return ui.row({ id = "row", width = 300, gap = 10, align = "start" }, {
            ui.text(LONG, { id = "t" }),
            ui.button("Go", { id = "b" }),
        })
    end,
    stack = function()
        return ui.stack({ id = "st", width = 200, height = 100, align = "center" }, { box("under", 200, 100), box("over", 40, 20) })
    end,
    fill = function()
        return ui.column({ id = "root", padding = 20, gap = 10 }, {
            box("head", 50, 30),
            ui.scroll({ id = "sc" }, ui.column({}, { box("tall", 100, 1000) })),
            ui.textfield({ id = "tf" }),
            ui.button("natural", { id = "btn" }),
        })
    end,
    card = function()
        return ui.card({ id = "card", width = 300 }, { box("inside", 50, 20) })
    end,
}
local k = 0
for name, view in cases do
    ui.app({ name = name, rect = { 0, 1000 * k, 400, 300 }, agent = true, view = view })
    k += 1
end
"#;

fn page() -> ScriptPage {
    load_pkg(&manifest(400.0, 12000.0, ""), PAGE).unwrap_or_else(|e| panic!("{e}"))
}

/// App-local rectangle of element `id` in app `app`.
fn r(p: &mut ScriptPage, app: &str, id: &str) -> (f64, f64, f64, f64) {
    let d = describe(p, app);
    let ox = d.get("rect").at(1).num_or(0.0);
    let oy = d.get("rect").at(2).num_or(0.0);
    let n = node(&d, id);
    (n.get("x").num_or(0.0) - ox, n.get("y").num_or(0.0) - oy, n.get("w").num_or(0.0), n.get("h").num_or(0.0))
}

#[test]
fn columns_padding_gap_align() {
    let mut p = page();
    assert_eq!(r(&mut p, "column", "a"), (10.0, 10.0, 100.0, 20.0));
    assert_eq!(r(&mut p, "column", "b"), (10.0, 35.0, 50.0, 30.0));
    let c = r(&mut p, "column", "centered");
    assert_eq!((c.2, c.3), (300.0, 60.0));
    assert_eq!(r(&mut p, "column", "c"), (c.0 + 100.0, c.1, 100.0, 20.0));
}

#[test]
fn rows_spacer_grow_justify_align() {
    let mut p = page();
    let r1 = r(&mut p, "row", "r1");
    assert_eq!(r(&mut p, "row", "a").0, r1.0);
    // centred vertically in the 40-high row
    assert_eq!(r(&mut p, "row", "a").1, r1.1 + 10.0);
    // the spacer pushes b to the right edge
    assert_eq!(r(&mut p, "row", "b").0, r1.0 + 350.0);
    // grow 1 : 2 shares 300
    assert_eq!(r(&mut p, "row", "g1").2, 100.0);
    assert_eq!(r(&mut p, "row", "g2").2, 200.0);
    assert_eq!(r(&mut p, "row", "g2").0, 100.0);
    // between: 0, 150, 300
    let xs: Vec<f64> = ["j1", "j2", "j3"].iter().map(|id| r(&mut p, "row", id).0).collect();
    assert_eq!(xs, [0.0, 150.0, 300.0]);
    // stretch: full height, gap 10
    let (s1, s2) = (r(&mut p, "row", "s1"), r(&mut p, "row", "s2"));
    assert_eq!((s1.3, s2.3, s2.0 - s1.0), (80.0, 80.0, 60.0));
    // end: bottom of the row
    let r5 = r(&mut p, "row", "r5");
    assert_eq!(r(&mut p, "row", "e1").1, r5.1 + 60.0);
}

#[test]
fn justify_grid_sizes() {
    let mut p = page();
    assert_eq!(r(&mut p, "justify", "m").1, 100.0);
    // grid: 3 columns of (320 - 20) / 3 = 100, gap 10; row 2 starts after the
    // tallest of row 1 (50); fixed heights are kept, widths fill the column
    assert_eq!(r(&mut p, "grid", "c1"), (0.0, 0.0, 100.0, 30.0));
    assert_eq!(r(&mut p, "grid", "c2").0, 110.0);
    assert_eq!(r(&mut p, "grid", "c3").0, 220.0);
    assert_eq!(r(&mut p, "grid", "c4"), (0.0, 60.0, 100.0, 10.0));
    // stretch respects max_w, min_w wins over the available width, fixed stays fixed
    assert_eq!(r(&mut p, "sizes", "max").2, 50.0);
    assert_eq!(r(&mut p, "sizes", "min").2, 500.0);
    assert_eq!(r(&mut p, "sizes", "fixed").2, 120.0);
}

#[test]
fn text_wraps_and_rows_shrink_it() {
    let mut p = page();
    let lh = metrics().line_height(15.0) as f64;
    let t = r(&mut p, "text", "t");
    assert!(t.2 <= 200.0 && t.3 >= 3.0 * lh - 0.01, "wrapped text {t:?} (line height {lh})");
    assert_eq!(r(&mut p, "text", "s").3, lh);
    assert_eq!(r(&mut p, "text", "s").1, t.1 + t.3);
    // in a row, the text gives way to the button, which keeps its natural width
    let b = r(&mut p, "shrink", "b");
    let t = r(&mut p, "shrink", "t");
    let go = metrics().measure("Go", 15.0) as f64 + 32.0;
    assert!((b.2 - go).abs() < 0.01, "button {b:?}");
    assert!((t.2 + 10.0 + b.2 - 300.0).abs() < 0.01, "text {t:?} button {b:?}");
    assert!(t.3 > lh, "shrunk text should wrap");
    assert!((b.0 - (t.0 + t.2 + 10.0)).abs() < 0.01);
}

#[test]
fn stack_fill_and_cards() {
    let mut p = page();
    assert_eq!(r(&mut p, "stack", "over"), (80.0, 40.0, 40.0, 20.0));
    // a scroll without a height takes what is left of a fixed-height column
    let head = r(&mut p, "fill", "head");
    let sc = r(&mut p, "fill", "sc");
    let tf = r(&mut p, "fill", "tf");
    let btn = r(&mut p, "fill", "btn");
    assert_eq!(head, (20.0, 20.0, 50.0, 30.0));
    assert_eq!(sc.1, 60.0);
    assert!((btn.1 + btn.3 - 280.0).abs() < 0.01, "button ends at the padding: {btn:?}");
    assert!((tf.1 - (sc.1 + sc.3 + 10.0)).abs() < 0.01);
    // text fields and scrolls fill the width; buttons keep their natural width
    assert_eq!((sc.2, tf.2), (360.0, 360.0));
    assert!(btn.2 < 200.0);
    // card padding is the theme's pad + 4
    let card = r(&mut p, "card", "card");
    assert_eq!(r(&mut p, "card", "inside"), (card.0 + 16.0, card.1 + 16.0, 50.0, 20.0));
    let _ = Data::Nil;
}
