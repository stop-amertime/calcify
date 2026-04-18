//! probe-splash-affine — Technique 3: affine-copy / broadcast-copy viability.
//!
//! Question: across consecutive ticks, does the (write_addr, write_value)
//! sequence follow a regular pattern? Specifically, is there a run of N
//! ticks where each tick does exactly one memory write at `base + i*stride`
//! with values that are themselves affine (constant or +c each tick)?
//! If so, N ticks collapse to one vectorised "memcpy-with-transform".
//!
//! Method:
//!   * Enable `state.write_log` per tick.
//!   * For each tick, pick the FIRST (addr, value) written (filter out
//!     writes to address-0-ish that are iterator bookkeeping — we'll show
//!     the distribution).
//!   * Compute address_delta[t] = addr[t] - addr[t-1] and
//!     value_delta[t] = value[t] - value[t-1].
//!   * Count maximal runs of identical (address_delta, value_delta) tuples.
//!
//! Also report: distribution of writes-per-tick, and top-5 write-address
//! addresses (in case there's a hot iterator address that's not actually
//! the copy target).
//!
//! No transformation is implemented — just measurement.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "probe-splash-affine", about = "Measure affine-store run lengths")]
struct Args {
    #[arg(short, long)]
    input: Option<PathBuf>,

    #[arg(long, default_value = "719359")]
    halt: i32,

    #[arg(long, default_value = "3000000")]
    max_ticks: u32,
}

fn main() {
    let args = Args::parse();
    let default_css =
        PathBuf::from("C:/Users/AdmT9N0CX01V65438A/Documents/src/calcite/output/bootle-ctest.css");
    let css_path = args.input.unwrap_or(default_css);

    eprintln!("probe-splash-affine — Technique 3");
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

    // Enable write log.
    state.write_log = Some(Vec::with_capacity(32));

    // Per-tick summary: (writes_count, first_write_addr, first_write_val,
    // first_mem_write_addr, first_mem_write_val). "mem" = addr >= 0 (filters
    // out state-var writes that are tick bookkeeping).
    let mut per_tick_writes: Vec<(u32, i32, i32, i32, i32)> =
        Vec::with_capacity(args.max_ticks as usize);
    let mut writes_count_hist: HashMap<u32, u64> = HashMap::new();

    let loop_start = Instant::now();
    let mut ticks: u32 = 0;

    loop {
        if let Some(ref mut log) = state.write_log {
            log.clear();
        }
        evaluator.tick(&mut state);
        ticks += 1;

        let (count, first_addr, first_val, first_mem_addr, first_mem_val) = {
            let log = state.write_log.as_ref().unwrap();
            let count = log.len() as u32;
            let first = log.first().copied().unwrap_or((i32::MIN, 0));
            let first_mem = log
                .iter()
                .copied()
                .find(|&(a, _)| a >= 0)
                .unwrap_or((i32::MIN, 0));
            (count, first.0, first.1, first_mem.0, first_mem.1)
        };
        per_tick_writes.push((count, first_addr, first_val, first_mem_addr, first_mem_val));
        *writes_count_hist.entry(count).or_insert(0) += 1;

        if state.read_mem(args.halt) != 0 || ticks >= args.max_ticks {
            break;
        }
    }
    state.write_log = None;
    eprintln!(
        "  halted at tick {} in {:.1}s",
        ticks,
        loop_start.elapsed().as_secs_f64()
    );

    // Writes-per-tick histogram.
    println!("=== affine-store viability report ===");
    println!("total ticks: {}", ticks);
    println!();
    println!("writes per tick (how many state.write_mem calls):");
    let mut buckets: Vec<(u32, u64)> = writes_count_hist.into_iter().collect();
    buckets.sort_by_key(|&(k, _)| k);
    for (k, v) in &buckets {
        println!("  {:>3} writes/tick: {:>10} ticks ({:>5.2}%)",
            k, v, *v as f64 / ticks as f64 * 100.0);
    }

    // Two variants: (a) first write per tick, (b) first memory write per tick.
    // Variant (b) is more robust because state-var writes (addr < 0) are
    // typically bookkeeping.
    for variant_idx in 0..2 {
        let variant_name = if variant_idx == 0 {
            "first write (any)"
        } else {
            "first memory write (addr >= 0)"
        };
        println!();
        println!("---- variant: {} ----", variant_name);

        // Build delta sequence.
        let mut deltas: Vec<(i64, i64)> = Vec::with_capacity(ticks as usize);
        // skip_count counts ticks where no such write exists (we can't form a
        // delta, treat as "break").
        let mut skip_count = 0u64;
        let mut prev: Option<(i32, i32)> = None;
        for t in 0..ticks as usize {
            let (_count, fa, fv, ma, mv) = per_tick_writes[t];
            let chosen = if variant_idx == 0 {
                if fa == i32::MIN { None } else { Some((fa, fv)) }
            } else {
                if ma == i32::MIN { None } else { Some((ma, mv)) }
            };
            match (prev, chosen) {
                (Some((pa, pv)), Some((a, v))) => {
                    deltas.push(((a as i64) - (pa as i64), (v as i64) - (pv as i64)));
                }
                _ => {
                    // break in sequence — insert sentinel
                    deltas.push((i64::MIN, i64::MIN));
                    if chosen.is_none() {
                        skip_count += 1;
                    }
                }
            }
            prev = chosen;
        }
        println!("  ticks with no applicable write: {}", skip_count);

        // Run-lengths of identical deltas (excluding sentinels).
        let mut run_lengths: Vec<u32> = Vec::new();
        let mut run_deltas: Vec<(i64, i64)> = Vec::new();
        let mut i = 0;
        while i < deltas.len() {
            let d = deltas[i];
            if d.0 == i64::MIN {
                i += 1;
                continue;
            }
            let mut j = i + 1;
            while j < deltas.len() && deltas[j] == d {
                j += 1;
            }
            run_lengths.push((j - i) as u32);
            run_deltas.push(d);
            i = j;
        }

        let total_in_runs: u64 = run_lengths.iter().map(|&l| l as u64).sum();

        // Cumulative by K.
        println!("  cumulative (ticks in runs of identical (addr_delta, val_delta) >= K):");
        for k in &[1u32, 4, 16, 64, 256, 1024, 4096, 16384, 65536] {
            let covered: u64 = run_lengths
                .iter()
                .filter(|&&l| l >= *k)
                .map(|&l| l as u64)
                .sum();
            println!(
                "    K = {:>6}: {:>14} ticks ({:>6.2}% of total, {:>6.2}% of in-runs)",
                k,
                covered,
                covered as f64 / ticks as f64 * 100.0,
                if total_in_runs > 0 {
                    covered as f64 / total_in_runs as f64 * 100.0
                } else { 0.0 }
            );
        }

        // Top-5 longest runs.
        let mut by_len: Vec<(u32, (i64, i64))> = run_lengths
            .iter()
            .zip(run_deltas.iter())
            .map(|(&l, &d)| (l, d))
            .collect();
        by_len.sort_by(|a, b| b.0.cmp(&a.0));
        println!("  top-5 longest runs of identical deltas:");
        for (i, (len, (da, dv))) in by_len.iter().take(5).enumerate() {
            println!(
                "    #{}: length={:>6}  addr_delta={:+}  val_delta={:+}",
                i + 1,
                len,
                da,
                dv
            );
        }

        // Top-5 delta tuples by total tick coverage.
        let mut by_delta: HashMap<(i64, i64), u64> = HashMap::new();
        for (&d, &l) in run_deltas.iter().zip(run_lengths.iter()) {
            *by_delta.entry(d).or_insert(0) += l as u64;
        }
        let mut dt: Vec<((i64, i64), u64)> = by_delta.into_iter().collect();
        dt.sort_by(|a, b| b.1.cmp(&a.1));
        println!("  top-5 (addr_delta, val_delta) tuples by total tick coverage:");
        for (i, (d, t)) in dt.iter().take(5).enumerate() {
            println!(
                "    #{}: addr_delta={:+}  val_delta={:+}  total_ticks={}  ({:.2}%)",
                i + 1,
                d.0,
                d.1,
                t,
                *t as f64 / ticks as f64 * 100.0
            );
        }
    }
}
