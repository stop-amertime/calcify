# Phase 1 — DAG IR (design)

**Status:** in progress. Companion to
[`compiler-mission.md`](compiler-mission.md) § Phase 1 and
[`compiler-spec.md`](compiler-spec.md) § Phase 1 forward-looking notes.

## What this phase does

Build an explicit dataflow representation of a `CompiledProgram`'s op
stream and run it through a second evaluation backend that produces
identical results to the existing bytecode interpreter.

No perf gain expected. The DAG is the substrate Phase 2 will rewrite.
This phase exists to prove the substrate is correct.

## Source IR

Build the DAG from `Vec<Op>` (post-compile bytecode), not from `Expr`.

Reasoning:
- `Op` is already SSA-shaped: every op writes one `dst` slot. Definition
  is the op; use is any later op that reads the slot.
- Pattern lowerings (`DispatchChain`, broadcast-write, `MemoryFill`,
  `Call`, packed-byte read, etc.) already exist in the Op stream as
  super-ops. Building from `Op` gets them for free; building from `Expr`
  would re-derive them.
- The Op-stream's only non-pure construct is control flow
  (`BranchIfNotEqLit`, `Jump`, `DispatchChain`, `Dispatch`,
  `MemoryFill`/`MemoryCopy` with `exit_target`). We model control flow
  as a CFG of basic blocks; each block is itself a pure DAG.

## Node vocabulary

`DagNode` mirrors `Op` 1:1 for now. The two differences:

1. Operands are `NodeId` (graph edges), not `Slot` (flat-array indices).
   Two nodes share an operand iff they read the same slot's most-recent
   definition reaching them.
2. Branch terminators carry `BlockId` targets instead of raw PC indices.

This keeps the Phase 1 scope honest: same vocabulary, different
representation. Phase 2 collapses Op-shaped nodes into canonical pure
nodes (e.g. an `if(style())` cascade collapses to a `Select` node tree
with no branches).

## CFG construction

Standard textbook algorithm:

1. Walk `Vec<Op>`, mark every op that is a branch/jump target as a
   leader. The op at PC 0 is also a leader.
2. Each leader starts a basic block; the block runs through to either
   the next leader minus 1 OR a terminator op (anything that branches
   or jumps), whichever comes first.
3. Terminator ops record their successor block(s). Fall-through
   successors are the next sequential block.

Targets to extract:
- `BranchIfZero`, `BranchIfNotEqLit`, `LoadStateAndBranchIfNotEqLit`:
  two successors (taken-target, fallthrough).
- `Jump`: one successor (target).
- `DispatchChain`: many successors (one per chain entry + miss).
- `Dispatch`: two successors (table-hit flows fall through; miss jumps
  to `fallback_target`). Note: `Dispatch` writes to `dst` either way —
  it's a load with control-flow side effect on miss.
- `MemoryFill`/`MemoryCopy`: one successor (`exit_target`, which is the
  loop-exit block that fall-through eventually reaches in the
  un-optimised version). The bulk op is a tail call out of the loop.

## Walker semantics — Phase 1

For Phase 1 we accept a trivial walker:

> Visit basic blocks in their original linear order, evaluating each
> block's nodes in their original linear order, dispatching to the same
> `exec_ops`-equivalent arms. The terminator decides which block runs
> next. No DAG-level rewrites, no cross-block scheduling.

That walker produces bit-identical results to `exec_ops` because it
*is* `exec_ops` with an extra layer of indirection (block-then-op
instead of just op). The point is that the data-edge representation
exists and is sound — Phase 2's rewrites are what extract value from
it.

## What lives where

```
crates/calcite-core/src/dag/
  mod.rs           Public entry: build_dag(&CompiledProgram) -> Dag
  build.rs         CFG / SSA construction
  walker.rs        Backend that evaluates a Dag against State
  types.rs         DagNode, BlockId, NodeId, BlockTerminator
```

`Evaluator` gains a `backend: Backend` enum
(`Bytecode | Dag`), defaulted to `Bytecode`. Construction-time
selection — no per-tick branching cost on the hot path.

## What stays out of scope

- Cross-block code motion. Phase 1 evaluates blocks in source order.
- Dead-node elimination, common-subexpression elimination, constant
  propagation. Phase 2.
- Replacing the bytecode interpreter as the default. Phase 1 ships
  both; Phase 3 retires v1.
- Any change to `compile.rs`, `eval.rs`'s tick loop semantics, the
  pattern recognisers, or `State`. The new backend is purely additive.

## Acceptance gates

1. `cargo test -p calcite-core` green with default backend (Bytecode).
2. `cargo test -p calcite-core` green with `CALCITE_BACKEND=dag` set.
3. Phase 0.5 primitive conformance suite produces the same
   PASS/SKIP/XFAIL counts under both backends (41/5/3 today).
4. A new integration test runs a representative cabinet through both
   backends from the same snapshot and asserts bit-identical state
   after N ticks.
5. wasm build still succeeds.

## Out-of-band rules respected

- **Cardinal rule.** The DAG node vocabulary mirrors Op vocabulary,
  which mirrors CSS expression structure. No x86 knowledge enters.
- **Wasm safety.** No `std::time`, no threads, no fs/net. The walker
  is pure data-structure traversal.
- **Both targets agree.** Phase 1's PR runs the conformance suite
  under both backends on calcite-cli; the wasm build is exercised by
  the existing `wasm-pack build` step.
