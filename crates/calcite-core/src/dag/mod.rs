//! v2 DAG IR — Phase 1 substrate for the calcite-v2 clean-rewrite stream.
//!
//! See `docs/v2-rewrite-design.md` for the design contract. This module
//! is parallel to v1's `compile`/`eval` path: it lowers a `ParsedProgram`
//! to a per-tick dataflow DAG and walks the DAG to mutate `State`.
//!
//! Phase 1 ships dormant: activated only when `Evaluator` is constructed
//! with `Backend::DagV2`. Default stays `Backend::Bytecode` until Phase 3.

pub mod types;
pub mod lowering;
pub mod walker;

pub use types::{Dag, DagNode, NodeId, SlotId, StyleCondNode, TickPosition};

use crate::types::ParsedProgram;

/// Build a DAG from a parsed program.
///
/// Slot resolution defers to `eval::property_to_address`, which reads
/// the global address map populated by `State::load_properties`. The
/// caller must therefore call `State::load_properties` before
/// `build_dag` — the same ordering v1's `Evaluator::from_parsed`
/// already requires.
pub fn build_dag(program: &ParsedProgram) -> Dag {
    lowering::lower_parsed_program(program)
}
