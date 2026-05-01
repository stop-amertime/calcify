//! v2 DAG walker.
//!
//! The walker logic lives on `Evaluator::dag_v2_tick` in `eval.rs`, not
//! in this module — the walker mutates `self` (cascade caches, call
//! frames, memo) while reading `&Dag`, which is awkward to express as a
//! free function over a pure-data DAG. Phase 3 may move it back here
//! once codegen replaces the interpretive walk.
//!
//! This file kept as a placeholder so the module path
//! `calcite_core::dag::walker` exists for future code to land into.
