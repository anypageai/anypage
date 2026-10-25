//! Rendering through hayro (content stream interpreter) and vello_cpu
//! (rasteriser): 256 px tiles for `page.tiled`, whole pages for `pdf.render`.
//!
//! Tiles are a pure function of (tile key, params): the tile's affine maps page
//! space straight to the tile's pixels, so vector content is rasterised at the
//! tile's own resolution and stays crisp at every zoom level.

use std::sync::{Arc, Mutex, OnceLock};

use anypage_core::{tile_matrix, Rect, TileKey, Value, TILE_SIZE};
use anypage_engine::{Image, TileSource};
use anypage_runtime::Data;
use hayro::hayro_interpret::font::{FontData, FontQuery};
use hayro::hayro_interpret::InterpreterSettings;
use hayro::kurbo::{Affine, Rect as KRect};
use hayro::vello_cpu::{self, color::AlphaColor, Pixmap, RasterizerSettings, RenderContext, Resources};
use hayro::{RenderCache, RenderSettings};

use crate::doc::{PdfDoc, Snapshot};
use crate::util::R;

/// Largest image `pdf.render` produces.
pub const MAX_SIDE: u32 = 16_384;
pub const MAX_PIXELS: u64 = 48 << 20;
/// Rows rendered per pass in a large render, so cancellation is honoured.
const BAND_PIXELS: u64 = 4 << 20;

// ---------------------------------------------------------------- fonts

type FontPick = Option<(FontData, u32)>;

fn cjk_override() -> &'static Mutex<FontPick> {
    static O: OnceLock<Mutex<FontPick>> = OnceLock::new();
    O.get_or_init(|| Mutex::new(None))
}

/// Give the renderer a CJK-capable font (TrueType/OpenType bytes, face index)
/// for CID fonts that are not embedded. Without it the module looks for one
/// among the platform's fonts.
pub fn set_cjk_font(data: Vec<u8>, index: u32) {
    *cjk_override().lock().unwrap_or_else(|p| p.into_inner()) = Some((Arc::new(data) as FontData, index));
}

const CJK_CANDIDATES: [&str; 11] = [
    r"C:\Windows\Fonts\msjh.ttc",
    r"C:\Windows\Fonts\msyh.ttc",
    r"C:\Windows\Fonts\simsun.ttc",
    r"C:\Windows\Fonts\mingliub.ttc",
    "/system/fonts/NotoSansCJK-Regular.ttc",
    "/system/fonts/NotoSerifCJK-Regular.ttc",
    "/system/fonts/NotoSansSC-Regular.otf",
    "/system/fonts/DroidSansFallback.ttf",
    "/System/Library/Fonts/PingFang.ttc",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
];

fn system_cjk() -> FontPick {
    static F: OnceLock<FontPick> = OnceLock::new();
    F.get_or_init(|| {
        for path in CJK_CANDIDATES {
            let Ok(bytes) = std::fs::read(path) else { continue };
            let ok = skrifa::FontRef::from_index(&bytes, 0).is_ok_and(|f| {
                use skrifa::MetadataProvider;
                f.charmap().map('\u{4E2D}').is_some()
            });
            if ok {
                log::info!(target: "anypage_pdf", "CJK fallback font: {path}");
                return Some((Arc::new(bytes) as FontData, 0));
            }
        }
        None
    })
    .clone()
}

/// The CJK font non-embedded CID fonts are drawn with, if any.
pub fn cjk_font() -> FontPick {
    let o = cjk_override().lock().unwrap_or_else(|p| p.into_inner()).clone();
    o.or_else(system_cjk)
}

fn resolve_font(q: &FontQuery) -> FontPick {
    match q {
        FontQuery::Standard(s) => Some(s.get_font_data()),
        FontQuery::Fallback(f) => {
            if f.character_collection.is_some() {
                if let Some(c) = cjk_font() {
                    return Some(c);
                }
            }
            Some(f.pick_standard_font().get_font_data())
        }
    }
}

pub fn interpreter_settings() -> InterpreterSettings {
    InterpreterSettings { font_resolver: Arc::new(resolve_font), warning_sink: Arc::new(|_| {}), render_annotations: true, ..Default::default() }
}

// ---------------------------------------------------------------- rasterising

fn level() -> vello_cpu::Level {
    static L: OnceLock<vello_cpu::Level> = OnceLock::new();
    *L.get_or_init(|| vello_cpu::Level::try_detect().unwrap_or(vello_cpu::Level::baseline()))
}

/// Page `i` drawn through `page_to_px` (page space to pixels) into a `w` x `h`
/// premultiplied RGBA8 buffer, paper colour `paper` (non-premultiplied RGBA).
pub fn render_region(snap: &Snapshot, i: usize, page_to_px: Affine, w: u16, h: u16, paper: [u8; 4]) -> R<Vec<u8>> {
    let page = snap.page(i)?;
    let geom = snap.geoms[i];
    crate::guard("rendering", || {
        let mut ctx = RenderContext::new_with(w, h, vello_cpu::RenderSettings { level: level(), num_threads: 0 });
        let (pw, ph) = geom.size();
        if paper[3] > 0 {
            ctx.set_transform(page_to_px);
            ctx.set_paint(AlphaColor::from_rgba8(paper[0], paper[1], paper[2], paper[3]));
            ctx.fill_rect(&KRect::new(0.0, 0.0, pw, ph));
        }
        let cache = RenderCache::new();
        hayro::render_into(page, &cache, &interpreter_settings(), &RenderSettings::default(), &mut ctx, page_to_px * geom.user_to_page());
        ctx.flush();
        let mut pm = Pixmap::new(w, h);
        ctx.render_with(&mut pm, &mut Resources::default(), RasterizerSettings::default());
        Ok(pm.data_as_u8_slice().to_vec())
    })
}

/// Page `i` at `width_px` pixels wide, in bands so `cancelled` is honoured.
pub fn render_page(snap: &Snapshot, i: usize, width_px: f64, paper: [u8; 4], cancelled: &dyn Fn() -> bool, progress: &dyn Fn(f32)) -> R<Option<Image>> {
    let (pw, ph) = snap.geoms.get(i).ok_or("no such page")?.size();
    if !width_px.is_finite() || width_px < 1.0 {
        return Err("width_px must be a positive number".into());
    }
    let s = width_px / pw;
    let w = width_px.round().max(1.0) as u64;
    let h = (ph * s).round().max(1.0) as u64;
    if w > MAX_SIDE as u64 || h > MAX_SIDE as u64 || w * h > MAX_PIXELS {
        return Err(format!("{w} x {h} px is too large (at most {MAX_SIDE} px a side and {} Mpx)", MAX_PIXELS >> 20));
    }
    let band = (BAND_PIXELS / w).clamp(1, h);
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    let mut y0 = 0;
    while y0 < h {
        if cancelled() {
            return Ok(None);
        }
        let bh = band.min(h - y0);
        let t = Affine::translate((0.0, -(y0 as f64))) * Affine::scale(s);
        let px = render_region(snap, i, t, w as u16, bh as u16, paper)?;
        let start = (y0 * w * 4) as usize;
        rgba[start..start + px.len()].copy_from_slice(&px);
        y0 += bh;
        progress(y0 as f32 / h as f32);
    }
    Ok(Some(Image { w: w as u32, h: h as u32, rgba, newest: None }))
}

// ---------------------------------------------------------------- tiles

/// Parameters of the `pdf.page` tile source.
pub struct TileParams {
    pub doc: Arc<PdfDoc>,
    pub page: f64,
    pub rect: Option<Rect>,
    pub paper: [u8; 4],
}

pub fn rect_from(d: &Data) -> Option<Rect> {
    let n: Vec<f64> = d.items().iter().filter_map(Data::num).collect();
    if n.len() == 4 && n[2] > 0.0 && n[3] > 0.0 && n.iter().all(|x| x.is_finite()) {
        Some(Rect::new(n[0] as f32, n[1] as f32, n[2] as f32, n[3] as f32))
    } else {
        None
    }
}

pub fn color_from(d: &Data, default: [u8; 4]) -> [u8; 4] {
    match d.num() {
        Some(c) if c.is_finite() && c >= 0.0 => (c.min(u32::MAX as f64) as u32).to_be_bytes(),
        _ => default,
    }
}

impl TileParams {
    pub fn from_data(d: &Data) -> R<TileParams> {
        let doc = crate::doc_arg(d.get("doc"))?;
        Ok(TileParams { doc, page: d.get("page").num_or(1.0), rect: rect_from(d.get("rect")), paper: color_from(d.get("background"), [255, 255, 255, 255]) })
    }
}

/// The tile source behind `page.tiled{source = "pdf.page"}`. `layer` is the
/// tiled layer's world rect; tile keys address it from its top-left corner.
pub struct PdfTiles {
    pub layer: Rect,
}

/// Page space to the pixels of tile `key`, for a page drawn into world rect
/// `target` inside a tiled layer at world rect `layer`.
pub fn tile_affine(layer: Rect, target: Rect, page_size: (f64, f64), key: TileKey) -> Affine {
    // layer-local world units to the tile's pixels, as every tiled source maps them
    let world_to_px = Affine::new(tile_matrix(key).map(f64::from));
    // page points to layer-local world units
    let page_to_world = Affine::new([
        target.w as f64 / page_size.0,
        0.0,
        0.0,
        target.h as f64 / page_size.1,
        (target.x - layer.x) as f64,
        (target.y - layer.y) as f64,
    ]);
    world_to_px * page_to_world
}

impl PdfTiles {
    pub fn tile(&self, key: TileKey, p: &TileParams, cancelled: &dyn Fn() -> bool) -> R<Option<Vec<u8>>> {
        let snap = p.doc.snapshot()?;
        let i = snap.index(p.page)?;
        if cancelled() {
            return Ok(None);
        }
        let target = p.rect.unwrap_or(self.layer);
        let t = tile_affine(self.layer, target, snap.geoms[i].size(), key);
        render_region(&snap, i, t, TILE_SIZE as u16, TILE_SIZE as u16, p.paper).map(Some)
    }
}

fn error_tile() -> Vec<u8> {
    let n = (TILE_SIZE * TILE_SIZE) as usize;
    let mut v = Vec::with_capacity(n * 4);
    for k in 0..n {
        let (x, y) = (k % TILE_SIZE as usize, k / TILE_SIZE as usize);
        let stripe = (x + y) / 16 % 2 == 0;
        v.extend_from_slice(if stripe { &[250, 228, 228, 255] } else { &[244, 244, 244, 255] });
    }
    v
}

impl TileSource for PdfTiles {
    fn render(&self, key: TileKey, params: &Value, cancelled: &dyn Fn() -> bool) -> Option<Vec<u8>> {
        let d = anypage_runtime::data::value_to_data(&**params);
        // no document yet (still loading): nothing to draw, nothing wrong
        if d.get("doc").is_nil() {
            return Some(vec![0; (TILE_SIZE * TILE_SIZE * 4) as usize]);
        }
        let r = TileParams::from_data(&d).and_then(|p| self.tile(key, &p, cancelled));
        match r {
            Ok(px) => px,
            Err(e) => {
                log::warn!(target: "anypage_pdf", "tile {key:?}: {e}");
                Some(error_tile())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hayro::kurbo::Point;

    #[test]
    fn tile_affine_maps_page_corners() {
        // a 612 x 792 page shown at 2 world units per point, layer at (100, 50)
        let layer = Rect::new(100.0, 50.0, 1224.0, 1584.0);
        let key = TileKey { page: 0, level: 1, tx: 3, ty: 2 };
        let t = tile_affine(layer, layer, (612.0, 792.0), key);
        // page point (p) -> world (100 + 2p.x, 50 + 2p.y) -> tile px ((w - 100) * 2 - 768, (w - 50) * 2 - 512)
        let p = t * Point::new(200.0, 150.0);
        assert!((p.x - (400.0 * 2.0 - 768.0)).abs() < 1e-9);
        assert!((p.y - (300.0 * 2.0 - 512.0)).abs() < 1e-9);
    }
}
