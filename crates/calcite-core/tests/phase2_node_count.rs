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

#[test]
fn phase2_annotation_density_on_demo_cabinet() {
    let cabinet = worktree_root().join("web").join("demo.css");
    if !cabinet.is_file() {
        eprintln!(
            "phase2_annotation_density: skipping — no cabinet at {}",
            cabinet.display()
        );
        return;
    }
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
}
