//! Diagnostic probe: classify what op sits immediately before each isolated
//! `BranchIfNotEqLit { a: X, ... }`, and (for non-LoadState predecessors)
//! how far back the nearest `LoadState { dst: X, ... }` is — if any.
//!
//! Used to size the `LS_WINDOW` constant in `fuse_loadstate_branch`'s
//! widened scan and to confirm the brief's hypothesis about intervening
//! ops between LoadState and its matching branch.

use std::collections::HashMap;
use std::path::PathBuf;

use calcite_core::Evaluator;
use calcite_core::compile::Op;

fn op_kind(op: &Op) -> &'static str {
    match op {
        Op::LoadLit { .. } => "LoadLit",
        Op::LoadSlot { .. } => "LoadSlot",
        Op::LoadState { .. } => "LoadState",
        Op::LoadMem { .. } => "LoadMem",
        Op::LoadMem16 { .. } => "LoadMem16",
        Op::LoadPackedByte { .. } => "LoadPackedByte",
        Op::Add { .. } => "Add",
        Op::Sub { .. } => "Sub",
        Op::Mul { .. } => "Mul",
        Op::Div { .. } => "Div",
        Op::Mod { .. } => "Mod",
        Op::Neg { .. } => "Neg",
        Op::Abs { .. } => "Abs",
        Op::Sign { .. } => "Sign",
        Op::Pow { .. } => "Pow",
        Op::Min { .. } => "Min",
        Op::Max { .. } => "Max",
        Op::Clamp { .. } => "Clamp",
        Op::Round { .. } => "Round",
        Op::Floor { .. } => "Floor",
        Op::And { .. } => "And",
        Op::Shr { .. } => "Shr",
        Op::Shl { .. } => "Shl",
        Op::AddLit { .. } => "AddLit",
        Op::SubLit { .. } => "SubLit",
        Op::MulLit { .. } => "MulLit",
        Op::AndLit { .. } => "AndLit",
        Op::ShrLit { .. } => "ShrLit",
        Op::ShlLit { .. } => "ShlLit",
        Op::ModLit { .. } => "ModLit",
        Op::Bit { .. } => "Bit",
        Op::BitAnd16 { .. } => "BitAnd16",
        Op::BitOr16 { .. } => "BitOr16",
        Op::BitXor16 { .. } => "BitXor16",
        Op::BitNot16 { .. } => "BitNot16",
        Op::CmpEq { .. } => "CmpEq",
        Op::BranchIfZero { .. } => "BranchIfZero",
        Op::BranchIfNotEqLit { .. } => "BranchIfNotEqLit",
        Op::LoadStateAndBranchIfNotEqLit { .. } => "LoadStateAndBranchIfNotEqLit",
        Op::BranchIfNotEqLit2 { .. } => "BranchIfNotEqLit2",
        Op::Jump { .. } => "Jump",
        Op::DispatchChain { .. } => "DispatchChain",
        Op::Dispatch { .. } => "Dispatch",
        Op::DispatchFlatArray { .. } => "DispatchFlatArray",
        Op::Call { .. } => "Call",
        Op::StoreState { .. } => "StoreState",
        Op::StoreMem { .. } => "StoreMem",
        Op::MemoryFill { .. } => "MemoryFill",
        Op::MemoryCopy { .. } => "MemoryCopy",
        Op::ReplicatedBody { .. } => "ReplicatedBody",
    }
}

#[derive(Default)]
struct Stats {
    total_bif: usize,
    /// What op kind is at ops[i-1] for every BranchIfNotEqLit at ops[i].
    pred_kinds: HashMap<&'static str, usize>,
    /// For each BranchIfNotEqLit{a:X}, distance back (in ops) to the
    /// nearest `LoadState{dst:X}` in the same basic block (no jump/branch
    /// in between). 0 = adjacent. None counted in `no_loadstate_in_window`.
    ls_distance: HashMap<usize, usize>,
    no_loadstate_in_window: usize,
    /// Subset: branches where there IS a LoadState within window AND no
    /// disqualifying intervening op (rough estimate, no slot-aliasing check).
    fusable_naive: usize,
}

fn is_block_ender(op: &Op) -> bool {
    matches!(
        op,
        Op::BranchIfZero { .. }
            | Op::BranchIfNotEqLit { .. }
            | Op::LoadStateAndBranchIfNotEqLit { .. }
            | Op::BranchIfNotEqLit2 { .. }
            | Op::Jump { .. }
            | Op::DispatchChain { .. }
            | Op::Dispatch { .. }
            | Op::MemoryFill { .. }
            | Op::MemoryCopy { .. }
            | Op::Call { .. }
            | Op::StoreState { .. }
            | Op::StoreMem { .. }
    )
}

const SCAN_WINDOW: usize = 16;

fn classify(ops: &[Op], stats: &mut Stats) {
    for i in 0..ops.len() {
        let target_slot = match &ops[i] {
            Op::BranchIfNotEqLit { a, .. } => *a,
            _ => continue,
        };
        stats.total_bif += 1;
        // Predecessor kind.
        if i > 0 {
            *stats.pred_kinds.entry(op_kind(&ops[i - 1])).or_insert(0) += 1;
        }
        // Walk backwards looking for LoadState{dst:target_slot} within window,
        // stopping at any block-ender.
        let mut found_at: Option<usize> = None;
        for back in 1..=SCAN_WINDOW {
            if back > i { break; }
            let j = i - back;
            let op = &ops[j];
            if let Op::LoadState { dst, .. } = op {
                if *dst == target_slot {
                    found_at = Some(back - 1); // 0 = adjacent
                    break;
                }
            }
            if is_block_ender(op) { break; }
        }
        match found_at {
            Some(d) => {
                *stats.ls_distance.entry(d).or_insert(0) += 1;
                stats.fusable_naive += 1;
            }
            None => stats.no_loadstate_in_window += 1,
        }
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
    classify(&program.ops, &mut stats);

    eprintln!("Classifying {} dispatch tables...", program.dispatch_tables.len());
    for table in &program.dispatch_tables {
        for (_k, (entry_ops, _s)) in &table.entries {
            classify(entry_ops, &mut stats);
        }
        classify(&table.fallback_ops, &mut stats);
    }

    eprintln!("Classifying broadcast value_ops + spillovers...");
    for bw in &program.broadcast_writes {
        classify(&bw.value_ops, &mut stats);
        if let Some(ref sp) = bw.spillover {
            for (_k, (spill_ops, _s)) in &sp.entries {
                classify(spill_ops, &mut stats);
            }
        }
    }

    eprintln!();
    eprintln!("=== BranchIfNotEqLit predecessor classification ===");
    eprintln!("Total isolated BIfNEL ops:    {}", stats.total_bif);
    eprintln!("With LoadState{{dst:X}} in {}-window: {} ({:.2}%)",
        SCAN_WINDOW, stats.fusable_naive,
        (stats.fusable_naive as f64) * 100.0 / (stats.total_bif.max(1) as f64));
    eprintln!("Without:                       {}", stats.no_loadstate_in_window);

    eprintln!();
    eprintln!("Distance from BIfNEL{{a:X}} back to LoadState{{dst:X}}:");
    eprintln!("  (0 = adjacent — what the original peephole catches)");
    let mut dists: Vec<_> = stats.ls_distance.iter().collect();
    dists.sort_by_key(|(k, _)| *k);
    for (d, count) in &dists {
        eprintln!("  dist={:3}  count={}", d, count);
    }

    eprintln!();
    eprintln!("Predecessor op kinds for BIfNEL (ops[i-1]):");
    let mut preds: Vec<_> = stats.pred_kinds.iter().collect();
    preds.sort_by_key(|(_, c)| std::cmp::Reverse(**c));
    for (kind, count) in preds.iter().take(20) {
        eprintln!("  {:32}  {} ({:.1}%)", kind, count,
            (**count as f64) * 100.0 / (stats.total_bif.max(1) as f64));
    }
}
