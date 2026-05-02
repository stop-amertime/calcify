//! Long-horizon wall-clock probe. For each backend, parses the cabinet
//! and starts ticking from frame 0 with no warmup; runs until a fixed
//! wall-clock budget elapses. Reports total ticks completed and a final
//! state hash so we can see (a) which backend ran fastest and (b) that
//! they all did the same work.
//!
//! Why no warmup: cold start is part of what ships. Boot CSS-DOS for
//! the first time and you pay the cold cost; hiding it behind a warmup
//! gives flattering steady-state numbers that don't show up to a user.
//!
//! Why fixed wall-clock instead of fixed ticks: the question we care
//! about is "how much progress per second of real time", and giving
//! every backend the same window answers that directly.
//!
//! Usage: `probe-v2-long <cabinet.css> [seconds]`
//!
//!   probe-v2-long ../../output/zork1.css 30
//!
//! Defaults to 30 seconds per backend.

use std::env;
use std::fs;
use std::time::{Duration, Instant};

use calcite_core::dag::{build_dag, WasmDag};
use calcite_core::parser::parse_css;
use calcite_core::{Backend, Evaluator, State};

fn main() {
    let path = env::args()
        .nth(1)
        .expect("usage: probe-v2-long <cabinet.css> [seconds]");
    let seconds: u64 = env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(30);
    let budget = Duration::from_secs(seconds);

    eprintln!("loading {path}…");
    let css = fs::read_to_string(&path).expect("read cabinet");
    let parsed = parse_css(&css).expect("parse");

    // Coverage report (cheap; uses its own State).
    {
        let dag = build_dag(&parsed);
        let n_terms = dag.terminals.len();
        let mut tmp_state = State::default();
        tmp_state.load_properties(&parsed.properties);
        let n_state_vars = tmp_state.state_var_count() as u32;
        let mc_capacity = 0u32;
        let wasm_dag = WasmDag::build(dag, n_state_vars, mc_capacity);
        let (total, covered) = wasm_dag.coverage();
        println!("=== coverage ===");
        println!(
            "  emitted / total terminals: {covered} / {total}  ({:.1}%)",
            100.0 * covered as f64 / total.max(1) as f64
        );
        println!("  wasm bytes: {}", wasm_dag.wasm_bytes.len());
        println!("  state vars: {n_state_vars}");
        println!();
    }

    println!(
        "=== speed: {seconds}s wall-clock per backend, no warmup ==="
    );
    for &backend in &[
        Backend::Bytecode,
        Backend::DagV2,
        Backend::DagV2Closures,
        Backend::DagV2Inlined,
        Backend::DagV2Wasm,
    ] {
        // Fresh state + evaluator for each backend so they all start
        // from the same cold tick 0.
        let mut state = State::default();
        state.load_properties(&parsed.properties);
        let mut ev = Evaluator::from_parsed(&parsed);
        ev.set_backend(backend);

        // Tick in batches between wall-clock checks so we don't pay
        // the Instant::now() cost on every tick. 1024 is small enough
        // that the timing-overrun is bounded.
        const BATCH: u64 = 1024;
        let start = Instant::now();
        let mut ticks: u64 = 0;
        loop {
            for _ in 0..BATCH {
                ev.tick(&mut state);
            }
            ticks += BATCH;
            if start.elapsed() >= budget {
                break;
            }
        }
        let elapsed = start.elapsed();
        let rate = ticks as f64 / elapsed.as_secs_f64();
        let hash = hash_state(&state);
        println!(
            "{backend:?}: {ticks} ticks in {:.2}s ({:.0} ticks/s, {:.2} µs/tick)  state-hash={hash:016x}",
            elapsed.as_secs_f64(),
            rate,
            elapsed.as_secs_f64() * 1e6 / ticks as f64,
        );
    }
}

/// Sum-then-fold hash over state-vars + memory. Not cryptographic —
/// just a quick "did the backends end up in the same place" check.
/// Two backends with the same hash after the same number of ticks
/// from the same initial state are doing the same work.
fn hash_state(state: &State) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for v in &state.state_vars {
        h = h.wrapping_mul(0x100000001b3).wrapping_add(*v as u64);
    }
    for &b in &state.memory {
        h = h.wrapping_mul(0x100000001b3).wrapping_add(b as u64);
    }
    h
}
