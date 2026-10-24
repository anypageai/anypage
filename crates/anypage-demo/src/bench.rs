//! Scripted run of the demo page, host-independent: the host calls `step`
//! before each frame and asks `continuous` / `next_time` how to schedule.
//!
//! Timeline (seconds): zoom into the fractal, edit parameters and start jobs,
//! pan the right column, go quiet, an idle window (nothing should render),
//! a predictable-time window (only the 1 Hz clock and the cursor blink run, so
//! frames should match the number of predicted changes), then resume.

use std::collections::HashMap;

use anypage_engine::{Engine, Page};

use crate::mandel::world_of;
use crate::page::{DemoPage, MANDEL, RIGHT_COLUMN, JOB_COUNT};

pub struct Bench {
    actions: Vec<(f64, &'static str)>,
    next: usize,
    marks: HashMap<&'static str, (u64, u64)>,
    pan_base: Option<(f32, f32)>,
    pub done: bool,
}

const IDLE: (f64, f64) = (12.0, 15.0);
const PREDICT: (f64, f64) = (15.5, 18.5);
const END: f64 = 22.0;

impl Default for Bench {
    fn default() -> Self {
        Self::new()
    }
}

impl Bench {
    pub fn new() -> Self {
        Self {
            actions: vec![
                (0.3, "focus"),
                (6.0, "edit"),
                (6.0, "jobs"),
                (6.2, "right"),
                (11.0, "quiet"),
                (IDLE.0, "idle-begin"),
                (IDLE.1, "idle-end"),
                (IDLE.1, "timers"),
                (PREDICT.0, "predict-begin"),
                (PREDICT.1, "predict-end"),
                (PREDICT.1 + 0.1, "resume"),
                (END, "finish"),
            ],
            next: 0,
            marks: HashMap::new(),
            pan_base: None,
            done: false,
        }
    }

    fn zooming(t: f64) -> bool {
        (0.4..6.0).contains(&t)
    }
    fn panning(t: f64) -> bool {
        (6.3..11.0).contains(&t)
    }

    /// The script itself needs a frame every vsync (camera moving).
    pub fn continuous(&self, t: f64) -> bool {
        Self::zooming(t) || Self::panning(t)
    }

    pub fn next_time(&self) -> Option<f64> {
        self.actions.get(self.next).map(|a| a.0)
    }

    pub fn step(&mut self, e: &mut Engine<DemoPage>, t: f64) {
        while let Some(&(at, name)) = self.actions.get(self.next) {
            if t < at {
                break;
            }
            self.next += 1;
            let s = e.stats();
            let counters = (s.frames, s.commits);
            match name {
                "focus" => {
                    let (wx, wy) = world_of(MANDEL, -0.745, 0.11); // seahorse valley
                    let cam = e.camera_mut();
                    let (sx, sy) = cam.world_to_screen(wx, wy);
                    let (vw, vh) = (cam.viewport_w, cam.viewport_h);
                    cam.pan(vw * 0.5 - sx, vh * 0.5 - sy);
                }
                "edit" => {
                    e.page_mut().set_iterations(768);
                    e.page_mut().set_hue(320.0);
                    e.mark_op();
                }
                "jobs" => {
                    e.page_mut().run_jobs();
                    e.mark_op();
                }
                "right" => {
                    e.fit_rect(RIGHT_COLUMN);
                    self.pan_base = Some((e.camera().tx, e.camera().ty));
                }
                "quiet" => {
                    let p = e.page_mut();
                    p.set_streams(false);
                    p.set_anim(false);
                    p.set_timers(false);
                    p.cancel_jobs();
                    e.mark_op();
                }
                "timers" => {
                    e.page_mut().set_timers(true);
                    e.fit();
                }
                "resume" => {
                    let p = e.page_mut();
                    p.set_streams(true);
                    p.set_anim(true);
                    e.fit();
                }
                "finish" => self.done = true,
                _ => {
                    self.marks.insert(name, counters);
                }
            }
        }
        if Self::zooming(t) {
            let cam = e.camera_mut();
            let (cx, cy) = (cam.viewport_w * 0.5, cam.viewport_h * 0.5);
            cam.zoom_at(cx, cy, 1.012);
        }
        if Self::panning(t) {
            if let Some((tx, ty)) = self.pan_base {
                let cam = e.camera_mut();
                cam.tx = tx;
                cam.ty = ty + (((t - 6.3) * 1.3).sin() * 160.0) as f32;
            }
        }
    }

    fn window(&self, a: &str, b: &str) -> (u64, u64) {
        match (self.marks.get(a), self.marks.get(b)) {
            (Some(x), Some(y)) => (y.0.saturating_sub(x.0), y.1.saturating_sub(x.1)),
            _ => (0, 0),
        }
    }

    pub fn report(&self, e: &Engine<DemoPage>) -> String {
        let s = e.stats();
        let g = e.page().graph();
        let gs = &g.stats;
        let (idle_frames, idle_wakes) = self.window("idle-begin", "idle-end");
        let (pred_frames, pred_wakes) = self.window("predict-begin", "predict-end");
        // changes the page predicted inside the window: whole seconds of the clock
        // and half-period toggles of the cursor
        let secs = (PREDICT.1.floor() - PREDICT.0.floor()) as u64;
        let blinks = ((PREDICT.1 / 0.53).floor() - (PREDICT.0 / 0.53).floor()) as u64;
        let (w, h) = e.surface_size();
        let mut r = String::new();
        r += &format!("# AnyPage engine bench · {}\n\n", e.adapter_name());
        r += &format!(
            "- backend {}, present mode {}, {} tile workers, {} job workers, surface {}x{}\n",
            e.backend(),
            e.present_mode(),
            e.tile_workers(),
            JOB_COUNT,
            w,
            h
        );
        r += &format!(
            "- duration {:.1} s, frames rendered {}, loop wake-ups {}, wake-ups with nothing visible to redraw {}\n",
            END, s.frames, s.commits, s.skipped
        );
        r += &format!("- first frame {:.0} ms after start\n\n", s.first_frame_ms.unwrap_or(0.0));
        r += "| metric | value |\n|---|---|\n";
        r += &format!("| frame CPU time (ms) | {} |\n", s.frame_cpu.line(""));
        r += &format!("| frame interval while continuous (ms) | {} |\n", s.interval.line(""));
        r += &format!("| operation → present (ms) | {} |\n", s.lat_ops.line(""));
        r += &format!("| push-stream item → present (ms) | {} |\n", s.lat_stream.line(""));
        r += &format!("| async result → present (ms) | {} |\n", s.lat_async.line(""));
        r += &format!("| tile request → present (ms) | {} |\n", s.lat_tile.line(""));
        r += &format!("| tile raster time (ms) | {} |\n", s.tile_ms.line(""));
        r += &format!(
            "| idle window {:.0}–{:.0} s, nothing changing | {} frames, {} wake-ups |\n",
            IDLE.0, IDLE.1, idle_frames, idle_wakes
        );
        r += &format!(
            "| predictable window {:.1}–{:.1} s, only 1 Hz clock + cursor blink | {} frames, {} wake-ups; predicted changes {} (clock {} + cursor {}); a 60 fps loop would draw {} |\n",
            PREDICT.0,
            PREDICT.1,
            pred_frames,
            pred_wakes,
            secs + blinks,
            secs,
            blinks,
            ((PREDICT.1 - PREDICT.0) * 60.0) as u64
        );
        r += &format!(
            "| graph | {} cells; {} operator runs; {} verified without running; {} cutoffs; async {} started / {} done / {} cancelled; {} stream items, {} dropped |\n",
            g.len(),
            gs.inline_evals,
            gs.verified + gs.clock_skips,
            gs.cutoffs,
            gs.async_started,
            gs.async_completed,
            gs.async_cancelled,
            gs.stream_items,
            g.stream_dropped()
        );
        r += &format!("| tiles rastered | {} |\n", s.tiles_rastered);
        r += &format!("| panel rasters uploaded | {} |\n", s.panel_rasters);
        r += &format!("| memory | {:.0} MB peak |\n", s.peak_rss.max(anypage_engine::stats::rss_mb()));
        r
    }
}
