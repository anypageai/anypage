//! How expensive is the computation graph itself? (docs/ARCHITECTURE.md section 18:
//! "每頁 10⁵～10⁶ 格的失效傳播能否在幀預算內")
//!
//! cargo run --release -p anypage-core --example graph_bench

use std::sync::Arc;
use std::time::Instant;

use anypage_core::{op_inline, val, CellId, Graph, OpClass, ThreadPool};

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

fn main() {
    println!("anypage-core graph benchmark (release build recommended)\n");
    wide(100_000, 1_000);
    wide(1_000_000, 10_000);
    chain(100_000);
}
