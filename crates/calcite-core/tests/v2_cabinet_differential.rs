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

/// Two nested function calls where the inner is called with
/// `calc(--at + 1)`. Even simpler than the read2/readMem case but
/// the same Param-flowing-through-calc-into-nested-call shape.
#[test]
fn v2_nested_call_calc_param_plus_one() {
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
    let parsed = parse_css(css).expect("parse");

    let mut s1 = State::default();
    s1.load_properties(&parsed.properties);
    let mut e1 = Evaluator::from_parsed(&parsed);
    e1.set_backend(Backend::Bytecode);
    e1.tick(&mut s1);
    let mut s2 = State::default();
    s2.load_properties(&parsed.properties);
    let mut e2 = Evaluator::from_parsed(&parsed);
    e2.set_backend(Backend::DagV2);
    e2.tick(&mut s2);

    let r1 = s1.get_var("result").unwrap();
    let r2 = s2.get_var("result").unwrap();
    eprintln!("v1 result={r1} v2 result={r2}");
    // Expect 5*1000 + 6 = 5006
    assert_eq!(r1, 5006, "v1 sanity");
    assert_eq!(r2, 5006, "v2 should match v1");
}

/// Reproduce nested function calls returning the right value for a
/// computed arg. Mirrors `--read2(calc(var(--q1) * 4 + 2))` in the
/// cabinet — `read2` calls `readMem` twice, each result is added.
#[test]
fn v2_nested_function_with_calc_arg() {
    let css = r#"
        @property --mc33 { syntax: "<integer>"; inherits: true; initial-value: 61440; }
        @property --q1 { syntax: "<integer>"; inherits: true; initial-value: 16; }
        @property --result { syntax: "<integer>"; inherits: true; initial-value: -1; }
        @function --readMem(--at <integer>) returns <integer> {
          result: if(
            style(--at: 64): 0;
            style(--at: 65): 0;
            style(--at: 66): mod(var(--__1mc33), 256);
            style(--at: 67): round(down, var(--__1mc33) / 256);
          else: 0);
        }
        @function --read2(--at <integer>) returns <integer> {
          result: calc(--readMem(var(--at)) + --readMem(calc(var(--at) + 1)) * 256);
        }
        :root {
          --result: --read2(calc(var(--q1) * 4 + 2));
        }
    "#;
    let parsed = parse_css(css).expect("parse");

    let mut s1 = State::default();
    s1.load_properties(&parsed.properties);
    let mut e1 = Evaluator::from_parsed(&parsed);
    e1.set_backend(Backend::Bytecode);
    e1.tick(&mut s1);

    let mut s2 = State::default();
    s2.load_properties(&parsed.properties);
    let mut e2 = Evaluator::from_parsed(&parsed);
    e2.set_backend(Backend::DagV2);
    e2.tick(&mut s2);

    let r1 = s1.get_var("result").unwrap();
    let r2 = s2.get_var("result").unwrap();
    eprintln!("v1 result={r1} v2 result={r2}");
    assert_eq!(r1, 61440, "v1 sanity");
    assert_eq!(r2, 61440, "v2 should match v1");
}

/// Function with dispatch-table-shape if() on a parameter (the
/// param is the LHS of every style() test). Mirrors the cabinet's
/// `--readMem(--at)` dispatch. The recogniser must build a Switch
/// whose `key` references the function's `Param(0)` — without
/// substituting in the call-site arg. v1's bytecode lowering had
/// equivalent path; v2 must match.
#[test]
fn v2_function_dispatch_table_param_key() {
    let css = r#"
        @property --r { syntax: "<integer>"; inherits: true; initial-value: 0; }
        @property --result { syntax: "<integer>"; inherits: true; initial-value: -1; }
        @function --readMem(--at <integer>) returns <integer> {
          result: if(
            style(--at: 0): 100;
            style(--at: 1): 101;
            style(--at: 2): 102;
            style(--at: 3): 103;
            style(--at: 4): 104;
            style(--at: 5): 105;
          else: -1);
        }
        :root {
          --r: 3;
          --result: --readMem(var(--r));
        }
    "#;
    let parsed = parse_css(css).expect("parse");

    let mut s1 = State::default();
    s1.load_properties(&parsed.properties);
    let mut e1 = Evaluator::from_parsed(&parsed);
    e1.set_backend(Backend::Bytecode);
    e1.tick(&mut s1);

    let mut s2 = State::default();
    s2.load_properties(&parsed.properties);
    let mut e2 = Evaluator::from_parsed(&parsed);
    e2.set_backend(Backend::DagV2);
    e2.tick(&mut s2);

    let r1 = s1.get_var("result").unwrap();
    let r2 = s2.get_var("result").unwrap();
    assert_eq!(r1, 103, "v1 sanity: readMem(3) should be 103");
    assert_eq!(r2, 103, "v2 readMem(3) wrong: got {}", r2);
}

/// Reproduce read2 returning 61440 from a packed cell with
/// initial-value 61440.
#[test]
fn v2_read2_packed_cell_initial_value() {
    // `--mc33` is a packed-cell state-var holding bytes mem[66] and mem[67].
    // initial-value 61440 = 0x_F000 → mem[66]=0x00, mem[67]=0xF0.
    // read2(66) should return 0 + 0xF0*256 = 61440.
    //
    // Note: just declaring `--mc33: <integer>; initial-value: 61440;` isn't
    // enough — calcite's packed-cell layout is set up via
    // `wire_state_for_packed_memory`, which expects a specific
    // packed_cell_table. Without that, mc33 is a plain state var, so
    // reading mem[66] still returns 0. Skip if wiring isn't exposed.
    let css = r#"
        @property --mc33 { syntax: "<integer>"; inherits: true; initial-value: 61440; }
        @property --result { syntax: "<integer>"; inherits: true; initial-value: -1; }
        @function --readMem(--at <integer>) returns <integer> {
          result: if(
            style(--at: 66): mod(var(--__1mc33), 256);
            style(--at: 67): round(down, var(--__1mc33) / 256);
          else: 0);
        }
        @function --read2(--at <integer>) returns <integer> {
          result: calc(--readMem(var(--at)) + --readMem(calc(var(--at) + 1)) * 256);
        }
        :root { --result: --read2(66); }
    "#;
    let parsed = parse_css(css).expect("parse");

    let mut s1 = State::default();
    s1.load_properties(&parsed.properties);
    let mut e1 = Evaluator::from_parsed(&parsed);
    e1.set_backend(Backend::Bytecode);
    e1.tick(&mut s1);

    let mut s2 = State::default();
    s2.load_properties(&parsed.properties);
    let mut e2 = Evaluator::from_parsed(&parsed);
    e2.set_backend(Backend::DagV2);
    e2.tick(&mut s2);

    let r1 = s1.get_var("result").unwrap();
    let r2 = s2.get_var("result").unwrap();
    eprintln!("v1 result={r1} v2 result={r2}");
    eprintln!("v1 mc33={:?} v2 mc33={:?}", s1.get_var("mc33"), s2.get_var("mc33"));
    // Don't assert; just observe.
}

/// Reproduce the cabinet's `--bit` shape: nested function calls with
/// arithmetic on params, used as the LHS of a parent's style() test.
/// This was the suspected root of the tick-6 cabinet divergence.
#[test]
fn v2_bit_function_zero_for_low_bit() {
    let css = r#"
        @property --flags { syntax: "<integer>"; inherits: true; initial-value: 6; }
        @property --result { syntax: "<integer>"; inherits: true; initial-value: -1; }
        @function --rightShift(--a <integer>, --b <integer>) returns <integer> {
          result: round(down, var(--a) / pow(2, var(--b)));
        }
        @function --bit(--val <integer>, --idx <integer>) returns <integer> {
          result: mod(--rightShift(var(--val), var(--idx)), 2);
        }
        :root {
          --result: --bit(var(--__1flags), 9);
        }
    "#;
    let parsed = parse_css(css).expect("parse");

    let mut s1 = State::default();
    s1.load_properties(&parsed.properties);
    let mut e1 = Evaluator::from_parsed(&parsed);
    e1.set_backend(Backend::Bytecode);
    e1.tick(&mut s1);
    let r1 = s1.get_var("result").unwrap();

    let mut s2 = State::default();
    s2.load_properties(&parsed.properties);
    let mut e2 = Evaluator::from_parsed(&parsed);
    e2.set_backend(Backend::DagV2);
    e2.tick(&mut s2);
    let r2 = s2.get_var("result").unwrap();

    assert_eq!(r1, 0, "v1 sanity: bit(6, 9) should be 0");
    assert_eq!(r2, 0, "v2 bit(6, 9) wrong: got {}", r2);
}

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

/// Diagnostic: dump named-property values at start of tick T from
/// both backends side-by-side. Useful when pinpointing which input
/// to a divergent assignment is wrong. Run with:
///   DIAG_TICK=6 cargo test -p calcite-core --test v2_cabinet_differential v2_diagnose -- --ignored --nocapture
#[test]
#[ignore]
fn v2_diagnose() {
    let Some(path) = cabinet_path() else {
        eprintln!("no cabinet, skipping");
        return;
    };
    let css = std::fs::read_to_string(&path).expect("read");
    let target_tick: usize = std::env::var("DIAG_TICK")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6);
    let parsed = parse_css(&css).expect("parse cabinet");
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

    // Run target_tick - 1 ticks: state at this point reflects what
    // tick `target_tick` will read for `--__1*` (Prev) reads.
    for _ in 1..target_tick {
        e1.tick(&mut s1);
        e2.tick(&mut s2);
    }
    eprintln!("=== mem bytes after tick {} ===", target_tick - 1);
    for addr in [0, 1, 2, 3, 4, 5, 6, 7, 32, 33, 34, 35, 64, 65, 66, 67] {
        let m1 = s1.memory.get(addr).copied().unwrap_or(0);
        let m2 = s2.memory.get(addr).copied().unwrap_or(0);
        // Read via state.read_mem too (which uses packed cells path)
        let r1 = s1.read_mem(addr as i32);
        let r2 = s2.read_mem(addr as i32);
        let mark = if m1 == m2 && r1 == r2 { "  " } else { "!!" };
        eprintln!("  {} mem[{addr}] flat: v1={m1} v2={m2}  read_mem: v1={r1} v2={r2}", mark);
    }

    let names = [
        "AX", "CX", "DX", "BX", "SP", "BP", "SI", "DI",
        "CS", "DS", "ES", "SS", "IP", "flags",
        "_tf", "_irqActive", "picVector", "opcode",
        "mc0", "mc1", "mc17", "mc18", "mc33", "mc761", "mc762", "mc763",
        "_slot0Live", "_slot1Live", "_slot2Live",
        "memAddr0", "memAddr1", "memAddr2", "memVal0", "memVal1", "memVal2",
        "_writeWidth", "tickCount",
    ];
    eprintln!("=== state after tick {} (= start of tick {}) ===", target_tick - 1, target_tick);
    for n in names {
        let v1 = s1.get_var(n).unwrap_or(i32::MIN);
        let v2 = s2.get_var(n).unwrap_or(i32::MIN);
        let mark = if v1 == v2 { "  " } else { "!!" };
        eprintln!("  {} --{:<14} v1={:>10} v2={:>10}", mark, n, v1, v2);
    }
    // Run the target tick.
    e1.tick(&mut s1);
    e2.tick(&mut s2);
    eprintln!("=== state after tick {} ===", target_tick);
    for n in names {
        let v1 = s1.get_var(n).unwrap_or(i32::MIN);
        let v2 = s2.get_var(n).unwrap_or(i32::MIN);
        let mark = if v1 == v2 { "  " } else { "!!" };
        eprintln!("  {} --{:<14} v1={:>10} v2={:>10}", mark, n, v1, v2);
    }
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
