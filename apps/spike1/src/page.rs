//! The demo page: one graph holding all five kinds of timing, laid out in one
//! world. Every panel is a derived cell whose operator rasterises it; the
//! renderer only demands the panels the camera can see.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anypage_core::{
    level_scale, op_async, op_inline, val, CellId, Graph, Input, OpClass, Rect, Status, StreamPolicy,
    StreamSnapshot, ThreadPool, Wake,
};

use crate::feeds::{self, Rng, Tick, Token};
use crate::mandel::{hsv, MandelParams, MandelSource};
use crate::raster::{Canvas, Rgba, TextRaster};

// ---------------------------------------------------------------- layout

pub const BOUNDS: Rect = Rect::new(0.0, 0.0, 2400.0, 1556.0);
const TITLE: Rect = Rect::new(0.0, 0.0, 2400.0, 84.0);
const MANDEL_LABEL: Rect = Rect::new(0.0, 100.0, 1400.0, 46.0);
pub const MANDEL: Rect = Rect::new(0.0, 156.0, 1400.0, 1400.0);
pub const CONTROLS: Rect = Rect::new(1440.0, 100.0, 960.0, 270.0);
const CHART: Rect = Rect::new(1440.0, 390.0, 960.0, 300.0);
const TEXT: Rect = Rect::new(1440.0, 710.0, 960.0, 300.0);
pub const EFFECT: Rect = Rect::new(1440.0, 1030.0, 470.0, 300.0);
const EFFECT_LABEL: Rect = Rect::new(1440.0, 1030.0, 470.0, 50.0);
pub const ORBIT: Rect = Rect::new(1930.0, 1030.0, 470.0, 300.0);
const JOBS: Rect = Rect::new(1440.0, 1350.0, 960.0, 206.0);
pub const RIGHT_COLUMN: Rect = Rect::new(1440.0, 100.0, 960.0, 1456.0);

// controls, in panel-local units
const ITER_TRACK: Rect = Rect::new(24.0, 96.0, 560.0, 20.0);
const HUE_TRACK: Rect = Rect::new(24.0, 170.0, 560.0, 20.0);
const BTN_STREAMS: Rect = Rect::new(620.0, 64.0, 316.0, 52.0);
const BTN_ANIM: Rect = Rect::new(620.0, 126.0, 316.0, 52.0);
const BTN_RUN: Rect = Rect::new(24.0, 208.0, 300.0, 48.0);
const BTN_CANCEL: Rect = Rect::new(340.0, 208.0, 244.0, 48.0);
const BTN_FIT: Rect = Rect::new(620.0, 208.0, 316.0, 48.0);

const ITER_MIN: f32 = 32.0;
const ITER_MAX: f32 = 4096.0;
pub const JOB_COUNT: usize = 6;

// ---------------------------------------------------------------- types

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Static,
    PullStream,
    PushStream,
    Async,
    Operations,
}

impl Class {
    pub fn color(self) -> Rgba {
        match self {
            Class::Static => [96, 156, 255, 255],
            Class::PullStream => [196, 128, 255, 255],
            Class::PushStream => [72, 204, 140, 255],
            Class::Async => [255, 172, 64, 255],
            Class::Operations => [255, 104, 128, 255],
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Class::Static => "靜態",
            Class::PullStream => "拉式串流",
            Class::PushStream => "推式串流",
            Class::Async => "非同步",
            Class::Operations => "操作",
        }
    }
}

pub struct Panel {
    pub id: u32,
    pub rect: Rect,
    pub cell: CellId,
    pub class: Class,
    /// Lower runs first when the frame budget is tight (section 3.2).
    pub priority: u8,
    pub after_effect: bool,
}

#[derive(Clone, Copy, Debug)]
pub enum JobResult {
    Idle,
    Done { value: f64, ms: f32 },
}

#[derive(Clone, Copy, Debug)]
pub struct EffectParams {
    pub t: f32,
    pub hue: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slider {
    Iterations,
    Hue,
}

#[derive(Clone, Copy, Debug)]
pub enum Hit {
    Slider(Slider, f32),
    Streams,
    Anim,
    RunJobs,
    CancelJobs,
    Fit,
}

pub struct Page {
    pub graph: Graph,
    pub mandel: Arc<MandelSource>,
    pub mandel_params: CellId,
    pub effect_params: CellId,
    pub cam_level: CellId,
    pub t_anim: CellId,
    iterations: CellId,
    hue: CellId,
    streams_flag: CellId,
    anim_flag: CellId,
    job_seeds: Vec<CellId>,
    pub jobs: Vec<CellId>,
    pub panels: Vec<Panel>,
    pub iterations_v: u32,
    pub hue_v: f32,
    pub anim_on: bool,
    streams_on: Arc<AtomicBool>,
    quit: Arc<AtomicBool>,
    rng: Rng,
    _feeds: Vec<JoinHandle<()>>,
}

// ---------------------------------------------------------------- drawing helpers

const PANEL_BG: Rgba = [22, 25, 32, 255];
const TEXT_HI: Rgba = [232, 236, 244, 255];
const TEXT_LO: Rgba = [140, 148, 164, 255];

fn scale_of(i: &Input) -> f32 {
    level_scale(*i.get::<i32>().unwrap_or(&0)).min(8.0)
}

fn hue_rgba(h: f32, s: f64, v: f64, a: u8) -> Rgba {
    let c = hsv((h as f64).rem_euclid(360.0), s, v);
    [c[0], c[1], c[2], a]
}

/// Background, class badge, title and one-line subtitle. Returns the canvas.
fn panel(tr: &TextRaster, r: Rect, scale: f32, class: Class, title: &str, sub: &str) -> Canvas {
    let mut c = Canvas::new(r.w, r.h, scale);
    c.clear([0, 0, 0, 0]);
    c.round_rect(0.0, 0.0, r.w, r.h, 14.0, PANEL_BG);
    c.stroke_round_rect(0.5, 0.5, r.w - 1.0, r.h - 1.0, 14.0, 1.0, [255, 255, 255, 18]);
    badge(&mut c, tr, 18.0, 14.0, class);
    let bw = 18.0 + badge_width(tr, class) + 12.0;
    c.text(tr, bw, 13.0, 20.0, TEXT_HI, title);
    if !sub.is_empty() {
        c.text(tr, 18.0, 44.0, 14.0, TEXT_LO, sub);
    }
    c
}

fn badge_width(tr: &TextRaster, class: Class) -> f32 {
    let mut probe = Canvas::new(1.0, 1.0, 1.0);
    probe.text(tr, 0.0, 0.0, 14.0, [0, 0, 0, 0], class.label()) + 18.0
}

fn badge(c: &mut Canvas, tr: &TextRaster, x: f32, y: f32, class: Class) {
    let w = badge_width(tr, class);
    let col = class.color();
    c.round_rect(x, y, w, 24.0, 12.0, [col[0], col[1], col[2], 46]);
    c.text(tr, x + 9.0, y + 3.0, 14.0, col, class.label());
}

fn button(c: &mut Canvas, tr: &TextRaster, r: Rect, label: &str, on: Option<bool>, accent: Rgba) {
    let fill = match on {
        Some(true) => [accent[0], accent[1], accent[2], 70],
        _ => [255, 255, 255, 14],
    };
    c.round_rect(r.x, r.y, r.w, r.h, 10.0, fill);
    c.stroke_round_rect(r.x, r.y, r.w, r.h, 10.0, 1.0, [accent[0], accent[1], accent[2], 120]);
    c.text(tr, r.x + 14.0, r.y + r.h * 0.5 - 11.0, 17.0, TEXT_HI, label);
}

fn slider(c: &mut Canvas, tr: &TextRaster, r: Rect, frac: f32, label: &str, accent: Rgba) {
    c.text(tr, r.x, r.y - 30.0, 16.0, TEXT_HI, label);
    c.round_rect(r.x, r.y, r.w, r.h, r.h * 0.5, [255, 255, 255, 22]);
    c.round_rect(r.x, r.y, r.w * frac.clamp(0.0, 1.0), r.h, r.h * 0.5, [accent[0], accent[1], accent[2], 150]);
    c.circle(r.x + r.w * frac.clamp(0.0, 1.0), r.y + r.h * 0.5, r.h * 0.75, TEXT_HI);
}

fn iter_frac(n: u32) -> f32 {
    ((n as f32 / ITER_MIN).ln() / (ITER_MAX / ITER_MIN).ln()).clamp(0.0, 1.0)
}

fn iter_from_frac(f: f32) -> u32 {
    (ITER_MIN * (ITER_MAX / ITER_MIN).powf(f.clamp(0.0, 1.0))).round() as u32
}

fn contains(r: &Rect, x: f32, y: f32) -> bool {
    x >= r.x && x <= r.right() && y >= r.y && y <= r.bottom()
}

// ---------------------------------------------------------------- page

impl Page {
    pub fn new(tr: Arc<TextRaster>, wake: Wake, start: Instant) -> Self {
        let pool = Arc::new(ThreadPool::new(JOB_COUNT, "job"));
        let mut g = Graph::new(pool);
        g.set_wake(wake);

        // input cells: commands, camera, clock
        let cam_level = g.input("camera.level", val(0i32));
        let iterations = g.input("iterations", val(256u32));
        let hue = g.input("hue", val(205.0f32));
        let streams_flag = g.input("streams.on", val(true));
        let anim_flag = g.input("animation.on", val(true));
        let t_anim = g.input("t", val(0.0f32));

        // push streams
        let (ticks, tick_p) = g.stream::<Tick>("ticks", 600, 8192, StreamPolicy::Append);
        let (tokens, token_p) = g.stream::<Token>("tokens", 4000, 8192, StreamPolicy::Append);

        // static: parameters of the engine-computed tile source
        let mandel_params = g.derive(
            "mandel.params",
            op_inline("mandel.params", OpClass::Native, |i, _| {
                Ok(val(MandelParams {
                    iterations: *i[0].get::<u32>().unwrap_or(&256),
                    hue: *i[1].get::<f32>().unwrap_or(&205.0),
                }))
            }),
            &[iterations, hue],
        );

        // pull stream on the GPU: the cell holds the uniforms, the renderer runs the shader
        let effect_params = g.derive(
            "effect.params",
            op_inline("effect.params", OpClass::Gpu, |i, _| {
                Ok(val(EffectParams { t: *i[0].get::<f32>().unwrap_or(&0.0), hue: *i[1].get::<f32>().unwrap_or(&205.0) }))
            }),
            &[t_anim, hue],
        );

        // async jobs: seed input -> job on the worker pool
        let mut job_seeds = Vec::new();
        let mut jobs = Vec::new();
        for k in 0..JOB_COUNT {
            let seed = g.input("job.seed", val(0u64));
            let job = g.derive(&format!("job.{k}"), job_op(), &[seed]);
            job_seeds.push(seed);
            jobs.push(job);
        }

        let mut panels = Vec::new();
        let mut add = |g: &mut Graph,
                       id: u32,
                       rect: Rect,
                       class: Class,
                       priority: u8,
                       after: bool,
                       name: &str,
                       op: Arc<dyn anypage_core::Op>,
                       inputs: &[CellId]| {
            let cell = g.derive(name, op, inputs);
            panels.push(Panel { id, rect, cell, class, priority, after_effect: after });
        };

        let t = tr.clone();
        add(&mut g, 0, TITLE, Class::Static, 4, false, "title.raster", op_inline("title.raster", OpClass::Native, move |i, _| {
            let mut c = Canvas::new(TITLE.w, TITLE.h, scale_of(&i[0]));
            c.clear([0, 0, 0, 0]);
            c.text(&t, 4.0, 4.0, 36.0, TEXT_HI, "AnyPage spike 1 — 一個頁面，所有時間性");
            c.text(&t, 6.0, 54.0, 17.0, TEXT_LO, "靜態、拉式串流、推式串流、非同步、操作：同一張計算圖、同一個排程器、同一個合成器。拖曳平移，滾輪縮放。");
            Ok(val(c.into_image(None)))
        }), &[cam_level]);

        let t = tr.clone();
        add(&mut g, 1, MANDEL_LABEL, Class::Static, 4, false, "mandel.label", op_inline("mandel.label", OpClass::Native, move |i, _| {
            let mut c = Canvas::new(MANDEL_LABEL.w, MANDEL_LABEL.h, scale_of(&i[0]));
            c.clear([0, 0, 0, 0]);
            badge(&mut c, &t, 0.0, 10.0, Class::Static);
            let x = badge_width(&t, Class::Static) + 12.0;
            let p = i[1].get::<MandelParams>().copied().unwrap_or(MandelParams { iterations: 0, hue: 0.0 });
            c.text(&t, x, 9.0, 20.0, TEXT_HI, &format!(
                "引擎自己計算的 tile：需求驅動、離開視口即取消、參數變了舊 tile 先頂著（迭代 {}）",
                p.iterations
            ));
            Ok(val(c.into_image(None)))
        }), &[cam_level, mandel_params]);

        let t = tr.clone();
        add(&mut g, 2, CONTROLS, Class::Operations, 0, false, "controls.raster", op_inline("controls.raster", OpClass::Native, move |i, _| {
            let iters = *i[1].get::<u32>().unwrap_or(&256);
            let h = *i[2].get::<f32>().unwrap_or(&0.0);
            let streams = *i[3].get::<bool>().unwrap_or(&true);
            let anim = *i[4].get::<bool>().unwrap_or(&true);
            let mut c = panel(&t, CONTROLS, scale_of(&i[0]), Class::Operations, "指令改輸入格，結果沿圖傳播",
                "滑桿與按鈕都是對輸入格下指令；色相同時影響 tile、圖表、GPU 動畫與 CPU 動畫。");
            let acc = Class::Operations.color();
            slider(&mut c, &t, ITER_TRACK, iter_frac(iters), &format!("迭代次數 iterations：{iters}"), acc);
            slider(&mut c, &t, HUE_TRACK, h / 360.0, &format!("色相 hue：{h:.0}°"), hue_rgba(h, 0.75, 0.95, 255));
            button(&mut c, &t, BTN_STREAMS, &format!("串流 Streams：{}（空白鍵）", if streams { "開" } else { "關" }), Some(streams), Class::PushStream.color());
            button(&mut c, &t, BTN_ANIM, &format!("動畫 Animation：{}（A）", if anim { "開" } else { "關" }), Some(anim), Class::PullStream.color());
            button(&mut c, &t, BTN_RUN, "執行非同步任務（J）", None, Class::Async.color());
            button(&mut c, &t, BTN_CANCEL, "取消任務（C）", None, Class::Async.color());
            button(&mut c, &t, BTN_FIT, "貼合整頁（R）", None, acc);
            Ok(val(c.into_image(None)))
        }), &[cam_level, iterations, hue, streams_flag, anim_flag]);

        let t = tr.clone();
        add(&mut g, 3, CHART, Class::PushStream, 2, false, "chart.raster", op_inline("chart.raster", OpClass::Native, move |i, _| {
            let h = *i[2].get::<f32>().unwrap_or(&0.0);
            let mut c = panel(&t, CHART, scale_of(&i[0]), Class::PushStream, "即時資料 Live ticks",
                "生產者在另一條執行緒以不固定速率推送，偶爾連發；引擎每幀合批成一次 commit。");
            let (x0, y0, w, hh) = (20.0, 76.0, CHART.w - 40.0, CHART.h - 96.0);
            c.round_rect(x0, y0, w, hh, 8.0, [255, 255, 255, 8]);
            let mut newest = None;
            if let Some(snap) = i[1].get::<StreamSnapshot<Tick>>() {
                let items = &snap.items;
                if items.len() >= 2 {
                    let (mut lo, mut hi) = (f32::MAX, f32::MIN);
                    for it in items.iter() {
                        lo = lo.min(it.v);
                        hi = hi.max(it.v);
                    }
                    let span = (hi - lo).max(0.5);
                    let n = items.len() as f32;
                    let pts: Vec<(f32, f32)> = items
                        .iter()
                        .enumerate()
                        .map(|(k, it)| (x0 + 8.0 + (w - 16.0) * k as f32 / (n - 1.0), y0 + hh - 10.0 - (hh - 20.0) * (it.v - lo) / span))
                        .collect();
                    c.fill_under(&pts, y0 + hh - 2.0, hue_rgba(h, 0.7, 0.9, 40));
                    c.polyline(&pts, 2.0, hue_rgba(h, 0.7, 1.0, 255));
                    let last = items[items.len() - 1].clone();
                    newest = Some(last.produced);
                    let window_s = last.t - items[0].t;
                    c.text(&t, CHART.w - 430.0, 12.0, 17.0, TEXT_HI, &format!(
                        "最新 {:.2}　近 {:.1} 秒 {} 筆　累計 {}",
                        last.v,
                        window_s,
                        items.len(),
                        snap.total
                    ));
                }
            }
            Ok(val(c.into_image(newest)))
        }), &[cam_level, ticks, hue]);

        let t = tr.clone();
        add(&mut g, 4, TEXT, Class::PushStream, 2, false, "tokens.raster", op_inline("tokens.raster", OpClass::Native, move |i, _| {
            let mut c = panel(&t, TEXT, scale_of(&i[0]), Class::PushStream, "逐字輸出 Token stream",
                "模擬模型逐字輸出：小片段、速率不固定、句間停頓；只畫最後幾行。");
            let mut newest = None;
            if let Some(snap) = i[1].get::<StreamSnapshot<Token>>() {
                let all: String = snap.items.iter().map(|tk| tk.text.as_str()).collect();
                let chars: Vec<char> = all.chars().collect();
                let tail: String = chars[chars.len().saturating_sub(1600)..].iter().collect();
                c.text_box(&t, 20.0, 76.0, TEXT.w - 40.0, TEXT.h - 92.0, 21.0, TEXT_HI, &format!("{tail}▍"), true);
                newest = snap.items.last().map(|tk| tk.produced);
            }
            Ok(val(c.into_image(newest)))
        }), &[cam_level, tokens]);

        let t = tr.clone();
        add(&mut g, 5, ORBIT, Class::PullStream, 1, false, "orbit.raster", op_inline("orbit.raster", OpClass::Native, move |i, _| {
            let time = *i[1].get::<f32>().unwrap_or(&0.0);
            let h = *i[2].get::<f32>().unwrap_or(&0.0);
            let mut c = panel(&t, ORBIT, scale_of(&i[0]), Class::PullStream, "原生算子，依賴 t", "只有可見時才每幀求值。");
            let (cx, cy) = (ORBIT.w * 0.5, 72.0 + (ORBIT.h - 72.0) * 0.5);
            for k in 0..16 {
                let kf = k as f32;
                let a = time * (0.5 + kf * 0.11) + kf * 0.7;
                let rad = 18.0 + kf * 6.2;
                let col = hue_rgba(h + kf * 18.0, 0.65, 1.0, 230);
                c.circle(cx + a.cos() * rad * 1.6, cy + a.sin() * rad * 0.9, 4.0 + (k % 4) as f32 * 1.6, col);
            }
            Ok(val(c.into_image(None)))
        }), &[cam_level, t_anim, hue]);

        let t = tr.clone();
        add(&mut g, 6, EFFECT_LABEL, Class::PullStream, 1, true, "effect.label", op_inline("effect.label", OpClass::Native, move |i, _| {
            let mut c = Canvas::new(EFFECT_LABEL.w, EFFECT_LABEL.h, scale_of(&i[0]));
            c.clear([0, 0, 0, 0]);
            c.round_rect(0.0, 0.0, EFFECT_LABEL.w, EFFECT_LABEL.h, 14.0, [10, 12, 16, 170]);
            badge(&mut c, &t, 18.0, 13.0, Class::PullStream);
            let x = 18.0 + badge_width(&t, Class::PullStream) + 12.0;
            c.text(&t, x, 12.0, 20.0, TEXT_HI, "GPU 算子（WGSL），依賴 t");
            Ok(val(c.into_image(None)))
        }), &[cam_level]);

        let t = tr.clone();
        let mut job_inputs = vec![cam_level];
        job_inputs.extend(jobs.iter().copied());
        add(&mut g, 7, JOBS, Class::Async, 3, false, "jobs.raster", op_inline("jobs.raster", OpClass::Native, move |i, _| {
            let mut c = panel(&t, JOBS, scale_of(&i[0]), Class::Async, "工作池上的 Job（J 執行，C 取消）",
                "完成前畫佔位與進度；完成時合批成一條指令；改了輸入，舊任務自動取消。");
            let n = JOB_COUNT as f32;
            let gap = 12.0;
            let cw = (JOBS.w - 40.0 - gap * (n - 1.0)) / n;
            let acc = Class::Async.color();
            for (k, job) in i[1..].iter().enumerate() {
                let x = 20.0 + k as f32 * (cw + gap);
                let y = 78.0;
                c.round_rect(x, y, cw, 108.0, 10.0, [255, 255, 255, 10]);
                c.text(&t, x + 12.0, y + 10.0, 16.0, TEXT_HI, &format!("任務 {}", k + 1));
                let (line1, line2, prog): (String, String, Option<f32>) = match &job.status {
                    Status::Pending { progress } => (format!("進行中 {:.0}%", progress * 100.0), String::new(), Some(*progress)),
                    Status::Error(e) => (e.clone(), String::new(), None),
                    Status::Empty => ("—".into(), String::new(), None),
                    Status::Ready => match job.get::<JobResult>() {
                        Some(JobResult::Done { value, ms }) => ("完成".into(), format!("{ms:.0} ms · {:.3}", value.abs().fract()), None),
                        _ => ("閒置".into(), String::new(), None),
                    },
                };
                let col = match &job.status {
                    Status::Pending { .. } => acc,
                    Status::Error(_) => [255, 110, 110, 255],
                    Status::Ready if matches!(job.get::<JobResult>(), Some(JobResult::Done { .. })) => Class::PushStream.color(),
                    _ => TEXT_LO,
                };
                c.text(&t, x + 12.0, y + 40.0, 15.0, col, &line1);
                if !line2.is_empty() {
                    c.text(&t, x + 12.0, y + 64.0, 13.0, TEXT_LO, &line2);
                }
                if let Some(p) = prog {
                    c.round_rect(x + 12.0, y + 84.0, cw - 24.0, 8.0, 4.0, [255, 255, 255, 24]);
                    c.round_rect(x + 12.0, y + 84.0, (cw - 24.0) * p, 8.0, 4.0, acc);
                }
            }
            Ok(val(c.into_image(None)))
        }), &job_inputs);

        let streams_on = Arc::new(AtomicBool::new(true));
        let quit = Arc::new(AtomicBool::new(false));
        let feeds = vec![
            feeds::spawn_ticks(tick_p, streams_on.clone(), quit.clone(), start),
            feeds::spawn_tokens(token_p, streams_on.clone(), quit.clone()),
        ];

        Self {
            graph: g,
            mandel: Arc::new(MandelSource::new(MANDEL)),
            mandel_params,
            effect_params,
            cam_level,
            t_anim,
            iterations,
            hue,
            streams_flag,
            anim_flag,
            job_seeds,
            jobs,
            panels,
            iterations_v: 256,
            hue_v: 205.0,
            anim_on: true,
            streams_on,
            quit,
            rng: Rng::new(0x5EED_1234_ABCD_0001),
            _feeds: feeds,
        }
    }

    pub fn streams_on(&self) -> bool {
        self.streams_on.load(Ordering::Relaxed)
    }

    // --- commands: every UI action is a command on an input cell ---

    pub fn set_iterations(&mut self, n: u32) {
        let n = n.clamp(ITER_MIN as u32, ITER_MAX as u32);
        if n != self.iterations_v {
            self.iterations_v = n;
            self.graph.set(self.iterations, val(n));
        }
    }

    pub fn set_hue(&mut self, h: f32) {
        let h = h.clamp(0.0, 360.0).round();
        if (h - self.hue_v).abs() >= 1.0 {
            self.hue_v = h;
            self.graph.set(self.hue, val(h));
        }
    }

    pub fn set_streams(&mut self, on: bool) {
        self.streams_on.store(on, Ordering::Relaxed);
        self.graph.set(self.streams_flag, val(on));
    }

    pub fn set_anim(&mut self, on: bool) {
        self.anim_on = on;
        self.graph.set(self.anim_flag, val(on));
    }

    pub fn run_jobs(&mut self) {
        for k in 0..JOB_COUNT {
            let seed = self.rng.next() | 1;
            self.graph.set(self.job_seeds[k], val(seed));
        }
    }

    pub fn cancel_jobs(&mut self) {
        for k in 0..JOB_COUNT {
            self.graph.cancel(self.jobs[k]);
        }
    }

    pub fn slider_value(&mut self, s: Slider, frac: f32) {
        match s {
            Slider::Iterations => self.set_iterations(iter_from_frac(frac)),
            Slider::Hue => self.set_hue(frac * 360.0),
        }
    }

    /// Slider fraction for a world x while dragging (may leave the track).
    pub fn slider_frac(&self, s: Slider, wx: f32) -> f32 {
        let track = match s {
            Slider::Iterations => ITER_TRACK,
            Slider::Hue => HUE_TRACK,
        };
        ((wx - CONTROLS.x - track.x) / track.w).clamp(0.0, 1.0)
    }

    pub fn hit(&self, wx: f32, wy: f32) -> Option<Hit> {
        if !contains(&CONTROLS, wx, wy) {
            return None;
        }
        let (x, y) = (wx - CONTROLS.x, wy - CONTROLS.y);
        let grab = |r: Rect| Rect::new(r.x - 12.0, r.y - 14.0, r.w + 24.0, r.h + 28.0);
        if contains(&grab(ITER_TRACK), x, y) {
            return Some(Hit::Slider(Slider::Iterations, self.slider_frac(Slider::Iterations, wx)));
        }
        if contains(&grab(HUE_TRACK), x, y) {
            return Some(Hit::Slider(Slider::Hue, self.slider_frac(Slider::Hue, wx)));
        }
        [(BTN_STREAMS, Hit::Streams), (BTN_ANIM, Hit::Anim), (BTN_RUN, Hit::RunJobs), (BTN_CANCEL, Hit::CancelJobs), (BTN_FIT, Hit::Fit)]
            .into_iter()
            .find(|(r, _)| contains(r, x, y))
            .map(|(_, h)| h)
    }

    pub fn shutdown(&mut self) {
        self.quit.store(true, Ordering::Relaxed);
        self.graph.cancel_all();
    }
}

/// Synthetic CPU work that reports progress and honours cancellation.
fn job_op() -> Arc<dyn anypage_core::Op> {
    op_async("job.compute", |i, ctx| {
        let seed = *i[0].get::<u64>().unwrap_or(&0);
        if seed == 0 {
            return Ok(val(JobResult::Idle));
        }
        let t0 = Instant::now();
        let dur_ms = 900 + seed % 2600;
        let mut acc = 0.0f64;
        for step in 0..100u64 {
            if ctx.cancelled() {
                return Err("已取消".into());
            }
            let deadline = t0 + Duration::from_millis(dur_ms * (step + 1) / 100);
            while Instant::now() < deadline {
                for k in 0..2000u64 {
                    acc += ((k ^ seed) as f64 * 1e-3).sin();
                }
            }
            ctx.set_progress((step + 1) as f32 / 100.0);
        }
        Ok(val(JobResult::Done { value: std::hint::black_box(acc), ms: t0.elapsed().as_secs_f32() * 1000.0 }))
    })
}
