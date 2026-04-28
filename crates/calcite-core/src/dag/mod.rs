//! Phase 1 dataflow DAG: explicit CFG + per-block op slices, plus a
//! walker backend that produces results identical to the bytecode
//! interpreter.
//!
//! See `docs/phase1-dag-ir.md` for design, `docs/compiler-mission.md`
//! § Phase 1 for context.

mod build;
mod types;
mod walker;

pub use build::build_dag;
pub use types::{BasicBlock, BlockId, BranchKind, Dag, OpIndex, Terminator};
pub use walker::dag_execute;
