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
pub mod closure_codegen;
pub mod inlined_codegen;
pub mod wasm_codegen;
// Native-only: the wasm host (wasmtime) doesn't compile for
// wasm32-unknown-unknown. The browser executes the codegen output
// via its built-in WebAssembly engine; that wiring lives in the
// calcite-wasm crate as a follow-up.
#[cfg(not(target_arch = "wasm32"))]
pub mod wasm_host;

pub use types::{
    Dag, DagBroadcast, DagNode, DagPackedBroadcast, NodeId, SlotId, StyleCondNode, TickPosition,
    TRANSIENT_BASE,
};
pub use closure_codegen::CompiledDag;
pub use inlined_codegen::InlinedDag;
pub use wasm_codegen::{TermDispatch, WasmDag, WasmSlab};
#[cfg(not(target_arch = "wasm32"))]
pub use wasm_host::WasmHost;

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
