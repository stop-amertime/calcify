//! Phase 3 spike: closure-backend correctness vs the v2 walker.
//!
//! `Backend::DagV2Closures` is a function-pointer-per-node dispatch
//! variant of the v2 walker. It must produce identical state to
//! `Backend::DagV2` on the same cabinet, since both lower from the
//! same `Dag`. These tests gate that.

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
fn closures_matches_walker_simple_calc() {
    let css = r#"
        @property --result { syntax: "<integer>"; inherits: true; initial-value: 0; }
        :root { --result: calc(40 + 2); }
    "#;
    let v2 = run_backend(css, Backend::DagV2, 1);
    let cl = run_backend(css, Backend::DagV2Closures, 1);
    assert_eq!(v2, 42, "v2 sanity");
    assert_eq!(cl, 42, "closures should match v2");
}

#[test]
fn closures_matches_walker_nested_calls() {
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
    let cl = run_backend(css, Backend::DagV2Closures, 1);
    assert_eq!(v2, 5006, "v2 sanity");
    assert_eq!(cl, 5006, "closures should match v2");
}

#[test]
fn closures_matches_walker_dispatch() {
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
    let cl = run_backend(css, Backend::DagV2Closures, 1);
    assert_eq!(v2, 300, "v2 sanity");
    assert_eq!(cl, 300, "closures should match v2");
}

#[test]
fn closures_matches_walker_bitwise() {
    // Reuse the v1 bit-decomposition shape so BitwiseOp lowers.
    let css = r#"
        @property --result { syntax: "<integer>"; inherits: true; initial-value: 0; }
        @function --and(--a <integer>, --b <integer>) returns <integer> {
          result: calc(
            mod(round(down, var(--a) / 1), 2) * mod(round(down, var(--b) / 1), 2) * 1
            + mod(round(down, var(--a) / 2), 2) * mod(round(down, var(--b) / 2), 2) * 2
            + mod(round(down, var(--a) / 4), 2) * mod(round(down, var(--b) / 4), 2) * 4
            + mod(round(down, var(--a) / 8), 2) * mod(round(down, var(--b) / 8), 2) * 8
            + mod(round(down, var(--a) / 16), 2) * mod(round(down, var(--b) / 16), 2) * 16
            + mod(round(down, var(--a) / 32), 2) * mod(round(down, var(--b) / 32), 2) * 32
            + mod(round(down, var(--a) / 64), 2) * mod(round(down, var(--b) / 64), 2) * 64
            + mod(round(down, var(--a) / 128), 2) * mod(round(down, var(--b) / 128), 2) * 128
            + mod(round(down, var(--a) / 256), 2) * mod(round(down, var(--b) / 256), 2) * 256
            + mod(round(down, var(--a) / 512), 2) * mod(round(down, var(--b) / 512), 2) * 512
            + mod(round(down, var(--a) / 1024), 2) * mod(round(down, var(--b) / 1024), 2) * 1024
            + mod(round(down, var(--a) / 2048), 2) * mod(round(down, var(--b) / 2048), 2) * 2048
            + mod(round(down, var(--a) / 4096), 2) * mod(round(down, var(--b) / 4096), 2) * 4096
            + mod(round(down, var(--a) / 8192), 2) * mod(round(down, var(--b) / 8192), 2) * 8192
            + mod(round(down, var(--a) / 16384), 2) * mod(round(down, var(--b) / 16384), 2) * 16384
            + mod(round(down, var(--a) / 32768), 2) * mod(round(down, var(--b) / 32768), 2) * 32768
          );
        }
        :root { --result: --and(49350, 13107); }
    "#;
    let v2 = run_backend(css, Backend::DagV2, 1);
    let cl = run_backend(css, Backend::DagV2Closures, 1);
    // 49350 & 13107 = 0xC0C6 & 0x3333 = 0x0002 = 2
    assert_eq!(v2, 2, "v2 sanity (49350 & 13107 = 2)");
    assert_eq!(cl, 2, "closures should match v2");
}
