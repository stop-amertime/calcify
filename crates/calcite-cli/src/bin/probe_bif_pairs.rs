//! Diagnostic probe: classify static adjacent BranchIfNotEqLit pairs in
//! the compiled program. Helps explain why `BranchIfNotEqLit ->
//! BranchIfNotEqLit` is 12.35% of runtime adjacencies despite the
//! `dispatch_chains` pass.
//!
//! Scans main.ops + every dispatch table entry + fallback + broadcast
//! value_ops + spillover + function bodies. For each adjacent pair,
//! reports:
//!   - same-slot (chain candidate that fell below MIN_CHAIN_LEN=6) vs
//!     different-slot (independent guards)
//!   - the longest run of same-slot BIfNELs starting at index i
//!   - whether the chain follows miss-target shape (next BIfNEL is at
//!     pc = current.target) or fall-through shape (next BIfNEL is at
//!     pc = i + 1, reached on equality)

use std::collections::HashMap;
use std::path::PathBuf;

use calcite_core::Evaluator;
use calcite_core::compile::Op;

#[derive(Default)]
struct Stats {
    total_bif_pairs: usize,
    same_slot_pairs: usize,
    diff_slot_pairs: usize,
    /// length of the same-slot fall-through chain starting here (only counted
    /// when the run can't be merged because chain_dispatch follows miss-targets)
    fallthrough_chain_lens: HashMap<usize, usize>,
    /// length of the same-slot miss-target chain starting here (these should
    /// have been collapsed by chain_dispatch — anything < 6 escaped)
    miss_chain_lens: HashMap<usize, usize>,
    /// number of bare same-slot adjacent pairs whose miss-target points
    /// elsewhere (i.e., not at op[i+1]).
    same_slot_not_chain_shape: usize,
}

fn classify_chain_shape(ops: &[Op], stats: &mut Stats) {
    let n = ops.len();
    let mut i = 0;
    while i + 1 < n {
        let (a0, val0, target0) = match &ops[i] {
            Op::BranchIfNotEqLit { a, val, target } => (*a, *val, *target),
            _ => { i += 1; continue; }
        };
        let (a1, _val1, _target1) = match &ops[i + 1] {
            Op::BranchIfNotEqLit { a, val, target } => (*a, *val, *target),
            _ => { i += 1; continue; }
        };
        stats.total_bif_pairs += 1;
        if a0 == a1 {
            stats.same_slot_pairs += 1;
            // Same slot. Chain shape requires miss_target of branch i to
            // point at the next branch in the chain. The dispatch_chains
            // pass walks miss-targets, not fall-through.
            //
            // Walk both shapes from this start and report longer of the two.
            // Miss-shape: starting at i, follow target chain.
            let mut miss_len = 1;
            let mut pc = i;
            loop {
                let (key, _v, t) = match &ops[pc] {
                    Op::BranchIfNotEqLit { a, val, target } => (*a, *val, *target),
                    _ => break,
                };
                if key != a0 { break; }
                let next = t as usize;
                if next >= n { break; }
                if let Op::BranchIfNotEqLit { a, .. } = &ops[next] {
                    if *a != a0 { break; }
                    miss_len += 1;
                    pc = next;
                } else { break; }
            }
            // Fall-through shape: at i+1, i+2, ... successive bare BIfNELs
            // on the same slot whose miss-target points elsewhere.
            let mut ft_len = 1;
            let mut j = i;
            loop {
                if j + 1 >= n { break; }
                let next_a = match &ops[j + 1] {
                    Op::BranchIfNotEqLit { a, .. } => *a,
                    _ => break,
                };
                if next_a != a0 { break; }
                ft_len += 1;
                j += 1;
            }
            // Categorise. If miss_len >= 6 the chains pass should have caught it
            // (sanity check — should never happen); else report both shapes.
            *stats.miss_chain_lens.entry(miss_len).or_insert(0) += 1;
            *stats.fallthrough_chain_lens.entry(ft_len).or_insert(0) += 1;

            if (target0 as usize) != i + 1 {
                stats.same_slot_not_chain_shape += 1;
            }
        } else {
            stats.diff_slot_pairs += 1;
        }
        i += 1;
    }
}

fn main() {
    let css_path = std::env::args().nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(
            "C:/Users/AdmT9N0CX01V65438A/Documents/src/CSS-DOS/doom8088.css"
        ));
    eprintln!("Parsing {}...", css_path.display());
    let css = std::fs::read_to_string(&css_path).expect("read");
    let parsed = calcite_core::parser::parse_css(&css).expect("parse");
    eprintln!("Compiling...");
    let evaluator = Evaluator::from_parsed(&parsed);
    let program = evaluator.compiled();

    let mut stats = Stats::default();

    eprintln!("Classifying main.ops ({} ops)...", program.ops.len());
    classify_chain_shape(&program.ops, &mut stats);

    eprintln!("Classifying {} dispatch tables...", program.dispatch_tables.len());
    let mut table_pairs = 0;
    let mut entries_classified = 0;
    for table in &program.dispatch_tables {
        for (_k, (entry_ops, _s)) in &table.entries {
            let before = stats.total_bif_pairs;
            classify_chain_shape(entry_ops, &mut stats);
            table_pairs += stats.total_bif_pairs - before;
            entries_classified += 1;
        }
        let before = stats.total_bif_pairs;
        classify_chain_shape(&table.fallback_ops, &mut stats);
        table_pairs += stats.total_bif_pairs - before;
    }
    eprintln!("  {} entries (+ fallbacks), {} pairs in dispatch sub-trees", entries_classified, table_pairs);

    eprintln!("Classifying broadcast value_ops + spillovers...");
    for bw in &program.broadcast_writes {
        classify_chain_shape(&bw.value_ops, &mut stats);
    }

    eprintln!();
    eprintln!("=== Static BranchIfNotEqLit -> BranchIfNotEqLit pairs ===");
    eprintln!("Total adjacent BIfNEL pairs: {}", stats.total_bif_pairs);
    eprintln!("  same-slot:   {} ({:.2}%)", stats.same_slot_pairs,
        (stats.same_slot_pairs as f64) * 100.0 / (stats.total_bif_pairs.max(1) as f64));
    eprintln!("  diff-slot:   {} ({:.2}%)", stats.diff_slot_pairs,
        (stats.diff_slot_pairs as f64) * 100.0 / (stats.total_bif_pairs.max(1) as f64));
    eprintln!("  same-slot but miss-target != i+1: {}", stats.same_slot_not_chain_shape);

    eprintln!();
    eprintln!("Same-slot MISS-shape chain length distribution:");
    eprintln!("  (chains_in_ops follows these; len >= 6 => collapsed)");
    let mut miss_lens: Vec<_> = stats.miss_chain_lens.iter().collect();
    miss_lens.sort_by_key(|(k, _)| *k);
    for (len, count) in &miss_lens {
        eprintln!("  len={:3}  count={}", len, count);
    }

    eprintln!();
    eprintln!("Same-slot FALL-THROUGH chain length distribution:");
    eprintln!("  (chains_in_ops does NOT follow these — they survive)");
    let mut ft_lens: Vec<_> = stats.fallthrough_chain_lens.iter().collect();
    ft_lens.sort_by_key(|(k, _)| *k);
    for (len, count) in &ft_lens {
        eprintln!("  len={:3}  count={}", len, count);
    }

    eprintln!();
    eprintln!("=== Inter-block: BIfNEL whose miss-target is another BIfNEL ===");
    let mut inter_main = 0usize;
    let mut inter_main_same_slot = 0usize;
    for (i, op) in program.ops.iter().enumerate() {
        if let Op::BranchIfNotEqLit { a, target, .. } = op {
            let t = *target as usize;
            if t == i + 1 { continue; } // adjacent already counted
            if t < program.ops.len() {
                if let Op::BranchIfNotEqLit { a: a2, .. } = &program.ops[t] {
                    inter_main += 1;
                    if *a2 == *a { inter_main_same_slot += 1; }
                }
            }
        }
    }
    eprintln!("main.ops: {} BIfNELs jump-to BIfNEL (non-adjacent target)", inter_main);
    eprintln!("  of which same-slot: {}", inter_main_same_slot);

    // Count unique BIfNEL→BIfNEL "edges" across all op arrays. Two pairs:
    //   (a) static adjacency in same op array (already counted above).
    //   (b) jump-edge: ops[i] is BIfNEL and ops[ops[i].target] is BIfNEL.
    let mut total_edges = 0usize;
    let mut total_same_slot_edges = 0usize;
    let mut count_edges = |ops: &[Op]| {
        let mut e = 0usize;
        let mut ss = 0usize;
        for (i, op) in ops.iter().enumerate() {
            if let Op::BranchIfNotEqLit { a, target, .. } = op {
                let t = *target as usize;
                // adjacency
                if t == i + 1 { continue; }
                if t < ops.len() {
                    if let Op::BranchIfNotEqLit { a: a2, .. } = &ops[t] {
                        e += 1;
                        if *a2 == *a { ss += 1; }
                    }
                }
            }
        }
        (e, ss)
    };
    let (e, ss) = count_edges(&program.ops);
    total_edges += e; total_same_slot_edges += ss;
    for table in &program.dispatch_tables {
        for (_k, (entry_ops, _s)) in &table.entries {
            let (e, ss) = count_edges(entry_ops);
            total_edges += e; total_same_slot_edges += ss;
        }
        let (e, ss) = count_edges(&table.fallback_ops);
        total_edges += e; total_same_slot_edges += ss;
    }
    eprintln!();
    eprintln!("All op arrays: {} cross-edge BIfNEL→BIfNEL ({} same-slot)",
        total_edges, total_same_slot_edges);

    // Total BIfNEL count
    let mut total_bifnel = 0usize;
    let mut count_bifnel = |ops: &[Op]| {
        ops.iter().filter(|o| matches!(o, Op::BranchIfNotEqLit { .. })).count()
    };
    total_bifnel += count_bifnel(&program.ops);
    let mut bifnels_in_dispatch = 0usize;
    for table in &program.dispatch_tables {
        for (_k, (entry_ops, _s)) in &table.entries {
            bifnels_in_dispatch += count_bifnel(entry_ops);
        }
        bifnels_in_dispatch += count_bifnel(&table.fallback_ops);
    }
    total_bifnel += bifnels_in_dispatch;
    eprintln!();
    eprintln!("Total BIfNEL ops: {} (main.ops: {}, dispatch sub-trees: {})",
        total_bifnel, count_bifnel(&program.ops), bifnels_in_dispatch);
    eprintln!("DispatchChain ops emitted: {} (i.e., {} chains collapsed of len>=6)",
        program.chain_tables.len(), program.chain_tables.len());

    // Per-chain length stats
    let mut chain_lens: Vec<usize> = program.chain_tables.iter().map(|t| t.entries.len()).collect();
    chain_lens.sort();
    if !chain_lens.is_empty() {
        let total: usize = chain_lens.iter().sum();
        eprintln!("Collapsed chain lengths: total entries = {}", total);
        eprintln!("  min={}  max={}  avg={:.1}",
            chain_lens[0], chain_lens[chain_lens.len()-1],
            (total as f64) / (chain_lens.len() as f64));
    }

    // Walk the program WITHOUT the MIN_CHAIN_LEN cutoff and see what chain
    // lengths the walker would have produced. This tells us how much we
    // are leaving on the table by raising / lowering MIN_CHAIN_LEN.
    let walk_chains = |ops: &[Op], lens: &mut Vec<usize>| {
        let n = ops.len();
        let mut i = 0;
        let mut seen = vec![false; n];
        while i < n {
            if seen[i] { i += 1; continue; }
            let (key, _v0, t0) = match &ops[i] {
                Op::BranchIfNotEqLit { a, val, target } => (*a, *val, *target),
                _ => { i += 1; continue; }
            };
            let mut entries = 1usize;
            let mut miss = t0;
            let mut chain_pcs = vec![i];
            loop {
                let nx = miss as usize;
                if nx >= n { break; }
                if let Op::BranchIfNotEqLit { a, target, .. } = &ops[nx] {
                    if *a != key { break; }
                    entries += 1;
                    miss = *target;
                    chain_pcs.push(nx);
                } else { break; }
            }
            for pc in &chain_pcs { seen[*pc] = true; }
            lens.push(entries);
            i += 1;
        }
    };
    let mut all_lens: Vec<usize> = Vec::new();
    walk_chains(&program.ops, &mut all_lens);
    for table in &program.dispatch_tables {
        for (_k, (entry_ops, _s)) in &table.entries {
            walk_chains(entry_ops, &mut all_lens);
        }
        walk_chains(&table.fallback_ops, &mut all_lens);
    }
    // Histogram
    let mut hist: HashMap<usize, usize> = HashMap::new();
    let mut total_entries_all = 0usize;
    let mut total_entries_short = 0usize;
    for &l in &all_lens {
        *hist.entry(l).or_insert(0) += 1;
        total_entries_all += l;
        if l < 6 { total_entries_short += l; }
    }
    let mut hk: Vec<_> = hist.iter().collect();
    hk.sort_by_key(|(k, _)| *k);
    eprintln!();
    eprintln!("=== If MIN_CHAIN_LEN were lowered: chain length histogram ===");
    eprintln!("(over ALL op arrays, deduplicated by walker)");
    for (l, c) in hk.iter().take(20) {
        eprintln!("  len={:3}  count={:6}  total_entries={}", l, c, **l * **c);
    }
    eprintln!();
    eprintln!("Total entries across all chains: {}", total_entries_all);
    eprintln!("Entries in chains of len < 6 (SURVIVE chains pass): {} ({:.1}%)",
        total_entries_short,
        (total_entries_short as f64) * 100.0 / (total_entries_all.max(1) as f64));

    // Diff-slot adjacent pairs: do they share a target (AND-guard) or not?
    let mut shared_target = 0usize;
    let mut diff_target = 0usize;
    let mut analyse_pairs = |ops: &[Op]| {
        let n = ops.len();
        for i in 0..n.saturating_sub(1) {
            let (a0, t0) = match &ops[i] {
                Op::BranchIfNotEqLit { a, target, .. } => (*a, *target),
                _ => continue,
            };
            let (a1, t1) = match &ops[i + 1] {
                Op::BranchIfNotEqLit { a, target, .. } => (*a, *target),
                _ => continue,
            };
            if a0 == a1 { continue; }
            if t0 == t1 { shared_target += 1; } else { diff_target += 1; }
        }
    };
    analyse_pairs(&program.ops);
    for table in &program.dispatch_tables {
        for (_k, (entry_ops, _s)) in &table.entries {
            analyse_pairs(entry_ops);
        }
        analyse_pairs(&table.fallback_ops);
    }
    eprintln!();
    eprintln!("=== Adjacent diff-slot BIfNEL pair analysis ===");
    eprintln!("  shared target (AND-guard shape): {}", shared_target);
    eprintln!("  different targets:               {}", diff_target);
}
