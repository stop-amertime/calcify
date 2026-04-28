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
use crate::State;

/// Build a DAG from a parsed program.
///
/// `state` must already be populated via `State::load_properties` so the
/// slot map can resolve property names to negative state-var slots and
/// non-negative memory addresses.
pub fn build_dag(program: &ParsedProgram, state: &State) -> Dag {
    lowering::lower_parsed_program(program, state)
}
