//! Quick post-compile audit: count jumps whose targets still land on
//! another `Op::Jump`, and count "ops dynamically reachable as targets
//! of branches" before/after a hypothetical further shorten pass. Used
//! to verify S1.2's shorten_jumps pass left no chainable jumps behind.

use std::path::PathBuf;

use calcite_core::Evaluator;
use calcite_core::compile::Op;

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

fn audit_slice(name: &str, ops: &[Op]) {
    let n = ops.len();
    let mut total_jumps = 0u64;
    let mut chainable_jumps = 0u64;
    let mut total_branches = 0u64;
    let mut chainable_branch_targets = 0u64;
    let mut self_jumps = 0u64;

    let target_is_jump = |t: u32| -> bool {
        let pc = t as usize;
        if pc >= n { return false; }
        matches!(ops[pc], Op::Jump { .. })
    };

    for (i, op) in ops.iter().enumerate() {
        match op {
            Op::Jump { target } => {
                total_jumps += 1;
                if (*target as usize) == i {
                    self_jumps += 1;
                } else if target_is_jump(*target) {
                    chainable_jumps += 1;
                }
            }
            Op::BranchIfZero { target, .. }
            | Op::BranchIfNotEqLit { target, .. }
            | Op::LoadStateAndBranchIfNotEqLit { target, .. } => {
                total_branches += 1;
                if target_is_jump(*target) {
                    chainable_branch_targets += 1;
                }
            }
            Op::DispatchChain { miss_target, .. } => {
                total_branches += 1;
                if target_is_jump(*miss_target) {
                    chainable_branch_targets += 1;
                }
            }
            _ => {}
        }
    }

    eprintln!(
        "{:30}  ops:{:>10}  Jump:{:>9} (self:{:>5}, chainable→Jump:{:>6}) Branches:{:>9} (chainable→Jump:{:>6})",
        name, n, total_jumps, self_jumps, chainable_jumps, total_branches, chainable_branch_targets,
    );
}

fn main() {
    let path = doom_path();
    eprintln!("probe-jump-audit: cabinet = {}", path.display());
    let css = std::fs::read_to_string(&path).expect("read cabinet");
    let parsed = calcite_core::parser::parse_css(&css).expect("parse");
    eprintln!("Building evaluator...");
    let evaluator = Evaluator::from_parsed(&parsed);
    let program = evaluator.compiled();

    eprintln!();
    audit_slice("main", &program.ops);
    for (ti, table) in program.dispatch_tables.iter().enumerate() {
        for (k, (entry_ops, _)) in table.entries.iter().take(2) {
            audit_slice(&format!("dt[{}].entry({})", ti, k), entry_ops);
        }
        if !table.fallback_ops.is_empty() {
            audit_slice(&format!("dt[{}].fallback", ti), &table.fallback_ops);
        }
        if ti >= 4 { break; }
    }

    // Chain-table audit: for each chain table, count entries pointing at
    // a Jump op in the main slice. Distinguish chain tables actually
    // referenced from the main op stream (where the audit is meaningful)
    // from ones referenced only by sub-slices (where main's PCs are
    // unrelated and the result is meaningless).
    let mut chains_used_in_main: std::collections::HashSet<u32> =
        std::collections::HashSet::new();
    for op in &program.ops {
        if let Op::DispatchChain { chain_id, .. } = op {
            chains_used_in_main.insert(*chain_id);
        }
    }

    let mut chains_used_in_subslices: std::collections::HashSet<u32> =
        std::collections::HashSet::new();
    for table in &program.dispatch_tables {
        for (_k, (entry_ops, _)) in &table.entries {
            for op in entry_ops {
                if let Op::DispatchChain { chain_id, .. } = op {
                    chains_used_in_subslices.insert(*chain_id);
                }
            }
        }
        for op in &table.fallback_ops {
            if let Op::DispatchChain { chain_id, .. } = op {
                chains_used_in_subslices.insert(*chain_id);
            }
        }
    }
    for bw in &program.broadcast_writes {
        for op in &bw.value_ops {
            if let Op::DispatchChain { chain_id, .. } = op {
                chains_used_in_subslices.insert(*chain_id);
            }
        }
    }
    for func in &program.functions {
        for op in &func.body_ops {
            if let Op::DispatchChain { chain_id, .. } = op {
                chains_used_in_subslices.insert(*chain_id);
            }
        }
    }

    let mut main_total_entries = 0u64;
    let mut main_entries_to_jump = 0u64;
    let mut sub_total_entries = 0u64;
    let mut sub_entries_to_jump_by_main_pc = 0u64;
    let n = program.ops.len();
    for (id, table) in program.chain_tables.iter().enumerate() {
        let id = id as u32;
        let in_main = chains_used_in_main.contains(&id);
        let in_sub = chains_used_in_subslices.contains(&id);
        for (_k, body_pc) in table.entries.iter() {
            let pc = *body_pc as usize;
            let is_jump_in_main = pc < n && matches!(program.ops[pc], Op::Jump { .. });
            if in_main {
                main_total_entries += 1;
                if is_jump_in_main {
                    main_entries_to_jump += 1;
                }
            }
            if in_sub && !in_main {
                sub_total_entries += 1;
                if is_jump_in_main {
                    sub_entries_to_jump_by_main_pc += 1;
                }
            }
        }
    }
    eprintln!();
    eprintln!(
        "chain_tables: total={}  main-referenced={}  sub-only={}",
        program.chain_tables.len(),
        chains_used_in_main.len(),
        chains_used_in_subslices.len() - chains_used_in_main.intersection(&chains_used_in_subslices).count()
    );
    eprintln!(
        "  main chain entries: total={}  pointing at Jump in main: {}",
        main_total_entries, main_entries_to_jump
    );
    eprintln!(
        "  sub-only chain entries: total={}  (false-positive Jump-by-main-pc: {})",
        sub_total_entries, sub_entries_to_jump_by_main_pc
    );
}
