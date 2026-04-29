//! v2-vs-v1 differential smoke test on a real cabinet.
//!
//! v1 is *not* the contract (Chrome is — see Phase 0.5 conformance),
//! but on a cabinet where v1 is known to match Chrome, v1 is a fast
//! oracle for v2 — diffs localise where v2 diverges before the
//! divergence cascades through hundreds of ticks.
//!
//! Cabinet pick: CSS-DOS's `hello-text.css` (~2.8 MB) — the smallest
//! pre-built cabinet that exercises broadcast writes (above the 1 MB
//! fast-path threshold) and `@function` calls. Located via env var
//! `CSS_DOS_CABINET` or the default sibling-repo path.
//!
//! Cabinet-running tests are `#[ignore]` by default — they parse ~3
//! MB of CSS. Run with:
//!   cargo test -p calcite-core --test v2_cabinet_differential -- --ignored
//!
//! The two synthetic tests at the top of the file (param dispatch,
//! register-dispatch shape) run in normal `cargo test` and pin the
//! shapes v2 must handle correctly without delegation surprises.

use std::path::PathBuf;

use calcite_core::eval::{Backend, Evaluator};
use calcite_core::parser::parse_css;
use calcite_core::State;

// ----- Synthetic regression tests (always run) ---------------------

/// Function call with parameter dispatch: returns one of the args
/// based on a key parameter. v2's FuncCall delegates to v1's
/// eval_function_call which uses dispatch_tables; this exercises the
/// path's correctness without the cabinet's complications.
#[test]
fn v2_func_dispatch_returns_param() {
    let css = r#"
        @property --r { syntax: "<integer>"; inherits: true; initial-value: 0; }
        @property --AL { syntax: "<integer>"; inherits: true; initial-value: 72; }
        @property --CL { syntax: "<integer>"; inherits: true; initial-value: 99; }
        @property --result { syntax: "<integer>"; inherits: true; initial-value: 0; }
        @function --getReg(--idx <integer>, --al <integer>, --cl <integer>) returns <integer> {
          result: if(
            style(--idx: 0): var(--al);
            style(--idx: 1): var(--cl);
            else: 0);
        }
        :root {
          --result: --getReg(var(--r), var(--AL), var(--CL));
        }
    "#;
    let parsed = parse_css(css).expect("parse");
    let mut s2 = State::default();
    s2.load_properties(&parsed.properties);
    let mut e2 = Evaluator::from_parsed(&parsed);
    e2.set_backend(Backend::DagV2);
    e2.tick(&mut s2);
    assert_eq!(s2.get_var("result").unwrap(), 72);
}

/// Register-dispatch shape: outer override + inner opcode dispatch +
/// else fallthrough to var(--__1self). The shape kiln/emit-css.mjs
/// emits for every CSS-DOS register.
#[test]
fn v2_register_dispatch_shape() {
    let css = r#"
        @property --opcode { syntax: "<integer>"; inherits: true; initial-value: 8; }
        @property --AX { syntax: "<integer>"; inherits: true; initial-value: 72; }
        @property --tf { syntax: "<integer>"; inherits: true; initial-value: 0; }
        @property --irq { syntax: "<integer>"; inherits: true; initial-value: 0; }
        :root {
            --opcode: 8;
            --AX: if(
                style(--tf: 1): var(--__1AX);
                style(--irq: 1): var(--__1AX);
                else: if(
                    style(--opcode: 137): 999;
                    style(--opcode: 4): 100;
                    else: var(--__1AX)));
        }
    "#;
    let parsed = parse_css(css).expect("parse");
    let mut s1 = State::default();
    s1.load_properties(&parsed.properties);
    let mut e1 = Evaluator::from_parsed(&parsed);
    e1.set_backend(Backend::Bytecode);

    let mut s2 = State::default();
    s2.load_properties(&parsed.properties);
    let mut e2 = Evaluator::from_parsed(&parsed);
    e2.set_backend(Backend::DagV2);

    for tick in 1..=5 {
        e1.tick(&mut s1);
        e2.tick(&mut s2);
        let v1 = s1.get_var("AX").unwrap();
        let v2 = s2.get_var("AX").unwrap();
        assert_eq!(v1, v2, "tick {tick}: v1={v1} v2={v2}");
    }
}

// ----- Cabinet differential (gated, large parse) -------------------

fn cabinet_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CSS_DOS_CABINET") {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
    }
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    let candidate = p
        .parent()? // .claude/worktrees
        .parent()? // .claude
        .parent()? // calcite
        .parent()? // src
        .join("CSS-DOS")
        .join("tests")
        .join("harness")
        .join("cache")
        .join("hello-text.css");
    candidate.is_file().then_some(candidate)
}

/// Tick-by-tick diff. Returns the first tick at which v2 diverges
/// from v1, with the var/memory differences at that tick. Useful for
/// localising regressions during Phase 2 work.
fn first_divergent_tick(
    css: &str,
    max_ticks: usize,
) -> Option<(usize, Vec<(usize, i32, i32)>, Vec<(usize, u8, u8)>)> {
    let parsed = parse_css(css).expect("parse cabinet");
    let mut s1 = State::default();
    s1.load_properties(&parsed.properties);
    let mut e1 = Evaluator::from_parsed(&parsed);
    e1.wire_state_for_packed_memory(&mut s1);
    e1.wire_state_for_disk_window(&mut s1);
    e1.set_backend(Backend::Bytecode);

    let mut s2 = State::default();
    s2.load_properties(&parsed.properties);
    let mut e2 = Evaluator::from_parsed(&parsed);
    e2.wire_state_for_packed_memory(&mut s2);
    e2.wire_state_for_disk_window(&mut s2);
    e2.set_backend(Backend::DagV2);

    for tick in 1..=max_ticks {
        e1.tick(&mut s1);
        e2.tick(&mut s2);
        let mut var_diffs: Vec<(usize, i32, i32)> = Vec::new();
        for (i, (a, b)) in s1.state_vars.iter().zip(s2.state_vars.iter()).enumerate() {
            if a != b {
                var_diffs.push((i, *a, *b));
            }
        }
        let mut mem_diffs: Vec<(usize, u8, u8)> = Vec::new();
        let mem_len = s1.memory.len().min(s2.memory.len());
        for i in 0..mem_len {
            if s1.memory[i] != s2.memory[i] {
                mem_diffs.push((i, s1.memory[i], s2.memory[i]));
                if mem_diffs.len() >= 16 {
                    break;
                }
            }
        }
        if !var_diffs.is_empty() || !mem_diffs.is_empty() {
            return Some((tick, var_diffs, mem_diffs));
        }
    }
    None
}

/// Find the first tick at which v2 diverges from v1 on the cabinet.
/// Currently divergence appears around tick 6 (function-call
/// delegation has subtle differences from v1's bytecode path on
/// some shapes). Phase 2 inlining will eliminate the delegation.
#[test]
#[ignore]
fn v2_first_divergence() {
    let Some(path) = cabinet_path() else {
        eprintln!("no cabinet, skipping");
        return;
    };
    let css = std::fs::read_to_string(&path).expect("read");
    let max_ticks: usize = std::env::var("DIFF_TICKS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    match first_divergent_tick(&css, max_ticks) {
        None => eprintln!("v1 and v2 identical for {max_ticks} ticks"),
        Some((tick, var_diffs, mem_diffs)) => {
            let parsed = parse_css(&css).expect("re-parse");
            let mut s = State::default();
            s.load_properties(&parsed.properties);
            eprintln!("first divergence at tick {tick}");
            for (i, a, b) in var_diffs.iter().take(20) {
                let name = s.state_var_names.get(*i).cloned().unwrap_or_default();
                eprintln!("  var[{i}] (--{name}): v1={a} v2={b}");
            }
            for (i, a, b) in mem_diffs.iter().take(8) {
                eprintln!("  mem[0x{i:x}]: v1={a} v2={b}");
            }
        }
    }
}
