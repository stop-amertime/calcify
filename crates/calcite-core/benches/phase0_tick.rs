//! Phase 0 ceiling probe — single tick of Doom8088 level-load work.
//!
//! **Throwaway after Phase 3 lands.** This bench exists to answer one
//! question (mission doc § Phase 0): if a perfect compiler emitted Rust
//! for the work the CSS does in one tick, how much faster is that Rust
//! than the bytecode interpreter doing the same tick? See
//! `docs/compiler-mission.md` and `docs/compiler-spec.md`. Decision gate:
//! >=10x ceiling commits us to the compiler road; 3-10x recalibrates
//! the 30-100x estimate; <3x abandons the road.
//!
//! Region pick: the spec named segments 0x2D96 (~15%) and 0x55 (~67.8%)
//! as candidates. Initial plan was 0x2D96 (smaller, cleaner). Reality:
//! the `stage_loading` snapshot starts deep inside 0x55, and stepping
//! forward to find a 0x2D96 tick takes >200K ticks at REP-fastfwd-off
//! pace (>100s startup). Switched to 0x55 which is where state already
//! sits — and which is the bigger ceiling-relevant region anyway.
//!
//! Inputs:
//!   - <CSS-DOS>/tmp/doom8088.css                  (~358 MB cabinet)
//!   - <CSS-DOS>/tmp/doom-snapshots-pj/stage_loading.snap  (~1.5 MB state)
//!
//! Both files are produced by the CSS-DOS harness (see
//! `tests/harness/bench-doom-stages.mjs --capture-snapshots=DIR`); they
//! are not in git. The bench skips with a warning if either is missing,
//! the same way `x86css.rs::bench_v4_tick` does for its v4 fixture.
//!
//! Methodology:
//!   - Parse the cabinet ONCE per `cargo bench` invocation (cold parse
//!     ~3.8s native per LOGBOOK; can't be amortized into Criterion's
//!     per-iteration budget without misrepresenting per-tick cost).
//!   - Restore the snapshot ONCE into a "primed state". The bench
//!     measures REPEATING THE SAME TICK from that primed state — each
//!     iteration restores the primed state into a working state via
//!     `State::restore(blob)`, then runs one tick. Both interpreter
//!     and handcoded benches use the same restore mechanism so the
//!     restore cost cancels in the ratio.
//!   - REP fastfwd is disabled (`CALCITE_REP_FASTFWD=0`) for both
//!     groups: the cabinet trips a contract panic at a REP MOVSW the
//!     bulk path doesn't yet handle (LOGBOOK 2026-04-28: known
//!     incomplete area), and disabling it gives an honest per-instruction
//!     CSS-tick cost. The handcoded baseline doesn't have a fastfwd
//!     path either; both sides simulate one CSS tick of work.
//!
//! Conformance: a separate `#[test]` (NOT a bench) asserts that
//! `handcoded_tick(state)` produces the same memory and state-vars as
//! `evaluator.tick(state)` from the same primed state. The speed ratio
//! is meaningless without conformance.

use std::path::PathBuf;

use criterion::{criterion_group, criterion_main, Criterion};

use calcite_core::eval::Evaluator;
use calcite_core::parser::parse_css;
use calcite_core::State;

/// Walk up from `CARGO_MANIFEST_DIR` (= crates/calcite-core/) looking for
/// the first ancestor with a sibling named `CSS-DOS/`. Handles both the
/// main calcite checkout and worktrees uniformly.
fn find_css_dos_dir() -> Option<PathBuf> {
    let mut cur = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for _ in 0..10 {
        if let Some(parent) = cur.parent() {
            let candidate = parent.join("CSS-DOS");
            if candidate.is_dir() {
                return Some(candidate);
            }
            cur = parent.to_path_buf();
        } else {
            return None;
        }
    }
    None
}

fn cabinet_path() -> PathBuf {
    if let Ok(p) = std::env::var("PHASE0_CABINET") {
        return PathBuf::from(p);
    }
    find_css_dos_dir()
        .map(|d| d.join("tmp/doom8088.css"))
        .unwrap_or_else(|| PathBuf::from("doom8088.css"))
}

fn snapshot_path() -> PathBuf {
    if let Ok(p) = std::env::var("PHASE0_SNAPSHOT") {
        return PathBuf::from(p);
    }
    find_css_dos_dir()
        .map(|d| d.join("tmp/doom-snapshots-pj/stage_loading.snap"))
        .unwrap_or_else(|| PathBuf::from("stage_loading.snap"))
}

/// Inputs the bench shares across iterations:
/// - `evaluator`: ready to tick (mutable; bench borrows mutably per iter).
/// - `primed_blob`: snapshot bytes representing the "input state" for
///    every iteration. Restoring from this blob is ~1 MB memcpy +
///    HashMap/state-var rebuild — fast and identical across iterations.
/// - `working_state`: a State pre-loaded with `parsed.properties` so it
///    has the right shape for `restore()` to splice values into.
struct BenchInputs {
    evaluator: Evaluator,
    primed_blob: Vec<u8>,
    working_state: State,
}

fn try_load() -> Option<BenchInputs> {
    let cab = cabinet_path();
    let snap = snapshot_path();
    if !cab.exists() {
        eprintln!("phase0: skipping — cabinet not found at {}", cab.display());
        eprintln!("  set PHASE0_CABINET=/path/to/doom8088.css to override");
        return None;
    }
    if !snap.exists() {
        eprintln!("phase0: skipping — snapshot not found at {}", snap.display());
        eprintln!("  set PHASE0_SNAPSHOT=/path/to/stage_loading.snap to override");
        eprintln!("  (capture with: bench-doom-stages.mjs --capture-snapshots=DIR)");
        return None;
    }

    // See file-level methodology comment for why fastfwd is disabled.
    if std::env::var("CALCITE_REP_FASTFWD").is_err() {
        // SAFETY: bench setup runs single-threaded before any tick fires.
        unsafe { std::env::set_var("CALCITE_REP_FASTFWD", "0"); }
        eprintln!("phase0: setting CALCITE_REP_FASTFWD=0 (Phase 0 methodology)");
    }

    let t0 = std::time::Instant::now();
    let css = std::fs::read_to_string(&cab).expect("read cabinet");
    eprintln!("phase0: read cabinet ({:.1} MB) in {:.2}s",
        css.len() as f64 / 1e6, t0.elapsed().as_secs_f64());

    let t1 = std::time::Instant::now();
    let parsed = parse_css(&css).expect("parse cabinet");
    eprintln!("phase0: parsed in {:.2}s", t1.elapsed().as_secs_f64());

    let t2 = std::time::Instant::now();
    let mut working_state = State::default();
    working_state.load_properties(&parsed.properties);
    let evaluator = Evaluator::from_parsed(&parsed);
    eprintln!("phase0: evaluator built in {:.2}s", t2.elapsed().as_secs_f64());

    let primed_blob = std::fs::read(&snap).expect("read snapshot");
    working_state.restore(&primed_blob).expect("restore snapshot to validate");
    eprintln!("phase0: snapshot validated, {} bytes", primed_blob.len());

    let cs = working_state.get_var("CS").unwrap_or(0);
    let ip = working_state.get_var("IP").unwrap_or(0);
    eprintln!("phase0: input state CS:IP = {:#06x}:{:#06x}", cs, ip);

    Some(BenchInputs { evaluator, primed_blob, working_state })
}

/// Phase 0 measurement bench. See file-level methodology comment.
fn bench_phase0(c: &mut Criterion) {
    let Some(mut inputs) = try_load() else { return };

    let mut group = c.benchmark_group("phase0");
    group.sample_size(50);

    // Three groups, all sharing the same primed state:
    // - `restore_only`: cost of `state.restore(blob)` alone. Subtract from
    //   the others to get real per-tick cost.
    // - `interpreter_tick`: restore + one interpreter tick.
    // - `handcoded_tick`: (Phase 0.3+) restore + one hand-derived tick.
    // Ratio = (interpreter - restore) / (handcoded - restore).

    group.bench_function("restore_only", |b| {
        b.iter(|| {
            inputs.working_state.restore(&inputs.primed_blob).unwrap();
        });
    });

    group.bench_function("interpreter_tick", |b| {
        b.iter(|| {
            inputs.working_state.restore(&inputs.primed_blob).unwrap();
            inputs.evaluator.tick(&mut inputs.working_state);
        });
    });

    // TODO Phase 0.3: handcoded_tick group goes here, restoring the
    // same primed state and calling the hand-derived function.

    group.finish();
}

criterion_group!(benches, bench_phase0);
criterion_main!(benches);
