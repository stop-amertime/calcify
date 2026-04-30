# v2 flat-walker A/B implementation brief

You (future me) are picking this up after context compaction. This is
the entire briefing. Read it before touching code.

## What's already done before you start

- The session that wrote this brief reorganised `CLAUDE.md` (both main
  and this worktree's copy). Read it. The "Real carts are the test
  suite" section is the rule that drives every measurement below.
- Working tree clean at commit `1afc685` ("core: v2 DAG walker —
  Phase 2 correctness landed"). That commit is the **rollback oracle**.
  If a step breaks something, `git reset --hard 1afc685` and read the
  diff — don't add runtime feature flags to keep the broken thing live.

## The problem

v2 DAG walker (`Evaluator::dag_v2_tick` at `eval.rs:1786`, recursive
over `DagNode` enum) is correctness-clean for ≥2000 ticks of
`hello-text.css` against v1, but ~18.5× slower than v1's flat-bytecode
walker (27K vs 509K t/s on that micro-fixture). The structural cause
is recursive tree walk over an enum vs. v1's flat `Vec<Op>` over an
i32 slot array.

**Note the testing pitfall**: `hello-text.css` is the *only* number
the user got to that 18.5× figure. Per the new CLAUDE.md rule, that
number is debug telemetry, not evidence of where the gap actually is.
Real-cart numbers might tell a different story (e.g. broadcast paths
might dominate, in which case flat-walker wins look smaller). Get a
real-cart baseline before writing code.

## The plan, agreed with user

A/B-gated stacking sequence. Tree walker stays alive in the binary
behind `CALCITE_V2_FLAT=0`/`=1` until step 1 ships green. Each step
is its own commit. Earlier steps unlock later ones.

1. **Flatten the DAG to `Vec<Op>` and walk iteratively.** Big lever.
   Estimated 3–5×. Tree walker stays as oracle.
2. **Specialised Load ops** — `LoadStateVar` / `LoadMemory` /
   `LoadTransient` / `LoadCommitted`, baked at lowering. Removes the
   slot-id range dispatch from the inner loop.
3. **Peephole fuse `Calc + Lit / LoadVar`** — port v1's
   `AddLit` / `SubLit` / `AndLit` / `ShrLit` etc. (each was 5–15% in
   the 2026-04-16 round; see `docs/log.md`).
4. **`DispatchFlatArray` for `Switch`** — dense-array fast path; was
   v1's single biggest win.
5. **Drop the tree walker** and the bridge scaffolding (`name_to_slot`,
   `absorbed_properties`, the `FuncCall` variant — see
   `docs/v2-self-review-2026-04-29.md` for why these are debt).
6. **Bigger structural moves** (16-bit bitwise recognition,
   change-gated ops, affine self-loop) — deferred until 1–5 land.

Out of scope right now: slot-enum rework, splitting numeric/string IRs,
node-count probe (#22).

## Step 1 in detail (the only step you should plan for in detail today)

Goal: replace the recursive `dag_v2_tick` with an iterative walk over
a flat op stream, and demonstrate ≥3× speedup on the bench-of-record
(zork via bench-web.mjs) without correctness regressions.

### Architecture

Add a parallel flat `Vec<DagOp>` representation alongside `DagNode`.
**Don't replace `DagNode` yet** — lowering keeps producing the tree;
a new pass linearises tree → flat ops. This keeps the diff manageable
and lets the tree walker stay live through the transition.

The op vocabulary mirrors v1's `Op` shape (linear, slot-indexed)
adapted to v2's three-tier slot space (state-var negative, memory
non-negative under `TRANSIENT_BASE`, transient at/above
`TRANSIENT_BASE`). Per-walk memo becomes a slot-indexed scratch array
instead of a node-indexed memo (no enum dispatch in the hot path).

### The Call/Param question (decided)

User and I weighed two flavours:

- **(a) Escape-hatch**: keep `Call`/`Param` as "call out to tree walker
  for this subtree". Flatten the easy 90%, leave function-body walks
  recursive. Lands step 1 in days. Step 5 cleans up after the ceiling
  becomes the long tail of recursive calls.
- **(b) Full flatten**: function bodies as separate op streams indexed
  by `fn_id`; new `CallOp { fn_id, arg_op_ids }` pushes a frame; new
  `LoadParam(i)` reads from the frame's pre-evaluated args. Strictly
  better end-state, more upfront work, more risk of correctness drift
  in the lowering.

**Go with (a) for step 1.** Reason: flattening function bodies adds a
second axis of structural change to a step that's already
restructuring the walker. Land the walker change first, prove it on
real carts, then collapse the escape hatch as its own commit. (b) is
the right end-state but does not need to ship in one go.

### Implementation sketch

1. New module `crates/calcite-core/src/dag/flat.rs`. Defines
   `enum DagOp` (start with the obvious set: `LoadLit`, `LoadVar`,
   `Calc{Add,Sub,Mul,Div,Mod,Pow,Min,Max,Clamp,Round,Abs,Neg,Sign}`,
   `IfBranch{cond_slot, target}`, `Jump{target}`, `Switch{key_slot,
   table_id, fallback}`, `WriteVar`, `IndirectStore{port_id, packed}`,
   `Concat`, `CallTreeFallback{node_id}`). Each op writes to one
   walker-scratch slot, reads from 0–N walker-scratch slots, and
   advances PC.
2. New pass: `Dag::lower_to_flat()` builds two parallel structures —
   `Vec<DagOp>` (terminals concatenated, with `WriteVar` at terminal
   boundaries) and a slot allocator that gives every interior node a
   walk-scratch slot id. Inputs are reused (memoisation = same slot).
   Variadic Min/Max get an `args_offset, n` into a side `Vec<u32>`.
3. New walker: `dag_v2_tick_flat` (sibling of `dag_v2_tick`, not a
   replacement). Same `&mut State` signature. Iterative `while pc <
   ops.len()` match over `DagOp`. State writeback in the same three
   phases as today (scalar WriteVars, then commit, then broadcasts).
4. Gate at the call sites in `eval.rs` (the two `self.dag_v2_tick(state)`
   lines, ~621 and ~756): if `CALCITE_V2_FLAT=1`, call
   `dag_v2_tick_flat`. Read the env var **once at Evaluator
   construction** and cache as a bool field — env reads in hot paths
   are forbidden on wasm (per CLAUDE.md wasm safety).
5. `CallTreeFallback` op delegates one node to the existing
   `dag_eval_node` so `Call`/`FuncCall`/`Param` keep working without
   needing flat-side function bodies. Parameter resolution still uses
   the existing `call_stack: Vec<Vec<i32>>` on the Evaluator; the flat
   walker pre-evaluates args via flat ops, then pushes a frame and
   delegates the body walk to the tree walker.

### Pitfalls to watch

These are the failure modes I think you'll hit. Each has an answer
that is *not* "add an env var to disable the optimisation".

- **`StyleCondNode` is a sub-AST attached to `If` branches, not a
  `DagNode`.** It can contain nested And/Or, and its LHS is a
  `NodeId`. The flat lowering of `If` needs to lower the predicate to
  flat ops *before* the branch op (so the branch consumes a slot), or
  emit a small bytecode for the predicate inline. Easiest: lower each
  predicate to a sequence of `Eq`/`And`/`Or` ops writing a 0/1 to a
  scratch slot, then `IfBranch{cond_slot, target}`. Don't let
  `StyleCondNode` leak into the flat op vocabulary.
- **Phase boundaries.** The current walker runs three phases: scalar
  WriteVars (cascade caches), writeback to State, broadcasts. Flat
  ops can't reorder across phase boundaries without changing
  semantics. Solution: emit the op stream in three contiguous segments
  with explicit phase-boundary markers, or run three flat walks (one
  per phase) and reuse the scratch slots between them.
- **Variadic `Min`/`Max`.** Don't allocate a `Vec` per op invocation.
  Side table indexed by op argument; walker reads `n` slots and folds.
- **`LitStr`/`Concat`.** Numeric eval returns 0 for both. Keep them
  as their own ops (`LoadLitStr`, `Concat{n_args}`) but only the
  display path (textBuffer rendering) actually consumes their string
  values. Most flat walks never touch them.
- **`IndirectStore` and the broadcast executors.** These already
  execute outside the per-node walk (`dag_exec_broadcast_with`,
  `dag_exec_packed_broadcast`). Flat walker still calls into the
  existing broadcast executors — don't try to inline broadcast bodies
  into the flat op stream in step 1.
- **Memo invalidation.** The current per-walk epoch-tagged memo is
  on `DagNode` indices. Flat walker doesn't need a memo at all
  (each scratch slot is written exactly once per walk, by exactly one
  op). Drop the memo from the flat path. Don't try to share memo
  storage between paths.
- **`Param(i)` from the tree-walker fallback.** When a flat-side
  `CallTreeFallback` node represents a `Call`, it still needs to push
  a call frame before delegating to `dag_eval_node`, and pop it after.
  Args evaluated as flat ops first, frame pushed with their values,
  then `dag_eval_node(body_root)` against the existing call stack.
- **`CALCITE_V2_FLAT` is a dev knob.** Document it in `docs/log.md`
  with a "remove this flag once step 1 ships green" follow-up. Don't
  let it metastasise into a long-term config surface.

### Success criteria for step 1

1. Tree walker still works, untouched. Bench-web.mjs zork run with
   `CALCITE_V2_FLAT=0` matches today's tick count and end-state hash
   exactly.
2. Flat walker passes the smoke suite
   (`node ../CSS-DOS/tests/harness/run.mjs smoke`).
3. Flat walker boots zork end-to-end via bench-web.mjs. End-state hash
   identical to `CALCITE_V2_FLAT=0`. (Run with `--cart=zork` and
   `--runs=3` to wash thermal noise.)
4. Flat walker hits ≥3× over tree-walker ticks/s on zork. If it
   doesn't, do not move to step 2 — find out why first. Likely
   suspects: too many `CallTreeFallback` ops (function calls
   dominate), bad scratch-slot reuse, or the linearisation pass
   itself is slow enough to bite cold-start.
5. Bootle-ctest splash-fill works (memory hash bit-identical to
   baseline) — this is the regression-magnet workload from the
   2026-04-18 session.
6. **`docs/log.md` entry written** with: real-cart numbers (zork,
   rogue, bootle-ctest, doom8088 if budget allows), what worked,
   what didn't, what's next. No micro-fixture numbers in the entry.

## Bench commands you'll want

```sh
# Dev server (one-off, leave running)
node ../CSS-DOS/web/scripts/dev.mjs                    # :5173

# From a worktree, point CSS-DOS at the right calcite binaries:
export CALCITE_REPO=$(pwd)

# Real-cart bench (the one that counts)
node ../CSS-DOS/tests/harness/bench-web.mjs --cart=zork --runs=3
node ../CSS-DOS/tests/harness/bench-web.mjs --cart=doom8088 --runs=1

# A/B against the same binary:
CALCITE_V2_FLAT=0 node ../CSS-DOS/tests/harness/bench-web.mjs --cart=zork --runs=3
CALCITE_V2_FLAT=1 node ../CSS-DOS/tests/harness/bench-web.mjs --cart=zork --runs=3

# Conformance (first-line divergence finder)
node ../CSS-DOS/tests/harness/pipeline.mjs fulldiff <cabinet>.css --max-ticks=10000

# Native CLI for debugging only — never the perf number of record
target/release/calcite-cli.exe -i <path-to.css>
```

## What you should NOT do

- Don't measure on `hello-text.css` and call anything done.
- Don't keep `CALCITE_V2_FLAT` past step 1's ship. The flag exists to
  let one bisect against the oracle — once flat is correct and faster
  on real carts, the tree walker code goes (step 5).
- Don't replace `DagNode` in step 1. The tree walker is the oracle.
- Don't optimise function-call performance in step 1 — escape-hatch
  is fine; collapse it later.
- Don't add new pattern recognisers in step 1. Recogniser work
  belongs in steps 2–5 (specialised loads, peephole fusion, dispatch
  flat-array).
- Don't gate behind `#[cfg]` features either. Runtime env var only.
  Both targets must build the same code.

## Files you'll touch (best guess)

- New: `crates/calcite-core/src/dag/flat.rs` (op vocabulary +
  walker)
- New: pass in `crates/calcite-core/src/dag/lowering.rs` or sibling
  module to linearise `Dag` → `FlatProgram`
- Edit: `crates/calcite-core/src/dag/mod.rs` (export new module)
- Edit: `crates/calcite-core/src/dag/types.rs` (maybe add a
  `flat: Option<FlatProgram>` to `Dag`, or store separately on
  `Evaluator` — prefer separate, less churn)
- Edit: `crates/calcite-core/src/eval.rs` (gate at the two
  `dag_v2_tick` call sites; new `dag_v2_tick_flat` method; flag read
  in `Evaluator::new`)
- Edit: `docs/log.md` (entry on landing)
- Edit: `crates/calcite-core/Cargo.toml` only if a new dep is needed
  (none expected)

## Final reminder

Performance numbers in the logbook must be from real carts via
bench-web.mjs. If you find yourself about to write "27K → 90K t/s on
hello-text" as evidence of progress, stop and run zork instead.
