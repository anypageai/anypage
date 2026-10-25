//! Where time goes on the Luau demo page: each panel's script function
//! (records a display list) next to the native raster of that list.
//!
//!   cargo run --release -p anypage-demo --example script_costs

use std::sync::Arc;
use std::time::{Duration, Instant};

use anypage_core::{val, CellId};
use anypage_demo::script;
use anypage_engine::{Page, TextRaster};

fn median(v: &mut [f32]) -> f32 {
    v.sort_by(f32::total_cmp);
    v[v.len() / 2]
}

fn main() {
    let text = Arc::new(TextRaster::load().expect("font"));
    let mut page = script::load(text, Arc::new(|| {})).expect("load");
    println!("{}", page.summary());
    let cam = page.spec().camera_level.expect("camera level cell");
    let panels: Vec<CellId> = page.spec().panels.iter().map(|p| p.cell).collect();
    let names: Vec<String> = panels.iter().map(|c| page.graph().label(page.graph().inputs_of(*c)[0]).to_string()).collect();
    let mut script_us: Vec<Vec<f32>> = vec![Vec::new(); panels.len()];
    let mut raster_us: Vec<Vec<f32>> = vec![Vec::new(); panels.len()];
    let start = Instant::now();
    std::thread::sleep(Duration::from_millis(1500)); // let the streams fill
    for round in 0..40i32 {
        let g = page.parts().0;
        g.commit();
        g.tick(start.elapsed().as_secs_f64());
        // alternate the camera level so every raster runs each round
        g.set(cam, val(round % 2));
        for (k, cell) in panels.iter().enumerate() {
            let before = g.stats.inline_evals;
            g.demand(*cell);
            let ran = g.stats.inline_evals - before;
            raster_us[k].push(g.eval_us(*cell));
            if ran >= 2 {
                script_us[k].push(g.eval_us(g.inputs_of(*cell)[0]));
            }
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    println!("\n| panel | script runs | script function p50 (µs) | native raster p50 (µs) |\n|---|---|---|---|");
    for k in 0..panels.len() {
        let runs = script_us[k].len();
        let s = if runs > 0 { format!("{:.0}", median(&mut script_us[k])) } else { "—".into() };
        println!("| {} | {} / 40 | {} | {:.0} |", names[k].trim_end_matches(".draw"), runs, s, median(&mut raster_us[k]));
    }
    page.shutdown();
}
