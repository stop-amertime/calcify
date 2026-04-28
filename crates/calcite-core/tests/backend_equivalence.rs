//! Phase 1 acceptance gate: the DAG walker and the bytecode interpreter
//! produce bit-identical state when run against a representative cabinet
//! from a common starting state for N ticks.
//!
//! "Bit-identical" means: every state_var, every memory byte, every
//! extended-map entry, every string property is equal between the two
//! runs. Frame counter is also compared. If any of these diverge,
//! Phase 1 has failed its gate — the DAG walker must reproduce the
//! incumbent backend exactly before Phase 2 starts rewriting.
//!
//! The fixture is `web/demo.css` (~730 KB), the only real cabinet in
//! the worktree. The test runs a small number of ticks (enough to
//! exercise dispatch chains, broadcast writes, and bulk memory ops in
//! a typical x86 emulator boot path) and panics on first divergence.
//!
//! If `web/demo.css` is missing (e.g. fresh checkout, stripped tarball),
//! the test no-ops with a printed note. Future work: ship a dedicated
//! small-cabinet fixture so the gate is hermetic.

use std::path::PathBuf;

use calcite_core::eval::{Backend, Evaluator};
use calcite_core::parser::parse_css;
use calcite_core::State;

const TICKS: u32 = 200;

fn worktree_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

fn run_n_ticks(css: &str, backend: Backend, ticks: u32) -> State {
    let parsed = parse_css(css).expect("parse cabinet css");
    let mut state = State::default();
    state.load_properties(&parsed.properties);
    let mut ev = Evaluator::from_parsed(&parsed);
    ev.set_backend(backend);
    ev.wire_state_for_packed_memory(&mut state);
    for _ in 0..ticks {
        ev.tick(&mut state);
    }
    state
}

fn diff_states(a: &State, b: &State) -> Vec<String> {
    let mut diffs = Vec::new();
    if a.frame_counter != b.frame_counter {
        diffs.push(format!(
            "frame_counter: bytecode={} dag={}",
            a.frame_counter, b.frame_counter
        ));
    }
    if a.state_vars.len() != b.state_vars.len() {
        diffs.push(format!(
            "state_vars.len: bytecode={} dag={}",
            a.state_vars.len(),
            b.state_vars.len()
        ));
    }
    let n = a.state_vars.len().min(b.state_vars.len());
    for i in 0..n {
        if a.state_vars[i] != b.state_vars[i] {
            let name = a.state_var_names.get(i).map(|s| s.as_str()).unwrap_or("?");
            diffs.push(format!(
                "state_vars[{i}] (--{name}): bytecode={} dag={}",
                a.state_vars[i], b.state_vars[i]
            ));
            if diffs.len() > 32 {
                diffs.push("(more state_var diffs omitted)".into());
                break;
            }
        }
    }
    if a.memory.len() != b.memory.len() {
        diffs.push(format!(
            "memory.len: bytecode={} dag={}",
            a.memory.len(),
            b.memory.len()
        ));
    }
    let n = a.memory.len().min(b.memory.len());
    let mut mem_diffs = 0;
    for i in 0..n {
        if a.memory[i] != b.memory[i] {
            mem_diffs += 1;
            if mem_diffs <= 16 {
                diffs.push(format!(
                    "memory[0x{i:x}]: bytecode={:#x} dag={:#x}",
                    a.memory[i], b.memory[i]
                ));
            }
        }
    }
    if mem_diffs > 16 {
        diffs.push(format!("(+{} more memory diffs omitted)", mem_diffs - 16));
    }
    if a.extended.len() != b.extended.len() {
        diffs.push(format!(
            "extended.len: bytecode={} dag={}",
            a.extended.len(),
            b.extended.len()
        ));
    }
    for (k, va) in &a.extended {
        match b.extended.get(k) {
            Some(vb) if vb == va => {}
            other => diffs.push(format!(
                "extended[{k}]: bytecode={:?} dag={:?}",
                Some(va),
                other
            )),
        }
    }
    if a.string_properties != b.string_properties {
        diffs.push(format!(
            "string_properties: bytecode={:?} dag={:?}",
            a.string_properties, b.string_properties
        ));
    }
    diffs
}

#[test]
fn backends_agree_on_demo_cabinet() {
    let cabinet = worktree_root().join("web").join("demo.css");
    if !cabinet.is_file() {
        eprintln!(
            "backends_agree_on_demo_cabinet: skipping — no cabinet at {}",
            cabinet.display()
        );
        return;
    }
    let css = std::fs::read_to_string(&cabinet).expect("read cabinet");
    eprintln!("running {TICKS} ticks under each backend");
    let bytecode_state = run_n_ticks(&css, Backend::Bytecode, TICKS);
    let dag_state = run_n_ticks(&css, Backend::Dag, TICKS);
    let closure_state = run_n_ticks(&css, Backend::Closure, TICKS);

    let diffs = diff_states(&bytecode_state, &dag_state);
    assert_no_diffs("DAG walker", &diffs);
    let diffs = diff_states(&bytecode_state, &closure_state);
    assert_no_diffs("Closure backend", &diffs);
}

fn assert_no_diffs(name: &str, diffs: &[String]) {
    if !diffs.is_empty() {
        let mut msg = format!(
            "{name} diverged from bytecode interpreter after {TICKS} ticks ({} diff{}):\n",
            diffs.len(),
            if diffs.len() == 1 { "" } else { "s" },
        );
        for d in diffs {
            msg.push_str("  ");
            msg.push_str(d);
            msg.push('\n');
        }
        panic!("{msg}");
    }
}
