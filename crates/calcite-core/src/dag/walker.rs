//! v2 DAG walker.
//!
//! The walker logic lives on `Evaluator::dag_v2_tick` in `eval.rs`, not
//! in this module. The reason is that walking a `FuncCall` requires
//! reaching back into v1's `eval_function_call` for parameter binding
//! (Phase 1 stub — see `docs/v2-rewrite-design.md` § State model).
//! Phase 2 will inline function bodies natively into the DAG and the
//! walker can move back into a free function in this module then.
//!
//! This file kept as a placeholder so the module path
//! `calcite_core::dag::walker` exists for future code to land into.
