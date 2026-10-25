//! The drawing resource (`cad.drawing`): the parsed DXF, its hatches, and
//! the current scene snapshot. Edits lock, change the drawing and publish a
//! new scene; tile workers render from the snapshot they took.

use std::collections::{HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex, MutexGuard};

use anypage_runtime::{Data, Res};
use dxf::entities::{Entity, EntityType};
use dxf::enums::AcadVersion;
use dxf::{Drawing, Handle};

use crate::geom::{ocs, Aff, P};
use crate::hatch::{scan, write_hatch, Hatch, RawItem};
use crate::scene::{build, Anchored, Builder, Group, Scene};

/// Largest DXF accepted. The `dxf` crate holds a parsed file in about 20
/// times its size and peaks near 50 times while parsing (measured with
/// `examples/memory.rs`: 5.4 MB → 103 MB live, 262 MB peak), so phones get
/// a lower limit.
#[cfg(target_os = "android")]
pub const MAX_DXF_BYTES: usize = 32 << 20;
#[cfg(not(target_os = "android"))]
pub const MAX_DXF_BYTES: usize = 256 << 20;
const AUTO_HANDLE: Handle = Handle(0xFFFF_FFFF_FFFF_FFFF);
const R12_HATCH_WARNING: &str = "this R12 drawing has hatches: it will be saved as AutoCAD 2000 DXF (HATCH needs R13 or later)";

/// A new entity for [`State::add_entities`].
pub enum NewEnt {
    Dxf(Box<Entity>),
    Hatch(Box<Hatch>),
}

pub struct State {
    pub dwg: Drawing,
    /// Model-space hatches, each after the entity it followed.
    pub hatches: Anchored,
    /// Block hatches by upper-case block name.
    pub block_hatches: HashMap<String, Anchored>,
    pub scene: Arc<Scene>,
    pub last_ids: Vec<u64>,
    pub warnings: Vec<String>,
    pub encoding: &'static str,
    /// Open phases in ms: dxf parse, hatch scan, fixups, scene build.
    pub timings: [f64; 4],
    /// The newest version handed out for this drawing's resource; edits
    /// return a handle at a newer one, whatever handle they were given.
    pub version: u64,
    /// An empty drawing waiting for a `cad.load` job to fill it: edits are
    /// refused until then. `Some(error)` when that load failed.
    pub pending: bool,
    pub load_error: Option<String>,
    /// Entity position by handle; checked on use, rebuilt when stale.
    entity_pos: HashMap<u64, usize>,
}

pub struct CadDoc {
    state: Mutex<State>,
}

/// Header values read from the raw text before parsing.
fn header_value(bytes: &[u8], var: &str) -> Option<String> {
    let limit = bytes.len().min(1 << 20);
    let hay = &bytes[..limit];
    let pos = hay.windows(var.len()).position(|w| w == var.as_bytes())?;
    let rest = &hay[pos + var.len()..];
    // the next pair: code line, then value line
    let mut lines = rest.split(|&b| b == b'\n').skip(1);
    let _code = lines.next()?;
    let value = lines.next()?;
    Some(String::from_utf8_lossy(value).trim().to_string())
}

fn codepage_encoding(cp: &str) -> Option<&'static encoding_rs::Encoding> {
    let cp = cp.to_ascii_uppercase();
    let n = cp.trim_start_matches("ANSI_").trim_start_matches("DOS");
    Some(match n {
        "874" => encoding_rs::WINDOWS_874,
        "932" => encoding_rs::SHIFT_JIS,
        "936" => encoding_rs::GBK,
        "949" => encoding_rs::EUC_KR,
        "950" => encoding_rs::BIG5,
        "1250" => encoding_rs::WINDOWS_1250,
        "1251" => encoding_rs::WINDOWS_1251,
        "1252" => encoding_rs::WINDOWS_1252,
        "1253" => encoding_rs::WINDOWS_1253,
        "1254" => encoding_rs::WINDOWS_1254,
        "1255" => encoding_rs::WINDOWS_1255,
        "1256" => encoding_rs::WINDOWS_1256,
        "1257" => encoding_rs::WINDOWS_1257,
        "1258" => encoding_rs::WINDOWS_1258,
        _ => return None,
    })
}

/// `\U+XXXX` escapes in legacy files.
fn unescape(s: &str) -> String {
    if !s.contains("\\U+") {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("\\U+") {
        out.push_str(&rest[..i]);
        let hex = rest.get(i + 3..i + 7).filter(|h| h.chars().all(|c| c.is_ascii_hexdigit()));
        match hex.and_then(|h| u32::from_str_radix(h, 16).ok()).and_then(char::from_u32) {
            Some(c) => {
                out.push(c);
                rest = &rest[i + 7..];
            }
            None => {
                out.push_str("\\U+");
                rest = &rest[i + 3..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn max_handle(dwg: &Drawing, hatches: &Anchored, block_hatches: &HashMap<String, Anchored>) -> u64 {
    let mut m = 0u64;
    let mut see = |h: Handle| {
        if h.0 != AUTO_HANDLE.0 {
            m = m.max(h.0);
        }
    };
    let ent = |e: &Entity, see: &mut dyn FnMut(Handle)| {
        see(e.common.handle);
        match &e.specific {
            EntityType::Insert(i) => {
                see(i.__seqend_handle);
                i.__attributes_and_handles.iter().for_each(|a| see(a.1));
            }
            EntityType::Polyline(pl) => {
                see(pl.__seqend_handle);
                pl.__vertices_and_handles.iter().for_each(|v| see(v.1));
            }
            _ => {}
        }
    };
    for e in dwg.entities() {
        ent(e, &mut see);
    }
    for b in dwg.blocks() {
        see(b.handle);
        for e in &b.entities {
            ent(e, &mut see);
        }
    }
    dwg.layers().for_each(|x| see(x.handle));
    dwg.line_types().for_each(|x| see(x.handle));
    dwg.styles().for_each(|x| see(x.handle));
    dwg.dim_styles().for_each(|x| see(x.handle));
    dwg.block_records().for_each(|x| see(x.handle));
    dwg.app_ids().for_each(|x| see(x.handle));
    dwg.views().for_each(|x| see(x.handle));
    dwg.ucss().for_each(|x| see(x.handle));
    dwg.view_ports().for_each(|x| see(x.handle));
    dwg.objects().for_each(|o| see(o.common.handle));
    for (_, h) in hatches.iter().chain(block_hatches.values().flatten()) {
        see(Handle(h.handle));
    }
    m
}

fn anchors(items: Vec<RawItem>, known: &HashSet<u64>) -> Anchored {
    let mut out = Vec::new();
    let mut prev: Option<u64> = None;
    for it in items {
        match it {
            RawItem::Entity(h) => {
                if known.contains(&h) {
                    prev = Some(h);
                }
            }
            RawItem::Hatch(h) => out.push((prev, *h)),
        }
    }
    out
}

impl CadDoc {
    /// Parse DXF bytes (ASCII or binary).
    pub fn open(bytes: &[u8]) -> Result<CadDoc, String> {
        Self::open_with(bytes, &|_| {}, &|| false)
    }

    /// [`CadDoc::open`] reporting progress (0..1) and stopping when
    /// `cancelled` says so (checked after the `dxf` crate's parse, which
    /// cannot be interrupted, and during the scene build).
    pub fn open_with(bytes: &[u8], progress: &dyn Fn(f32), cancelled: &dyn Fn() -> bool) -> Result<CadDoc, String> {
        Ok(CadDoc { state: Mutex::new(Self::load_state(bytes, progress, cancelled)?) })
    }

    fn load_state(bytes: &[u8], progress: &dyn Fn(f32), cancelled: &dyn Fn() -> bool) -> Result<State, String> {
        if bytes.len() > MAX_DXF_BYTES {
            return Err(format!("DXF larger than {} MB", MAX_DXF_BYTES >> 20));
        }
        if bytes.iter().take(64).all(|b| b.is_ascii_whitespace()) {
            return Err("empty DXF".into());
        }
        let ver = header_value(bytes, "$ACADVER").unwrap_or_default();
        let utf8 = ver.as_str() >= "AC1021" && ver.starts_with("AC");
        let legacy = if utf8 { None } else { header_value(bytes, "$DWGCODEPAGE").and_then(|cp| codepage_encoding(&cp)) };
        let enc = legacy.unwrap_or(encoding_rs::WINDOWS_1252);
        progress(0.05);
        let t0 = std::time::Instant::now();
        let ms = |t: std::time::Instant| t.elapsed().as_secs_f64() * 1000.0;
        let load = |enc: &'static encoding_rs::Encoding| {
            catch_unwind(AssertUnwindSafe(|| Drawing::load_with_encoding(&mut &bytes[..], enc)))
                .map_err(|_| "the DXF parser failed on this file (malformed data)".to_string())?
                .map_err(|e| format!("not a readable DXF: {e}"))
        };
        let mut used = enc;
        let mut dwg = match load(enc) {
            Ok(d) => d,
            Err(e) if enc != encoding_rs::WINDOWS_1252 => {
                used = encoding_rs::WINDOWS_1252;
                load(used).map_err(|_| e)?
            }
            Err(e) => return Err(e),
        };
        let decode = move |v: &[u8]| -> String {
            if utf8 {
                unescape(&String::from_utf8_lossy(v))
            } else {
                unescape(&used.decode(v).0)
            }
        };
        let parse_ms = ms(t0);
        if cancelled() {
            return Err("cancelled".into());
        }
        progress(0.6);
        let t1 = std::time::Instant::now();
        // the scan only matters when there are hatches to find
        let has_hatch = bytes.windows(5).any(|w| w == b"HATCH");
        let raw = if has_hatch { catch_unwind(AssertUnwindSafe(|| scan(bytes, &decode))).unwrap_or_default() } else { Default::default() };
        let scan_ms = ms(t1);
        let t2 = std::time::Instant::now();
        let mut warnings = raw.warnings;
        // before anything indexes entities by position: this removes some
        let strays = fix_attribute_strays(&mut dwg);
        if strays > 0 {
            warnings.push(format!("{strays} attribute parts re-attached to their INSERTs"));
        }
        // unique top-level handles
        let mut seen = HashSet::new();
        let mut dup = HashSet::new();
        for (i, e) in dwg.entities().enumerate() {
            if !seen.insert(e.common.handle.0) {
                dup.insert(i);
            }
        }
        let known: HashSet<u64> = seen.clone();
        let hatches = anchors(raw.model, &known);
        let mut block_hatches: HashMap<String, Anchored> = HashMap::new();
        for (name, items) in raw.blocks {
            let known: HashSet<u64> = dwg.blocks().find(|b| b.name.eq_ignore_ascii_case(&name)).map(|b| b.entities.iter().map(|e| e.common.handle.0).collect()).unwrap_or_default();
            let hs = anchors(items, &known);
            if !hs.is_empty() {
                block_hatches.entry(name.to_uppercase()).or_default().extend(hs);
            }
        }
        let mut next = max_handle(&dwg, &hatches, &block_hatches) + 1;
        next = assign_auto_handles(&mut dwg, next);
        if !dup.is_empty() {
            warnings.push(format!("{} entities had duplicate handles and were given new ones", dup.len()));
            for (i, e) in dwg.entities_mut().enumerate() {
                if dup.contains(&i) {
                    e.common.handle = Handle(next);
                    next += 1;
                }
            }
        }
        let mut hatches = hatches;
        // hatches without a handle, or colliding with an entity, get new ones
        let mut used_ids: HashSet<u64> = dwg.entities().map(|e| e.common.handle.0).collect();
        for (_, h) in hatches.iter_mut() {
            if h.handle == 0 || !used_ids.insert(h.handle) {
                h.handle = next;
                next += 1;
                used_ids.insert(h.handle);
            }
        }
        if dwg.header.next_available_handle.0 < next {
            dwg.header.next_available_handle = Handle(next);
        }
        let any_hatch = !hatches.is_empty() || block_hatches.values().any(|v| !v.is_empty());
        if any_hatch && dwg.header.version < AcadVersion::R13 {
            warnings.push(R12_HATCH_WARNING.into());
        }
        let fix_ms = ms(t2);
        progress(0.7);
        let t3 = std::time::Instant::now();
        let font = crate::font::global();
        let scene = build(&dwg, &hatches, &block_hatches, font.as_deref(), cancelled).ok_or("cancelled")?;
        warnings.extend(scene.warnings.iter().cloned());
        let encoding = if utf8 { "UTF-8" } else { used.name() };
        let timings = [parse_ms, scan_ms, fix_ms, ms(t3)];
        log::debug!(target: "anypage_cad", "open {} bytes: parse {:.0} ms, hatch scan {:.0} ms, fixups {:.0} ms, scene {:.0} ms", bytes.len(), timings[0], timings[1], timings[2], timings[3]);
        progress(1.0);
        Ok(State {
            dwg,
            hatches,
            block_hatches,
            scene: Arc::new(scene),
            last_ids: Vec::new(),
            warnings,
            encoding,
            timings,
            version: 0,
            pending: false,
            load_error: None,
            entity_pos: HashMap::new(),
        })
    }

    fn empty_state() -> State {
        let mut dwg = Drawing::new();
        dwg.header.version = AcadVersion::R2010;
        let scene = build(&dwg, &Vec::new(), &HashMap::new(), None, &|| false).unwrap_or_default();
        State {
            dwg,
            hatches: Vec::new(),
            block_hatches: HashMap::new(),
            scene: Arc::new(scene),
            last_ids: Vec::new(),
            warnings: Vec::new(),
            encoding: "UTF-8",
            timings: [0.0; 4],
            version: 0,
            pending: false,
            load_error: None,
            entity_pos: HashMap::new(),
        }
    }

    /// An empty drawing (AutoCAD 2010 format: UTF-8 text, LWPOLYLINE, HATCH).
    pub fn new_empty() -> CadDoc {
        CadDoc { state: Mutex::new(Self::empty_state()) }
    }

    /// An empty drawing that a `cad.load` job fills (`{bytes, into}`), so a
    /// handler can hold the drawing while it loads off the UI thread.
    pub fn new_pending() -> CadDoc {
        let mut st = Self::empty_state();
        st.pending = true;
        CadDoc { state: Mutex::new(st) }
    }

    pub fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn scene(&self) -> Arc<Scene> {
        self.lock().scene.clone()
    }

    /// How long opening took, in ms: dxf parse, hatch scan, fixups, scene build.
    pub fn load_timings(&self) -> [f64; 4] {
        self.lock().timings
    }

    /// Fill a pending drawing from DXF bytes (the `cad.load` job). The parse
    /// runs without the lock; a drawing that is not pending (already
    /// filled, or never pending) is left alone, so running the job again
    /// never overwrites edits. Returns the version to hand out.
    pub fn fill(&self, bytes: &[u8], progress: &dyn Fn(f32), cancelled: &dyn Fn() -> bool) -> Result<u64, String> {
        {
            let st = self.lock();
            if !st.pending {
                return Ok(st.version);
            }
        }
        let loaded = catch_unwind(AssertUnwindSafe(|| Self::load_state(bytes, progress, cancelled))).unwrap_or_else(|_| Err("internal error while reading DXF".into()));
        let mut st = self.lock();
        if !st.pending {
            return Ok(st.version);
        }
        let version = st.version + 1;
        match loaded {
            Ok(mut fresh) => {
                fresh.version = version;
                *st = fresh;
                Ok(version)
            }
            Err(e) if e == "cancelled" => Err(e),
            Err(e) => {
                st.pending = false;
                st.load_error = Some(e.clone());
                st.version = version;
                Err(e)
            }
        }
    }

    /// Run an edit with panics contained and return the drawing at its next
    /// version. The version comes from a counter kept here, so edits made
    /// through an older handle still produce a newer value. The state stays
    /// consistent because every edit rebuilds what it touched.
    pub fn edit<R>(&self, r: &Res, f: impl FnOnce(&mut State) -> Result<R, String>) -> Result<(Data, R), String> {
        let mut st = self.lock();
        if st.pending {
            return Err("the drawing is still loading".into());
        }
        st.entity_pos.clear();
        let out = catch_unwind(AssertUnwindSafe(|| f(&mut st))).unwrap_or_else(|_| Err("internal error while editing the drawing".into()))?;
        st.version = st.version.max(r.version()) + 1;
        Ok((Data::Res(at_version(r, st.version)), out))
    }

    /// DXF bytes (ASCII), hatches included. Leaves the drawing as it is.
    pub fn save(&self) -> Result<Vec<u8>, String> {
        let mut st = self.lock();
        if st.pending {
            return Err("the drawing is still loading".into());
        }
        catch_unwind(AssertUnwindSafe(|| st.save())).unwrap_or_else(|_| Err("internal error while writing DXF".into()))
    }
}

/// `r` at version `v` (>= its own): resources only step forward.
pub fn at_version(r: &Res, v: u64) -> Res {
    let mut cur = r.next_version();
    while cur.version() < v {
        cur = cur.next_version();
    }
    cur
}

// ---------------------------------------------------------------- saving

fn split_pairs(text: &str) -> Vec<(&str, &str)> {
    let lines: Vec<&str> = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
    lines.as_chunks::<2>().0.iter().map(|[c, v]| (c.trim(), *v)).collect()
}

/// For one container's pairs (from just after its header to its end),
/// the pair index after each top-level entity, by handle.
fn insertion_points(pairs: &[(&str, &str)], start: usize, end: usize) -> (HashMap<u64, usize>, usize) {
    let mut after: HashMap<u64, usize> = HashMap::new();
    let first = (start..end).find(|&i| pairs[i].0 == "0").unwrap_or(end);
    let mut i = first;
    while i < end {
        // pairs[i] is 0/<type> of a top-level entity
        let mut handle = None;
        let mut j = i + 1;
        loop {
            while j < end && pairs[j].0 != "0" {
                if pairs[j].0 == "5" && handle.is_none() {
                    handle = u64::from_str_radix(pairs[j].1.trim(), 16).ok();
                }
                j += 1;
            }
            // sub-entities belong to the entity before them
            if j < end && matches!(pairs[j].1.trim(), "VERTEX" | "SEQEND" | "ATTRIB") {
                j += 1;
                continue;
            }
            break;
        }
        if let Some(h) = handle {
            after.insert(h, j);
        }
        i = j;
    }
    (after, first)
}

/// The dxf crate writes an `AcDbXrecord` tail (multiline-attribute data)
/// into every ATTRIB / ATTDEF of 2007+ files, which its own reader then
/// reads as stray entities. The tail is optional: drop it.
///
/// It also writes an embedded MTEXT entity (handle 0) after each of them,
/// which its INSERT reader does not expect: those are dropped too (the
/// attribute's own value carries the text).
fn drop_attribute_xrecords<'a>(pairs: Vec<(&'a str, &'a str)>) -> Vec<(&'a str, &'a str)> {
    let mut out = Vec::with_capacity(pairs.len());
    let mut in_attr = false;
    let mut skipping = false;
    let mut i = 0;
    while i < pairs.len() {
        let (c, v) = pairs[i];
        if c == "0" {
            let after_attr = in_attr;
            in_attr = matches!(v.trim(), "ATTRIB" | "ATTDEF");
            skipping = false;
            if after_attr && v.trim() == "MTEXT" && pairs.get(i + 1).is_some_and(|p| p.0 == "5" && p.1.trim() == "0") {
                // skip the embedded MTEXT up to the next entity
                i += 1;
                while i < pairs.len() && pairs[i].0 != "0" {
                    i += 1;
                }
                in_attr = false;
                continue;
            }
        } else if in_attr && c == "100" && v.trim() == "AcDbXrecord" {
            skipping = true;
        }
        if !skipping {
            out.push((c, v));
        }
        i += 1;
    }
    out
}

/// Sub-entities (ATTRIB, VERTEX, SEQEND) the dxf crate leaves with its
/// placeholder handle get real ones, starting at `next`. Returns the next free.
fn assign_auto_handles(dwg: &mut Drawing, mut next: u64) -> u64 {
    let mut take = |h: &mut Handle| {
        if h.0 == AUTO_HANDLE.0 || h.0 == 0 {
            *h = Handle(next);
            next += 1;
        }
    };
    for e in dwg.entities_mut() {
        match &mut e.specific {
            EntityType::Insert(ins) if !ins.__attributes_and_handles.is_empty() => {
                ins.__attributes_and_handles.iter_mut().for_each(|a| take(&mut a.1));
                take(&mut ins.__seqend_handle);
            }
            EntityType::Polyline(pl) => {
                pl.__vertices_and_handles.iter_mut().for_each(|v| take(&mut v.1));
                take(&mut pl.__seqend_handle);
            }
            _ => {}
        }
    }
    next
}

/// Repair what the dxf crate's reader leaves at top level when an INSERT's
/// attributes carry embedded MTEXT (2010+ files): stray ATTRIBs go back to
/// their INSERT, embedded MTEXT and SEQEND markers are dropped.
fn fix_attribute_strays(dwg: &mut Drawing) -> usize {
    let records: HashSet<u64> = dwg.block_records().map(|r| r.handle.0).collect();
    let mut remove: Vec<usize> = Vec::new();
    let mut moves: Vec<(usize, usize)> = Vec::new(); // (attrib index, insert index)
    let mut last_insert: Option<usize> = None;
    for (i, e) in dwg.entities().enumerate() {
        match &e.specific {
            EntityType::Insert(_) => last_insert = Some(i),
            EntityType::Seqend(_) => {
                remove.push(i);
                last_insert = None;
            }
            EntityType::MText(_) if e.common.__owner_handle.0 != 0 && !records.contains(&e.common.__owner_handle.0) && !records.is_empty() => {
                remove.push(i);
            }
            EntityType::Attribute(_) => {
                if let Some(k) = last_insert {
                    moves.push((i, k));
                    remove.push(i);
                }
            }
            _ => last_insert = None,
        }
    }
    if remove.is_empty() {
        return 0;
    }
    for (a, k) in &moves {
        let att = match dwg.entities().nth(*a).map(|e| &e.specific) {
            Some(EntityType::Attribute(att)) => att.clone(),
            _ => continue,
        };
        if let Some(EntityType::Insert(ins)) = dwg.entities_mut().nth(*k).map(|e| &mut e.specific) {
            ins.__has_attributes = true;
            ins.__attributes_and_handles.push((att, AUTO_HANDLE));
        }
    }
    let remove: HashSet<usize> = remove.into_iter().collect();
    retain_entities(dwg, |i, _| !remove.contains(&i))
}

impl State {
    fn owner_of(&self, block_name: &str) -> Option<u64> {
        self.dwg.block_records().find(|r| r.name.eq_ignore_ascii_case(block_name)).map(|r| r.handle.0).filter(|h| *h != 0)
    }

    /// DXF text of the drawing. The drawing itself is not changed: an R12
    /// drawing with hatches is written as R2000 by switching the version
    /// for the write only (the warning about it is given when the hatches
    /// arrive, on load or `cad.add`). Sub-entity handles are assigned when
    /// entities are loaded or added, so there is nothing to assign here.
    pub fn save(&mut self) -> Result<Vec<u8>, String> {
        let any_hatch = !self.hatches.is_empty() || self.block_hatches.values().any(|v| !v.is_empty());
        let version = self.dwg.header.version;
        let write_version = if any_hatch && version < AcadVersion::R13 { AcadVersion::R2000 } else { version };
        if has_auto_handles(&self.dwg) {
            // not expected: every path that adds sub-entities assigns them
            let start = (max_handle(&self.dwg, &self.hatches, &self.block_hatches) + 1).max(self.dwg.header.next_available_handle.0);
            let next = assign_auto_handles(&mut self.dwg, start);
            self.dwg.header.next_available_handle = Handle(next.max(self.dwg.header.next_available_handle.0));
            log::warn!(target: "anypage_cad", "save: assigned handles to sub-entities that had none");
        }
        let mut buf = Vec::new();
        self.dwg.header.version = write_version;
        let written = catch_unwind(AssertUnwindSafe(|| self.dwg.save(&mut buf)));
        self.dwg.header.version = version;
        written.map_err(|_| "internal error while writing DXF".to_string())?.map_err(|e| format!("writing DXF: {e}"))?;
        let xrecord_tails = write_version >= AcadVersion::R2007;
        if !any_hatch && !xrecord_tails {
            return Ok(buf);
        }
        let ascii_only = write_version <= AcadVersion::R2004;
        let text = String::from_utf8(buf).map_err(|_| "the DXF writer produced invalid text".to_string())?;
        let mut pairs = split_pairs(&text);
        if xrecord_tails {
            pairs = drop_attribute_xrecords(pairs);
        }
        // where to insert what: pair index -> text
        let mut inserts: Vec<(usize, String)> = Vec::new();
        let mut i = 0;
        let mut section = "";
        let model_owner = self.owner_of("*Model_Space");
        while i < pairs.len() {
            let (c, v) = pairs[i];
            if c == "0" && v.trim() == "SECTION" {
                section = pairs.get(i + 1).map(|p| p.1.trim()).unwrap_or("");
                if section == "ENTITIES" {
                    let start = i + 2;
                    let end = (start..pairs.len()).find(|&k| pairs[k].0 == "0" && pairs[k].1.trim() == "ENDSEC").unwrap_or(pairs.len());
                    let (after, first) = insertion_points(&pairs, start, end);
                    for (anchor, h) in &self.hatches {
                        let at = anchor.and_then(|a| after.get(&a).copied()).unwrap_or(match anchor {
                            None => first,
                            Some(_) => end,
                        });
                        inserts.push((at, write_hatch(h, model_owner, ascii_only)));
                    }
                    i = end;
                    continue;
                }
            }
            if section == "BLOCKS" && c == "0" && v.trim() == "BLOCK" {
                let name_idx = (i + 1..pairs.len()).take_while(|&k| pairs[k].0 != "0").find(|&k| pairs[k].0 == "2");
                let name = name_idx.map(|k| pairs[k].1.trim().to_uppercase()).unwrap_or_default();
                let end = (i + 1..pairs.len()).find(|&k| pairs[k].0 == "0" && pairs[k].1.trim() == "ENDBLK").unwrap_or(pairs.len());
                if let Some(hs) = self.block_hatches.get(&name) {
                    let start = (i + 1..end).find(|&k| pairs[k].0 == "0").unwrap_or(end);
                    let (after, first) = insertion_points(&pairs, start, end);
                    let owner = self.owner_of(&name);
                    for (anchor, h) in hs {
                        let at = anchor.and_then(|a| after.get(&a).copied()).unwrap_or(match anchor {
                            None => first,
                            Some(_) => end,
                        });
                        inserts.push((at, write_hatch(h, owner, ascii_only)));
                    }
                }
                i = end;
                continue;
            }
            i += 1;
        }
        // stable: hatches at the same place keep their order
        inserts.sort_by_key(|x| x.0);
        let mut out = String::with_capacity(text.len() + inserts.iter().map(|x| x.1.len()).sum::<usize>());
        let mut k = 0;
        for (idx, (c, v)) in pairs.iter().enumerate() {
            while k < inserts.len() && inserts[k].0 == idx {
                out.push_str(&inserts[k].1);
                k += 1;
            }
            out.push_str(&format!("{c:>3}\r\n{v}\r\n"));
        }
        while k < inserts.len() {
            out.push_str(&inserts[k].1);
            k += 1;
        }
        Ok(out.into_bytes())
    }

    // ---------------------------------------------------------------- edits

    fn next_handle(&mut self) -> u64 {
        let n = self.dwg.header.next_available_handle.0.max(1);
        self.dwg.header.next_available_handle = Handle(n + 1);
        n
    }

    /// Position of the top-level entity with handle `id`. The index is
    /// built on first use after an edit ([`CadDoc::edit`] clears it) and
    /// checked on every hit.
    pub fn entity_index(&mut self, id: u64) -> Option<usize> {
        if self.entity_pos.is_empty() {
            self.entity_pos = self.dwg.entities().enumerate().map(|(i, e)| (e.common.handle.0, i)).collect();
        }
        let i = *self.entity_pos.get(&id)?;
        if self.dwg.entities().nth(i).is_some_and(|e| e.common.handle.0 == id) {
            return Some(i);
        }
        // stale: rebuild once
        self.entity_pos = self.dwg.entities().enumerate().map(|(i, e)| (e.common.handle.0, i)).collect();
        self.entity_pos.get(&id).copied()
    }

    /// Ids in drawing order: entities, each followed by the hatches that
    /// came after it in the file (as [`build`] orders groups).
    fn drawing_order(&self) -> Vec<u64> {
        let mut by_anchor: HashMap<Option<u64>, Vec<u64>> = HashMap::new();
        for (a, h) in &self.hatches {
            by_anchor.entry(*a).or_default().push(h.handle);
        }
        let mut out = by_anchor.remove(&None).unwrap_or_default();
        for e in self.dwg.entities() {
            out.push(e.common.handle.0);
            if let Some(hs) = by_anchor.remove(&Some(e.common.handle.0)) {
                out.extend(hs);
            }
        }
        let mut rest: Vec<(Option<u64>, Vec<u64>)> = by_anchor.into_iter().collect();
        rest.sort_by_key(|(a, _)| *a);
        out.extend(rest.into_iter().flat_map(|(_, hs)| hs));
        out
    }

    /// Recompute the groups of `ids` (missing ones are dropped, new ones
    /// placed in drawing order). One pass over the drawing whatever the
    /// number of ids.
    pub fn refresh(&mut self, ids: &HashSet<u64>) {
        self.refresh_blocks(ids, &[]);
    }

    /// [`State::refresh`], first dropping the built geometry of `blocks`
    /// (upper-case names) whose definitions changed.
    pub fn refresh_blocks(&mut self, ids: &HashSet<u64>, blocks: &[String]) {
        if ids.is_empty() && blocks.is_empty() {
            return;
        }
        let mut new_groups: HashMap<u64, Group> = HashMap::new();
        let font = crate::font::global();
        let mut known = self.scene.blocks.clone();
        for b in blocks {
            known.remove(b);
        }
        let (layers, memo, warnings) = {
            // start from the scene's layer list so existing items keep their indices
            let mut b = Builder::with_layers(&self.dwg, &self.block_hatches, font.as_deref(), &self.scene.layers, &known);
            for e in self.dwg.entities() {
                if ids.contains(&e.common.handle.0) {
                    if let Some(g) = b.entity_group(e) {
                        new_groups.insert(g.id, g);
                    }
                }
            }
            for (_, h) in &self.hatches {
                if ids.contains(&h.handle) {
                    if let Some(g) = b.hatch_group(h) {
                        new_groups.insert(g.id, g);
                    }
                }
            }
            let w = b.take_warnings();
            (b.layers, b.memo, w)
        };
        let mut scene = (*self.scene).clone();
        scene.layers = layers;
        scene.blocks = memo;
        for w in warnings {
            if !scene.warnings.contains(&w) {
                scene.warnings.push(w);
            }
        }
        scene.groups.retain_mut(|g| {
            if !ids.contains(&g.id) {
                return true;
            }
            match new_groups.remove(&g.id) {
                Some(n) => {
                    *g = Arc::new(n);
                    true
                }
                None => false,
            }
        });
        // new entities (not in the scene yet): at the end, in drawing order
        if !new_groups.is_empty() {
            for id in self.drawing_order() {
                if let Some(g) = new_groups.remove(&id) {
                    scene.groups.push(Arc::new(g));
                }
            }
        }
        scene.update_bounds();
        self.scene = Arc::new(scene);
    }

    /// Rebuild the whole scene (layer table changes).
    pub fn rebuild(&mut self) {
        let font = crate::font::global();
        if let Some(scene) = build(&self.dwg, &self.hatches, &self.block_hatches, font.as_deref(), &|| false) {
            self.scene = Arc::new(scene);
        }
    }

    /// Add entities and hatches in order; one scene update for the batch.
    /// Returns the new ids.
    pub fn add_entities(&mut self, items: Vec<NewEnt>) -> Vec<u64> {
        let layers_before = self.dwg.layers().count();
        let mut ids = Vec::with_capacity(items.len());
        let mut hatch_added = false;
        // layer names are case-insensitive: use the table's spelling
        let mut spelling: HashMap<String, String> = self.dwg.layers().map(|l| (l.name.to_uppercase(), l.name.clone())).collect();
        for it in items {
            match it {
                NewEnt::Dxf(e) => {
                    let mut e = *e;
                    e.common.handle = Handle::empty();
                    match spelling.get(&e.common.layer.to_uppercase()) {
                        Some(s) => e.common.layer = s.clone(),
                        None => {
                            spelling.insert(e.common.layer.to_uppercase(), e.common.layer.clone());
                        }
                    }
                    // the dxf crate gives sub-entities marked this way real handles
                    if let EntityType::Insert(ins) = &mut e.specific {
                        ins.__attributes_and_handles.iter_mut().for_each(|a| a.1 = AUTO_HANDLE);
                    }
                    if let EntityType::Polyline(pl) = &mut e.specific {
                        pl.__vertices_and_handles.iter_mut().for_each(|v| v.1 = AUTO_HANDLE);
                    }
                    ids.push(self.dwg.add_entity(e).common.handle.0);
                }
                NewEnt::Hatch(h) => {
                    let mut h = *h;
                    h.handle = self.next_handle();
                    let anchor = self.dwg.entities().last().map(|e| e.common.handle.0);
                    ids.push(h.handle);
                    self.hatches.push((anchor, h));
                    hatch_added = true;
                }
            }
        }
        if hatch_added && self.dwg.header.version < AcadVersion::R13 && !self.warnings.iter().any(|w| w == R12_HATCH_WARNING) {
            self.warnings.push(R12_HATCH_WARNING.into());
        }
        if self.dwg.layers().count() != layers_before {
            self.rebuild();
        } else {
            self.refresh(&ids.iter().copied().collect());
        }
        ids
    }

    pub fn ensure_layer(&mut self, name: &str) -> bool {
        if self.dwg.layers().any(|l| l.name.eq_ignore_ascii_case(name)) {
            return false;
        }
        self.dwg.add_layer(dxf::tables::Layer { name: name.to_string(), ..Default::default() });
        true
    }

    /// Translate entities by `d` in drawing coordinates. Nothing moves when
    /// one of them cannot (its ids are the error).
    pub fn move_ids(&mut self, ids: &HashSet<u64>, d: P) -> Result<(), Vec<u64>> {
        let failed: Vec<u64> = self
            .dwg
            .entities()
            .filter(|e| ids.contains(&e.common.handle.0))
            .filter(|e| {
                let mut probe = (*e).clone();
                !translate_entity(&mut probe, d)
            })
            .map(|e| e.common.handle.0)
            .collect();
        if !failed.is_empty() {
            return Err(failed);
        }
        let mut dim_blocks: Vec<String> = Vec::new();
        for e in self.dwg.entities_mut() {
            if ids.contains(&e.common.handle.0) {
                if let Some(b) = dim_block(&e.specific) {
                    dim_blocks.push(b.to_uppercase());
                }
                translate_entity(e, d);
            }
        }
        // a dimension's block holds its drawn geometry: move it along
        for b in self.dwg.blocks_mut() {
            if dim_blocks.contains(&b.name.to_uppercase()) {
                for e in b.entities.iter_mut() {
                    translate_entity(e, d);
                }
            }
        }
        for (_, h) in self.hatches.iter_mut() {
            if ids.contains(&h.handle) {
                h.translate(d);
            }
        }
        self.refresh_blocks(ids, &dim_blocks);
        Ok(())
    }

    /// Copies of entities moved by `d`; returns the new ids.
    pub fn copy_ids(&mut self, ids: &HashSet<u64>, d: P) -> Vec<u64> {
        let mut items: Vec<NewEnt> = Vec::new();
        for e in self.dwg.entities().filter(|e| ids.contains(&e.common.handle.0)) {
            if dim_block(&e.specific).is_some() {
                // a copied dimension would share (and move) the original's block
                continue;
            }
            let mut e = e.clone();
            translate_entity(&mut e, d);
            items.push(NewEnt::Dxf(Box::new(e)));
        }
        for (_, h) in self.hatches.iter().filter(|(_, h)| ids.contains(&h.handle)) {
            let mut h = h.clone();
            h.translate(d);
            items.push(NewEnt::Hatch(Box::new(h)));
        }
        self.add_entities(items)
    }

    /// Remove entities and hatches; one pass over the drawing.
    pub fn delete_ids(&mut self, ids: &HashSet<u64>) -> usize {
        // hatches that followed a deleted entity follow the nearest kept one before it
        let mut prev_kept: HashMap<u64, Option<u64>> = HashMap::new();
        let mut last_kept: Option<u64> = None;
        for e in self.dwg.entities() {
            let h = e.common.handle.0;
            if ids.contains(&h) {
                prev_kept.insert(h, last_kept);
            } else {
                last_kept = Some(h);
            }
        }
        for (anchor, _) in self.hatches.iter_mut() {
            if let Some(a) = *anchor {
                if let Some(p) = prev_kept.get(&a) {
                    *anchor = *p;
                }
            }
        }
        let mut removed = retain_entities(&mut self.dwg, |_, e| !ids.contains(&e.common.handle.0));
        let before = self.hatches.len();
        self.hatches.retain(|(_, h)| !ids.contains(&h.handle));
        removed += before - self.hatches.len();
        self.refresh(ids);
        removed
    }

    pub fn set_layer(&mut self, ids: &HashSet<u64>, layer: &str) {
        let added = self.ensure_layer(layer);
        let spelled = self.dwg.layers().find(|l| l.name.eq_ignore_ascii_case(layer)).map(|l| l.name.clone()).unwrap_or_else(|| layer.to_string());
        for e in self.dwg.entities_mut() {
            if ids.contains(&e.common.handle.0) {
                e.common.layer = spelled.clone();
            }
        }
        for (_, h) in self.hatches.iter_mut() {
            if ids.contains(&h.handle) {
                h.layer = spelled.clone();
            }
        }
        if added {
            self.rebuild();
        } else {
            self.refresh(ids);
        }
    }

    /// `aci` 0..=256, or a 24-bit true colour.
    pub fn set_color(&mut self, ids: &HashSet<u64>, aci: i16, true_color: Option<i32>) {
        let color = aci_color(aci);
        for e in self.dwg.entities_mut() {
            if ids.contains(&e.common.handle.0) {
                e.common.color = color.clone();
                e.common.color_24_bit = true_color.unwrap_or(0);
            }
        }
        for (_, h) in self.hatches.iter_mut() {
            if ids.contains(&h.handle) {
                h.aci = aci;
                h.true_color = true_color;
            }
        }
        self.refresh(ids);
    }
}

/// Keep the entities `keep` accepts, in order, in one pass: kept ones are
/// swapped forward and the rest popped off the end (the `dxf` crate only
/// removes by index, which shifts everything after it).
fn retain_entities(dwg: &mut Drawing, mut keep: impl FnMut(usize, &Entity) -> bool) -> usize {
    let n = dwg.entities().count();
    let mut w = 0;
    {
        let mut refs: Vec<&mut Entity> = dwg.entities_mut().collect();
        for r in 0..n {
            if keep(r, refs[r]) {
                if w != r {
                    let (lo, hi) = refs.split_at_mut(r);
                    std::mem::swap(&mut *lo[w], &mut *hi[0]);
                }
                w += 1;
            }
        }
    }
    for i in (w..n).rev() {
        dwg.remove_entity(i);
    }
    n - w
}

fn has_auto_handles(dwg: &Drawing) -> bool {
    dwg.entities().any(|e| match &e.specific {
        EntityType::Insert(ins) => ins.__attributes_and_handles.iter().any(|a| a.1 == AUTO_HANDLE) || (ins.__has_attributes && ins.__seqend_handle == AUTO_HANDLE),
        EntityType::Polyline(pl) => pl.__vertices_and_handles.iter().any(|v| v.1 == AUTO_HANDLE) || pl.__seqend_handle == AUTO_HANDLE,
        _ => false,
    })
}

pub fn aci_color(aci: i16) -> dxf::Color {
    match aci {
        0 => dxf::Color::by_block(),
        1..=255 => dxf::Color::from_index(aci as u8),
        _ => dxf::Color::by_layer(),
    }
}

fn dim_block(e: &EntityType) -> Option<&str> {
    let b = match e {
        EntityType::RotatedDimension(d) => &d.dimension_base,
        EntityType::RadialDimension(d) => &d.dimension_base,
        EntityType::DiameterDimension(d) => &d.dimension_base,
        EntityType::AngularThreePointDimension(d) => &d.dimension_base,
        EntityType::OrdinateDimension(d) => &d.dimension_base,
        _ => return None,
    };
    Some(b.block_name.as_str()).filter(|s| !s.is_empty())
}

/// A drawing-plane delta expressed in an OCS (inverse of its plan projection).
fn ocs_delta(normal: &dxf::Vector, d: P) -> Option<P> {
    let m = ocs((normal.x, normal.y, normal.z), 0.0);
    let inv = Aff { e: 0.0, f: 0.0, ..m }.inverse()?;
    Some(inv.apply_v(d))
}

fn mv(q: &mut dxf::Point, d: P) {
    q.x += d.x;
    q.y += d.y;
}

/// Translate one entity by `d` (drawing plane). False if its type can't move.
pub fn translate_entity(e: &mut Entity, d: P) -> bool {
    match &mut e.specific {
        EntityType::Line(l) => {
            mv(&mut l.p1, d);
            mv(&mut l.p2, d);
        }
        EntityType::Circle(c) => {
            let Some(o) = ocs_delta(&c.normal, d) else { return false };
            mv(&mut c.center, o);
        }
        EntityType::Arc(a) => {
            let Some(o) = ocs_delta(&a.normal, d) else { return false };
            mv(&mut a.center, o);
        }
        EntityType::Ellipse(el) => mv(&mut el.center, d),
        EntityType::LwPolyline(pl) => {
            let Some(o) = ocs_delta(&pl.extrusion_direction, d) else { return false };
            for v in pl.vertices.iter_mut() {
                v.x += o.x;
                v.y += o.y;
            }
        }
        EntityType::Polyline(pl) => {
            let plan = !(pl.is_3d_polyline() || pl.is_3d_polygon_mesh() || pl.is_polyface_mesh());
            let o = if plan {
                match ocs_delta(&pl.normal, d) {
                    Some(o) => o,
                    None => return false,
                }
            } else {
                d
            };
            for v in pl.vertices_mut() {
                mv(&mut v.location, o);
            }
        }
        EntityType::Spline(s) => {
            s.control_points.iter_mut().for_each(|q| mv(q, d));
            s.fit_points.iter_mut().for_each(|q| mv(q, d));
        }
        EntityType::Text(t) => {
            let Some(o) = ocs_delta(&t.normal, d) else { return false };
            mv(&mut t.location, o);
            mv(&mut t.second_alignment_point, o);
        }
        EntityType::Attribute(t) => {
            let Some(o) = ocs_delta(&t.normal, d) else { return false };
            mv(&mut t.location, o);
            mv(&mut t.second_alignment_point, o);
        }
        EntityType::AttributeDefinition(t) => {
            let Some(o) = ocs_delta(&t.normal, d) else { return false };
            mv(&mut t.location, o);
            mv(&mut t.second_alignment_point, o);
        }
        EntityType::MText(t) => mv(&mut t.insertion_point, d),
        EntityType::Insert(ins) => {
            let Some(o) = ocs_delta(&ins.extrusion_direction, d) else { return false };
            mv(&mut ins.location, o);
            for a in ins.attributes_mut() {
                let ao = ocs_delta(&a.normal, d).unwrap_or(d);
                mv(&mut a.location, ao);
                mv(&mut a.second_alignment_point, ao);
            }
        }
        EntityType::RotatedDimension(x) => {
            mv(&mut x.dimension_base.definition_point_1, d);
            mv(&mut x.dimension_base.text_mid_point, d);
            mv(&mut x.insertion_point, d);
            mv(&mut x.definition_point_2, d);
            mv(&mut x.definition_point_3, d);
        }
        EntityType::RadialDimension(x) => {
            mv(&mut x.dimension_base.definition_point_1, d);
            mv(&mut x.dimension_base.text_mid_point, d);
            mv(&mut x.definition_point_2, d);
        }
        EntityType::DiameterDimension(x) => {
            mv(&mut x.dimension_base.definition_point_1, d);
            mv(&mut x.dimension_base.text_mid_point, d);
            mv(&mut x.definition_point_2, d);
        }
        EntityType::AngularThreePointDimension(x) => {
            mv(&mut x.dimension_base.definition_point_1, d);
            mv(&mut x.dimension_base.text_mid_point, d);
            mv(&mut x.definition_point_2, d);
            mv(&mut x.definition_point_3, d);
            mv(&mut x.definition_point_4, d);
            mv(&mut x.definition_point_5, d);
        }
        EntityType::OrdinateDimension(x) => {
            mv(&mut x.dimension_base.definition_point_1, d);
            mv(&mut x.dimension_base.text_mid_point, d);
            mv(&mut x.definition_point_2, d);
            mv(&mut x.definition_point_3, d);
        }
        EntityType::ModelPoint(pt) => mv(&mut pt.location, d),
        EntityType::Solid(s) => {
            let Some(o) = ocs_delta(&s.extrusion_direction, d) else { return false };
            for q in [&mut s.first_corner, &mut s.second_corner, &mut s.third_corner, &mut s.fourth_corner] {
                mv(q, o);
            }
        }
        EntityType::Trace(s) => {
            let Some(o) = ocs_delta(&s.extrusion_direction, d) else { return false };
            for q in [&mut s.first_corner, &mut s.second_corner, &mut s.third_corner, &mut s.fourth_corner] {
                mv(q, o);
            }
        }
        EntityType::Face3D(f) => {
            for q in [&mut f.first_corner, &mut f.second_corner, &mut f.third_corner, &mut f.fourth_corner] {
                mv(q, d);
            }
        }
        EntityType::Leader(l) => l.vertices.iter_mut().for_each(|q| mv(q, d)),
        EntityType::MLine(l) => {
            mv(&mut l.start_point, d);
            l.vertices.iter_mut().for_each(|q| mv(q, d));
        }
        EntityType::Ray(r) => mv(&mut r.start_point, d),
        EntityType::XLine(r) => mv(&mut r.first_point, d),
        EntityType::Image(im) => mv(&mut im.location, d),
        EntityType::Wipeout(w) => mv(&mut w.location, d),
        _ => return false,
    }
    true
}

