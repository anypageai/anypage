//! Test PDFs, generated with lopdf, with every position known.
//!
//! Page 1 (612 x 792): text in Helvetica-Bold, Times (TJ spacing, kerning,
//! Tw), Courier; a table; a font with /Differences; a font with a ToUnicode
//! CMap; vector paths; a 2x2 image; invisible text; text in a form XObject;
//! rotated text.
//! Page 2: CJK through Type 0 fonts (Identity-H + ToUnicode, and the
//! predefined UniCNS-UTF16-H CMap), mixed with Latin on one line.
//! Page 3: /Rotate 90, text and a black square.
//! Page 4: an AcroForm: text, check box, radio group, combo box, nested field.
//! Outline with a named destination; Info with a CJK author.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use lopdf::{dictionary, Dictionary, Document, Object, ObjectId, Stream, StringFormat};

pub fn lit(s: &str) -> Object {
    Object::String(s.as_bytes().to_vec(), StringFormat::Literal)
}

pub fn utf16(s: &str) -> Object {
    let mut v = vec![0xFE, 0xFF];
    for u in s.encode_utf16() {
        v.extend_from_slice(&u.to_be_bytes());
    }
    Object::String(v, StringFormat::Hexadecimal)
}

fn stream(doc: &mut Document, dict: Dictionary, content: &str) -> ObjectId {
    doc.add_object(Stream::new(dict, content.as_bytes().to_vec()))
}

fn rect(a: f64, b: f64, c: f64, d: f64) -> Object {
    Object::Array(vec![Object::Real(a as f32), Object::Real(b as f32), Object::Real(c as f32), Object::Real(d as f32)])
}

const CMAP_HEAD: &str = "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n/CMapName /Test def\n/CMapType 2 def\n";
const CMAP_TAIL: &str = "endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n";

pub const PAGE1: &str = "\
BT /F1 24 Tf 72 720 Td (Quarterly Report) Tj ET
BT /F2 12 Tf 72 680 Td [(The)-250(quick)-250(brown)-250(fox)-250(W)80(ater)] TJ ET
BT /F2 12 Tf 2 Tw 72 662 Td (jumps over the lazy dog) Tj 0 Tw ET
BT /F3 12 Tf 72 640 Td (Courier 12345) Tj ET
BT /F4 10 Tf 72 600 Td (Item) Tj ET BT /F4 10 Tf 200 600 Td (Qty) Tj ET BT /F4 10 Tf 320 600 Td (Price) Tj ET
BT /F4 10 Tf 72 585 Td (Apples) Tj ET BT /F4 10 Tf 200 585 Td (3) Tj ET BT /F4 10 Tf 320 585 Td (1,200.50) Tj ET
BT /F4 10 Tf 72 570 Td (Pears) Tj ET BT /F4 10 Tf 200 570 Td (12) Tj ET BT /F4 10 Tf 320 570 Td (7.25) Tj ET
BT /F4 10 Tf 72 555 Td (Plums) Tj ET BT /F4 10 Tf 200 555 Td (5) Tj ET BT /F4 10 Tf 320 555 Td (\\(3\\)) Tj ET
BT /F5 12 Tf 72 500 Td (AB) Tj ET
BT /F6 12 Tf 72 480 Td (AB) Tj ET
1 0 0 rg 400 600 100 50 re f
0 0 1 RG 4 w 400 500 m 550 500 l S
0 0.6 0 rg 505 400 m 505 416.57 491.57 430 475 430 c 458.43 430 445 416.57 445 400 c 445 383.43 458.43 370 475 370 c 491.57 370 505 383.43 505 400 c f
q 100 0 0 100 72 300 cm /Im1 Do Q
BT 3 Tr /F4 10 Tf 72 250 Td (invisible OCR layer) Tj 0 Tr ET
q /Fm1 Do Q
BT /F1 12 Tf 0 1 -1 0 560 100 Tm (Vertical) Tj ET
";

pub const PAGE2: &str = "\
BT /F7 16 Tf 72 700 Td <0001000200030004> Tj ET
BT /F8 16 Tf 72 650 Td <8868683C> Tj ET
BT /F1 16 Tf 72 600 Td (PDF) Tj /F8 16 Tf <65874EF6> Tj ET
BT /F4 12 Tf 72 550 Td (Second page about quarterly numbers) Tj ET
";

pub const PAGE3: &str = "\
BT /F4 14 Tf 72 720 Td (Rotated page) Tj ET
0 g 500 100 50 50 re f
";

pub const PAGE4: &str = "\
BT /F4 12 Tf 72 705 Td (Name:) Tj ET
BT /F4 12 Tf 72 663 Td (Agree:) Tj ET
BT /F4 12 Tf 72 623 Td (Colour:) Tj ET
BT /F4 12 Tf 72 584 Td (Size:) Tj ET
BT /F4 12 Tf 72 544 Td (City:) Tj ET
";

/// The fixture as a lopdf document.
pub fn fixture_doc() -> Document {
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let ids: Vec<ObjectId> = (0..4).map(|_| doc.new_object_id()).collect();

    // fonts
    let helv_b = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica-Bold", "Encoding" => "WinAnsiEncoding" });
    let times = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Times-Roman", "Encoding" => "WinAnsiEncoding" });
    let courier = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Courier", "Encoding" => "WinAnsiEncoding" });
    let helv = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica", "Encoding" => "WinAnsiEncoding" });
    let diff = doc.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
        "Encoding" => dictionary! { "Type" => "Encoding", "BaseEncoding" => "WinAnsiEncoding", "Differences" => vec![65.into(), "Eacute".into(), "bullet".into()] },
    });
    let tu1 = stream(&mut doc, Dictionary::new(), &format!("{CMAP_HEAD}1 begincodespacerange\n<00> <FF>\nendcodespacerange\n2 beginbfchar\n<41> <03A9>\n<42> <00660069>\nendbfchar\n{CMAP_TAIL}"));
    let tounicode = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica", "Encoding" => "WinAnsiEncoding", "ToUnicode" => tu1 });
    // CJK: Identity-H, not embedded, ToUnicode
    let fd7 = doc.add_object(dictionary! { "Type" => "FontDescriptor", "FontName" => "MingLiU", "Flags" => 4, "FontBBox" => rect(0.0, -120.0, 1000.0, 880.0), "ItalicAngle" => 0, "Ascent" => 880, "Descent" => -120, "CapHeight" => 700, "StemV" => 80 });
    let cid7 = doc.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "CIDFontType2", "BaseFont" => "MingLiU",
        "CIDSystemInfo" => dictionary! { "Registry" => lit("Adobe"), "Ordering" => lit("Identity"), "Supplement" => 0 },
        "FontDescriptor" => fd7, "DW" => 1000, "CIDToGIDMap" => "Identity",
    });
    let tu7 = stream(&mut doc, Dictionary::new(), &format!("{CMAP_HEAD}1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n4 beginbfchar\n<0001> <4E2D>\n<0002> <6587>\n<0003> <6E2C>\n<0004> <8A66>\nendbfchar\n{CMAP_TAIL}"));
    let type0_identity = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type0", "BaseFont" => "MingLiU", "Encoding" => "Identity-H", "DescendantFonts" => vec![cid7.into()], "ToUnicode" => tu7 });
    // CJK: predefined UniCNS-UTF16-H CMap, no ToUnicode
    let fd8 = doc.add_object(dictionary! { "Type" => "FontDescriptor", "FontName" => "MSung-Light", "Flags" => 6, "FontBBox" => rect(-160.0, -250.0, 1015.0, 1070.0), "ItalicAngle" => 0, "Ascent" => 880, "Descent" => -120, "CapHeight" => 880, "StemV" => 93 });
    let cid8 = doc.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "CIDFontType0", "BaseFont" => "MSung-Light",
        "CIDSystemInfo" => dictionary! { "Registry" => lit("Adobe"), "Ordering" => lit("CNS1"), "Supplement" => 4 },
        "FontDescriptor" => fd8, "DW" => 1000,
    });
    let type0_uni = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type0", "BaseFont" => "MSung-Light", "Encoding" => "UniCNS-UTF16-H", "DescendantFonts" => vec![cid8.into()] });

    // image: red, green / blue, white
    let img = doc.add_object(Stream::new(
        dictionary! { "Type" => "XObject", "Subtype" => "Image", "Width" => 2, "Height" => 2, "ColorSpace" => "DeviceRGB", "BitsPerComponent" => 8 },
        vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255],
    ));
    // form XObject with text, moved by its matrix
    let form = stream(
        &mut doc,
        dictionary! {
            "Type" => "XObject", "Subtype" => "Form", "BBox" => rect(0.0, -10.0, 200.0, 30.0),
            "Matrix" => vec![1.into(), 0.into(), 0.into(), 1.into(), 300.into(), 250.into()],
            "Resources" => dictionary! { "Font" => dictionary! { "F1" => helv_b } },
        },
        "BT /F1 12 Tf 0 0 Td (Inside form) Tj ET",
    );

    let fonts = dictionary! { "F1" => helv_b, "F2" => times, "F3" => courier, "F4" => helv, "F5" => diff, "F6" => tounicode, "F7" => type0_identity, "F8" => type0_uni };
    let resources = doc.add_object(dictionary! { "Font" => fonts, "XObject" => dictionary! { "Im1" => img, "Fm1" => form } });

    let contents: Vec<ObjectId> = [PAGE1, PAGE2, PAGE3, PAGE4].iter().map(|c| stream(&mut doc, Dictionary::new(), c)).collect();
    for (k, id) in ids.iter().enumerate() {
        let mut p = dictionary! { "Type" => "Page", "Parent" => pages_id, "Contents" => contents[k] };
        if k == 2 {
            p.set("Rotate", 90);
        }
        doc.objects.insert(*id, Object::Dictionary(p));
    }

    // ---- form on page 4
    let p4 = ids[3];
    let helv_dr = helv;
    let ap_name = stream(
        &mut doc,
        dictionary! { "Type" => "XObject", "Subtype" => "Form", "BBox" => rect(0.0, 0.0, 250.0, 20.0), "Resources" => dictionary! { "Font" => dictionary! { "Helv" => helv_dr } } },
        "/Tx BMC BT /Helv 12 Tf 0 g 2 5 Td (Ada) Tj ET EMC",
    );
    let name_field = doc.add_object(dictionary! {
        "FT" => "Tx", "T" => lit("name"), "V" => lit("Ada"), "DA" => lit("/Helv 12 Tf 0 g"),
        "Type" => "Annot", "Subtype" => "Widget", "Rect" => rect(150.0, 700.0, 400.0, 720.0), "P" => p4, "F" => 4,
        "AP" => dictionary! { "N" => ap_name },
    });
    let check_on = stream(&mut doc, dictionary! { "Type" => "XObject", "Subtype" => "Form", "BBox" => rect(0.0, 0.0, 14.0, 14.0) }, "0 g 2 2 10 10 re f");
    let check_off = stream(&mut doc, dictionary! { "Type" => "XObject", "Subtype" => "Form", "BBox" => rect(0.0, 0.0, 14.0, 14.0) }, "0.5 G 0.5 0.5 13 13 re S");
    let agree = doc.add_object(dictionary! {
        "FT" => "Btn", "T" => lit("agree"), "V" => "Off", "AS" => "Off",
        "Type" => "Annot", "Subtype" => "Widget", "Rect" => rect(150.0, 660.0, 164.0, 674.0), "P" => p4, "F" => 4,
        "AP" => dictionary! { "N" => dictionary! { "Yes" => check_on, "Off" => check_off } },
    });
    let color_id = doc.new_object_id();
    let red_w = doc.add_object(dictionary! {
        "Type" => "Annot", "Subtype" => "Widget", "Parent" => color_id, "Rect" => rect(150.0, 620.0, 164.0, 634.0), "P" => p4, "F" => 4, "AS" => "red",
        "AP" => dictionary! { "N" => dictionary! { "red" => check_on, "Off" => check_off } },
    });
    let blue_w = doc.add_object(dictionary! {
        "Type" => "Annot", "Subtype" => "Widget", "Parent" => color_id, "Rect" => rect(200.0, 620.0, 214.0, 634.0), "P" => p4, "F" => 4, "AS" => "Off",
        "AP" => dictionary! { "N" => dictionary! { "blue" => check_on, "Off" => check_off } },
    });
    doc.objects.insert(color_id, Object::Dictionary(dictionary! { "FT" => "Btn", "Ff" => 49152, "T" => lit("color"), "V" => "red", "Kids" => vec![red_w.into(), blue_w.into()] }));
    let size = doc.add_object(dictionary! {
        "FT" => "Ch", "Ff" => 131072, "T" => lit("size"), "V" => lit("M"), "Opt" => vec![lit("S"), lit("M"), lit("L")], "DA" => lit("/Helv 10 Tf 0 g"),
        "Type" => "Annot", "Subtype" => "Widget", "Rect" => rect(150.0, 580.0, 250.0, 596.0), "P" => p4, "F" => 4,
    });
    let address = doc.new_object_id();
    let city = doc.add_object(dictionary! {
        "FT" => "Tx", "T" => lit("city"), "Parent" => address, "V" => lit(""), "DA" => lit("/Helv 10 Tf 0 g"),
        "Type" => "Annot", "Subtype" => "Widget", "Rect" => rect(150.0, 540.0, 350.0, 556.0), "P" => p4, "F" => 4,
    });
    doc.objects.insert(address, Object::Dictionary(dictionary! { "T" => lit("address"), "Kids" => vec![city.into()] }));
    if let Ok(p) = doc.get_dictionary_mut(p4) {
        p.set("Annots", vec![name_field.into(), agree.into(), red_w.into(), blue_w.into(), size.into(), city.into()]);
    }

    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => ids.iter().map(|i| Object::Reference(*i)).collect::<Vec<_>>(), "Count" => 4,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()], "Resources" => resources,
        }),
    );

    // ---- outline and named destination
    let outlines = doc.new_object_id();
    let o1 = doc.new_object_id();
    let o2 = doc.new_object_id();
    let o3 = doc.new_object_id();
    doc.objects.insert(outlines, Object::Dictionary(dictionary! { "Type" => "Outlines", "First" => o1, "Last" => o3, "Count" => 3 }));
    doc.objects.insert(o1, Object::Dictionary(dictionary! { "Title" => lit("Report"), "Parent" => outlines, "Dest" => vec![ids[0].into(), "XYZ".into(), 0.into(), 792.into(), 0.into()], "First" => o2, "Last" => o2, "Next" => o3, "Count" => 1 }));
    doc.objects.insert(o2, Object::Dictionary(dictionary! { "Title" => lit("Table"), "Parent" => o1, "Dest" => lit("tbl") }));
    doc.objects.insert(o3, Object::Dictionary(dictionary! { "Title" => utf16("\u{4E2D}\u{6587}"), "Parent" => outlines, "Prev" => o1, "A" => dictionary! { "S" => "GoTo", "D" => vec![ids[1].into(), "Fit".into()] } }));

    let acroform = dictionary! {
        "Fields" => vec![name_field.into(), agree.into(), color_id.into(), size.into(), address.into()],
        "DR" => dictionary! { "Font" => dictionary! { "Helv" => helv_dr } }, "DA" => lit("/Helv 0 Tf 0 g"),
    };
    let catalog = doc.add_object(dictionary! {
        "Type" => "Catalog", "Pages" => pages_id, "Outlines" => outlines, "AcroForm" => acroform,
        "Names" => dictionary! { "Dests" => dictionary! { "Names" => vec![lit("tbl"), vec![ids[0].into(), "XYZ".into(), 72.into(), 600.into(), 0.into()].into()] } },
    });
    doc.trailer.set("Root", catalog);
    let info = doc.add_object(dictionary! { "Title" => lit("AnyPage Test Fixture"), "Author" => utf16("Tester \u{6E2C}\u{8A66}"), "CreationDate" => lit("D:20261024120000+08'00'") });
    doc.trailer.set("Info", info);
    doc
}

pub fn fixture() -> Arc<Vec<u8>> {
    let mut doc = fixture_doc();
    let mut out = Vec::new();
    doc.save_to(&mut out).expect("write fixture");
    Arc::new(out)
}

/// `pages` pages of `lines` lines of running text each, for timings.
pub fn text_heavy(pages: usize, lines: usize) -> Arc<Vec<u8>> {
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let helv = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica", "Encoding" => "WinAnsiEncoding" });
    let mut kids = Vec::new();
    for p in 0..pages {
        let mut s = String::from("BT /F1 10 Tf 12 TL 54 750 Td\n");
        for l in 0..lines {
            s.push_str(&format!("(Page {p} line {l}: the quick brown fox jumps over the lazy dog, again and again.) '\n"));
        }
        s.push_str("ET\n");
        let mut st = Stream::new(Dictionary::new(), s.into_bytes());
        let _ = st.compress();
        let c = doc.add_object(st);
        let page = doc.add_object(dictionary! { "Type" => "Page", "Parent" => pages_id, "Contents" => c });
        kids.push(Object::Reference(page));
    }
    let n = kids.len() as i64;
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => kids, "Count" => n, "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()], "Resources" => dictionary! { "Font" => dictionary! { "F1" => helv } } }),
    );
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    let mut out = Vec::new();
    doc.save_to(&mut out).expect("write");
    Arc::new(out)
}

/// `pages` pages that all show the same `glyphs` single letters scattered at
/// pseudo-random places, none near enough to another to share a line (map
/// labels, CAD dimensions; or a crafted file).
pub fn scattered_glyphs(pages: usize, glyphs: usize) -> Arc<Vec<u8>> {
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let helv = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica", "Encoding" => "WinAnsiEncoding" });
    let mut s = String::from("BT /F1 1.5 Tf\n");
    let mut seed = 0x9E37_79B9u32;
    for k in 0..glyphs {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        let x = (seed % 58_000) as f64 / 100.0 + 10.0;
        let y = ((seed / 58_000) % 76_000) as f64 / 100.0 + 10.0;
        let c = (b'a' + (k % 26) as u8) as char;
        s.push_str(&format!("1 0 0 1 {x:.2} {y:.2} Tm ({c}) Tj\n"));
    }
    s.push_str("ET\n");
    let mut st = Stream::new(Dictionary::new(), s.into_bytes());
    let _ = st.compress();
    let content = doc.add_object(st);
    let kids: Vec<Object> = (0..pages).map(|_| Object::Reference(doc.add_object(dictionary! { "Type" => "Page", "Parent" => pages_id, "Contents" => content }))).collect();
    let n = kids.len() as i64;
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => kids, "Count" => n, "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()], "Resources" => dictionary! { "Font" => dictionary! { "F1" => helv } } }),
    );
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    let mut out = Vec::new();
    doc.save_to(&mut out).expect("write");
    Arc::new(out)
}

/// Every reference in `doc` points at an object that exists.
pub fn dangling_refs(doc: &Document) -> Vec<(ObjectId, ObjectId)> {
    let mut out = Vec::new();
    for (id, o) in &doc.objects {
        let mut stack = vec![o];
        while let Some(o) = stack.pop() {
            match o {
                Object::Reference(r) if !doc.objects.contains_key(r) => out.push((*id, *r)),
                Object::Array(a) => stack.extend(a.iter()),
                Object::Dictionary(d) => stack.extend(d.iter().map(|(_, v)| v)),
                Object::Stream(s) => stack.extend(s.dict.iter().map(|(_, v)| v)),
                _ => {}
            }
        }
    }
    out
}

pub fn out_dir() -> PathBuf {
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-output");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Write premultiplied RGBA as a PNG under target/test-output.
pub fn write_png(name: &str, w: u32, h: u32, rgba: &[u8]) {
    let path = out_dir().join(name);
    let Ok(file) = std::fs::File::create(&path) else { return };
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut un = rgba.to_vec();
    for p in un.as_chunks_mut::<4>().0 {
        let a = p[3] as u32;
        if a > 0 && a < 255 {
            for c in &mut p[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    if let Ok(mut wr) = enc.write_header() {
        let _ = wr.write_image_data(&un);
    }
}

/// Pixel (x, y) of an RGBA buffer `w` wide.
pub fn px(rgba: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * w + x) * 4) as usize;
    [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
}

pub fn near(a: [u8; 4], b: [u8; 4], tol: u8) -> bool {
    a.iter().zip(b.iter()).all(|(x, y)| x.abs_diff(*y) <= tol)
}

/// Share of pixels in a rect darker than `level` (sum of RGB / 3).
pub fn dark_share(rgba: &[u8], w: u32, x0: u32, y0: u32, x1: u32, y1: u32, level: u32) -> f64 {
    let mut n = 0;
    let mut dark = 0;
    for y in y0..y1 {
        for x in x0..x1 {
            let p = px(rgba, w, x, y);
            n += 1;
            if (p[0] as u32 + p[1] as u32 + p[2] as u32) / 3 < level {
                dark += 1;
            }
        }
    }
    dark as f64 / n.max(1) as f64
}

pub fn system_cjk_font() -> bool {
    anypage_pdf::render::cjk_font().is_some()
}
