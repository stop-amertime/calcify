//! probe-cycle-detect — run CycleTracker against bootle-ctest.css and
//! report the first tick at which it detects/locks.

use std::path::PathBuf;

use calcite_core::cycle_tracker::CycleTracker;

fn main() {
    let css_path =
        PathBuf::from("C:/Users/AdmT9N0CX01V65438A/Documents/src/calcite/output/bootle-ctest.css");
    let css = std::fs::read_to_string(&css_path).expect("read css");
    let parsed = calcite_core::parser::parse_css(&css).expect("parse");
    let mut state = calcite_core::State::default();
    state.load_properties(&parsed.properties);
    let mut evaluator = calcite_core::Evaluator::from_parsed(&parsed);
    state.write_log = Some(Vec::with_capacity(64));

    let mut tracker = CycleTracker::new();
    let mut pre_vars: Vec<i32> = Vec::with_capacity(state.state_vars.len());

    let halt_addr = 719359;
    let mut ticks: u64 = 0;
    loop {
        if let Some(log) = state.write_log.as_mut() { log.clear(); }
        pre_vars.clear();
        pre_vars.extend_from_slice(&state.state_vars);
        evaluator.tick(&mut state);
        ticks += 1;

        let log_ref: &[(i32, i32)] = state
            .write_log
            .as_deref()
            .unwrap_or(&[]);
        if let Some(info) = tracker.observe(&pre_vars, &state.state_vars, log_ref) {
            eprintln!(
                "LOCK at tick {}: period={} addr_stride/cycle={} writes/cycle={} fill={:?}",
                ticks,
                info.period,
                info.addr_stride_per_cycle,
                info.writes_per_cycle,
                info.fill_value,
            );
            break;
        }
        if state.read_mem(halt_addr) != 0 || ticks >= 500_000 {
            break;
        }
    }
    eprintln!("first_candidate_tick: {}", tracker.stats.first_candidate_tick);
    eprintln!("first_lock_tick:      {}", tracker.stats.first_lock_tick);
    eprintln!("detected_period:      {}", tracker.stats.detected_period);
    eprintln!("ticks_observed:       {}", tracker.stats.ticks_observed);
}
