//! Raster workers for every `Tiled` layer of a page. Requests carry the
//! layer's parameter version; parameters are published per version so a
//! worker always renders exactly the version it was asked for.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anypage_core::{overview_level, PageLayout, RasterQueue, TileKey, Value, Wake};

use crate::page::{TileSource, TiledLayer};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TileReq {
    pub layer: u16,
    pub ver: u32,
    pub key: TileKey,
}

pub struct TileDone {
    pub req: TileReq,
    pub rgba: Option<Vec<u8>>,
    pub ms: f32,
}

struct Shared {
    sources: Vec<Arc<dyn TileSource>>,
    params: Mutex<Vec<VecDeque<(u32, Value)>>>,
    latest: Vec<AtomicU32>,
}

pub struct TileSystem {
    pub queue: Arc<RasterQueue<TileReq>>,
    pub rx: Receiver<TileDone>,
    shared: Arc<Shared>,
    pub layouts: Vec<PageLayout>,
    pub ov_levels: Vec<i32>,
    pub requested: HashMap<TileReq, Instant>,
    pub workers: usize,
}

impl TileSystem {
    pub fn new(layers: &[TiledLayer], wake: Wake, workers: usize) -> Self {
        let queue = RasterQueue::<TileReq>::new();
        let (tx, rx) = channel();
        let shared = Arc::new(Shared {
            sources: layers.iter().map(|l| l.source.clone()).collect(),
            params: Mutex::new(layers.iter().map(|_| VecDeque::new()).collect()),
            latest: layers.iter().map(|_| AtomicU32::new(0)).collect(),
        });
        let layouts: Vec<PageLayout> = layers.iter().map(|l| PageLayout { pages: vec![l.rect], bounds: l.rect }).collect();
        let ov_levels = layouts.iter().map(overview_level).collect();
        let workers = if layers.is_empty() { 0 } else { workers.max(1) };
        for i in 0..workers {
            let (q, tx, sh, wake) = (queue.clone(), tx.clone(), shared.clone(), wake.clone());
            std::thread::Builder::new()
                .name(format!("tile-{i}"))
                .spawn(move || {
                    while let Some(req) = q.take() {
                        let t = Instant::now();
                        let layer = req.layer as usize;
                        let params = sh.params.lock().unwrap()[layer].iter().rev().find(|e| e.0 == req.ver).map(|e| e.1.clone());
                        // stop work that is many parameter versions behind
                        let latest = &sh.latest[layer];
                        let stale = || latest.load(Ordering::Relaxed) > req.ver.saturating_add(8);
                        // a tile source that panics loses that tile, not the worker
                        let rgba = params.and_then(|p| {
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sh.sources[layer].render(req.key, &p, &stale))).unwrap_or(None)
                        });
                        let ms = t.elapsed().as_secs_f32() * 1000.0;
                        if tx.send(TileDone { req, rgba, ms }).is_err() {
                            break;
                        }
                        wake();
                    }
                })
                .expect("spawn tile worker");
        }
        Self { queue, rx, shared, layouts, ov_levels, requested: HashMap::new(), workers }
    }

    pub fn publish_params(&self, layer: usize, ver: u32, v: &Value) {
        let mut all = self.shared.params.lock().unwrap();
        let q = &mut all[layer];
        if q.back().map(|b| b.0) == Some(ver) {
            return;
        }
        q.push_back((ver, v.clone()));
        while q.len() > 32 {
            q.pop_front();
        }
        self.shared.latest[layer].store(ver, Ordering::Relaxed);
    }
}

impl Drop for TileSystem {
    fn drop(&mut self) {
        self.queue.shutdown();
    }
}
