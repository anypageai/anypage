//! The `sheet` module driven through real pages, headless: the Luau API,
//! versions and recomputation, jobs, errors, and the demo package.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anypage_core::{CellId, Status};
use anypage_engine::{HostResult, Image, Page, PageRequest, PointerResponse, TextRaster, Wake};
use anypage_runtime::data::input_to_data;
use anypage_runtime::{Data, Natives, Package, ScriptPage};

fn natives() -> Natives {
    let mut n = Natives::new();
    anypage_sheet::register(&mut n);
    n
}

fn text() -> Arc<TextRaster> {
    Arc::new(TextRaster::load().expect("a system font"))
}

fn wake() -> Wake {
    Arc::new(|| {})
}

const PANEL: &str = "page.panel({ rect = { 0, 0, 10, 10 }, draw = function(d) end })\n";

fn load(caps: &str, src: &str) -> Result<ScriptPage, String> {
    let m = format!(r#"{{ "format": 1, "id": "test", "bounds": [0, 0, 400, 300], "capabilities": {caps}, "limits": {{ "handler_ms": 1000, "load_ms": 5000 }} }}"#);
    let src = format!("{PANEL}{src}");
    let pkg = Package::from_files("test", [("page.json", m.as_str()), ("main.luau", src.as_str())])?;
    ScriptPage::load(&pkg, &natives(), text(), wake())
}

const SHEET: &str = r#"{"modules": ["sheet"]}"#;

fn wait_for(p: &mut ScriptPage, cell: CellId, mut ok: impl FnMut(&Data) -> bool) -> Data {
    let t0 = Instant::now();
    loop {
        let g = p.parts().0;
        g.commit();
        let i = g.demand(cell);
        let d = input_to_data(&i);
        if matches!(i.status, Status::Ready) && ok(&d) {
            return d;
        }
        if let Status::Error(e) = &i.status {
            panic!("cell failed: {e}");
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "timed out; last {d:?} {:?}", i.status);
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn num(p: &mut ScriptPage, name: &str) -> f64 {
    let d = p.cell_value(name).unwrap_or_else(|| panic!("no cell {name}"));
    d.num().unwrap_or_else(|| panic!("{name} is {d:?}"))
}

#[test]
fn luau_api_end_to_end() {
    let src = r#"
        local wb = sheet.new({ name = "t" })
        wb = sheet.set_range(wb, "A1", { { 1, 2, "=A1+B1" }, { "x", true, "=C1*10" } })
        assert(wb:get("C1") == 3, "computed")
        assert(wb:get("Sheet1!C2") == 30, "with sheet name")
        assert(wb:formula("C2") == "=C1*10", "formula text")
        assert(wb:formula("A1") == nil, "no formula")
        local t = wb:range("A1:C2")
        assert(t.columns[3] == "C" and t.rows[2][2] == true and t.rows[2][1] == "x", "range table")
        local h = wb:range("A1:C2", { header = true, text = true })
        assert(h.columns[1] == "1" and #h.rows == 1 and h.text[1][3] == "30", "header + text")
        assert(#wb:sheets() == 1 and wb:sheets()[1] == "Sheet1", "sheets")
        local dm = wb:dims()
        assert(dm.rows == 2 and dm.cols == 3 and dm.used == "A1:C2", "dims")
        assert(sheet.col_name(28) == "AB" and sheet.col_index("ab") == 28, "columns")
        assert(sheet.ref(2, 3) == "C2" and sheet.ref(1, 1, "My sheet") == "'My sheet'!A1", "refs")
        local pr = sheet.parse_ref("'My sheet'!B2:D9")
        assert(pr.sheet == "My sheet" and pr.row == 2 and pr.last_col == 4, "parse_ref")
        wb = sheet.add_sheet(wb, "Data")
        wb = sheet.set(wb, "Data!A1", 7)
        wb = sheet.set(wb, "D1", "=Data!A1*2")
        assert(wb:get("D1") == 14, "cross sheet")
        local ok, err = pcall(sheet.set, wb, "A1:B2", 1)
        assert(not ok and string.find(tostring(err), "is a range"), "set takes one cell")
        ok, err = pcall(sheet.set, wb, "Nope!A1", 1)
        assert(not ok and string.find(tostring(err), "no sheet named"), "unknown sheet")
        ok, err = pcall(function() return wb:get(42) end)
        assert(not ok and string.find(tostring(err), "must be a string"), "bad argument")
        ok, err = pcall(sheet.get, {}, "A1")
        assert(not ok and string.find(tostring(err), "must be a workbook"), "not a workbook")
        ok, err = pcall(sheet.open, "\xD0\xCF\x11\xE0 old binary")
        assert(not ok and string.find(tostring(err), "save it as .xlsx"), "old xls")
        ok, err = pcall(sheet.open, "\xFF\xFE not utf8")
        assert(not ok and string.find(tostring(err), "UTF%-8"), "not text")
        local csvwb = sheet.open("x,y\n1,2\n")
        assert(csvwb:get("B2") == 2, "open falls back to CSV for text")
        local c = wb:cell("C2")
        assert(c.formula == "=C1*10" and c.value == 30 and c.type == "number" and c.ref == "Sheet1!C2", "cell info")

        Book = page.input("book", wb)
        page.derive("total", { Book }, function(w) return w:get("C2") end)
        page.derive("info", { Book }, function(w) return w:cell("C2") end)
        page.derive("csv", { Book }, function(w) return w:to_csv() end)
        page.derive("bad_ref", { Book }, function(w) return w:get("Q1:Q2") end)
        page.derive("effect_in_cell", { Book }, function(w) return sheet.set(w, "A1", 1) end)
        page.on("key", function(k)
            local w = page.get(Book)
            if k == "up" then
                page.set(Book, sheet.set(w, "A1", w:get("A1") + 1))
            elseif k == "z" then
                page.set(Book, sheet.undo(w))
            elseif k == "y" then
                page.set(Book, sheet.redo(w))
            elseif k == "bad" then
                page.set(Book, sheet.set(w, "A1", "=1/0"))
            elseif k == "rows" then
                page.set(Book, sheet.insert_rows(w, "Sheet1!1:2"))
            end
            return true
        end)
    "#;
    let mut p = load(SHEET, src).unwrap();
    assert_eq!(num(&mut p, "total"), 30.0);
    // an edit returns the next version: everything that read the book recomputes
    assert!(p.key("up"));
    assert_eq!(num(&mut p, "total"), 40.0);
    assert!(p.cell_value("csv").unwrap().as_str().unwrap().starts_with("2,2,4,14\r\n"));
    assert!(p.key("z"));
    assert_eq!(num(&mut p, "total"), 30.0, "undo");
    assert!(p.key("y"));
    assert_eq!(num(&mut p, "total"), 40.0, "redo");
    assert!(p.key("bad"));
    assert_eq!(p.cell_value("total").unwrap(), Data::map([("error", Data::str("#DIV/0!"))]));
    assert!(p.key("rows"));
    assert_eq!(p.cell_value("info").unwrap().get("ref").as_str(), Some("Sheet1!C2"), "info follows the version");
    let csv = p.cell_value("csv").unwrap();
    assert!(csv.as_str().unwrap().starts_with(",,,\r\n,,,\r\n#DIV/0!,"), "{csv:?}");
    // errors stay readable
    let bad = p.cell_value("bad_ref").unwrap();
    assert!(bad.get("message").as_str().unwrap().contains("is a range"), "{bad:?}");
    let eff = p.cell_value("effect_in_cell").unwrap();
    assert!(eff.get("message").as_str().unwrap().contains("changes state"), "{eff:?}");
    assert_eq!(p.handler_errors, 0);
}

#[test]
fn undeclared_module_is_refused() {
    let e = load("{}", "sheet.new()").err().unwrap();
    assert!(e.contains("\"sheet\" module is not declared"), "{e}");
}

#[test]
fn load_and_export_jobs() {
    let src = r#"
        local wb = sheet.new()
        wb = sheet.set_range(wb, "A1", { { "n", "sq" }, { 3, "=A2^2" }, { 4, "=A3^2" } })
        File = page.input("file", wb:save())
        Csv = page.input("csv", "a;b\n1;2\n")
        page.job("loaded", "sheet.load", { File })
        page.job("from_csv", "sheet.load", { Csv }, { format = "csv" })
        page.derive("sum", { page.job("loaded2", "sheet.load", { File }) }, function(w)
            if w.state then return nil end -- { state = "pending" } until the job is done
            return w:get("B2") + w:get("B3")
        end)
        page.job("export", "sheet.export", { page.input("book", wb) })
        page.job("export_csv", "sheet.export", { page.input("book2", wb) }, { format = "csv", delimiter = "tab" })
        page.job("broken", "sheet.load", { page.input("junk", "PK not really a zip") })
    "#;
    let mut p = load(SHEET, src).unwrap();
    let sum = p.cell_id("sum").unwrap();
    assert_eq!(wait_for(&mut p, sum, |d| d.num().is_some()), Data::Num(25.0));
    let loaded = p.cell_id("loaded").unwrap();
    let wb = wait_for(&mut p, loaded, |d| d.as_res().is_some());
    assert_eq!(wb.as_res().unwrap().kind(), "sheet.workbook");
    let from_csv = p.cell_id("from_csv").unwrap();
    let wb = wait_for(&mut p, from_csv, |d| d.as_res().is_some());
    let w = wb.as_res().unwrap().get::<anypage_sheet::Workbook>().unwrap();
    assert_eq!(w.lock().unwrap().to_csv(0, &Default::default()).unwrap(), "a,b\r\n1,2\r\n", "semicolons sniffed");
    let export = p.cell_id("export").unwrap();
    let bytes = wait_for(&mut p, export, |d| matches!(d, Data::Bytes(_)));
    assert!(bytes.as_bytes().unwrap().starts_with(b"PK"));
    let export_csv = p.cell_id("export_csv").unwrap();
    assert_eq!(wait_for(&mut p, export_csv, |d| d.as_str().is_some()).as_str(), Some("n\tsq\r\n3\t9\r\n4\t16\r\n"));
    // a failing job reports why
    let broken = p.cell_id("broken").unwrap();
    let t0 = Instant::now();
    loop {
        let g = p.parts().0;
        g.commit();
        if let Status::Error(e) = &g.demand(broken).status {
            assert!(e.contains("not a readable xlsx"), "{e}");
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
    }
    p.shutdown();
}

// ---------------------------------------------------------------- the demo package

fn demo_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../pages/sheet-demo")
}

fn demo() -> ScriptPage {
    let pkg = Package::from_dir(&demo_dir()).unwrap();
    ScriptPage::load(&pkg, &natives(), text(), wake()).unwrap()
}

// layout constants of pages/sheet-demo/main.luau
const GRID_Y: f32 = 124.0;
const HEAD_W: f32 = 52.0;
const ROW_H: f32 = 28.0;
const COL_W: [f32; 9] = [230.0, 120.0, 90.0, 120.0, 130.0, 120.0, 130.0, 130.0, 130.0];
const BG: [u8; 3] = [22, 25, 32];
const HEAD_BG: [u8; 3] = [32, 36, 46];
const ACCENT: [u8; 3] = [88, 166, 255];

/// World position of the middle of a cell while the grid shows row 1, column A first.
fn cell_center(row: usize, col: usize) -> (f32, f32) {
    let x = HEAD_W + COL_W[..col - 1].iter().sum::<f32>() + COL_W[col - 1] / 2.0;
    (x, GRID_Y + row as f32 * ROW_H + ROW_H / 2.0)
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

fn act(p: &mut ScriptPage, json: &str) -> serde_json::Value {
    serde_json::from_str(&p.act(json)).unwrap()
}

fn panel_image(p: &mut ScriptPage, index: usize) -> Image {
    let cell = p.spec().panels[index].cell;
    let g = p.parts().0;
    let i = g.demand(cell);
    let img = i.get::<Image>().expect("a rasterised panel");
    Image { w: img.w, h: img.h, rgba: img.rgba.clone(), newest: None }
}

fn pixel(img: &Image, x: u32, y: u32) -> [u8; 3] {
    let k = ((y * img.w + x) * 4) as usize;
    [img.rgba[k], img.rgba[k + 1], img.rgba[k + 2]]
}

fn near(a: [u8; 3], b: [u8; 3]) -> bool {
    a.iter().zip(b).all(|(x, y)| (*x as i32 - y as i32).abs() <= 3)
}

fn write_png(img: &Image, name: &str) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-output");
    std::fs::create_dir_all(&dir).unwrap();
    let file = std::fs::File::create(dir.join(name)).unwrap();
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), img.w, img.h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    // premultiplied to straight alpha (the panels are opaque, so this is a no-op there)
    let straight: Vec<u8> = img
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| {
            let a = p[3] as u32;
            let un = |c: u8| (c as u32 * 255 + a / 2).checked_div(a).map_or(0, |v| v.min(255) as u8);
            [un(p[0]), un(p[1]), un(p[2]), p[3]]
        })
        .collect();
    enc.write_header().unwrap().write_image_data(&straight).unwrap();
}

#[test]
fn demo_page_edits_cells_and_recomputes_the_total() {
    let mut p = demo();
    assert_eq!(p.manifest.id, "com.anypage.sheet-demo");
    assert_eq!(p.spec().panels.len(), 4);
    assert!(close(num(&mut p, "total"), 572.04), "grand total of the sample invoice");

    // C2 (bolts quantity) is selected; typing starts editing, Enter commits
    assert!(p.key("2"));
    assert!(p.text_focus().is_some(), "editing asks for the keyboard / IME");
    p.text_input("00");
    assert!(p.key("enter"));
    assert!(p.text_focus().is_none());
    // 200 x 0.35 = 70 (was 42): subtotal 572.80, tax 28.64
    assert!(close(num(&mut p, "total"), 601.44), "{}", num(&mut p, "total"));
    let sel = p.cell_value("sel").unwrap();
    assert_eq!((sel.get("row").num(), sel.get("col").num()), (Some(3.0), Some(3.0)), "Enter moves down");

    assert!(p.key("ctrl+z"));
    assert!(close(num(&mut p, "total"), 572.04), "undo");
    assert!(p.key("ctrl+y"));
    assert!(close(num(&mut p, "total"), 601.44), "redo");

    // click D12 (the tax rate), edit it with the keyboard
    let (x, y) = cell_center(12, 4);
    assert_eq!(p.pointer_down(x, y), PointerResponse::Handled);
    assert!(p.key("enter"));
    let editing = p.cell_value("edit").unwrap();
    let shown = editing.get("text").as_str().unwrap().to_string();
    assert!(shown == "5%" || shown == "0.05", "the editor starts from the cell's input: {shown}");
    for _ in 0..shown.chars().count() {
        assert!(p.key("backspace"));
    }
    assert_eq!(p.cell_value("edit").unwrap().get("text").as_str(), Some(""));
    p.text_input("10%");
    assert!(p.key("enter"));
    assert!(close(num(&mut p, "total"), 630.08), "tax 10% of 572.80");

    // Delete clears; Escape cancels an edit without writing
    let (x, y) = cell_center(12, 4);
    p.pointer_down(x, y);
    assert!(p.key("delete"));
    assert!(close(num(&mut p, "total"), 572.8));
    assert!(p.key("9"));
    assert!(p.key("escape"));
    assert!(close(num(&mut p, "total"), 572.8), "cancelled edit wrote nothing");

    // a bad formula shows its error in the grid, not a script failure
    let (x, y) = cell_center(18, 1);
    p.pointer_down(x, y);
    assert!(p.key("="));
    p.text_input("1/0");
    assert!(p.key("enter"));
    let r = act(&mut p, r#"{"action": "read", "params": {"ref": "A18:A18"}}"#);
    assert_eq!(r["result"]["columns"][0], "#DIV/0!", "{r}");

    // paste tab-separated cells at A19
    p.pointer_down(x, y + ROW_H);
    assert!(p.key("ctrl+v"));
    assert_eq!(p.take_request(), Some(PageRequest::Paste));
    p.paste("Extra\t\t3\t$2.00\t=C19*D19\nMore\t\t1\t$1.00\t=C20*D20\n");
    let r = act(&mut p, r#"{"action": "read", "params": {"ref": "A19:E20"}}"#);
    assert_eq!(r["result"]["columns"][4], "6", "header names come from the first row: {r}");
    // a row with empty cells is a Luau table with holes, which comes back as a map
    assert_eq!(r["result"]["rows"][0]["5"], 1, "{r}");

    // the semantic port: an agent sets a cell and reads the total
    let r = act(&mut p, r#"{"action": "set_cell", "params": {"ref": "D12", "value": "5%"}}"#);
    assert_eq!(r["result"]["ok"], true, "{r}");
    assert!(close(num(&mut p, "total"), 601.44));
    let r = act(&mut p, r#"{"action": "set_cell", "params": {"ref": "Nope!A1", "value": 1}}"#);
    assert_eq!(r["result"]["ok"], false);
    assert!(r["result"]["error"].as_str().unwrap().contains("no sheet named"), "{r}");
    let r = act(&mut p, r#"{"action": "formulas"}"#);
    assert!(r["result"].as_array().map(|a| a.len()).unwrap_or(0) >= 20 || r["result"]["$len"].as_u64().unwrap_or(0) >= 20, "{r}");
    let r = act(&mut p, r#"{"get": "total"}"#);
    assert!((r["value"].as_f64().unwrap() - 601.44).abs() < 1e-9);

    // save asks the host for a file; the answer reaches the status line
    assert!(p.key("ctrl+s"));
    let (id, name, bytes) = match p.take_request() {
        Some(PageRequest::SaveFile { id, name, bytes }) => (id, name, bytes),
        other => panic!("{other:?}"),
    };
    assert_eq!(name, "invoice.xlsx");
    assert!(bytes.starts_with(b"PK"));
    p.host_result(id, HostResult::Saved { name: "invoice.xlsx".into() });
    assert!(p.cell_value("status").unwrap().as_str().unwrap().contains("saved"));
    // and the saved file opens again (dropped onto the page), through the
    // sheet.load job, off the page's thread
    let dropped_at = act(&mut p, r#"{"get": "total"}"#);
    p.file_drop("invoice.xlsx", bytes);
    let fs = p.cell_id("file_state").unwrap();
    let state = wait_for(&mut p, fs, |d| d.as_str().is_some_and(|s| s.contains("opened")));
    assert!(state.as_str().unwrap().ends_with("invoice.xlsx"), "{state:?}");
    assert!(close(num(&mut p, "total"), 601.44), "the reopened file computes the same total ({dropped_at})");
    // the handlers now edit the opened file
    let r = act(&mut p, r#"{"action": "set_cell", "params": {"ref": "Invoice!D12", "value": "10%"}}"#);
    assert_eq!(r["result"]["ok"], true, "{r}");
    assert!(close(num(&mut p, "total"), 630.08), "tax 10% of 572.80 in the opened file");
    let r = act(&mut p, r#"{"action": "set_cell", "params": {"ref": "Invoice!D12", "value": "5%"}}"#);
    assert_eq!(r["result"]["ok"], true, "{r}");
    // a CSV export through the same dialog path
    assert!(p.key("ctrl+e"));
    match p.take_request() {
        Some(PageRequest::SaveFile { name, bytes, .. }) => {
            assert_eq!(name, "invoice.csv");
            let text = String::from_utf8(bytes.to_vec()).unwrap();
            assert!(text.starts_with("Item,Date,Qty,Unit price,Amount,Size\r\nSteel bolts M8,2024-01-15,200,0.35,70,large\r\n"), "{text}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(p.handler_errors, 0);
}

#[test]
fn demo_page_survives_bad_files_deep_formulas_and_far_scrolling() {
    let mut p = demo();
    let fs = p.cell_id("file_state").unwrap();
    // a file that is not a spreadsheet: the job fails, the page keeps working
    p.file_drop("junk.xlsx", Arc::new(b"PK\x03\x04 not really a zip".to_vec()));
    let state = wait_for(&mut p, fs, |d| d.as_str().is_some_and(|s| s.contains("cannot open")));
    assert!(state.as_str().unwrap().contains("not a readable xlsx"), "{state:?}");
    assert!(close(num(&mut p, "total"), 572.04), "the sample is still there");
    let title = panel_image(&mut p, 0);
    assert!((0..title.w).flat_map(|x| (38..56).map(move |y| (x, y))).any(|(x, y)| pixel(&title, x, y)[0] > 200 && pixel(&title, x, y)[1] < 160), "the error is shown in red");
    // a file whose formula nests 1 000 levels deep (a stack overflow in IronCalc)
    let w = anypage_sheet::Workbook::new(&Default::default()).unwrap();
    w.lock().unwrap().set(0, 1, 1, &Data::str("=1+1")).unwrap();
    let ok = w.lock().unwrap().to_xlsx().unwrap();
    p.file_drop("ok.xlsx", Arc::new(ok));
    wait_for(&mut p, fs, |d| d.as_str().is_some_and(|s| s.contains("opened ok.xlsx")));
    assert_eq!(p.cell_value("total"), Some(Data::Nil), "another workbook: no invoice total");
    // an agent typing a formula nested 1 000 levels deep gets an error, not a crash
    let deep = format!("={}1{}", "(".repeat(1000), ")".repeat(1000));
    let r = act(&mut p, &serde_json::json!({"action": "set_cell", "params": {"ref": "A1", "value": deep}}).to_string());
    assert_eq!(r["result"]["ok"], false, "{r}");
    assert!(r["result"]["error"].as_str().unwrap().contains("levels deep"), "{r}");
    // scrolling far down stops at the last row; the grid still draws
    let (x, y) = cell_center(3, 3);
    for _ in 0..3 {
        p.wheel(x, y, 0.0, -1.0e7);
    }
    let v = p.cell_value("view").unwrap();
    assert_eq!(v.get("top").num(), Some((1_048_576 - 19 + 1) as f64), "the last row is at the bottom of the 19-row grid: {v:?}");
    let grid = panel_image(&mut p, 2);
    assert_eq!((grid.w, grid.h), (1280, 560));
    assert_eq!(p.handler_errors, 0);
}

#[test]
fn luau_load_into_literal_tables_and_inline_limits() {
    let src = r#"
        local wb = sheet.new()
        wb = sheet.set_range(wb, "A1", { { "code", "on" }, { "00123", "TRUE" } })
        -- typed: "00123" became a number
        assert(wb:get("A2") == 123, "typed")
        -- a table read back keeps its types when written again
        local t = wb:range("A1:B2", { header = true })
        t.rows[1][1] = "00123"
        wb = sheet.set_range(wb, "D1", t)
        assert(wb:get("D2") == "00123" and wb:get("E2") == true, "a table is written literally")
        wb = sheet.set_range(wb, "G1", { { "007", "=1+1" } }, { literal = true })
        assert(wb:get("G1") == "007" and wb:get("H1") == "=1+1" and wb:formula("H1") == nil, "literal")
        assert(wb:get("A1") == "code", "header")
        -- inline opens are small; big files go through the job
        local ok, err = pcall(sheet.from_csv, string.rep("1,", 250000))
        assert(not ok and string.find(tostring(err), "sheet.load"), tostring(err))
        assert(sheet.name(wb) == "workbook", "name")
        Book = page.input("book", wb)
        File = page.input("file", nil)
        local loaded = page.job("loaded", "sheet.load", { File }, { into = wb })
        page.derive("a1", { Book, loaded }, function(w)
            return w:get("A1")
        end)
        local other = sheet.new()
        other = sheet.set(other, "A1", "from the file")
        Bytes = other:save()
        page.on("key", function(k)
            if k == "o" then
                page.set(File, Bytes)
            elseif k == "e" then
                page.set(Book, sheet.set(page.get(Book), "B1", "edited"))
            end
            return true
        end)
    "#;
    let mut p = load(SHEET, src).unwrap();
    // no file yet: the job gives nil, nothing changes
    let loaded = p.cell_id("loaded").unwrap();
    assert_eq!(wait_for(&mut p, loaded, |_| true), Data::Nil);
    assert_eq!(p.cell_value("a1"), Some(Data::str("code")));
    assert!(p.key("o"));
    let a1 = p.cell_id("a1").unwrap();
    assert_eq!(wait_for(&mut p, a1, |d| d.as_str() == Some("from the file")), Data::str("from the file"));
    // handlers hold the same workbook and edit the loaded content
    assert!(p.key("e"));
    let book = p.cell_value("book").unwrap();
    let w = book.as_res().unwrap().get::<anypage_sheet::Workbook>().unwrap();
    let b = w.lock().unwrap();
    assert_eq!(b.value(0, 1, 1).unwrap(), Data::str("from the file"));
    assert_eq!(b.value(0, 1, 2).unwrap(), Data::str("edited"));
    drop(b);
    p.shutdown();
}

#[test]
fn demo_page_draws_the_grid() {
    let mut p = demo();
    // select E13 (the grand total) so the selection is somewhere known
    let (x, y) = cell_center(13, 5);
    p.pointer_down(x, y);
    let grid = panel_image(&mut p, 2);
    assert_eq!((grid.w, grid.h), (1280, 560));
    write_png(&grid, "sheet-demo-grid.png");

    // background, header row, selection border, and text where values are
    let col_x = |c: usize| HEAD_W + COL_W[..c - 1].iter().sum::<f32>();
    assert!(near(pixel(&grid, 1275, 300), BG), "right margin is background: {:?}", pixel(&grid, 1275, 300));
    assert!(near(pixel(&grid, (col_x(2) + 4.0) as u32, 3), HEAD_BG), "header row: {:?}", pixel(&grid, (col_x(2) + 4.0) as u32, 3));
    // cell fills come from the workbook's styles: A1:F1 #26324A, A13:D13 #1E3B2C
    assert!(near(pixel(&grid, (col_x(3) + 4.0) as u32, 28 + 4), [0x26, 0x32, 0x4A]), "styled header fill: {:?}", pixel(&grid, (col_x(3) + 4.0) as u32, 32));
    assert!(near(pixel(&grid, (col_x(3) + 4.0) as u32, 13 * 28 + 4), [0x1E, 0x3B, 0x2C]), "styled total fill: {:?}", pixel(&grid, (col_x(3) + 4.0) as u32, 13 * 28 + 4));
    assert!(near(pixel(&grid, (col_x(3) + 4.0) as u32, 3 * 28 + 4), BG), "unstyled cells have no fill");
    let sx = col_x(5) as u32;
    let sy = (13.0 * ROW_H) as u32;
    assert!(near(pixel(&grid, sx, sy + 14), ACCENT), "selection border: {:?}", pixel(&grid, sx, sy + 14));
    assert!(near(pixel(&grid, sx + 60, sy), ACCENT), "selection border top: {:?}", pixel(&grid, sx + 60, sy));
    let ink = |c: usize, r: usize| -> usize {
        let (x0, y0) = (col_x(c) as u32 + 3, (r as f32 * ROW_H) as u32 + 3);
        let mut n = 0;
        for yy in y0..y0 + ROW_H as u32 - 6 {
            for xx in x0..x0 + COL_W[c - 1] as u32 - 6 {
                let px = pixel(&grid, xx, yy);
                if px.iter().zip(BG).map(|(a, b)| (*a as i32 - b as i32).abs()).sum::<i32>() > 120 {
                    n += 1;
                }
            }
        }
        n
    };
    assert!(ink(1, 2) > 40, "the text 'Steel bolts M8' is drawn: {} bright pixels", ink(1, 2));
    assert!(ink(5, 13) > 20, "the grand total is drawn in E13: {}", ink(5, 13));
    assert_eq!(ink(7, 5), 0, "G5 is empty");
    // numbers are right-aligned: the right half of C2 has ink, the left quarter none
    let (cx, cy) = (col_x(3) as u32, (2.0 * ROW_H) as u32);
    let bright = |x0: u32, x1: u32| (x0..x1).flat_map(|x| (cy + 4..cy + 24).map(move |y| (x, y))).filter(|&(x, y)| pixel(&grid, x, y)[0] > 120).count();
    assert!(bright(cx + 50, cx + 88) > 10 && bright(cx + 2, cx + 22) == 0, "C2 = 120 is right-aligned");

    // editing draws the editor and a caret over the cell
    assert!(p.key("f2"));
    let editing = panel_image(&mut p, 2);
    write_png(&editing, "sheet-demo-grid-editing.png");
    assert!(near(pixel(&editing, sx + 120, sy + 4), [12, 14, 20]), "editor background: {:?}", pixel(&editing, sx + 120, sy + 4));
    let totals = panel_image(&mut p, 3);
    write_png(&totals, "sheet-demo-totals.png");
    let bar = panel_image(&mut p, 1);
    write_png(&bar, "sheet-demo-formula-bar.png");
    // the formula bar shows the formula being edited: some accent caret pixels
    let caret = (0..bar.w).flat_map(|x| (8..36).map(move |y| (x, y))).filter(|&(x, y)| near(pixel(&bar, x, y), ACCENT)).count();
    assert!(caret >= 20, "caret in the formula bar: {caret} px");
}
