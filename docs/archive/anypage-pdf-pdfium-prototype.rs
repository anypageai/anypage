//! L2 adapter: PDFium behind a single dedicated thread.
//!
//! PDFium is not thread-safe at all, so one process gets exactly one PDFium
//! thread and every document/page/raster call is serialised on it
//! (ARCHITECTURE.md §8, "L2 執行緒契約"). The core never sees a PDFium type:
//! it only receives finished BGRA tiles.
//!
//! Only the raw `FPDF_*` bindings are used so that the crate does not depend on
//! the high-level API surface of `pdfium-render`, which changes often.

use std::os::raw::c_int;
use std::path::Path;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Instant;

use anypage_core::{tile_matrix, RasterQueue, TileKey, TILE_SIZE};
use pdfium_render::bindgen::{FPDFBitmap_BGRA, FPDF_ANNOT, FPDF_PAGE, FS_MATRIX, FS_RECTF};
use pdfium_render::prelude::*;

#[derive(Clone, Debug)]
pub struct DocInfo {
    /// `(width, height)` in points, per page.
    pub page_sizes: Vec<(f32, f32)>,
    /// Time from thread start to document opened, including loading the library.
    pub open_ms: f32,
}

pub struct TileResult {
    pub key: TileKey,
    /// `TILE_SIZE * TILE_SIZE * 4` bytes, BGRA, tightly packed.
    pub bgra: Vec<u8>,
    pub raster_ms: f32,
    /// Non-zero only when this tile caused the page to be loaded.
    pub page_load_ms: f32,
}

pub struct PdfWorker {
    pub info: DocInfo,
    pub results: Receiver<TileResult>,
    queue: Arc<RasterQueue>,
    handle: Option<JoinHandle<()>>,
}

impl PdfWorker {
    /// Load `pdf` on a new PDFium thread and start serving `queue`. Blocks until
    /// the document is open so the caller has page sizes for layout. `wake` is
    /// called after each finished tile so an event-driven render loop can sleep.
    pub fn spawn(
        pdfium_dir: &Path,
        pdf: &Path,
        queue: Arc<RasterQueue>,
        wake: Box<dyn Fn() + Send>,
    ) -> Result<Self, String> {
        let (info_tx, info_rx) = channel::<Result<DocInfo, String>>();
        let (res_tx, res_rx) = channel::<TileResult>();
        let q = queue.clone();
        let pdfium_dir = pdfium_dir.to_path_buf();
        let pdf = pdf.to_path_buf();
        let handle = std::thread::Builder::new()
            .name("pdfium".into())
            .spawn(move || worker_main(&pdfium_dir, &pdf, q, info_tx, res_tx, wake))
            .map_err(|e| format!("cannot spawn PDFium thread: {e}"))?;
        let info = info_rx
            .recv()
            .map_err(|_| "PDFium thread died before opening the document".to_string())??;
        Ok(Self { info, results: res_rx, queue, handle: Some(handle) })
    }
}

impl Drop for PdfWorker {
    fn drop(&mut self) {
        self.queue.shutdown();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn bind(dir: &Path) -> Result<Box<dyn PdfiumLibraryBindings>, String> {
    let lib = Pdfium::pdfium_platform_library_name_at_path(dir);
    match Pdfium::bind_to_library(&lib) {
        Ok(b) => Ok(b),
        Err(e1) => Pdfium::bind_to_system_library().map_err(|e2| {
            format!("cannot load PDFium: tried {lib} ({e1:?}), then the system library ({e2:?})")
        }),
    }
}

fn ms(since: Instant) -> f32 {
    since.elapsed().as_secs_f32() * 1000.0
}

fn worker_main(
    pdfium_dir: &Path,
    pdf: &Path,
    queue: Arc<RasterQueue>,
    info_tx: Sender<Result<DocInfo, String>>,
    res_tx: Sender<TileResult>,
    wake: Box<dyn Fn() + Send>,
) {
    let t_open = Instant::now();
    let bindings = match bind(pdfium_dir) {
        Ok(b) => b,
        Err(e) => {
            let _ = info_tx.send(Err(e));
            return;
        }
    };
    let pdfium = Pdfium::new(bindings);
    let b = pdfium.bindings();

    let path = pdf.to_string_lossy().to_string();
    let doc = b.FPDF_LoadDocument(&path, None);
    if doc.is_null() {
        let code = b.FPDF_GetLastError();
        let _ = info_tx.send(Err(format!("FPDF_LoadDocument failed with error {code} for {path}")));
        return;
    }

    let n = b.FPDF_GetPageCount(doc).max(0);
    let mut page_sizes = Vec::with_capacity(n as usize);
    for i in 0..n {
        let (mut w, mut h) = (0.0f64, 0.0f64);
        if b.FPDF_GetPageSizeByIndex(doc, i, &mut w, &mut h) == 0 || w <= 0.0 || h <= 0.0 {
            (w, h) = (612.0, 792.0);
        }
        page_sizes.push((w as f32, h as f32));
    }
    if info_tx.send(Ok(DocInfo { page_sizes, open_ms: ms(t_open) })).is_err() {
        b.FPDF_CloseDocument(doc);
        return;
    }

    let t = TILE_SIZE as c_int;
    let bitmap = b.FPDFBitmap_CreateEx(t, t, FPDFBitmap_BGRA as c_int, std::ptr::null_mut(), 0);
    let stride = b.FPDFBitmap_GetStride(bitmap) as usize;
    let row = TILE_SIZE as usize * 4;
    let mut pages: Vec<Option<FPDF_PAGE>> = vec![None; n as usize];

    while let Some(key) = queue.take() {
        let t_start = Instant::now();
        let idx = key.page as usize;
        if idx >= pages.len() {
            queue.complete(key);
            continue;
        }
        let mut page_load_ms = 0.0;
        let page = match pages[idx] {
            Some(p) => p,
            None => {
                let t_page = Instant::now();
                let p = b.FPDF_LoadPage(doc, idx as c_int);
                page_load_ms = ms(t_page);
                pages[idx] = Some(p);
                p
            }
        };

        // White background, then the page drawn through the tile's transform
        // and clipped to the tile. Areas outside the page stay white; the render
        // side clips the quad to the page rect anyway.
        b.FPDFBitmap_FillRect(bitmap, 0, 0, t, t, 0xFFFF_FFFF);
        if !page.is_null() {
            let m = tile_matrix(key);
            let matrix = FS_MATRIX { a: m[0], b: m[1], c: m[2], d: m[3], e: m[4], f: m[5] };
            let clip = FS_RECTF { left: 0.0, top: 0.0, right: t as f32, bottom: t as f32 };
            b.FPDF_RenderPageBitmapWithMatrix(bitmap, page, &matrix, &clip, FPDF_ANNOT as c_int);
        }

        let buf = b.FPDFBitmap_GetBuffer(bitmap) as *const u8;
        // SAFETY: PDFium owns `stride * TILE_SIZE` bytes behind `buf` for the
        // lifetime of `bitmap`, which outlives this loop body.
        let raw = unsafe { std::slice::from_raw_parts(buf, stride * TILE_SIZE as usize) };
        let bgra = if stride == row {
            raw.to_vec()
        } else {
            let mut v = Vec::with_capacity(row * TILE_SIZE as usize);
            for r in raw.chunks(stride) {
                v.extend_from_slice(&r[..row]);
            }
            v
        };

        let result = TileResult { key, bgra, raster_ms: ms(t_start), page_load_ms };
        if res_tx.send(result).is_err() {
            break;
        }
        wake();
    }

    b.FPDFBitmap_Destroy(bitmap);
    for p in pages.into_iter().flatten() {
        if !p.is_null() {
            b.FPDF_ClosePage(p);
        }
    }
    b.FPDF_CloseDocument(doc);
}
