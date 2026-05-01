//! Phase 3 (c+) diagnostic: terminal coverage + speed for the
//! per-terminal straight-line backend on a real cabinet.
//!
//! The prototype emits straight-line ops only for terminals whose cone
//! is covered by the supported op set (Lit, LoadVar, arity-2
//! arithmetic). Anything else falls back to the walker on a
//! per-terminal basis. This probe reports:
//!   - how many terminals emit vs fall back
//!   - max local count across emitted terminals (sizes the scratch)
//!   - speed of all four backends
//!
//! Coverage is the gating signal: if <10% of terminals emit, the
//! straight-line path can't move the needle no matter how fast it
//! runs. If coverage is high but speed doesn't move, the bottleneck
//! is elsewhere (probably broadcasts, which still go through the
//! walker).
//!
//! Usage: `probe-v2-inlined <cabinet.css> [warmup] [bench]`

use std::env;
use std::fs;
use std::time::Instant;

use calcite_core::dag::{build_dag, InlinedDag};
use calcite_core::parser::parse_css;
use calcite_core::{Backend, Evaluator, State};

fn main() {
    let path = env::args()
        .nth(1)
        .expect("usage: probe-v2-inlined <cabinet.css> [warmup] [bench]");
    let warmup: u64 = env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(500);
    let bench: u64 = env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(10_000);

    eprintln!("loading {path}…");
    let css = fs::read_to_string(&path).expect("read cabinet");
    let parsed = parse_css(&css).expect("parse");

    // Coverage report (build the inlined DAG standalone).
    let dag = build_dag(&parsed);
    let n_nodes = dag.nodes.len();
    let n_terms = dag.terminals.len();
    // Count WriteVar vs IndirectStore terminals so the coverage
    // fraction is reported against the right denominator.
    let mut n_write_terms = 0usize;
    let mut n_indirect_terms = 0usize;
    for &t in &dag.terminals {
        match &dag.nodes[t as usize] {
            calcite_core::dag::DagNode::WriteVar { .. } => n_write_terms += 1,
            calcite_core::dag::DagNode::IndirectStore { .. } => n_indirect_terms += 1,
            _ => {}
        }
    }
    let inlined = InlinedDag::build(dag);
    let (total, covered) = inlined.coverage();
    println!("=== inlined-codegen coverage ===");
    println!("  total nodes:         {n_nodes}");
    println!("  total terminals:     {n_terms}");
    println!("    WriteVar:          {n_write_terms}");
    println!("    IndirectStore:     {n_indirect_terms}");
    println!(
        "  emitted / total:     {covered} / {total}  ({:.1}%)",
        100.0 * covered as f64 / total as f64
    );
    println!(
        "  emitted / WriteVar:  {covered} / {n_write_terms}  ({:.1}%)",
        100.0 * covered as f64 / n_write_terms.max(1) as f64
    );

    println!();
    println!("=== speed ===");
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
}
