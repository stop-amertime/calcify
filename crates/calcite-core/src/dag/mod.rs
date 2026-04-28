//! Phase 1 dataflow DAG: explicit CFG + per-block op slices, plus a
//! walker backend that produces results identical to the bytecode
//! interpreter.
//!
//! See `docs/phase1-dag-ir.md` for design, `docs/compiler-mission.md`
//! § Phase 1 for context.

mod build;
mod normalise;
mod patterns;
mod types;
mod walker;

pub use build::build_dag;
pub use normalise::{
    normalise, normalise_with, Annotation, IdiomKind, NormalisedDag, Pattern,
};
pub use patterns::phase2_patterns;
pub use types::{BasicBlock, BlockId, BranchKind, Dag, OpIndex, Terminator};
pub use walker::dag_execute;
