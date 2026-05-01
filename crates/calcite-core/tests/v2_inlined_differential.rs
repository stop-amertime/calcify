//! Phase 3 (c+): inlined-codegen correctness vs the v2 walker.
//!
//! `Backend::DagV2Inlined` emits per-terminal straight-line ops for
//! cones it covers (Lit, LoadVar, arity-2 Add/Sub/Mul/Div/Mod) and
//! falls back to the walker for everything else. Both code paths must
//! produce identical state to `Backend::DagV2` on the same cabinet.
//!
//! Tests cover both the emit path (simple_calc) and the walker
//! fallback path (nested_calls, dispatch, bitwise) — the cone-emitter
//! bails on Call / If / BitwiseOp, so those terminals route through
//! the walker.

use calcite_core::eval::{Backend, Evaluator};
use calcite_core::parser::parse_css;
use calcite_core::State;

fn run_backend(css: &str, backend: Backend, ticks: usize) -> i64 {
    let parsed = parse_css(css).expect("parse");
    let mut state = State::default();
    state.load_properties(&parsed.properties);
    let mut ev = Evaluator::from_parsed(&parsed);
    ev.set_backend(backend);
    for _ in 0..ticks {
        ev.tick(&mut state);
    }
    state.get_var("result").unwrap_or(0) as i64
}

#[test]
fn inlined_matches_walker_simple_calc() {
    // Cone covered by the prototype: Lit + Add. Emit path.
    let css = r#"
        @property --result { syntax: "<integer>"; inherits: true; initial-value: 0; }
        :root { --result: calc(40 + 2); }
    "#;
    let v2 = run_backend(css, Backend::DagV2, 1);
    let inl = run_backend(css, Backend::DagV2Inlined, 1);
    assert_eq!(v2, 42, "v2 sanity");
    assert_eq!(inl, 42, "inlined should match v2");
}

#[test]
fn inlined_matches_walker_arith_chain() {
    // Larger arith cone over LoadVar + literals. Cone covered.
    let css = r#"
        @property --x { syntax: "<integer>"; inherits: true; initial-value: 7; }
        @property --y { syntax: "<integer>"; inherits: true; initial-value: 3; }
        @property --result { syntax: "<integer>"; inherits: true; initial-value: 0; }
        :root {
          --result: calc((var(--x) + 1) * (var(--y) - 2) + var(--x) * var(--y));
        }
    "#;
    let v2 = run_backend(css, Backend::DagV2, 1);
    let inl = run_backend(css, Backend::DagV2Inlined, 1);
    // (7+1)*(3-2) + 7*3 = 8 + 21 = 29
    assert_eq!(v2, 29, "v2 sanity");
    assert_eq!(inl, 29, "inlined should match v2");
}

#[test]
fn inlined_matches_walker_div_mod() {
    let css = r#"
        @property --result { syntax: "<integer>"; inherits: true; initial-value: 0; }
        :root { --result: calc(mod(100, 7) * 10 + (100 / 7)); }
    "#;
    let v2 = run_backend(css, Backend::DagV2, 1);
    let inl = run_backend(css, Backend::DagV2Inlined, 1);
    // 100 mod 7 = 2; 100 / 7 = 14 (truncated as i32 from f64 14.28); 2*10 + 14 = 34
    assert_eq!(v2, inl, "inlined should match v2");
    assert_eq!(v2, 34, "v2 sanity");
}

#[test]
fn inlined_matches_walker_nested_calls() {
    // Cone contains Call/Param — emitter bails, walker runs.
    let css = r#"
        @property --result { syntax: "<integer>"; inherits: true; initial-value: -1; }
        @function --inner(--x <integer>) returns <integer> {
          result: var(--x);
        }
        @function --outer(--at <integer>) returns <integer> {
          result: calc(--inner(var(--at)) * 1000 + --inner(calc(var(--at) + 1)));
        }
        :root { --result: --outer(5); }
    "#;
    let v2 = run_backend(css, Backend::DagV2, 1);
    let inl = run_backend(css, Backend::DagV2Inlined, 1);
    assert_eq!(v2, 5006, "v2 sanity");
    assert_eq!(inl, 5006, "inlined should match v2 (walker fallback)");
}

#[test]
fn inlined_matches_walker_dispatch() {
    // Cone contains Switch/If — emitter bails, walker runs.
    let css = r#"
        @property --k { syntax: "<integer>"; inherits: true; initial-value: 2; }
        @property --result { syntax: "<integer>"; inherits: true; initial-value: -1; }
        :root {
          --result: if(
            style(--k: 0): 100;
            style(--k: 1): 200;
            style(--k: 2): 300;
            style(--k: 3): 400;
          else: 999);
        }
    "#;
    let v2 = run_backend(css, Backend::DagV2, 1);
    let inl = run_backend(css, Backend::DagV2Inlined, 1);
    assert_eq!(v2, 300, "v2 sanity");
    assert_eq!(inl, 300, "inlined should match v2 (walker fallback)");
}

#[test]
fn inlined_matches_walker_mixed_cabinet() {
    // Two terminals: one cone is fully covered (--a), the other has a
    // Call (--b). Each terminal is independently emit-or-fall-back.
    let css = r#"
        @property --x { syntax: "<integer>"; inherits: true; initial-value: 4; }
        @property --a { syntax: "<integer>"; inherits: true; initial-value: 0; }
        @property --result { syntax: "<integer>"; inherits: true; initial-value: 0; }
        @function --double(--n <integer>) returns <integer> {
          result: calc(var(--n) * 2);
        }
        :root {
          --a: calc(var(--x) + 10);
          --result: --double(var(--x));
        }
    "#;
    let v2 = run_backend(css, Backend::DagV2, 1);
    let inl = run_backend(css, Backend::DagV2Inlined, 1);
    // --result = double(4) = 8
    assert_eq!(v2, 8, "v2 sanity");
    assert_eq!(inl, 8, "inlined should match v2");
}
