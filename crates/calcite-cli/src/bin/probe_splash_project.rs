//! probe-splash-project — end-to-end test of the runtime period projector.
//!
//! Runs bootle-ctest.css under TWO drivers and compares:
//!   1. Baseline: plain tick loop until halt.
//!   2. Projected: tick loop with `tick_period::PeriodTracker` skipping
//!      forward via `memset` whenever the detector is locked.
//!
//! Reports: halt tick, wall time, speedup, and a final-memory hash so we
//! can confirm bit-identical output.

use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;

use calcite_core::tick_period::PeriodTracker;

#[derive(Parser, Debug)]
#[command(name = "probe-splash-project", about = "Runtime period projector — end-to-end test")]
struct Args {
    #[arg(short, long)]
    input: Option<PathBuf>,

    #[arg(long, default_value = "719359")]
    halt: i32,

    #[arg(long, default_value = "3000000")]
    max_ticks: u64,

    /// Skip the baseline run (useful when iterating on the projector).
    #[arg(long)]
    skip_baseline: bool,

    /// Skip the projected run (baseline-only, for sanity).
    #[arg(long)]
    skip_projected: bool,

    /// Run the projected driver but always reject projection at the
    /// caller level (observes but never projects). Useful for isolating
    /// tracker-observation overhead vs projection correctness.
    #[arg(long)]
    disable_projection: bool,
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    // FNV-1a 64.
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
    disable_projection: bool,
) -> (u64, f64, u64, u64, calcite_core::tick_period::PeriodStats) {
    let css = std::fs::read_to_string(css_path).expect("read css");
    let parsed = calcite_core::parser::parse_css(&css).expect("parse");
    let mut state = calcite_core::State::default();
    state.load_properties(&parsed.properties);
    let mut evaluator = calcite_core::Evaluator::from_parsed(&parsed);
    let mut tracker = PeriodTracker::new(&state);

    // Enable write_log so the tracker can see memory writes.
    state.write_log = Some(Vec::with_capacity(16));

    // Reusable pre-tick scratch buffer — avoids a per-tick heap alloc.
    let mut pre_vars_scratch: Vec<i32> = Vec::with_capacity(state.state_vars.len());

    let t0 = Instant::now();
    let mut ticks: u64 = 0;
    'outer: loop {
        // Try to project.
        if !disable_projection && tracker.can_project(&state.state_vars) {
            let saved_log = state.write_log.take();

            // Snapshot for rollback. Cloning state.memory is 820KB → ~80us
            // at memcpy speeds. A single projected iteration of period 26
            // saves 26 ticks ≈ 150us at 170K ticks/s. So the snapshot is
            // paying for itself if we project >= ~1 iteration (always
            // true — budget starts at 32).
            let mem_snapshot = state.memory.clone();
            let vars_snapshot = state.state_vars.clone();
            let frame_snapshot = state.frame_counter;
            let extended_snapshot = state.extended.clone();
            let strings_snapshot = state.string_properties.clone();

            let projected = tracker.project(&mut state);
            if projected > 0 {
                let period = tracker.locked_period().unwrap_or(1);
                // Validate by running one real iteration (period ticks) with
                // write_log still DISABLED (saved_log held aside). Skipping
                // the per-write log push is a material speedup during
                // validation ticks — they're ~3% of all ticks at the splash
                // phase's peak projection rate.
                let pre_vars = state.state_vars.clone();
                for _ in 0..period {
                    evaluator.tick(&mut state);
                    ticks += 1;
                    if state.read_mem(halt_addr) != 0 || ticks >= max_ticks {
                        // Halt fires — state is valid regardless of lock.
                        break 'outer;
                    }
                }
                let post_vars = state.state_vars.clone();
                let still_locked = tracker.validate_iteration(&pre_vars, &post_vars);
                // Restore write_log now that validation is done.
                state.write_log = saved_log;
                if still_locked {
                    // Validation passed. The projected iterations were correct.
                    ticks += (period as u64) * (projected as u64);
                } else {
                    // Rollback: restore state to pre-projection.
                    state.memory = mem_snapshot;
                    state.state_vars = vars_snapshot;
                    state.frame_counter = frame_snapshot;
                    state.extended = extended_snapshot;
                    state.string_properties = strings_snapshot;
                    // We've already charged `period` ticks for validation;
                    // undo them since state was rolled back.
                    ticks -= period as u64;
                }
                continue;
            }
            // project returned 0 — nothing projected; state is unchanged
            // but snapshots were taken, so it doesn't matter. Drop them and
            // restore write_log.
            drop(mem_snapshot);
            drop(vars_snapshot);
            drop(extended_snapshot);
            drop(strings_snapshot);
            let _ = frame_snapshot;
            state.write_log = saved_log;
        }

        // Normal tick path.
        //
        // If already locked, we only reach here when can_project() returned
        // false (misaligned) — which is expected on the first few post-lock
        // ticks until we hit an iteration boundary. Don't bother observing
        // or logging memory writes during that brief window — the tracker
        // only needs calibration data in Cold mode.
        let locked = tracker.is_locked();
        if !locked {
            if let Some(log) = state.write_log.as_mut() {
                log.clear();
            }
            pre_vars_scratch.clear();
            pre_vars_scratch.extend_from_slice(&state.state_vars);
        }
        evaluator.tick(&mut state);
        ticks += 1;

        if !locked {
            let mw = state
                .write_log
                .as_ref()
                .and_then(|v| v.iter().copied().find(|&(a, _)| a >= 0));
            tracker.observe(&pre_vars_scratch, mw);
        }

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
    let args = Args::parse();
    let default_css =
        PathBuf::from("C:/Users/AdmT9N0CX01V65438A/Documents/src/calcite/output/bootle-ctest.css");
    let css_path = args.input.unwrap_or(default_css);

    eprintln!("probe-splash-project — runtime period projector");
    eprintln!("  css: {}", css_path.display());
    eprintln!("  halt: memory[{}] != 0", args.halt);

    let mut baseline = None;
    if !args.skip_baseline {
        eprintln!();
        eprintln!("-- baseline run --");
        let r = run_baseline(&css_path, args.halt, args.max_ticks);
        eprintln!(
            "  halted at tick {} in {:.2}s ({:.0} ticks/s)",
            r.0, r.1, r.0 as f64 / r.1
        );
        eprintln!("  final memory hash: {:016x}", r.2);
        eprintln!("  final state_vars hash: {:016x}", r.3);
        baseline = Some(r);
    }

    let mut projected = None;
    if !args.skip_projected {
        eprintln!();
        eprintln!("-- projected run --");
        let r = run_projected(&css_path, args.halt, args.max_ticks, args.disable_projection);
        eprintln!(
            "  halted at tick {} in {:.2}s ({:.0} ticks/s)",
            r.0, r.1, r.0 as f64 / r.1
        );
        eprintln!("  final memory hash: {:016x}", r.2);
        eprintln!("  final state_vars hash: {:016x}", r.3);
        eprintln!("  period stats:");
        eprintln!("    ticks_observed:   {}", r.4.ticks_observed);
        eprintln!("    lock_count:       {}", r.4.lock_count);
        eprintln!("    miss_count:       {}", r.4.miss_count);
        eprintln!("    iters_projected:  {}", r.4.iters_projected);
        eprintln!("    ticks_projected:  {}", r.4.ticks_projected);
        eprintln!("    bytes_memset:     {}", r.4.bytes_memset);
        eprintln!("    calibrations_att: {}", r.4.calibrations_attempted);
        eprintln!("    last_best_period: {}", r.4.last_best_period);
        eprintln!("    last_best_matches:{}", r.4.last_best_matches);
        eprintln!("    last_reject_rsn:  {}", r.4.last_reject_reason);
        projected = Some(r);
    }

    if let (Some(b), Some(p)) = (baseline, projected) {
        eprintln!();
        eprintln!("-- comparison --");
        eprintln!(
            "  halt tick:    baseline {} vs projected {} ({})",
            b.0, p.0,
            if b.0 == p.0 { "match" } else { "DIFFER" }
        );
        eprintln!(
            "  memory hash:  baseline {:016x} vs projected {:016x} ({})",
            b.2, p.2,
            if b.2 == p.2 { "match" } else { "DIFFER" }
        );
        eprintln!(
            "  state_vars:   baseline {:016x} vs projected {:016x} ({})",
            b.3, p.3,
            if b.3 == p.3 { "match" } else { "DIFFER" }
        );
        eprintln!(
            "  speedup:      {:.2}× ({:.2}s → {:.2}s)",
            b.1 / p.1, b.1, p.1
        );
    }
}
