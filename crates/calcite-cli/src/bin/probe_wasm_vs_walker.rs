//! Pinpoint the first tick where DagV2Wasm and DagV2 (walker) diverge on a
//! real cabinet. Walker is the "this is what v2 thinks the answer is"
//! reference; wasm is supposed to produce bit-identical output.
//!
//! Strategy: run both on the same cabinet, check hashes at coarse
//! milestones to find the divergence window, then binary-search inside it.
//!
//! Usage: `probe-wasm-vs-walker <cabinet.css> [max_ticks]`

use std::env;
use std::fs;

use calcite_core::parser::parse_css;
use calcite_core::{Backend, Evaluator, State};

fn main() {
    let path = env::args()
        .nth(1)
        .expect("usage: probe-wasm-vs-walker <cabinet.css> [max_ticks]");
    let max_ticks: u64 = env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(50_000);

    eprintln!("loading {path}…");
    let css = fs::read_to_string(&path).expect("read cabinet");
    let parsed = parse_css(&css).expect("parse");

    eprintln!("scanning walker vs wasm at 1000-tick resolution to {max_ticks}…");

    let mut walker_state = State::default();
    walker_state.load_properties(&parsed.properties);
    let mut walker = Evaluator::from_parsed(&parsed);
    walker.set_backend(Backend::DagV2);

    let mut wasm_state = State::default();
    wasm_state.load_properties(&parsed.properties);
    let mut wasm = Evaluator::from_parsed(&parsed);
    wasm.set_backend(Backend::DagV2Wasm);

    const STEP: u64 = 1000;
    let mut tick: u64 = 0;
    let mut last_match_tick: u64 = 0;
    let mut first_diff_tick: Option<u64> = None;

    while tick < max_ticks {
        for _ in 0..STEP {
            walker.tick(&mut walker_state);
            wasm.tick(&mut wasm_state);
        }
        tick += STEP;

        let wh = hash_state(&walker_state);
        let kh = hash_state(&wasm_state);
        if wh == kh {
            last_match_tick = tick;
        } else {
            first_diff_tick = Some(tick);
            println!(
                "tick {tick}: walker={wh:016x}  wasm={kh:016x}  DIVERGED (last match was tick {last_match_tick})"
            );
            break;
        }
    }

    if first_diff_tick.is_none() {
        println!("no divergence in {max_ticks} ticks");
        return;
    }
    let lo = last_match_tick;
    let hi = first_diff_tick.unwrap();
    println!();
    println!("=== narrowing in window [{lo}, {hi}] ===");

    // Restart cleanly and walk to lo, then run forward 1 tick at a time.
    let mut walker_state = State::default();
    walker_state.load_properties(&parsed.properties);
    let mut walker = Evaluator::from_parsed(&parsed);
    walker.set_backend(Backend::DagV2);
    let mut wasm_state = State::default();
    wasm_state.load_properties(&parsed.properties);
    let mut wasm = Evaluator::from_parsed(&parsed);
    wasm.set_backend(Backend::DagV2Wasm);

    for _ in 0..lo {
        walker.tick(&mut walker_state);
        wasm.tick(&mut wasm_state);
    }
    // Confirm we're matched at lo.
    assert_eq!(hash_state(&walker_state), hash_state(&wasm_state), "should match at lo");

    for t in lo..hi {
        walker.tick(&mut walker_state);
        wasm.tick(&mut wasm_state);
        let wh = hash_state(&walker_state);
        let kh = hash_state(&wasm_state);
        if wh != kh {
            println!("first divergence: AFTER tick {} (i.e. tick {t} computed differently)", t + 1);
            // Find which slot/byte differs.
            report_diff(&walker_state, &wasm_state);
            return;
        }
    }
    println!("(could not narrow further)");
}

fn report_diff(a: &State, b: &State) {
    let mut sv_diffs: Vec<(usize, i32, i32)> = Vec::new();
    for (i, (av, bv)) in a.state_vars.iter().zip(b.state_vars.iter()).enumerate() {
        if av != bv {
            sv_diffs.push((i, *av, *bv));
        }
    }
    let mut mem_diffs: Vec<(usize, u8, u8)> = Vec::new();
    let n = a.memory.len().min(b.memory.len());
    for i in 0..n {
        if a.memory[i] != b.memory[i] {
            mem_diffs.push((i, a.memory[i], b.memory[i]));
            if mem_diffs.len() >= 16 { break; }
        }
    }
    println!("  state_vars diffs: {} (showing up to 16)", sv_diffs.len());
    for (i, av, bv) in sv_diffs.iter().take(16) {
        println!("    sv[{i}]: walker={av} wasm={bv}");
    }
    println!("  memory diffs: {} (showing up to 16)", mem_diffs.len());
    for (i, av, bv) in mem_diffs.iter().take(16) {
        println!("    mem[0x{i:05x}]: walker=0x{av:02x} wasm=0x{bv:02x}");
    }
}

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
