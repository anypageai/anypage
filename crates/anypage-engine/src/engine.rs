//! The engine loop (docs/ARCHITECTURE.md sections 3.2, 3.3, 7, 8).
//!
//! Each frame:
//! 1. collect the sinks the camera can see (panels, GPU operators, tile layers);
//! 2. advance the clock only if one of those sinks is due ([`Graph::next_expiry`]);
//! 3. commit everything produced on other threads in one batch;
//! 4. demand visible sinks in priority order within the frame budget,
//!    continuing unfinished work next frame;
//! 5. composite, but only if something visible changed;
//! 6. tell the host when to call again: next frame, at a predicted time, or
//!    never (until input or a wake from another thread).
//!
//! Nothing is hard-coded as "animated": a panel keeps the engine at 60 fps
//! only while the function behind it says its value changes every frame.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anypage_core::{
    ancestor, level_for_scale, tile_world_rect, tile_world_size, val, visible_tiles, Camera, CellId, Rect, TileKey, Wake,
    MAX_LEVEL, TILE_SIZE,
};

use crate::gpu::{CameraUniform, Draw, EffectUniform, Gpu, Target, TexKey, Vertex};
use crate::page::{Class, EffectParams, HostResult, Page, PageRequest, PointerResponse};
use crate::raster::{Canvas, Image, TextRaster};
use crate::stats::{rss_mb, Stats};
use crate::tiles::{TileReq, TileSystem};

const CLEAR: wgpu::Color = wgpu::Color { r: 0.006, g: 0.007, b: 0.010, a: 1.0 };
const DUE_EPS: f64 = 0.0005;

pub struct EngineConfig {
    pub vsync: bool,
    pub tile_workers: usize,
    pub tile_budget_mb: usize,
    pub frame_budget_ms: f32,
    pub show_hud: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        // the measurement panel is for development: hosts turn it on (h toggles it)
        Self { vsync: true, tile_workers: cpus.saturating_sub(2).clamp(2, 24), tile_budget_mb: 256, frame_budget_ms: 10.0, show_hud: false }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FrameOutcome {
    /// A frame was presented.
    pub rendered: bool,
    /// Call `frame` again as soon as possible (animation, or deferred work).
    pub continuous: bool,
    /// Otherwise, call `frame` at this time: the earliest moment something
    /// visible is predicted to change because of time.
    pub wake_at: Option<Instant>,
}

#[derive(Clone, Copy)]
enum Mode {
    Idle,
    Pan,
    Page,
    Pinch { dist: f32, mid: (f32, f32) },
}

type PanelInfo = (u32, Rect, CellId, Class, u8, bool);

pub struct Engine<P: Page> {
    gpu: Gpu,
    page: P,
    camera: Camera,
    tiles: TileSystem,
    text: Arc<TextRaster>,
    stats: Stats,
    /// GPU pipeline per page effect; `None` if the device rejected its shader.
    effects: Vec<Option<usize>>,
    effect_versions: Vec<u64>,
    pointers: Vec<(u64, f32, f32)>,
    mode: Mode,
    pending_op: Option<Instant>,
    camera_dirty: bool,
    first: bool,
    last_level: i32,
    uploaded: HashMap<u32, u64>,
    /// Newest stream item each panel has shown; latency counts only new data.
    shown_newest: HashMap<u32, Instant>,
    pending_async: Vec<Instant>,
    hud_at: Instant,
    hud_ver: u64,
    pub hud_visible: bool,
    start: Instant,
    scale_factor: f32,
    cfg: EngineConfig,
    last: FrameOutcome,
    next_desc: String,
    /// The previous frame was part of back-to-back drawing (animation or camera motion).
    prev_moving: bool,
    /// (cell version, pixel size) each GPU layer was last rendered at.
    layer_drawn: HashMap<u32, (u64, u32, u32)>,
    /// Requests only the host can serve (clipboard, file dialogs).
    host_requests: Vec<PageRequest>,
    /// Last pointer position, for hover.
    hover: (f32, f32),
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

impl<P: Page> Engine<P> {
    pub fn new(target: &Target, size: (u32, u32), scale_factor: f32, page: P, text: Arc<TextRaster>, wake: Wake, cfg: EngineConfig) -> Result<Self, String> {
        let start = Instant::now();
        let mut gpu = Gpu::new(target, size.0, size.1, cfg.vsync, cfg.tile_budget_mb << 20)?;
        let effects: Vec<Option<usize>> = page
            .spec()
            .effects
            .iter()
            .enumerate()
            .map(|(k, e)| match gpu.add_effect(&e.wgsl) {
                Ok(i) => Some(i),
                Err(err) => {
                    log::warn!("GPU operator {k} disabled: {}", err.lines().map(str::trim).filter(|l| !l.is_empty()).take(6).collect::<Vec<_>>().join(" | "));
                    None
                }
            })
            .collect();
        let tiles = TileSystem::new(&page.spec().tiled, wake, cfg.tile_workers);
        let (w, h) = gpu.size();
        let mut camera = Camera::new(w as f32, h as f32);
        camera.fit_rect(page.spec().bounds, 24.0 * scale_factor);
        let n_fx = effects.len();
        let mut engine = Self {
            gpu,
            page,
            camera,
            tiles,
            text,
            stats: Stats::new(start),
            effects,
            effect_versions: vec![0; n_fx],
            pointers: Vec::new(),
            mode: Mode::Idle,
            pending_op: None,
            camera_dirty: true,
            first: true,
            last_level: i32::MIN,
            uploaded: HashMap::new(),
            shown_newest: HashMap::new(),
            pending_async: Vec::new(),
            hud_at: start,
            hud_ver: 0,
            hud_visible: cfg.show_hud,
            start,
            scale_factor,
            cfg,
            last: FrameOutcome::default(),
            next_desc: String::new(),
            prev_moving: false,
            layer_drawn: HashMap::new(),
            host_requests: Vec::new(),
            hover: (0.0, 0.0),
        };
        log::info!("{}", engine.summary());
        engine.notify_viewport();
        Ok(engine)
    }

    /// Tell the page the window size in logical pixels, so it can pick a
    /// layout (and fit it with `page.fit_rect`).
    fn notify_viewport(&mut self) {
        let (w, h) = self.gpu.size();
        let s = self.scale_factor.max(0.1);
        self.page.viewport(w as f32 / s, h as f32 / s, s);
        self.drain_requests();
    }

    // ------------------------------------------------------------ host API

    pub fn summary(&self) -> String {
        format!(
            "AnyPage engine · {:?} · {} · {:?} · font {} · {} tile workers",
            self.gpu.adapter_info.backend,
            self.gpu.adapter_info.name,
            self.gpu.present_mode(),
            self.text.source,
            self.tiles.workers
        )
    }
    pub fn adapter_name(&self) -> &str {
        &self.gpu.adapter_info.name
    }
    pub fn backend(&self) -> String {
        format!("{:?}", self.gpu.adapter_info.backend)
    }
    pub fn present_mode(&self) -> String {
        format!("{:?}", self.gpu.present_mode())
    }
    pub fn surface_size(&self) -> (u32, u32) {
        self.gpu.size()
    }
    pub fn tile_workers(&self) -> usize {
        self.tiles.workers
    }
    pub fn start(&self) -> Instant {
        self.start
    }
    pub fn stats(&self) -> &Stats {
        &self.stats
    }
    pub fn last(&self) -> FrameOutcome {
        self.last
    }
    pub fn next_wake_desc(&self) -> &str {
        &self.next_desc
    }
    pub fn page(&self) -> &P {
        &self.page
    }
    pub fn page_mut(&mut self) -> &mut P {
        &mut self.page
    }
    pub fn camera(&self) -> &Camera {
        &self.camera
    }
    /// Scripted camera moves count as operations for latency statistics.
    pub fn camera_mut(&mut self) -> &mut Camera {
        self.camera_dirty = true;
        self.mark_op();
        &mut self.camera
    }
    pub fn mark_op(&mut self) {
        if self.pending_op.is_none() {
            self.pending_op = Some(Instant::now());
        }
    }
    pub fn fit(&mut self) {
        let b = self.page.spec().bounds;
        self.fit_rect(b);
    }
    pub fn fit_rect(&mut self, r: Rect) {
        self.camera.fit_rect(r, 24.0 * self.scale_factor);
        self.camera_dirty = true;
        self.mark_op();
    }
    pub fn resize(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        self.gpu.resize(w, h);
        self.camera.set_viewport(w as f32, h as f32);
        self.camera_dirty = true;
        self.notify_viewport();
    }
    pub fn set_scale_factor(&mut self, f: f32) {
        self.scale_factor = f.max(0.5);
        self.hud_at = self.start;
        self.camera_dirty = true;
        self.notify_viewport();
    }
    pub fn attach_surface(&mut self, target: &Target, w: u32, h: u32) -> Result<(), String> {
        self.gpu.attach_surface(target, w, h)?;
        self.camera.set_viewport(w as f32, h as f32);
        self.camera_dirty = true;
        self.notify_viewport();
        Ok(())
    }
    pub fn detach_surface(&mut self) {
        self.gpu.detach_surface();
    }
    pub fn enter_idle(&mut self, now: Instant) {
        self.stats.enter_idle(now);
    }
    /// The platform asked for a repaint (exposed, uncovered, captured): the
    /// next frame must present even if nothing on the page changed.
    pub fn invalidate(&mut self) {
        self.first = true;
    }
    pub fn shutdown(&mut self) {
        self.page.shutdown();
    }

    // ------------------------------------------------------------ input

    fn pinch_metrics(&self) -> (f32, (f32, f32)) {
        let (_, ax, ay) = self.pointers[0];
        let (_, bx, by) = self.pointers[1];
        (((ax - bx).powi(2) + (ay - by).powi(2)).sqrt(), ((ax + bx) * 0.5, (ay + by) * 0.5))
    }

    fn after_input(&mut self) {
        self.mark_op();
        self.drain_requests();
    }

    fn drain_requests(&mut self) {
        while let Some(r) = self.page.take_request() {
            match r {
                PageRequest::FitPage => self.fit(),
                PageRequest::FitRect(rect) => self.fit_rect(rect),
                other => self.host_requests.push(other),
            }
        }
    }

    /// Clipboard and file requests from the page, for the host to serve.
    pub fn take_host_request(&mut self) -> Option<PageRequest> {
        self.drain_requests();
        if self.host_requests.is_empty() {
            None
        } else {
            Some(self.host_requests.remove(0))
        }
    }

    /// Where the page's caret is on screen (physical pixels: x, y, w, h)
    /// while it wants text input; `None` otherwise.
    pub fn text_focus(&self) -> Option<(f32, f32, f32, f32)> {
        let r = self.page.text_focus()?;
        if ![r.x, r.y, r.w, r.h].iter().all(|v| v.is_finite() && v.abs() < 1.0e9) {
            return None;
        }
        let (x0, y0) = self.camera.world_to_screen(r.x, r.y);
        let (x1, y1) = self.camera.world_to_screen(r.right(), r.bottom());
        Some((x0, y0, (x1 - x0).max(1.0), (y1 - y0).max(1.0)))
    }

    pub fn text_input(&mut self, text: &str) {
        self.page.text_input(text);
        self.after_input();
    }
    pub fn ime_preedit(&mut self, text: &str) {
        self.page.ime_preedit(text);
        self.after_input();
    }
    pub fn paste(&mut self, text: &str) {
        self.page.paste(text);
        self.after_input();
    }
    pub fn file_drop(&mut self, name: &str, bytes: Arc<Vec<u8>>) {
        self.page.file_drop(name, bytes);
        self.after_input();
    }
    pub fn host_result(&mut self, id: u64, result: HostResult) {
        self.page.host_result(id, result);
        self.after_input();
    }

    /// Physical pixels. Mouse buttons and touch points both come through here.
    pub fn pointer_down(&mut self, id: u64, x: f32, y: f32) {
        self.pointers.retain(|p| p.0 != id);
        self.pointers.push((id, x, y));
        match self.pointers.len() {
            1 => {
                let (wx, wy) = self.camera.screen_to_world(x, y);
                self.mode = match self.page.pointer_down(wx, wy) {
                    PointerResponse::Captured => Mode::Page,
                    PointerResponse::Handled => Mode::Idle,
                    PointerResponse::Ignored => Mode::Pan,
                };
            }
            2 => {
                if matches!(self.mode, Mode::Page) {
                    self.page.pointer_cancel();
                }
                let (dist, mid) = self.pinch_metrics();
                self.mode = Mode::Pinch { dist, mid };
            }
            _ => {}
        }
        self.after_input();
    }

    pub fn pointer_move(&mut self, id: u64, x: f32, y: f32) {
        let Some(k) = self.pointers.iter().position(|p| p.0 == id) else {
            // no button held: hover
            if self.pointers.is_empty() && (x, y) != self.hover {
                self.hover = (x, y);
                let (wx, wy) = self.camera.screen_to_world(x, y);
                self.page.pointer_move(wx, wy);
                self.drain_requests();
            }
            return;
        };
        let (_, px, py) = self.pointers[k];
        self.pointers[k] = (id, x, y);
        match self.mode {
            Mode::Idle => return,
            Mode::Pan => {
                self.camera.pan(x - px, y - py);
                self.camera_dirty = true;
            }
            Mode::Page => {
                let (wx, wy) = self.camera.screen_to_world(x, y);
                self.page.pointer_drag(wx, wy);
            }
            Mode::Pinch { dist, mid } => {
                if self.pointers.len() < 2 {
                    return;
                }
                let (d, m) = self.pinch_metrics();
                // keep the world point under the old midpoint under the new one, then scale around it
                self.camera.pan(m.0 - mid.0, m.1 - mid.1);
                if dist > 1.0 {
                    self.camera.zoom_at(m.0, m.1, d / dist);
                }
                self.mode = Mode::Pinch { dist: d, mid: m };
                self.camera_dirty = true;
            }
        }
        self.after_input();
    }

    pub fn pointer_up(&mut self, id: u64) {
        self.pointers.retain(|p| p.0 != id);
        match (self.pointers.len(), self.mode) {
            (0, Mode::Page) => self.page.pointer_up(),
            (1, Mode::Pinch { .. }) => self.mode = Mode::Pan,
            _ => {}
        }
        if self.pointers.is_empty() {
            self.mode = Mode::Idle;
        }
        self.after_input();
    }

    pub fn pointer_cancel(&mut self) {
        if matches!(self.mode, Mode::Page) {
            self.page.pointer_cancel();
        }
        self.pointers.clear();
        self.mode = Mode::Idle;
    }

    /// Mouse wheel or trackpad scroll at a screen point, in lines (positive
    /// `dy` = wheel up). The page may consume it (a scrolling list);
    /// otherwise it zooms the camera.
    pub fn wheel(&mut self, x: f32, y: f32, dx: f32, dy: f32) {
        let (wx, wy) = self.camera.screen_to_world(x, y);
        if !self.page.wheel(wx, wy, dx, dy) {
            self.camera.zoom_at(x, y, 1.15f32.powf(dy));
            self.camera_dirty = true;
        }
        self.after_input();
    }

    /// Key names as in [`Page::key`]. The page sees every key first; the
    /// engine's own keys ("h" HUD, "r" fit) apply only if the page did not
    /// take the key and is not editing text.
    pub fn key(&mut self, name: &str) -> bool {
        let mut handled = self.page.key(name);
        if !handled && self.page.text_focus().is_none() {
            handled = match name {
                "h" => {
                    self.hud_visible = !self.hud_visible;
                    self.camera_dirty = true;
                    true
                }
                "r" => {
                    self.fit();
                    true
                }
                _ => false,
            };
        }
        self.after_input();
        handled
    }

    // ------------------------------------------------------------ the frame

    pub fn frame(&mut self) -> FrameOutcome {
        let t0 = Instant::now();
        if !self.gpu.has_surface() {
            self.last = FrameOutcome::default();
            return self.last;
        }
        self.stats.leave_idle(t0);
        self.stats.commits += 1;
        let now_s = self.start.elapsed().as_secs_f64();
        let deadline = t0 + Duration::from_secs_f32(self.cfg.frame_budget_ms / 1000.0);
        let view = self.camera.visible_world();
        let level = level_for_scale(self.camera.scale);

        let (panels, effects, layers, gpu_layers, cam_cell) = {
            let spec = self.page.spec();
            let panels: Vec<PanelInfo> = spec.panels.iter().map(|p| (p.id, p.rect, p.cell, p.class, p.priority, p.over_effects)).collect();
            let effects: Vec<(Rect, CellId)> = spec.effects.iter().map(|e| (e.rect, e.params)).collect();
            let layers: Vec<(Rect, CellId)> = spec.tiled.iter().map(|l| (l.rect, l.params)).collect();
            let gpu_layers: Vec<(u32, Rect, CellId, Arc<dyn crate::page::GpuRenderer>)> =
                spec.layers.iter().map(|l| (l.id, l.rect, l.cell, l.renderer.clone())).collect();
            (panels, effects, layers, gpu_layers, spec.camera_level)
        };
        let mut sinks: Vec<CellId> = Vec::new();
        sinks.extend(panels.iter().filter(|p| p.1.intersects(&view)).map(|p| p.2));
        sinks.extend(gpu_layers.iter().filter(|l| l.1.intersects(&view)).map(|l| l.2));
        sinks.extend(effects.iter().filter(|e| e.0.intersects(&view)).map(|e| e.1));
        sinks.extend(layers.iter().filter(|l| l.0.intersects(&view)).map(|l| l.1));

        let graph = self.page.parts().0;
        if level != self.last_level {
            if let Some(c) = cam_cell {
                graph.set(c, val(level));
            }
            self.last_level = level;
        }
        // time: advance the clock only if something visible is due now
        // (half a millisecond of tolerance for timer wake-ups that land early)
        if graph.next_expiry(&sinks) <= now_s + DUE_EPS {
            graph.tick(now_s);
        }
        let rep = graph.commit();
        for (_, finished) in &rep.completed {
            self.pending_async.push(*finished);
        }
        if self.pending_async.len() > 256 {
            self.pending_async.clear();
        }
        // let the page react to finished jobs and new stream items
        if !rep.completed.is_empty() || !rep.streams_changed.is_empty() {
            let ids: Vec<CellId> = rep.completed.iter().map(|c| c.0).collect();
            self.page.after_commit(&ids);
            self.drain_requests();
        }
        let graph = self.page.parts().0;

        let camera_moved = self.camera_dirty;
        let mut changed = self.camera_dirty || self.first;
        let mut deferred = false;
        let mut newest_stream: Option<Instant> = None;
        let mut async_shown = false;

        // panels: visible only, priority order, within the frame budget
        let mut order: Vec<usize> = (0..panels.len()).filter(|&i| panels[i].1.intersects(&view)).collect();
        order.sort_by_key(|&i| panels[i].4);
        for i in order {
            let (id, _, cell, class, _, _) = panels[i];
            if graph.is_dirty(cell) && Instant::now() >= deadline {
                deferred = true;
                continue;
            }
            let Some(inp) = graph.demand_until(cell, deadline) else {
                deferred = true;
                continue;
            };
            if self.uploaded.get(&id) == Some(&inp.version) {
                continue;
            }
            if let Some(img) = inp.get::<Image>() {
                self.gpu.upload(TexKey::Panel(id), img.w, img.h, &img.rgba, inp.version, true);
                self.uploaded.insert(id, inp.version);
                self.stats.panel_rasters += 1;
                changed = true;
                if class == Class::PushStream {
                    // a redraw for another reason (cursor blink) is not stream latency
                    if let Some(n) = img.newest.filter(|n| self.shown_newest.get(&id) != Some(n)) {
                        self.shown_newest.insert(id, n);
                        newest_stream = Some(newest_stream.map_or(n, |m: Instant| m.max(n)));
                    }
                }
                if class == Class::Async {
                    async_shown = true;
                }
            }
        }

        // GPU layers: native renderers into cached offscreen textures, redrawn
        // only when their value or their size on screen changed
        let max_tex = self.gpu.max_texture().min(4096) as f32;
        let mut layers_shown: Vec<(u32, Rect)> = Vec::new();
        for (id, rect, cell, renderer) in &gpu_layers {
            if !rect.intersects(&view) {
                continue;
            }
            let graph = self.page.parts().0;
            let inp = match graph.demand_until(*cell, deadline) {
                Some(i) => i,
                None => {
                    deferred = true;
                    continue;
                }
            };
            let Some(value) = inp.value.clone() else { continue };
            let s = (self.camera.scale * (max_tex / (rect.w * self.camera.scale)).min(max_tex / (rect.h * self.camera.scale)).min(1.0)).max(0.01);
            let (pw, ph) = (((rect.w * s).round() as u32).max(1), ((rect.h * s).round() as u32).max(1));
            if self.layer_drawn.get(id) != Some(&(inp.version, pw, ph)) {
                match self.gpu.render_layer(TexKey::Layer(*id), pw, ph, inp.version, renderer.as_ref(), &value) {
                    Ok(()) => {}
                    Err(e) => log::warn!("GPU layer {id}: {}", e.lines().next().unwrap_or("")),
                }
                self.layer_drawn.insert(*id, (inp.version, pw, ph));
                changed = true;
            }
            layers_shown.push((*id, *rect));
        }
        let graph = self.page.parts().0;

        // GPU operators: the cell holds the uniforms, the compositor runs the shader
        let mut fx_uniforms: Vec<(usize, EffectUniform)> = Vec::new();
        for (k, (rect, cell)) in effects.iter().enumerate() {
            if !rect.intersects(&view) {
                continue;
            }
            let inp = graph.demand(*cell);
            let p = inp.get::<EffectParams>().copied().unwrap_or(EffectParams([0.0; 8]));
            if self.effect_versions[k] != inp.version {
                self.effect_versions[k] = inp.version;
                changed = true;
            }
            if let Some(fx) = self.effects[k] {
                fx_uniforms.push((fx, EffectUniform { rect: [rect.x, rect.y, rect.w, rect.h], p: p.0 }));
            }
        }

        // tiles: finished ones first (budgeted), then what is wanted now
        let mut tile_lat: Vec<Instant> = Vec::new();
        let mut layer_versions: Vec<u32> = vec![0; layers.len()];
        for (li, (rect, params)) in layers.iter().enumerate() {
            if rect.intersects(&view) {
                let mp = graph.demand(*params);
                layer_versions[li] = mp.version as u32;
                if let Some(v) = &mp.value {
                    self.tiles.publish_params(li, mp.version as u32, v);
                }
            }
        }
        // Always take some finished tiles, even when the frame is over budget:
        // their wake-up was already spent, so results left in the channel
        // would wait for the next input. If more may be waiting, ask for
        // another frame.
        let mut uploads = 0;
        loop {
            if uploads >= 64 || (uploads >= 8 && Instant::now() >= deadline + Duration::from_millis(3)) {
                deferred = true;
                break;
            }
            let Ok(done) = self.tiles.rx.try_recv() else { break };
            self.tiles.queue.complete(done.req);
            let Some(rgba) = done.rgba else { continue };
            self.stats.tile_ms.push(done.ms);
            self.stats.tiles_rastered += 1;
            let key = TexKey::Tile(done.req.layer, done.req.key);
            if matches!(self.gpu.tex(&key), Some(t) if t.ver > done.req.ver as u64) {
                continue;
            }
            let pinned = done.req.key.level == self.tiles.ov_levels[done.req.layer as usize];
            self.gpu.upload(key, TILE_SIZE, TILE_SIZE, &rgba, done.req.ver as u64, pinned);
            uploads += 1;
            changed = true;
            if let Some(t) = self.tiles.requested.remove(&done.req) {
                tile_lat.push(t);
            }
        }
        if self.tiles.requested.len() > 8192 {
            self.tiles.requested.clear();
        }

        let mut wanted: Vec<TileReq> = Vec::new();
        let mut prefetch: Vec<TileReq> = Vec::new();
        let mut layer_draw: Vec<(usize, i32, Vec<TileKey>, Vec<TileKey>)> = Vec::new();
        let mut settled = true;
        for (li, (rect, _)) in layers.iter().enumerate() {
            if !rect.intersects(&view) {
                continue;
            }
            let ver = layer_versions[li];
            let layout = &self.tiles.layouts[li];
            let ov = self.tiles.ov_levels[li];
            let tile_level = level.clamp(ov, MAX_LEVEL);
            let mut overview = Vec::new();
            visible_tiles(layout, rect, ov, &mut overview);
            let mut visible = Vec::new();
            visible_tiles(layout, &view, tile_level, &mut visible);
            let (cx, cy) = view.center();
            visible.sort_by(|a, b| {
                let da = tile_world_rect(layout, *a).center();
                let db = tile_world_rect(layout, *b).center();
                let fa = (da.0 - cx).powi(2) + (da.1 - cy).powi(2);
                let fb = (db.0 - cx).powi(2) + (db.1 - cy).powi(2);
                fa.partial_cmp(&fb).unwrap_or(std::cmp::Ordering::Equal)
            });
            let gpu = &self.gpu;
            let stale = |k: &TileKey| gpu.tex(&TexKey::Tile(li as u16, *k)).map_or(true, |t| t.ver != ver as u64);
            for k in overview.iter().chain(visible.iter()) {
                if stale(k) {
                    wanted.push(TileReq { layer: li as u16, ver, key: *k });
                }
            }
            if visible.iter().any(|k| stale(k)) {
                settled = false;
            }
            let mut ring = Vec::new();
            visible_tiles(layout, &view.inflate(tile_world_size(tile_level)), tile_level, &mut ring);
            for k in ring {
                if !visible.contains(&k) && stale(&k) {
                    prefetch.push(TileReq { layer: li as u16, ver, key: k });
                }
            }
            layer_draw.push((li, tile_level, overview, visible));
        }
        let now = Instant::now();
        for r in wanted.iter().chain(prefetch.iter()) {
            self.tiles.requested.entry(*r).or_insert(now);
        }
        self.tiles.queue.publish(wanted.iter().chain(prefetch.iter()).copied());

        // when does the visible page next change because of time?
        // Due already (an every-frame function of time) means continuous; a
        // future expiry means sleep until exactly then, never spin towards it.
        let next = graph.next_expiry(&sinks);
        let continuous = deferred || next <= now_s + DUE_EPS;
        let wake_at = if !continuous && next.is_finite() { Some(self.start + Duration::from_secs_f64(next.max(0.0))) } else { None };
        self.next_desc = if deferred {
            "下一幀（工作未做完，分幀繼續）".into()
        } else if continuous {
            "每幀（可見內容依賴時間）".into()
        } else if next.is_finite() {
            format!("{:.2} 秒後（可預測的時間函數）", next - now_s)
        } else {
            "無，等待輸入或資料".into()
        };

        let hud_due = self.hud_visible && (self.first || self.hud_at.elapsed() >= Duration::from_millis(250));
        if !changed {
            self.stats.skipped += 1;
            self.last = FrameOutcome { rendered: false, continuous, wake_at };
            return self.last;
        }

        // ---- draw list
        let mut verts: Vec<Vertex> = Vec::with_capacity(8192);
        let mut draws: Vec<Draw> = Vec::new();
        for (li, tile_level, overview, visible) in &layer_draw {
            let li = *li;
            let layout = &self.tiles.layouts[li];
            let rect = layers[li].0;
            let ov = self.tiles.ov_levels[li];
            let quad = |gpu: &mut Gpu, verts: &mut Vec<Vertex>, draws: &mut Vec<Draw>, geom: TileKey, tex: TileKey, uv: [f32; 4]| {
                let r = tile_world_rect(layout, geom);
                let Some(clip) = r.intersection(&rect) else { return };
                let u0 = uv[0] + (clip.x - r.x) / r.w * uv[2];
                let v0 = uv[1] + (clip.y - r.y) / r.h * uv[3];
                let uv = [u0, v0, clip.w / r.w * uv[2], clip.h / r.h * uv[3]];
                let range = push_quad(verts, clip, uv);
                let key = TexKey::Tile(li as u16, tex);
                gpu.touch(&key);
                draws.push(Draw::Quads { key, range, screen: false });
            };
            for k in overview {
                if self.gpu.tex(&TexKey::Tile(li as u16, *k)).is_some() {
                    quad(&mut self.gpu, &mut verts, &mut draws, *k, *k, [0.0, 0.0, 1.0, 1.0]);
                }
            }
            if *tile_level > ov {
                for k in visible {
                    if self.gpu.tex(&TexKey::Tile(li as u16, *k)).is_some() {
                        quad(&mut self.gpu, &mut verts, &mut draws, *k, *k, [0.0, 0.0, 1.0, 1.0]);
                        continue;
                    }
                    for d in 1..(*tile_level - ov) {
                        let (a, uv) = ancestor(*k, d);
                        if self.gpu.tex(&TexKey::Tile(li as u16, a)).is_some() {
                            quad(&mut self.gpu, &mut verts, &mut draws, *k, a, uv);
                            break;
                        }
                    }
                }
            }
        }
        for (id, rect) in &layers_shown {
            if self.gpu.tex(&TexKey::Layer(*id)).is_some() {
                let range = push_quad(&mut verts, *rect, [0.0, 0.0, 1.0, 1.0]);
                draws.push(Draw::Quads { key: TexKey::Layer(*id), range, screen: false });
            }
        }
        for over in [false, true] {
            if over {
                for (i, _) in &fx_uniforms {
                    draws.push(Draw::Effect(*i));
                }
            }
            for &(id, rect, _, _, _, o) in &panels {
                if o == over && rect.intersects(&view) && self.gpu.tex(&TexKey::Panel(id)).is_some() {
                    let range = push_quad(&mut verts, rect, [0.0, 0.0, 1.0, 1.0]);
                    draws.push(Draw::Quads { key: TexKey::Panel(id), range, screen: false });
                }
            }
        }
        if hud_due {
            self.hud_at = Instant::now();
            self.hud_ver += 1;
            let tl = layer_draw.first().map(|l| l.1).unwrap_or(level);
            let img = self.hud_image(tl);
            self.gpu.upload(TexKey::Hud, img.w, img.h, &img.rgba, self.hud_ver, true);
        }
        if self.hud_visible {
            if let Some(t) = self.gpu.tex(&TexKey::Hud) {
                let (_, sh) = self.gpu.size();
                let m = 12.0 * self.scale_factor;
                let r = Rect::new(m, sh as f32 - t.h as f32 - m, t.w as f32, t.h as f32);
                let range = push_quad(&mut verts, r, [0.0, 0.0, 1.0, 1.0]);
                draws.push(Draw::Quads { key: TexKey::Hud, range, screen: true });
            }
        }

        let cpu_ms = t0.elapsed().as_secs_f32() * 1000.0;
        let (w, h) = self.gpu.size();
        let world = CameraUniform::new(self.camera.scale, self.camera.tx, self.camera.ty, w as f32, h as f32);
        let screen = CameraUniform::new(1.0, 0.0, 0.0, w as f32, h as f32);
        let rendered = match self.gpu.render(world, screen, &fx_uniforms, &verts, &draws, CLEAR) {
            Ok(r) => r,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.gpu.resize(w, h);
                false
            }
            Err(e) => {
                log::warn!("surface error: {e:?}");
                false
            }
        };
        if rendered {
            let presented = Instant::now();
            // pacing only matters between back-to-back frames, not across intentional sleeps
            self.stats.on_present(presented, cpu_ms, self.prev_moving);
            self.prev_moving = camera_moved || continuous;
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
            if settled && !layer_draw.is_empty() && self.stats.first_settled_ms.is_none() {
                self.stats.first_settled_ms = Some((presented - self.start).as_secs_f32() * 1000.0);
            }
            self.gpu.evict_tiles_over_budget();
            self.camera_dirty = false;
            self.first = false;
        }
        self.last = FrameOutcome { rendered, continuous, wake_at };
        self.last
    }

    fn hud_image(&self, tile_level: i32) -> Image {
        let s = &self.stats;
        let g = self.page.graph();
        let gs = &g.stats;
        let (tiles_n, tiles_b) = self.gpu.tile_count_and_bytes();
        let q = self.tiles.queue.snapshot();
        let lines = [
            format!("{:?} · {} · {:?}", self.gpu.adapter_info.backend, self.gpu.adapter_info.name, self.gpu.present_mode()),
            format!(
                "{:.0} fps · 幀 CPU p50 {:.2} ms · p95 {:.2} ms · 已繪 {} 幀 · 沒有可見變化而跳過 {} 次",
                s.fps,
                s.frame_cpu.recent_pct(240, 50.0),
                s.frame_cpu.recent_pct(240, 95.0),
                s.frames,
                s.skipped
            ),
            format!("下次喚醒：{}", self.next_desc),
            format!(
                "計算圖 {} 格 · 求值 {} · 驗證後免算 {} · 值相同截斷 {} · 非同步 {} / {} · 串流 {} 筆 丟 {}",
                g.len(),
                gs.inline_evals,
                gs.verified + gs.clock_skips,
                gs.cutoffs,
                gs.async_completed,
                gs.async_started,
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
                "tile L{} 縮放 {:.2} · 快取 {} 張 {} MB · 佇列 {} 進行中 {} · 已光柵 {} 張 · {} 條工作執行緒",
                tile_level,
                self.camera.scale,
                tiles_n,
                tiles_b / (1024 * 1024),
                q.wanted,
                q.inflight,
                s.tiles_rastered,
                self.tiles.workers
            ),
            format!("最長閒置 {:.1} 秒，期間 0 幀 · 記憶體 {:.0} MB", s.longest_idle_s, rss_mb()),
        ];
        let (lw, lh) = (920.0, 18.0 + lines.len() as f32 * 21.0);
        // shrink to fit narrow screens (phones in portrait)
        let (sw, _) = self.gpu.size();
        let fit = ((sw as f32 - 24.0 * self.scale_factor) / (lw * self.scale_factor)).min(1.0).max(0.3);
        let mut c = Canvas::new(lw, lh, self.scale_factor * fit);
        c.clear([0, 0, 0, 0]);
        c.round_rect(0.0, 0.0, lw, lh, 10.0, [8, 10, 14, 215]);
        for (k, line) in lines.iter().enumerate() {
            let col = if k == 0 { [150, 200, 255, 255] } else if k == 2 { [196, 168, 255, 255] } else { [226, 230, 238, 255] };
            c.text(&self.text, 12.0, 8.0 + k as f32 * 21.0, 14.0, col, line);
        }
        c.into_image(None)
    }
}
