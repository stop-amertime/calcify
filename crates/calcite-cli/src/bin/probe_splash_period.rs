//! probe-splash-period — Technique 5: whole-loop-iteration affine evolution.
//!
//! Probes 1 and 3 showed per-tick specialisation is dead: no consecutive ticks
//! share a fingerprint and no consecutive tick pair has a stable write delta.
//! BUT the top fingerprints each fire exactly 32,000 times — the classic
//! signature of a 32K-iteration loop interleaving microcode stages.
//!
//! This probe searches for that structure:
//!
//!   1. Build per-tick fingerprint (uOp, IP, stable-state hash).
//!   2. Autocorrelate over a window to find period P in [2..512] such that
//!      fingerprints[t] == fingerprints[t+P] holds for a long stretch.
//!   3. For the winning P, verify over the full run: find the longest
//!      contiguous region where P-periodicity holds, report its length.
//!   4. Within that region, compute per-state-var delta per iteration
//!      (state_vars[t+P] - state_vars[t]). Classify each var as
//!      constant / affine-with-delta / chaotic.
//!   5. Check the write stream for affine targets with period P: for each
//!      offset o in [0..P), is the write at tick t*P+o an address that
//!      advances by a fixed stride per iteration?
//!
//! No transformation is implemented — just measurement.

use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "probe-splash-period", about = "Detect loop period and affine evolution")]
struct Args {
    #[arg(short, long)]
    input: Option<PathBuf>,

    #[arg(long, default_value = "719359")]
    halt: i32,

    #[arg(long, default_value = "3000000")]
    max_ticks: u32,

    /// Max period to search (inclusive).
    #[arg(long, default_value = "512")]
    max_period: u32,
}

#[inline]
fn fx_mix(mut h: u64, v: u64) -> u64 {
    const K: u64 = 0x517cc1b727220a95;
    h = h.wrapping_add(v).wrapping_mul(K);
    h ^= h >> 32;
    h
}

#[inline]
fn hash_i32_slice(vals: &[i32]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &v in vals {
        h = fx_mix(h, v as u32 as u64);
    }
    h
}

fn main() {
    let args = Args::parse();
    let default_css =
        PathBuf::from("C:/Users/AdmT9N0CX01V65438A/Documents/src/calcite/output/bootle-ctest.css");
    let css_path = args.input.unwrap_or(default_css);

    eprintln!("probe-splash-period — Technique 5");

    let t0 = Instant::now();
    let css = std::fs::read_to_string(&css_path).expect("read css");
    let parsed = calcite_core::parser::parse_css(&css).expect("parse");
    let mut state = calcite_core::State::default();
    state.load_properties(&parsed.properties);
    let mut evaluator = calcite_core::Evaluator::from_parsed(&parsed);
    eprintln!("  parse+compile: {:.1}s", t0.elapsed().as_secs_f64());

    let n_vars = state.state_var_count();
    let names = state.state_var_names.clone();
    let slot_of = |name: &str| names.iter().position(|n| n == name);
    let iterating_names = [
        "cycleCount", "memAddr", "SI", "CX", "DI", "AX", "BX", "IP",
        "biosAL", "memVal", "biosAH", "flags", "SP",
    ];
    let iterating: std::collections::HashSet<usize> = iterating_names
        .iter()
        .filter_map(|n| slot_of(n))
        .collect();
    let stable_slots: Vec<usize> = (0..n_vars).filter(|i| !iterating.contains(i)).collect();
    let slot_uop = slot_of("uOp");
    let slot_ip = slot_of("IP");

    // Collect: fingerprints, full state_vars snapshots, and per-tick first
    // memory write (addr >= 0).
    let mut fingerprints: Vec<u64> = Vec::with_capacity(args.max_ticks as usize);
    let mut flat_vars: Vec<i32> = Vec::with_capacity(n_vars * args.max_ticks as usize);
    let mut per_tick_mem_write: Vec<(i32, i32)> = Vec::with_capacity(args.max_ticks as usize);
    state.write_log = Some(Vec::with_capacity(16));

    let loop_start = Instant::now();
    let mut ticks: u32 = 0;
    loop {
        // Snapshot BEFORE tick.
        flat_vars.extend_from_slice(&state.state_vars);
        let uop = slot_uop.map(|s| state.state_vars[s]).unwrap_or(0);
        let ip = slot_ip.map(|s| state.state_vars[s]).unwrap_or(0);
        let stable: Vec<i32> = stable_slots.iter().map(|&s| state.state_vars[s]).collect();
        let fp = fx_mix(
            fx_mix(hash_i32_slice(&stable), uop as u32 as u64),
            ip as u32 as u64,
        );
        fingerprints.push(fp);

        // Clear write log.
        state.write_log.as_mut().unwrap().clear();
        evaluator.tick(&mut state);
        ticks += 1;

        // First memory write this tick (addr >= 0), else (-1, 0).
        let mw = state
            .write_log
            .as_ref()
            .unwrap()
            .iter()
            .copied()
            .find(|&(a, _)| a >= 0)
            .unwrap_or((-1, 0));
        per_tick_mem_write.push(mw);

        if state.read_mem(args.halt) != 0 || ticks >= args.max_ticks {
            break;
        }
    }
    state.write_log = None;
    let n = ticks as usize;
    eprintln!(
        "  halted at tick {} in {:.1}s",
        ticks,
        loop_start.elapsed().as_secs_f64()
    );

    // ---- Period detection via autocorrelation on a window ----
    //
    // For each P in [2..max_period], count how many t in [window_start..window_end-P]
    // satisfy fingerprints[t] == fingerprints[t+P]. The P with highest count wins.
    // Use a window centred on the middle of the run to avoid boot-time noise.
    let win_start = n / 4;
    let win_end = (3 * n) / 4;
    let win_len = win_end - win_start;
    eprintln!(
        "  autocorrelation window: ticks {}..{} ({} samples)",
        win_start, win_end, win_len
    );

    let mut scores: Vec<(u32, u64)> = Vec::new();
    for p in 2..=args.max_period {
        let mut hits = 0u64;
        let mut total = 0u64;
        for t in win_start..(win_end.saturating_sub(p as usize)) {
            total += 1;
            if fingerprints[t] == fingerprints[t + p as usize] {
                hits += 1;
            }
        }
        let _ = total;
        scores.push((p, hits));
    }
    scores.sort_by(|a, b| b.1.cmp(&a.1));

    println!("=== loop-period detection report ===");
    println!("total ticks: {}", n);
    println!();
    println!("top 10 candidate periods (autocorrelation over middle half):");
    println!(
        "  {:>6}  {:>12}  {:>8}",
        "period", "match count", "% of win"
    );
    for (p, hits) in scores.iter().take(10) {
        println!(
            "  {:>6}  {:>12}  {:>7.2}%",
            p,
            hits,
            *hits as f64 / win_len as f64 * 100.0
        );
    }

    let (best_p, best_hits) = scores[0];
    println!();
    println!(
        "→ winning period: P = {} ({} matches in window, {:.2}%)",
        best_p,
        best_hits,
        best_hits as f64 / win_len as f64 * 100.0
    );

    // ---- Verify on full stream: longest contiguous region of P-periodicity ----
    let p = best_p as usize;
    let mut longest_run = 0usize;
    let mut longest_start = 0usize;
    let mut cur_run = 0usize;
    let mut cur_start = 0usize;
    for t in 0..(n.saturating_sub(p)) {
        if fingerprints[t] == fingerprints[t + p] {
            if cur_run == 0 {
                cur_start = t;
            }
            cur_run += 1;
            if cur_run > longest_run {
                longest_run = cur_run;
                longest_start = cur_start;
            }
        } else {
            cur_run = 0;
        }
    }
    println!();
    println!(
        "longest contiguous P-periodic region: {} ticks starting at tick {} ({:.2}% of total)",
        longest_run,
        longest_start,
        longest_run as f64 / n as f64 * 100.0
    );

    // ---- Per-state-var classification over the longest region ----
    //
    // For each state var, for each offset o in [0..P), look at the sequence
    // state_vars[t+o] for t = longest_start, longest_start+P, longest_start+2P, ...
    // Classify: is it constant? linear with slope Δ? chaotic?
    println!();
    println!("per-state-var evolution over the P-periodic region:");
    println!(
        "  {:>14}  {:>12}  {:>12}  {}",
        "var", "at offset 0", "Δ/iteration", "classification"
    );

    // We need enough iterations to classify. Use at most 1000 iterations.
    let n_iters = (longest_run / p).min(1000);
    if n_iters < 3 {
        println!("  (not enough iterations to classify)");
    } else {
        for var_idx in 0..n_vars {
            // Sample at offset 0: values at iterations 0, 1, 2, ...
            let samples: Vec<i32> = (0..n_iters)
                .map(|i| flat_vars[(longest_start + i * p) * n_vars + var_idx])
                .collect();
            let first = samples[0];
            // All same?
            if samples.iter().all(|&v| v == first) {
                println!(
                    "  {:>14}  {:>12}  {:>12}  constant",
                    names[var_idx], first, 0
                );
                continue;
            }
            // Linear with constant delta?
            let d0 = samples[1].wrapping_sub(samples[0]);
            let linear = samples.windows(2).all(|w| w[1].wrapping_sub(w[0]) == d0);
            if linear {
                println!(
                    "  {:>14}  {:>12}  {:>12}  affine",
                    names[var_idx], first, d0
                );
            } else {
                // Count unique values to distinguish "periodic-small" from "chaotic".
                let uniq: std::collections::HashSet<i32> = samples.iter().copied().collect();
                println!(
                    "  {:>14}  {:>12}  {:>12}  non-affine (uniq={} of {})",
                    names[var_idx],
                    first,
                    "-",
                    uniq.len(),
                    n_iters
                );
            }
        }
    }

    // ---- Memory-write analysis ----
    //
    // For each offset o in [0..P), look at the sequence of memory-write
    // addresses (and values) at ticks longest_start + i*P + o across
    // iterations. Classify same way.
    println!();
    println!("per-offset memory-write evolution (offset within the P-tick iteration):");
    println!(
        "  {:>6}  {:>12}  {:>12}  {:>12}  {}",
        "offset", "addr@i=0", "Δaddr", "Δval", "classification"
    );

    let mut offsets_with_affine_writes = 0u32;
    let mut offsets_with_any_write = 0u32;
    let mut affine_offsets: Vec<(usize, i32, i32, i32)> = Vec::new(); // (offset, base_addr, d_addr, d_val)
    for o in 0..p {
        // Collect write addresses and values across iterations at this offset.
        let writes: Vec<(i32, i32)> = (0..n_iters)
            .map(|i| per_tick_mem_write[longest_start + i * p + o])
            .collect();
        // If most iterations have no memory write at this offset, classify no-write.
        let with_write = writes.iter().filter(|&&(a, _)| a >= 0).count();
        if with_write == 0 {
            continue;
        }
        offsets_with_any_write += 1;
        if with_write < writes.len() {
            // mixed — skip classification
            println!(
                "  {:>6}  {:>12}  {:>12}  {:>12}  mixed ({} of {} iters write)",
                o, "-", "-", "-", with_write, writes.len()
            );
            continue;
        }
        let addrs: Vec<i32> = writes.iter().map(|&(a, _)| a).collect();
        let vals: Vec<i32> = writes.iter().map(|&(_, v)| v).collect();
        let da = addrs[1].wrapping_sub(addrs[0]);
        let dv = vals[1].wrapping_sub(vals[0]);
        let addr_affine = addrs.windows(2).all(|w| w[1].wrapping_sub(w[0]) == da);
        let val_affine = vals.windows(2).all(|w| w[1].wrapping_sub(w[0]) == dv);
        let class = match (addr_affine, val_affine) {
            (true, true) => "addr affine, val affine",
            (true, false) => "addr affine, val chaotic",
            (false, true) => "addr chaotic, val affine",
            (false, false) => "both chaotic",
        };
        if addr_affine && val_affine {
            offsets_with_affine_writes += 1;
            affine_offsets.push((o, addrs[0], da, dv));
        }
        println!(
            "  {:>6}  {:>12}  {:>12}  {:>12}  {}",
            o, addrs[0], da, dv, class
        );
    }
    println!();
    println!(
        "offsets with writes: {} of {}; fully affine (addr+val): {}",
        offsets_with_any_write, p, offsets_with_affine_writes
    );
    if !affine_offsets.is_empty() {
        println!("affine-write offsets (candidate bulk-memcpy targets):");
        for (o, base, da, dv) in &affine_offsets {
            println!(
                "  offset {:>3}: base_addr={}  stride={:+}  val_stride={:+}  over {} iterations",
                o, base, da, dv, n_iters
            );
        }
    }
}
