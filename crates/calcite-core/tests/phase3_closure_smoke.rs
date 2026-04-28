//! Phase 3 closure-backend smoke: run a small batch of ticks under
//! each backend on `web/demo.css`, compare timings. Informational —
//! the spec gates Phase 3 final on a 5x speedup over interpreter, but
//! the closure prototype is expected to be near parity (or slower)
//! since most ops still route through `exec_ops`.

use std::path::PathBuf;
use std::time::Instant;

use calcite_core::eval::{Backend, Evaluator};
use calcite_core::parser::parse_css;
use calcite_core::State;

const TICKS: u32 = 500;
const WARMUP: u32 = 50;

fn worktree_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

fn time_ticks(css: &str, backend: Backend) -> f64 {
    let parsed = parse_css(css).expect("parse cabinet css");
    let mut state = State::default();
    state.load_properties(&parsed.properties);
    let mut ev = Evaluator::from_parsed(&parsed);
    ev.set_backend(backend);
    ev.wire_state_for_packed_memory(&mut state);
    for _ in 0..WARMUP { ev.tick(&mut state); }
    let t0 = Instant::now();
    for _ in 0..TICKS { ev.tick(&mut state); }
    t0.elapsed().as_secs_f64()
}

#[test]
fn phase3_closure_smoke_timings() {
    let cabinet = worktree_root().join("web").join("demo.css");
    if !cabinet.is_file() {
        eprintln!("phase3_closure_smoke: skipping — no cabinet at {}", cabinet.display());
        return;
    }
    let css = std::fs::read_to_string(&cabinet).expect("read cabinet");
    let bytecode_s = time_ticks(&css, Backend::Bytecode);
    let dag_s = time_ticks(&css, Backend::Dag);
    let closure_s = time_ticks(&css, Backend::Closure);
    eprintln!(
        "{TICKS} ticks: bytecode={:.3}s ({:.0} t/s), dag={:.3}s ({:.0} t/s), closure={:.3}s ({:.0} t/s)",
        bytecode_s, TICKS as f64 / bytecode_s,
        dag_s,      TICKS as f64 / dag_s,
        closure_s,  TICKS as f64 / closure_s,
    );
    eprintln!(
        "ratios: dag/bytecode = {:.2}x, closure/bytecode = {:.2}x",
        dag_s / bytecode_s,
        closure_s / bytecode_s,
    );
}
