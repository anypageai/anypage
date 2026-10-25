//! The module's Rust side against generated PDFs: structure, text with
//! positions, search, extraction, rendering (checked by pixel), tiles, edits,
//! save and reopen.

mod common;

use std::sync::Arc;
use std::time::Instant;

use anypage_core::{Rect, TileKey, TILE_SIZE};
use anypage_pdf::edit::{self, ImageSrc, NoteOpts, TextOpts};
use anypage_pdf::fonts::widths;
use anypage_pdf::render::{render_page, PdfTiles, TileParams};
use anypage_pdf::text::{Cell, Word};
use anypage_pdf::{PdfDoc, Snapshot};
use anypage_runtime::Data;
use common::*;

const WHITE: [u8; 4] = [255, 255, 255, 255];

fn open() -> PdfDoc {
    PdfDoc::open(fixture(), "").expect("fixture opens")
}

fn words(s: &Snapshot, i: usize) -> Vec<Word> {
    s.text(i).expect("text").words().cloned().collect()
}

fn word<'a>(ws: &'a [Word], t: &str) -> &'a Word {
    ws.iter().find(|w| w.text == t).unwrap_or_else(|| panic!("no word {t:?} in {:?}", ws.iter().map(|w| &w.text).collect::<Vec<_>>()))
}

fn width(font: &str, s: &str, size: f64) -> f64 {
    let w = widths(font);
    s.bytes().map(|b| w[b as usize] as f64).sum::<f64>() * size / 1000.0
}

fn close(a: f32, b: f64, tol: f64) -> bool {
    (a as f64 - b).abs() <= tol
}

fn render(s: &Snapshot, i: usize, w: f64) -> (u32, u32, Vec<u8>) {
    let img = render_page(s, i, w, WHITE, &|| false, &|_| {}).expect("render").expect("not cancelled");
    (img.w, img.h, img.rgba)
}

#[test]
fn structure_metadata_outline_forms() {
    let doc = open();
    let s = doc.snapshot().unwrap();
    assert_eq!(s.page_count(), 4);
    assert_eq!(s.geoms[0].size(), (612.0, 792.0));
    assert_eq!(s.geoms[2].size(), (792.0, 612.0), "page 3 is rotated");
    let m = s.pdf.metadata();
    assert_eq!(anypage_pdf::util::decode_text_string(m.author.as_deref().unwrap()), "Tester \u{6E2C}\u{8A66}");
    let outline = doc.with_lo(|d| Ok(edit::outline(d))).unwrap();
    assert_eq!(outline, vec![("Report".to_string(), Some(0), 0), ("Table".to_string(), Some(0), 1), ("\u{4E2D}\u{6587}".to_string(), Some(1), 0)]);
    let fields = doc.with_lo(|d| Ok(edit::fields(d))).unwrap();
    let names: Vec<(&str, &str)> = fields.iter().map(|f| (f.name.as_str(), f.kind)).collect();
    assert_eq!(names, [("name", "text"), ("agree", "checkbox"), ("color", "radio"), ("size", "choice"), ("address.city", "text")]);
    assert_eq!(fields[0].value, Data::str("Ada"));
    assert_eq!(fields[1].value, Data::Bool(false));
    assert_eq!(fields[2].value, Data::str("red"));
    assert_eq!(fields[2].states, ["red", "blue"]);
    assert_eq!(fields[3].options, ["S", "M", "L"]);
    // geometry agrees between lopdf (edits) and hayro (rendering) on every page
    doc.with_lo(|d| {
        for (i, pid) in edit::page_ids(d).into_iter().enumerate() {
            assert_eq!(edit::geom(d, pid), s.geoms[i], "page {}", i + 1);
            let ours = s.geoms[i].user_to_page().as_coeffs();
            let hayro = s.page(i).unwrap().initial_transform(true).as_coeffs();
            for k in 0..6 {
                assert!((ours[k] - hayro[k]).abs() < 1e-9, "page {} transform {ours:?} vs {hayro:?}", i + 1);
            }
        }
        Ok(())
    })
    .unwrap();
}

#[test]
fn text_with_positions_page_one() {
    let doc = open();
    let s = doc.snapshot().unwrap();
    let ws = words(&s, 0);
    // Helvetica-Bold 24 at user (72, 720): baseline 72 from the top
    let q = word(&ws, "Quarterly");
    assert!(close(q.rect[0], 72.0, 0.01) && close(q.rect[1], 72.0 - 0.8 * 24.0, 0.01) && close(q.rect[3], 24.0, 0.01), "{q:?}");
    assert!(close(q.rect[2], width("Helvetica-Bold", "Quarterly", 24.0), 0.01), "{q:?}");
    // the AFM widths themselves: H e l l o in Helvetica
    assert_eq!(width("Helvetica", "Hello", 1000.0), 722.0 + 556.0 + 222.0 + 222.0 + 556.0);
    // TJ: -250 thousandths separate words, a +80 kern does not
    let ordered: Vec<&str> = ws.iter().map(|w| w.text.as_str()).collect();
    let i = ordered.iter().position(|w| *w == "The").unwrap();
    assert_eq!(&ordered[i..i + 5], ["The", "quick", "brown", "fox", "Water"]);
    let quick = word(&ws, "quick");
    assert!(close(quick.rect[0], 72.0 + width("Times-Roman", "The", 12.0) + 3.0, 0.01), "{quick:?}");
    // Tw widens spaces
    let over = word(&ws, "over");
    assert!(close(over.rect[0], 72.0 + width("Times-Roman", "jumps ", 12.0) + 2.0, 0.01), "{over:?}");
    // Courier: 600 per glyph
    let c = word(&ws, "Courier");
    assert!(close(c.rect[2], 50.4, 0.001), "{c:?}");
    let digits = word(&ws, "12345");
    assert!(close(digits.rect[0], 72.0 + 8.0 * 7.2, 0.001), "{digits:?}");
    // /Differences and ToUnicode
    assert!(close(word(&ws, "\u{00C9}\u{2022}").rect[1], 792.0 - 500.0 - 9.6, 0.01));
    assert!(close(word(&ws, "\u{03A9}fi").rect[1], 792.0 - 480.0 - 9.6, 0.01));
    // invisible text (OCR layers) is text
    assert!(close(word(&ws, "invisible").rect[1], 792.0 - 250.0 - 8.0, 0.01));
    // text inside a form XObject, moved by its matrix
    let f = word(&ws, "Inside");
    assert!(close(f.rect[0], 300.0, 0.01) && close(f.rect[1], 792.0 - 250.0 - 9.6, 0.01), "{f:?}");
    // text rotated by its matrix reads upward: a tall narrow box
    let v = word(&ws, "Vertical");
    assert!(close(v.rect[0], 560.0 - 9.6, 0.01) && close(v.rect[2], 12.0, 0.01), "{v:?}");
    assert!(close(v.rect[3], width("Helvetica-Bold", "Vertical", 12.0), 0.01), "{v:?}");
    assert!(close(v.rect[1] + v.rect[3], 692.0, 0.01), "{v:?}");
}

#[test]
fn cjk_rotated_page_and_form_text() {
    let doc = open();
    let s = doc.snapshot().unwrap();
    let ws = words(&s, 1);
    // Identity-H with ToUnicode, full width (DW 1000)
    let a = word(&ws, "\u{4E2D}\u{6587}\u{6E2C}\u{8A66}");
    assert!(close(a.rect[0], 72.0, 0.01) && close(a.rect[2], 64.0, 0.01) && close(a.rect[1], 92.0 - 12.8, 0.01), "{a:?}");
    // predefined UniCNS-UTF16-H CMap without ToUnicode
    let b = word(&ws, "\u{8868}\u{683C}");
    assert!(close(b.rect[2], 32.0, 0.01), "{b:?}");
    // Latin and CJK fonts on one line, no gap: one word
    let m = word(&ws, "PDF\u{6587}\u{4EF6}");
    assert!(close(m.rect[2], 32.0 + 32.0, 0.01), "{m:?}");
    // rotated page: user (x, y) is displayed at (y, x)
    let ws = words(&s, 2);
    let r = word(&ws, "Rotated");
    assert!(close(r.rect[0], 720.0 - 0.2 * 14.0, 0.01) && close(r.rect[1], 72.0, 0.01), "{r:?}");
    assert!(close(r.rect[2], 14.0, 0.01) && close(r.rect[3], width("Helvetica", "Rotated", 14.0), 0.01), "{r:?}");
    // form field appearances are on the page
    let ws = words(&s, 3);
    assert!(ws.iter().any(|w| w.text == "Ada"));
    assert!(ws.iter().any(|w| w.text == "Name:"));
}

#[test]
fn search_and_extract() {
    let doc = open();
    let s = doc.snapshot().unwrap();
    let mut hits = Vec::new();
    for i in 0..s.page_count() {
        s.text(i).unwrap().search(i + 1, "QUICK brown", false, 100, &mut hits);
    }
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].page, 1);
    assert_eq!(hits[0].text, "quick brown");
    let ws = words(&s, 0);
    let (q, b) = (word(&ws, "quick"), word(&ws, "brown"));
    let r = hits[0].rects[0];
    assert!(close(r[0], q.rect[0] as f64, 0.01) && close(r[0] + r[2], (b.rect[0] + b.rect[2]) as f64, 0.01), "{r:?}");
    // across pages, CJK, case-sensitive
    let mut h2 = Vec::new();
    for i in 0..s.page_count() {
        s.text(i).unwrap().search(i + 1, "quarterly", false, 100, &mut h2);
    }
    assert_eq!(h2.iter().map(|h| h.page).collect::<Vec<_>>(), [1, 2]);
    let mut h3 = Vec::new();
    s.text(1).unwrap().search(2, "\u{8868}\u{683C}", false, 100, &mut h3);
    assert_eq!(h3.len(), 1);
    let mut h4 = Vec::new();
    s.text(0).unwrap().search(1, "quarterly", true, 100, &mut h4);
    assert!(h4.is_empty());

    // blocks in reading order, the table as data
    let t = s.text(0).unwrap();
    let plain = t.plain();
    assert!(plain.starts_with("Quarterly Report"), "{plain}");
    assert!(plain.find("quick").unwrap() < plain.find("Courier").unwrap());
    assert_eq!(t.tables.len(), 1, "{:?}", t.tables);
    let tb = &t.tables[0];
    assert_eq!(tb.columns, ["Item", "Qty", "Price"]);
    assert_eq!(tb.rows.len(), 3);
    assert_eq!(tb.rows[0], [Cell::Str("Apples".into()), Cell::Num(3.0), Cell::Num(1200.5)]);
    assert_eq!(tb.rows[2][2], Cell::Num(-3.0));
}

#[test]
fn render_checks_pixels() {
    let doc = open();
    let s = doc.snapshot().unwrap();
    let t0 = Instant::now();
    let (w, h, px1) = render(&s, 0, 612.0);
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    write_png("page1.png", w, h, &px1);
    assert_eq!((w, h), (612, 792));
    assert!(near(px(&px1, w, 450, 167), [255, 0, 0, 255], 2), "red rect {:?}", px(&px1, w, 450, 167));
    assert!(near(px(&px1, w, 475, 292), [0, 0, 255, 255], 2), "blue line {:?}", px(&px1, w, 475, 292));
    assert!(near(px(&px1, w, 475, 392), [0, 153, 0, 255], 2), "green disc {:?}", px(&px1, w, 475, 392));
    // the 2x2 image: red green / blue white
    assert!(near(px(&px1, w, 97, 417), [255, 0, 0, 255], 8), "{:?}", px(&px1, w, 97, 417));
    assert!(near(px(&px1, w, 147, 417), [0, 255, 0, 255], 8), "{:?}", px(&px1, w, 147, 417));
    assert!(near(px(&px1, w, 97, 467), [0, 0, 255, 255], 8), "{:?}", px(&px1, w, 97, 467));
    assert!(near(px(&px1, w, 147, 467), [255, 255, 255, 255], 8), "{:?}", px(&px1, w, 147, 467));
    assert_eq!(px(&px1, w, 600, 780), WHITE);
    // the title has ink; the invisible text has none
    assert!(dark_share(&px1, w, 72, 53, 250, 77, 128) > 0.1);
    assert!(dark_share(&px1, w, 72, 532, 160, 545, 200) < 0.001);
    // text drawn by a form XObject, and text turned by its matrix
    assert!(dark_share(&px1, w, 300, 533, 370, 545, 128) > 0.05, "form XObject text");
    assert!(dark_share(&px1, w, 551, 640, 562, 692, 128) > 0.05, "rotated text");
    // rotated page: the square at user (500..550, 100..150) shows at (100..150, 500..550)
    let (w3, h3, px3) = render(&s, 2, 792.0);
    write_png("page3-rotated.png", w3, h3, &px3);
    assert_eq!((w3, h3), (792, 612));
    assert!(near(px(&px3, w3, 125, 525), [0, 0, 0, 255], 2), "{:?}", px(&px3, w3, 125, 525));
    assert_eq!(px(&px3, w3, 300, 300), WHITE);
    assert!(dark_share(&px3, w3, 718, 72, 731, 140, 128) > 0.05, "rotated text has ink");
    // CJK through a system font, when the machine has one
    let (w2, h2, px2) = render(&s, 1, 612.0);
    write_png("page2-cjk.png", w2, h2, &px2);
    if system_cjk_font() {
        assert!(dark_share(&px2, w2, 72, 80, 136, 95, 128) > 0.05, "CJK glyphs drawn");
        assert!(dark_share(&px2, w2, 72, 130, 104, 145, 128) > 0.05, "UniCNS glyphs drawn");
    } else {
        eprintln!("no CJK system font: CJK rendering not checked");
    }
    eprintln!("render page 1 at 612 px: {ms:.1} ms");
}

fn tile(t: &PdfTiles, p: &TileParams, key: TileKey) -> Vec<u8> {
    t.tile(key, p, &|| false).expect("tile").expect("not cancelled")
}

#[test]
fn tiles_match_the_page_and_stay_crisp() {
    let doc = Arc::new(open());
    let s = doc.snapshot().unwrap();
    let layer = Rect::new(0.0, 0.0, 612.0, 792.0);
    let t = PdfTiles { layer };
    let p = TileParams { doc: doc.clone(), page: 1.0, rect: None, paper: WHITE };
    let (w, _, full) = render(&s, 0, 612.0);
    let n = TILE_SIZE;
    // level 0: tiles are the page render cut in squares
    let mut worst = 0u8;
    for (tx, ty) in [(0, 0), (1, 1), (2, 2)] {
        let tp = tile(&t, &p, TileKey { page: 0, level: 0, tx, ty });
        for y in 0..n {
            for x in 0..n {
                let (gx, gy) = (tx as u32 * n + x, ty as u32 * n + y);
                let a = px(&tp, n, x, y);
                if gx >= 612 || gy >= 792 {
                    assert_eq!(a[3], 0, "outside the page is transparent");
                    continue;
                }
                let b = px(&full, w, gx, gy);
                worst = worst.max(a.iter().zip(b.iter()).map(|(p, q)| p.abs_diff(*q)).max().unwrap_or(0));
            }
        }
    }
    assert!(worst <= 3, "tiles differ from the page render by {worst}");
    // level 3 (8 px per point): the 4 pt blue line is 32 px with sharp edges
    let key = TileKey { page: 0, level: 3, tx: 14, ty: 9 };
    let t0 = Instant::now();
    let tp = tile(&t, &p, key);
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    write_png("tile-L3-line.png", n, n, &tp);
    let col: Vec<[u8; 4]> = (0..64).map(|y| px(&tp, n, 216, y)).collect();
    assert!(near(col[14], WHITE, 0) && near(col[50], WHITE, 0), "{:?} {:?}", col[14], col[50]);
    for c in &col[17..47] {
        assert!(near(*c, [0, 0, 255, 255], 0), "{c:?}");
    }
    let blended = col.iter().filter(|c| !near(**c, WHITE, 0) && !near(**c, [0, 0, 255, 255], 0)).count();
    assert!(blended <= 2, "edges blend over {blended} px");
    // a page shown at half size inside a bigger layer, at an offset
    let big = PdfTiles { layer: Rect::new(-100.0, -100.0, 2000.0, 2000.0) };
    let half = TileParams { doc: doc.clone(), page: 1.0, rect: Some(Rect::new(0.0, 0.0, 306.0, 396.0)), paper: WHITE };
    let tp = tile(&big, &half, TileKey { page: 0, level: 0, tx: 0, ty: 0 });
    // red rect centre (450, 167) -> world (225, 83.5) -> tile px (325, 183.5): outside tile 0; (rect at x 200..250 world) -> px 300..350
    let tp2 = tile(&big, &half, TileKey { page: 0, level: 0, tx: 1, ty: 0 });
    assert!(near(px(&tp2, n, 325 - 256, 183), [255, 0, 0, 255], 2), "{:?}", px(&tp2, n, 69, 183));
    assert_eq!(px(&tp, n, 50, 50)[3], 0, "left of the page rect is transparent");
    // transparent paper: blank page areas stay transparent, ink does not
    let clear = TileParams { doc: doc.clone(), page: 1.0, rect: None, paper: [0, 0, 0, 0] };
    let tc = tile(&t, &clear, TileKey { page: 0, level: 0, tx: 1, ty: 0 });
    assert_eq!(px(&tc, n, 10, 10)[3], 0);
    assert!(near(px(&tc, n, 450 - 256, 167), [255, 0, 0, 255], 2));
    eprintln!("tile L3: {ms:.1} ms");
}

fn opts(pairs: &[(&str, Data)]) -> Data {
    Data::map(pairs.iter().map(|(k, v)| (*k, v.clone())))
}

#[test]
fn edits_render_extract_and_survive_saving() {
    let doc = open();
    let cjk = "\u{4E2D}\u{6587}\u{7DE8}\u{8F2F}";
    let image = ImageSrc::Rgba { w: 2, h: 1, rgba: vec![255, 0, 0, 255, 0, 0, 128, 128] };
    let (_, rev) = doc
        .edit(|d, cx| {
            let pid = edit::page_id(d, 1.0)?;
            let t = TextOpts::from_data(&opts(&[("x", Data::Num(100.0)), ("y", Data::Num(700.0)), ("text", Data::str("Edited text")), ("size", Data::Num(14.0))]))?;
            edit::add_text(d, cx, pid, &t)?;
            let t = TextOpts::from_data(&opts(&[("x", Data::Num(300.0)), ("y", Data::Num(700.0)), ("text", Data::str(cjk)), ("size", Data::Num(20.0)), ("color", Data::Num(0x0000FFFFu32 as f64))]))?;
            edit::add_text(d, cx, pid, &t)?;
            let path = Data::list(vec![Data::str("M"), Data::Num(400.0), Data::Num(740.0), Data::str("L"), Data::Num(480.0), Data::Num(740.0), Data::Num(440.0), Data::Num(780.0), Data::str("Z")]);
            let shapes = edit::shapes(&path, &opts(&[("fill", Data::Num(0x00C000FFu32 as f64))]))?;
            edit::add_path(d, cx, pid, &shapes)?;
            edit::add_image(d, cx, pid, image, [500.0, 700.0, 80.0, 40.0])?;
            edit::add_note(d, pid, &NoteOpts { x: 20.0, y: 20.0, text: "Check this".into(), title: Some("Ann".into()), color: 0xFFD84AFFu32 as f64 })?;
            // on the rotated page, page space still means what is displayed
            let p3 = edit::page_id(d, 3.0)?;
            let t = TextOpts::from_data(&opts(&[("x", Data::Num(100.0)), ("y", Data::Num(300.0)), ("text", Data::str("Upright")), ("size", Data::Num(16.0))]))?;
            edit::add_text(d, cx, p3, &t)
        })
        .unwrap();
    assert_eq!(rev, 1);
    let check = |doc: &PdfDoc, label: &str| {
        let s = doc.snapshot().unwrap();
        let ws = words(&s, 0);
        let e = word(&ws, "Edited");
        assert!(close(e.rect[0], 100.0, 0.01) && close(e.rect[1], 700.0, 0.01) && close(e.rect[3], 14.0, 0.01), "{label}: {e:?}");
        let c = word(&ws, cjk);
        assert!(close(c.rect[0], 300.0, 0.01) && close(c.rect[2], 80.0, 0.01), "{label}: {c:?}");
        let w3 = words(&s, 2);
        let u = word(&w3, "Upright");
        assert!(close(u.rect[0], 100.0, 0.01) && close(u.rect[1], 300.0, 0.01) && close(u.rect[2], width("Helvetica", "Upright", 16.0), 0.01), "{label}: {u:?}");
        assert!(u.rect[2] > u.rect[3], "{label}: written upright on the rotated page");
        let (w, h, img) = render(&s, 0, 612.0);
        write_png(&format!("edited-{label}.png"), w, h, &img);
        assert!(near(px(&img, w, 440, 765), [0, 192, 0, 255], 2), "{label}: triangle {:?}", px(&img, w, 440, 765));
        assert!(near(px(&img, w, 520, 720), [255, 0, 0, 255], 2), "{label}: image left {:?}", px(&img, w, 520, 720));
        // half-transparent blue over white
        let r = px(&img, w, 560, 720);
        assert!(r[0].abs_diff(127) < 8 && r[1].abs_diff(127) < 8 && r[2] > 245, "{label}: image right {r:?}");
        assert!(dark_share(&img, w, 100, 700, 170, 714, 160) > 0.08, "{label}: Latin text ink");
        if system_cjk_font() {
            let blue = (700..720).flat_map(|y| (300..380).map(move |x| (x, y))).filter(|&(x, y)| {
                let p = px(&img, w, x, y);
                p[2] > 150 && p[0] < 100
            });
            assert!(blue.count() > 80, "{label}: CJK text ink");
        }
        assert!(dark_share(&img, w, 20, 20, 40, 40, 200) > 0.05, "{label}: note icon");
        let annots = doc.with_lo(|d| Ok(edit::annotations(d, edit::page_id(d, 1.0)?))).unwrap();
        assert!(annots.iter().any(|a| a.get("type").as_str() == Some("Text") && a.get("contents").as_str() == Some("Check this")), "{label}");
        let (w3, h3, img3) = render(&s, 2, 792.0);
        write_png(&format!("edited-rotated-{label}.png"), w3, h3, &img3);
        assert!(dark_share(&img3, w3, 100, 300, 160, 316, 160) > 0.05, "{label}: text on the rotated page");
    };
    check(&doc, "live");
    let bytes = doc.save().unwrap();
    let again = PdfDoc::open(bytes, "").unwrap();
    check(&again, "reopened");
}

#[test]
fn highlights_keep_text_readable() {
    let doc = open();
    let s = doc.snapshot().unwrap();
    let mut hits = Vec::new();
    s.text(0).unwrap().search(1, "quick brown", false, 10, &mut hits);
    let r = hits[0].rects[0];
    doc.edit(|d, _| edit::highlight(d, edit::page_id(d, 1.0)?, &[[r[0] as f64, r[1] as f64, r[2] as f64, r[3] as f64]], 0xFFE100FFu32 as f64, Some("important"))).unwrap();
    let s = doc.snapshot().unwrap();
    let (w, h, img) = render(&s, 0, 612.0);
    write_png("highlight.png", w, h, &img);
    // background inside the highlight is yellow; glyphs stay dark (multiply)
    let (x0, y0, x1, y1) = (r[0] as u32, r[1] as u32 + 1, (r[0] + r[2]) as u32, (r[1] + r[3]) as u32 - 1);
    let mut yellow = 0;
    for y in y0..y1 {
        for x in x0..x1 {
            let p = px(&img, w, x, y);
            if p[0] > 240 && p[1] > 210 && p[2] < 40 {
                yellow += 1;
            }
        }
    }
    assert!(yellow as f64 > 0.3 * ((x1 - x0) * (y1 - y0)) as f64, "yellow {yellow}");
    assert!(dark_share(&img, w, x0, y0, x1, y1, 90) > 0.05, "text still dark under the highlight");
    let annots = doc.with_lo(|d| Ok(edit::annotations(d, edit::page_id(d, 1.0)?))).unwrap();
    let hl = annots.iter().find(|a| a.get("type").as_str() == Some("Highlight")).expect("highlight annotation");
    assert_eq!(hl.get("contents").as_str(), Some("important"));
    let rr: Vec<f64> = hl.get("rect").items().iter().filter_map(Data::num).collect();
    assert!((rr[0] - (r[0] as f64 - 1.0)).abs() < 0.05 && (rr[2] - (r[2] as f64 + 2.0)).abs() < 0.05, "{rr:?} vs {r:?}");
}

#[test]
fn forms_fill_and_reopen() {
    let doc = open();
    let values = vec![
        ("name".to_string(), Data::str("Grace Hopper")),
        ("agree".to_string(), Data::Bool(true)),
        ("color".to_string(), Data::str("blue")),
        ("size".to_string(), Data::str("L")),
        ("address.city".to_string(), Data::str("\u{53F0}\u{5317}")),
    ];
    doc.edit(|d, cx| edit::fill_form(d, cx, &values).map(|_| ())).unwrap();
    let reopened = PdfDoc::open(doc.save().unwrap(), "").unwrap();
    for d in [&doc, &reopened] {
        let f = d.with_lo(|lo| Ok(edit::fields(lo))).unwrap();
        let v: Vec<Data> = f.iter().map(|x| x.value.clone()).collect();
        assert_eq!(v, [Data::str("Grace Hopper"), Data::Bool(true), Data::str("blue"), Data::str("L"), Data::str("\u{53F0}\u{5317}")]);
        let s = d.snapshot().unwrap();
        let ws = words(&s, 3);
        assert!(ws.iter().any(|w| w.text == "Grace") && ws.iter().any(|w| w.text == "Hopper"), "new appearance is on the page");
        assert!(!ws.iter().any(|w| w.text == "Ada"), "old appearance replaced");
        assert!(ws.iter().any(|w| w.text == "\u{53F0}\u{5317}"));
        let (w, h, img) = render(&s, 3, 612.0);
        write_png("form-filled.png", w, h, &img);
        // the check box's on appearance: user (152..162, 662..672) -> page (152..162, 120..130)
        assert!(near(px(&img, w, 157, 125), [0, 0, 0, 255], 2), "{:?}", px(&img, w, 157, 125));
        // radio: blue on (200..214, 620..634) -> (207, 165); red off (157, 165)
        assert!(near(px(&img, w, 207, 165), [0, 0, 0, 255], 2));
        assert!(!near(px(&img, w, 157, 165), [0, 0, 0, 255], 60));
    }
    let err = doc.edit(|d, cx| edit::fill_form(d, cx, &[("nope".to_string(), Data::str("x"))]).map(|_| ())).unwrap_err();
    assert!(err.contains("no form field \"nope\"") && err.contains("address.city"), "{err}");
    let err = doc.edit(|d, cx| edit::fill_form(d, cx, &[("color".to_string(), Data::str("green"))]).map(|_| ())).unwrap_err();
    assert!(err.contains("blue"), "{err}");
}

#[test]
fn page_operations_and_merge() {
    let doc = open();
    doc.edit(|d, _| edit::insert_page(d, 2.0, None)).unwrap();
    let s = doc.snapshot().unwrap();
    assert_eq!(s.page_count(), 5);
    assert!(words(&s, 1).is_empty(), "the new page is blank");
    assert_eq!(s.geoms[1].size(), (612.0, 792.0));
    doc.edit(|d, _| edit::insert_page(d, 6.0, Some((300.0, 200.0)))).unwrap();
    assert_eq!(doc.snapshot().unwrap().geoms[5].size(), (300.0, 200.0));
    doc.edit(|d, _| edit::delete_page(d, 2.0)).unwrap();
    doc.edit(|d, _| edit::delete_page(d, 5.0)).unwrap();
    let s = doc.snapshot().unwrap();
    assert_eq!(s.page_count(), 4);
    assert!(words(&s, 1).iter().any(|w| w.text == "PDF\u{6587}\u{4EF6}"), "page 2 is the CJK page again");
    doc.edit(|d, _| edit::rotate_page(d, 1.0, 90.0)).unwrap();
    let s = doc.snapshot().unwrap();
    assert_eq!(s.geoms[0].size(), (792.0, 612.0));
    let w0 = words(&s, 0);
    let q = word(&w0, "Quarterly");
    assert!(q.rect[3] > q.rect[2], "rotated with its page: {q:?}");
    assert!(doc.edit(|d, _| edit::rotate_page(d, 1.0, 45.0)).is_err());
    // merge another document: pages and form fields come along
    let other = open();
    let src = other.with_lo(|d| Ok(d.clone())).unwrap();
    doc.edit(|d, _| edit::merge(d, &src).map(|_| ())).unwrap();
    let s = doc.snapshot().unwrap();
    assert_eq!(s.page_count(), 8);
    assert!(words(&s, 4).iter().any(|w| w.text == "Quarterly"), "inherited resources made explicit");
    assert_eq!(s.geoms[6].size(), (792.0, 612.0), "inherited rotation kept");
    assert_eq!(doc.with_lo(|d| Ok(edit::fields(d).len())).unwrap(), 10);
    // copy pages into a new document, in any order
    let copy = doc.with_lo(|d| edit::copy_pages(d, &[3.0, 2.0, 2.0])).unwrap();
    let copy = PdfDoc::from_document(copy);
    let s = copy.snapshot().unwrap();
    assert_eq!(s.page_count(), 3);
    assert_eq!(s.geoms[0].size(), (792.0, 612.0));
    assert!(words(&s, 1).iter().any(|w| w.text == "PDF\u{6587}\u{4EF6}") && words(&s, 2).iter().any(|w| w.text == "PDF\u{6587}\u{4EF6}"));
    // the only page cannot go
    let one = PdfDoc::from_document(anypage_pdf::doc::new_document(100.0, 100.0, 1));
    assert!(one.edit(|d, _| edit::delete_page(d, 1.0)).unwrap_err().contains("only page"));
}

#[test]
fn bad_input_is_an_error_not_a_panic() {
    assert!(PdfDoc::open(Arc::new(b"%PDF-1.7\ngarbage".to_vec()), "").is_err());
    assert!(PdfDoc::open(Arc::new(Vec::new()), "").err().unwrap_or_default().contains("empty"));
    let mut cut = fixture().as_ref().clone();
    cut.truncate(cut.len() / 2);
    // a truncated file either opens with what can be recovered or fails cleanly
    if let Ok(d) = PdfDoc::open(Arc::new(cut), "") {
        let s = d.snapshot().unwrap();
        for i in 0..s.page_count() {
            let _ = s.text(i);
        }
    }
    let doc = open();
    let s = doc.snapshot().unwrap();
    assert!(s.index(0.0).is_err() && s.index(5.0).is_err() && s.index(1.5).is_err());
    assert!(doc.edit(|d, cx| edit::add_text(d, cx, edit::page_id(d, 9.0)?, &TextOpts::from_data(&opts(&[("text", Data::str("x"))]))?)).is_err());
    assert!(TextOpts::from_data(&opts(&[("text", Data::str("x")), ("size", Data::Num(1e9))])).is_err());
    assert!(TextOpts::from_data(&opts(&[("text", Data::str("x")), ("font", Data::str("Wingdings"))])).is_err());
    assert!(render_page(&s, 0, 1e9, WHITE, &|| false, &|_| {}).err().unwrap_or_default().contains("too large"));
    assert!(render_page(&s, 0, 2000.0, WHITE, &|| true, &|_| {}).unwrap().is_none(), "cancelled");
}

#[test]
fn timings() {
    let bytes = text_heavy(200, 60);
    let t0 = Instant::now();
    let doc = PdfDoc::open(bytes.clone(), "").unwrap();
    let open_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let s = doc.snapshot().unwrap();
    assert_eq!(s.page_count(), 200);
    let t1 = Instant::now();
    let first = s.text(0).unwrap();
    let page_ms = t1.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(first.lines.len(), 60);
    // where the time goes: the interpreter, then our layout
    let tc = Instant::now();
    let glyphs = anypage_pdf::text::collect(&s, 1).unwrap();
    let collect_ms = tc.elapsed().as_secs_f64() * 1000.0;
    let n_glyphs = glyphs.len();
    let tl = Instant::now();
    let _ = anypage_pdf::text::layout(glyphs, 612.0, 792.0);
    let layout_ms = tl.elapsed().as_secs_f64() * 1000.0;
    eprintln!("page 2: {n_glyphs} glyphs, interpret {collect_ms:.1} ms, layout {layout_ms:.1} ms");
    let t2 = Instant::now();
    let mut hits = Vec::new();
    let pages: Vec<usize> = (0..s.page_count()).collect();
    s.text_each(&pages, &|| false, |i, t| {
        t.search(i + 1, "lazy dog, again", false, 100_000, &mut hits);
        Ok(true)
    })
    .unwrap();
    let search_ms = t2.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(hits.len(), 200 * 60);
    let t = PdfTiles { layer: Rect::new(0.0, 0.0, 612.0, 792.0) };
    let p = TileParams { doc: Arc::new(doc), page: 1.0, rect: None, paper: WHITE };
    let mut tile_ms = Vec::new();
    for (level, tx, ty) in [(0, 0, 0), (0, 1, 1), (1, 1, 1), (2, 3, 3), (3, 5, 5)] {
        let t3 = Instant::now();
        let _ = t.tile(TileKey { page: 0, level, tx, ty }, &p, &|| false).unwrap();
        tile_ms.push(format!("L{level} {:.1} ms", t3.elapsed().as_secs_f64() * 1000.0));
    }
    let t4 = Instant::now();
    let _ = render_page(&s, 0, 1224.0, WHITE, &|| false, &|_| {}).unwrap();
    let render_ms = t4.elapsed().as_secs_f64() * 1000.0;
    let report = format!(
        "200-page text PDF ({} KB): open {open_ms:.1} ms, text page 1 {page_ms:.1} ms, text+search all pages {search_ms:.0} ms, tiles {}, page at 1224 px {render_ms:.1} ms",
        bytes.len() >> 10,
        tile_ms.join(", ")
    );
    eprintln!("{report}");
    let _ = std::fs::write(out_dir().join("timings.txt"), report);
}

#[test]
fn scattered_glyph_pages_stay_fast_bounded_and_cancellable() {
    let t0 = Instant::now();
    let bytes = scattered_glyphs(12, 50_000);
    let gen_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let doc = PdfDoc::open(bytes.clone(), "").unwrap();
    let s = doc.snapshot().unwrap();
    // one page: interpretation and layout separately
    let t1 = Instant::now();
    let glyphs = anypage_pdf::text::collect(&s, 0).unwrap();
    let interp_ms = t1.elapsed().as_secs_f64() * 1000.0;
    let n = glyphs.len();
    assert!(n > 45_000, "{n} glyphs");
    let t2 = Instant::now();
    let pt = anypage_pdf::text::layout(glyphs, 612.0, 792.0);
    let layout_ms = t2.elapsed().as_secs_f64() * 1000.0;
    assert!(layout_ms < 3000.0, "layout of {n} scattered glyphs took {layout_ms:.0} ms");
    assert_eq!(pt.blocks.iter().map(|b| b.lines.len()).sum::<usize>(), pt.lines.len());
    let bytes_per_page = pt.approx_bytes();
    // every page through the cache: it stays within its byte budget
    let t3 = Instant::now();
    for i in 0..s.page_count() {
        let t = s.text(i).unwrap();
        assert_eq!(t.glyphs.len(), n);
        assert!(s.text_cache_bytes() <= anypage_pdf::doc::TEXT_CACHE_BYTES, "{} bytes cached", s.text_cache_bytes());
    }
    let all_ms = t3.elapsed().as_secs_f64() * 1000.0;
    assert!(s.text_cache_bytes() > 0);
    // a cancelled extraction stops and is not cached
    let fresh = PdfDoc::open(bytes, "").unwrap().snapshot().unwrap();
    let asked = std::cell::Cell::new(0);
    let cancel = || {
        asked.set(asked.get() + 1);
        true
    };
    assert_eq!(fresh.text_with(0, &cancel).err().as_deref(), Some(anypage_pdf::text::CANCELLED));
    assert!(asked.get() >= 1);
    assert_eq!(fresh.text_cache_bytes(), 0);
    assert_eq!(fresh.text_each(&[0, 1], &|| true, |_, _| Ok(true)).err().as_deref(), Some(anypage_pdf::text::CANCELLED));
    let report = format!(
        "scattered glyphs: {n} per page, file generated in {gen_ms:.0} ms; page 1 interpret {interp_ms:.0} ms, layout {layout_ms:.0} ms ({} lines, {} blocks, ~{} KB of text data); all 12 pages {all_ms:.0} ms, cache {} KB",
        pt.lines.len(),
        pt.blocks.len(),
        bytes_per_page >> 10,
        s.text_cache_bytes() >> 10
    );
    eprintln!("{report}");
    let _ = std::fs::write(out_dir().join("timings-scattered.txt"), report);
}

/// Opt-in: one page at the glyph limit, scattered (the worst case for layout
/// and memory): `cargo test --test pdf max_glyph_page -- --ignored --nocapture`.
#[test]
#[ignore]
fn max_glyph_page() {
    let n = anypage_pdf::text::MAX_GLYPHS + 10_000;
    let doc = PdfDoc::open(scattered_glyphs(1, n), "").unwrap();
    let s = doc.snapshot().unwrap();
    let t0 = Instant::now();
    let g = anypage_pdf::text::collect(&s, 0).unwrap();
    let interp = t0.elapsed().as_secs_f64() * 1000.0;
    let count = g.len();
    let t1 = Instant::now();
    let pt = anypage_pdf::text::layout(g, 612.0, 792.0);
    let layout = t1.elapsed().as_secs_f64() * 1000.0;
    let mut hits = Vec::new();
    let t2 = Instant::now();
    pt.search(1, "abcabc", false, 1000, &mut hits);
    let search = t2.elapsed().as_secs_f64() * 1000.0;
    eprintln!(
        "{count} glyphs kept of {n}: interpret {interp:.0} ms, layout {layout:.0} ms, search {search:.0} ms, {} lines, {} blocks, ~{} MB of text data",
        pt.lines.len(),
        pt.blocks.len(),
        pt.approx_bytes() >> 20
    );
    assert_eq!(count, anypage_pdf::text::MAX_GLYPHS);
}

#[test]
fn decompression_bombs_are_not_opened() {
    use std::io::Write;
    // a content stream of 300 MB of spaces, about 300 KB compressed
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    let chunk = vec![b' '; 1 << 20];
    for _ in 0..300 {
        z.write_all(&chunk).unwrap();
    }
    let bomb = z.finish().unwrap();
    let mut doc = lopdf::Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let st = lopdf::Stream::new(lopdf::dictionary! { "Filter" => "FlateDecode" }, bomb).with_compression(false);
    let content = doc.add_object(st);
    let page = doc.add_object(lopdf::dictionary! { "Type" => "Page", "Parent" => pages_id, "Contents" => content, "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()] });
    doc.objects.insert(pages_id, lopdf::Object::Dictionary(lopdf::dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 }));
    let catalog = doc.add_object(lopdf::dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    let mut out = Vec::new();
    doc.save_to(&mut out).unwrap();
    let t0 = Instant::now();
    let err = PdfDoc::open(Arc::new(out), "").err().unwrap_or_default();
    assert!(err.contains("decodes to more than 256 MB") && err.contains("not opened"), "{err}");
    eprintln!("bomb refused in {:.0} ms", t0.elapsed().as_secs_f64() * 1000.0);
    // the fixture and the timing file pass, quickly
    let t1 = Instant::now();
    anypage_pdf::limits::check_streams(&fixture()).unwrap();
    anypage_pdf::limits::check_streams(&text_heavy(200, 60)).unwrap();
    assert!(t1.elapsed().as_secs_f64() < 1.0);
    // a big real-looking stream (24 MB of content, ~5 MB compressed) is decoded once to be sure
    let text: String = (0..1_200_000).map(|i| format!("{} {} Td ({i}) Tj\n", i % 600, (i * 7) % 800)).collect();
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    z.write_all(text.as_bytes()).unwrap();
    let data = z.finish().unwrap();
    let mut f = format!("%PDF-1.7\n4 0 obj\n<< /Length {} /Filter /FlateDecode >>\nstream\n", data.len()).into_bytes();
    f.extend_from_slice(&data);
    f.extend_from_slice(b"\nendstream\nendobj\n");
    let t2 = Instant::now();
    anypage_pdf::limits::check_streams(&f).unwrap();
    let ms = t2.elapsed().as_secs_f64() * 1000.0;
    eprintln!("check of a {} KB stream that decodes to {} MB: {ms:.0} ms", data.len() >> 10, text.len() >> 20);
}

#[test]
fn geometry_after_an_edit_needs_no_rebuild() {
    let doc = open();
    let before = doc.meta();
    assert_eq!(before.info.bytes, Some(fixture().len()));
    assert_eq!(before.info.title.as_deref(), Some("AnyPage Test Fixture"));
    assert_eq!(before.info.created.as_deref(), Some("2026-10-24T12:00:00+08:00"));
    doc.edit(|d, cx| {
        let pid = edit::page_id(d, 1.0)?;
        edit::add_text(d, cx, pid, &TextOpts::from_data(&opts(&[("text", Data::str("x"))]))?)?;
        edit::insert_page(d, 2.0, Some((300.0, 200.0)))
    })
    .unwrap();
    assert!(!doc.has_current_snapshot(), "an edit does not read the document back");
    let after = doc.meta();
    assert!(!doc.has_current_snapshot(), "geometry does not read it back either");
    assert_eq!(after.page_count(), 5);
    assert_eq!(after.geoms[1].size(), (300.0, 200.0));
    assert_eq!(after.geoms[0], before.geoms[0]);
    // the same info as hayro read, except the size, which is unknown until saved
    let mut expected = before.info.clone();
    expected.bytes = None;
    assert_eq!(after.info, expected);
    // tiles and text read it back, once, and agree
    let s = doc.snapshot().unwrap();
    assert!(doc.has_current_snapshot());
    assert_eq!(s.geoms, after.geoms);
    let mut reread = s.info();
    reread.bytes = None;
    assert_eq!(reread, after.info);
}

#[test]
fn edits_that_fail_change_nothing() {
    let doc = open();
    let rev = doc.revision();
    let values = vec![("name".to_string(), Data::str("x")), ("nosuch".to_string(), Data::Num(1.0))];
    let err = doc.edit(|d, cx| edit::fill_form(d, cx, &values).map(|_| ())).unwrap_err();
    assert!(err.contains("no form field \"nosuch\""), "{err}");
    let err = doc.edit(|d, cx| edit::fill_form(d, cx, &[("name".to_string(), Data::str("x")), ("color".to_string(), Data::str("green"))]).map(|_| ())).unwrap_err();
    assert!(err.contains("not a choice"), "{err}");
    assert_eq!(doc.revision(), rev, "a failed edit keeps the revision");
    assert!(doc.has_current_snapshot());
    let f = doc.with_lo(|d| Ok(edit::fields(d))).unwrap();
    assert_eq!(f[0].value, Data::str("Ada"), "the field written before the bad one is untouched");
    assert!(words(&doc.snapshot().unwrap(), 3).iter().any(|w| w.text == "Ada"));
    // bad numbers are errors, not zeros
    for (k, v) in [("x", f64::NAN), ("y", f64::INFINITY), ("x", 1e300)] {
        let e = TextOpts::from_data(&opts(&[("text", Data::str("a")), (k, Data::Num(v))])).err().unwrap_or_default();
        assert!(e.contains("finite"), "{k} = {v}: {e}");
    }
    let e = TextOpts::from_data(&opts(&[("text", Data::str("a")), ("width", Data::Num(1e9))])).err().unwrap_or_default();
    assert!(e.contains("width"), "{e}");
    assert!(TextOpts::from_data(&opts(&[("text", Data::str("a")), ("line_height", Data::Num(f64::NAN))])).is_err());
    // a huge multiple of 90 degrees is reduced, not overflowed
    doc.edit(|d, _| edit::rotate_page(d, 1.0, 90.0 * 2f64.powi(1000))).unwrap();
    doc.edit(|d, _| edit::rotate_page(d, 1.0, -270.0)).unwrap();
    assert_eq!(doc.meta().geoms[0].rotate, 90);
}

#[test]
fn deleting_a_page_leaves_no_dangling_references() {
    let doc = open();
    // page 4 holds the form; page 1 the outline's and named destination's target
    doc.edit(|d, _| edit::delete_page(d, 4.0)).unwrap();
    doc.edit(|d, _| edit::delete_page(d, 1.0)).unwrap();
    for d in [doc.with_lo(|d| Ok(d.clone())).unwrap(), lopdf::Document::load_mem(&doc.save().unwrap()).unwrap()] {
        assert_eq!(dangling_refs(&d), [], "references to removed objects");
        assert!(edit::fields(&d).is_empty(), "the form went with its page");
        assert_eq!(edit::fields_data(&d).len(), 0);
        let outline = edit::outline(&d);
        assert_eq!(outline.len(), 3);
        assert_eq!(outline[0].1, None, "\"Report\" pointed at the deleted page 1");
        assert_eq!(outline[2].1, Some(0), "\"\u{4E2D}\u{6587}\" (old page 2) is page 1 now");
    }
    let s = doc.snapshot().unwrap();
    assert_eq!(s.page_count(), 2);
    assert!(words(&s, 0).iter().any(|w| w.text == "PDF\u{6587}\u{4EF6}"));
    // two copies of the form page: deleting one takes its fields (the radio
    // group and the nested field emptied too) and keeps the other's
    let doc = open();
    let moved = doc.with_lo(|d| edit::copy_pages(d, &[4.0, 4.0])).unwrap();
    let two = PdfDoc::from_document(moved);
    two.edit(|d, _| edit::delete_page(d, 1.0)).unwrap();
    two.with_lo(|d| {
        assert_eq!(dangling_refs(d), []);
        assert_eq!(edit::fields(d).len(), 5, "the fields of the remaining copy");
        Ok(())
    })
    .unwrap();
}

#[test]
fn tiles_without_a_document_are_blank() {
    use anypage_engine::TileSource;
    let t = PdfTiles { layer: Rect::new(0.0, 0.0, 612.0, 792.0) };
    let params: anypage_core::Value = Arc::new(Data::map([("page", Data::Num(1.0))]));
    let px = t.render(TileKey { page: 0, level: 0, tx: 0, ty: 0 }, &params, &|| false).expect("a tile");
    assert_eq!(px.len(), (TILE_SIZE * TILE_SIZE * 4) as usize);
    assert!(px.iter().all(|&b| b == 0), "transparent, not the error stripes");
    // a real problem still shows the error tile
    let bad: anypage_core::Value = Arc::new(Data::map([("doc", Data::str("not a doc"))]));
    let px = t.render(TileKey { page: 0, level: 0, tx: 0, ty: 0 }, &bad, &|| false).expect("a tile");
    assert!(px.iter().any(|&b| b != 0));
}

/// Opt-in: `ANYPAGE_PDF_SAMPLES="a.pdf;b.pdf" cargo test --test pdf real_files -- --ignored --nocapture`
/// opens real files and prints only counts and timings.
#[test]
#[ignore]
fn real_files() {
    let list = std::env::var("ANYPAGE_PDF_SAMPLES").unwrap_or_default();
    for path in list.split(';').filter(|p| !p.is_empty()) {
        let Ok(bytes) = std::fs::read(path) else { continue };
        let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let t0 = Instant::now();
        let doc = match PdfDoc::open(Arc::new(bytes), "") {
            Ok(d) => d,
            Err(e) => {
                eprintln!("{name}: open failed: {e}");
                continue;
            }
        };
        let open_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let s = doc.snapshot().unwrap();
        let n = s.page_count().min(30);
        let t1 = Instant::now();
        let (mut glyphs, mut words, mut blocks, mut tables, mut cjk, mut unknown) = (0, 0, 0, 0, 0, 0);
        let pages: Vec<usize> = (0..n).collect();
        s.text_each(&pages, &|| false, |_, t| {
            glyphs += t.glyphs.len();
            unknown += t.glyphs.iter().filter(|g| g.text.contains('\u{FFFD}')).count();
            cjk += t.glyphs.iter().filter(|g| g.text.chars().any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c))).count();
            words += t.words().count();
            blocks += t.blocks.len();
            tables += t.tables.len();
            Ok(true)
        })
        .unwrap();
        let text_ms = t1.elapsed().as_secs_f64() * 1000.0;
        let t2 = Instant::now();
        let img = render_page(&s, 0, 1000.0, WHITE, &|| false, &|_| {}).unwrap().unwrap();
        let render_ms = t2.elapsed().as_secs_f64() * 1000.0;
        let ink = dark_share(&img.rgba, img.w, 0, 0, img.w, img.h, 200);
        let lo = doc.with_lo(|d| Ok((edit::outline(d).len(), edit::fields(d).len())));
        let edit_ok = doc.edit(|d, cx| {
            let pid = edit::page_id(d, 1.0)?;
            edit::add_text(d, cx, pid, &TextOpts::from_data(&opts(&[("text", Data::str("probe \u{6E2C}\u{8A66}")), ("x", Data::Num(10.0)), ("y", Data::Num(10.0))]))?)
        });
        let reread = doc.snapshot().map(|s| s.text(0).map(|t| t.words().any(|w| w.text == "probe")).unwrap_or(false));
        eprintln!(
            "{name}: {} pages, open {open_ms:.0} ms; first {n} pages: {glyphs} glyphs ({cjk} CJK, {unknown} unmapped), {words} words, {blocks} blocks, {tables} tables in {text_ms:.0} ms ({:.1} ms/page); page 1 at 1000 px {render_ms:.0} ms, ink {:.1}%; outline/fields {:?}; edit {:?}, edit re-read {:?}",
            s.page_count(),
            text_ms / n.max(1) as f64,
            ink * 100.0,
            lo,
            edit_ok.map(|_| ()),
            reread
        );
    }
}

/// Opt-in: print the start of each block of page `ANYPAGE_PDF_PAGE` of the first sample.
#[test]
#[ignore]
fn real_file_blocks() {
    let list = std::env::var("ANYPAGE_PDF_SAMPLES").unwrap_or_default();
    let Some(path) = list.split(';').next().filter(|p| !p.is_empty()) else { return };
    let page: usize = std::env::var("ANYPAGE_PDF_PAGE").ok().and_then(|p| p.parse().ok()).unwrap_or(1);
    let doc = PdfDoc::open(Arc::new(std::fs::read(path).unwrap()), "").unwrap();
    let s = doc.snapshot().unwrap();
    let t = s.text(page - 1).unwrap();
    for (k, b) in t.blocks.iter().enumerate() {
        let text: String = t.block_text(b).chars().take(50).collect();
        eprintln!("{k:3} [{:5.0} {:5.0} {:5.0} {:5.0}] {}", b.rect[0], b.rect[1], b.rect[2], b.rect[3], text.replace('\n', " / "));
    }
}
