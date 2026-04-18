//! probe-splash-trace — Technique 1: trace-specialisation viability.
//!
//! Question: if we cached "which dispatch branch this tick took" and
//! specialised the inner loop body for runs where the same branch fires
//! repeatedly, how many ticks would be covered and how long are the runs?
//!
//! Fingerprint: `(uOp, IP, non-iterating-state-hash)` — identifies which
//! microcode stage + instruction address the tick was in, with iterators
//! (DI, SI, CX, cycleCount, memAddr, etc.) stripped so they don't break
//! the run. A consecutive run with identical fingerprints is a candidate
//! trace: a JIT could compile one specialised version of that inner tick
//! and run it N times tightly.
//!
//! Outputs:
//!   * histogram of run lengths (how many runs of length 1, 2-4, 5-16, ...)
//!   * total ticks covered by runs >= K (for K = 1, 4, 16, 64, 256, 1024)
//!   * top-5 longest runs with the fingerprint that identifies them
//!
//! No caching or specialisation is implemented — just measurement.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "probe-splash-trace", about = "Measure trace-specialisation run lengths")]
struct Args {
    #[arg(short, long)]
    input: Option<PathBuf>,

    #[arg(long, default_value = "719359")]
    halt: i32,

    #[arg(long, default_value = "3000000")]
    max_ticks: u32,
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

    eprintln!("probe-splash-trace — Technique 1");
    eprintln!("  css: {}", css_path.display());

    let t0 = Instant::now();
    let css = std::fs::read_to_string(&css_path).expect("read css");
    let parsed = calcite_core::parser::parse_css(&css).expect("parse");
    let mut state = calcite_core::State::default();
    state.load_properties(&parsed.properties);
    let mut evaluator = calcite_core::Evaluator::from_parsed(&parsed);
    eprintln!(
        "  parse+compile: {:.1}s, state vars: {}",
        t0.elapsed().as_secs_f64(),
        state.state_var_count(),
    );

    let n_vars = state.state_var_count();
    let names = state.state_var_names.clone();

    // Resolve key slot indices.
    let slot_of = |name: &str| names.iter().position(|n| n == name);

    // Iterating vars to strip from the fingerprint's non-iterating-state-hash.
    // These are the vars discovered in probe-splash-memo as having >10 unique
    // values across the run. Anything not in this set goes into the hash.
    let iterating_names = [
        "cycleCount", "memAddr", "SI", "CX", "DI", "AX", "BX", "IP",
        "biosAL", "memVal", "biosAH", "flags", "SP",
    ];
    let iterating: std::collections::HashSet<usize> = iterating_names
        .iter()
        .filter_map(|n| slot_of(n))
        .collect();
    let stable_slots: Vec<usize> = (0..n_vars).filter(|i| !iterating.contains(i)).collect();
    // `uOp` and `IP` get first-class roles in the fingerprint.
    let slot_uop = slot_of("uOp");
    let slot_ip = slot_of("IP");
    eprintln!(
        "  fingerprint = (uOp, IP, hash of {} stable vars): uOp_slot={:?}, IP_slot={:?}",
        stable_slots.len(),
        slot_uop,
        slot_ip
    );

    // Per-tick fingerprints.
    let mut fingerprints: Vec<u64> = Vec::with_capacity(args.max_ticks as usize);

    let loop_start = Instant::now();
    let mut ticks: u32 = 0;

    loop {
        let vars = &state.state_vars;
        let uop = slot_uop.map(|s| vars[s]).unwrap_or(0);
        let ip = slot_ip.map(|s| vars[s]).unwrap_or(0);
        let stable: Vec<i32> = stable_slots.iter().map(|&s| vars[s]).collect();
        let stable_hash = hash_i32_slice(&stable);
        let fp = fx_mix(
            fx_mix(stable_hash, uop as u32 as u64),
            ip as u32 as u64,
        );
        fingerprints.push(fp);

        evaluator.tick(&mut state);
        ticks += 1;

        if state.read_mem(args.halt) != 0 || ticks >= args.max_ticks {
            break;
        }
    }
    eprintln!(
        "  halted at tick {} in {:.1}s",
        ticks,
        loop_start.elapsed().as_secs_f64()
    );

    // ---- Compute run-lengths of identical fingerprints ----
    let mut run_lengths: Vec<u32> = Vec::new();
    let mut run_fps: Vec<u64> = Vec::new();
    let mut i = 0;
    while i < fingerprints.len() {
        let fp = fingerprints[i];
        let mut j = i + 1;
        while j < fingerprints.len() && fingerprints[j] == fp {
            j += 1;
        }
        run_lengths.push((j - i) as u32);
        run_fps.push(fp);
        i = j;
    }

    let total_ticks = fingerprints.len() as u64;
    let total_runs = run_lengths.len() as u64;

    // Bucket histogram.
    let buckets = [1u32, 2, 4, 16, 64, 256, 1024, 4096, 16384, 65536];
    let mut bucket_counts = vec![0u64; buckets.len() + 1];
    let mut bucket_ticks = vec![0u64; buckets.len() + 1];
    for &len in &run_lengths {
        let idx = buckets.iter().position(|&b| len < b).unwrap_or(buckets.len());
        bucket_counts[idx] += 1;
        bucket_ticks[idx] += len as u64;
    }

    println!("=== trace-specialisation viability report ===");
    println!("total ticks: {}", total_ticks);
    println!("total distinct runs: {}", total_runs);
    println!("avg run length: {:.2}", total_ticks as f64 / total_runs as f64);
    println!();
    println!("run-length histogram:");
    println!("  {:>12}  {:>10}  {:>14}  {:>7}", "length", "runs", "ticks", "% ticks");
    let mut lo = 0u32;
    for (i, &hi) in buckets.iter().enumerate() {
        let label = if lo == hi.saturating_sub(1) {
            format!("= {}", lo)
        } else {
            format!("{}..{}", lo, hi - 1)
        };
        println!(
            "  {:>12}  {:>10}  {:>14}  {:>6.2}%",
            label,
            bucket_counts[i],
            bucket_ticks[i],
            bucket_ticks[i] as f64 / total_ticks as f64 * 100.0,
        );
        lo = hi;
    }
    println!(
        "  {:>12}  {:>10}  {:>14}  {:>6.2}%",
        format!(">= {}", lo),
        bucket_counts[buckets.len()],
        bucket_ticks[buckets.len()],
        bucket_ticks[buckets.len()] as f64 / total_ticks as f64 * 100.0,
    );

    // Cumulative: "ticks covered by runs of length >= K"
    println!();
    println!("cumulative (ticks inside runs of length >= K):");
    for k in &[1u32, 4, 16, 64, 256, 1024, 4096, 16384, 65536] {
        let covered: u64 = run_lengths
            .iter()
            .filter(|&&l| l >= *k)
            .map(|&l| l as u64)
            .sum();
        println!(
            "  K = {:>6}: {:>14} ticks ({:>6.2}%)",
            k,
            covered,
            covered as f64 / total_ticks as f64 * 100.0
        );
    }

    // Top-5 longest runs.
    let mut by_len: Vec<(u32, u64)> = run_lengths
        .iter()
        .zip(run_fps.iter())
        .map(|(&l, &f)| (l, f))
        .collect();
    by_len.sort_by(|a, b| b.0.cmp(&a.0));
    println!();
    println!("top-5 longest runs:");
    for (i, (len, fp)) in by_len.iter().take(5).enumerate() {
        println!("  #{}: length={:>6}  fingerprint={:016x}", i + 1, len, fp);
    }

    // Also: how many distinct fingerprints account for 90% of ticks?
    let mut by_fp: HashMap<u64, u64> = HashMap::new();
    for (&fp, &len) in run_fps.iter().zip(run_lengths.iter()) {
        *by_fp.entry(fp).or_insert(0) += len as u64;
    }
    let mut fp_ticks: Vec<(u64, u64)> = by_fp.into_iter().collect();
    fp_ticks.sort_by(|a, b| b.1.cmp(&a.1));
    let mut cum = 0u64;
    let mut count_90 = 0usize;
    let target = (total_ticks as f64 * 0.9) as u64;
    for (i, &(_, t)) in fp_ticks.iter().enumerate() {
        cum += t;
        if cum >= target {
            count_90 = i + 1;
            break;
        }
    }
    println!();
    println!(
        "{} distinct fingerprints account for 90% of ticks (out of {} total)",
        count_90,
        fp_ticks.len()
    );
    println!("top-5 fingerprints by tick coverage:");
    for (i, (fp, t)) in fp_ticks.iter().take(5).enumerate() {
        println!(
            "  #{}: fingerprint={:016x}  ticks={:>10}  ({:>5.2}%)",
            i + 1,
            fp,
            t,
            *t as f64 / total_ticks as f64 * 100.0,
        );
    }
}
