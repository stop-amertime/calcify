//! DAG walker — evaluates a `Dag` against `State` for one tick.
//!
//! See `docs/v2-rewrite-design.md` § State model for the semantics this
//! walker must reproduce. Phase 1 is a working evaluator that produces
//! bit-identical results to v1's bytecode interpreter on the corner
//! cases pinned in `tests/v2_tick_semantics.rs`.
//!
//! Phase 1 implementation status: stub. Walker logic lands in the
//! follow-on commit alongside the `Backend::DagV2` wiring.

use super::types::Dag;
use crate::State;

/// Walk a DAG once, mutating `State` to commit this tick's writes.
///
/// **Stub.** Returns immediately without evaluating. The first
/// real implementation will land alongside `Backend::DagV2` wiring
/// in `eval.rs` — at that point this stub is replaced.
pub fn walk(_dag: &Dag, _state: &mut State) {
    // Phase 1 stub. Real walker arrives in the next commit.
}
