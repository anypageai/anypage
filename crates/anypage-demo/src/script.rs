//! The demo as a page package (pages/demo: page.json + main.luau + plasma.wgsl)
//! and the native operators it names. The Luau page is the same page as
//! [`crate::DemoPage`], built through the runtime instead of in Rust.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anypage_core::{op_async, op_inline, val, OpClass, Rect, StreamPolicy, StreamSnapshot, TileKey, Value};
use anypage_engine::{TextRaster, TileSource, Wake};
use anypage_runtime::{Data, Natives, Package, ScriptPage, StreamHandle, Streamed};

use crate::feeds::{self, Tick, Token};
use crate::mandel::{Mandel, MandelParams};
use crate::page::compute_job;

pub const PAGE_JSON: &str = include_str!("../../../pages/demo/page.json");
pub const MAIN_LUAU: &str = include_str!("../../../pages/demo/main.luau");
pub const PLASMA_WGSL: &str = include_str!("../../../pages/demo/plasma.wgsl");

/// The demo package compiled into the app.
pub fn package() -> Package {
    Package::from_files("builtin:demo", [("page.json", PAGE_JSON), ("main.luau", MAIN_LUAU), ("plasma.wgsl", PLASMA_WGSL)])
        .expect("built-in demo package is valid")
}

struct MandelData(Mandel);

impl TileSource for MandelData {
    fn render(&self, key: TileKey, params: &Value, cancelled: &dyn Fn() -> bool) -> Option<Vec<u8>> {
        let d = params.downcast_ref::<Data>()?;
        let p = MandelParams {
            iterations: d.get("iterations").num_or(256.0).clamp(8.0, 16384.0) as u32,
            hue: d.get("hue").num_or(205.0) as f32,
        };
        self.0.render_with(key, p, cancelled)
    }
}

/// Native operators this app offers to pages.
pub fn natives() -> Natives {
    let mut n = Natives::new();

    n.stream("demo.ticks", |g, spec| {
        let cap = spec.opts.get("cap").num_or(600.0).clamp(2.0, 4096.0) as usize;
        let (raw, producer) = g.stream::<Tick>(spec.label, cap, 8192, StreamPolicy::Append);
        let thread = feeds::spawn_ticks(producer, spec.on, spec.quit, Instant::now());
        let to_data = op_inline("demo.ticks.data", OpClass::Native, |i, _| {
            let Some(s) = i[0].get::<StreamSnapshot<Tick>>().filter(|s| !s.items.is_empty()) else {
                return Ok(val(Data::Nil));
            };
            let data = Data::map([
                ("t", Data::nums(s.items.iter().map(|x| x.t as f64))),
                ("v", Data::nums(s.items.iter().map(|x| x.v as f64))),
                ("total", Data::Num(s.total as f64)),
            ]);
            Ok(val(Streamed { data, newest: s.items.last().map(|x| x.produced) }))
        });
        let cell = g.derive(&format!("{}.data", spec.label), to_data, &[raw]);
        Ok(StreamHandle { cell, threads: vec![thread] })
    });

    n.stream("demo.tokens", |g, spec| {
        let cap = spec.opts.get("cap").num_or(4000.0).clamp(1.0, 4096.0) as usize;
        let tail = spec.opts.get("tail").num_or(1600.0).clamp(1.0, 20000.0) as usize;
        let (raw, producer) = g.stream::<Token>(spec.label, cap, 8192, StreamPolicy::Append);
        let thread = feeds::spawn_tokens(producer, spec.on, spec.quit);
        // the join and the tail happen here, natively: the script gets one string
        let to_data = op_inline("demo.tokens.data", OpClass::Native, move |i, _| {
            let Some(s) = i[0].get::<StreamSnapshot<Token>>().filter(|s| !s.items.is_empty()) else {
                return Ok(val(Data::Nil));
            };
            let all: String = s.items.iter().map(|x| x.text.as_str()).collect();
            let skip = all.chars().count().saturating_sub(tail);
            let text: String = all.chars().skip(skip).collect();
            let data = Data::map([("text", Data::str(&text)), ("total", Data::Num(s.total as f64))]);
            Ok(val(Streamed { data, newest: s.items.last().map(|x| x.produced) }))
        });
        let cell = g.derive(&format!("{}.data", spec.label), to_data, &[raw]);
        Ok(StreamHandle { cell, threads: vec![thread] })
    });

    n.job("demo.compute", |_opts| {
        Ok(op_async("demo.compute", |i, ctx| {
            let seed = i[0].get::<Data>().and_then(Data::num).unwrap_or(0.0).max(0.0) as u64;
            if seed == 0 {
                return Ok(val(Data::map([("state", Data::str("idle"))])));
            }
            let (value, ms) = compute_job(seed, ctx)?;
            Ok(val(Data::map([("state", Data::str("done")), ("value", Data::Num(value)), ("ms", Data::Num(ms as f64))])))
        }))
    });

    n.tile("mandelbrot", |rect: Rect, _opts| Ok(Arc::new(MandelData(Mandel { rect })) as Arc<dyn TileSource>));

    n
}

/// The built-in Luau demo page.
pub fn load(text: Arc<TextRaster>, wake: Wake) -> Result<ScriptPage, String> {
    ScriptPage::load(&package(), &natives(), text, wake)
}

/// A page package from a folder, with this app's native operators.
pub fn load_dir(dir: &Path, text: Arc<TextRaster>, wake: Wake) -> Result<ScriptPage, String> {
    ScriptPage::load(&Package::from_dir(dir)?, &natives(), text, wake)
}
