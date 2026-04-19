//! Slide an autocorrelation window across the whole run and report the
//! dominant period in each window. Unlike probe-splash-period (which takes
//! one middle-half autocorrelation), this shows how the period changes as
//! the workload transitions between regimes (pixel-fill → blit → busy-wait → ...).
//!
//! Usage: probe-regions [--window N] [--stride N] [--halt ADDR]

use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "probe-regions")]
struct Args {
    #[arg(short, long)]
    input: Option<PathBuf>,

    #[arg(long, default_value = "719359")]
    halt: i32,

    #[arg(long, default_value = "3000000")]
    max_ticks: u32,

    /// Autocorrelation window width (samples).
    #[arg(long, default_value = "4096")]
    window: u32,

    /// Slide step between windows (samples).
    #[arg(long, default_value = "8192")]
    stride: u32,

    /// Minimum period searched.
    #[arg(long, default_value = "2")]
    min_period: u32,

    /// Maximum period searched.
    #[arg(long, default_value = "256")]
    max_period: u32,
}

fn hash_i32_slice(vals: &[i32]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &v in vals {
        h ^= v as u32 as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn main() {
    let args = Args::parse();
    let default_css = PathBuf::from(
        "C:/Users/AdmT9N0CX01V65438A/Documents/src/calcite/output/bootle-ctest.css",
    );
    let css_path = args.input.unwrap_or(default_css);

    eprintln!("probe-regions");
    eprintln!("  window={}, stride={}, max_period={}", args.window, args.stride, args.max_period);

    let t0 = Instant::now();
    let css = std::fs::read_to_string(&css_path).expect("read css");
    let parsed = calcite_core::parser::parse_css(&css).expect("parse");
    let mut state = calcite_core::State::default();
    state.load_properties(&parsed.properties);
    let mut evaluator = calcite_core::Evaluator::from_parsed(&parsed);
    eprintln!("  parse+compile: {:.1}s", t0.elapsed().as_secs_f64());

    let n_vars = state.state_var_count();
    let names = state.state_var_names.clone();
    // Exclude iterating counters (spec convention) from the fingerprint.
    let iterating_names = [
        "cycleCount", "memAddr", "SI", "CX", "DI", "AX", "BX",
        "biosAL", "memVal", "biosAH", "flags", "SP",
    ];
    let iter_idx: std::collections::HashSet<usize> = iterating_names
        .iter()
        .filter_map(|n| names.iter().position(|x| x == n))
        .collect();
    let sub_idx: Vec<usize> = (0..n_vars).filter(|i| !iter_idx.contains(i)).collect();
    eprintln!("  fingerprint uses {} of {} slots (excl. iterating)", sub_idx.len(), n_vars);

    // Tick through the whole run, collect fingerprints.
    let mut fps: Vec<u64> = Vec::with_capacity(args.max_ticks as usize);
    let loop_start = Instant::now();
    let mut ticks: u32 = 0;
    loop {
        let sub: Vec<i32> = sub_idx.iter().map(|&s| state.state_vars[s]).collect();
        fps.push(hash_i32_slice(&sub));
        evaluator.tick(&mut state);
        ticks += 1;
        if state.read_mem(args.halt) != 0 || ticks >= args.max_ticks {
            break;
        }
    }
    let n = ticks as usize;
    eprintln!("  halted at tick {} in {:.1}s", ticks, loop_start.elapsed().as_secs_f64());

    // Slide window across the run.
    let w = args.window as usize;
    let stride = args.stride as usize;
    let max_p = args.max_period as usize;
    let min_p = args.min_period as usize;

    println!("=== sliding autocorrelation ===");
    println!(
        "  window={}, stride={}, period range [{}..{}]",
        w, stride, min_p, max_p
    );
    println!();
    println!(
        "  {:>10}  {:>10}  {:>8}  {:>7}  {:>8}",
        "start", "end", "best P", "match%", "vs total"
    );

    let mut start = 0usize;
    while start + w <= n {
        let end = start + w;
        let slice = &fps[start..end];
        let mut best_p = 0usize;
        let mut best_m = 0usize;
        for p in min_p..=max_p.min(w / 3) {
            let mut m = 0usize;
            for t in 0..(w - p) {
                if slice[t] == slice[t + p] { m += 1; }
            }
            if m > best_m { best_m = m; best_p = p; }
        }
        let overlap = (w - best_p) as f64;
        let rate = if overlap > 0.0 { best_m as f64 * 100.0 / overlap } else { 0.0 };
        let pct_of_total = start as f64 * 100.0 / n as f64;
        println!(
            "  {:>10}  {:>10}  {:>8}  {:>6.2}%  {:>7.1}%",
            start, end, best_p, rate, pct_of_total
        );
        start += stride;
    }
}
