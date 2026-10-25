//! The `cad` module driven through real pages: reading, measuring, picking,
//! editing, saving and reopening, vector export, encodings, bad input.

mod common;

use anypage_cad::CadDoc;
use anypage_engine::Page;
use anypage_runtime::Data;
use common::*;

const PANEL: &str = "page.panel({ rect = { 0, 0, 10, 10 }, draw = function(d) end })\n";

fn run(src: &str, files: Vec<(&'static str, Vec<u8>)>) {
    let n = natives(files);
    let full = format!("{PANEL}{src}");
    if let Err(e) = load_page(&full, &n) {
        panic!("{e}");
    }
}

fn sample() -> Vec<(&'static str, Vec<u8>)> {
    vec![("sample", sample_dxf())]
}

#[test]
fn reads_entities_layers_and_measures() {
    run(
        r##"
        local dwg = cad.open(test.bytes("sample"), "dxf")
        local info = dwg:info()
        assert(info.entities == 12, "entities " .. info.entities)
        assert(info.version == "R2010", info.version)
        assert(#info.warnings == 0, info.warnings[1])
        local ents = dwg:entities({})
        assert(#ents == 12, "listed " .. #ents)
        -- the two hatches keep their place before the last line
        assert(ents[10].type == "HATCH" and ents[11].type == "HATCH" and ents[12].type == "LINE", ents[10].type .. ents[12].type)
        assert(ents[10].solid == true and ents[11].pattern == "ANSI31")
        local circles = dwg:entities({ type = "CIRCLE" })
        assert(#circles == 2)
        local c = circles[1]
        assert(c.cx == 50 and c.cy == 50 and c.r == 20 and c.color == 0x0000FFFF and c.aci == 5, "circle")
        assert(c.bounds[1] == 30 and c.bounds[3] == 40, "bounds " .. c.bounds[1])
        -- layers
        local found = {}
        for _, l in dwg:layers() do found[l.name] = l end
        assert(found.RED.color == 0xFF0000FF and found.RED.on == true and found.RED.entities == 1)
        assert(found.HIDDEN.on == false)
        local red = dwg:entities({ layer = "red" })
        assert(#red == 1 and red[1].type == "LINE" and red[1].color == 0xFF0000FF and red[1].aci == 256)
        -- measures are exact for arcs and circles
        local m = dwg:measure({ c.id })
        assert(math.abs(m.length - 2 * math.pi * 20) < 1e-9, "circumference " .. m.length)
        assert(math.abs(m.area - math.pi * 400) < 1e-9, "area " .. m.area)
        local pl = dwg:entities({ type = "LWPOLYLINE" })[1]
        assert(pl.bulges[1] == 1 and #pl.points == 6)
        m = dwg:measure({ pl.id })
        assert(math.abs(m.length - (math.pi * 10 + 10)) < 1e-9, "polyline " .. m.length)
        local solid = ents[10]
        m = dwg:measure({ solid.id })
        assert(math.abs(m.area - 300) < 1e-9, "hatch area " .. m.area)
        -- inserts are measured through their block: two arms of 8
        local ins = dwg:entities({ type = "INSERT" })
        assert(#ins == 2 and ins[1].block == "MARK" and ins[1].sx == 2)
        m = dwg:measure({ ins[1].id })
        assert(math.abs(m.length - 16) < 1e-9, "insert " .. m.length)
        m = dwg:measure({ ins[1].id, ins[2].id, 999999 })
        assert(m.count == 2 and m.missing == 1)
        -- text
        local t = dwg:entities({ type = "TEXT" })[1]
        assert(t.text == "CAD" and t.height == 8)
        -- picking: nearest, topmost; empty space; hidden layers are not picked
        assert(dwg:pick(50, 10.4, 1) == red[1].id)
        assert(dwg:pick(50, 50, 1) == nil)
        assert(dwg:pick(80, 22, 0.5) == solid.id)
        assert(dwg:pick(50, 95, 1) == nil)
        assert(dwg:pick(50, 95, 1, { layers = { HIDDEN = true } }) ~= nil)
        -- window and crossing selection
        local inside = dwg:select({ 65, 12, 30, 20 }, { inside = true })
        assert(#inside == 1 and inside[1] == solid.id, "inside " .. #inside)
        local crossing = dwg:select({ 0, 0, 100, 12 })
        assert(#crossing == 1 and crossing[1] == red[1].id)
        local b = dwg:bounds()
        assert(b[1] == 10 and b[2] == 10, "bounds " .. b[1] .. "," .. b[2])
        "##,
        sample(),
    );
}

#[test]
fn edits_save_and_reopen() {
    run(
        r##"
        local dwg = cad.open(test.bytes("sample"))
        dwg = cad.add(dwg, {
            { type = "LINE", x1 = 0, y1 = 0, x2 = 3, y2 = 4, layer = "NEW", aci = 30 },
            { type = "HATCH", loops = { { 0, 0, 10, 0, 10, 10, 0, 10 }, { 2, 2, 8, 2, 8, 8, 2, 8 } }, color = 0x336699FF },
            { type = "TEXT", x = 5, y = 5, text = "標註 Ø20", height = 2 },
            { type = "MTEXT", x = 0, y = -10, text = "{\\fArial;line one}\\Pline two", height = 1.5 },
            { type = "SPLINE", control = { 0, 0, 1, 2, 3, 2, 4, 0 } },
            { type = "INSERT", block = "MARK", x = 50, y = 90, attributes = { { tag = "NO", value = "A-1" } } },
        })
        local ids = dwg:last_ids()
        assert(#ids == 6)
        assert(dwg:measure({ ids[1] }).length == 5)
        assert(math.abs(dwg:measure({ ids[2] }).area - 64) < 1e-9, "hatch with hole")
        assert(dwg:entity(ids[1]).layer == "NEW" and dwg:entity(ids[1]).aci == 30)
        assert(dwg:entity(ids[4]).text == "line one\nline two", dwg:entity(ids[4]).text)
        local spline_len = dwg:measure({ ids[5] }).length
        assert(spline_len > 4 and spline_len < 3 + 2 + 3, "spline " .. spline_len)

        -- move, including the mirrored OCS circle (its OCS x moves the other way)
        local circles = dwg:entities({ type = "CIRCLE" })
        local circle, ocs = circles[1], circles[2]
        dwg = cad.move(dwg, { circle.id }, 5, -5)
        assert(dwg:entity(circle.id).cx == 55 and dwg:entity(circle.id).cy == 45)
        local before = dwg:entity(ocs.id).bounds[1]
        dwg = cad.move(dwg, { ocs.id }, 2, 0)
        assert(dwg:entity(ocs.id).cx == -90, "OCS x " .. dwg:entity(ocs.id).cx)
        assert(math.abs(dwg:entity(ocs.id).bounds[1] - (before + 2)) < 1e-9)
        -- hatches and inserts move too
        dwg = cad.move(dwg, { ids[2], ids[6] }, 1, 1)
        assert(dwg:entity(ids[2]).bounds[1] == 1 and dwg:entity(ids[6]).x == 51)
        -- layer and colour
        dwg = cad.set_layer(dwg, { circle.id }, "RED")
        assert(dwg:entity(circle.id).layer == "RED")
        dwg = cad.set_color(dwg, { circle.id }, "bylayer")
        assert(dwg:entity(circle.id).color == 0xFF0000FF)
        dwg = cad.set_color(dwg, { circle.id }, { color = 0x10203040 })
        assert(dwg:entity(circle.id).true_color == 0x102030)
        dwg = cad.set_layer(dwg, { ids[1] }, "BRAND_NEW")
        local names = {}
        for _, l in dwg:layers() do names[l.name] = true end
        assert(names.BRAND_NEW and names.NEW)
        -- copy
        dwg = cad.copy(dwg, { circle.id, ids[2] }, 100, 0)
        local copies = dwg:last_ids()
        assert(#copies == 2 and dwg:entity(copies[1]).cx == 155)
        -- delete
        local hidden = dwg:entities({ layer = "HIDDEN" })[1]
        dwg = cad.delete(dwg, { hidden.id })
        assert(dwg:entity(hidden.id) == nil)
        -- layer table edits
        dwg = cad.add_layer(dwg, { name = "PLOT", aci = 1, on = false })
        dwg = cad.layer_on(dwg, "PLOT", true)

        -- save and reopen: same entities, same ids, same order
        local bytes = dwg:save()
        assert(buffer.len(bytes) > 1000)
        local again = cad.open(bytes)
        local a, b = dwg:entities({}), again:entities({})
        assert(#a == #b, #a .. " vs " .. #b)
        for i = 1, #a do
            assert(a[i].id == b[i].id and a[i].type == b[i].type and a[i].layer == b[i].layer,
                i .. ": " .. a[i].type .. "#" .. a[i].id .. " vs " .. b[i].type .. "#" .. b[i].id)
        end
        assert(again:entity(copies[1]).cx == 155)
        assert(math.abs(again:measure({ ids[2] }).area - 64) < 1e-9)
        assert(again:entity(ids[2]).true_color == 0x336699)
        assert(again:entity(ids[3]).text == "標註 Ø20", again:entity(ids[3]).text)
        assert(again:entity(ids[6]).attributes[1].value == "A-1")
        local found
        for _, l in again:layers() do if l.name == "PLOT" then found = l end end
        assert(found and found.on and found.aci == 1)
        -- a third generation is byte-stable in content
        local third = cad.open(again:save())
        assert(#third:entities({}) == #b)
        "##,
        sample(),
    );
}

#[test]
fn vector_paths_in_the_shared_form() {
    run(
        r##"
        local dwg = cad.open(test.bytes("sample"))
        local paths = dwg:to_paths({ fit = { 0, 0, 200, 200 }, flip_y = true })
        assert(#paths > 10, "paths " .. #paths)
        local red = dwg:entities({ layer = "RED" })[1]
        local line, text, fill
        for _, p in paths do
            assert(p.path[1] == "M", "starts with M")
            assert(p.stroke or p.fill, "stroke or fill")
            if p.id == red.id then line = p end
            if p.text == "CAD" then text = p end
            if p.fill and p.even_odd then fill = p end
        end
        assert(line and line.stroke == 0xFF0000FF and line.width == 0.25)
        assert(#line.path == 6 and line.path[4] == "L")
        -- the drawing is fitted into 200 x 200 with y down: the line at the bottom
        for i = 2, #line.path, 3 do
            local x, y = line.path[i], line.path[i + 1]
            assert(x >= 0 and x <= 200 and y > 150 and y <= 200, x .. "," .. y)
        end
        assert(text and text.fill == 0xFF00FFFF and #text.path > 20)
        assert(fill and fill.path[#fill.path] == "Z")
        -- arcs come out as cubic Béziers
        local arc = dwg:entities({ type = "ARC" })[1]
        for _, p in paths do
            if p.id == arc.id then
                assert(p.path[4] == "C", "arc as C")
            end
        end
        -- only some layers, ACI 7 in a chosen colour, no text
        local only = dwg:to_paths({ layers = { "RED" }, text = false })
        assert(#only == 1)
        local raw = dwg:to_paths({ flip_y = false })
        local ok = false
        for _, p in raw do
            if p.id == red.id then ok = p.path[2] == 10 and p.path[3] == 10 end
        end
        assert(ok, "unfitted paths are in drawing units")
        "##,
        sample(),
    );
}

#[test]
fn errors_are_clear_and_effects_stay_in_handlers() {
    let n = natives(sample());
    let e = load_page(&format!("{PANEL} cad.open(nil)"), &n).err().unwrap();
    assert!(e.contains("cad.open needs the file's bytes"), "{e}");
    let e = load_page(&format!("{PANEL} cad.open('not a dxf at all')"), &n).err().unwrap();
    assert!(e.contains("cad.open"), "{e}");
    let e = load_page(&format!("{PANEL} cad.open(test.bytes('sample'), 'dwg')"), &n).err().unwrap();
    assert!(e.contains("DWG is not supported"), "{e}");
    let e = load_page(&format!("{PANEL} cad.add(cad.new(), {{ type = 'BLOB' }})"), &n).err().unwrap();
    assert!(e.contains("cannot add a BLOB"), "{e}");
    let e = load_page(&format!("{PANEL} cad.add(cad.new(), {{ type = 'CIRCLE', cx = 0, cy = 0, r = -1 }})"), &n).err().unwrap();
    assert!(e.contains("r must be > 0"), "{e}");
    let e = load_page(&format!("{PANEL} cad.layers(42)"), &n).err().unwrap();
    assert!(e.contains("expected a drawing"), "{e}");
    let e = load_page(&format!("{PANEL} cad.step_to_mesh('ISO-10303-21;')"), &n).err();
    if cfg!(not(feature = "step")) {
        assert!(e.unwrap().contains("feature \"step\""));
    }
    // edits from a cell function are refused
    let src = format!(
        r#"{PANEL}
        local D = page.input("d", cad.new())
        Bad = page.derive("bad", {{ D }}, function(d) return cad.add(d, {{ type = "POINT", x = 1, y = 1 }}) end)
        "#
    );
    let mut p = load_page(&src, &n).unwrap();
    let r = p.act(r#"{"get": "bad"}"#);
    assert!(r.contains("changes state"), "{r}");
}

#[test]
fn legacy_code_pages_and_unicode_escapes() {
    // AutoCAD 2000 file in Big5 (Taiwan): 中文 is A4 A4 A4 E5
    let mut big5 = b"  0\r\nSECTION\r\n  2\r\nHEADER\r\n  9\r\n$ACADVER\r\n  1\r\nAC1015\r\n  9\r\n$DWGCODEPAGE\r\n  3\r\nANSI_950\r\n  0\r\nENDSEC\r\n  0\r\nSECTION\r\n  2\r\nENTITIES\r\n  0\r\nTEXT\r\n  5\r\n20\r\n  8\r\n0\r\n 10\r\n0.0\r\n 20\r\n0.0\r\n 40\r\n2.5\r\n  1\r\n".to_vec();
    big5.extend_from_slice(&[0xA4, 0xA4, 0xA4, 0xE5]);
    big5.extend_from_slice(b" \\U+00B1\r\n  0\r\nENDSEC\r\n  0\r\nEOF\r\n");
    run(
        r##"
        local dwg = cad.open(test.bytes("big5"))
        local t = dwg:entities({ type = "TEXT" })[1]
        assert(t.text == "中文 ±", t.text)
        assert(dwg:info().encoding == "Big5", dwg:info().encoding)
        "##,
        vec![("big5", big5)],
    );
}

#[test]
fn r12_polylines_meshes_and_block_hatches() {
    // R12: POLYLINE with VERTEX bulges and widths, a 3D polyline; a block
    // holding a HATCH, inserted twice
    let mut s = String::new();
    let mut pair = |c: i32, v: &str| s.push_str(&format!("{c:>3}\n{v}\n"));
    for (c, v) in [(0, "SECTION"), (2, "BLOCKS"), (0, "BLOCK"), (8, "0"), (2, "PAD"), (70, "0"), (10, "0.0"), (20, "0.0"), (30, "0.0"), (3, "PAD")] {
        pair(c, v);
    }
    let h = hatch("3A0", "0", 0, true, &[(0.0, 0.0), (4.0, 0.0), (4.0, 2.0), (0.0, 2.0)]);
    let mut body = s.clone();
    body.push_str(&h.replace("\r\n", "\n"));
    let mut s = body;
    let mut pair = |c: i32, v: &str| s.push_str(&format!("{c:>3}\n{v}\n"));
    for (c, v) in [(0, "ENDBLK"), (8, "0"), (0, "ENDSEC"), (0, "SECTION"), (2, "ENTITIES")] {
        pair(c, v);
    }
    // 2D polyline: (0,0) -bulge 1-> (10,0) -> (10,10), width 0.5
    for (c, v) in [(0, "POLYLINE"), (5, "40"), (8, "PL"), (66, "1"), (10, "0.0"), (20, "0.0"), (30, "0.0"), (70, "0"), (40, "0.5"), (41, "0.5")] {
        pair(c, v);
    }
    for (x, y, b) in [("0.0", "0.0", "1.0"), ("10.0", "0.0", "0.0"), ("10.0", "10.0", "0.0")] {
        for (c, v) in [(0, "VERTEX"), (8, "PL"), (10, x), (20, y), (30, "0.0"), (42, b)] {
            pair(c, v);
        }
    }
    pair(0, "SEQEND");
    pair(8, "PL");
    // 3D polyline
    for (c, v) in [(0, "POLYLINE"), (5, "41"), (8, "PL"), (66, "1"), (10, "0.0"), (20, "0.0"), (30, "0.0"), (70, "8")] {
        pair(c, v);
    }
    for (x, y, z) in [("0.0", "20.0", "0.0"), ("3.0", "24.0", "7.0")] {
        for (c, v) in [(0, "VERTEX"), (8, "PL"), (10, x), (20, y), (30, z), (70, "32")] {
            pair(c, v);
        }
    }
    pair(0, "SEQEND");
    for (x, hd) in [("50.0", "42"), ("60.0", "43")] {
        for (c, v) in [(0, "INSERT"), (5, hd), (8, "0"), (62, "1"), (2, "PAD"), (10, x), (20, "50.0"), (30, "0.0")] {
            pair(c, v);
        }
    }
    pair(0, "ENDSEC");
    pair(0, "EOF");
    let bytes = s.into_bytes();
    // pixels: the block hatch is filled red at both inserts
    let doc = CadDoc::open(&bytes).expect("R12 file");
    let dwg = Data::Res(anypage_runtime::Res::new(anypage_cad::KIND, doc));
    let (doc, _) = anypage_cad::doc_of(&dwg).unwrap();
    let opts = Data::map([("rect", Data::nums([0.0, 0.0, 100.0, 100.0])), ("width_px", Data::Num(400.0)), ("background", Data::Num(255.0))]);
    let img = anypage_cad::render_image(&doc, &opts, &|| false).unwrap();
    save_png("cad_r12_block_hatch.png", img.w, img.h, &img.rgba);
    for x in [52.0, 62.0] {
        let (px_, py_) = ((x * 4.0) as u32, ((100.0 - 51.0) * 4.0) as u32);
        assert!(near(px(&img.rgba, img.w, px_, py_), [255, 0, 0], 2), "block hatch BYBLOCK red at x {x}: {:?}", px(&img.rgba, img.w, px_, py_));
    }
    run(
        r##"
        local dwg = cad.open(test.bytes("r12"))
        local pls = dwg:entities({ type = "POLYLINE" })
        assert(#pls == 2)
        local m = dwg:measure({ pls[1].id })
        assert(math.abs(m.length - (math.pi * 5 + 10)) < 1e-9, "polyline " .. m.length)
        assert(math.abs(dwg:measure({ pls[2].id }).length - 5) < 1e-9, "3D polyline in plan")
        local ins = dwg:entities({ type = "INSERT" })
        assert(math.abs(dwg:measure({ ins[1].id }).area - 8) < 1e-9, "block hatch area")
        -- save (upgraded to a version that has HATCH) and reopen: the block hatch survives
        local again = cad.open(dwg:save())
        assert(again:info().version == "R2000", again:info().version)
        local ins2 = again:entities({ type = "INSERT" })
        assert(math.abs(again:measure({ ins2[2].id }).area - 8) < 1e-9, "block hatch after save")
        assert(math.abs(again:measure({ again:entities({ type = "POLYLINE" })[1].id }).length - (math.pi * 5 + 10)) < 1e-9)
        "##,
        vec![("r12", bytes)],
    );
}

#[test]
fn hostile_input_never_panics() {
    let sample = sample_dxf();
    // truncations
    for cut in [0, 1, 10, 100, sample.len() / 3, sample.len() / 2, sample.len() - 20] {
        let _ = CadDoc::open(&sample[..cut]);
    }
    // byte flips (deterministic)
    let mut seed = 0x2545F4914F6CDD1Du64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut opened = 0;
    for _ in 0..300 {
        let mut b = sample.clone();
        for _ in 0..4 {
            let i = (next() % b.len() as u64) as usize;
            b[i] = (next() % 256) as u8;
        }
        if let Ok(doc) = CadDoc::open(&b) {
            opened += 1;
            let _ = doc.save();
        }
    }
    eprintln!("{opened}/300 corrupted files still opened");
    // absurd numbers
    let s = "  0\nSECTION\n  2\nENTITIES\n  0\nCIRCLE\n  5\n10\n 10\n1e308\n 20\n-1e308\n 40\n1e308\n  0\nLINE\n  5\n11\n 10\nNaN\n 20\n0\n 11\ninf\n 21\n0\n  0\nENDSEC\n  0\nEOF\n";
    if let Ok(doc) = CadDoc::open(s.as_bytes()) {
        let dwg = Data::Res(anypage_runtime::Res::new(anypage_cad::KIND, doc));
        let (doc, _) = anypage_cad::doc_of(&dwg).unwrap();
        let _ = anypage_cad::render_image(&doc, &Data::Nil, &|| false);
    }
    // hatch with absurd counts
    let h = "  0\nSECTION\n  2\nENTITIES\n  0\nHATCH\n  5\n10\n100\nAcDbHatch\n 10\n0\n 20\n0\n 30\n0\n  2\nSOLID\n 70\n1\n 71\n0\n 91\n999999999\n  0\nENDSEC\n  0\nEOF\n";
    let doc = CadDoc::open(h.as_bytes()).expect("the rest of the file still opens");
    assert!(doc.scene().groups.is_empty());
}

#[test]
fn inserts_with_attributes_from_2010_files() {
    // what the dxf crate itself writes for an INSERT with attributes in a 2010 file:
    // ATTRIB, an embedded MTEXT, ATTRIB, MTEXT, SEQEND
    let mut d = dxf::Drawing::new();
    d.header.version = dxf::enums::AcadVersion::R2010;
    d.add_block(dxf::Block { name: "TAG".into(), ..Default::default() });
    let mut ins = dxf::entities::Insert { name: "TAG".into(), location: dxf::Point::new(5.0, 5.0, 0.0), ..Default::default() };
    for (tag, value) in [("A", "first"), ("B", "second")] {
        ins.add_attribute(&mut d, dxf::entities::Attribute { attribute_tag: tag.into(), value: value.into(), location: dxf::Point::new(5.0, 5.0, 0.0), ..Default::default() });
    }
    d.add_entity(dxf::entities::Entity::new(dxf::entities::EntityType::Insert(ins)));
    let mut bytes = Vec::new();
    d.save(&mut bytes).unwrap();
    run(
        r##"
        local dwg = cad.open(test.bytes("attrs"))
        local ents = dwg:entities({})
        assert(#ents == 1 and ents[1].type == "INSERT", #ents .. " " .. ents[#ents].type)
        assert(#ents[1].attributes == 2 and ents[1].attributes[2].value == "second")
        local again = cad.open(dwg:save())
        local e2 = again:entities({})
        assert(#e2 == 1 and #e2[1].attributes == 2, "after save " .. #e2)
        "##,
        vec![("attrs", bytes)],
    );
}

#[test]
fn load_and_render_jobs() {
    use anypage_core::Status;
    use std::time::{Duration, Instant};
    let src = r##"
        page.panel({ rect = { 0, 0, 10, 10 }, draw = function(d) end })
        local B = page.input("bytes", test.bytes("sample"))
        Loaded = page.job("loaded", "cad.load", { B })
        local O = page.input("opts", { width_px = 320, background = 0x000000FF })
        Img = page.job("img", "cad.render", { Loaded, O })
        page.image({ rect = { 0, 20, 320, 320 }, source = Img })
    "##;
    let n = natives(vec![("sample", sample_dxf())]);
    let mut p = load_page(src, &n).unwrap_or_else(|e| panic!("{e}"));
    let panel = p.spec().panels[1].cell;
    let t = Instant::now();
    loop {
        let g = p.parts().0;
        g.commit();
        let i = g.demand(panel);
        if let Some(img) = i.get::<anypage_engine::Image>().filter(|img| img.w > 1) {
            assert_eq!(img.w, 320);
            // the red line of the sample is in the picture
            assert!(img.rgba.chunks(4).any(|px| px[0] > 200 && px[1] < 40 && px[2] < 40));
            break;
        }
        assert!(!matches!(i.status, Status::Error(_)), "{:?}", i.status);
        assert!(t.elapsed() < Duration::from_secs(10), "jobs timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
    let loaded = p.cell_value("loaded").unwrap();
    assert_eq!(loaded.as_res().map(|r| r.kind().to_string()), Some("cad.drawing".to_string()));
    p.shutdown();
}

#[test]
fn nested_instances_in_paths_entities_and_edits() {
    run(
        r##"
        local dwg = cad.open(test.bytes("nested"))
        local ins = dwg:entities({ type = "INSERT" })
        assert(#ins == 2 and ins[1].block == "OUTER")
        -- bounds of an instance: x 0..30, y 0..4 for the first
        local b = ins[1].bounds
        assert(b[1] == 0 and b[2] == 0 and b[3] == 30 and b[4] == 4, b[1] .. " " .. b[2] .. " " .. b[3] .. " " .. b[4])
        -- vector paths: every line drawn, colours resolved per instance
        local paths = dwg:to_paths({ flip_y = false })
        assert(#paths == 12, #paths)
        local colours = {}
        for _, p in paths do
            assert(p.id == ins[1].id or p.id == ins[2].id)
            local key = string.format("%08X@%g,%g", p.stroke, p.path[2], p.path[3])
            colours[key] = true
        end
        for _, want in { "00FF00FF@0,0", "FF0000FF@0,2", "0000FFFF@0,4", "FFFF00FF@20,0", "FF0000FF@20,2", "000000FF@0,10", "FFFF00FF@20,10" } do
            assert(colours[want], "missing " .. want)
        end
        -- moving an insert moves everything inside it; measure follows
        dwg = cad.move(dwg, { ins[1].id }, 100, 0)
        assert(dwg:entity(ins[1].id).bounds[1] == 100)
        assert(dwg:pick(105, 4, 0.1) == ins[1].id and dwg:pick(5, 4, 0.1) == nil)
        assert(dwg:measure({ ins[1].id }).length == 60)
        -- copy of an insert, then delete the original
        dwg = cad.copy(dwg, { ins[2].id }, 0, 100)
        local copy = dwg:last_ids()[1]
        dwg = cad.delete(dwg, { ins[2].id })
        assert(dwg:pick(25, 110, 0.1) == copy and dwg:pick(25, 10, 0.1) == nil)
        "##,
        vec![("nested", nested_colour_dxf())],
    );
}
