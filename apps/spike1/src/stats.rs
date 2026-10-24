//! What the spike exists to measure.

use std::time::Instant;

#[derive(Default)]
pub struct Samples {
    v: Vec<f32>,
}

impl Samples {
    pub fn push(&mut self, x: f32) {
        if self.v.len() >= 50_000 {
            self.v.drain(..10_000);
        }
        self.v.push(x);
    }
    pub fn len(&self) -> usize {
        self.v.len()
    }
    pub fn pct(&self, p: f32) -> f32 {
        pct(&self.v, p)
    }
    pub fn recent_pct(&self, n: usize, p: f32) -> f32 {
        pct(&self.v[self.v.len().saturating_sub(n)..], p)
    }
    /// Median of the last `n` samples as "12.3 ms", or "—" when there are none.
    pub fn recent_ms(&self, n: usize) -> String {
        if self.v.is_empty() {
            "—".into()
        } else {
            format!("{:.1} ms", self.recent_pct(n, 50.0))
        }
    }
    pub fn mean(&self) -> f32 {
        if self.v.is_empty() {
            0.0
        } else {
            self.v.iter().sum::<f32>() / self.v.len() as f32
        }
    }
    pub fn max(&self) -> f32 {
        self.v.iter().copied().fold(0.0, f32::max)
    }
    pub fn line(&self, unit: &str) -> String {
        if self.v.is_empty() {
            return "—".into();
        }
        format!(
            "p50 {:.2}{u} · p95 {:.2}{u} · p99 {:.2}{u} · max {:.2}{u} · n={}",
            self.pct(50.0),
            self.pct(95.0),
            self.pct(99.0),
            self.max(),
            self.len(),
            u = unit
        )
    }
}

pub fn pct(v: &[f32], p: f32) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    s[(((p / 100.0) * (s.len() - 1) as f32).round() as usize).min(s.len() - 1)]
}

pub fn rss_mb() -> f64 {
    memory_stats::memory_stats().map(|m| m.physical_mem as f64 / (1024.0 * 1024.0)).unwrap_or(0.0)
}

pub struct Stats {
    pub start: Instant,
    /// Frames rendered and presented.
    pub frames: u64,
    /// Loop iterations that committed the graph.
    pub commits: u64,
    /// Commits where nothing visible changed, so nothing was rendered.
    pub skipped: u64,
    pub frame_cpu: Samples,
    pub interval: Samples,
    pub lat_ops: Samples,
    pub lat_stream: Samples,
    pub lat_async: Samples,
    pub lat_tile: Samples,
    pub tile_ms: Samples,
    pub tiles_rastered: u64,
    pub panel_rasters: u64,
    pub first_frame_ms: Option<f32>,
    pub first_settled_ms: Option<f32>,
    pub last_present: Option<Instant>,
    pub idle_start: Option<Instant>,
    pub longest_idle_s: f32,
    pub peak_rss: f64,
    fps_window: (Instant, u64),
    pub fps: f32,
}

impl Stats {
    pub fn new(start: Instant) -> Self {
        Self {
            start,
            frames: 0,
            commits: 0,
            skipped: 0,
            frame_cpu: Samples::default(),
            interval: Samples::default(),
            lat_ops: Samples::default(),
            lat_stream: Samples::default(),
            lat_async: Samples::default(),
            lat_tile: Samples::default(),
            tile_ms: Samples::default(),
            tiles_rastered: 0,
            panel_rasters: 0,
            first_frame_ms: None,
            first_settled_ms: None,
            last_present: None,
            idle_start: None,
            longest_idle_s: 0.0,
            peak_rss: 0.0,
            fps_window: (start, 0),
            fps: 0.0,
        }
    }

    pub fn on_present(&mut self, now: Instant, cpu_ms: f32) {
        self.frames += 1;
        self.frame_cpu.push(cpu_ms);
        if let Some(last) = self.last_present {
            self.interval.push((now - last).as_secs_f32() * 1000.0);
        }
        self.last_present = Some(now);
        if self.first_frame_ms.is_none() {
            self.first_frame_ms = Some((now - self.start).as_secs_f32() * 1000.0);
        }
        let (t, f) = self.fps_window;
        let dt = (now - t).as_secs_f32();
        if dt >= 0.5 {
            self.fps = (self.frames - f) as f32 / dt;
            self.fps_window = (now, self.frames);
            self.peak_rss = self.peak_rss.max(rss_mb());
        }
    }

    /// Called when the loop is about to sleep with nothing scheduled.
    pub fn enter_idle(&mut self, now: Instant) {
        if self.idle_start.is_none() {
            self.idle_start = Some(now);
        }
    }

    /// Called when work resumes; returns how long the engine was idle.
    pub fn leave_idle(&mut self, now: Instant) -> Option<f32> {
        let s = self.idle_start.take()?;
        let d = (now - s).as_secs_f32();
        self.longest_idle_s = self.longest_idle_s.max(d);
        // the fps window should not average across a sleep
        self.fps_window = (now, self.frames);
        Some(d)
    }
}
