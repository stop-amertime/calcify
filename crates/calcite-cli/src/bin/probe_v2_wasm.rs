//! Phase 3 (a) diagnostic: wasm-codegen terminal coverage + 5-backend
//! speed report on a real cabinet.
//!
//! Mirrors `probe-v2-inlined`: builds the wasm DAG standalone for
//! coverage stats, then ticks each backend (Bytecode / DagV2 /
//! DagV2Closures / DagV2Inlined / DagV2Wasm) for the same warmup +
//! bench window and reports throughput.
//!
//! Coverage is the gating signal — if it's the same as inlined's
//! coverage (it should be, since both use the same lowered op set),
//! a wasm speed win comes from the JIT'd straight-line code rather
//! than from extra coverage.
//!
//! Usage: `probe-v2-wasm <cabinet.css> [warmup] [bench]`

use std::env;
use std::fs;
use std::time::Instant;

use calcite_core::dag::{build_dag, WasmDag};
use calcite_core::parser::parse_css;
use calcite_core::{Backend, Evaluator, State};

fn main() {
    let path = env::args()
        .nth(1)
        .expect("usage: probe-v2-wasm <cabinet.css> [warmup] [bench]");
    let warmup: u64 = env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(500);
    let bench: u64 = env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(10_000);

    eprintln!("loading {path}…");
    let css = fs::read_to_string(&path).expect("read cabinet");
    let parsed = parse_css(&css).expect("parse");

    // Coverage report (build the wasm DAG standalone).
    let dag = build_dag(&parsed);
    let n_nodes = dag.nodes.len();
    let n_terms = dag.terminals.len();
    let mut n_write_terms = 0usize;
    let mut n_indirect_terms = 0usize;
    for &t in &dag.terminals {
        match &dag.nodes[t as usize] {
            calcite_core::dag::DagNode::WriteVar { .. } => n_write_terms += 1,
            calcite_core::dag::DagNode::IndirectStore { .. } => n_indirect_terms += 1,
            _ => {}
        }
    }
    // Need state.state_var_count() to size linear memory. Build a
    // throwaway state from properties just for the count.
    let mut tmp_state = State::default();
    tmp_state.load_properties(&parsed.properties);
    let n_state_vars = tmp_state.state_var_count() as u32;
    let mc_capacity = 0u32; // memory cells trampoline through host (see eval driver)
    let wasm_dag = WasmDag::build(dag, n_state_vars, mc_capacity);
    let (total, covered) = wasm_dag.coverage();
    println!("=== wasm-codegen coverage ===");
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
    println!("  wasm bytes:          {}", wasm_dag.wasm_bytes.len());
    println!("  slabs:               {}", wasm_dag.slab_count());
    if !wasm_dag.slabs.is_empty() {
        let avg = wasm_dag.slabs.iter().map(|s| s.term_indices.len()).sum::<usize>() as f64
            / wasm_dag.slabs.len() as f64;
        let max = wasm_dag.slabs.iter().map(|s| s.term_indices.len()).max().unwrap_or(0);
        let max_locals = wasm_dag.slabs.iter().map(|s| s.local_count).max().unwrap_or(0);
        println!("    avg terms/slab:    {avg:.1}");
        println!("    max terms/slab:    {max}");
        println!("    max locals/slab:   {max_locals}");
    }

    println!();
    println!("=== speed ===");
    for &backend in &[
        Backend::Bytecode,
        Backend::DagV2,
        Backend::DagV2Closures,
        Backend::DagV2Inlined,
        Backend::DagV2Wasm,
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
