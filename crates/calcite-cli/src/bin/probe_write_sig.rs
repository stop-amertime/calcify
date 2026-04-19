//! probe-write-sig — dump per-tick write-address streams from the splash fill.
//!
//! Goal: see the structural signature of a REP STOSB iteration and confirm
//! that consecutive ticks within the hot loop produce write streams that
//! are identical up to a constant address offset.

use std::path::PathBuf;

fn main() {
    let css_path =
        PathBuf::from("C:/Users/AdmT9N0CX01V65438A/Documents/src/calcite/output/bootle-ctest.css");
    let css = std::fs::read_to_string(&css_path).expect("read css");
    let parsed = calcite_core::parser::parse_css(&css).expect("parse");
    let mut state = calcite_core::State::default();
    state.load_properties(&parsed.properties);
    let mut evaluator = calcite_core::Evaluator::from_parsed(&parsed);
    state.write_log = Some(Vec::with_capacity(64));

    let halt_addr = 719359;
    // Run until mid-splash; dump ticks in a window.
    let dump_start = 20_000u64;
    let dump_end = 20_030u64;
    let mut ticks: u64 = 0;
    loop {
        if let Some(log) = state.write_log.as_mut() { log.clear(); }
        evaluator.tick(&mut state);
        ticks += 1;
        if ticks >= dump_start && ticks < dump_end {
            if let Some(log) = state.write_log.as_ref() {
                let mem_writes: Vec<_> = log.iter().copied().filter(|&(a, _)| a >= 0).collect();
                let state_writes: Vec<_> = log.iter().copied().filter(|&(a, _)| a < 0).collect();
                eprintln!(
                    "tick {}: mem_writes={:?} state_writes={:?}",
                    ticks, mem_writes, state_writes
                );
            }
        }
        if state.read_mem(halt_addr) != 0 || ticks >= 50_000 {
            break;
        }
    }
}
