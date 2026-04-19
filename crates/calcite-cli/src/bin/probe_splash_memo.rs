//! probe-splash-memo — measure state-vector recurrence during the splash fill.
//!
//! Answers: does the per-tick input state (state_vars + memory read set) recur
//! often enough that value-keyed tick memoisation would pay off?
//!
//! Runs `output/bootle-ctest.css` end-to-end until halt (memory[719359] != 0,
//! which is tick ~1,828,538). Per tick, captures:
//!   * snapshot of state.state_vars (before the tick)
//!   * the set of (addr, value) pairs that read_mem/read_mem16 returned
//! Then computes hit-rate statistics for several candidate key-subset
//! definitions and writes a JSON-ish report to stdout.
//!
//! This binary NEVER changes the interpreter's execution path. It only reads
//! from the optional `state.read_log` instrumentation added to State. The
//! memoisation cache itself is NOT built here — we just measure viability.
//!
//! Expect this binary to be slow: it allocates a fresh Vec per tick for the
//! read log and pushes ~50-200 entries into it, then hashes. For the splash
//! fill (~1.83M ticks) this takes several minutes in release mode.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "probe-splash-memo", about = "Measure tick-memoisation viability")]
struct Args {
    /// Path to the CSS file (default: ../../output/bootle-ctest.css relative to repo root).
    #[arg(short, long)]
    input: Option<PathBuf>,

    /// Halt when memory at this addr becomes non-zero.
    #[arg(long, default_value = "719359")]
    halt: i32,

    /// Max ticks before giving up (safety bound).
    #[arg(long, default_value = "3000000")]
    max_ticks: u32,

    /// Sample 1 in N ticks for detailed logging (full data kept for every tick;
    /// this just controls progress prints).
    #[arg(long, default_value = "100000")]
    progress_every: u32,

    /// Write per-tick hashes to this CSV file (for offline analysis). Large!
    #[arg(long)]
    dump_csv: Option<PathBuf>,
}

// ---- A small fast hasher (FNV-1a style, 64-bit) ----
// The standard `DefaultHasher` is SipHash and ~3x slower. For 1.83M ticks
// this matters. Collision probability at 2M inputs for a 64-bit hash is
// negligible (~10^-8 by birthday bound).

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

#[inline]
fn hash_pair_slice(vals: &[(i32, i32)]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222326;
    for &(a, v) in vals {
        h = fx_mix(h, (a as u32 as u64) ^ ((v as u32 as u64) << 32));
    }
    h
}

struct KeyStats {
    name: String,
    // hash -> tick count
    counts: HashMap<u64, u64>,
    // For memory-validated rate: among ticks where state-key recurred, how
    // many had a matching reads_hash? Indexed by state-key -> reads_hash ->
    // count. First occurrence of each (state_key, reads_hash) pair is the
    // "miss" baseline; subsequent occurrences are validated hits.
    reads_by_key: HashMap<u64, HashMap<u64, u64>>,
}

impl KeyStats {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            counts: HashMap::new(),
            reads_by_key: HashMap::new(),
        }
    }

    fn record(&mut self, state_key: u64, reads_key: u64) {
        *self.counts.entry(state_key).or_insert(0) += 1;
        *self
            .reads_by_key
            .entry(state_key)
            .or_insert_with(HashMap::new)
            .entry(reads_key)
            .or_insert(0) += 1;
    }

    fn report(&self, total_ticks: u64) {
        let unique = self.counts.len() as u64;
        let recurring_ticks: u64 = self.counts.values().filter(|&&c| c > 1).map(|&c| c - 1).sum();
        let recurrence_rate = recurring_ticks as f64 / total_ticks as f64 * 100.0;

        // Memory-validated hit rate: for each (state_key, reads_hash) bucket,
        // count-1 ticks are validated hits. Sum across buckets.
        let validated_hits: u64 = self
            .reads_by_key
            .values()
            .flat_map(|m| m.values())
            .map(|&c| c.saturating_sub(1))
            .sum();
        let validated_rate = validated_hits as f64 / total_ticks as f64 * 100.0;

        // Top 5 keys by frequency
        let mut top: Vec<(u64, u64)> = self.counts.iter().map(|(&k, &v)| (k, v)).collect();
        top.sort_by(|a, b| b.1.cmp(&a.1));
        top.truncate(5);

        // For each top key, how many distinct reads_hashes did it have?
        // And how many ticks does its biggest reads_hash bucket cover?
        println!("  [{}]", self.name);
        println!("    unique_state_keys: {}", unique);
        println!(
            "    recurring_ticks: {} ({:.2}% naive recurrence rate)",
            recurring_ticks, recurrence_rate
        );
        println!(
            "    validated_hits:  {} ({:.2}% memory-validated hit rate)",
            validated_hits, validated_rate
        );
        println!("    top 5 state keys:");
        for (i, (k, c)) in top.iter().enumerate() {
            let reads_map = &self.reads_by_key[k];
            let distinct_reads = reads_map.len();
            let max_bucket = reads_map.values().copied().max().unwrap_or(0);
            println!(
                "      #{}: key={:016x}  ticks={}  distinct_read_sets={}  biggest_bucket={}",
                i + 1,
                k,
                c,
                distinct_reads,
                max_bucket,
            );
        }
    }
}

fn main() {
    let args = Args::parse();

    let default_css =
        PathBuf::from("C:/Users/AdmT9N0CX01V65438A/Documents/src/calcite/output/bootle-ctest.css");
    let css_path = args.input.unwrap_or(default_css);

    eprintln!("probe-splash-memo");
    eprintln!("  css: {}", css_path.display());
    eprintln!("  halt addr: {}", args.halt);
    eprintln!("  max ticks: {}", args.max_ticks);

    // ---- Parse + compile ----
    let t0 = Instant::now();
    let css = std::fs::read_to_string(&css_path).expect("read css");
    eprintln!("  css bytes: {}", css.len());
    let parsed = calcite_core::parser::parse_css(&css).expect("parse");
    let mut state = calcite_core::State::default();
    state.load_properties(&parsed.properties);
    let mut evaluator = calcite_core::Evaluator::from_parsed(&parsed);
    eprintln!(
        "  parse+compile: {:.1}s, state vars: {}, memory bytes: {}",
        t0.elapsed().as_secs_f64(),
        state.state_var_count(),
        state.memory.len(),
    );

    let n_vars = state.state_var_count();
    let names = state.state_var_names.clone();

    // ---- First pass: discover iteration signature per state var ----
    // We need to decide which vars are "iterating" so we can build the excluded
    // variants. Cheapest: just record unique_count per slot during the main
    // pass too. We'll do ONE end-to-end run and compute everything from the
    // per-tick hashes we collect.

    // Pre-allocate per-tick arrays.
    let mut per_tick_state_hashes_full: Vec<u64> = Vec::with_capacity(args.max_ticks as usize);
    let mut per_tick_reads_hash: Vec<u64> = Vec::with_capacity(args.max_ticks as usize);

    // Track unique values per state var slot (for "iterating" detection).
    // Use small hash set per slot.
    let mut unique_per_slot: Vec<std::collections::HashSet<i32>> = (0..n_vars)
        .map(|_| std::collections::HashSet::new())
        .collect();

    // Flat buffer of every tick's pre-tick state_vars snapshot. Size:
    // 32 vars * 4 bytes * 1.83M ticks = ~234 MB. We need the raw values so
    // we can re-key for each variant without rerunning the splash.
    let mut flat_state_vars: Vec<i32> = Vec::with_capacity(n_vars * args.max_ticks as usize);

    // Enable read logging.
    *state.read_log.borrow_mut() = Some(Vec::with_capacity(256));

    let loop_start = Instant::now();
    let mut ticks: u32 = 0;
    let mut last_progress = Instant::now();

    loop {
        // Snapshot state_vars BEFORE the tick (that's the tick's input).
        flat_state_vars.extend_from_slice(&state.state_vars);
        let state_hash = hash_i32_slice(&state.state_vars);
        per_tick_state_hashes_full.push(state_hash);

        // Track unique values per slot.
        for (i, &v) in state.state_vars.iter().enumerate() {
            unique_per_slot[i].insert(v);
        }

        // Clear read log.
        {
            let mut borrow = state.read_log.borrow_mut();
            if let Some(ref mut log) = *borrow {
                log.clear();
            }
        }

        // Run one tick.
        evaluator.tick(&mut state);
        ticks += 1;

        // Hash the reads for this tick.
        let reads_hash = {
            let borrow = state.read_log.borrow();
            let log = borrow.as_ref().unwrap();
            hash_pair_slice(log)
        };
        per_tick_reads_hash.push(reads_hash);

        // Halt check.
        if state.read_mem(args.halt) != 0 {
            break;
        }
        if ticks >= args.max_ticks {
            eprintln!("  !!! hit max_ticks before halt");
            break;
        }

        if ticks % args.progress_every == 0 && last_progress.elapsed().as_secs_f64() > 5.0 {
            let tps = ticks as f64 / loop_start.elapsed().as_secs_f64();
            eprintln!(
                "  tick {} ({:.0} ticks/s, heap ~{} MB flat state vars)",
                ticks,
                tps,
                (flat_state_vars.len() * 4) / 1_000_000
            );
            last_progress = Instant::now();
        }
    }

    let loop_elapsed = loop_start.elapsed().as_secs_f64();
    eprintln!(
        "  halted at tick {} in {:.1}s ({:.0} ticks/s, instrumented)",
        ticks, loop_elapsed, ticks as f64 / loop_elapsed
    );
    eprintln!(
        "  unique per slot: {:?}",
        unique_per_slot
            .iter()
            .enumerate()
            .map(|(i, s)| (names[i].clone(), s.len()))
            .collect::<Vec<_>>()
    );

    // Disable read logging.
    *state.read_log.borrow_mut() = None;

    // ---- Build variants ----
    //
    // V1: all state vars
    // V2: exclude top-K most unique (likely iterators)
    // V3: include only vars with <= THRESHOLD unique values
    // V4: V2 + (DI & 0xFF) if DI exists
    //
    // Discover iterating slots.
    let mut slot_uniq: Vec<(usize, usize)> = unique_per_slot
        .iter()
        .enumerate()
        .map(|(i, s)| (i, s.len()))
        .collect();
    slot_uniq.sort_by(|a, b| b.1.cmp(&a.1));

    eprintln!("  top-8 most-unique slots:");
    for &(i, u) in slot_uniq.iter().take(8) {
        eprintln!("    {} ({}): {} unique", names[i], i, u);
    }

    // V2 exclusion set: slots with > 10 unique values
    let iterating: std::collections::HashSet<usize> = slot_uniq
        .iter()
        .filter(|&&(_, u)| u > 10)
        .map(|&(i, _)| i)
        .collect();
    let v2_slots: Vec<usize> = (0..n_vars).filter(|i| !iterating.contains(i)).collect();
    eprintln!(
        "  V2 kept slots ({}): {:?}",
        v2_slots.len(),
        v2_slots.iter().map(|&i| &names[i]).collect::<Vec<_>>()
    );

    // V3: only vars with <= 3 unique values (very stable state).
    let v3_slots: Vec<usize> = (0..n_vars)
        .filter(|&i| unique_per_slot[i].len() <= 3)
        .collect();

    // V4: V2 + low byte of DI (if DI exists). Implemented by adding a
    // synthetic value after the standard hash.
    let di_slot = names.iter().position(|n| n == "DI");

    // ---- Aggregate ----
    let mut v1 = KeyStats::new("V1_all_state_vars");
    let mut v2 = KeyStats::new("V2_exclude_iterators_10+");
    let mut v3 = KeyStats::new("V3_only_stable_vars_<=3_unique");
    let mut v4 = KeyStats::new("V4_V2_plus_DI_low_byte");

    let total = ticks as u64;
    for t in 0..ticks as usize {
        let row = &flat_state_vars[t * n_vars..(t + 1) * n_vars];
        let reads_h = per_tick_reads_hash[t];

        let k1 = hash_i32_slice(row);
        v1.record(k1, reads_h);

        let sub2: Vec<i32> = v2_slots.iter().map(|&s| row[s]).collect();
        let k2 = hash_i32_slice(&sub2);
        v2.record(k2, reads_h);

        let sub3: Vec<i32> = v3_slots.iter().map(|&s| row[s]).collect();
        let k3 = hash_i32_slice(&sub3);
        v3.record(k3, reads_h);

        let k4 = if let Some(di) = di_slot {
            let mut buf = sub2.clone();
            buf.push(row[di] & 0xFF);
            hash_i32_slice(&buf)
        } else {
            k2
        };
        v4.record(k4, reads_h);
    }
    // ---- Report ----
    println!("=== memoisation viability report ===");
    println!("total ticks: {}", total);
    println!("halt tick: {}", ticks);
    v1.report(total);
    v2.report(total);
    v3.report(total);
    v4.report(total);

    // Bonus: sanity-check determinism at a few top-V1 keys (if the SAME
    // state_key has multiple reads_hashes, something in the execution is
    // nondeterministic, which would be a serious finding).
    let mut nondet_keys_v1 = 0u64;
    for (_k, reads_map) in &v1.reads_by_key {
        if reads_map.len() > 1 {
            nondet_keys_v1 += 1;
        }
    }
    println!(
        "\nsanity: V1 state_keys with >1 distinct read-hash (indicates state-var key is insufficient): {}",
        nondet_keys_v1
    );

    if let Some(path) = args.dump_csv {
        let mut csv = String::new();
        csv.push_str("tick,state_hash,reads_hash\n");
        for t in 0..ticks as usize {
            csv.push_str(&format!(
                "{},{:016x},{:016x}\n",
                t, per_tick_state_hashes_full[t], per_tick_reads_hash[t]
            ));
        }
        std::fs::write(&path, csv).expect("write csv");
        eprintln!("  wrote CSV: {}", path.display());
    }
}
