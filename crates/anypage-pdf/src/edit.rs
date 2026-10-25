//! Editing through lopdf. Every function here changes a document in place and
//! speaks page space (points, origin top-left of the displayed page, y down);
//! content is written in user space by prefixing `cm` with the page-to-user
//! transform, so rotated and cropped pages need no special cases.

use std::collections::{HashMap, HashSet, VecDeque};

use anypage_runtime::Data;
use lopdf::{dictionary, Dictionary, Document, Object, ObjectId, Stream};

use crate::doc::{page_index, real};
use crate::fonts::{self, Cjk, Run};
use crate::geom::{bbox, quad_user, rect_to_user, PageGeom};
use crate::util::{decode_text_string, deref, f, get_array, get_dict, get_name, get_num, get_text, name, num, text_string, unpack, R};

/// Things an edit session remembers about a document: which pages already had
/// their old content wrapped in `q ... Q`, and the font and graphics-state
/// objects it created, so repeated edits reuse them.
#[derive(Default)]
pub struct EditCx {
    pub wrapped: HashSet<ObjectId>,
    pub objects: HashMap<String, ObjectId>,
}

pub const MAX_PATH_NUMBERS: usize = 4_000_000;
pub const MAX_IMAGE_PIXELS: u64 = 64 << 20;
pub const MAX_TEXT: usize = 1 << 20;
pub const MAX_RECTS: usize = 10_000;
pub const MAX_PAGE_SIDE: f64 = 14_400.0;
pub const MAX_PAGES: usize = 100_000;

fn le(e: lopdf::Error) -> String {
    e.to_string()
}

// ---------------------------------------------------------------- pages

pub fn page_ids(doc: &Document) -> Vec<ObjectId> {
    doc.get_pages().into_values().collect()
}

pub fn page_id(doc: &Document, page: f64) -> R<ObjectId> {
    let ids = page_ids(doc);
    Ok(ids[page_index(page, ids.len())?])
}

/// A page attribute, looked up through the page tree.
pub fn inherited<'a>(doc: &'a Document, pid: ObjectId, key: &[u8]) -> Option<&'a Object> {
    let mut id = pid;
    for _ in 0..64 {
        let d = doc.get_dictionary(id).ok()?;
        if let Ok(o) = d.get(key) {
            let o = deref(doc, o);
            if !matches!(o, Object::Null) {
                return Some(o);
            }
        }
        match d.get(b"Parent") {
            Ok(Object::Reference(p)) => id = *p,
            _ => return None,
        }
    }
    None
}

fn box_of(doc: &Document, o: &Object) -> Option<[f64; 4]> {
    let Object::Array(a) = o else { return None };
    let v: Vec<f64> = a.iter().filter_map(|x| num(deref(doc, x))).collect();
    (v.len() == 4 && v.iter().all(|x| x.is_finite())).then(|| [v[0], v[1], v[2], v[3]])
}

pub fn geom(doc: &Document, pid: ObjectId) -> PageGeom {
    let media = inherited(doc, pid, b"MediaBox").and_then(|o| box_of(doc, o)).unwrap_or([0.0, 0.0, 595.2756, 841.8898]);
    let crop = inherited(doc, pid, b"CropBox").and_then(|o| box_of(doc, o));
    let rotate = inherited(doc, pid, b"Rotate").and_then(num).unwrap_or(0.0);
    PageGeom::new(media, crop, rotate as i64)
}

fn contents_list(doc: &Document, pid: ObjectId) -> Vec<Object> {
    let Ok(page) = doc.get_dictionary(pid) else { return Vec::new() };
    match page.get(b"Contents") {
        Ok(Object::Reference(id)) => match doc.get_object(*id) {
            Ok(Object::Array(a)) => a.clone(),
            Ok(Object::Stream(_)) => vec![Object::Reference(*id)],
            _ => Vec::new(),
        },
        Ok(Object::Array(a)) => a.clone(),
        _ => Vec::new(),
    }
}

/// Add a content stream to a page, after wrapping what is there in `q ... Q`
/// so its leftover graphics state cannot leak into ours.
pub fn append_content(doc: &mut Document, cx: &mut EditCx, pid: ObjectId, ops: String) -> R<()> {
    let mut list = contents_list(doc, pid);
    if !cx.wrapped.contains(&pid) && !list.is_empty() {
        let q = doc.add_object(Stream::new(Dictionary::new(), b"q\n".to_vec()));
        let qq = doc.add_object(Stream::new(Dictionary::new(), b"\nQ\n".to_vec()));
        list.insert(0, Object::Reference(q));
        list.push(Object::Reference(qq));
    }
    let mut s = Stream::new(Dictionary::new(), ops.into_bytes());
    if s.content.len() > 512 {
        let _ = s.compress();
    }
    let id = doc.add_object(s);
    list.push(Object::Reference(id));
    doc.get_dictionary_mut(pid).map_err(le)?.set("Contents", list);
    cx.wrapped.insert(pid);
    Ok(())
}

/// `q` plus the transform that lets the following operators use page space.
fn begin(g: &PageGeom) -> String {
    let m = g.page_to_user().as_coeffs();
    format!("q {} {} {} {} {} {} cm\n", f(m[0]), f(m[1]), f(m[2]), f(m[3]), f(m[4]), f(m[5]))
}

// ---------------------------------------------------------------- resources

/// The page's own resources dictionary, made so if it was inherited.
fn resources_mut(doc: &mut Document, pid: ObjectId) -> R<&mut Dictionary> {
    enum Loc {
        Ref(ObjectId),
        Here,
        Missing,
    }
    let loc = match doc.get_dictionary(pid).map_err(le)?.get(b"Resources") {
        Ok(Object::Reference(r)) => Loc::Ref(*r),
        Ok(Object::Dictionary(_)) => Loc::Here,
        _ => Loc::Missing,
    };
    let loc = match loc {
        Loc::Ref(r) if matches!(doc.get_object(r), Ok(Object::Dictionary(_))) => Loc::Ref(r),
        Loc::Ref(_) | Loc::Missing => {
            let inh = match inherited(doc, pid, b"Resources") {
                Some(Object::Dictionary(d)) => d.clone(),
                _ => Dictionary::new(),
            };
            doc.get_dictionary_mut(pid).map_err(le)?.set("Resources", inh);
            Loc::Here
        }
        Loc::Here => Loc::Here,
    };
    match loc {
        Loc::Ref(r) => doc.get_dictionary_mut(r).map_err(le),
        _ => match doc.get_dictionary_mut(pid).map_err(le)?.get_mut(b"Resources") {
            Ok(Object::Dictionary(d)) => Ok(d),
            _ => Err("internal: page resources".into()),
        },
    }
}

/// The category dictionary (Font, XObject, ExtGState) of a resources dict.
fn category_mut<'a>(doc: &'a mut Document, pid: ObjectId, cat: &[u8]) -> R<&'a mut Dictionary> {
    let target = {
        let res = resources_mut(doc, pid)?;
        match res.get(cat) {
            Ok(Object::Reference(r)) => Some(*r),
            Ok(Object::Dictionary(_)) => None,
            _ => {
                res.set(cat.to_vec(), Dictionary::new());
                None
            }
        }
    };
    if let Some(r) = target {
        if matches!(doc.get_object(r), Ok(Object::Dictionary(_))) {
            return doc.get_dictionary_mut(r).map_err(le);
        }
        resources_mut(doc, pid)?.set(cat.to_vec(), Dictionary::new());
    }
    match resources_mut(doc, pid)?.get_mut(cat) {
        Ok(Object::Dictionary(d)) => Ok(d),
        _ => Err("internal: resource category".into()),
    }
}

/// Name under which `id` is in the page's `cat` resources, added if needed.
fn resource_name(doc: &mut Document, pid: ObjectId, cat: &[u8], prefix: &str, id: ObjectId) -> R<String> {
    let d = category_mut(doc, pid, cat)?;
    for (k, v) in d.iter() {
        if matches!(v, Object::Reference(r) if *r == id) {
            return Ok(String::from_utf8_lossy(k).into_owned());
        }
    }
    let mut k = 1;
    let n = loop {
        let n = format!("{prefix}{k}");
        if !d.has(n.as_bytes()) {
            break n;
        }
        k += 1;
    };
    d.set(n.clone(), Object::Reference(id));
    Ok(n)
}

/// An object made once per document and reused (fonts, graphics states).
fn shared(doc: &mut Document, cx: &mut EditCx, key: &str, make: impl FnOnce(&mut Document) -> ObjectId) -> ObjectId {
    match cx.objects.get(key) {
        Some(id) if doc.objects.contains_key(id) => *id,
        _ => {
            let id = make(doc);
            cx.objects.insert(key.to_string(), id);
            id
        }
    }
}

fn latin_font(doc: &mut Document, cx: &mut EditCx, font: &str) -> ObjectId {
    shared(doc, cx, &format!("font:{font}"), |d| fonts::standard_font_object(d, font))
}

fn cjk_font(doc: &mut Document, cx: &mut EditCx, c: Cjk) -> ObjectId {
    shared(doc, cx, &c.key(), |d| fonts::cjk_font_object(d, c))
}

fn gstate(doc: &mut Document, cx: &mut EditCx, fill_a: f64, stroke_a: f64, blend: Option<&str>) -> ObjectId {
    let key = format!("gs:{}:{}:{}", f(fill_a), f(stroke_a), blend.unwrap_or(""));
    shared(doc, cx, &key, |d| {
        let mut g = dictionary! { "Type" => "ExtGState", "ca" => real(fill_a), "CA" => real(stroke_a) };
        if let Some(b) = blend {
            g.set("BM", name(b));
        }
        d.add_object(g)
    })
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|c| format!("{c:02X}")).collect()
}

fn rgb_op(c: [f64; 4], op: &str) -> String {
    format!("{} {} {} {op}\n", f(c[0]), f(c[1]), f(c[2]))
}

// ---------------------------------------------------------------- text

/// Largest coordinate an edit accepts, in points from the page origin.
pub const MAX_COORD: f64 = 1e6;

/// A page-space coordinate from `d[key]`: finite (Luau's `0/0` and `1/0` are
/// errors, not 0) and within [`MAX_COORD`].
pub fn coord(d: &Data, key: &str, default: f64) -> R<f64> {
    let v = d.get(key);
    let x = match v {
        Data::Nil => default,
        _ => v.num().ok_or_else(|| format!("{key} must be a number"))?,
    };
    if !x.is_finite() || x.abs() > MAX_COORD {
        return Err(format!("{key} must be a finite number of points (|{key}| <= {MAX_COORD}), not {x}"));
    }
    Ok(x)
}

pub struct TextOpts {
    pub text: String,
    pub x: f64,
    pub y: f64,
    pub size: f64,
    pub color: f64,
    pub font: &'static str,
    pub width: Option<f64>,
    pub line_height: f64,
    pub align: String,
    pub lang: Option<Cjk>,
}

impl TextOpts {
    pub fn from_data(d: &Data) -> R<TextOpts> {
        let text = match d.get("text") {
            Data::Str(s) => s.to_string(),
            Data::Num(n) => crate::fmt_num(*n),
            Data::Nil => return Err("add_text needs { text = \"...\" }".into()),
            other => return Err(format!("text must be a string, not {other:?}")),
        };
        if text.len() > MAX_TEXT {
            return Err(format!("text longer than {} KB", MAX_TEXT >> 10));
        }
        let size = d.get("size").num_or(12.0);
        if !(0.5..=1000.0).contains(&size) {
            return Err("size must be between 0.5 and 1000 points".into());
        }
        let font = fonts::pick_standard(d.get("font").str_or("Helvetica"), d.get("bold").truthy(), d.get("italic").truthy())?;
        let lang = match d.get("lang").as_str() {
            Some(l) => Some(Cjk::from_lang(l).ok_or_else(|| format!("unknown lang \"{l}\" (zh-Hant, zh-Hans, ja, ko)"))?),
            None => None,
        };
        let width = match d.get("width") {
            Data::Nil => None,
            v => match v.num() {
                Some(w) if w > 0.0 && w <= MAX_PAGE_SIDE => Some(w),
                _ => return Err(format!("width must be a number of points in 0..={MAX_PAGE_SIDE} (or nil for no wrapping)")),
            },
        };
        let line_height = d.get("line_height").num_or(1.2);
        if !line_height.is_finite() {
            return Err("line_height must be a finite number".into());
        }
        Ok(TextOpts {
            text,
            x: coord(d, "x", 72.0)?,
            y: coord(d, "y", 72.0)?,
            size,
            color: d.get("color").num_or(255.0),
            font,
            width,
            line_height: line_height.clamp(0.5, 10.0),
            align: d.get("align").str_or("left").to_string(),
            lang,
        })
    }

    pub fn lines(&self) -> Vec<String> {
        match self.width {
            Some(w) => fonts::wrap(&self.text, self.font, self.size, w, self.lang),
            None => self.text.split('\n').map(str::to_string).collect(),
        }
    }

    /// The box the text occupies (page space).
    pub fn extent(&self) -> [f64; 4] {
        self.extent_of(&self.lines())
    }

    /// The box `lines` (from [`TextOpts::lines`]) occupy.
    pub fn extent_of(&self, lines: &[String]) -> [f64; 4] {
        let w = lines.iter().map(|l| fonts::text_width(l, self.font, self.size, self.lang)).fold(0.0, f64::max);
        let w = self.width.unwrap_or(w);
        let h = self.size * (1.0 + self.line_height * (lines.len().max(1) - 1) as f64);
        let x = match (self.align.as_str(), self.width) {
            ("center", None) => self.x - w / 2.0,
            ("right", None) => self.x - w,
            _ => self.x,
        };
        [x, self.y, w, h]
    }
}

/// Text operators (between BT and ET per line) for `lines` in page space,
/// registering fonts with `font_name`.
fn text_ops(o: &TextOpts, lines: &[String], mut font_name: impl FnMut(Option<Cjk>) -> R<String>) -> R<String> {
    let mut s = String::new();
    for (k, line) in lines.iter().enumerate() {
        let runs = fonts::runs(line, o.lang);
        if runs.is_empty() {
            continue;
        }
        let width = fonts::runs_width(&runs, o.font, o.size);
        let x = match (o.align.as_str(), o.width) {
            ("center", Some(w)) => o.x + (w - width) / 2.0,
            ("right", Some(w)) => o.x + w - width,
            ("center", None) => o.x - width / 2.0,
            ("right", None) => o.x - width,
            _ => o.x,
        };
        let base = o.y + fonts::ASCENT * o.size + k as f64 * o.line_height * o.size;
        s.push_str(&format!("BT 1 0 0 -1 {} {} Tm\n", f(x), f(base)));
        for r in &runs {
            let (n, bytes) = match r {
                Run::Latin(b) => (font_name(None)?, b),
                Run::Cjk(c, b) => (font_name(Some(*c))?, b),
            };
            s.push_str(&format!("/{n} {} Tf <{}> Tj\n", f(o.size), hex(bytes)));
        }
        s.push_str("ET\n");
    }
    Ok(s)
}

pub fn add_text(doc: &mut Document, cx: &mut EditCx, pid: ObjectId, o: &TextOpts) -> R<()> {
    let g = geom(doc, pid);
    let lines = o.lines();
    let c = unpack(o.color);
    let mut s = begin(&g);
    if c[3] < 1.0 {
        let gs = gstate(doc, cx, c[3], c[3], None);
        let n = resource_name(doc, pid, b"ExtGState", "APGS", gs)?;
        s.push_str(&format!("/{n} gs\n"));
    }
    s.push_str(&rgb_op(c, "rg"));
    let ops = text_ops(o, &lines, |cjk| {
        let id = match cjk {
            None => latin_font(doc, cx, o.font),
            Some(k) => cjk_font(doc, cx, k),
        };
        resource_name(doc, pid, b"Font", "APF", id)
    })?;
    s.push_str(&ops);
    s.push_str("Q\n");
    append_content(doc, cx, pid, s)
}

// ---------------------------------------------------------------- vector paths

#[derive(Clone, Debug, PartialEq)]
pub enum Seg {
    M(f64, f64),
    L(f64, f64),
    C(f64, f64, f64, f64, f64, f64),
    Z,
}

#[derive(Clone, Debug)]
pub struct PathStyle {
    pub stroke: Option<f64>,
    pub fill: Option<f64>,
    pub width: f64,
    pub cap: u8,
    pub join: u8,
    pub dash: Vec<f64>,
    pub even_odd: bool,
}

impl PathStyle {
    fn from_data(d: &Data, base: Option<&PathStyle>) -> R<PathStyle> {
        let b = base.cloned().unwrap_or(PathStyle { stroke: None, fill: None, width: 1.0, cap: 0, join: 0, dash: Vec::new(), even_odd: false });
        let cap = match d.get("cap").as_str() {
            Some("butt") => 0,
            Some("round") => 1,
            Some("square") => 2,
            Some(o) => return Err(format!("cap must be butt, round or square, not {o}")),
            None => b.cap,
        };
        let join = match d.get("join").as_str() {
            Some("miter") => 0,
            Some("round") => 1,
            Some("bevel") => 2,
            Some(o) => return Err(format!("join must be miter, round or bevel, not {o}")),
            None => b.join,
        };
        let dash: Vec<f64> = d.get("dash").items().iter().filter_map(Data::num).filter(|x| *x >= 0.0 && x.is_finite()).collect();
        Ok(PathStyle {
            stroke: d.get("stroke").num().or(b.stroke),
            fill: d.get("fill").num().or(b.fill),
            width: d.get("width").num().filter(|w| *w >= 0.0 && w.is_finite()).unwrap_or(b.width),
            cap,
            join,
            dash: if dash.is_empty() { b.dash } else { dash },
            even_odd: d.get("even_odd").as_bool().unwrap_or(b.even_odd),
        })
    }
}

/// The shared vector path form: `{ "M", x, y, "L", x, y, "C", x1, y1, x2, y2,
/// x, y, "Q", x1, y1, x, y, "Z" }`; numbers after a command repeat it.
pub fn parse_path(items: &[Data]) -> R<Vec<Seg>> {
    if items.len() > MAX_PATH_NUMBERS {
        return Err(format!("path has more than {MAX_PATH_NUMBERS} items"));
    }
    let mut out = Vec::new();
    let mut i = 0;
    let mut cmd = String::new();
    let mut cur = (0.0, 0.0);
    let mut start = (0.0, 0.0);
    let take = |i: &mut usize, n: usize| -> R<Vec<f64>> {
        let v: Vec<f64> = items.get(*i..*i + n).ok_or("path ends in the middle of a command")?.iter().map(|d| d.num().filter(|x| x.is_finite())).collect::<Option<_>>().ok_or("path coordinates must be numbers")?;
        *i += n;
        Ok(v)
    };
    while i < items.len() {
        if let Some(c) = items[i].as_str() {
            cmd = c.to_string();
            i += 1;
            if cmd == "Z" || cmd == "z" {
                out.push(Seg::Z);
                cur = start;
                continue;
            }
        } else if cmd.is_empty() {
            return Err("path must start with a command (\"M\")".into());
        }
        match cmd.as_str() {
            "M" => {
                let v = take(&mut i, 2)?;
                out.push(Seg::M(v[0], v[1]));
                cur = (v[0], v[1]);
                start = cur;
                cmd = "L".into();
            }
            "L" => {
                let v = take(&mut i, 2)?;
                out.push(Seg::L(v[0], v[1]));
                cur = (v[0], v[1]);
            }
            "C" => {
                let v = take(&mut i, 6)?;
                out.push(Seg::C(v[0], v[1], v[2], v[3], v[4], v[5]));
                cur = (v[4], v[5]);
            }
            "Q" => {
                let v = take(&mut i, 4)?;
                let (c1x, c1y) = (cur.0 + 2.0 / 3.0 * (v[0] - cur.0), cur.1 + 2.0 / 3.0 * (v[1] - cur.1));
                let (c2x, c2y) = (v[2] + 2.0 / 3.0 * (v[0] - v[2]), v[3] + 2.0 / 3.0 * (v[1] - v[3]));
                out.push(Seg::C(c1x, c1y, c2x, c2y, v[2], v[3]));
                cur = (v[2], v[3]);
            }
            "Z" | "z" => return Err("numbers after Z".into()),
            other => return Err(format!("unknown path command \"{other}\" (M, L, C, Q, Z)")),
        }
        if !matches!(out.first(), Some(Seg::M(..))) {
            return Err("path must start with \"M\"".into());
        }
    }
    Ok(out)
}

/// The shapes in a path argument: a flat path, `{ path = ..., stroke, ... }`,
/// a list of those (what `cad` `to_paths` returns), or a flat path with style
/// keys alongside.
pub fn shapes(d: &Data, opts: &Data) -> R<Vec<(Vec<Seg>, PathStyle)>> {
    let base = PathStyle::from_data(opts, None)?;
    let one = |x: &Data| -> R<(Vec<Seg>, PathStyle)> {
        match x {
            Data::List(items) => Ok((parse_path(items)?, base.clone())),
            Data::Map(_) => {
                let style = PathStyle::from_data(x, Some(&base))?;
                let p = x.get("path");
                if !p.is_nil() {
                    return Ok((parse_path(p.items())?, style));
                }
                // numeric keys "1", "2", ... are the path
                let mut numbered: Vec<(usize, &Data)> = x.entries().filter_map(|(k, v)| k.parse::<usize>().ok().map(|n| (n, v))).collect();
                numbered.sort_by_key(|e| e.0);
                let items: Vec<Data> = numbered.into_iter().map(|e| e.1.clone()).collect();
                Ok((parse_path(&items)?, style))
            }
            _ => Err("path must be a list like { \"M\", x, y, \"L\", x, y }".into()),
        }
    };
    match d {
        Data::List(items) if items.first().is_some_and(|f| matches!(f, Data::Map(_) | Data::List(_))) => items.iter().map(one).collect(),
        _ => Ok(vec![one(d)?]),
    }
}

fn path_ops(segs: &[Seg]) -> String {
    let mut s = String::new();
    for seg in segs {
        match seg {
            Seg::M(x, y) => s.push_str(&format!("{} {} m\n", f(*x), f(*y))),
            Seg::L(x, y) => s.push_str(&format!("{} {} l\n", f(*x), f(*y))),
            Seg::C(a, b, c, d, e, g) => s.push_str(&format!("{} {} {} {} {} {} c\n", f(*a), f(*b), f(*c), f(*d), f(*e), f(*g))),
            Seg::Z => s.push_str("h\n"),
        }
    }
    s
}

pub fn add_path(doc: &mut Document, cx: &mut EditCx, pid: ObjectId, shapes: &[(Vec<Seg>, PathStyle)]) -> R<()> {
    let g = geom(doc, pid);
    let mut s = begin(&g);
    for (segs, st) in shapes {
        if segs.is_empty() {
            continue;
        }
        let stroke = st.stroke.or(if st.fill.is_none() { Some(255.0) } else { None });
        let fill_c = st.fill.map(unpack);
        let stroke_c = stroke.map(unpack);
        s.push_str("q\n");
        let (fa, sa) = (fill_c.map_or(1.0, |c| c[3]), stroke_c.map_or(1.0, |c| c[3]));
        if fa < 1.0 || sa < 1.0 {
            let gs = gstate(doc, cx, fa, sa, None);
            let n = resource_name(doc, pid, b"ExtGState", "APGS", gs)?;
            s.push_str(&format!("/{n} gs\n"));
        }
        s.push_str(&format!("{} w {} J {} j\n", f(st.width), st.cap, st.join));
        if !st.dash.is_empty() {
            let d: Vec<String> = st.dash.iter().map(|x| f(*x)).collect();
            s.push_str(&format!("[{}] 0 d\n", d.join(" ")));
        }
        if let Some(c) = fill_c {
            s.push_str(&rgb_op(c, "rg"));
        }
        if let Some(c) = stroke_c {
            s.push_str(&rgb_op(c, "RG"));
        }
        s.push_str(&path_ops(segs));
        let star = if st.even_odd { "*" } else { "" };
        s.push_str(&match (fill_c.is_some(), stroke_c.is_some()) {
            (true, true) => format!("B{star}\n"),
            (true, false) => format!("f{star}\n"),
            _ => "S\n".to_string(),
        });
        s.push_str("Q\n");
    }
    s.push_str("Q\n");
    append_content(doc, cx, pid, s)
}

// ---------------------------------------------------------------- images

pub enum ImageSrc {
    /// Premultiplied RGBA8.
    Rgba { w: u32, h: u32, rgba: Vec<u8> },
    Jpeg { w: u32, h: u32, components: u8, adobe: bool, bytes: Vec<u8> },
}

/// Width, height, components and the Adobe marker of a baseline or
/// progressive JPEG, from its frame header.
pub fn jpeg_info(b: &[u8]) -> Option<(u32, u32, u8, bool)> {
    if b.get(0..2)? != [0xFF, 0xD8] {
        return None;
    }
    let mut i = 2;
    let mut adobe = false;
    while i + 4 <= b.len() {
        if b[i] != 0xFF {
            return None;
        }
        let marker = b[i + 1];
        if marker == 0xFF {
            i += 1;
            continue;
        }
        let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        if marker == 0xEE && b.get(i + 4..i + 9) == Some(b"Adobe") {
            adobe = true;
        }
        if matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF) {
            let h = u16::from_be_bytes([*b.get(i + 5)?, *b.get(i + 6)?]) as u32;
            let w = u16::from_be_bytes([*b.get(i + 7)?, *b.get(i + 8)?]) as u32;
            let n = *b.get(i + 9)?;
            return Some((w, h, n, adobe));
        }
        i += 2 + len;
    }
    None
}

/// PNG bytes to premultiplied RGBA8.
pub fn decode_png(b: &[u8]) -> R<(u32, u32, Vec<u8>)> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(b));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info().map_err(|e| format!("PNG: {e}"))?;
    let (w, h) = reader.info().size();
    if w as u64 * h as u64 > MAX_IMAGE_PIXELS {
        return Err(format!("image is {w} x {h}; at most {} Mpx", MAX_IMAGE_PIXELS >> 20));
    }
    let mut buf = vec![0u8; reader.output_buffer_size().ok_or("PNG: image too large")?];
    let info = reader.next_frame(&mut buf).map_err(|e| format!("PNG: {e}"))?;
    let px = &buf[..info.buffer_size()];
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    match info.color_type {
        png::ColorType::Rgba => rgba.extend_from_slice(px),
        png::ColorType::Rgb => px.as_chunks::<3>().0.iter().for_each(|p| rgba.extend_from_slice(&[p[0], p[1], p[2], 255])),
        png::ColorType::GrayscaleAlpha => px.as_chunks::<2>().0.iter().for_each(|p| rgba.extend_from_slice(&[p[0], p[0], p[0], p[1]])),
        png::ColorType::Grayscale => px.iter().for_each(|&g| rgba.extend_from_slice(&[g, g, g, 255])),
        other => return Err(format!("PNG: unsupported colour type {other:?}")),
    }
    for p in rgba.as_chunks_mut::<4>().0 {
        let a = p[3] as u32;
        for c in &mut p[..3] {
            *c = (*c as u32 * a / 255) as u8;
        }
    }
    Ok((w, h, rgba))
}

impl ImageSrc {
    pub fn from_data(d: &Data) -> R<ImageSrc> {
        if let Some(img) = d.as_res().and_then(|r| r.get::<anypage_engine::Image>()) {
            if img.rgba.len() != (img.w as usize) * (img.h as usize) * 4 || img.w == 0 || img.h == 0 {
                return Err("the image has no pixels".into());
            }
            return Ok(ImageSrc::Rgba { w: img.w, h: img.h, rgba: img.rgba.clone() });
        }
        let b = d.as_bytes().ok_or("image must be an image value, or PNG or JPEG bytes")?;
        if let Some((w, h, components, adobe)) = jpeg_info(b) {
            if !matches!(components, 1 | 3 | 4) || w == 0 || h == 0 {
                return Err("unsupported JPEG".into());
            }
            return Ok(ImageSrc::Jpeg { w, h, components, adobe, bytes: b.to_vec() });
        }
        if b.starts_with(&[0x89, b'P', b'N', b'G']) {
            let (w, h, rgba) = decode_png(b)?;
            return Ok(ImageSrc::Rgba { w, h, rgba });
        }
        Err("image bytes are neither PNG nor JPEG".into())
    }
}

fn image_object(doc: &mut Document, src: ImageSrc) -> R<ObjectId> {
    match src {
        ImageSrc::Jpeg { w, h, components, adobe, bytes } => {
            let cs = match components {
                1 => "DeviceGray",
                4 => "DeviceCMYK",
                _ => "DeviceRGB",
            };
            let mut d = dictionary! { "Type" => "XObject", "Subtype" => "Image", "Width" => w as i64, "Height" => h as i64, "ColorSpace" => cs, "BitsPerComponent" => 8, "Filter" => "DCTDecode" };
            if components == 4 && adobe {
                d.set("Decode", vec![1.into(), 0.into(), 1.into(), 0.into(), 1.into(), 0.into(), 1.into(), 0.into()]);
            }
            Ok(doc.add_object(Stream::new(d, bytes).with_compression(false)))
        }
        ImageSrc::Rgba { w, h, rgba } => {
            if w as u64 * h as u64 > MAX_IMAGE_PIXELS {
                return Err(format!("image is {w} x {h}; at most {} Mpx", MAX_IMAGE_PIXELS >> 20));
            }
            let n = (w * h) as usize;
            let mut rgb = Vec::with_capacity(n * 3);
            let mut alpha = Vec::with_capacity(n);
            for p in rgba.as_chunks::<4>().0 {
                let a = p[3];
                let un = |c: u8| if a == 0 { 0 } else { ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8 };
                rgb.extend_from_slice(&[un(p[0]), un(p[1]), un(p[2])]);
                alpha.push(a);
            }
            let mut d = dictionary! { "Type" => "XObject", "Subtype" => "Image", "Width" => w as i64, "Height" => h as i64, "ColorSpace" => "DeviceRGB", "BitsPerComponent" => 8 };
            if alpha.iter().any(|&a| a < 255) {
                let mut m = Stream::new(dictionary! { "Type" => "XObject", "Subtype" => "Image", "Width" => w as i64, "Height" => h as i64, "ColorSpace" => "DeviceGray", "BitsPerComponent" => 8 }, alpha);
                let _ = m.compress();
                let mid = doc.add_object(m);
                d.set("SMask", mid);
            }
            let mut s = Stream::new(d, rgb);
            let _ = s.compress();
            Ok(doc.add_object(s))
        }
    }
}

pub fn add_image(doc: &mut Document, cx: &mut EditCx, pid: ObjectId, src: ImageSrc, r: [f64; 4]) -> R<()> {
    let g = geom(doc, pid);
    let id = image_object(doc, src)?;
    let n = resource_name(doc, pid, b"XObject", "APIm", id)?;
    let s = format!("{}q {} 0 0 {} {} {} cm /{n} Do Q\nQ\n", begin(&g), f(r[2]), f(-r[3]), f(r[0]), f(r[1] + r[3]));
    append_content(doc, cx, pid, s)
}

// ---------------------------------------------------------------- annotations

fn push_annot(doc: &mut Document, pid: ObjectId, annot: ObjectId) -> R<()> {
    let target = match doc.get_dictionary(pid).map_err(le)?.get(b"Annots") {
        Ok(Object::Reference(r)) if matches!(doc.get_object(*r), Ok(Object::Array(_))) => Some(*r),
        _ => None,
    };
    if let Some(r) = target {
        if let Ok(Object::Array(a)) = doc.get_object_mut(r) {
            a.push(Object::Reference(annot));
        }
        return Ok(());
    }
    let page = doc.get_dictionary_mut(pid).map_err(le)?;
    match page.get_mut(b"Annots") {
        Ok(Object::Array(a)) => a.push(Object::Reference(annot)),
        _ => page.set("Annots", vec![Object::Reference(annot)]),
    }
    Ok(())
}

fn rect_obj(r: hayro::kurbo::Rect) -> Object {
    Object::Array(vec![real(r.x0), real(r.y0), real(r.x1), real(r.y1)])
}

pub fn highlight(doc: &mut Document, pid: ObjectId, rects: &[[f64; 4]], color: f64, contents: Option<&str>) -> R<()> {
    if rects.is_empty() {
        return Err("highlight needs at least one rect { x, y, w, h }".into());
    }
    if rects.len() > MAX_RECTS {
        return Err(format!("at most {MAX_RECTS} rects"));
    }
    let g = geom(doc, pid);
    let c = unpack(color);
    let mut quads = Vec::new();
    let mut pts = Vec::new();
    let mut ap = String::from("/GS0 gs\n");
    ap.push_str(&rgb_op(c, "rg"));
    for r in rects {
        let q = quad_user(&g, r[0], r[1], r[2], r[3]);
        for p in &q {
            quads.push(real(p.x));
            quads.push(real(p.y));
        }
        pts.extend_from_slice(&q);
        // TL, TR, BR, BL
        ap.push_str(&format!("{} {} m {} {} l {} {} l {} {} l h f\n", f(q[0].x), f(q[0].y), f(q[1].x), f(q[1].y), f(q[3].x), f(q[3].y), f(q[2].x), f(q[2].y)));
    }
    let b = bbox(&pts).inflate(1.0, 1.0);
    let form = Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Form", "BBox" => rect_obj(b),
            "Resources" => dictionary! { "ExtGState" => dictionary! { "GS0" => dictionary! { "Type" => "ExtGState", "BM" => "Multiply", "ca" => real(c[3]), "CA" => real(c[3]) } } },
        },
        ap.into_bytes(),
    );
    let form = doc.add_object(form);
    let mut annot = dictionary! {
        "Type" => "Annot", "Subtype" => "Highlight", "Rect" => rect_obj(b), "QuadPoints" => quads,
        "C" => vec![real(c[0]), real(c[1]), real(c[2])], "CA" => real(c[3]), "F" => 4, "P" => pid,
        "AP" => dictionary! { "N" => form },
    };
    if let Some(t) = contents {
        annot.set("Contents", text_string(t));
    }
    let id = doc.add_object(annot);
    push_annot(doc, pid, id)
}

pub struct NoteOpts {
    pub x: f64,
    pub y: f64,
    pub text: String,
    pub title: Option<String>,
    pub color: f64,
}

pub fn add_note(doc: &mut Document, pid: ObjectId, o: &NoteOpts) -> R<()> {
    if o.text.len() > MAX_TEXT {
        return Err("note text too long".into());
    }
    let g = geom(doc, pid);
    let r = rect_to_user(&g, o.x, o.y, 20.0, 20.0);
    let (w, h) = (r.width(), r.height());
    let c = unpack(o.color);
    // a note icon: rounded square, folded corner, three lines
    let ap = format!(
        "{}0.25 0.2 0.1 RG 1 w\n1 1 {} {} re B\n0.25 0.2 0.1 rg\n{} {} m {} {} l {} {} l f\n{} {} m {} {} l S\n{} {} m {} {} l S\n{} {} m {} {} l S\n",
        rgb_op(c, "rg"),
        f(w - 2.0),
        f(h - 2.0),
        f(w - 7.0),
        f(h - 1.0),
        f(w - 1.0),
        f(h - 7.0),
        f(w - 7.0),
        f(h - 7.0),
        f(4.0),
        f(h * 0.62),
        f(w - 9.0),
        f(h * 0.62),
        f(4.0),
        f(h * 0.45),
        f(w - 4.0),
        f(h * 0.45),
        f(4.0),
        f(h * 0.28),
        f(w - 4.0),
        f(h * 0.28)
    );
    let form = doc.add_object(Stream::new(dictionary! { "Type" => "XObject", "Subtype" => "Form", "BBox" => vec![0.into(), 0.into(), real(w), real(h)] }, ap.into_bytes()));
    let mut annot = dictionary! {
        "Type" => "Annot", "Subtype" => "Text", "Rect" => rect_obj(r), "Contents" => text_string(&o.text),
        "Name" => "Comment", "C" => vec![real(c[0]), real(c[1]), real(c[2])], "F" => 28, "Open" => false, "P" => pid,
        "AP" => dictionary! { "N" => form },
    };
    if let Some(t) = &o.title {
        annot.set("T", text_string(t));
    }
    let id = doc.add_object(annot);
    push_annot(doc, pid, id)
}

/// Annotations of a page: `{ type, rect, contents, title, color }`.
pub fn annotations(doc: &Document, pid: ObjectId) -> Vec<Data> {
    let g = geom(doc, pid);
    let to_page = g.user_to_page();
    let Ok(page) = doc.get_dictionary(pid) else { return Vec::new() };
    let Some(list) = get_array(doc, page, b"Annots") else { return Vec::new() };
    let mut out = Vec::new();
    for a in list.iter().take(10_000) {
        let Object::Dictionary(d) = deref(doc, a) else { continue };
        let kind = get_name(doc, d, b"Subtype").map(|n| String::from_utf8_lossy(n).into_owned()).unwrap_or_default();
        let mut m: Vec<(&str, Data)> = vec![("type", Data::str(&kind))];
        if let Some(b) = d.get(b"Rect").ok().and_then(|o| box_of(doc, deref(doc, o))) {
            let pts = [hayro::kurbo::Point::new(b[0], b[1]), hayro::kurbo::Point::new(b[2], b[3])].map(|p| to_page * p);
            let r = bbox(&pts);
            m.push(("rect", crate::rect_data([r.x0 as f32, r.y0 as f32, r.width() as f32, r.height() as f32])));
        }
        if let Some(t) = get_text(doc, d, b"Contents") {
            m.push(("contents", Data::str(&t)));
        }
        if let Some(t) = get_text(doc, d, b"T") {
            m.push(("title", Data::str(&t)));
        }
        if let Some(c) = get_array(doc, d, b"C").filter(|c| c.len() == 3) {
            let v: Vec<u32> = c.iter().map(|x| (num(deref(doc, x)).unwrap_or(0.0).clamp(0.0, 1.0) * 255.0).round() as u32).collect();
            m.push(("color", Data::Num(((v[0] << 24) | (v[1] << 16) | (v[2] << 8) | 255) as f64)));
        }
        if kind == "Widget" {
            if let Some(t) = field_name_of(doc, d) {
                m.push(("field", Data::str(&t)));
            }
        }
        out.push(Data::map(m));
    }
    out
}

fn field_name_of(doc: &Document, d: &Dictionary) -> Option<String> {
    let mut parts = Vec::new();
    let mut cur = d;
    for _ in 0..32 {
        if let Some(t) = get_text(doc, cur, b"T") {
            parts.push(t);
        }
        match get_dict(doc, cur, b"Parent") {
            Some(p) => cur = p,
            None => break,
        }
    }
    (!parts.is_empty()).then(|| parts.into_iter().rev().collect::<Vec<_>>().join("."))
}

// ---------------------------------------------------------------- forms

#[derive(Clone, Debug)]
pub struct Field {
    pub name: String,
    pub id: ObjectId,
    pub kind: &'static str,
    pub value: Data,
    pub options: Vec<String>,
    pub flags: i64,
    pub widgets: Vec<ObjectId>,
    pub da: Option<String>,
    pub quadding: i64,
    /// On-state names of check boxes and radio buttons.
    pub states: Vec<String>,
}

#[derive(Clone, Default)]
struct Inherit {
    ft: Option<Vec<u8>>,
    ff: Option<i64>,
    v: Option<Object>,
    da: Option<String>,
    q: Option<i64>,
    opt: Option<Vec<Object>>,
}

fn ref_ids(doc: &Document, d: &Dictionary, key: &[u8]) -> Vec<ObjectId> {
    get_array(doc, d, key).map(|a| a.iter().filter_map(|o| if let Object::Reference(r) = o { Some(*r) } else { None }).collect()).unwrap_or_default()
}

fn on_states(doc: &Document, widget: &Dictionary) -> Vec<String> {
    let n = get_dict(doc, widget, b"AP").and_then(|ap| get_dict(doc, ap, b"N"));
    n.map(|n| n.iter().map(|(k, _)| String::from_utf8_lossy(k).into_owned()).filter(|k| k != "Off").collect()).unwrap_or_default()
}

fn walk_field(doc: &Document, id: ObjectId, parent: &str, inh: &Inherit, depth: u32, seen: &mut HashSet<ObjectId>, out: &mut Vec<Field>) {
    if depth > 32 || !seen.insert(id) || out.len() > 10_000 {
        return;
    }
    let Ok(d) = doc.get_dictionary(id) else { return };
    let name = match get_text(doc, d, b"T") {
        Some(t) if parent.is_empty() => t,
        Some(t) => format!("{parent}.{t}"),
        None => parent.to_string(),
    };
    let mut h = inh.clone();
    if let Some(n) = get_name(doc, d, b"FT") {
        h.ft = Some(n.to_vec());
    }
    if let Some(n) = get_num(doc, d, b"Ff") {
        h.ff = Some(n as i64);
    }
    if let Ok(v) = d.get(b"V") {
        h.v = Some(deref(doc, v).clone());
    }
    if let Some(s) = get_text(doc, d, b"DA") {
        h.da = Some(s);
    }
    if let Some(q) = get_num(doc, d, b"Q") {
        h.q = Some(q as i64);
    }
    if let Some(o) = get_array(doc, d, b"Opt") {
        h.opt = Some(o.clone());
    }
    let kids = ref_ids(doc, d, b"Kids");
    let field_kids: Vec<ObjectId> = kids.iter().copied().filter(|k| doc.get_dictionary(*k).is_ok_and(|kd| kd.has(b"T"))).collect();
    if !field_kids.is_empty() {
        for k in field_kids {
            walk_field(doc, k, &name, &h, depth + 1, seen, out);
        }
        return;
    }
    let widgets = if kids.is_empty() { vec![id] } else { kids };
    let ff = h.ff.unwrap_or(0);
    let kind = match h.ft.as_deref() {
        Some(b"Tx") => "text",
        Some(b"Btn") if ff & (1 << 16) != 0 => "button",
        Some(b"Btn") if ff & (1 << 15) != 0 => "radio",
        Some(b"Btn") => "checkbox",
        Some(b"Ch") => "choice",
        Some(b"Sig") => "signature",
        _ => "unknown",
    };
    let value = match &h.v {
        Some(Object::String(s, _)) => Data::str(&decode_text_string(s)),
        Some(Object::Name(n)) => {
            let n = String::from_utf8_lossy(n).into_owned();
            match kind {
                "checkbox" => Data::Bool(n != "Off"),
                _ if n == "Off" => Data::Nil,
                _ => Data::str(&n),
            }
        }
        Some(Object::Array(a)) => Data::list(a.iter().filter_map(|o| if let Object::String(s, _) = deref(doc, o) { Some(Data::str(&decode_text_string(s))) } else { None }).collect()),
        _ if kind == "checkbox" => Data::Bool(false),
        _ => Data::Nil,
    };
    let options = h
        .opt
        .iter()
        .flatten()
        .filter_map(|o| match deref(doc, o) {
            Object::String(s, _) => Some(decode_text_string(s)),
            Object::Array(pair) => pair.get(1).and_then(|x| if let Object::String(s, _) = deref(doc, x) { Some(decode_text_string(s)) } else { None }),
            _ => None,
        })
        .collect();
    let mut states: Vec<String> = Vec::new();
    for w in &widgets {
        if let Ok(wd) = doc.get_dictionary(*w) {
            for s in on_states(doc, wd) {
                if !states.contains(&s) {
                    states.push(s);
                }
            }
        }
    }
    out.push(Field { name, id, kind, value, options, flags: ff, widgets, da: h.da.clone(), quadding: h.q.unwrap_or(0), states });
}

pub fn fields(doc: &Document) -> Vec<Field> {
    let mut out = Vec::new();
    let Ok(cat) = doc.catalog() else { return out };
    let Some(form) = get_dict(doc, cat, b"AcroForm") else { return out };
    let mut inh = Inherit::default();
    if let Some(s) = get_text(doc, form, b"DA") {
        inh.da = Some(s);
    }
    let mut seen = HashSet::new();
    for id in ref_ids(doc, form, b"Fields") {
        walk_field(doc, id, "", &inh, 0, &mut seen, &mut out);
    }
    out
}

/// Which page each annotation is on.
fn annot_pages(doc: &Document) -> HashMap<ObjectId, usize> {
    let mut m = HashMap::new();
    for (i, pid) in page_ids(doc).into_iter().enumerate() {
        if let Ok(p) = doc.get_dictionary(pid) {
            for a in ref_ids(doc, p, b"Annots") {
                m.entry(a).or_insert(i);
            }
        }
    }
    m
}

pub fn fields_data(doc: &Document) -> Vec<Data> {
    let pages = annot_pages(doc);
    let ids = page_ids(doc);
    fields(doc)
        .into_iter()
        .map(|fl| {
            let mut rects = Vec::new();
            for w in &fl.widgets {
                let Some(&pi) = pages.get(w) else { continue };
                let Ok(wd) = doc.get_dictionary(*w) else { continue };
                let Some(b) = wd.get(b"Rect").ok().and_then(|o| box_of(doc, deref(doc, o))) else { continue };
                let g = geom(doc, ids[pi]);
                let t = g.user_to_page();
                let r = bbox(&[t * hayro::kurbo::Point::new(b[0], b[1]), t * hayro::kurbo::Point::new(b[2], b[3])]);
                rects.push(Data::map([
                    ("page", Data::Num(pi as f64 + 1.0)),
                    ("x", Data::Num(crate::round2(r.x0))),
                    ("y", Data::Num(crate::round2(r.y0))),
                    ("w", Data::Num(crate::round2(r.width()))),
                    ("h", Data::Num(crate::round2(r.height()))),
                ]));
            }
            let mut m = vec![
                ("name", Data::str(&fl.name)),
                ("type", Data::str(fl.kind)),
                ("value", fl.value.clone()),
                ("readonly", Data::Bool(fl.flags & 1 != 0)),
                ("required", Data::Bool(fl.flags & 2 != 0)),
                ("widgets", Data::list(rects)),
            ];
            if !fl.options.is_empty() {
                m.push(("options", Data::list(fl.options.iter().map(|o| Data::str(o)).collect())));
            }
            if !fl.states.is_empty() {
                m.push(("states", Data::list(fl.states.iter().map(|o| Data::str(o)).collect())));
            }
            if fl.kind == "text" {
                m.push(("multiline", Data::Bool(fl.flags & (1 << 12) != 0)));
            }
            Data::map(m)
        })
        .collect()
}

/// Font size and colour from a default appearance string ("/Helv 0 Tf 0 g").
fn parse_da(da: Option<&str>) -> (f64, [f64; 4]) {
    let toks: Vec<&str> = da.unwrap_or("").split_whitespace().collect();
    let mut size = 0.0;
    let mut color = [0.0, 0.0, 0.0, 1.0];
    for (i, t) in toks.iter().enumerate() {
        let n = |k: usize| i.checked_sub(k).and_then(|j| toks.get(j)).and_then(|s| s.parse::<f64>().ok());
        match *t {
            "Tf" => size = n(1).unwrap_or(0.0),
            "g" => {
                let g = n(1).unwrap_or(0.0);
                color = [g, g, g, 1.0];
            }
            "rg" => color = [n(3).unwrap_or(0.0), n(2).unwrap_or(0.0), n(1).unwrap_or(0.0), 1.0],
            _ => {}
        }
    }
    (size, color)
}

/// The `/Rect` of a widget annotation, normalised.
fn widget_box(doc: &Document, widget: ObjectId) -> Option<[f64; 4]> {
    let wd = doc.get_dictionary(widget).ok()?;
    let b = wd.get(b"Rect").ok().and_then(|o| box_of(doc, deref(doc, o)))?;
    Some(crate::geom::norm_box(b))
}

fn text_appearance(doc: &mut Document, cx: &mut EditCx, widget: ObjectId, value: &str, field: &Field) -> R<()> {
    let b = widget_box(doc, widget).ok_or("form widget has no Rect")?;
    let (w, h) = ((b[2] - b[0]).max(1.0), (b[3] - b[1]).max(1.0));
    let (da_size, color) = parse_da(field.da.as_deref());
    let multiline = field.flags & (1 << 12) != 0;
    let size = if da_size > 0.0 { da_size } else if multiline { 10.0 } else { ((h - 4.0) * 0.75).clamp(4.0, 12.0) };
    let o = TextOpts {
        text: value.to_string(),
        x: 0.0,
        y: 0.0,
        size,
        color: 0.0,
        font: "Helvetica",
        width: multiline.then_some((w - 4.0).max(1.0)),
        line_height: 1.15,
        align: "left".into(),
        lang: None,
    };
    let lines = if multiline { o.lines() } else { vec![value.replace('\n', " ")] };
    let mut fonts_used: Vec<(String, ObjectId)> = Vec::new();
    let mut s = String::from("/Tx BMC\nq 1 1 ");
    s.push_str(&format!("{} {} re W n\n", f(w - 2.0), f(h - 2.0)));
    s.push_str(&rgb_op(color, "rg"));
    for (k, line) in lines.iter().enumerate() {
        let runs = fonts::runs(line, None);
        let tw = fonts::runs_width(&runs, "Helvetica", size);
        let x = match field.quadding {
            1 => (w - tw) / 2.0,
            2 => w - 2.0 - tw,
            _ => 2.0,
        };
        let base = if multiline { h - 2.0 - fonts::ASCENT * size - k as f64 * 1.15 * size } else { (h - size) / 2.0 + 0.2 * size };
        s.push_str(&format!("BT 1 0 0 1 {} {} Tm\n", f(x), f(base)));
        for r in &runs {
            let (key, id, bytes) = match r {
                Run::Latin(bt) => ("F0".to_string(), latin_font(doc, cx, "Helvetica"), bt),
                Run::Cjk(c, bt) => (format!("C{}", c.key().trim_start_matches("cjk:")), cjk_font(doc, cx, *c), bt),
            };
            if !fonts_used.iter().any(|(k, _)| *k == key) {
                fonts_used.push((key.clone(), id));
            }
            s.push_str(&format!("/{key} {} Tf <{}> Tj\n", f(size), hex(bytes)));
        }
        s.push_str("ET\n");
    }
    s.push_str("Q\nEMC\n");
    let mut font_dict = Dictionary::new();
    for (k, id) in fonts_used {
        font_dict.set(k, id);
    }
    let form = doc.add_object(Stream::new(
        dictionary! { "Type" => "XObject", "Subtype" => "Form", "BBox" => vec![0.into(), 0.into(), real(w), real(h)], "Resources" => dictionary! { "Font" => font_dict } },
        s.into_bytes(),
    ));
    let wd = doc.get_dictionary_mut(widget).map_err(le)?;
    wd.set("AP", dictionary! { "N" => form });
    Ok(())
}

fn value_string(v: &Data) -> R<String> {
    Ok(match v {
        Data::Str(s) => s.to_string(),
        Data::Num(n) => crate::fmt_num(*n),
        Data::Bool(b) => b.to_string(),
        Data::Nil => String::new(),
        other => return Err(format!("a field value must be a string, number or boolean, not {other:?}")),
    })
}

/// One checked change of [`fill_form`].
enum Fill {
    Text(String),
    Check(bool),
    Radio(String),
}

/// Set field values (`{ name = value }`) and redraw their appearances.
///
/// Every name and value is checked before the document is touched, so a fill
/// that fails changes nothing.
pub fn fill_form(doc: &mut Document, cx: &mut EditCx, values: &[(String, Data)]) -> R<usize> {
    let all = fields(doc);
    if all.is_empty() {
        return Err("the document has no form fields".into());
    }
    let mut plan: Vec<(&Field, Fill)> = Vec::with_capacity(values.len());
    for (key, v) in values {
        let field = all.iter().find(|fl| fl.name == *key).ok_or_else(|| {
            let names: Vec<&str> = all.iter().map(|fl| fl.name.as_str()).take(30).collect();
            format!("no form field \"{key}\" (fields: {})", names.join(", "))
        })?;
        if field.flags & 1 != 0 {
            return Err(format!("form field \"{key}\" is read-only"));
        }
        let fill = match field.kind {
            "text" | "choice" => {
                let s = value_string(v)?;
                if s.len() > MAX_TEXT {
                    return Err(format!("the value for \"{key}\" is longer than {} KB", MAX_TEXT >> 10));
                }
                if field.kind == "choice" && !field.options.is_empty() && field.flags & (1 << 18) == 0 && !field.options.contains(&s) && !s.is_empty() {
                    return Err(format!("\"{s}\" is not one of the options of \"{key}\": {}", field.options.join(", ")));
                }
                if field.widgets.iter().any(|w| widget_box(doc, *w).is_none()) {
                    return Err(format!("form field \"{key}\" has a widget without a Rect, so its value cannot be drawn"));
                }
                Fill::Text(s)
            }
            "checkbox" => Fill::Check(match v {
                Data::Str(s) => &**s != "Off" && !s.is_empty(),
                other => other.truthy(),
            }),
            "radio" => {
                let s = value_string(v)?;
                if !s.is_empty() && !field.states.contains(&s) {
                    return Err(format!("\"{s}\" is not a choice of \"{key}\": {}", field.states.join(", ")));
                }
                Fill::Radio(if s.is_empty() { "Off".to_string() } else { s })
            }
            other => return Err(format!("form field \"{key}\" is a {other} and cannot be filled")),
        };
        plan.push((field, fill));
    }
    for (field, fill) in &plan {
        match fill {
            Fill::Text(s) => {
                doc.get_dictionary_mut(field.id).map_err(le)?.set("V", text_string(s));
                for w in &field.widgets {
                    text_appearance(doc, cx, *w, s, field)?;
                }
            }
            Fill::Check(on) => {
                let mut v_name = "Off".to_string();
                for w in &field.widgets {
                    let state = doc.get_dictionary(*w).ok().map(|wd| on_states(doc, wd)).and_then(|s| s.into_iter().next()).unwrap_or_else(|| "Yes".into());
                    let as_name = if *on { state.clone() } else { "Off".into() };
                    if *on {
                        v_name = state;
                    }
                    doc.get_dictionary_mut(*w).map_err(le)?.set("AS", name(&as_name));
                }
                doc.get_dictionary_mut(field.id).map_err(le)?.set("V", name(&v_name));
            }
            Fill::Radio(s) => {
                for w in &field.widgets {
                    let has = doc.get_dictionary(*w).ok().map(|wd| on_states(doc, wd)).is_some_and(|st| st.contains(s));
                    doc.get_dictionary_mut(*w).map_err(le)?.set("AS", name(if has { s } else { "Off" }));
                }
                doc.get_dictionary_mut(field.id).map_err(le)?.set("V", name(s));
            }
        }
    }
    Ok(plan.len())
}

// ---------------------------------------------------------------- page operations

fn parent_of(doc: &Document, pid: ObjectId) -> R<ObjectId> {
    match doc.get_dictionary(pid).map_err(le)?.get(b"Parent") {
        Ok(Object::Reference(p)) => Ok(*p),
        _ => Err("page has no parent in the page tree".into()),
    }
}

fn add_count(doc: &mut Document, mut node: ObjectId, delta: i64) {
    for _ in 0..64 {
        let Ok(d) = doc.get_dictionary_mut(node) else { return };
        let c = d.get(b"Count").ok().and_then(|o| o.as_i64().ok()).unwrap_or(0);
        d.set("Count", c + delta);
        match d.get(b"Parent") {
            Ok(Object::Reference(p)) => node = *p,
            _ => return,
        }
    }
}

fn kids_mut(doc: &mut Document, node: ObjectId) -> R<&mut Vec<Object>> {
    let kref = match doc.get_dictionary(node).map_err(le)?.get(b"Kids") {
        Ok(Object::Reference(r)) => Some(*r),
        _ => None,
    };
    if let Some(r) = kref {
        return match doc.get_object_mut(r) {
            Ok(Object::Array(a)) => Ok(a),
            _ => Err("broken page tree".into()),
        };
    }
    let d = doc.get_dictionary_mut(node).map_err(le)?;
    if !matches!(d.get(b"Kids"), Ok(Object::Array(_))) {
        d.set("Kids", Vec::<Object>::new());
    }
    match d.get_mut(b"Kids") {
        Ok(Object::Array(a)) => Ok(a),
        _ => Err("broken page tree".into()),
    }
}

/// Remove page `page` and everything that only it held: its annotations, the
/// form fields whose widgets were on it (emptied parent fields go too). Every
/// remaining reference to what was removed (outline and link destinations,
/// named destinations, `/P` entries, replies) becomes null, so the saved file
/// has no reference to a missing object.
pub fn delete_page(doc: &mut Document, page: f64) -> R<()> {
    let ids = page_ids(doc);
    let i = page_index(page, ids.len())?;
    if ids.len() == 1 {
        return Err("cannot delete the only page of a document".into());
    }
    let pid = ids[i];
    let parent = parent_of(doc, pid)?;
    // annotations of this page that no other page shows
    let elsewhere: HashSet<ObjectId> = ids.iter().filter(|&&p| p != pid).filter_map(|p| doc.get_dictionary(*p).ok()).flat_map(|d| ref_ids(doc, d, b"Annots")).collect();
    let annots: Vec<ObjectId> = doc.get_dictionary(pid).map(|d| ref_ids(doc, d, b"Annots")).unwrap_or_default().into_iter().filter(|a| !elsewhere.contains(a)).collect();
    let widgets: Vec<ObjectId> = annots.iter().copied().filter(|a| doc.get_dictionary(*a).is_ok_and(|d| get_name(doc, d, b"Subtype") == Some(b"Widget"))).collect();
    kids_mut(doc, parent)?.retain(|k| !matches!(k, Object::Reference(r) if *r == pid));
    add_count(doc, parent, -1);
    let mut gone: HashSet<ObjectId> = annots.into_iter().collect();
    gone.insert(pid);
    gone.extend(remove_fields(doc, &widgets));
    for id in &gone {
        doc.objects.remove(id);
    }
    null_refs(doc, &gone);
    Ok(())
}

/// Remove `target` from the array `holder[key]` (direct or indirect); the
/// number of entries left.
fn remove_from_array(doc: &mut Document, holder: ObjectId, key: &[u8], target: ObjectId) -> Option<usize> {
    let indirect = match doc.get_dictionary(holder).ok()?.get(key).ok()? {
        Object::Reference(r) => Some(*r),
        Object::Array(_) => None,
        _ => return None,
    };
    let arr = match indirect {
        Some(r) => match doc.get_object_mut(r).ok()? {
            Object::Array(a) => a,
            _ => return None,
        },
        None => match doc.get_dictionary_mut(holder).ok()?.get_mut(key).ok()? {
            Object::Array(a) => a,
            _ => return None,
        },
    };
    arr.retain(|o| !matches!(o, Object::Reference(r) if *r == target));
    Some(arr.len())
}

/// Remove `target` from the AcroForm's `/Fields`.
fn remove_from_fields(doc: &mut Document, target: ObjectId) {
    let Ok(Object::Reference(root)) = doc.trailer.get(b"Root").cloned() else { return };
    let form = match doc.get_dictionary(root).ok().and_then(|c| c.get(b"AcroForm").ok()) {
        Some(Object::Reference(r)) => Some(*r),
        Some(Object::Dictionary(_)) => None,
        _ => return,
    };
    if let Some(r) = form {
        remove_from_array(doc, r, b"Fields", target);
        return;
    }
    // the catalog's own AcroForm dictionary
    let fields_ref = match doc.get_dictionary(root).ok().and_then(|c| c.get(b"AcroForm").ok()) {
        Some(Object::Dictionary(f)) => match f.get(b"Fields") {
            Ok(Object::Reference(r)) => Some(*r),
            _ => None,
        },
        _ => return,
    };
    let arr = match fields_ref {
        Some(r) => match doc.get_object_mut(r) {
            Ok(Object::Array(a)) => a,
            _ => return,
        },
        None => match doc.get_dictionary_mut(root).ok().and_then(|c| c.get_mut(b"AcroForm").ok()) {
            Some(Object::Dictionary(f)) => match f.get_mut(b"Fields") {
                Ok(Object::Array(a)) => a,
                _ => return,
            },
            _ => return,
        },
    };
    arr.retain(|o| !matches!(o, Object::Reference(r) if *r == target));
}

/// Take widgets out of the field tree: out of their parent's `/Kids` (a
/// parent left with none is taken out the same way) or out of `/Fields`.
/// Returns the emptied parent fields.
fn remove_fields(doc: &mut Document, widgets: &[ObjectId]) -> Vec<ObjectId> {
    let mut emptied = Vec::new();
    let mut queue: Vec<ObjectId> = widgets.to_vec();
    let mut seen = HashSet::new();
    while let Some(w) = queue.pop() {
        if seen.len() > 100_000 || !seen.insert(w) {
            continue;
        }
        let parent = match doc.get_dictionary(w).ok().and_then(|d| d.get(b"Parent").ok()) {
            Some(Object::Reference(p)) => Some(*p),
            _ => None,
        };
        remove_from_fields(doc, w);
        if let Some(p) = parent {
            if remove_from_array(doc, p, b"Kids", w) == Some(0) {
                emptied.push(p);
                queue.push(p);
            }
        }
    }
    emptied
}

/// Every reference to an object in `gone` becomes null.
fn null_refs(doc: &mut Document, gone: &HashSet<ObjectId>) {
    let mut stack: Vec<&mut Object> = doc.objects.values_mut().collect();
    while let Some(o) = stack.pop() {
        match o {
            Object::Reference(r) if gone.contains(r) => *o = Object::Null,
            Object::Array(a) => stack.extend(a.iter_mut()),
            Object::Dictionary(d) => stack.extend(d.iter_mut().map(|(_, v)| v)),
            Object::Stream(s) => stack.extend(s.dict.iter_mut().map(|(_, v)| v)),
            _ => {}
        }
    }
    for (_, v) in doc.trailer.iter_mut() {
        if matches!(v, Object::Reference(r) if gone.contains(r)) {
            *v = Object::Null;
        }
    }
}

pub fn insert_page(doc: &mut Document, at: f64, size: Option<(f64, f64)>) -> R<()> {
    let ids = page_ids(doc);
    if ids.len() >= MAX_PAGES {
        return Err(format!("documents are limited to {MAX_PAGES} pages"));
    }
    let n = ids.len();
    if !at.is_finite() || at.fract() != 0.0 || at < 1.0 || at > n as f64 + 1.0 {
        return Err(format!("insert position {at} is outside 1..={}", n + 1));
    }
    let at = at as usize;
    let (w, h) = match size {
        Some(s) => s,
        None => match ids.get(at.saturating_sub(1).min(n.saturating_sub(1))) {
            Some(&p) => {
                let g = geom(doc, p);
                let b = g.visible_box();
                (b[2] - b[0], b[3] - b[1])
            }
            None => (612.0, 792.0),
        },
    };
    if !(1.0..=MAX_PAGE_SIDE).contains(&w) || !(1.0..=MAX_PAGE_SIDE).contains(&h) {
        return Err(format!("page sides must be 1..={MAX_PAGE_SIDE} points"));
    }
    let (parent, pos) = if at <= n {
        let neighbour = ids[at - 1];
        let parent = parent_of(doc, neighbour)?;
        let pos = kids_mut(doc, parent)?.iter().position(|k| matches!(k, Object::Reference(r) if *r == neighbour)).ok_or("broken page tree")?;
        (parent, pos)
    } else if let Some(&last) = ids.last() {
        let parent = parent_of(doc, last)?;
        let pos = kids_mut(doc, parent)?.iter().position(|k| matches!(k, Object::Reference(r) if *r == last)).ok_or("broken page tree")? + 1;
        (parent, pos)
    } else {
        let root = root_pages(doc)?;
        (root, kids_mut(doc, root)?.len())
    };
    let pid = crate::doc::blank_page(doc, parent, w, h);
    doc.get_dictionary_mut(pid).map_err(le)?.set("Rotate", 0);
    kids_mut(doc, parent)?.insert(pos, Object::Reference(pid));
    add_count(doc, parent, 1);
    Ok(())
}

pub fn rotate_page(doc: &mut Document, page: f64, degrees: f64) -> R<()> {
    if !degrees.is_finite() || degrees.rem_euclid(90.0) != 0.0 {
        return Err("rotation must be a multiple of 90 degrees".into());
    }
    let pid = page_id(doc, page)?;
    let cur = geom(doc, pid).rotate as i64;
    // reduced first: a huge multiple of 90 must not overflow the integer
    let turn = degrees.rem_euclid(360.0) as i64;
    let new = (cur + turn).rem_euclid(360);
    doc.get_dictionary_mut(pid).map_err(le)?.set("Rotate", new);
    Ok(())
}

fn root_pages(doc: &Document) -> R<ObjectId> {
    match doc.catalog().map_err(le)?.get(b"Pages") {
        Ok(Object::Reference(r)) => Ok(*r),
        _ => Err("the document has no page tree".into()),
    }
}

fn is_page(o: &Object) -> bool {
    matches!(o, Object::Dictionary(d) if d.get(b"Type").ok().and_then(|t| t.as_name().ok()) == Some(b"Page"))
}

fn rewrite_refs(o: &mut Object, f: &mut impl FnMut(ObjectId) -> Object) {
    match o {
        Object::Reference(r) => *o = f(*r),
        Object::Array(a) => a.iter_mut().for_each(|x| rewrite_refs(x, f)),
        Object::Dictionary(d) => d.iter_mut().for_each(|(_, x)| rewrite_refs(x, f)),
        Object::Stream(s) => s.dict.iter_mut().for_each(|(_, x)| rewrite_refs(x, f)),
        _ => {}
    }
}

/// Copy pages of `src` (and everything they use) to the end of `dst`'s page
/// tree. Inherited attributes are made explicit; links to pages that are not
/// copied become null; form fields come along into `dst`'s AcroForm.
pub fn import_pages(dst: &mut Document, src: &Document, pages: &[ObjectId]) -> R<usize> {
    if page_ids(dst).len() + pages.len() > MAX_PAGES {
        return Err(format!("documents are limited to {MAX_PAGES} pages"));
    }
    let root = root_pages(dst)?;
    let selected: HashSet<ObjectId> = pages.iter().copied().collect();
    let mut map: HashMap<ObjectId, ObjectId> = HashMap::new();
    let mut queue: VecDeque<ObjectId> = VecDeque::new();
    for p in pages {
        if !map.contains_key(p) {
            map.insert(*p, dst.new_object_id());
            queue.push_back(*p);
        }
    }
    // the copies go into `dst` only once all of them are made, so a copy that
    // fails leaves `dst` as it was
    let mut copies: Vec<(ObjectId, Object)> = Vec::new();
    while let Some(old) = queue.pop_front() {
        if copies.len() >= 5_000_000 {
            return Err("too many objects to copy".into());
        }
        let Ok(obj) = src.get_object(old) else { continue };
        let mut obj = obj.clone();
        if selected.contains(&old) {
            if let Object::Dictionary(d) = &mut obj {
                for key in [&b"MediaBox"[..], b"CropBox", b"Resources", b"Rotate"] {
                    if !d.has(key) {
                        if let Some(v) = inherited(src, old, key) {
                            d.set(key.to_vec(), v.clone());
                        }
                    }
                }
                d.remove(b"Parent");
            }
        }
        rewrite_refs(&mut obj, &mut |r| {
            if let Some(n) = map.get(&r) {
                return Object::Reference(*n);
            }
            match src.get_object(r) {
                Ok(t) if is_page(t) && !selected.contains(&r) => Object::Null,
                Ok(_) => {
                    let n = dst.new_object_id();
                    map.insert(r, n);
                    queue.push_back(r);
                    Object::Reference(n)
                }
                Err(_) => Object::Null,
            }
        });
        if selected.contains(&old) {
            if let Object::Dictionary(d) = &mut obj {
                d.set("Parent", root);
            }
        }
        let Some(&new_id) = map.get(&old) else { continue };
        copies.push((new_id, obj));
    }
    let new_pages: Vec<Object> = pages.iter().filter_map(|p| map.get(p)).map(|n| Object::Reference(*n)).collect();
    let n = new_pages.len();
    kids_mut(dst, root)?.extend(new_pages);
    dst.objects.extend(copies);
    add_count(dst, root, n as i64);
    // form fields whose widgets came along
    let src_fields = src.catalog().ok().and_then(|c| get_dict(src, c, b"AcroForm")).map(|f| ref_ids(src, f, b"Fields")).unwrap_or_default();
    let moved: Vec<Object> = src_fields.iter().filter_map(|f| map.get(f)).map(|n| Object::Reference(*n)).collect();
    if !moved.is_empty() {
        let src_form = src.catalog().ok().and_then(|c| get_dict(src, c, b"AcroForm")).cloned().unwrap_or_default();
        let form_ref = match dst.catalog().map_err(le)?.get(b"AcroForm") {
            Ok(Object::Reference(r)) => Some(*r),
            _ => None,
        };
        let form: &mut Dictionary = match form_ref {
            Some(r) => dst.get_dictionary_mut(r).map_err(le)?,
            None => {
                let cat = dst.catalog_mut().map_err(le)?;
                if !matches!(cat.get(b"AcroForm"), Ok(Object::Dictionary(_))) {
                    let mut d = Dictionary::new();
                    if let Ok(da) = src_form.get(b"DA") {
                        d.set("DA", da.clone());
                    }
                    cat.set("AcroForm", d);
                }
                match cat.get_mut(b"AcroForm") {
                    Ok(Object::Dictionary(d)) => d,
                    _ => return Err("internal: AcroForm".into()),
                }
            }
        };
        match form.get_mut(b"Fields") {
            Ok(Object::Array(a)) => a.extend(moved),
            _ => form.set("Fields", moved),
        }
    }
    dst.max_id = dst.objects.keys().map(|k| k.0).max().unwrap_or(0).max(dst.max_id);
    Ok(n)
}

/// Every page of `b` appended to `a`.
pub fn merge(a: &mut Document, b: &Document) -> R<usize> {
    let pages = page_ids(b);
    import_pages(a, b, &pages)
}

/// A new document with the given pages (1-based, any order, repeats allowed).
pub fn copy_pages(src: &Document, pages: &[f64]) -> R<Document> {
    let ids = page_ids(src);
    let mut sel = Vec::new();
    for p in pages {
        sel.push(ids[page_index(*p, ids.len())?]);
    }
    if sel.is_empty() {
        return Err("copy_pages needs at least one page".into());
    }
    let mut dst = crate::doc::new_document(612.0, 792.0, 0);
    // repeats: copy one at a time so each copy is its own page object
    let mut seen = HashSet::new();
    let mut run: Vec<ObjectId> = Vec::new();
    for id in sel {
        if !seen.insert(id) {
            import_pages(&mut dst, src, &run)?;
            run.clear();
            seen.clear();
            seen.insert(id);
        }
        run.push(id);
    }
    import_pages(&mut dst, src, &run)?;
    Ok(dst)
}

// ---------------------------------------------------------------- outline

/// `(title, page index, level)` in document order.
pub fn outline(doc: &Document) -> Vec<(String, Option<usize>, usize)> {
    let mut out = Vec::new();
    let Ok(cat) = doc.catalog() else { return out };
    let Some(root) = get_dict(doc, cat, b"Outlines") else { return out };
    let index: HashMap<ObjectId, usize> = page_ids(doc).into_iter().enumerate().map(|(i, p)| (p, i)).collect();
    let mut seen = HashSet::new();
    if let Ok(Object::Reference(first)) = root.get(b"First") {
        walk_outline(doc, *first, 0, &index, &mut seen, &mut out);
    }
    out
}

fn walk_outline(doc: &Document, first: ObjectId, level: usize, index: &HashMap<ObjectId, usize>, seen: &mut HashSet<ObjectId>, out: &mut Vec<(String, Option<usize>, usize)>) {
    let mut cur = Some(first);
    while let Some(id) = cur {
        if level > 32 || out.len() >= 10_000 || !seen.insert(id) {
            return;
        }
        let Ok(d) = doc.get_dictionary(id) else { return };
        let title = get_text(doc, d, b"Title").unwrap_or_default();
        let dest = d.get(b"Dest").ok().cloned().or_else(|| {
            let a = get_dict(doc, d, b"A")?;
            (get_name(doc, a, b"S") == Some(b"GoTo")).then(|| a.get(b"D").ok().cloned()).flatten()
        });
        let page = dest.and_then(|o| dest_page(doc, &o, index, 0));
        out.push((title, page, level));
        if let Ok(Object::Reference(c)) = d.get(b"First") {
            walk_outline(doc, *c, level + 1, index, seen, out);
        }
        cur = match d.get(b"Next") {
            Ok(Object::Reference(n)) => Some(*n),
            _ => None,
        };
    }
}

fn dest_page(doc: &Document, o: &Object, index: &HashMap<ObjectId, usize>, depth: u32) -> Option<usize> {
    if depth > 8 {
        return None;
    }
    match o {
        Object::Reference(r) => match doc.get_object(*r).ok()? {
            Object::Dictionary(d) if is_page(&Object::Dictionary(d.clone())) => index.get(r).copied(),
            other => dest_page(doc, other, index, depth + 1),
        },
        Object::Array(a) => match a.first()? {
            Object::Reference(p) => index.get(p).copied(),
            Object::Integer(n) => usize::try_from(*n).ok(),
            _ => None,
        },
        Object::Dictionary(d) => dest_page(doc, d.get(b"D").ok()?, index, depth + 1),
        Object::Name(n) | Object::String(n, _) => {
            let target = named_dest(doc, n)?;
            dest_page(doc, &target, index, depth + 1)
        }
        _ => None,
    }
}

fn named_dest(doc: &Document, key: &[u8]) -> Option<Object> {
    let cat = doc.catalog().ok()?;
    if let Some(dests) = get_dict(doc, cat, b"Dests") {
        if let Ok(v) = dests.get(key) {
            return Some(deref(doc, v).clone());
        }
    }
    let tree = get_dict(doc, cat, b"Names").and_then(|n| get_dict(doc, n, b"Dests"))?;
    name_tree(doc, tree, key, 0)
}

fn name_tree(doc: &Document, node: &Dictionary, key: &[u8], depth: u32) -> Option<Object> {
    if depth > 32 {
        return None;
    }
    if let Some(names) = get_array(doc, node, b"Names") {
        for pair in names.chunks(2) {
            if let (Object::String(k, _), Some(v)) = (deref(doc, &pair[0]), pair.get(1)) {
                if k.as_slice() == key {
                    return Some(deref(doc, v).clone());
                }
            }
        }
    }
    for kid in get_array(doc, node, b"Kids").into_iter().flatten() {
        if let Object::Dictionary(k) = deref(doc, kid) {
            if let Some(v) = name_tree(doc, k, key, depth + 1) {
                return Some(v);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_parse_and_repeat_commands() {
        let p = parse_path(&[Data::str("M"), Data::Num(0.0), Data::Num(0.0), Data::str("L"), Data::Num(10.0), Data::Num(0.0), Data::Num(10.0), Data::Num(10.0), Data::str("Z")]).unwrap();
        assert_eq!(p, vec![Seg::M(0.0, 0.0), Seg::L(10.0, 0.0), Seg::L(10.0, 10.0), Seg::Z]);
        assert!(parse_path(&[Data::Num(1.0)]).is_err());
        assert!(parse_path(&[Data::str("L"), Data::Num(1.0), Data::Num(1.0)]).is_err());
        assert!(parse_path(&[Data::str("M"), Data::Num(1.0)]).is_err());
        assert!(parse_path(&[Data::str("X")]).is_err());
        let q = parse_path(&[Data::str("M"), Data::Num(0.0), Data::Num(0.0), Data::str("Q"), Data::Num(3.0), Data::Num(3.0), Data::Num(6.0), Data::Num(0.0)]).unwrap();
        assert_eq!(q[1], Seg::C(2.0, 2.0, 4.0, 2.0, 6.0, 0.0));
    }

    #[test]
    fn shapes_accept_every_shared_form() {
        let flat = Data::list(vec![Data::str("M"), Data::Num(0.0), Data::Num(0.0), Data::str("L"), Data::Num(5.0), Data::Num(5.0)]);
        let one = shapes(&flat, &Data::map([("stroke", Data::Num(0xFF0000FFu32 as f64))])).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].1.stroke, Some(0xFF0000FFu32 as f64));
        let styled = Data::map([("path", flat.clone()), ("width", Data::Num(3.0))]);
        let list = Data::list(vec![styled.clone(), styled]);
        let many = shapes(&list, &Data::Nil).unwrap();
        assert_eq!(many.len(), 2);
        assert_eq!(many[1].1.width, 3.0);
        // a Luau table mixing the path with style keys arrives as a map with numeric keys
        let mixed = Data::map([("1", Data::str("M")), ("2", Data::Num(1.0)), ("3", Data::Num(2.0)), ("fill", Data::Num(255.0))]);
        let m = shapes(&mixed, &Data::Nil).unwrap();
        assert_eq!(m[0].0, vec![Seg::M(1.0, 2.0)]);
        assert_eq!(m[0].1.fill, Some(255.0));
    }

    #[test]
    fn jpeg_headers() {
        // SOI, APP14 "Adobe", SOF0 8-bit 20x10, 3 components
        let mut b = vec![0xFF, 0xD8, 0xFF, 0xEE, 0x00, 0x07, b'A', b'd', b'o', b'b', b'e'];
        b.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08, 0x00, 0x0A, 0x00, 0x14, 0x03]);
        assert_eq!(jpeg_info(&b), Some((20, 10, 3, true)));
        assert_eq!(jpeg_info(b"\x89PNG"), None);
    }

    #[test]
    fn default_appearance_strings() {
        assert_eq!(parse_da(Some("/Helv 9 Tf 0 g")), (9.0, [0.0, 0.0, 0.0, 1.0]));
        assert_eq!(parse_da(Some("1 0 0 rg /F1 0 Tf")), (0.0, [1.0, 0.0, 0.0, 1.0]));
        assert_eq!(parse_da(None).0, 0.0);
    }
}
