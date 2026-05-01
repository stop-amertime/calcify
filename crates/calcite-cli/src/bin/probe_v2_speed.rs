//! Side-by-side wall-clock comparison of v1 (Backend::Bytecode) and
//! v2 (Backend::DagV2) on a real cabinet.
//!
//! Per `docs/compiler-mission.md`, Phase 1 had no perf component and
//! Phase 2's gate is node-count reduction, not ticks/sec. This probe
//! is a sanity instrument — "is v2 even runnable on a cabinet, and
//! roughly how slow?" — not a perf-claim source.
//!
//! Usage: `probe_v2_speed <cabinet.css> [warmup_ticks] [bench_ticks]`

use std::env;
use std::fs;
use std::time::Instant;

use calcite_core::{Backend, Evaluator, State};
use calcite_core::parser::parse_css;

fn main() {
    let path = env::args().nth(1).expect("usage: probe_v2_speed <cabinet.css> [warmup] [bench]");
    let warmup: u64 = env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(500);
    let bench: u64 = env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(2000);

    eprintln!("loading {path}…");
    let css = fs::read_to_string(&path).expect("read cabinet");
    let parsed = parse_css(&css).expect("parse");

    for &backend in &[
        Backend::Bytecode,
        Backend::DagV2,
        Backend::DagV2Closures,
        Backend::DagV2Inlined,
    ] {
        let mut state = State::default();
        state.load_properties(&parsed.properties);
        let mut ev = Evaluator::from_parsed(&parsed);
        ev.set_backend(backend);
        eprint!("{backend:?}: warmup… ");
        for _ in 0..warmup {
            ev.tick(&mut state);
        }
        eprintln!("done.");

        let t0 = Instant::now();
        for _ in 0..bench {
            ev.tick(&mut state);
        }
        let dt = t0.elapsed();
        let per_tick_us = dt.as_secs_f64() * 1e6 / bench as f64;
        let ticks_per_s = bench as f64 / dt.as_secs_f64();
        println!(
            "{backend:?}: {bench} ticks in {dt:?} ({per_tick_us:.2} µs/tick, {ticks_per_s:.0} ticks/s)"
        );
    }
}

