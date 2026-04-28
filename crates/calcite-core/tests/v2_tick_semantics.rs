//! Pin v1's tick-evaluation semantics so the v2 DAG walker has a spec
//! to match. Each test exercises one corner case and asserts what v1
//! observably does — not what we *think* it should do.
//!
//! The v2 walker (Phase 1, see `docs/v2-rewrite-design.md`) must
//! produce bit-identical results to v1 on every test in this file. If a
//! test here changes meaning, we have changed the contract.
//!
//! Corner cases covered:
//!   1. Self-reference current-tick: `--AX: calc(var(--AX) + 1)` —
//!      reads prior tick's --AX and increments.
//!   2. Forward dependency: `--AX: var(--CX)` where --CX is declared
//!      after --AX. Topological sort reorders.
//!   3. Forward dependency via --__1: same shape but with --__1CX —
//!      reads committed (prior-tick) state, no reordering needed.
//!   4. Same-tick read after write: `--AX: 7; --CX: var(--AX)` —
//!      --CX sees the new --AX (per-tick property cache).
//!   5. --__1 read after write: `--AX: 7; --CX: var(--__1AX)` —
//!      --CX sees prior tick's --AX (cache bypassed).
//!   6. Memory write then current-tick memory read in same tick.
//!   7. Memory write then --__1 memory read in same tick.
//!   8. Two-tick sequence: prior-tick reads return last tick's value,
//!      not initial-value.
//!   9. Buffer-copy assignment (--__1mN: var(--__2mN)) is skipped, no
//!      effect on state.
//!  10. Multi-tick self-update via --__1: `--AX: calc(var(--__1AX)+1)`
//!      — same as #1 but explicit.

use calcite_core::eval::Evaluator;
use calcite_core::parser::parse_css;
use calcite_core::State;
use std::sync::Once;

// v1's rep_fast_forward path panics on cabinets without --opcode (any
// tiny test cabinet). Disable it for these tests — they're exercising
// CSS-evaluation semantics, not REP string-op behaviour.
static REP_FASTFWD_OFF: Once = Once::new();
fn disable_rep_fastfwd() {
    REP_FASTFWD_OFF.call_once(|| {
        // SAFETY: tests run before any Evaluator is constructed, and
        // rep_fastfwd_enabled() caches via OnceLock on first read.
        // Setting the env before any tick is invariant-safe.
        unsafe { std::env::set_var("CALCITE_REP_FASTFWD", "0") };
    });
}

const STD_PROPS: &str = r#"
@property --AX { syntax: "<integer>"; inherits: true; initial-value: 0; }
@property --CX { syntax: "<integer>"; inherits: true; initial-value: 0; }
@property --DX { syntax: "<integer>"; inherits: true; initial-value: 0; }
@property --BX { syntax: "<integer>"; inherits: true; initial-value: 0; }
@property --m0  { syntax: "<integer>"; inherits: true; initial-value: 0; }
@property --m1  { syntax: "<integer>"; inherits: true; initial-value: 0; }
@property --m2  { syntax: "<integer>"; inherits: true; initial-value: 0; }
"#;

fn setup(body_css: &str) -> (Evaluator, State) {
    disable_rep_fastfwd();
    let full = format!("{STD_PROPS}\n{body_css}");
    let parsed = parse_css(&full).expect("parse");
    let mut state = State::default();
    state.load_properties(&parsed.properties);
    let evaluator = Evaluator::from_parsed(&parsed);
    (evaluator, state)
}

// 1. Self-reference current-tick: increments per tick from prior value.
#[test]
fn case01_self_reference_current_tick() {
    let (mut e, mut s) = setup(".cpu { --AX: calc(var(--AX) + 1); }");
    e.tick(&mut s);
    assert_eq!(s.get_var("AX").unwrap(), 1, "tick 1 sees AX=0 (initial)");
    e.tick(&mut s);
    assert_eq!(s.get_var("AX").unwrap(), 2, "tick 2 sees AX=1");
    e.tick(&mut s);
    assert_eq!(s.get_var("AX").unwrap(), 3);
}

// 2. Forward dependency on a current-tick read: topo sort handles it.
#[test]
fn case02_forward_dep_current_tick() {
    // --AX: var(--CX); --CX: 42
    // Declared in this order, but --AX depends on --CX (current-tick),
    // so topo sort runs --CX first. --AX should land at 42 in tick 1.
    let (mut e, mut s) = setup(
        ".cpu { --AX: var(--CX); --CX: 42; }",
    );
    e.tick(&mut s);
    assert_eq!(s.get_var("CX").unwrap(), 42);
    assert_eq!(s.get_var("AX").unwrap(), 42, "topo sort moves --CX before --AX");
}

// 3. Forward dependency via --__1: no ordering constraint, sees prior tick.
#[test]
fn case03_forward_dep_prev_tick() {
    let (mut e, mut s) = setup(
        ".cpu { --AX: var(--__1CX); --CX: 42; }",
    );
    // Tick 1: --__1CX is the prior-tick value (initial=0); --CX is set to 42.
    e.tick(&mut s);
    assert_eq!(s.get_var("CX").unwrap(), 42);
    assert_eq!(s.get_var("AX").unwrap(), 0, "tick 1: --__1CX is initial 0");
    // Tick 2: --__1CX now reads last tick's CX=42.
    e.tick(&mut s);
    assert_eq!(s.get_var("AX").unwrap(), 42);
}

// 4. Same-tick read after write: per-tick cache returns the new value.
#[test]
fn case04_same_tick_cache() {
    let (mut e, mut s) = setup(
        ".cpu { --AX: 7; --CX: calc(var(--AX) + 1); }",
    );
    e.tick(&mut s);
    assert_eq!(s.get_var("AX").unwrap(), 7);
    assert_eq!(s.get_var("CX").unwrap(), 8, "--CX sees this-tick --AX=7");
}

// 5. --__1 read explicitly bypasses the cache.
#[test]
fn case05_prev_tick_bypasses_cache() {
    let (mut e, mut s) = setup(
        ".cpu { --AX: 7; --CX: var(--__1AX); }",
    );
    // Tick 1: --__1AX is initial 0, --AX gets 7.
    e.tick(&mut s);
    assert_eq!(s.get_var("AX").unwrap(), 7);
    assert_eq!(s.get_var("CX").unwrap(), 0, "tick 1: --__1AX is initial 0");
    // Tick 2: --__1AX reads last tick's AX=7.
    e.tick(&mut s);
    assert_eq!(s.get_var("CX").unwrap(), 7);
}

// 6. Memory write then current-tick memory read in same tick.
//    --m0: 99; --AX: var(--m0) — does --AX see 99 or initial 0?
#[test]
fn case06_mem_write_then_current_read() {
    let (mut e, mut s) = setup(
        ".cpu { --m0: 99; --AX: var(--m0); }",
    );
    e.tick(&mut s);
    assert_eq!(s.memory[0], 99);
    // This pins v1's behaviour, whatever it is. The test will tell us.
    let observed_ax = s.get_var("AX").unwrap();
    eprintln!("case06: --AX = {observed_ax} (memory write same-tick read)");
    // We expect 99 (cache-driven), but pin it as documented.
    assert_eq!(observed_ax, 99, "memory write visible to same-tick current-tick read");
}

// 7. Memory write then --__1 memory read in same tick.
#[test]
fn case07_mem_write_then_prev_read() {
    let (mut e, mut s) = setup(
        ".cpu { --m0: 99; --AX: var(--__1m0); }",
    );
    e.tick(&mut s);
    assert_eq!(s.memory[0], 99);
    let observed = s.get_var("AX").unwrap();
    eprintln!("case07: --AX = {observed} (memory write same-tick __1 read)");
    // --__1m0 should bypass cache and read state. The question is *when*
    // the memory cell is committed: if --m0 is written before --AX runs,
    // --AX sees 99; if after, it sees 0.
    // Pin whatever v1 does; v2 must match.
    assert_eq!(
        observed, 0,
        "current expectation: --__1 reads pre-write state; assert pins observed v1 behaviour"
    );
}

// 8. Two-tick sequence pin.
#[test]
fn case08_two_tick_sequence() {
    let (mut e, mut s) = setup(
        ".cpu {
            --AX: calc(var(--__1AX) + 10);
            --CX: var(--__1AX);
        }",
    );
    e.tick(&mut s); // AX = 0+10 = 10; CX = 0
    assert_eq!(s.get_var("AX").unwrap(), 10);
    assert_eq!(s.get_var("CX").unwrap(), 0);
    e.tick(&mut s); // AX = 10+10 = 20; CX = 10
    assert_eq!(s.get_var("AX").unwrap(), 20);
    assert_eq!(s.get_var("CX").unwrap(), 10);
}

// 9. Buffer-copy assignment is skipped — has no effect.
#[test]
fn case09_buffer_copy_skipped() {
    // This shape is what kiln emits as the double-buffer copy; v1 skips
    // these. The expected behaviour: writing --__1m0 does NOT change
    // --m0's committed state — the assignment is a no-op.
    let (mut e, mut s) = setup(
        ".cpu {
            --m0: 50;
            --__1m0: 999;
        }",
    );
    e.tick(&mut s);
    assert_eq!(s.memory[0], 50, "regular write lands");
    // Reading --m0 (current) should give us 50 from this tick. The buffer-
    // copy assignment to --__1m0 should be a no-op — no separate slot was
    // updated and re-reading via --__1m0 next tick reads m0's prior value.
    e.tick(&mut s);
    assert_eq!(s.memory[0], 50, "still 50 after second tick (no other writer)");
}

// 10. Multi-tick self-update with explicit --__1: behaviour matches case 1.
#[test]
fn case10_explicit_prev_self_update() {
    let (mut e, mut s) = setup(".cpu { --AX: calc(var(--__1AX) + 1); }");
    for expected in 1..=5 {
        e.tick(&mut s);
        assert_eq!(s.get_var("AX").unwrap(), expected);
    }
}

// 11. Diamond dependency: --DX depends on both --AX and --CX (both current).
#[test]
fn case11_diamond_topology() {
    let (mut e, mut s) = setup(
        ".cpu {
            --DX: calc(var(--AX) + var(--CX));
            --AX: 10;
            --CX: 20;
        }",
    );
    e.tick(&mut s);
    assert_eq!(s.get_var("AX").unwrap(), 10);
    assert_eq!(s.get_var("CX").unwrap(), 20);
    assert_eq!(s.get_var("DX").unwrap(), 30, "topo sort handles diamond");
}

// 12. Read of --foo where --foo is never assigned: should see initial value.
#[test]
fn case12_unassigned_var_read() {
    // --AX is declared with initial-value 0, but never assigned in .cpu.
    // --CX reads it. Expected: --CX = 0.
    let (mut e, mut s) = setup(".cpu { --CX: var(--AX); }");
    e.tick(&mut s);
    assert_eq!(s.get_var("AX").unwrap(), 0);
    assert_eq!(s.get_var("CX").unwrap(), 0);
}
