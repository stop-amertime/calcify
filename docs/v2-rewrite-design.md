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
| `LoadVar { slot, kind: TickPosition }` | `Expr::Var { name, .. }` (resolved) | One node covers both "this tick" and "previous tick" reads. `kind` is `Current` for `--foo`, `Prev` for `--__1foo` / `--__2foo`. The walker uses `kind` to choose between the per-tick property cache and committed state — see § State model. |
| `Calc { op, args }` | `Expr::Calc(CalcOp::*)` | Generic arithmetic. Op enum mirrors `CalcOp`: Add, Sub, Mul, Div, Mod, Min, Max, Clamp, Round(strategy), Pow, Sign, Abs, Neg. |
| `BitOp { op, args }` | `Expr::FunctionCall { name, .. }` for known pure-bit `@function` names | Recognised at lowering time. The exact name list is loaded from `css-lib.mjs` during Phase 1 day-1 (open question 1) — candidates include `--and`, `--or`, `--xor`, `--not`, `--shl`, `--shr`, `--bit`, `--lowerBytes`, `--rightShift`, but the list isn't finalised here. Bodies are pure bit ops; lowering bypasses the call frame. Falls back to `FuncCall` if unrecognised. |
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

`StyleCondNode` is a tiny sub-IR (`Single { slot, value: NodeId }`,
`And(Vec<StyleCondNode>)`, `Or(Vec<StyleCondNode>)`); not a top-level
DAG node because it never produces a value, only gates one.

The DAG itself is `Vec<DagNode>` indexed by `NodeId`. Each tick = one
topological evaluation pass. Cross-tick dataflow flows through `LoadPrev
→ … → WriteVar` chains where the `WriteVar`'s slot is the same one
`LoadPrev` reads next tick.

## Lowering rules — Expr → DagNode

Direct, mechanical, no idiom recognition (Phase 1):

1. **Slot map.** Built from `properties` after `state.load_properties`,
   keyed by *bare* property name (no `--` prefix, no `__0`/`__1`/`__2`
   prefix — those prefixes are structural and don't denote a separate
   slot; they denote *which tick's value of the same slot* to read).
   See § State model for the slot/address scheme.

2. **`Expr::Literal(v)` → `Lit(v)`.**

3. **`Expr::StringLiteral(s)` → `LitStr(s)`.**

4. **`Expr::Var { name, fallback }` → `LoadVar { slot, kind }`.**
   - Strip the `--` prefix.
   - If the name starts with `__1` or `__2`, `kind = Prev`; else
     `kind = Current`. (v1's `strip_prop_prefixes` treats `__0`/`__1`/
     `__2` identically — they all resolve to the same slot. v2 follows.)
   - `slot = slot_of(stripped_name)` against the slot map.
   - `fallback` handling is **deferred** — Phase 1 sub-task: read v1's
     `compile.rs` (search `Expr::Var` lowering) and v1's
     `eval::resolve_property` to determine whether `fallback` is ever
     runtime-meaningful in the cabinets we run. If only `--__2*`
     fallbacks fire at runtime and they're constant, lower the constant
     into the slot's initial value at load. If runtime-meaningful, emit
     a `VarOrFallback` node. The cabinet shapes in `kiln/emit-css.mjs`
     suggest only `--__2mN: var(--__1mN, init)` and the buffer-copy
     shape need this — both are single-assignment shapes that never
     reach the walker (see assignment filtering rule below).

5. **`Expr::Calc(op)` → `Calc { op, args: lowered }`.** Map each
   `CalcOp` variant 1:1 to a `Calc` op kind. Arity matches.

6. **`Expr::FunctionCall { name, args }`:**
   - If `name` matches a known pure-bit `@function` (final list pinned
     during Phase 1 day 1 — see open question 1), lower to
     `BitOp { op, args: lowered }` directly, bypassing the call.
   - Else, lower to `FuncCall { fn_id, args: lowered }`. Each user
     `@function` is its own sub-DAG keyed by `fn_id`, rooted at the
     function's `result` expression.

7. **`Expr::StyleCondition { branches, fallback }` → `If { branches:
   lowered, fallback: lowered }`.** Each branch's `StyleTest` lowers to
   a `StyleCondNode`; each branch's `then` lowers to a `NodeId`. A
   `StyleTest::Single { property, value: Expr }` lowers to
   `StyleCondNode::Single { slot: slot_of(property), value: lowered_node }`
   — the value side is itself a `NodeId` even though in practice it's
   almost always a literal. The walker then evaluates both sides and
   compares.

8. **`Expr::Concat(parts)` → `Concat(lowered)`.**

### Top-level assignment lowering

For each `Assignment { property, value }` in
`program.assignments`:

- **Skip if** the property name starts with `--__0`, `--__1`, or
  `--__2`. These are CSS triple-buffer copies (e.g.
  `--__1mN: var(--__2mN, init)`) that are no-ops in v1's flat mutable
  state model (`eval.rs:413`); v2 follows the same elision. The
  semantic effect of the buffer copy — making the previous tick's
  value visible as `--__1mN` next tick — is achieved automatically
  by writing `--mN` and reading `state.read_mem(slot_of(mN))`.

- **Skip if** the property is in `program.fast_path_absorbed` or in
  the absorbed-properties set returned by
  `pattern::broadcast_write::recognise_broadcast` /
  `pattern::packed_broadcast_write::recognise_packed_broadcast` on
  `program.assignments`.

- **Otherwise**, lower `value` to a `NodeId` and emit
  `WriteVar { slot: slot_of(property), value }`.

The DAG's terminal nodes are the union of:

- `WriteVar` per non-skipped, non-absorbed assignment.
- `IndirectStore` per `prebuilt_broadcast_write` and per
  `prebuilt_packed_broadcast_port`.
- `IndirectStore` per recogniser-found broadcast write.

For each `prebuilt_broadcast_write` and `prebuilt_packed_broadcast_port`
the DAG builder emits the Phase 2 super-node directly:
`IndirectStore { gate, addr, val, width, region, pack }`. Phase 1's
walker has to evaluate it, which is fine — its semantics are well-
defined and don't require any Phase 2 recogniser to fire. (This is the
one place where Phase 1 emits a super-node despite "no recognition" —
it's the cheaper alternative to re-parsing 30 MB of memory cells.)

## State model and the slot map

Reuses `State` (no changes). v1's address space is unified:

- **State-var slots** get *negative* addresses (-1, -2, …) keyed by
  bare name and allocated in declaration order during
  `state.load_properties` (see `state.rs:813-820`).
- **Memory bytes** (properties named `mN` for integer N) get
  *non-negative* addresses == N, indexing into `state.memory[]` (or
  the packed-cell table for `mcN`).
- **`State::read_mem(addr: i32)`** dispatches on the sign and routes
  through packed-cell / disk-window / extended-memory tables as needed
  (see `state.rs:251-`). The walker uses the same routing.

The DAG builder's slot map is `HashMap<String, i32>` keyed by *bare*
property name (no `--` prefix, no `__0`/`__1`/`__2` prefix), built
post-`load_properties` because `load_properties` is what assigns the
negative slot indices.

### `--__1<x>` and `--__2<x>` read semantics

There is no separate "previous-tick" state array in v1 (`state.rs`
contains no `prev_state_vars` field). The illusion of "previous-tick
read" emerges from two mechanisms:

1. **Per-tick property cache.** `Evaluator` maintains
   `self.properties: HashMap<String, Value>` during a tick — written by
   each assignment, read by subsequent assignments referencing
   `var(--foo)`. `--__1foo` reads explicitly *bypass* this cache
   (`eval.rs:1800`) and read from committed `state` instead.

2. **Topological sort on current-tick reads only.** `--__1*` reads
   don't impose ordering constraints (`eval.rs:1231`), so writes to
   slot S can be scheduled before reads of `--__1S` — and the read
   sees whatever S was committed to last tick.

Memory cells (the broadcast-write targets) are written by ports near
the end of the tick (after all reads), so `--__1mN` reads return prior-
tick values naturally because no current-tick write has landed yet.

The v2 walker reproduces this:

- `LoadVar { slot, kind: Current }` checks the per-tick cache (a
  `Vec<Option<Value>>` indexed by slot, reset each tick); falls back
  to `state.read_mem(slot)` if the cache slot is empty.
- `LoadVar { slot, kind: Prev }` reads `state.read_mem(slot)`
  directly, bypassing the cache.
- `WriteVar { slot, value }` writes the computed value into the cache
  *and* commits to `state.read_mem`/`write_mem` immediately. (v1
  commits in a writeback phase at end-of-tick; v2 may match or may
  commit eagerly — Phase 1 sub-task to decide. Eager commit is simpler
  for the walker; lazy commit reduces redundant `state.write_mem` calls
  for slots overwritten multiple times. Default to eager unless a perf
  gate makes lazy preferable.)

This is structurally subtle and its corner cases (write-before-prev-
read, broadcast-write ordering, packed-cell timing) need pinning down
before the walker is written. **Phase 1 day-1 sub-task: write a unit
test in `tests/v2_tick_semantics.rs` that exercises each corner case
against v1, and use it as the gate for the walker design.**

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
   `tests/backend_equivalence_v2.rs` runs a real cabinet for N=1000
   ticks under both backends from the same starting snapshot and
   asserts bit-identical state vars and memory at the end. Cabinet
   pick: **`output/rogue.css`** if it exists (small, ~6050 ticks/s at
   v1 speed → 1000-tick differential <200ms), else the smallest
   `tests/fixtures/*.css` that exercises broadcast writes and
   `@function` calls. Decide at wire-up time.
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

- `Lit`, `LoadVar` (Current/Prev), `Calc`, `BitOp`, `FuncCall`, `If`,
  `Concat` — direct from CSS spec / `Expr` enum.
- `WriteVar` — direct from `Assignment` (a CSS top-level declaration).
- `Switch`, `IndirectStore`, `Hold`, `Mux`, `BitField` — all motivated
  by structural CSS shapes (catalogue idioms 1, 3-5+7, 8, 9, 11). Each
  has a documented genericity probe in
  [`phase2-idiom-catalogue.md`](../../calcite-v2/docs/phase2-idiom-catalogue.md).

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
   Pulled from `../CSS-DOS/kiln/css-lib.mjs`. Phase 1 day-1 task: grep
   that file for the canonical list and pin it in `dag/lowering.rs`
   constants. The list is structural (CSS standard library shapes),
   not cabinet content.

2. **Differential test cabinet pick.** Pick the smallest cabinet that
   exercises broadcast writes, `@function` calls, and the per-tick
   property cache. `output/rogue.css` is the leading candidate but its
   existence and size aren't checked yet.

3. **Whether to call v1's `recognise_broadcast` directly or
   reimplement.** Calling it is cheaper and preserves bug-for-bug
   compatibility; reimplementing is cleaner but doubles the recogniser
   surface area. Default to **call v1's**, on the grounds that the
   matcher's contract is "find broadcast shapes" — same job either way,
   no value in two implementations. Revisit if Phase 2's DAG-level
   matcher does the job naturally.

4. **`Var` fallback runtime semantics.** Whether `Expr::Var.fallback`
   ever fires at runtime in the cabinets we run, or whether it's
   purely a compile-time initial-value marker. Phase 1 day-1 task:
   check v1's `eval::resolve_property` and the `compile.rs` lowering;
   confirm the only runtime-meaningful use is the `--__2mN: var(..., init)`
   shape (which is filtered out by the buffer-copy skip rule); pin
   the rule in `dag/lowering.rs`. If runtime-meaningful elsewhere,
   add a `VarOrFallback` node.

5. **Walker write commit timing.** v1 has a writeback phase at end-of-
   tick; v2 walker can commit eagerly (simpler) or batch (fewer
   `state.write_mem` calls but harder to reason about). Phase 1 day-1
   task: write the corner-case unit test (see § State model) and use
   it to decide. Eager is the default unless the test fails.

6. **Tick-semantics corner-case test.** Phase 1 cannot start writing
   the walker until the corner cases are pinned. Concrete tests
   needed:
   - Write to slot S, then `--__1S` read in same tick (does it see
     prior or current tick value? — depends on topological position).
   - Broadcast write to memory cell M, then `--__1mM` read later in
     tick (depends on broadcast-write-ordering policy).
   - `var(--foo)` read where `--foo` has not yet been assigned this
     tick (does it see committed state or property cache?).
   These tests run against v1 first to pin v1's actual semantics, then
   v2 has to match.

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
