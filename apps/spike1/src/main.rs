//! Phase-0 spike 1: one page, every kind of timing.
//!
//! Static engine-computed tiles, a GPU pull stream, a CPU pull stream, two push
//! streams, async jobs and direct manipulation all live on one page, in one
//! computation graph, under one frame scheduler and one compositor.
//! See docs/ARCHITECTURE.md sections 3 and 15.
//!
//!   cargo run --release -p spike1                 interactive
//!   cargo run --release -p spike1 -- --bench      scripted 20 s run, writes bench-results/
//!   cargo run --release -p spike1 -- --no-vsync

mod feeds;
mod gpu;
mod mandel;
mod page;
mod raster;
mod stats;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anypage_core::{
    ancestor, level_for_scale, overview_level, tile_world_rect, tile_world_size, val, visible_tiles, Camera,
    PageLayout, RasterQueue, Rect, TileKey, Wake, MAX_LEVEL, TILE_SIZE,
};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

use gpu::{CameraUniform, Draw, EffectUniform, Gpu, TexKey, Vertex};
use mandel::{MandelParams, MandelSource};
use page::{Class, EffectParams, Hit, Page, Slider};
use raster::{Canvas, Image, TextRaster};
use stats::{rss_mb, Stats};

const TITLE: &str = "AnyPage spike 1 — 一個頁面，所有時間性";
const FRAME_BUDGET_MS: f32 = 10.0;
const CLEAR: wgpu::Color = wgpu::Color { r: 0.006, g: 0.007, b: 0.010, a: 1.0 };

#[derive(Debug, Clone, Copy)]
enum UserEvent {
    Wake,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct TileReq {
    ver: u32,
    key: TileKey,
}

struct TileDone {
    req: TileReq,
    rgba: Option<Vec<u8>>,
    ms: f32,
}

/// Raster workers for the engine-computed Tiled source.
struct Tiles {
    queue: Arc<RasterQueue<TileReq>>,
    rx: Receiver<TileDone>,
    source: Arc<MandelSource>,
    layout: PageLayout,
    ov_level: i32,
    requested: HashMap<TileReq, Instant>,
    workers: usize,
}

impl Tiles {
    fn new(source: Arc<MandelSource>, wake: Wake, workers: usize) -> Self {
        let queue = RasterQueue::<TileReq>::new();
        let (tx, rx) = channel();
        let layout = PageLayout { pages: vec![source.rect], bounds: source.rect };
        let ov_level = overview_level(&layout);
        for i in 0..workers {
            let (q, tx, src, wake) = (queue.clone(), tx.clone(), source.clone(), wake.clone());
            std::thread::Builder::new()
                .name(format!("tile-{i}"))
                .spawn(move || {
                    while let Some(req) = q.take() {
                        let t = Instant::now();
                        // stop work that is many parameter versions behind
                        let stale = || src.latest() > req.ver.saturating_add(8);
                        let rgba = src.params(req.ver).and_then(|p| src.render(req.key, p, &stale));
                        let ms = t.elapsed().as_secs_f32() * 1000.0;
                        if tx.send(TileDone { req, rgba, ms }).is_err() {
                            break;
                        }
                        wake();
                    }
                })
                .expect("spawn tile worker");
        }
        Self { queue, rx, source, layout, ov_level, requested: HashMap::new(), workers }
    }
}

impl Drop for Tiles {
    fn drop(&mut self) {
        self.queue.shutdown();
    }
}

enum Drag {
    Pan { last: (f32, f32) },
    Slider(Slider),
}

/// Scripted run for `--bench`: zoom, edit, pan, go idle, resume.
struct Bench {
    actions: Vec<(f32, &'static str)>,
    next: usize,
    idle_window: (f32, f32),
    idle_frames: Option<(u64, u64)>,
    idle_commits: Option<(u64, u64)>,
    end: f32,
    pan_base: Option<(f32, f32)>,
    done: bool,
}

impl Bench {
    fn new() -> Self {
        Self {
            actions: vec![
                (0.3, "focus"),
                (6.0, "edit"),
                (6.0, "jobs"),
                (6.2, "right"),
                (11.0, "quiet"),
                (12.0, "idle-begin"),
                (15.0, "idle-end"),
                (15.0, "resume"),
                (19.5, "finish"),
            ],
            next: 0,
            idle_window: (12.0, 15.0),
            idle_frames: None,
            idle_commits: None,
            end: 19.5,
            pan_base: None,
            done: false,
        }
    }
    fn zooming(t: f32) -> bool {
        (0.4..6.0).contains(&t)
    }
    fn panning(t: f32) -> bool {
        (6.3..11.0).contains(&t)
    }
    fn continuous(&self, t: f32) -> bool {
        Self::zooming(t) || Self::panning(t)
    }
    fn next_time(&self) -> Option<f32> {
        self.actions.get(self.next).map(|a| a.0)
    }
}

struct State {
    window: Arc<Window>,
    gpu: Gpu,
    page: Page,
    camera: Camera,
    tiles: Tiles,
    text: Arc<TextRaster>,
    stats: Stats,
    cursor: (f32, f32),
    drag: Option<Drag>,
    pending_op: Option<Instant>,
    camera_dirty: bool,
    first: bool,
    last_level: i32,
    uploaded: HashMap<u32, u64>,
    pending_async: Vec<Instant>,
    hud_at: Instant,
    hud_ver: u64,
    hud_visible: bool,
    start: Instant,
    needs_frame: bool,
    title_idle_secs: u64,
    bench: Option<Bench>,
}

fn ms_since(t: Instant) -> f32 {
    t.elapsed().as_secs_f32() * 1000.0
}

fn push_quad(verts: &mut Vec<Vertex>, r: Rect, uv: [f32; 4]) -> std::ops::Range<u32> {
    let start = verts.len() as u32;
    let (x0, y0, x1, y1) = (r.x, r.y, r.right(), r.bottom());
    let (u0, v0, u1, v1) = (uv[0], uv[1], uv[0] + uv[2], uv[1] + uv[3]);
    verts.extend_from_slice(&[
        Vertex { pos: [x0, y0], uv: [u0, v0] },
        Vertex { pos: [x1, y0], uv: [u1, v0] },
        Vertex { pos: [x0, y1], uv: [u0, v1] },
        Vertex { pos: [x1, y0], uv: [u1, v0] },
        Vertex { pos: [x1, y1], uv: [u1, v1] },
        Vertex { pos: [x0, y1], uv: [u0, v1] },
    ]);
    start..verts.len() as u32
}

impl State {
    fn new(window: Arc<Window>, wake: Wake, vsync: bool, bench: bool) -> Self {
        let start = Instant::now();
        let gpu = Gpu::new(window.clone(), vsync, 256 * 1024 * 1024);
        let text = Arc::new(TextRaster::load().expect("font"));
        let page = Page::new(text.clone(), wake.clone(), start);
        let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8).saturating_sub(4).clamp(2, 24);
        let tiles = Tiles::new(page.mandel.clone(), wake, workers);
        let (w, h) = gpu.size();
        let mut camera = Camera::new(w as f32, h as f32);
        camera.fit_rect(page::BOUNDS, 24.0);
        println!(
            "AnyPage spike 1 · {:?} · {} · {:?} · font {} · {} tile workers · {} job workers",
            gpu.adapter_info.backend,
            gpu.adapter_info.name,
            gpu.present_mode(),
            text.source,
            workers,
            page::JOB_COUNT
        );
        Self {
            window,
            gpu,
            page,
            camera,
            tiles,
            text,
            stats: Stats::new(start),
            cursor: (0.0, 0.0),
            drag: None,
            pending_op: None,
            camera_dirty: true,
            first: true,
            last_level: i32::MIN,
            uploaded: HashMap::new(),
            pending_async: Vec::new(),
            hud_at: start,
            hud_ver: 0,
            hud_visible: true,
            start,
            needs_frame: true,
            title_idle_secs: 0,
            bench: if bench { Some(Bench::new()) } else { None },
        }
    }

    fn op(&mut self) {
        if self.pending_op.is_none() {
            self.pending_op = Some(Instant::now());
        }
    }

    fn fit(&mut self) {
        self.camera.fit_rect(page::BOUNDS, 24.0);
        self.camera_dirty = true;
        self.op();
    }

    fn apply_hit(&mut self, hit: Hit) {
        match hit {
            Hit::Slider(s, f) => {
                self.page.slider_value(s, f);
                self.drag = Some(Drag::Slider(s));
            }
            Hit::Streams => {
                let on = !self.page.streams_on();
                self.page.set_streams(on);
            }
            Hit::Anim => {
                let on = !self.page.anim_on;
                self.page.set_anim(on);
            }
            Hit::RunJobs => self.page.run_jobs(),
            Hit::CancelJobs => self.page.cancel_jobs(),
            Hit::Fit => self.fit(),
        }
        self.op();
    }

    // ------------------------------------------------------------ bench script

    fn bench_step(&mut self, t: f32) {
        let Some(b) = self.bench.as_mut() else { return };
        let mut due = Vec::new();
        while let Some(&(at, name)) = b.actions.get(b.next) {
            if t < at {
                break;
            }
            due.push(name);
            b.next += 1;
        }
        let zooming = Bench::zooming(t);
        let panning = Bench::panning(t);
        let pan_base = b.pan_base;
        for name in due {
            match name {
                "focus" => {
                    // seahorse valley, centred
                    let (wx, wy) = (636.6, 904.0);
                    let (sx, sy) = self.camera.world_to_screen(wx, wy);
                    self.camera.pan(self.camera.viewport_w * 0.5 - sx, self.camera.viewport_h * 0.5 - sy);
                }
                "edit" => {
                    self.page.set_iterations(768);
                    self.page.set_hue(320.0);
                }
                "jobs" => self.page.run_jobs(),
                "right" => {
                    self.camera.fit_rect(page::RIGHT_COLUMN, 24.0);
                    if let Some(b) = self.bench.as_mut() {
                        b.pan_base = Some((self.camera.tx, self.camera.ty));
                    }
                }
                "quiet" => {
                    self.page.set_streams(false);
                    self.page.set_anim(false);
                    self.page.cancel_jobs();
                }
                "idle-begin" => {
                    if let Some(b) = self.bench.as_mut() {
                        b.idle_frames = Some((self.stats.frames, 0));
                        b.idle_commits = Some((self.stats.commits, 0));
                    }
                }
                "idle-end" => {
                    if let Some(b) = self.bench.as_mut() {
                        if let Some((f0, _)) = b.idle_frames {
                            b.idle_frames = Some((f0, self.stats.frames));
                        }
                        if let Some((c0, _)) = b.idle_commits {
                            b.idle_commits = Some((c0, self.stats.commits));
                        }
                    }
                }
                "resume" => {
                    self.page.set_streams(true);
                    self.page.set_anim(true);
                    self.camera.fit_rect(page::BOUNDS, 24.0);
                }
                "finish" => {
                    if let Some(b) = self.bench.as_mut() {
                        b.done = true;
                    }
                }
                _ => {}
            }
            // markers must not cause a render, or they would pollute the idle window
            if !name.starts_with("idle") && name != "finish" {
                self.camera_dirty = true;
                self.op();
            }
        }
        if zooming {
            let (cx, cy) = (self.camera.viewport_w * 0.5, self.camera.viewport_h * 0.5);
            self.camera.zoom_at(cx, cy, 1.012);
            self.camera_dirty = true;
            self.op();
        }
        if panning {
            if let Some((tx, ty)) = pan_base {
                self.camera.tx = tx;
                self.camera.ty = ty + ((t - 6.3) * 1.3).sin() * 160.0;
                self.camera_dirty = true;
                self.op();
            }
        }
    }

    fn bench_report(&self) -> String {
        let b = self.bench.as_ref().expect("bench");
        let s = &self.stats;
        let g = &self.page.graph.stats;
        let idle_frames = b.idle_frames.map(|(a, z)| z.saturating_sub(a)).unwrap_or(0);
        let idle_commits = b.idle_commits.map(|(a, z)| z.saturating_sub(a)).unwrap_or(0);
        let mut r = String::new();
        r += &format!("# spike 1 bench · {}\n\n", self.gpu.adapter_info.name);
        r += &format!(
            "- backend {:?}, present mode {:?}, {} tile workers, {} job workers, window {}x{}\n",
            self.gpu.adapter_info.backend,
            self.gpu.present_mode(),
            self.tiles.workers,
            page::JOB_COUNT,
            self.gpu.size().0,
            self.gpu.size().1
        );
        r += &format!("- duration {:.1} s, frames rendered {}, loop commits {}, commits with nothing visible to redraw {}\n", b.end, s.frames, s.commits, s.skipped);
        r += &format!("- first frame {:.0} ms after start\n\n", s.first_frame_ms.unwrap_or(0.0));
        r += "| metric | value |\n|---|---|\n";
        r += &format!("| frame CPU time (ms) | {} |\n", s.frame_cpu.line(""));
        r += &format!("| frame interval (ms) | {} |\n", s.interval.line(""));
        r += &format!("| operation → present (ms) | {} |\n", s.lat_ops.line(""));
        r += &format!("| push-stream item → present (ms) | {} |\n", s.lat_stream.line(""));
        r += &format!("| async result → present (ms) | {} |\n", s.lat_async.line(""));
        r += &format!("| tile request → present (ms) | {} |\n", s.lat_tile.line(""));
        r += &format!("| tile raster time (ms) | {} |\n", s.tile_ms.line(""));
        r += &format!(
            "| idle window {:.0}–{:.0} s | {} frames rendered, {} loop wake-ups |\n",
            b.idle_window.0, b.idle_window.1, idle_frames, idle_commits
        );
        r += &format!(
            "| graph | {} cells, {} inline evals, async {} started / {} done / {} cancelled, {} stream items, {} dropped |\n",
            self.page.graph.len(),
            g.inline_evals,
            g.async_started,
            g.async_completed,
            g.async_cancelled,
            g.stream_items,
            self.page.graph.stream_dropped()
        );
        r += &format!("| tiles rastered | {} |\n", s.tiles_rastered);
        r += &format!("| panel rasters uploaded | {} |\n", s.panel_rasters);
        r += &format!("| memory | {:.0} MB now, {:.0} MB peak |\n", rss_mb(), s.peak_rss.max(rss_mb()));
        r
    }

    // ------------------------------------------------------------ the frame

    fn frame(&mut self) {
        let t0 = Instant::now();
        if let Some(idle) = self.stats.leave_idle(t0) {
            if idle >= 1.0 {
                self.window.set_title(TITLE);
                self.title_idle_secs = 0;
            }
        }
        self.stats.commits += 1;
        let now_s = self.start.elapsed().as_secs_f32();
        self.bench_step(now_s);

        // inputs from the camera and the clock
        let view = self.camera.visible_world();
        let anim_visible = self.page.anim_on && (view.intersects(&page::EFFECT) || view.intersects(&page::ORBIT));
        if anim_visible {
            self.page.graph.set(self.page.t_anim, val(now_s));
        }
        let level = level_for_scale(self.camera.scale);
        if level != self.last_level {
            self.page.graph.set(self.page.cam_level, val(level));
            self.last_level = level;
        }

        // one commit for everything produced elsewhere since the last frame
        let rep = self.page.graph.commit();
        for (_, finished) in &rep.completed {
            self.pending_async.push(*finished);
        }
        if self.pending_async.len() > 256 {
            self.pending_async.clear();
        }

        let mut changed = self.camera_dirty || self.first;
        let mut deferred = false;
        let mut newest_stream: Option<Instant> = None;
        let mut async_shown = false;

        // panels: only visible ones are demanded, in priority order, within budget
        let mut order: Vec<usize> = (0..self.page.panels.len()).filter(|&i| self.page.panels[i].rect.intersects(&view)).collect();
        order.sort_by_key(|&i| self.page.panels[i].priority);
        for i in order {
            let (id, cell, class) = {
                let p = &self.page.panels[i];
                (p.id, p.cell, p.class)
            };
            if ms_since(t0) > FRAME_BUDGET_MS && self.page.graph.is_dirty(cell) {
                deferred = true;
                continue;
            }
            let inp = self.page.graph.demand(cell);
            if self.uploaded.get(&id) == Some(&inp.version) {
                continue;
            }
            if let Some(img) = inp.get::<Image>() {
                self.gpu.upload(TexKey::Panel(id), img.w, img.h, &img.rgba, inp.version, true);
                self.uploaded.insert(id, inp.version);
                self.stats.panel_rasters += 1;
                changed = true;
                if class == Class::PushStream {
                    if let Some(n) = img.newest {
                        newest_stream = Some(newest_stream.map_or(n, |m: Instant| m.max(n)));
                    }
                }
                if class == Class::Async {
                    async_shown = true;
                }
            }
        }

        // static tiles: parameters come from the graph, their version keys the tiles
        let mp = self.page.graph.demand(self.page.mandel_params);
        let ver = mp.version as u32;
        if let Some(p) = mp.get::<MandelParams>() {
            self.tiles.source.publish(ver, *p);
        }
        let mut tile_lat: Vec<Instant> = Vec::new();
        let mut uploads = 0;
        while uploads < 64 && ms_since(t0) < FRAME_BUDGET_MS + 3.0 {
            let Ok(done) = self.tiles.rx.try_recv() else { break };
            self.tiles.queue.complete(done.req);
            let Some(rgba) = done.rgba else { continue };
            self.stats.tile_ms.push(done.ms);
            self.stats.tiles_rastered += 1;
            let key = TexKey::Tile(done.req.key);
            if matches!(self.gpu.tex(&key), Some(t) if t.ver > done.req.ver as u64) {
                continue;
            }
            let pinned = done.req.key.level == self.tiles.ov_level;
            self.gpu.upload(key, TILE_SIZE, TILE_SIZE, &rgba, done.req.ver as u64, pinned);
            uploads += 1;
            changed = true;
            if let Some(t) = self.tiles.requested.remove(&done.req) {
                tile_lat.push(t);
            }
        }
        if self.tiles.requested.len() > 8192 {
            self.tiles.requested.retain(|r, _| r.ver == ver);
        }

        let ov = self.tiles.ov_level;
        let tile_level = level.clamp(ov, MAX_LEVEL);
        let mut overview = Vec::new();
        visible_tiles(&self.tiles.layout, &page::MANDEL, ov, &mut overview);
        let mut visible = Vec::new();
        visible_tiles(&self.tiles.layout, &view, tile_level, &mut visible);
        let (cx, cy) = view.center();
        let layout = &self.tiles.layout;
        visible.sort_by(|a, b| {
            let da = tile_world_rect(layout, *a).center();
            let db = tile_world_rect(layout, *b).center();
            let fa = (da.0 - cx).powi(2) + (da.1 - cy).powi(2);
            let fb = (db.0 - cx).powi(2) + (db.1 - cy).powi(2);
            fa.partial_cmp(&fb).unwrap_or(std::cmp::Ordering::Equal)
        });
        let stale = |gpu: &Gpu, k: &TileKey| gpu.tex(&TexKey::Tile(*k)).map_or(true, |t| t.ver != ver as u64);
        let mut wanted: Vec<TileReq> = Vec::new();
        for k in overview.iter().chain(visible.iter()) {
            if stale(&self.gpu, k) {
                wanted.push(TileReq { ver, key: *k });
            }
        }
        let mut ring = Vec::new();
        visible_tiles(&self.tiles.layout, &view.inflate(tile_world_size(tile_level)), tile_level, &mut ring);
        for k in ring {
            if !visible.contains(&k) && stale(&self.gpu, &k) {
                wanted.push(TileReq { ver, key: k });
            }
        }
        let now = Instant::now();
        for r in &wanted {
            self.tiles.requested.entry(*r).or_insert(now);
        }
        self.tiles.queue.publish(wanted.iter().copied());
        let settled = view.intersects(&page::MANDEL) && visible.iter().all(|k| !stale(&self.gpu, k));

        let must_render = changed || anim_visible;
        if !must_render {
            self.stats.skipped += 1;
            self.needs_frame = deferred;
            return;
        }

        // ---- draw list: tiles (best available per key), panels, GPU effect, HUD
        let mut verts: Vec<Vertex> = Vec::with_capacity(8192);
        let mut draws: Vec<Draw> = Vec::new();
        let tile_quad = |gpu: &mut Gpu, verts: &mut Vec<Vertex>, draws: &mut Vec<Draw>, geom: TileKey, tex: TileKey, uv: [f32; 4]| {
            let r = tile_world_rect(layout, geom);
            let Some(clip) = r.intersection(&page::MANDEL) else { return };
            let u0 = uv[0] + (clip.x - r.x) / r.w * uv[2];
            let v0 = uv[1] + (clip.y - r.y) / r.h * uv[3];
            let uv = [u0, v0, clip.w / r.w * uv[2], clip.h / r.h * uv[3]];
            let range = push_quad(verts, clip, uv);
            gpu.touch(&TexKey::Tile(tex));
            draws.push(Draw::Quads { key: TexKey::Tile(tex), range, screen: false });
        };
        if view.intersects(&page::MANDEL) {
            for k in &overview {
                if self.gpu.tex(&TexKey::Tile(*k)).is_some() {
                    tile_quad(&mut self.gpu, &mut verts, &mut draws, *k, *k, [0.0, 0.0, 1.0, 1.0]);
                }
            }
            if tile_level > ov {
                for k in &visible {
                    if self.gpu.tex(&TexKey::Tile(*k)).is_some() {
                        tile_quad(&mut self.gpu, &mut verts, &mut draws, *k, *k, [0.0, 0.0, 1.0, 1.0]);
                        continue;
                    }
                    for d in 1..(tile_level - ov) {
                        let (a, uv) = ancestor(*k, d);
                        if self.gpu.tex(&TexKey::Tile(a)).is_some() {
                            tile_quad(&mut self.gpu, &mut verts, &mut draws, *k, a, uv);
                            break;
                        }
                    }
                }
            }
        }

        let panels: Vec<(u32, Rect, bool)> = self.page.panels.iter().map(|p| (p.id, p.rect, p.after_effect)).collect();
        for &(id, rect, after) in &panels {
            if !after && rect.intersects(&view) && self.gpu.tex(&TexKey::Panel(id)).is_some() {
                let range = push_quad(&mut verts, rect, [0.0, 0.0, 1.0, 1.0]);
                draws.push(Draw::Quads { key: TexKey::Panel(id), range, screen: false });
            }
        }
        let effect = if view.intersects(&page::EFFECT) {
            let ep = self.page.graph.demand(self.page.effect_params);
            let p = ep.get::<EffectParams>().copied().unwrap_or(EffectParams { t: 0.0, hue: 0.0 });
            draws.push(Draw::Effect);
            let e = page::EFFECT;
            Some(EffectUniform { rect: [e.x, e.y, e.w, e.h], t: p.t, hue: p.hue, _pad: [0.0; 2] })
        } else {
            None
        };
        for &(id, rect, after) in &panels {
            if after && rect.intersects(&view) && self.gpu.tex(&TexKey::Panel(id)).is_some() {
                let range = push_quad(&mut verts, rect, [0.0, 0.0, 1.0, 1.0]);
                draws.push(Draw::Quads { key: TexKey::Panel(id), range, screen: false });
            }
        }

        if self.hud_at.elapsed() >= Duration::from_millis(250) || self.first {
            self.hud_at = Instant::now();
            self.hud_ver += 1;
            let img = self.hud_image(tile_level);
            self.gpu.upload(TexKey::Hud, img.w, img.h, &img.rgba, self.hud_ver, true);
        }
        if let Some(t) = self.gpu.tex(&TexKey::Hud).filter(|_| self.hud_visible) {
            let (_, sh) = self.gpu.size();
            let r = Rect::new(12.0, sh as f32 - t.h as f32 - 12.0, t.w as f32, t.h as f32);
            let range = push_quad(&mut verts, r, [0.0, 0.0, 1.0, 1.0]);
            draws.push(Draw::Quads { key: TexKey::Hud, range, screen: true });
        }

        let cpu_ms = ms_since(t0);
        let (w, h) = self.gpu.size();
        let world = CameraUniform::new(self.camera.scale, self.camera.tx, self.camera.ty, w as f32, h as f32);
        let screen = CameraUniform::new(1.0, 0.0, 0.0, w as f32, h as f32);
        match self.gpu.render(world, screen, effect, &verts, &draws, CLEAR) {
            Ok(()) => {}
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                let (w, h) = self.gpu.size();
                self.gpu.resize(w, h);
            }
            Err(e) => eprintln!("surface error: {e:?}"),
        }
        let presented = Instant::now();
        self.stats.on_present(presented, cpu_ms);
        let lat = |t: Instant| (presented - t).as_secs_f32() * 1000.0;
        if let Some(t) = self.pending_op.take() {
            self.stats.lat_ops.push(lat(t));
        }
        if let Some(t) = newest_stream {
            self.stats.lat_stream.push(lat(t));
        }
        if async_shown {
            for t in self.pending_async.drain(..) {
                self.stats.lat_async.push(lat(t));
            }
        }
        for t in tile_lat {
            self.stats.lat_tile.push(lat(t));
        }
        if settled && self.stats.first_settled_ms.is_none() {
            self.stats.first_settled_ms = Some((presented - self.start).as_secs_f32() * 1000.0);
        }
        self.gpu.evict_tiles_over_budget();
        self.camera_dirty = false;
        self.first = false;
        let bench_moving = self.bench.as_ref().map_or(false, |b| b.continuous(now_s));
        self.needs_frame = anim_visible || deferred || bench_moving;
    }

    fn hud_image(&self, tile_level: i32) -> Image {
        let s = &self.stats;
        let g = &self.page.graph;
        let gs = &g.stats;
        let (tiles_n, tiles_b) = self.gpu.tile_count_and_bytes();
        let q = self.tiles.queue.snapshot();
        let lines = [
            format!(
                "{:?} · {} · {:?}",
                self.gpu.adapter_info.backend,
                self.gpu.adapter_info.name,
                self.gpu.present_mode()
            ),
            format!(
                "{:.0} fps · 幀 CPU p50 {:.2} ms · p95 {:.2} ms · 已繪 {} 幀 · 沒有可見變化而跳過 {} 次",
                s.fps,
                s.frame_cpu.recent_pct(240, 50.0),
                s.frame_cpu.recent_pct(240, 95.0),
                s.frames,
                s.skipped
            ),
            format!(
                "計算圖 {} 格 · 行內求值 {} · 非同步 開始 {} 完成 {} 取消 {} · 串流 {} 筆 丟棄 {}",
                g.len(),
                gs.inline_evals,
                gs.async_started,
                gs.async_completed,
                gs.async_cancelled,
                gs.stream_items,
                g.stream_dropped()
            ),
            format!(
                "事件到上屏 p50：操作 {} · 串流 {} · 非同步 {} · tile {}",
                s.lat_ops.recent_ms(120),
                s.lat_stream.recent_ms(120),
                s.lat_async.recent_ms(60),
                s.lat_tile.recent_ms(400)
            ),
            format!(
                "tile L{} 縮放 {:.2} · 快取 {} 張 {} MB · 佇列 {} 進行中 {} · 已光柵 {} 張 平均 {:.1} ms · {} 條工作執行緒",
                tile_level,
                self.camera.scale,
                tiles_n,
                tiles_b / (1024 * 1024),
                q.wanted,
                q.inflight,
                s.tiles_rastered,
                s.tile_ms.mean(),
                self.tiles.workers
            ),
            format!("最長閒置 {:.1} 秒，期間 0 幀 · 記憶體 {:.0} MB", s.longest_idle_s, rss_mb()),
            "拖曳平移 · 滾輪縮放 · 空白鍵 串流 · A 動畫 · J 任務 · C 取消 · R 貼合 · H 隱藏面板 · 1/2 迭代 · Esc 離開".to_string(),
        ];
        let dpr = self.window.scale_factor() as f32;
        let (lw, lh) = (900.0, 20.0 + lines.len() as f32 * 21.0);
        let mut c = Canvas::new(lw, lh, dpr);
        c.clear([0, 0, 0, 0]);
        c.round_rect(0.0, 0.0, lw, lh, 10.0, [8, 10, 14, 215]);
        for (k, line) in lines.iter().enumerate() {
            let col = if k == 0 { [150, 200, 255, 255] } else if k == lines.len() - 1 { [150, 156, 170, 255] } else { [226, 230, 238, 255] };
            c.text(&self.text, 12.0, 9.0 + k as f32 * 21.0, 14.0, col, line);
        }
        c.into_image(None)
    }
}

struct Args {
    bench: bool,
    vsync: bool,
}

struct App {
    args: Args,
    proxy: Arc<Mutex<EventLoopProxy<UserEvent>>>,
    wake_flag: Arc<AtomicBool>,
    state: Option<State>,
}

impl App {
    fn wake(&self) -> Wake {
        let flag = self.wake_flag.clone();
        let proxy = self.proxy.clone();
        Arc::new(move || {
            if !flag.swap(true, Ordering::AcqRel) {
                let _ = proxy.lock().unwrap().send_event(UserEvent::Wake);
            }
        })
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        let attrs = Window::default_attributes().with_title(TITLE).with_inner_size(LogicalSize::new(1600.0, 1000.0));
        let window = Arc::new(el.create_window(attrs).expect("create window"));
        let st = State::new(window.clone(), self.wake(), self.args.vsync, self.args.bench);
        self.state = Some(st);
        window.request_redraw();
    }

    fn user_event(&mut self, _el: &ActiveEventLoop, _event: UserEvent) {
        self.wake_flag.store(false, Ordering::Release);
        if let Some(st) = &self.state {
            st.window.request_redraw();
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(st) = self.state.as_mut() else { return };
        match event {
            WindowEvent::CloseRequested => {
                st.page.shutdown();
                el.exit();
            }
            WindowEvent::Resized(size) => {
                st.gpu.resize(size.width, size.height);
                st.camera.set_viewport(size.width as f32, size.height as f32);
                st.camera_dirty = true;
                st.window.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                st.frame();
                if st.bench.as_ref().map_or(false, |b| b.done) {
                    let report = st.bench_report();
                    println!("\n{report}");
                    let _ = std::fs::create_dir_all("bench-results");
                    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                    let path = format!("bench-results/spike1-{stamp}.md");
                    if std::fs::write(&path, &report).is_ok() {
                        println!("written to {path}");
                    }
                    st.page.shutdown();
                    el.exit();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let (x, y) = (position.x as f32, position.y as f32);
                st.cursor = (x, y);
                match st.drag {
                    Some(Drag::Pan { last }) => {
                        st.camera.pan(x - last.0, y - last.1);
                        st.drag = Some(Drag::Pan { last: (x, y) });
                        st.camera_dirty = true;
                        st.op();
                        st.window.request_redraw();
                    }
                    Some(Drag::Slider(s)) => {
                        let (wx, _) = st.camera.screen_to_world(x, y);
                        let f = st.page.slider_frac(s, wx);
                        st.page.slider_value(s, f);
                        st.op();
                        st.window.request_redraw();
                    }
                    None => {}
                }
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => match state {
                ElementState::Pressed => {
                    let (wx, wy) = st.camera.screen_to_world(st.cursor.0, st.cursor.1);
                    match st.page.hit(wx, wy) {
                        Some(hit) => st.apply_hit(hit),
                        None => st.drag = Some(Drag::Pan { last: st.cursor }),
                    }
                    st.window.request_redraw();
                }
                ElementState::Released => st.drag = None,
            },
            WindowEvent::MouseWheel { delta, .. } => {
                let factor = match delta {
                    MouseScrollDelta::LineDelta(_, y) => 1.15f32.powf(y),
                    MouseScrollDelta::PixelDelta(p) => 1.0015f32.powf(p.y as f32),
                };
                st.camera.zoom_at(st.cursor.0, st.cursor.1, factor);
                st.camera_dirty = true;
                st.op();
                st.window.request_redraw();
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                match &event.logical_key {
                    Key::Named(NamedKey::Escape) => {
                        st.page.shutdown();
                        el.exit();
                        return;
                    }
                    Key::Named(NamedKey::Space) => st.apply_hit(Hit::Streams),
                    Key::Character(c) => match c.as_str() {
                        "r" | "R" => st.fit(),
                        "a" | "A" => st.apply_hit(Hit::Anim),
                        "j" | "J" => st.apply_hit(Hit::RunJobs),
                        "c" | "C" => st.apply_hit(Hit::CancelJobs),
                        "h" | "H" => {
                            st.hud_visible = !st.hud_visible;
                            st.camera_dirty = true;
                        }
                        "1" => {
                            let n = st.page.iterations_v / 2;
                            st.page.set_iterations(n);
                            st.op();
                        }
                        "2" => {
                            let n = st.page.iterations_v * 2;
                            st.page.set_iterations(n);
                            st.op();
                        }
                        _ => {}
                    },
                    _ => {}
                }
                st.window.request_redraw();
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let Some(st) = self.state.as_mut() else { return };
        if st.needs_frame {
            st.window.request_redraw();
            el.set_control_flow(ControlFlow::Wait);
            return;
        }
        if let Some(b) = &st.bench {
            if let Some(at) = b.next_time() {
                let when = st.start + Duration::from_secs_f32(at);
                if Instant::now() >= when {
                    st.window.request_redraw();
                    el.set_control_flow(ControlFlow::Wait);
                } else {
                    el.set_control_flow(ControlFlow::WaitUntil(when));
                }
                return;
            }
        }
        // Nothing scheduled: sleep. Only the window title ticks once a second,
        // which is an OS call, not a rendered frame.
        let now = Instant::now();
        st.stats.enter_idle(now);
        let idle_start = st.stats.idle_start.unwrap_or(now);
        let secs = (now - idle_start).as_secs();
        if secs >= 1 && secs != st.title_idle_secs {
            st.title_idle_secs = secs;
            st.window.set_title(&format!("{TITLE} — 閒置 {secs} 秒，0 幀"));
        }
        el.set_control_flow(ControlFlow::WaitUntil(idle_start + Duration::from_secs(secs + 1)));
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let args = Args { bench: argv.iter().any(|a| a == "--bench"), vsync: !argv.iter().any(|a| a == "--no-vsync") };
    let event_loop = EventLoop::<UserEvent>::with_user_event().build().expect("event loop");
    let proxy = Arc::new(Mutex::new(event_loop.create_proxy()));
    let mut app = App { args, proxy, wake_flag: Arc::new(AtomicBool::new(false)), state: None };
    event_loop.run_app(&mut app).expect("run app");
}
