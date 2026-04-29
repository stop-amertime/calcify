//! S1.3 diagnostic: classify every `Op::BranchIfNotEqLit` that didn't
//! get absorbed into a `Op::DispatchChain` to decide whether closing
//! that gap is worth pursuing.
//!
//! Categories per BranchIfNotEqLit at PC i with target T:
//!   * "head of short chain" — would be the start of a same-slot
//!     run, but the run length didn't hit MIN_CHAIN_LEN. The whole
//!     run is reported under the head's count.
//!   * "tail of short chain" — already counted by its head; counted
//!     here only for visibility.
//!   * "isolated, target=non-branch" — single test, miss target lands
//!     on something that isn't a BranchIfNotEqLit. Can't chain even
//!     in principle.
//!   * "isolated, target=different-slot branch" — chain genuinely
//!     tests multiple slots.
//!   * "isolated, target=same-slot branch" — would chain into a head
//!     above; only reported as a sanity check.

use std::path::PathBuf;

use calcite_core::Evaluator;
use calcite_core::compile::{CompiledProgram, Op, Slot};

fn doom_path() -> PathBuf {
    if let Ok(p) = std::env::var("CALCITE_DOOM_CSS") {
        return PathBuf::from(p);
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for ancestor in manifest.ancestors() {
        let c = ancestor.join("CSS-DOS").join("doom8088.css");
        if c.is_file() { return c; }
        if let Some(p) = ancestor.parent() {
            let c = p.join("CSS-DOS").join("doom8088.css");
            if c.is_file() { return c; }
        }
    }
    PathBuf::from("doom8088.css")
}

fn classify(program: &CompiledProgram) {
    let ops = &program.ops;
    let n = ops.len();

    // Build a "head of chain?" map. A BranchIfNotEqLit at PC i is the
    // head of a (potential) chain if the previous op (at PC i-1) is
    // either NOT a BranchIfNotEqLit, OR is one whose miss-target isn't
    // i, OR tests a different slot. Equivalently: it's a head if no
    // earlier same-slot BranchIfNotEqLit miss-targets to i.
    //
    // Simplification: walk the ops once; for every BranchIfNotEqLit at
    // PC i, if its target T is a same-slot BranchIfNotEqLit, flag T as
    // "tail" (not a head). Anything not flagged is a head.
    let mut is_tail = vec![false; n];
    for (i, op) in ops.iter().enumerate() {
        if let Op::BranchIfNotEqLit { a, target, .. } = op {
            let t = *target as usize;
            if t < n {
                if let Op::BranchIfNotEqLit { a: a2, .. } = &ops[t] {
                    if *a == *a2 {
                        is_tail[t] = true;
                    }
                }
            }
            let _ = i;
        }
    }

    // Walk again counting categories.
    const MIN_CHAIN_LEN: usize = 6;
    let mut chain_lengths: std::collections::HashMap<usize, u64> = std::collections::HashMap::new();
    let mut isolated_target_jump = 0u64;
    let mut isolated_target_different_slot_branch = 0u64;
    let mut isolated_target_other_op = 0u64;
    let mut isolated_target_offstream = 0u64;
    let mut head_count = 0u64;
    let mut tail_count = 0u64;
    let mut total_branches = 0u64;
    let mut total_unchained_branches = 0u64;
    let mut chained_runs_total = 0u64;
    let mut chained_branches_in_runs = 0u64;

    for i in 0..n {
        let (a, target) = match &ops[i] {
            Op::BranchIfNotEqLit { a, val: _, target } => (*a, *target),
            _ => continue,
        };
        total_branches += 1;
        if is_tail[i] {
            tail_count += 1;
            continue;
        }
        head_count += 1;
        // Walk the run starting at i: count entries.
        let mut run_len = 1usize;
        let mut cur_target = target;
        loop {
            let pc = cur_target as usize;
            if pc >= n { break; }
            match &ops[pc] {
                Op::BranchIfNotEqLit { a: a2, target: t2, .. } if *a2 == a => {
                    run_len += 1;
                    cur_target = *t2;
                }
                _ => break,
            }
        }
        if run_len >= MIN_CHAIN_LEN {
            // This shouldn't happen post-build_dispatch_chains; but if
            // it does, treat it as a residual.
            chained_runs_total += 1;
            chained_branches_in_runs += run_len as u64;
            continue;
        }
        *chain_lengths.entry(run_len).or_insert(0) += 1;
        total_unchained_branches += run_len as u64;

        if run_len == 1 {
            // Categorise the miss target.
            let pc = target as usize;
            if pc >= n {
                isolated_target_offstream += 1;
            } else {
                match &ops[pc] {
                    Op::Jump { .. } => isolated_target_jump += 1,
                    Op::BranchIfNotEqLit { a: a2, .. } => {
                        if *a2 == a {
                            // Shouldn't happen since we'd have run_len > 1
                            // — but guard anyway.
                            isolated_target_other_op += 1;
                        } else {
                            isolated_target_different_slot_branch += 1;
                        }
                    }
                    _ => isolated_target_other_op += 1,
                }
            }
        }
    }

    let total = total_branches as f64;
    eprintln!();
    eprintln!("=== BranchIfNotEqLit classification (doom8088, post-compile) ===");
    eprintln!("  total BranchIfNotEqLit ops:    {}", total_branches);
    eprintln!("  heads (start of run):          {}", head_count);
    eprintln!("  tails (already in a run):      {}", tail_count);
    if chained_runs_total > 0 {
        eprintln!(
            "  residual chainable runs (>= {}): {} runs, {} branches  — bug: should be 0",
            MIN_CHAIN_LEN, chained_runs_total, chained_branches_in_runs
        );
    }
    eprintln!();
    eprintln!("Run-length histogram for unchained heads (run_len < {}):", MIN_CHAIN_LEN);
    let mut lengths: Vec<_> = chain_lengths.iter().collect();
    lengths.sort_by_key(|(l, _)| **l);
    for (len, count) in lengths {
        let branches = (*len as u64) * *count;
        let pct = branches as f64 * 100.0 / total;
        eprintln!(
            "  run_len={}: {:>10} runs  ({:>10} branches, {:>5.1} % of all BranchIfNotEqLit)",
            len, count, branches, pct,
        );
    }
    eprintln!();
    eprintln!("Total unchained branches (all runs len < {}): {} ({:.1} % of all BranchIfNotEqLit)",
        MIN_CHAIN_LEN, total_unchained_branches, total_unchained_branches as f64 * 100.0 / total);
    eprintln!();
    eprintln!("Isolated (run_len=1) miss-target categories:");
    let iso_total = isolated_target_jump + isolated_target_different_slot_branch
        + isolated_target_other_op + isolated_target_offstream;
    let pct_iso = |v: u64| -> f64 {
        if iso_total == 0 { 0.0 } else { v as f64 * 100.0 / iso_total as f64 }
    };
    eprintln!("  → Jump (post-shorten this should be ~0): {} ({:.1} % of isolated)", isolated_target_jump, pct_iso(isolated_target_jump));
    eprintln!("  → different-slot BranchIfNotEqLit:        {} ({:.1} %)", isolated_target_different_slot_branch, pct_iso(isolated_target_different_slot_branch));
    eprintln!("  → some other op (intervening / end):      {} ({:.1} %)", isolated_target_other_op, pct_iso(isolated_target_other_op));
    eprintln!("  → off-stream (program end):               {} ({:.1} %)", isolated_target_offstream, pct_iso(isolated_target_offstream));

    // Try a what-if: lower MIN_CHAIN_LEN to 2, 3, 4, 5 and see how many
    // branches the existing recogniser would absorb if it lowered its
    // threshold. This tells us whether tightening MIN_CHAIN_LEN is the
    // win or whether a different strategy is needed.
    eprintln!();
    eprintln!("What-if: lowering MIN_CHAIN_LEN");
    for threshold in [2usize, 3, 4, 5] {
        let mut absorbed_branches = 0u64;
        let mut runs_added = 0u64;
        for (len, count) in chain_lengths.iter() {
            if *len >= threshold {
                runs_added += *count;
                absorbed_branches += (*len as u64) * *count;
            }
        }
        let pct = absorbed_branches as f64 * 100.0 / total;
        eprintln!(
            "  threshold={}: would absorb {} branches in {} runs ({:.1} % of all BranchIfNotEqLit)",
            threshold, absorbed_branches, runs_added, pct
        );
    }

    // Used-slot diversity sanity check: how many distinct slots appear
    // as the `a` of an unchained BranchIfNotEqLit? Tells us whether
    // there's a small set of "key slots" worth indexing.
    let mut slots_used = std::collections::HashMap::<Slot, u64>::new();
    for op in ops {
        if let Op::BranchIfNotEqLit { a, .. } = op {
            *slots_used.entry(*a).or_insert(0) += 1;
        }
    }
    eprintln!();
    eprintln!("Distinct `a` slots across all BranchIfNotEqLit: {}", slots_used.len());
    let mut top: Vec<_> = slots_used.iter().collect();
    top.sort_by_key(|(_, c)| std::cmp::Reverse(**c));
    eprintln!("Top 10 most-tested slots:");
    for (s, c) in top.iter().take(10) {
        eprintln!("  slot {}: {} BranchIfNotEqLit ops", s, c);
    }
}

fn main() {
    let path = doom_path();
    eprintln!("probe-branch-chains: cabinet = {}", path.display());
    let css = std::fs::read_to_string(&path).expect("read");
    let parsed = calcite_core::parser::parse_css(&css).expect("parse");
    let evaluator = Evaluator::from_parsed(&parsed);
    let program = evaluator.compiled();
    eprintln!(
        "  main ops: {}, chain tables: {}",
        program.ops.len(),
        program.chain_tables.len(),
    );
    classify(program);
}
