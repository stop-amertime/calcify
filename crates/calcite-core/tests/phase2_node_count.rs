//! Phase 2 decision-gate metric: how many ops in a representative
//! cabinet get annotated by the Phase 2 pattern set?
//!
//! The mission doc's gate is "≥30 % node-count reduction on Doom."
//! Doom isn't in this worktree — `web/demo.css` is the only real
//! cabinet — so we use it as the proxy and report the fraction of ops
//! claimed by an annotation. An annotation typically replaces 1–2 ops
//! with one Phase 3 fast-path, so an annotation density of ~30 % is
//! roughly equivalent to a 30 % node-count reduction in Phase 3
//! emitted-code count.
//!
//! This test is informational — it prints the metric and never fails.
//! The decision-gate verdict is human-read from the printed numbers.
//! If `web/demo.css` is missing (fresh checkout), the test no-ops.

use std::path::PathBuf;

use calcite_core::dag::{build_dag, normalise, phase2_patterns};
use calcite_core::parser::parse_css;
use calcite_core::Evaluator;

fn worktree_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

fn doom_cabinet_path() -> PathBuf {
    // Try a few likely locations for the doom8088 cabinet:
    // - $CALCITE_DOOM_CSS env override
    // - CSS-DOS sibling repo
    // - user-level src tree (this dev box)
    if let Ok(env_path) = std::env::var("CALCITE_DOOM_CSS") {
        return PathBuf::from(env_path);
    }
    // CARGO_MANIFEST_DIR = .../calcite[.../.claude/worktrees/<name>]/crates/calcite-core
    // Walk up to find the parent of the calcite checkout, then look at
    // ../CSS-DOS/doom8088.css.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for ancestor in manifest.ancestors() {
        let candidate = ancestor.join("CSS-DOS").join("doom8088.css");
        if candidate.is_file() {
            return candidate;
        }
        let parent_candidate = ancestor.parent().map(|p| p.join("CSS-DOS").join("doom8088.css"));
        if let Some(c) = parent_candidate {
            if c.is_file() { return c; }
        }
    }
    PathBuf::from("CSS-DOS/doom8088.css") // sentinel: pick_cabinet checks is_file()
}

fn pick_cabinet() -> Option<PathBuf> {
    let doom = doom_cabinet_path();
    if doom.is_file() { return Some(doom); }
    let demo = worktree_root().join("web").join("demo.css");
    if demo.is_file() { return Some(demo); }
    None
}

#[test]
fn phase2_annotation_density_on_demo_cabinet() {
    let cabinet = match pick_cabinet() {
        Some(p) => p,
        None => {
            eprintln!("phase2_annotation_density: skipping — no cabinet found");
            return;
        }
    };
    eprintln!("phase2_annotation_density: cabinet = {}", cabinet.display());
    let css = std::fs::read_to_string(&cabinet).expect("read cabinet");
    let parsed = parse_css(&css).expect("parse cabinet css");
    let evaluator = Evaluator::from_parsed(&parsed);
    let program = evaluator.compiled();

    let dag = build_dag(program);
    let patterns = phase2_patterns();

    // Re-run normalise to get the metric (the public `normalise` entry
    // point uses the same pattern set; we also count per-pattern hits
    // for diagnostic value).
    let norm = normalise(program, &dag);
    let total_ops = program.ops.len();
    let annotated = norm.count();
    let density = if total_ops == 0 { 0.0 } else { annotated as f64 / total_ops as f64 };

    // Per-pattern tally — useful when debugging "why didn't X fire?"
    let mut per_pattern_counts: std::collections::HashMap<&'static str, usize> =
        std::collections::HashMap::new();
    for ann in &norm.annotations {
        let label = match ann.kind {
            calcite_core::dag::IdiomKind::Hold => "hold",
            calcite_core::dag::IdiomKind::BitField { .. } => "bitfield",
            calcite_core::dag::IdiomKind::RepeatedLoad { .. } => "repeated-load",
            calcite_core::dag::IdiomKind::ConstFold { .. } => "const-fold",
        };
        *per_pattern_counts.entry(label).or_insert(0) += 1;
    }

    eprintln!(
        "Phase 2 annotation density on {} ops:\n  annotated: {} ({:.1} %)\n  patterns:  {:?}\n  pattern set size: {}",
        total_ops,
        annotated,
        density * 100.0,
        per_pattern_counts,
        patterns.len(),
    );

    // Op-kind census — what the DAG actually contains. Tells us which
    // patterns to write next.
    use calcite_core::compile::Op;
    let mut kind_counts: std::collections::HashMap<&'static str, usize> =
        std::collections::HashMap::new();
    for op in &program.ops {
        let label = match op {
            Op::LoadLit { .. } => "LoadLit",
            Op::LoadSlot { .. } => "LoadSlot",
            Op::LoadState { .. } => "LoadState",
            Op::LoadMem { .. } => "LoadMem",
            Op::LoadMem16 { .. } => "LoadMem16",
            Op::LoadPackedByte { .. } => "LoadPackedByte",
            Op::Add { .. } => "Add",
            Op::AddLit { .. } => "AddLit",
            Op::Sub { .. } => "Sub",
            Op::SubLit { .. } => "SubLit",
            Op::Mul { .. } => "Mul",
            Op::MulLit { .. } => "MulLit",
            Op::Div { .. } => "Div",
            Op::Mod { .. } => "Mod",
            Op::ModLit { .. } => "ModLit",
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
            Op::AndLit { .. } => "AndLit",
            Op::Shr { .. } => "Shr",
            Op::ShrLit { .. } => "ShrLit",
            Op::Shl { .. } => "Shl",
            Op::ShlLit { .. } => "ShlLit",
            Op::Bit { .. } => "Bit",
            Op::BitAnd16 { .. } => "BitAnd16",
            Op::BitOr16 { .. } => "BitOr16",
            Op::BitXor16 { .. } => "BitXor16",
            Op::BitNot16 { .. } => "BitNot16",
            Op::CmpEq { .. } => "CmpEq",
            Op::BranchIfZero { .. } => "BranchIfZero",
            Op::BranchIfNotEqLit { .. } => "BranchIfNotEqLit",
            Op::LoadStateAndBranchIfNotEqLit { .. } => "LoadStateAndBranchIfNotEqLit",
            Op::Jump { .. } => "Jump",
            Op::DispatchChain { .. } => "DispatchChain",
            Op::Dispatch { .. } => "Dispatch",
            Op::DispatchFlatArray { .. } => "DispatchFlatArray",
            Op::Call { .. } => "Call",
            Op::StoreState { .. } => "StoreState",
            Op::StoreMem { .. } => "StoreMem",
            Op::MemoryFill { .. } => "MemoryFill",
            Op::MemoryCopy { .. } => "MemoryCopy",
        };
        *kind_counts.entry(label).or_insert(0) += 1;
    }
    let mut sorted: Vec<_> = kind_counts.iter().collect();
    sorted.sort_by_key(|(_, c)| std::cmp::Reverse(**c));
    eprintln!("\nOp-kind census (top 15 of {} kinds):", sorted.len());
    for (kind, count) in sorted.iter().take(15) {
        let pct = **count as f64 / total_ops as f64 * 100.0;
        eprintln!("  {:>30}: {:>10}  ({:>5.1} %)", kind, count, pct);
    }
    eprintln!("\nDispatch-chain count: {}", program.chain_tables.len());
    eprintln!("Dispatch-table count: {}", program.dispatch_tables.len());
    eprintln!("Flat-dispatch arrays: {}", program.flat_dispatch_arrays.len());
    eprintln!("Function bodies:      {}", program.functions.len());
    eprintln!("Broadcast writes:     {}", program.broadcast_writes.len());
    eprintln!("Packed broadcast:     {}", program.packed_broadcast_writes.len());
    eprintln!("Total slots:          {}", program.slot_count);
    eprintln!("DAG blocks:           {}", dag.blocks.len());
}
