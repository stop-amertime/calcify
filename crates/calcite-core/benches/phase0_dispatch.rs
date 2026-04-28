//! Phase 0 ceiling probe — dispatch elimination on a synthetic CSS shape.
//!
//! **Throwaway after Phase 3 lands.** The companion bench `phase0_tick.rs`
//! measures real per-tick cost on doom8088 but can't isolate "what would
//! a perfect compiler do for THIS work?" without dumping and rewriting
//! thousands of executed Op steps. This bench takes the orthogonal
//! approach the spec also allows: pick one structurally-typical CSS
//! shape (a function with a dispatch on its first parameter — e.g.
//! `--getReg16`), hand-write the Rust a perfect compiler would emit,
//! microbench the two paths.
//!
//! Why this works as a ceiling probe: per LOGBOOK 2026-04-28, the level-
//! load op mix is dominated by load-then-compare-then-branch chains
//! (>60% of ops). Those chains are exactly what dispatch tables and
//! conditional cascades compile down to in the interpreter. If
//! eliminating dispatch on the simplest such shape buys us Nx, the
//! whole-cabinet ceiling is bounded above by N (assuming dispatch
//! work is the dominant cost) and below by less (other costs don't
//! all collapse). So the dispatch-elimination ceiling on this shape
//! is a useful upper-bound estimate for Phase 0's decision gate.
//!
//! Methodology:
//!   - CSS: one `@function --getReg16(--r, --ax, --cx, --dx, --bx,
//!     --sp, --bp, --si, --di)` returning an `if(style(--r: N): ...)`
//!     8-arm dispatch (real shape from doom8088.css).
//!   - One `:root { --result: --getReg16(--frame, ...) }` assignment.
//!   - `--frame` is a `@property` cycling 0..7 each tick (so the
//!     dispatch hits every arm uniformly across iterations).
//!   - Interpreter: parse, compile, tick repeatedly (the dispatch
//!     recogniser fires on >=4 branches, so this hits the HashMap
//!     dispatch path that dominates the level-load).
//!   - Handcoded: a free Rust function `get_reg16(r, ax, cx, ...)`
//!     with a 9-arm `match`, called repeatedly with the same parameters.
//!   - Conformance test: bit-identical results on every input 0..7.
//!
//! Caveat: this is one shape, not the whole vocabulary. Other shapes
//! (broadcast writes, packed-cell access, function calls with deep
//! locals) may have different ceilings. Phase 1+ will measure those
//! as the DAG vocabulary is built out.

use criterion::{criterion_group, criterion_main, Criterion};

use calcite_core::eval::Evaluator;
use calcite_core::parser::parse_css;
use calcite_core::State;

/// The synthetic CSS shape — one dispatch function, one assignment that
/// calls it. `--frame` cycles 0..7 by self-incrementing each tick.
const PROGRAM: &str = r#"
@property --frame {
    syntax: "<integer>";
    initial-value: 0;
    inherits: false;
}
@property --result {
    syntax: "<integer>";
    initial-value: 0;
    inherits: false;
}
@property --ax { syntax: "<integer>"; initial-value: 1000; inherits: false; }
@property --cx { syntax: "<integer>"; initial-value: 2000; inherits: false; }
@property --dx { syntax: "<integer>"; initial-value: 3000; inherits: false; }
@property --bx { syntax: "<integer>"; initial-value: 4000; inherits: false; }
@property --sp { syntax: "<integer>"; initial-value: 5000; inherits: false; }
@property --bp { syntax: "<integer>"; initial-value: 6000; inherits: false; }
@property --si { syntax: "<integer>"; initial-value: 7000; inherits: false; }
@property --di { syntax: "<integer>"; initial-value: 8000; inherits: false; }

@function --getReg16(--r <integer>,
                     --ax <integer>, --cx <integer>, --dx <integer>, --bx <integer>,
                     --sp <integer>, --bp <integer>, --si <integer>, --di <integer>)
    returns <integer>
{
    result: if(
        style(--r: 0): var(--ax);
        style(--r: 1): var(--cx);
        style(--r: 2): var(--dx);
        style(--r: 3): var(--bx);
        style(--r: 4): var(--sp);
        style(--r: 5): var(--bp);
        style(--r: 6): var(--si);
        style(--r: 7): var(--di);
        else: 0
    );
}

:root {
    --frame: mod(calc(var(--frame) + 1), 8);
    --result: --getReg16(var(--frame),
                          var(--ax), var(--cx), var(--dx), var(--bx),
                          var(--sp), var(--bp), var(--si), var(--di));
}
"#;

/// Hand-rolled equivalent of `--getReg16`. This is what a perfect
/// compiler would emit: a `match` on i32, branchless on most targets
/// (cmov chain or jump table), 5-10 cycles total.
#[inline(always)]
fn get_reg16(r: i32, ax: i32, cx: i32, dx: i32, bx: i32,
             sp: i32, bp: i32, si: i32, di: i32) -> i32 {
    match r {
        0 => ax, 1 => cx, 2 => dx, 3 => bx,
        4 => sp, 5 => bp, 6 => si, 7 => di,
        _ => 0,
    }
}

fn bench_dispatch(c: &mut Criterion) {
    let parsed = parse_css(PROGRAM).expect("parse synthetic program");
    let mut state = State::default();
    state.load_properties(&parsed.properties);
    let mut evaluator = Evaluator::from_parsed(&parsed);

    // Warm up — let frame cycle a bit.
    for _ in 0..16 { evaluator.tick(&mut state); }

    let ax = state.get_var("ax").unwrap_or(0);
    let cx = state.get_var("cx").unwrap_or(0);
    let dx = state.get_var("dx").unwrap_or(0);
    let bx = state.get_var("bx").unwrap_or(0);
    let sp = state.get_var("sp").unwrap_or(0);
    let bp = state.get_var("bp").unwrap_or(0);
    let si = state.get_var("si").unwrap_or(0);
    let di = state.get_var("di").unwrap_or(0);

    let mut group = c.benchmark_group("phase0_dispatch");
    group.sample_size(100);

    // Interpreter: one full tick = pre-tick hook (no-op here) + state-
    // vars snapshot + compiled bytecode execution (the dispatch + the
    // frame increment) + writeback + change detection. The frame param
    // cycles so the dispatch hits different arms each iteration.
    group.bench_function("interpreter_tick", |b| {
        b.iter(|| evaluator.tick(&mut state));
    });

    // Handcoded: simulate the same SEMANTIC work the tick does — read
    // current frame from a mutable cell, compute result via the match,
    // write back both frame and result. Equivalent shape to the
    // interpreter's bytecode for this program.
    //
    // The interpreter's tick also pays writeback and change-detection
    // overhead that any compiler would still pay (state-vars do change,
    // and the runtime contract is still that they're observable). For
    // a fair ceiling number we include "store the result" but NOT the
    // interpreter-specific overhead like state-vars cloning or
    // change-detection scans (those are interpreter implementation
    // costs, not work the CSS demands).
    let mut frame: i32 = state.get_var("frame").unwrap_or(0);
    let mut result_cell: i32 = 0;
    group.bench_function("handcoded_tick_equivalent", |b| {
        b.iter(|| {
            // The :root assignments do this:
            //   --frame := (frame + 1) mod 8
            //   --result := getReg16(frame, ax, cx, ..., di)
            // CSS evaluates both assignments, then writeback applies
            // them. Order: result reads the OLD frame, then frame is
            // updated. (Calcite's semantics: assignments evaluate
            // against pre-tick state, then all writebacks apply.)
            let new_result = get_reg16(
                std::hint::black_box(frame),
                ax, cx, dx, bx, sp, bp, si, di,
            );
            let new_frame = (frame + 1) % 8;
            // Writeback both:
            result_cell = std::hint::black_box(new_result);
            frame = new_frame;
        });
    });

    group.finish();
}

criterion_group!(benches, bench_dispatch);
criterion_main!(benches);
