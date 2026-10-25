//! Headless page tests: real page packages drive the `pdf` module through
//! Luau, the way an app or an agent does.

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anypage_core::{Status, TileKey, TILE_SIZE};
use anypage_engine::{Page, PageRequest, PointerResponse, TextRaster, Wake};
use anypage_pdf::{api::image_of, edit, render::render_page, PdfDoc};
use anypage_runtime::{Data, Natives, Package, ScriptPage};
use common::*;

fn natives() -> Natives {
    let mut n = Natives::new();
    anypage_pdf::register(&mut n);
    n
}

fn text() -> Arc<TextRaster> {
    Arc::new(TextRaster::load().expect("a system font"))
}

fn wake() -> Wake {
    Arc::new(|| {})
}

fn load(src: &str) -> Result<ScriptPage, String> {
    let m = r#"{ "format": 1, "id": "test", "bounds": [0, 0, 800, 800], "capabilities": { "modules": ["pdf"], "files": true },
               "limits": { "call_ms": 1000, "handler_ms": 2000, "load_ms": 5000 } }"#;
    let pkg = Package::from_files("test", [("page.json", m), ("main.luau", src)])?;
    ScriptPage::load(&pkg, &natives(), text(), wake())
}

/// The value of a named cell once it is ready and `ok` accepts it.
fn wait_for(p: &mut ScriptPage, name: &str, mut ok: impl FnMut(&Data) -> bool) -> Data {
    let cell = p.cell_id(name).unwrap_or_else(|| panic!("no cell {name}"));
    let t0 = Instant::now();
    loop {
        let g = p.parts().0;
        g.commit();
        let i = g.demand(cell);
        let d = anypage_runtime::data::input_to_data(&i);
        if matches!(i.status, Status::Ready) && ok(&d) {
            return d;
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "{name} timed out; last {d:?} {:?}", i.status);
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn value(p: &mut ScriptPage, name: &str) -> Data {
    wait_for(p, name, |_| true)
}

/// Render tile `key` of the page's first tiled layer with its params cell.
fn tile(p: &mut ScriptPage, key: TileKey) -> Vec<u8> {
    let (cell, src) = {
        let t = &p.spec().tiled[0];
        (t.params, t.source.clone())
    };
    let g = p.parts().0;
    g.commit();
    let v = g.demand(cell).value.expect("tile params ready");
    src.render(key, &v, &|| false).expect("a tile")
}

/// A 2 x 1 PNG: red, blue.
fn png_bytes() -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, 2, 1);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().expect("png header");
        w.write_image_data(&[255, 0, 0, 0, 0, 255]).expect("png data");
    }
    out
}

fn doc_of(d: &Data) -> Arc<PdfDoc> {
    anypage_pdf::doc_arg(d).expect("a document")
}

const API_PAGE: &str = r#"
Doc = page.input("doc", nil)
Log = page.input("log", "")
PageIn = page.input("page_no", 1)
Width = page.input("width", 612)
local function when(d, f) if d == nil then return nil end return f(d) end
page.derive("count", { Doc }, function(d) return when(d, function(d) return d:page_count() end) end)
page.derive("size3", { Doc }, function(d) return when(d, function(d) return d:page_size(3) end) end)
page.derive("meta", { Doc }, function(d) return when(d, function(d) return d:metadata() end) end)
page.derive("outline", { Doc }, function(d) return when(d, function(d) return d:outline() end) end)
page.derive("words", { Doc }, function(d) return when(d, function(d) return d:words(1) end) end)
page.derive("text2", { Doc }, function(d) return when(d, function(d) return d:text(2) end) end)
page.derive("hits", { Doc }, function(d) return when(d, function(d) return d:search("quick BROWN") end) end)
page.derive("extract", { Doc }, function(d) return when(d, function(d) return d:extract(1) end) end)
page.derive("fields", { Doc }, function(d) return when(d, function(d) return d:form_fields() end) end)
page.derive("annots", { Doc }, function(d) return when(d, function(d) return d:annotations(1) end) end)
page.derive("version", { Doc }, function(d) return tostring(d) end)
page.derive("bad", { Doc }, function(d) return when(d, function(d) return pdf.add_text(d, 1, { text = "no" }) end) end)
page.derive("measure", {}, function() return pdf.measure_text("Hello", { size = 12 }) end)
local params = page.derive("params", { Doc }, function(d) return { doc = d, page = 1 } end)
page.tiled({ rect = { 0, 0, 612, 792 }, source = "pdf.page", params = params })
page.job("render", "pdf.render", { Doc, PageIn, Width })
page.job("search_job", "pdf.search", { Doc, page.input("q", "quarterly") })
page.job("extract_job", "pdf.extract", { Doc, PageIn })

Bytes = page.input("bytes", nil)
page.job("loaded", "pdf.load", { Bytes }, { editable = true })
page.derive("loaded_count", { page.job("loaded2", "pdf.load", { Bytes }) }, function(d) return d:page_count() end)
page.on("file_drop", function(f)
	if string.sub(f.name, -4) == ".png" then
		page.set(Doc, pdf.add_image(page.get(Doc), 1, f.data, { 400, 640, 60, 30 }))
	else
		page.set(Doc, pdf.open(f.data))
		page.set(Bytes, f.data)
	end
	return true
end)
page.on("key", function(k)
	local d = page.get(Doc)
	if k == "h" then
		page.set(Doc, pdf.highlight(d, 1, d:search("quick brown"), color.rgba(255, 225, 0, 200)))
	elseif k == "t" then
		page.set(Doc, pdf.add_text(d, 1, { x = 100, y = 700, text = "From Luau 中文", size = 14 }))
	elseif k == "p" then
		page.set(Doc, pdf.add_path(d, 1, { "M", 300, 700, "L", 360, 700, "L", 330, 750, "Z", fill = color.rgba(0, 160, 0) }))
	elseif k == "f" then
		page.set(Doc, pdf.fill_form(d, { name = "Linus", agree = true }))
	elseif k == "n" then
		page.set(Doc, pdf.add_note(d, 2, { x = 50, y = 50, text = "see table" }))
	elseif k == "m" then
		page.set(Doc, pdf.merge(d, pdf.new({ width = 200, height = 300, pages = 2 })))
	elseif k == "x" then
		page.set(Doc, pdf.delete_page(pdf.insert_page(pdf.rotate_page(d, 2, 90), 1), 1))
	elseif k == "v" then
		-- two edits through the same old handle: the second must still be newer
		local a = pdf.add_text(d, 1, { x = 10, y = 10, text = "a" })
		local b = pdf.add_text(d, 1, { x = 10, y = 30, text = "b" })
		page.set(Log, tostring(a) .. " " .. tostring(b) .. " " .. tostring(a == b))
		page.set(Doc, b)
	elseif k == "e" then
		local ok, err = pcall(pdf.open, "not a pdf at all")
		local ok2, err2 = pcall(function() return d:words(99) end)
		local ok3, err3 = pcall(pdf.add_path, d, 1, { "M", 1 })
		page.set(Log, tostring(ok) .. "|" .. tostring(err) .. "|" .. tostring(ok2) .. "|" .. tostring(err2) .. "|" .. tostring(ok3) .. "|" .. tostring(err3))
	elseif k == "c" then
		page.set(Doc, d:copy_pages({ 2, 1 }))
	elseif k == "s" then
		local copy = pdf.open(d:save())
		page.set(Log, "saved " .. copy:page_count() .. " " .. #copy:annotations(1))
	end
	return true
end)
"#;

#[test]
fn the_luau_api_end_to_end() {
    let mut p = load(API_PAGE).unwrap();
    assert_eq!(value(&mut p, "count"), Data::Nil);
    p.file_drop("fixture.pdf", fixture());
    assert_eq!(value(&mut p, "count"), Data::Num(4.0));
    let s3 = value(&mut p, "size3");
    assert_eq!((s3.get("w").num(), s3.get("h").num(), s3.get("rotation").num()), (Some(792.0), Some(612.0), Some(90.0)));
    let meta = value(&mut p, "meta");
    assert_eq!(meta.get("title").as_str(), Some("AnyPage Test Fixture"));
    assert_eq!(meta.get("author").as_str(), Some("Tester \u{6E2C}\u{8A66}"));
    assert_eq!(meta.get("created").as_str(), Some("2026-10-24T12:00:00+08:00"));
    let outline = value(&mut p, "outline");
    assert_eq!(outline.items().len(), 3);
    assert_eq!(outline.at(2).get("title").as_str(), Some("Table"));
    assert_eq!(outline.at(2).get("level").num(), Some(1.0));
    assert_eq!(outline.at(3).get("page").num(), Some(2.0));
    let words = value(&mut p, "words");
    let q = words.items().iter().find(|w| w.get("text").as_str() == Some("Quarterly")).expect("Quarterly");
    assert_eq!((q.get("x").num(), q.get("y").num(), q.get("h").num()), (Some(72.0), Some(52.8), Some(24.0)));
    assert!(value(&mut p, "text2").as_str().unwrap().contains("PDF\u{6587}\u{4EF6}"));
    let hits = value(&mut p, "hits");
    assert_eq!(hits.items().len(), 1);
    assert_eq!(hits.at(1).get("text").as_str(), Some("quick brown"));
    assert_eq!(hits.at(1).get("rects").at(1).items().len(), 4);
    let ex = value(&mut p, "extract");
    assert!(ex.get("blocks").at(1).get("text").as_str().unwrap().starts_with("Quarterly Report"));
    let t = ex.get("tables").at(1);
    assert_eq!(t.get("columns"), &Data::list(vec![Data::str("Item"), Data::str("Qty"), Data::str("Price")]));
    assert_eq!(t.get("rows").at(1), &Data::list(vec![Data::str("Apples"), Data::Num(3.0), Data::Num(1200.5)]));
    let fields = value(&mut p, "fields");
    assert_eq!(fields.items().len(), 5);
    assert_eq!(fields.at(5).get("name").as_str(), Some("address.city"));
    assert_eq!(fields.at(1).get("widgets").at(1).get("page").num(), Some(4.0));
    assert_eq!(value(&mut p, "measure").get("w").num(), Some(27.34));
    // effects refuse to run inside cell functions
    let cell = p.cell_id("bad").unwrap();
    let st = p.parts().0.demand(cell).status;
    assert!(matches!(&st, Status::Error(e) if e.contains("changes state")), "{st:?}");

    // jobs
    let img = wait_for(&mut p, "render", |d| image_of(d).is_some());
    let im = image_of(&img).unwrap();
    assert_eq!((im.w, im.h), (612, 792));
    assert!(near(px(&im.rgba, im.w, 450, 167), [255, 0, 0, 255], 2));
    let sj = wait_for(&mut p, "search_job", |d| !d.items().is_empty());
    assert_eq!(sj.items().iter().map(|h| h.get("page").num().unwrap()).collect::<Vec<_>>(), [1.0, 2.0]);
    let ej = wait_for(&mut p, "extract_job", |d| !d.is_nil());
    assert_eq!(ej.get("tables").items().len(), 1);

    // tiles through the page's params cell
    let t0 = tile(&mut p, TileKey { page: 0, level: 1, tx: 3, ty: 0 });
    // red rect (400..500, 142..192) at 2 px/pt -> (800..1000, 284..384): tile 3 covers x 768..1024, y 0..256
    assert!(near(px(&t0, TILE_SIZE, 900 - 768, 250), [255, 255, 255, 255], 0));
    let t1 = tile(&mut p, TileKey { page: 0, level: 1, tx: 3, ty: 1 });
    assert!(near(px(&t1, TILE_SIZE, 900 - 768, 330 - 256), [255, 0, 0, 255], 2));

    // edits: every one is a new version that cells see
    let v0 = value(&mut p, "version");
    assert!(v0.as_str().unwrap().ends_with("v0"), "{v0:?}");
    assert!(p.key("h"));
    let annots = value(&mut p, "annots");
    assert_eq!(annots.items().len(), 1);
    assert_eq!(annots.at(1).get("type").as_str(), Some("Highlight"));
    assert!(value(&mut p, "version").as_str().unwrap().ends_with("v1"));
    assert!(p.key("t"));
    let words = value(&mut p, "words");
    let w = words.items().iter().find(|w| w.get("text").as_str() == Some("Luau")).expect("added text");
    assert_eq!(w.get("y").num(), Some(700.0));
    assert!(words.items().iter().any(|w| w.get("text").as_str() == Some("\u{4E2D}\u{6587}")));
    assert!(p.key("p"));
    let t2 = tile(&mut p, TileKey { page: 0, level: 0, tx: 1, ty: 2 });
    // triangle (300..360, 700..750) at 1 px/pt: tile (256..512, 512..768)
    assert!(near(px(&t2, TILE_SIZE, 330 - 256, 715 - 512), [0, 160, 0, 255], 2), "{:?}", px(&t2, TILE_SIZE, 74, 203));
    assert!(p.key("f"));
    let fields = value(&mut p, "fields");
    assert_eq!(fields.at(1).get("value").as_str(), Some("Linus"));
    assert_eq!(fields.at(2).get("value"), &Data::Bool(true));
    assert!(p.key("n"));
    assert!(p.key("v"));
    let log = value(&mut p, "log");
    let parts: Vec<&str> = log.as_str().unwrap().split(' ').collect();
    assert!(parts[0].ends_with("v6") && parts[1].ends_with("v7") && parts[2] == "false", "{log:?}");
    assert!(p.key("m"));
    assert_eq!(value(&mut p, "count"), Data::Num(6.0));
    assert!(p.key("x"));
    assert_eq!(value(&mut p, "count"), Data::Num(6.0));
    assert_eq!(value(&mut p, "size3").get("w").num(), Some(792.0));
    assert!(p.key("s"));
    assert_eq!(value(&mut p, "log").as_str(), Some("saved 6 1"));
    // an image dropped as PNG bytes goes onto the page: left half red, right half blue
    p.file_drop("pic.png", Arc::new(png_bytes()));
    let doc = doc_of(&value(&mut p, "doc"));
    let s = doc.snapshot().unwrap();
    let img = render_page(&s, 0, 612.0, [255, 255, 255, 255], &|| false, &|_| {}).unwrap().unwrap();
    assert!(near(px(&img.rgba, img.w, 410, 655), [255, 0, 0, 255], 3), "{:?}", px(&img.rgba, img.w, 410, 655));
    assert!(near(px(&img.rgba, img.w, 450, 655), [0, 0, 255, 255], 3), "{:?}", px(&img.rgba, img.w, 450, 655));
    // the load job, with and without the structure
    let loaded = wait_for(&mut p, "loaded", |d| d.as_res().is_some());
    assert_eq!(doc_of(&loaded).snapshot().unwrap().page_count(), 4);
    assert_eq!(wait_for(&mut p, "loaded_count", |d| d.num().is_some()), Data::Num(4.0));
    // copy pages into a new document
    assert!(p.key("c"));
    assert_eq!(value(&mut p, "count"), Data::Num(2.0));
    let cell = p.cell_id("size3").unwrap();
    let st = p.parts().0.demand(cell).status;
    assert!(matches!(&st, Status::Error(e) if e.contains("page 3 does not exist")), "page 3 is gone in the copy: {st:?}");
    // errors are script errors with clear messages, never crashes
    assert!(p.key("e"));
    let log = value(&mut p, "log");
    let parts: Vec<&str> = log.as_str().unwrap().split('|').collect();
    assert_eq!(parts[0], "false");
    assert!(parts[1].contains("pdf.open") && parts[1].contains("not a PDF"), "{log:?}");
    assert!(parts[3].contains("page 99 does not exist"), "{log:?}");
    assert!(parts[5].contains("path ends in the middle"), "{log:?}");
    assert_eq!(p.handler_errors, 0);
    p.shutdown();
}

const LIMITS_PAGE: &str = r#"
Log = page.input("log", "")
Doc = page.input("doc", nil)
-- the tiles of a page whose document is still loading are blank
local params = page.derive("params", { Doc }, function(d) return { doc = d, page = 1 } end)
page.tiled({ rect = { 0, 0, 612, 792 }, source = "pdf.page", params = params })
page.on("key", function(k)
	local out = {}
	local function try(name, f, ...)
		local ok, err = pcall(f, ...)
		-- the message without its stack trace
		table.insert(out, name .. "=" .. (ok and "ok" or string.match(tostring(err), "^[^\n]*")))
	end
	if k == "a" then
		local big = pdf.new({ pages = 40 })
		big = pdf.add_text(big, 3, { x = 72, y = 72, text = "needle" })
		try("search_all", function() return big:search("needle") end)
		try("text_all", function() return big:text() end)
		try("search_page", function() assert(#big:search("needle", { page = 3 }) == 1) end)
		try("text_page", function() assert(big:text(3) == "needle") end)
		try("small_doc", function() assert(#pdf.new({ pages = 5 }):search("x") == 0) end)
	elseif k == "b" then
		local d = pdf.new({})
		try("nan_x", pdf.add_text, d, 1, { x = 0 / 0, text = "a" })
		try("inf_y", pdf.add_note, d, 1, { x = 1, y = 1 / 0, text = "a" })
		try("huge_width", pdf.measure_text, "a b c", { width = 1e9 })
		try("rotate_huge", pdf.rotate_page, d, 1, 90 * 2 ^ 1000)
	elseif k == "c" then
		local d = page.get(Doc)
		try("fill", pdf.fill_form, d, { name = "Linus", nosuch = 1 })
		local f = d:form_fields()
		table.insert(out, "name=" .. tostring(f[1].value) .. " " .. tostring(d))
	end
	page.set(Log, table.concat(out, "\n"))
	return true
end)
page.on("file_drop", function(f)
	page.set(Doc, pdf.open(f.data))
	return true
end)
"#;

#[test]
fn inline_limits_bad_numbers_and_failed_edits() {
    let mut p = load(LIMITS_PAGE).unwrap();
    // no document yet: blank tiles, no error tile
    let t = tile(&mut p, TileKey { page: 0, level: 0, tx: 0, ty: 0 });
    assert!(t.iter().all(|&b| b == 0));
    let get = |p: &mut ScriptPage| -> Vec<String> { value(p, "log").as_str().unwrap_or("").split('\n').map(str::to_string).collect() };
    assert!(p.key("a"));
    let log = get(&mut p);
    assert!(log[0].starts_with("search_all=") && log[0].contains("40 pages") && log[0].contains("pdf.search job"), "{log:?}");
    assert!(log[1].starts_with("text_all=") && log[1].contains("40 pages"), "{log:?}");
    assert_eq!(&log[2..], ["search_page=ok", "text_page=ok", "small_doc=ok"]);
    assert!(p.key("b"));
    let log = get(&mut p);
    assert!(log[0].contains("x must be a finite number"), "{log:?}");
    assert!(log[1].contains("y must be a finite number"), "{log:?}");
    assert!(log[2].contains("width must be"), "{log:?}");
    assert_eq!(log[3], "rotate_huge=ok");
    // a fill that names a missing field changes nothing, not even the version
    p.file_drop("fixture.pdf", fixture());
    assert!(p.key("c"));
    let log = get(&mut p);
    assert!(log[0].contains("no form field \"nosuch\""), "{log:?}");
    assert!(log[1].starts_with("name=Ada ") && log[1].ends_with("v0"), "{log:?}");
    assert_eq!(p.handler_errors, 0);
    p.shutdown();
}

#[test]
fn undeclared_module_is_refused() {
    let m = r#"{ "format": 1, "id": "t", "bounds": [0, 0, 100, 100], "capabilities": {} }"#;
    let pkg = Package::from_files("t", [("page.json", m), ("main.luau", "page.panel({ rect = { 0, 0, 10, 10 }, draw = function(d) end }) pdf.new()")]).unwrap();
    let e = ScriptPage::load(&pkg, &natives(), text(), wake()).err().unwrap();
    assert!(e.contains("\"pdf\" module is not declared"), "{e}");
}

fn demo_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("pages").join("pdf-demo")
}

#[test]
fn the_demo_page_loads_and_works() {
    let pkg = Package::from_dir(&demo_dir()).expect("demo package");
    let t0 = Instant::now();
    let mut p = ScriptPage::load(&pkg, &natives(), text(), wake()).expect("demo loads");
    let load_ms = t0.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(value(&mut p, "info").get("pages").num(), Some(2.0));
    // search job: the query starts as "revenue"
    let hits = wait_for(&mut p, "hits", |d| d.items().len() >= 3);
    assert_eq!(hits.items().iter().map(|h| h.get("page").num().unwrap() as i32).collect::<Vec<_>>(), [1, 1, 2]);
    // extraction job: the table written cell by cell comes back as data
    let ex = wait_for(&mut p, "extract", |d| !d.get("tables").items().is_empty());
    let t = ex.get("tables").at(1);
    assert_eq!(t.get("columns"), &Data::list(["Region", "Q1", "Q2", "Revenue"].iter().map(|s| Data::str(s)).collect()));
    assert_eq!(t.get("rows").at(1), &Data::list(vec![Data::str("North"), Data::Num(120.0), Data::Num(135.0), Data::Num(1250.0)]));
    assert_eq!(t.get("rows").items().len(), 4);
    assert!(ex.get("blocks").at(1).get("text").as_str().unwrap().starts_with("AnyPage Quarterly Report"));
    // the tiles show the page (title ink near the top left)
    let tl = tile(&mut p, TileKey { page: 0, level: 0, tx: 0, ty: 0 });
    write_png("demo-tile-0-0.png", TILE_SIZE, TILE_SIZE, &tl);
    assert!(dark_share(&tl, TILE_SIZE, 56, 56, 250, 82, 128) > 0.05);
    let doc = doc_of(&value(&mut p, "doc"));
    let s = doc.snapshot().unwrap();
    let img = render_page(&s, 0, 612.0, [255, 255, 255, 255], &|| false, &|_| {}).unwrap().unwrap();
    write_png("demo-page1.png", img.w, img.h, &img.rgba);
    // bars: the third (East, 1620.75) from x 300 to 360, up to y 520 - 162
    assert!(near(px(&img.rgba, img.w, 330, 450), [70, 130, 220, 255], 3));

    // click the page: a note where the pointer was
    assert_eq!(p.pointer_down(24.0 + 300.0, 84.0 + 650.0), PointerResponse::Handled);
    let doc = doc_of(&value(&mut p, "doc"));
    let annots = doc.with_lo(|d| Ok(edit::annotations(d, edit::page_id(d, 1.0)?))).unwrap();
    assert_eq!(annots.len(), 1);
    let r: Vec<f64> = annots[0].get("rect").items().iter().filter_map(Data::num).collect();
    assert!((r[0] - 290.0).abs() < 0.05 && (r[1] - 640.0).abs() < 0.05, "{r:?}");
    // typing replaces the query; CJK search
    assert!(p.key("escape"));
    p.text_input("\u{71DF}\u{6536}");
    let hits = wait_for(&mut p, "hits", |d| d.items().len() == 1);
    assert_eq!(hits.at(1).get("page").num(), Some(1.0));
    // the highlight button
    assert_eq!(p.pointer_down(660.0 + 460.0, 84.0 + 80.0), PointerResponse::Handled);
    let doc = doc_of(&value(&mut p, "doc"));
    let annots = doc.with_lo(|d| Ok(edit::annotations(d, edit::page_id(d, 1.0)?))).unwrap();
    assert!(annots.iter().any(|a| a.get("type").as_str() == Some("Highlight")));
    // the stamp button writes CJK and Latin
    assert_eq!(p.pointer_down(660.0 + 650.0, 84.0 + 80.0), PointerResponse::Handled);
    let doc = doc_of(&value(&mut p, "doc"));
    let words: Vec<String> = doc.snapshot().unwrap().text(0).unwrap().words().map(|w| w.text.clone()).collect();
    assert!(words.contains(&"REVIEWED".to_string()) && words.contains(&"\u{5DF2}\u{5BE9}\u{95B1}".to_string()), "{words:?}");
    // pages: arrows and the insert button
    assert!(p.key("right"));
    assert_eq!(value(&mut p, "page"), Data::Num(2.0));
    assert_eq!(p.pointer_down(660.0 + 200.0, 84.0 + 130.0), PointerResponse::Handled);
    assert_eq!(value(&mut p, "info").get("pages").num(), Some(3.0));
    assert_eq!(value(&mut p, "page"), Data::Num(3.0));
    // the semantic port
    let r: serde_json::Value = serde_json::from_str(&p.act(r#"{"action": "goto", "params": {"page": 1}}"#)).unwrap();
    assert_eq!(r["result"]["page"], 1);
    let r: serde_json::Value = serde_json::from_str(&p.act(r#"{"action": "extract", "params": {"page": 1}}"#)).unwrap();
    assert!(r["result"]["tables"][0].as_str().unwrap_or("").starts_with("Region,Q1,Q2,Revenue\nNorth,120,135,1250"), "{r}");
    // the stamp sits above the title, so it reads first
    assert_eq!(r["result"]["blocks"][0], "\u{5DF2}\u{5BE9}\u{95B1} REVIEWED", "{r}");
    assert_eq!(r["result"]["blocks"][1], "AnyPage Quarterly Report", "{r}");
    // search: the page shown now inline, every page from the job's cell
    let r: serde_json::Value = serde_json::from_str(&p.act(r#"{"action": "search", "params": {"query": "unaudited"}}"#)).unwrap();
    assert_eq!(r["result"]["page"], 1, "{r}");
    assert_eq!(r["result"]["this_page"].as_array().map(Vec::len), Some(0), "{r}");
    assert_eq!(r["result"]["all_pages"], "hits", "{r}");
    wait_for(&mut p, "hits", |d| d.items().len() == 1 && d.at(1).get("text").as_str() == Some("unaudited"));
    let r: serde_json::Value = serde_json::from_str(&p.act(r#"{"get": "hits"}"#)).unwrap();
    assert_eq!(r["value"][0]["page"], 2, "{r}");
    let r: serde_json::Value = serde_json::from_str(&p.act(r#"{"action": "search", "params": {"query": "Revenue"}}"#)).unwrap();
    assert_eq!(r["result"]["this_page"][0]["page"], 1, "{r}");
    let r: serde_json::Value = serde_json::from_str(&p.act(r#"{"action": "add_text", "params": {"x": 56, "y": 700, "text": "agent was here"}}"#)).unwrap();
    assert_eq!(r["ok"], true, "{r}");
    let r: serde_json::Value = serde_json::from_str(&p.act(r#"{"set": "query", "value": "agent"}"#)).unwrap();
    assert_eq!(r["ok"], true);
    let hits = wait_for(&mut p, "hits", |d| d.items().len() == 1 && d.at(1).get("text").as_str() == Some("agent"));
    assert_eq!(hits.at(1).get("page").num(), Some(1.0));
    // save: the host gets the file; it reopens with every edit
    assert_eq!(p.pointer_down(660.0 + 520.0, 84.0 + 130.0), PointerResponse::Handled);
    let bytes = loop {
        match p.take_request() {
            Some(PageRequest::SaveFile { name, bytes, .. }) => {
                assert_eq!(name, "anypage-edited.pdf");
                break bytes;
            }
            Some(_) => continue,
            None => panic!("no save request"),
        }
    };
    let saved = PdfDoc::open(bytes, "").unwrap();
    let s = saved.snapshot().unwrap();
    assert_eq!(s.page_count(), 3);
    let words: Vec<String> = s.text(0).unwrap().words().map(|w| w.text.clone()).collect();
    assert!(words.contains(&"REVIEWED".to_string()) && words.contains(&"agent".to_string()));
    let n = saved.with_lo(|d| Ok(edit::annotations(d, edit::page_id(d, 1.0)?).len())).unwrap();
    assert_eq!(n, 2, "note and highlight");
    // dropping a file opens it
    p.file_drop("fixture.pdf", fixture());
    assert_eq!(value(&mut p, "info").get("pages").num(), Some(4.0));
    assert_eq!(p.handler_errors, 0);
    eprintln!("demo page loaded in {load_ms:.0} ms ({})", p.summary());
    p.shutdown();
}
