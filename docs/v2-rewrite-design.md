# calcite v2 — clean-rewrite design

**Status:** in progress, branch `calcite-v2-rewrite`. Companion to
[`compiler-mission.md`](compiler-mission.md). Parallel stream to the
additive `calcite-v2` worktree; the two streams will be reconciled by
the project owner. This doc is the v2-rewrite stream's contract.

## What "stream 2" is

A clean rewrite of the calcite pipeline from parser output to codegen.
v1 stays in the tree as a conformance backstop. The end state is:

```
ParsedProgram → v2 DAG → v2 idiom recognisers → DAG codegen → execute
```

No `Vec<Op>` in the pipeline. No dependency on v1's `pattern/` module
at runtime. v1's interpreter stays callable — for differential tests
during development and as a backstop after — but is not on the hot
path.

The constraints everything else lives under: the cardinal rule
(CSS-generic, no x86 knowledge, Chrome is ground truth) and bit-
identical results vs Chrome on the Phase 0.5 primitive conformance
suite. v1 is a faster proxy for Chrome that is useful for differential
testing, but it is **not** a peer implementation we are matching — if
Chrome and v1 disagree, we match Chrome.

## Why this stream chooses ParsedProgram, not Vec<Op>

The additive stream's [`phase1-dag-ir.md`](../../calcite-v2/docs/phase1-dag-ir.md)
chose `Vec<Op>` (post-compile bytecode) as the source IR, on the
reasoning that Op is already SSA-shaped and that v1's pattern lowerings
are already encoded in the Op stream "for free."

This stream rejects that choice for one decisive reason: **building on
top of v1's Op stream inherits v1's pattern-recognition decisions.** v1
decided where dispatch chains live, what gets broadcast-collapsed, where
packed reads happen, where word-pair fusion lands. A clean rewrite
should make those decisions in a substrate designed for the eventual
codegen target — not be downstream of them.

Concretely: v1's broadcast recogniser fires only when a property's
expression has a specific `if(style())` cascade shape. The Doom8088
cabinet fast-path absorbs ≥1MB of `--mN: …` before they ever reach
`Expr` form (see `parser/fast_path.rs`); v2 building from the Op stream
sees those as already-collapsed `Op::CompiledBroadcastWrite` super-ops,
inheriting whatever scope and gate-extraction rules v1 chose. Building
from `ParsedProgram` (with the prebuilt fast-path tables surfaced as
data, not as opaque ops) lets v2 redo those choices — or skip them —
freely.

## Source IR

`ParsedProgram` (parser output) is the v2 DAG's source IR. The v2 DAG
builder consumes:

- `properties: Vec<PropertyDef>` — `@property` declarations. Used to
  set up the slot map; not lowered to DAG nodes directly.
- `functions: Vec<FunctionDef>` — `@function` definitions. Each
  function's body is an Expr tree; lowered to a per-function sub-DAG
  rooted at the result expression.
- `assignments: Vec<Assignment>` — top-level `--prop: <expr>`
  assignments on `.cpu`. Each lowers to a `WriteVar(slot, dag_node)`.
- `prebuilt_broadcast_writes: Vec<BroadcastWrite>` — broadcast-write
  entries the parser fast-path absorbed before Expr trees were built.
  These are surfaced as data structures, not ops, and the DAG builder
  emits an `IndirectStore` super-node per port directly.
- `prebuilt_packed_broadcast_ports: Vec<PackedSlotPort>` — same shape
  for packed (`PACK_SIZE=2`) cabinets.
- `fast_path_absorbed: HashSet<String>` — property names already
  consumed by the fast-path. The DAG builder filters these out of the
  assignment loop so they aren't double-counted.

**No re-parse, no fast-path disable knob.** The fast-path is a
performance optimisation for parser throughput on huge cabinets
(Doom8088 is ~30 MB of CSS); rebuilding Expr trees for every memory
cell would cost ≥10× parse time. The fast-path output is structurally
a `BroadcastWrite { dest, address_map, gate, ... }` value — equivalent
information to what an Expr cascade encodes — so v2 takes the same
shape from either source.

This also means v1's `pattern/broadcast_write.rs` and
`pattern/packed_broadcast_write.rs` recognisers are still useful at
build time: they cover the residual `Expr`-form broadcast assignments
the fast-path didn't catch (small cabinets, non-CSS-DOS emitters, and
anything below the 1 MB fast-path threshold). v2 may keep calling them
as helpers during DAG construction, classified as "v1 code that stays
callable as a backstop" (see § What v1 code stays callable below).

## DAG node vocabulary

Refined against the actual `Expr` and `CalcOp` enums in
[`crates/calcite-core/src/types.rs`](../crates/calcite-core/src/types.rs).
Phase 1 lands the **pure expression nodes** plus the **side-effecting
nodes**; the **super-nodes** are Phase 2 deliverables but listed here
so Phase 1 doesn't paint into a corner.

### Pure expression nodes

| Node | Lowers from | Notes |
|------|------------|-------|
| `Lit(f64)` | `Expr::Literal` | f64 to match parser; codegen narrows to i32. |
| `LitStr(String)` | `Expr::StringLiteral` | Only used by string-syntax props (textBuffer). |
| `LoadVar { slot }` | `Expr::Var { name, .. }` (resolved) | Reads current-tick state. The `--__1foo` prefix → `LoadPrev { slot_of("foo") }` (separate node). |
| `LoadPrev { slot }` | `Expr::Var { name: "--__1<x>", .. }` | Reads previous-tick state. |
| `Calc { op, args }` | `Expr::Calc(CalcOp::*)` | Generic arithmetic. Op enum mirrors `CalcOp`: Add, Sub, Mul, Div, Mod, Min, Max, Clamp, Round(strategy), Pow, Sign, Abs, Neg. |
| `BitOp { op, args }` | `Expr::FunctionCall { name: "--and"/"--or"/"--xor"/"--not"/"--shl"/"--shr"/"--bit"/"--lowerBytes"/"--rightShift", .. }` | Recognised at lowering time: these are CSS `@function`s in css-lib but their bodies are pure bit ops. Lowered to native bitwise ops in the DAG. |
| `FuncCall { fn_id, args }` | `Expr::FunctionCall { name, .. }` for user `@function`s | Pure call. The function body is its own sub-DAG. |
| `If { branches: Vec<(StyleCondNode, NodeId)>, fallback: NodeId }` | `Expr::StyleCondition { branches, fallback }` | Branch-shape conditional. Phase 2 collapses long branches into `Switch`. |
| `StyleCond { property: SlotId, value: NodeId }` | `StyleTest::Single` | Note: a `StyleTest` is only meaningful as the predicate of a branch — it's not a full DAG node, but a sub-AST attached to `If` branches. |
| `StyleAnd(Vec<StyleCondNode>)`, `StyleOr(...)` | `StyleTest::And` / `StyleTest::Or` | Same — sub-AST of `If`. |
| `Concat(Vec<NodeId>)` | `Expr::Concat` | String concatenation. Display path only. |

### Side-effecting nodes

| Node | Notes |
|------|-------|
| `WriteVar { slot, value: NodeId }` | One per top-level `Assignment`. The DAG's terminal nodes — every dependency root reaches one. |

### Super-nodes (Phase 2)

| Node | Captures |
|------|----------|
| `Switch { key: NodeId, table: TableId, fallback: NodeId }` | Catalogue idiom 1. Long `if(style(--P: N))` chains. |
| `IndirectStore { gate: Option<NodeId>, addr: NodeId, val: NodeId, width: NodeId, region: RegionId, pack: u8 }` | Catalogue idioms 3+4+5+7. Broadcast write, packed or unpacked, gated, with optional word-pair width. |
| `Hold { slot }` | Catalogue idiom 8. `--x: var(--__1x)`. |
| `Mux { cases: Vec<(StyleCondNode, NodeId)>, fallback: NodeId }` | Catalogue idiom 9. Small priority cascades (TF/IRQ override). |
| `BitField { src: NodeId, shift: u8, mask: u32 }` | Catalogue idiom 11. Lowered from `(x >> n) & mask` shapes. |
| `BufferCopy { src_slot, dst_slot }` | Cabinet-wide double-buffer copy: `--__1mN: var(--__2mN, init)`. (New: not in the additive stream's catalogue, but observed in `kiln/emit-css.mjs` `emitMemoryBufferReadsStreaming`.) |

`StyleCondNode` is a tiny sub-IR (`Single { slot, value: NodeId }`,
`And(Vec<StyleCondNode>)`, `Or(Vec<StyleCondNode>)`); not a top-level
DAG node because it never produces a value, only gates one.

The DAG itself is `Vec<DagNode>` indexed by `NodeId`. Each tick = one
topological evaluation pass. Cross-tick dataflow flows through `LoadPrev
→ … → WriteVar` chains where the `WriteVar`'s slot is the same one
`LoadPrev` reads next tick.

## Lowering rules — Expr → DagNode

Direct, mechanical, no idiom recognition (Phase 1):

1. **Slot map.** Built from `properties` after `state.load_properties`.
   Negative slots = state vars (registers, internal flags). Non-negative
   slots = memory bytes / packed cells. The `--__1<name>` and
   `--__2<name>` prefixes are structural references to the same slot at
   different temporal positions, lowered to `LoadPrev` (and writes to
   the buffer-copy cell).
2. **`Expr::Literal(v)` → `Lit(v)`.**
3. **`Expr::StringLiteral(s)` → `LitStr(s)`.**
4. **`Expr::Var { name, fallback }`:**
   - Strip the `--` prefix.
   - If name starts with `__1` → `LoadPrev { slot: slot_of(rest) }`.
   - If name starts with `__2` → `LoadPrev2 { slot: slot_of(rest) }`
     (used in `--__1mN: var(--__2mN, init)` shapes; one tick further
     back).
   - Else → `LoadVar { slot: slot_of(name) }`.
   - `fallback` is currently used only by `--__2` shapes (the initial
     value when no prior tick exists); lowered as `Lit` and selected at
     load via `slot_init` rather than at runtime. (This matches v1's
     behaviour — `read_mem` returns the slot's current value, never
     falls through.)
5. **`Expr::Calc(op)` → `Calc { op, args: lowered }`.** Map each
   `CalcOp` variant 1:1 to a `Calc` op kind. Arity matches.
6. **`Expr::FunctionCall { name, args }`:**
   - If `name` is one of the known pure-bit-op `@function`s (`--and`,
     `--or`, `--xor`, `--not`, `--shl`, `--shr`, `--bit`, `--lowerBytes`,
     `--rightShift`), lower to `BitOp { op, args: lowered }` directly,
     bypassing the call. (This matches v1's `function_patterns` fast-
     path.) The list is fixed and short — we are recognising structural
     CSS shapes (these `@function`s come from `css-lib.mjs`'s standard
     library), not cabinet content.
   - Else, lower to `FuncCall { fn_id, args: lowered }`. Each user
     `@function` is its own sub-DAG keyed by `fn_id`.
7. **`Expr::StyleCondition { branches, fallback }` → `If { branches:
   lowered, fallback: lowered }`.** Each branch's `StyleTest` lowers to
   a `StyleCondNode`; each branch's `then` lowers to a `NodeId`.
8. **`Expr::Concat(parts)` → `Concat(lowered)`.**

For top-level `Assignment { property, value }`: lower `value` to a
`NodeId`, then emit `WriteVar { slot: slot_of(property), value }`.
The DAG's terminal nodes are exactly the `WriteVar` set, one per
non-absorbed assignment plus one synthesised per fast-path-absorbed
broadcast port (see § Source IR).

For each `prebuilt_broadcast_write` and `prebuilt_packed_broadcast_port`
the DAG builder emits the Phase 2 super-node directly:
`IndirectStore { gate, addr, val, width, region, pack }`. Phase 1's
walker has to evaluate it, which is fine — its semantics are well-
defined and don't require any Phase 2 recogniser to fire. (This is the
one place where Phase 1 emits a super-node despite "no recognition" —
it's the cheaper alternative to re-parsing 30 MB of memory cells.)

## State model and the slot map

Reuses `State` (no changes). The DAG builder's slot map is just
`HashMap<String, i32>` keyed by bare property name (no `--` prefix, no
`__1` / `__2` prefix), built from:

1. Properties classified as state vars by `state.load_properties` →
   slot = the negative index they got assigned.
2. Properties classified as memory bytes (`mN`) → slot = `N`.
3. Properties classified as packed cells (`mcN`) → slot = the
   `packed_cell_table` entry for cell `N`.

This has to happen post-`load_properties` because `load_properties`
performs the slot allocation (state vars take the first negative
indices; CSS declaration order matters). The DAG builder is constructed
inside `Evaluator::from_parsed` after `State` is wired, same as v1's
compile step.

**Reads of `--__1<x>`.** Lowered to `LoadPrev { slot: slot_of(x) }`.
The walker reads from `state.prev_state_vars` (or the equivalent for
memory) — same semantics as v1's `Op::LoadPrev`. No new state-model
plumbing needed.

**Reads of `--__2<x>`.** Currently only appear in
`--__1mN: var(--__2mN, init)` buffer-copy shapes. Lowered to a
`BufferCopy` super-node (Phase 1 emits this directly during lowering
because the shape is one assignment, trivially recognisable, and
encoding it as `LoadPrev2 → WriteVar` would gratuitously expose the
two-tick-back state vector that v1's `State` doesn't carry today). The
walker for `BufferCopy { src_slot, dst_slot }` is one assignment.

## Phase 1 acceptance gates

These are the only tests that have to pass before Phase 2 starts.
Failure on any one of them means investigate, not push through.

1. **`cargo test -p calcite-core` green** with the default backend
   (Bytecode), no behavioural change.
2. **Same tests green with `CALCITE_BACKEND=dag-v2`** set — the new
   backend produces bit-identical state.
3. **Phase 0.5 primitive conformance suite** produces the same
   PASS/SKIP/XFAIL counts under both backends. Reference: 41 PASS / 5
   SKIP / 3 XFAIL on main today (`tests/primitive_conformance.rs`).
4. **Differential cabinet test.** A new
   `tests/backend_equivalence_v2.rs` runs `web/demo.css` for N=1000
   ticks under both backends from the same starting snapshot and
   asserts bit-identical state vars and memory at the end. (Mirror of
   `tests/backend_equivalence.rs` from the additive worktree; rebuilt
   for v2.)
5. **`wasm-pack build crates/calcite-wasm --target web`** still
   succeeds with no errors. The DAG walker links clean for wasm32.
6. **No perf regression on `calcite-bench -i output/rogue.css -n
   2000`** under default-backend (Bytecode). v2 ships dormant —
   activated only by env flag in Phase 1. The web/CLI default stays
   v1 until Phase 3.

If gate 6 fails (i.e. shipping v2 dormant slowed the v1 hot path):
investigate what got accidentally pulled in. Most likely culprit is
sharing helpers v1 didn't import.

## What v1 code stays callable as a backstop

Stays callable, no rename, no deletion:

- `parser::*` — parse to `ParsedProgram`, including the fast-path. v2
  consumes the same output v1 does.
- `pattern::broadcast_write::recognise_broadcast` and
  `pattern::packed_broadcast_write::recognise_packed_broadcast` —
  called by v2 during DAG construction to recognise Expr-form broadcast
  shapes the parser fast-path didn't catch. (v2 *uses* these results
  rather than re-implementing the same matchers; the cardinal-rule
  contract is preserved because the matchers are pure CSS-shape over
  Expr.)
- `pattern::dispatch_table::recognise_dispatch` — same: v2 calls it
  during Switch super-node construction in Phase 2.
- `state::State` — no changes. Slot model is shared.
- `eval::property_to_address` and the global address map — shared.

What v2 **does not** call:

- `compile::*` — the Vec<Op> compiler. v2 produces a DAG, not ops.
- `eval::Evaluator::tick` and `exec_ops` — v1's interpreter. The v2
  walker is a separate code path.
- `pattern::fusion_sim::*` — v1's bytecode-fusion experiments. Phase 3
  codegen is its own thing.

The split is clean: v1 owns "Op stream and how to interpret/JIT it";
v2 owns "DAG and how to walk/codegen it." Both consume the same
`ParsedProgram` and write to the same `State`.

## Cardinal-rule audit

The DAG node vocabulary above contains zero x86, BIOS, DOS, or cabinet-
specific concepts. Every node corresponds to a CSS structural element:

- `Lit`, `LoadVar`, `LoadPrev`, `Calc`, `BitOp`, `FuncCall`, `If`,
  `Concat` — direct from CSS spec / `Expr` enum.
- `WriteVar` — direct from `Assignment` (a CSS top-level declaration).
- `Switch`, `IndirectStore`, `Hold`, `Mux`, `BitField`, `BufferCopy`
  — all motivated by structural CSS shapes (catalogue idioms 1, 3-5+7,
  8, 9, 11, plus the buffer-copy shape). Each has a documented
  genericity probe in [`phase2-idiom-catalogue.md`](../../calcite-v2/docs/phase2-idiom-catalogue.md).

Operational test: a calcite engineer who has never seen a CPU emulator
can derive every node above from CSS spec + `Expr` enum + the catalogue
of idioms CSS forces emitters into. None of them require knowing what
the cabinet computes.

## Wasm safety audit

- DAG nodes are plain enums + Vec<NodeId> + HashMap<String, SlotId>.
  No `std::time`, no threads, no fs/net, no `std::env` writes.
- The walker is pure data-structure traversal. The optional profile
  scope wrappers (matching v1) use `web_time::Instant`.
- Compile-time DAG construction reads `ParsedProgram` and writes a
  `Dag`. No I/O, no concurrency.

## Open questions to resolve before Phase 1 lands

1. **Exact set of pure-bit `@function` names to fast-path in lowering.**
   Pulled from `css-lib.mjs`. Need to grep that file for the canonical
   list and pin it in `dag/lowering.rs` constants. (Phase 1 task,
   first day of code.)
2. **`web/demo.css` size for the differential test.** If too small to
   exercise broadcast writes, switch to a small CSS-DOS cabinet
   (`output/rogue.css` works, ~6050 ticks/s at v1 speed, fits a
   1000-tick differential in <1s). Decide when wiring up the test.
3. **Whether to call v1's `recognise_broadcast` directly or
   reimplement.** Calling it is cheaper and preserves bug-for-bug
   compatibility; reimplementing is cleaner but doubles the recogniser
   surface area. Default to **call v1's**, on the grounds that the
   matcher's contract is "find broadcast shapes" — same job either way,
   no value in two implementations. Revisit if Phase 2's DAG-level
   matcher does the job naturally.

## File layout

```
crates/calcite-core/src/dag/
  mod.rs           Public entry: build_dag(&ParsedProgram, &State) -> Dag
                   plus Backend::DagV2 wiring.
  types.rs         DagNode, NodeId, SlotId, StyleCondNode, Dag.
  lowering.rs      Expr → DagNode. Slot map. Pure-bit-op fn name
                   recognition.
  walker.rs        Dag::eval(&mut State). Topological evaluator.
  walker_test.rs   Differential test scaffolding (probably moves to
                   tests/ before merge).
```

The `Evaluator` gains a `backend: Backend` field and a `Dag` alongside
`compiled`. `tick()` dispatches on the backend at construction time
(set once in `from_parsed`, never branched per-tick).

## What this design explicitly excludes

These are real things that *will* happen — just not in Phase 1:

- DAG-level CSE, constant folding, dead-code elimination — Phase 2.
- Cross-block code motion or scheduling — Phase 3 codegen.
- Any change to `compile.rs`, v1's pattern recognisers' internals, or
  `State`.
- Replacing `Evaluator::tick` as the default — both backends ship side
  by side until Phase 3 retires the old one.
