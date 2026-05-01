//! Diagnostic: per-tick visit/memo-hit/handler-call counts for the v2
//! closure backend (which mirrors the walker's visit shape since it
//! uses the same memo + recursion pattern).
//!
//! Question: of the ~16 µs/tick the v2 walker spends on rogue, how
//! does that decompose? Specifically:
//!   - How many node visits per tick? (vs v1's ~35K ops/tick)
//!   - How many of those hit the memo (cheap) vs invoke a handler?
//!   - What's the cost per handler call? (tick_ns / handler_calls)
//!
//! Different ratios point to different Phase 3 strategies:
//!   - Many visits, low cost each → DAG granularity is too fine,
//!     codegen needs to flatten/inline reachability cones.
//!   - Few visits, high cost each → per-handler logic is the cost
//!     (calc, function calls), codegen needs to specialise hot
//!     handlers.
//!
//! Usage: `probe-v2-flame <cabinet.css> [warmup] [bench]`

use std::env;
use std::fs;
use std::time::Instant;

use calcite_core::dag::closure_codegen::stats;
use calcite_core::parser::parse_css;
use calcite_core::{Backend, Evaluator, State};

fn main() {
    let path = env::args()
        .nth(1)
        .expect("usage: probe-v2-flame <cabinet.css> [warmup] [bench]");
    let warmup: u64 = env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(500);
    let bench: u64 = env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(20_000);

    eprintln!("loading {path}…");
    let css = fs::read_to_string(&path).expect("read cabinet");
    let parsed = parse_css(&css).expect("parse");

    // Run all three backends for speed comparison.
    for &backend in &[Backend::Bytecode, Backend::DagV2, Backend::DagV2Closures] {
        let mut state = State::default();
        state.load_properties(&parsed.properties);
        let mut ev = Evaluator::from_parsed(&parsed);
        ev.set_backend(backend);
        for _ in 0..warmup {
            ev.tick(&mut state);
        }
        let t0 = Instant::now();
        for _ in 0..bench {
            ev.tick(&mut state);
        }
        let dt = t0.elapsed();
        println!(
            "{backend:?}: {bench} ticks in {dt:?} ({:.2} µs/tick, {:.0} ticks/s)",
            dt.as_secs_f64() * 1e6 / bench as f64,
            bench as f64 / dt.as_secs_f64()
        );
    }

    // Then run closures with stats on for visit-count breakdown.
    println!();
    println!("=== closure-backend visit counters ===");
    let mut state = State::default();
    state.load_properties(&parsed.properties);
    let mut ev = Evaluator::from_parsed(&parsed);
    ev.set_backend(Backend::DagV2Closures);
    for _ in 0..warmup {
        ev.tick(&mut state);
    }
    stats::reset();
    stats::enable();
    let t0 = Instant::now();
    for _ in 0..bench {
        ev.tick(&mut state);
    }
    let dt = t0.elapsed();
    let (visits, memo_hits, handler_calls) = stats::snapshot();
    let visits_per_tick = visits as f64 / bench as f64;
    let hits_per_tick = memo_hits as f64 / bench as f64;
    let calls_per_tick = handler_calls as f64 / bench as f64;
    let hit_rate = 100.0 * memo_hits as f64 / visits as f64;
    let ns_per_tick = dt.as_nanos() as f64 / bench as f64;
    let ns_per_visit = ns_per_tick / visits_per_tick;
    let ns_per_call = ns_per_tick / calls_per_tick;
    println!("  bench window:        {bench} ticks in {dt:?}");
    println!("  ticks/s (with stats): {:.0}", bench as f64 / dt.as_secs_f64());
    println!("  visits/tick:         {visits_per_tick:.0}");
    println!("  memo hits/tick:      {hits_per_tick:.0}  ({hit_rate:.1}%)");
    println!("  handler calls/tick:  {calls_per_tick:.0}");
    println!();
    println!("  ns/tick:             {ns_per_tick:.0}");
    println!("  ns/visit (avg):      {ns_per_visit:.2}");
    println!("  ns/handler call:     {ns_per_call:.2}");
    println!();
    println!("Notes:");
    println!("  - 'visits' counts every eval_node entry (memo hit OR handler call)");
    println!("  - the walker has the same visit shape; closures pay the same memo + recursion cost");
    println!("  - compare against v1: rogue at ~35K ops/tick (per docs/log.md 2026-04-14)");
}
