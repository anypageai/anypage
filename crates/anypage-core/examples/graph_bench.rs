//! How expensive is the computation graph itself? (docs/ARCHITECTURE.md section 18:
//! "每頁 10⁵～10⁶ 格的失效傳播能否在幀預算內")
//!
//! cargo run --release -p anypage-core --example graph_bench

use std::sync::Arc;
use std::time::Instant;

use anypage_core::{op_inline, op_memo, val, CellId, Graph, OpClass, ThreadPool};

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

fn wide(cells: usize, inputs: usize) {
    let mut g = Graph::new(Arc::new(ThreadPool::new(1, "bench")));
    let sum = op_inline("sum2", OpClass::Native, |inp, _| {
        Ok(val(inp.iter().map(|i| *i.get::<i64>().unwrap()).sum::<i64>()))
    });

    let t_build = Instant::now();
    let mut ids: Vec<CellId> = (0..inputs).map(|i| g.input("in", val(i as i64))).collect();
    let mut layer: Vec<CellId> = ids.clone();
    // Each derived cell sums two cells of the previous layer, so one input
    // reaches a growing cone of cells layer by layer.
    while ids.len() < cells {
        let mut next = Vec::with_capacity(layer.len());
        for k in 0..layer.len() {
            if ids.len() >= cells {
                break;
            }
            let a = layer[k];
            let b = layer[(k + 1) % layer.len()];
            let id = g.derive("sum", sum.clone(), &[a, b]);
            next.push(id);
            ids.push(id);
        }
        layer = next;
    }
    let build_ms = ms(t_build);
    let sinks = layer;

    let t_full = Instant::now();
    for &s in &sinks {
        g.demand(s);
    }
    let full_ms = ms(t_full);
    let evals_full = g.stats.inline_evals;

    let t_mark = Instant::now();
    let marked = g.set(ids[0], val(1_000i64));
    let mark_ms = ms(t_mark);

    let t_re = Instant::now();
    for &s in &sinks {
        g.demand(s);
    }
    let re_ms = ms(t_re);
    let evals_re = g.stats.inline_evals - evals_full;

    let t_noop = Instant::now();
    for &s in &sinks {
        g.demand(s);
    }
    let noop_ms = ms(t_noop);

    println!(
        "| wide fan-in, {cells} cells, {inputs} inputs, {} sinks | build {build_ms:.1} ms | first full eval {full_ms:.1} ms ({evals_full} evals) | set 1 input: {marked} cells marked in {mark_ms:.3} ms | re-eval {re_ms:.2} ms ({evals_re} evals) | demand with nothing dirty {noop_ms:.3} ms |",
        sinks.len()
    );
}

fn chain(len: usize) {
    let mut g = Graph::new(Arc::new(ThreadPool::new(1, "bench")));
    let inc = op_inline("inc", OpClass::Native, |inp, _| Ok(val(inp[0].get::<i64>().unwrap() + 1)));
    let root = g.input("root", val(0i64));
    let mut prev = root;
    for _ in 0..len {
        prev = g.derive("inc", inc.clone(), &[prev]);
    }
    let t_full = Instant::now();
    g.demand(prev);
    let full_ms = ms(t_full);
    let t_mark = Instant::now();
    let marked = g.set(root, val(5i64));
    let mark_ms = ms(t_mark);
    let t_re = Instant::now();
    let v = *g.demand(prev).get::<i64>().unwrap();
    let re_ms = ms(t_re);
    assert_eq!(v, 5 + len as i64);
    println!(
        "| chain of {len} | first eval {full_ms:.1} ms | set root: {marked} marked in {mark_ms:.2} ms | re-eval whole chain {re_ms:.2} ms |"
    );
}

/// Early cutoff: 100k cells behind a clamp whose output does not change.
fn cutoff(cells: usize) {
    let mut g = Graph::new(Arc::new(ThreadPool::new(1, "bench")));
    let x = g.input("x", val(500i64));
    let clamp = g.derive("clamp", op_memo("clamp", OpClass::Native, |i, _| Ok((*i[0].get::<i64>().unwrap()).min(100))), &[x]);
    let inc = op_inline("inc", OpClass::Native, |inp, _| Ok(val(inp[0].get::<i64>().unwrap() + 1)));
    let sinks: Vec<CellId> = (0..cells).map(|_| g.derive("leaf", inc.clone(), &[clamp])).collect();
    for &s in &sinks {
        g.demand(s);
    }
    let evals = g.stats.inline_evals;
    let t = Instant::now();
    g.set(x, val(900i64));
    for &s in &sinks {
        g.demand(s);
    }
    let ms_ = ms(t);
    println!(
        "| early cutoff: input changes, clamp output does not, {cells} dependents | {ms_:.2} ms | {} operator runs | {} cutoffs | {} verified without running |",
        g.stats.inline_evals - evals,
        g.stats.cutoffs,
        g.stats.verified
    );
}

/// Predictable time: many clock readers that declared they hold for a second.
fn clock(readers: usize) {
    let mut g = Graph::new(Arc::new(ThreadPool::new(1, "bench")));
    let clock = g.clock("t");
    let sinks: Vec<CellId> = (0..readers)
        .map(|k| {
            let period = 1.0 + (k % 7) as f64 * 0.25;
            g.derive(
                "ticker",
                op_memo("ticker", OpClass::Native, move |i, ctx| {
                    let t = *i[0].get::<f64>().unwrap();
                    let n = (t / period).floor();
                    ctx.valid_until((n + 1.0) * period);
                    Ok(n as i64)
                }),
                &[clock],
            )
        })
        .collect();
    g.tick(0.0);
    for &s in &sinks {
        g.demand(s);
    }
    let evals = g.stats.inline_evals;
    // 60 frames of a 60 Hz loop: one second of wall time
    let t = Instant::now();
    let mut wakes = 0;
    let mut now = 0.0;
    for _ in 0..60 {
        now += 1.0 / 60.0;
        if g.next_expiry(&sinks[..1]) <= now {
            wakes += 1;
        }
        g.tick(now);
        for &s in &sinks {
            g.demand(s);
        }
    }
    let ms_ = ms(t);
    println!(
        "| predictable time: {readers} clock readers, 60 ticks (1 s) | {ms_:.2} ms total, {:.3} ms per tick | {} operator runs instead of {} | first reader wanted {wakes} of 60 frames |",
        ms_ / 60.0,
        g.stats.inline_evals - evals,
        readers * 60
    );
}

/// Worst case, sliced: a 100k chain recomputed under a 4 ms per-frame budget.
fn sliced_chain(len: usize) {
    let mut g = Graph::new(Arc::new(ThreadPool::new(1, "bench")));
    let inc = op_inline("inc", OpClass::Native, |inp, _| Ok(val(inp[0].get::<i64>().unwrap() + 1)));
    let root = g.input("root", val(0i64));
    let mut prev = root;
    for _ in 0..len {
        prev = g.derive("inc", inc.clone(), &[prev]);
    }
    g.demand(prev);
    g.set(root, val(1i64));
    let mut frames = 0;
    let mut worst = 0.0f64;
    loop {
        frames += 1;
        let t = Instant::now();
        let done = g.demand_until(prev, t + std::time::Duration::from_millis(4)).is_some();
        worst = worst.max(ms(t));
        if done {
            break;
        }
    }
    println!("| chain of {len}, 4 ms budget per frame | finished in {frames} frames | longest slice {worst:.2} ms |");
}

fn main() {
    println!("anypage-core graph benchmark (release build recommended)\n");
    wide(100_000, 1_000);
    wide(1_000_000, 10_000);
    chain(100_000);
    cutoff(100_000);
    clock(10_000);
    sliced_chain(100_000);
}
