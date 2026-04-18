//! probe-cycle-project — baseline vs CycleTracker-projected run of the
//! splash phase. Same layout as probe-splash-project but using the new
//! signature-based cycle detector.

use std::path::PathBuf;
use std::time::Instant;

use calcite_core::cycle_tracker::CycleTracker;

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn hash_state_vars(vars: &[i32]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &v in vars {
        h ^= v as u32 as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn run_baseline(css_path: &PathBuf, halt_addr: i32, max_ticks: u64) -> (u64, f64, u64, u64) {
    let css = std::fs::read_to_string(css_path).expect("read css");
    let parsed = calcite_core::parser::parse_css(&css).expect("parse");
    let mut state = calcite_core::State::default();
    state.load_properties(&parsed.properties);
    let mut evaluator = calcite_core::Evaluator::from_parsed(&parsed);

    let t0 = Instant::now();
    let mut ticks: u64 = 0;
    loop {
        evaluator.tick(&mut state);
        ticks += 1;
        if state.read_mem(halt_addr) != 0 || ticks >= max_ticks {
            break;
        }
    }
    let wall = t0.elapsed().as_secs_f64();
    let mem_hash = hash_bytes(&state.memory);
    let vars_hash = hash_state_vars(&state.state_vars);
    (ticks, wall, mem_hash, vars_hash)
}

fn run_projected(
    css_path: &PathBuf,
    halt_addr: i32,
    max_ticks: u64,
) -> (u64, f64, u64, u64, calcite_core::cycle_tracker::CycleStats) {
    let css = std::fs::read_to_string(css_path).expect("read css");
    let parsed = calcite_core::parser::parse_css(&css).expect("parse");
    let mut state = calcite_core::State::default();
    state.load_properties(&parsed.properties);
    let mut evaluator = calcite_core::Evaluator::from_parsed(&parsed);
    state.write_log = Some(Vec::with_capacity(16));

    let n_vars = state.state_vars.len();
    let mut tracker = CycleTracker::with_n_vars(n_vars);
    let mut pre_vars_scratch: Vec<i32> = Vec::with_capacity(n_vars);

    let t0 = Instant::now();
    let mut ticks: u64 = 0;
    let mut projection_budget: usize = 64;
    'outer: loop {
        // Only snapshot + project if we're locked AND aligned to a cycle
        // boundary. Otherwise the snapshot is pure waste.
        if tracker.can_project(&state.state_vars) {
            let saved_log = state.write_log.take();
            let mem_snapshot = state.memory.clone();
            let vars_snapshot = state.state_vars.clone();
            let frame_snapshot = state.frame_counter;

            let projected = tracker.project(&mut state, projection_budget);
            if projected > 0 {
                // Validate: run one real iteration (period ticks) and
                // check state advanced by exactly per_cycle_delta.
                let period = tracker.locked_period().unwrap_or(1);
                let pre_check = state.state_vars.clone();
                for _ in 0..period {
                    evaluator.tick(&mut state);
                    ticks += 1;
                    if state.read_mem(halt_addr) != 0 || ticks >= max_ticks {
                        break 'outer;
                    }
                }
                let consistent = tracker.validate_after_real_iter(&pre_check, &state.state_vars);
                state.write_log = saved_log;
                if consistent {
                    ticks += (period as u64) * (projected as u64);
                    projection_budget = (projection_budget * 2).min(4096);
                } else {
                    // Roll back. Re-hunt from a clean slate.
                    state.memory = mem_snapshot;
                    state.state_vars = vars_snapshot;
                    state.frame_counter = frame_snapshot;
                    tracker.unlock();
                    projection_budget = 64;
                }
                continue;
            }
            // project returned 0 despite can_project being true — shouldn't
            // happen, but recover gracefully.
            state.write_log = saved_log;
            drop(mem_snapshot);
            drop(vars_snapshot);
            let _ = frame_snapshot;
        }

        // Normal observation / tick path.
        if let Some(log) = state.write_log.as_mut() {
            log.clear();
        }
        pre_vars_scratch.clear();
        pre_vars_scratch.extend_from_slice(&state.state_vars);
        evaluator.tick(&mut state);
        ticks += 1;

        let log_ref: &[(i32, i32)] = state.write_log.as_deref().unwrap_or(&[]);
        tracker.observe(&pre_vars_scratch, &state.state_vars, log_ref);

        if state.read_mem(halt_addr) != 0 || ticks >= max_ticks {
            break;
        }
    }
    let wall = t0.elapsed().as_secs_f64();
    let mem_hash = hash_bytes(&state.memory);
    let vars_hash = hash_state_vars(&state.state_vars);
    (ticks, wall, mem_hash, vars_hash, tracker.stats.clone())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let max_ticks: u64 = args
        .iter()
        .position(|a| a == "--max-ticks")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(3_000_000);

    let css_path =
        PathBuf::from("C:/Users/AdmT9N0CX01V65438A/Documents/src/calcite/output/bootle-ctest.css");
    let halt_addr = 719359;

    eprintln!("probe-cycle-project — signature-based runtime projector");
    eprintln!();
    eprintln!("-- baseline --");
    let b = run_baseline(&css_path, halt_addr, max_ticks);
    eprintln!("  halt tick {} in {:.2}s ({:.0} ticks/s)", b.0, b.1, b.0 as f64 / b.1);
    eprintln!("  mem hash   {:016x}", b.2);
    eprintln!("  vars hash  {:016x}", b.3);

    eprintln!();
    eprintln!("-- projected --");
    let p = run_projected(&css_path, halt_addr, max_ticks);
    eprintln!("  halt tick {} in {:.2}s ({:.0} ticks/s)", p.0, p.1, p.0 as f64 / p.1);
    eprintln!("  mem hash   {:016x}", p.2);
    eprintln!("  vars hash  {:016x}", p.3);
    eprintln!("  first_lock_tick:  {}", p.4.first_lock_tick);
    eprintln!("  detected_period:  {}", p.4.detected_period);
    eprintln!("  ticks_observed:   {}", p.4.ticks_observed);
    eprintln!("  project_calls:    {}", p.4.project_calls);
    eprintln!("  project_success:  {}", p.4.project_success);
    eprintln!("  project_zero_aln: {}", p.4.project_zero_alignment);
    eprintln!("  iters_projected:  {}", p.4.iters_projected);
    eprintln!("  validate_fails:   {}", p.4.validate_fails);

    eprintln!();
    eprintln!("-- comparison --");
    eprintln!(
        "  halt:  baseline {} vs projected {} ({})",
        b.0, p.0, if b.0 == p.0 { "match" } else { "DIFFER" }
    );
    eprintln!(
        "  mem:   baseline {:016x} vs projected {:016x} ({})",
        b.2, p.2, if b.2 == p.2 { "match" } else { "DIFFER" }
    );
    eprintln!("  speedup: {:.2}× ({:.2}s → {:.2}s)", b.1 / p.1, b.1, p.1);
}
