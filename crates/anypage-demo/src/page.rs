//! The demo page: one graph holding every kind of timing, laid out in one world.
//!
//! There is no static/dynamic split. Every panel is a function of cells:
//! the title is a function of the camera level only, so it is computed once
//! per zoom level; the clock panel is a function of time that declares it holds
//! until the next whole second; the orbit is a function of time that changes
//! every frame while running and declares itself constant when paused.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anypage_core::{
    level_scale, op_async, op_inline, op_memo, val, CellId, EvalCtx, Graph, Input, Op, OpClass, Rect, Status,
    StreamPolicy, StreamSnapshot, ThreadPool, Wake,
};
use anypage_engine::{
    Canvas, Class, EffectLayer, EffectParams, Page, PageRequest, PageSpec, Panel, PointerResponse, Rgba, TextRaster,
    TiledLayer,
};

use crate::feeds::{self, Rng, Tick, Token};
use crate::mandel::{hsv, Mandel, MandelParams};

// ---------------------------------------------------------------- layout (world units)

pub const BOUNDS: Rect = Rect::new(0.0, 0.0, 2400.0, 1556.0);
const TITLE: Rect = Rect::new(0.0, 0.0, 1400.0, 84.0);
const CLOCK: Rect = Rect::new(1440.0, 4.0, 960.0, 82.0);
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

// controls, panel-local
const ITER_TRACK: Rect = Rect::new(24.0, 96.0, 560.0, 20.0);
const HUE_TRACK: Rect = Rect::new(24.0, 170.0, 560.0, 20.0);
const BTN_STREAMS: Rect = Rect::new(620.0, 58.0, 316.0, 42.0);
const BTN_ANIM: Rect = Rect::new(620.0, 106.0, 316.0, 42.0);
const BTN_TIMERS: Rect = Rect::new(620.0, 154.0, 316.0, 42.0);
const BTN_RUN: Rect = Rect::new(24.0, 208.0, 300.0, 48.0);
const BTN_CANCEL: Rect = Rect::new(340.0, 208.0, 244.0, 48.0);
const BTN_FIT: Rect = Rect::new(620.0, 208.0, 316.0, 48.0);

const ITER_MIN: f32 = 32.0;
const ITER_MAX: f32 = 4096.0;
pub const JOB_COUNT: usize = 6;
const BLINK: f64 = 0.53;

const PANEL_BG: Rgba = [22, 25, 32, 255];
const TEXT_HI: Rgba = [232, 236, 244, 255];
const TEXT_LO: Rgba = [140, 148, 164, 255];

/// An on/off input that remembers the clock time it last changed, so a paused
/// function of time can freeze exactly where it was.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Toggle {
    pub on: bool,
    pub since: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum JobResult {
    Idle,
    Done { value: f64, ms: f32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slider {
    Iterations,
    Hue,
}

pub struct DemoPage {
    graph: Graph,
    spec: PageSpec,
    iterations: CellId,
    hue: CellId,
    streams: CellId,
    anim: CellId,
    timers: CellId,
    job_seeds: Vec<CellId>,
    jobs: Vec<CellId>,
    pub iterations_v: u32,
    pub hue_v: f32,
    dragging: Option<Slider>,
    request: Option<PageRequest>,
    streams_on: Arc<AtomicBool>,
    quit: Arc<AtomicBool>,
    rng: Rng,
    _feeds: Vec<JoinHandle<()>>,
}

// ---------------------------------------------------------------- drawing helpers

fn scale_of(i: &Input) -> f32 {
    level_scale(*i.get::<i32>().unwrap_or(&0)).min(8.0)
}

fn toggle_of(i: &Input) -> Toggle {
    i.get::<Toggle>().copied().unwrap_or(Toggle { on: true, since: 0.0 })
}

fn hue_rgba(h: f32, s: f64, v: f64, a: u8) -> Rgba {
    let c = hsv((h as f64).rem_euclid(360.0), s, v);
    [c[0], c[1], c[2], a]
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

/// Background, class badge, title and subtitle.
fn panel(tr: &TextRaster, r: Rect, scale: f32, class: Class, title: &str, sub: &str) -> Canvas {
    let mut c = Canvas::new(r.w, r.h, scale);
    c.clear([0, 0, 0, 0]);
    c.round_rect(0.0, 0.0, r.w, r.h, 14.0, PANEL_BG);
    c.stroke_round_rect(0.5, 0.5, r.w - 1.0, r.h - 1.0, 14.0, 1.0, [255, 255, 255, 18]);
    badge(&mut c, tr, 18.0, 14.0, class);
    let x = 18.0 + badge_width(tr, class) + 12.0;
    c.text(tr, x, 13.0, 20.0, TEXT_HI, title);
    if !sub.is_empty() {
        c.text(tr, 18.0, 44.0, 14.0, TEXT_LO, sub);
    }
    c
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

fn hms(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

// ---------------------------------------------------------------- the page

impl DemoPage {
    pub fn new(tr: Arc<TextRaster>, wake: Wake) -> Self {
        let pool = Arc::new(ThreadPool::new(JOB_COUNT, "job"));
        let mut g = Graph::new(pool);
        g.set_wake(wake);

        // inputs: commands, camera, clock
        let clock = g.clock("t");
        let cam_level = g.input("camera.level", val(0i32));
        let iterations = g.input("iterations", val(256u32));
        let hue = g.input("hue", val(205.0f32));
        let streams = g.input("streams", val(Toggle { on: true, since: 0.0 }));
        let anim = g.input("animation", val(Toggle { on: true, since: 0.0 }));
        let timers = g.input("timers", val(Toggle { on: true, since: 0.0 }));

        // push streams
        let (ticks, tick_p) = g.stream::<Tick>("ticks", 600, 8192, StreamPolicy::Append);
        let (tokens, token_p) = g.stream::<Token>("tokens", 4000, 8192, StreamPolicy::Append);

        // static, engine-computed tiles: their parameters are a memoised function
        let mandel_params = g.derive(
            "mandel.params",
            op_memo("mandel.params", OpClass::Native, |i, _| {
                Ok(MandelParams { iterations: *i[0].get::<u32>().unwrap_or(&256), hue: *i[1].get::<f32>().unwrap_or(&205.0) })
            }),
            &[iterations, hue],
        );

        // GPU pull stream: a function of time while running, constant while paused
        let effect_params = g.derive(
            "effect.params",
            op_memo("effect.params", OpClass::Gpu, |i, ctx| {
                let a = toggle_of(&i[2]);
                let t = if a.on {
                    *i[0].get::<f64>().unwrap_or(&0.0)
                } else {
                    ctx.valid_until(f64::INFINITY);
                    a.since
                };
                Ok(EffectParams([t as f32, *i[1].get::<f32>().unwrap_or(&205.0), 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]))
            }),
            &[clock, hue, anim],
        );

        let mut job_seeds = Vec::new();
        let mut jobs = Vec::new();
        for k in 0..JOB_COUNT {
            let seed = g.input("job.seed", val(0u64));
            jobs.push(g.derive(&format!("job.{k}"), job_op(), &[seed]));
            job_seeds.push(seed);
        }

        let mut panels: Vec<Panel> = Vec::new();
        let mut add = |g: &mut Graph, id: u32, rect: Rect, class: Class, priority: u8, over: bool, op: Arc<dyn Op>, inputs: &[CellId]| {
            let cell = g.derive(op.name().to_string().as_str(), op, inputs);
            panels.push(Panel { id, rect, cell, class, priority, over_effects: over });
        };

        let t = tr.clone();
        add(&mut g, 0, TITLE, Class::Static, 4, false, op_inline("title.raster", OpClass::Native, move |i, _| {
            let mut c = Canvas::new(TITLE.w, TITLE.h, scale_of(&i[0]));
            c.clear([0, 0, 0, 0]);
            c.text(&t, 4.0, 4.0, 36.0, TEXT_HI, "AnyPage — 一個頁面，所有時間性");
            c.text(&t, 6.0, 54.0, 17.0, TEXT_LO, "沒有靜態與動態之分：每個面板都是函數。拖曳平移，滾輪或雙指縮放。");
            Ok(val(c.into_image(None)))
        }), &[cam_level]);

        let t = tr.clone();
        add(&mut g, 8, CLOCK, Class::PullStream, 1, false, op_inline("clock.raster", OpClass::Native, move |i, ctx| {
            let tm = toggle_of(&i[2]);
            let now = *i[1].get::<f64>().unwrap_or(&0.0);
            let shown = if tm.on {
                // predictable: this value cannot change before the next whole second
                ctx.valid_until(now.floor() + 1.0);
                now
            } else {
                ctx.valid_until(f64::INFINITY);
                tm.since
            };
            let mut c = Canvas::new(CLOCK.w, CLOCK.h, scale_of(&i[0]));
            c.clear([0, 0, 0, 0]);
            c.round_rect(0.0, 0.0, CLOCK.w, CLOCK.h, 14.0, PANEL_BG);
            badge(&mut c, &t, 18.0, 14.0, Class::PullStream);
            let x = 18.0 + badge_width(&t, Class::PullStream) + 12.0;
            c.text(&t, x, 12.0, 20.0, TEXT_HI, &format!("頁面已開啟 {}", hms(shown)));
            let sub = if tm.on {
                "可預測的時間函數：宣告「下一整秒前不會變」，引擎只在整秒醒來（T 暫停）"
            } else {
                "已暫停：宣告為常數，引擎不再為它醒來（T 繼續）"
            };
            c.text(&t, 18.0, 48.0, 14.0, TEXT_LO, sub);
            Ok(val(c.into_image(None)))
        }), &[cam_level, clock, timers]);

        let t = tr.clone();
        add(&mut g, 1, MANDEL_LABEL, Class::Static, 4, false, op_inline("mandel.label", OpClass::Native, move |i, _| {
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
        add(&mut g, 2, CONTROLS, Class::Operations, 0, false, op_inline("controls.raster", OpClass::Native, move |i, _| {
            let iters = *i[1].get::<u32>().unwrap_or(&256);
            let h = *i[2].get::<f32>().unwrap_or(&0.0);
            let (streams, anim, timers) = (toggle_of(&i[3]).on, toggle_of(&i[4]).on, toggle_of(&i[5]).on);
            let mut c = panel(&t, CONTROLS, scale_of(&i[0]), Class::Operations, "指令改輸入格，結果沿圖傳播",
                "色相一個輸入格同時影響 tile、圖表、GPU 動畫與 CPU 動畫。");
            let acc = Class::Operations.color();
            let onoff = |b: bool| if b { "開" } else { "關" };
            slider(&mut c, &t, ITER_TRACK, iter_frac(iters), &format!("迭代次數 iterations：{iters}"), acc);
            slider(&mut c, &t, HUE_TRACK, h / 360.0, &format!("色相 hue：{h:.0}°"), hue_rgba(h, 0.75, 0.95, 255));
            button(&mut c, &t, BTN_STREAMS, &format!("串流：{}（空白鍵）", onoff(streams)), Some(streams), Class::PushStream.color());
            button(&mut c, &t, BTN_ANIM, &format!("連續動畫：{}（A）", onoff(anim)), Some(anim), Class::PullStream.color());
            button(&mut c, &t, BTN_TIMERS, &format!("定時內容：{}（T）", onoff(timers)), Some(timers), Class::PullStream.color());
            button(&mut c, &t, BTN_RUN, "執行非同步任務（J）", None, Class::Async.color());
            button(&mut c, &t, BTN_CANCEL, "取消任務（C）", None, Class::Async.color());
            button(&mut c, &t, BTN_FIT, "貼合整頁（R）", None, acc);
            Ok(val(c.into_image(None)))
        }), &[cam_level, iterations, hue, streams, anim, timers]);

        let t = tr.clone();
        add(&mut g, 3, CHART, Class::PushStream, 2, false, op_inline("chart.raster", OpClass::Native, move |i, _| {
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
        add(&mut g, 4, TEXT, Class::PushStream, 2, false, op_inline("tokens.raster", OpClass::Native, move |i, ctx| {
            let tm = toggle_of(&i[3]);
            let now = *i[2].get::<f64>().unwrap_or(&0.0);
            let cursor_on = if tm.on {
                // the cursor blinks: predictable, so declare the next toggle
                let phase = (now / BLINK).floor();
                ctx.valid_until((phase + 1.0) * BLINK);
                phase as i64 % 2 == 0
            } else {
                ctx.valid_until(f64::INFINITY);
                true
            };
            let mut c = panel(&t, TEXT, scale_of(&i[0]), Class::PushStream, "逐字輸出 Token stream",
                "模擬模型逐字輸出：小片段、速率不固定；游標閃爍是可預測的時間函數。");
            let mut newest = None;
            if let Some(snap) = i[1].get::<StreamSnapshot<Token>>() {
                let all: String = snap.items.iter().map(|tk| tk.text.as_str()).collect();
                let chars: Vec<char> = all.chars().collect();
                let tail: String = chars[chars.len().saturating_sub(1600)..].iter().collect();
                let cursor = if cursor_on { "▍" } else { " " };
                c.text_box(&t, 20.0, 76.0, TEXT.w - 40.0, TEXT.h - 92.0, 21.0, TEXT_HI, &format!("{tail}{cursor}"), true);
                newest = snap.items.last().map(|tk| tk.produced);
            }
            Ok(val(c.into_image(newest)))
        }), &[cam_level, tokens, clock, timers]);

        let t = tr.clone();
        add(&mut g, 5, ORBIT, Class::PullStream, 1, false, op_inline("orbit.raster", OpClass::Native, move |i, ctx| {
            let a = toggle_of(&i[3]);
            let time = if a.on {
                *i[1].get::<f64>().unwrap_or(&0.0) as f32 // changes every frame: no declaration
            } else {
                ctx.valid_until(f64::INFINITY);
                a.since as f32
            };
            let h = *i[2].get::<f32>().unwrap_or(&0.0);
            let mut c = panel(&t, ORBIT, scale_of(&i[0]), Class::PullStream, "原生算子，依賴 t",
                if a.on { "連續動畫：每幀都變，只在可見時求值。" } else { "已暫停：宣告為常數，0 fps。" });
            let (cx, cy) = (ORBIT.w * 0.5, 72.0 + (ORBIT.h - 72.0) * 0.5);
            for k in 0..16 {
                let kf = k as f32;
                let ang = time * (0.5 + kf * 0.11) + kf * 0.7;
                let rad = 18.0 + kf * 6.2;
                let col = hue_rgba(h + kf * 18.0, 0.65, 1.0, 230);
                c.circle(cx + ang.cos() * rad * 1.6, cy + ang.sin() * rad * 0.9, 4.0 + (k % 4) as f32 * 1.6, col);
            }
            Ok(val(c.into_image(None)))
        }), &[cam_level, clock, hue, anim]);

        let t = tr.clone();
        add(&mut g, 6, EFFECT_LABEL, Class::PullStream, 1, true, op_inline("effect.label", OpClass::Native, move |i, _| {
            let mut c = Canvas::new(EFFECT_LABEL.w, EFFECT_LABEL.h, scale_of(&i[0]));
            c.clear([0, 0, 0, 0]);
            c.round_rect(0.0, 0.0, EFFECT_LABEL.w, EFFECT_LABEL.h, 14.0, [10, 12, 16, 170]);
            badge(&mut c, &t, 18.0, 13.0, Class::PullStream);
            let x = 18.0 + badge_width(&t, Class::PullStream) + 12.0;
            c.text(&t, x, 12.0, 20.0, TEXT_HI, "頁面提供的 GPU 算子（WGSL）");
            Ok(val(c.into_image(None)))
        }), &[cam_level]);

        let t = tr.clone();
        let mut job_inputs = vec![cam_level];
        job_inputs.extend(jobs.iter().copied());
        add(&mut g, 7, JOBS, Class::Async, 3, false, op_inline("jobs.raster", OpClass::Native, move |i, _| {
            let mut c = panel(&t, JOBS, scale_of(&i[0]), Class::Async, "工作池上的 Job（J 執行，C 取消）",
                "完成前畫佔位與進度；完成時合批成一次 commit；輸入沒變就不重跑。");
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
        let start = Instant::now();
        let feeds = vec![
            feeds::spawn_ticks(tick_p, streams_on.clone(), quit.clone(), start),
            feeds::spawn_tokens(token_p, streams_on.clone(), quit.clone()),
        ];

        let spec = PageSpec {
            bounds: BOUNDS,
            panels,
            tiled: vec![TiledLayer { rect: MANDEL, params: mandel_params, source: Arc::new(Mandel { rect: MANDEL }) }],
            effects: vec![EffectLayer { rect: EFFECT, wgsl: include_str!("../../../pages/demo/plasma.wgsl").to_string(), params: effect_params }],
            layers: Vec::new(),
            camera_level: Some(cam_level),
        };

        Self {
            graph: g,
            spec,
            iterations,
            hue,
            streams,
            anim,
            timers,
            job_seeds,
            jobs,
            iterations_v: 256,
            hue_v: 205.0,
            dragging: None,
            request: None,
            streams_on,
            quit,
            rng: Rng::new(0x5EED_1234_ABCD_0001),
            _feeds: feeds,
        }
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

    fn set_toggle(&mut self, cell: CellId, on: bool) {
        let since = self.graph.clock_now();
        self.graph.set_if_changed(cell, Toggle { on, since });
    }

    pub fn is_on(&self, cell_name: &str) -> bool {
        let cell = match cell_name {
            "streams" => self.streams,
            "anim" => self.anim,
            _ => self.timers,
        };
        self.graph.get::<Toggle>(cell).map_or(false, |t| t.on)
    }

    pub fn set_streams(&mut self, on: bool) {
        self.streams_on.store(on, Ordering::Relaxed);
        if self.is_on("streams") != on {
            self.set_toggle(self.streams, on);
        }
    }

    pub fn set_anim(&mut self, on: bool) {
        if self.is_on("anim") != on {
            self.set_toggle(self.anim, on);
        }
    }

    pub fn set_timers(&mut self, on: bool) {
        if self.is_on("timers") != on {
            self.set_toggle(self.timers, on);
        }
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

    fn slider_frac(&self, s: Slider, wx: f32) -> f32 {
        let track = match s {
            Slider::Iterations => ITER_TRACK,
            Slider::Hue => HUE_TRACK,
        };
        ((wx - CONTROLS.x - track.x) / track.w).clamp(0.0, 1.0)
    }

    fn slider_value(&mut self, s: Slider, frac: f32) {
        match s {
            Slider::Iterations => self.set_iterations(iter_from_frac(frac)),
            Slider::Hue => self.set_hue(frac * 360.0),
        }
    }
}

impl Page for DemoPage {
    fn parts(&mut self) -> (&mut Graph, &PageSpec) {
        (&mut self.graph, &self.spec)
    }
    fn graph(&self) -> &Graph {
        &self.graph
    }
    fn spec(&self) -> &PageSpec {
        &self.spec
    }

    fn pointer_down(&mut self, wx: f32, wy: f32) -> PointerResponse {
        if !contains(&CONTROLS, wx, wy) {
            return PointerResponse::Ignored;
        }
        let (x, y) = (wx - CONTROLS.x, wy - CONTROLS.y);
        let grab = |r: Rect| Rect::new(r.x - 12.0, r.y - 16.0, r.w + 24.0, r.h + 32.0);
        for (track, s) in [(ITER_TRACK, Slider::Iterations), (HUE_TRACK, Slider::Hue)] {
            if contains(&grab(track), x, y) {
                let f = self.slider_frac(s, wx);
                self.slider_value(s, f);
                self.dragging = Some(s);
                return PointerResponse::Captured;
            }
        }
        if contains(&BTN_STREAMS, x, y) {
            let on = !self.is_on("streams");
            self.set_streams(on);
        } else if contains(&BTN_ANIM, x, y) {
            let on = !self.is_on("anim");
            self.set_anim(on);
        } else if contains(&BTN_TIMERS, x, y) {
            let on = !self.is_on("timers");
            self.set_timers(on);
        } else if contains(&BTN_RUN, x, y) {
            self.run_jobs();
        } else if contains(&BTN_CANCEL, x, y) {
            self.cancel_jobs();
        } else if contains(&BTN_FIT, x, y) {
            self.request = Some(PageRequest::FitPage);
        } else {
            return PointerResponse::Ignored;
        }
        PointerResponse::Handled
    }

    fn pointer_drag(&mut self, wx: f32, _wy: f32) {
        if let Some(s) = self.dragging {
            let f = self.slider_frac(s, wx);
            self.slider_value(s, f);
        }
    }

    fn pointer_up(&mut self) {
        self.dragging = None;
    }

    fn key(&mut self, key: &str) -> bool {
        match key {
            "space" => {
                let on = !self.is_on("streams");
                self.set_streams(on);
            }
            "a" => {
                let on = !self.is_on("anim");
                self.set_anim(on);
            }
            "t" => {
                let on = !self.is_on("timers");
                self.set_timers(on);
            }
            "j" => self.run_jobs(),
            "c" => self.cancel_jobs(),
            "1" => {
                let n = self.iterations_v / 2;
                self.set_iterations(n);
            }
            "2" => {
                let n = self.iterations_v * 2;
                self.set_iterations(n);
            }
            _ => return false,
        }
        true
    }

    fn take_request(&mut self) -> Option<PageRequest> {
        self.request.take()
    }

    fn control(&mut self, name: &str, value: f64) -> bool {
        match name {
            "iterations" => self.set_iterations(value as u32),
            "hue" => self.set_hue(value as f32),
            "jobs" => self.run_jobs(),
            "cancel" => self.cancel_jobs(),
            "streams" => self.set_streams(value != 0.0),
            "anim" => self.set_anim(value != 0.0),
            "timers" => self.set_timers(value != 0.0),
            _ => return false,
        }
        true
    }

    fn shutdown(&mut self) {
        self.quit.store(true, Ordering::Relaxed);
        self.graph.cancel_all();
    }
}

/// Synthetic CPU work that reports progress and honours cancellation:
/// returns (a checksum, elapsed ms). Shared with the script page's `demo.compute`.
pub fn compute_job(seed: u64, ctx: &EvalCtx) -> Result<(f64, f32), String> {
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
    Ok((std::hint::black_box(acc), t0.elapsed().as_secs_f32() * 1000.0))
}

fn job_op() -> Arc<dyn Op> {
    op_async("job.compute", |i, ctx| {
        let seed = *i[0].get::<u64>().unwrap_or(&0);
        if seed == 0 {
            return Ok(val(JobResult::Idle));
        }
        let (value, ms) = compute_job(seed, ctx)?;
        Ok(val(JobResult::Done { value, ms }))
    })
}
