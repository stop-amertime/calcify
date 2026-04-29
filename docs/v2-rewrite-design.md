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
| `FuncCall { fn_id, args }` | `Expr::FunctionCall { name, .. }` (all calls, Phase 1) | Pure call. The function body is its own sub-DAG. **Phase 1 lowers every `FunctionCall` to `FuncCall` — no name-based fast-path.** v1's `FunctionPattern` enum classifies by body shape (Bitmask, RightShift, BitExtract, IdentityRead, …), which is more general than name matching and is the right model. v2 promotes that to a Phase 2 idiom-recogniser pass that emits `BitOp`/`BitField` super-nodes from body shape, regardless of `@function` name. |
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
   - The triple-buffer convention (see § State model § The CSS
     contract) maps `--foo`, `--__0foo`, `--__1foo`, `--__2foo` to
     the same logical slot at different temporal positions. v2's
     lowering reflects that:
     - Strip the `--` prefix; if `__0`/`__1`/`__2` follows, set
       `kind = Prev` and strip the buffer prefix.
     - Otherwise `kind = Current`.
   - Slot resolution:
     1. Look up the bare name in the slot map.
     2. If found → emit `LoadVar { slot, kind }`. The fallback is
        unreachable in CSS terms because the property is declared
        and always has a value; discard it.
     3. If not found and `fallback` is present → lower the fallback
        expression and emit *that* node. CSS spec: `var(--undef, X)`
        evaluates to `X`.
     4. If not found and no fallback → emit `Lit(0)`. CSS spec:
        unresolved `var()` falls through to the property's
        registered initial value (0 for `<integer>` syntax, our
        sole numeric type).

5. **`Expr::Calc(op)` → `Calc { op, args: lowered }`.** Map each
   `CalcOp` variant 1:1 to a `Calc` op kind. Arity matches.

6. **`Expr::FunctionCall { name, args }` → `FuncCall { fn_id, args: lowered }`.**
   No name-based fast-path in Phase 1. The function body is its own
   sub-DAG keyed by `fn_id`, rooted at the function's `result`
   expression. Phase 2's idiom recogniser will classify function
   bodies by *shape* (e.g. `mod(a, pow(2, b))` → bitmask;
   `round(down, a / pow(2, b))` → right shift) and replace recognised
   shapes with `BitOp`/`BitField`/`Switch` super-nodes. Body-shape
   matching is cardinal-rule stronger than name matching: any
   `@function` whose body is a bitmask gets the same fast-path,
   regardless of whether it's named `--and`, `--mask8`, or
   `--myUserDefinedHelper`.

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
  `--__2`. These assignments implement the CSS triple-buffer copy
  (e.g. `--__1mN: var(--__2mN, init)`) — they shift the per-slot
  history back one tick. The walker reconstructs the same shifted
  history from the `LoadVar { kind: Prev }` mechanism (see § State
  model § What `LoadVar { kind }` means), so the literal copy is
  redundant. Eliding it produces identical observable end-of-tick
  state — the buffered values are still visible to next tick's
  `--__1foo` reads, just retrieved differently.

- **Skip if** the property is in `program.fast_path_absorbed` or in
  the absorbed-properties set returned by
  `pattern::broadcast_write::recognise_broadcast` /
  `pattern::packed_broadcast_write::recognise_packed_broadcast` on
  `program.assignments`. These assignments are still evaluated, just
  via the `IndirectStore` super-node instead of an individual
  `WriteVar` per cell — the observable state is the same.

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

## State model and tick semantics

### The CSS contract

Chrome is the spec. CSS resolves a cascade of custom-property
assignments via *substitution*: each `var(--foo)` in a declaration's
RHS is substituted with `--foo`'s computed value as resolved during
the same cascade pass. When `--b: calc(var(--a) * 10)` and
`--a` resolves first to `5`, then `--b: calc(5 * 10) = 50`. When
`--c: calc(var(--b) + 1)` resolves later in the same cascade pass,
`var(--b)` substitutes the just-computed `50`, so `--c = 51`.

This is *not* simultaneous evaluation against a prior pool — it's
sequential substitution within one cascade pass, ordered such that
each `var()` reference sees the most-recently-resolved value of its
referent.

For the cabinet, this gives single-pass full resolution per cascade.
What the cabinet wants — multi-tick state — comes from a triple-
buffer convention layered on top:

- `--mN` — the new value being computed this cascade pass (this tick).
- `--__1mN` — the value `--mN` had at the end of the previous tick.
- `--__2mN` — the value `--mN` had two ticks ago.

The buffer-copy assignments (`--__1mN: var(--__2mN, init)`) shift the
history one position each tick. Because they're regular cascade
assignments, Chrome resolves them in the same single-pass
substitution model.

The pure-CSS-in-Chrome execution of a cabinet, then, is one cascade
pass per Chrome paint frame, where each pass:

1. Reads the prior frame's committed values from registered
   `@property` storage (the property's prior computed value).
2. Resolves the cascade with substitution.
3. Commits the new computed values for the next frame.

In CSS spec terms, the prior frame's committed values are exactly
what `var(--mN)` would read if there were no `--mN` assignment in
this cascade. With the `--mN` assignment present, substitution
replaces it.

The `--__1` / `--__2` prefixes are *separate properties* in CSS —
not "the prior tick's `--mN`," but their own properties whose values
happened to be set to `--mN`'s prior value by the buffer-copy
assignments last tick. Reading `var(--__1mN)` resolves against the
`--__1mN` property's current cascade-pass value — which, for the
cabinet shape, is what `--mN` was last tick.

### What v2 has to produce

Same observable end-of-tick state as Chrome would produce on the same
CSS. Nothing else.

### How v2 produces it

The walker reproduces single-pass substitution via a per-tick value
cache plus topological ordering of the assignment graph:

1. **Compile-time topological sort.** Build a dependency graph from
   `WriteVar` terminals: an edge from `WriteVar(B)` to `WriteVar(A)`
   means A's RHS contains a `var(--B)` reference (current-tick read).
   Sort the terminals so each writer comes before any of its readers.
   `var(--__1foo)` reads do *not* create edges — they reference a
   different property (the `--__1foo` slot), not a temporal offset
   on `--foo`.

2. **Per-tick evaluation.**
   - At tick start, clear a per-tick value cache (one entry per slot).
   - Walk the topologically-sorted terminal list. For each
     `WriteVar { slot, value }`:
     - Recursively evaluate `value` against the DAG. `LoadVar` checks
       the cache first; on miss, falls back to committed state.
     - Write the computed value into the cache.
   - At tick end, flush the cache to committed `State`. (Or commit
     each `WriteVar` eagerly — implementation choice.)

3. **Cycles.** A genuine `--a: var(--a)` self-reference (no `--__1`)
   has an edge to itself; topo sort would refuse. CSS spec: such a
   declaration is invalid at computed-value time and the property
   keeps its registered initial value. The compile-time sort detects
   the cycle and emits an `Invalid` marker for that terminal; the
   walker honours it by reading the slot's initial value into the
   cache without calling `WriteVar`.

This is the same mechanism v1 uses (see `eval.rs`'s
`topological_sort_assignments` and `self.properties` cache). The
reason it's correct isn't "v1 does this" — it's that the CSS cascade
is single-pass substitution, and topo sort + cache is the textbook
implementation. v1 happens to have arrived at the right model.

### Slot map

State vars and memory bytes share v1's unified index space (negative
addresses for state vars, non-negative for memory). v2 reuses
`State`'s storage and slot allocation directly so committed state is
exchangeable across walker boundaries. v2 does not invent a separate
slot allocation.

### What `LoadVar { kind }` means in the IR

`kind` distinguishes *which property* the load reads, not which tick
position. CSS-wise, `--foo` and `--__1foo` are different properties.
The cabinet's triple-buffer convention happens to make their values
look like the same slot at different temporal positions, but at the
CSS level they're independent.

v2 collapses `--foo` and its prefixed siblings to one slot in the
underlying state (matching v1's `strip_prop_prefixes`) because that's
how `State` is structured. `kind` records whether the read should
participate in current-tick topo-sort dependency edges:

- `Current` (from bare `--foo`): the read creates a dep edge. If
  `--foo` is also assigned this cascade, the read sees the
  this-cascade value via the cache.
- `Prev` (from `--__0` / `--__1` / `--__2` prefixed): the read does
  *not* create a dep edge. It reads the underlying slot's committed
  state, which is the value `--foo` had at the end of the previous
  cascade pass — i.e. last tick's `--foo`.

This mapping reproduces what Chrome does on the cabinet in CSS terms:
`var(--__1mN)` resolves against `--__1mN`'s computed value, which by
the buffer-copy convention equals last tick's `--mN`. The walker
takes a shortcut and reads it from the same slot directly, but the
result is identical.

## Phase 1 acceptance gates

The contract is **Chrome is the oracle**. Every gate either runs CSS
through Chrome and compares, or runs a check that doesn't depend on
v1's behaviour.

Gates that have to pass before Phase 2 starts:

1. **Phase 0.5 primitive conformance suite passes against v2.** The
   suite is small `.css` snippets, each exercising one CSS primitive
   (`var()`, `calc()`, `if(style())`, math functions, edge cases),
   with expected post-evaluation state derived from Chrome via
   Playwright `getComputedStyle`. Port from
   `.claude/worktrees/calcite-v2/` (where it was authored for the
   additive stream) or build fresh — see § Open questions.

   This is the *direct* test of the cardinal-rule contract:
   "calcite produces the same results Chrome would." It catches
   primitive-level divergences that whole-cabinet tests can only
   catch transitively through framebuffer drift.

2. **`cargo test -p calcite-core` green** with v2 selected. Existing
   integration tests are pre-existing on `main` and gate the
   `Evaluator` API — they don't gate v2 semantics directly, but they
   must keep passing.

3. **`wasm-pack build crates/calcite-wasm --target web`** succeeds
   with no errors. The walker compiles for `wasm32-unknown-unknown`.

Diagnostic tools, not gates:

- **v1 differential.** Running v1 and v2 on the same cabinet and
  diffing end-of-tick state is a useful fast smoke check during
  development — when it disagrees with v2, *one of them* is wrong vs
  Chrome and the diff localises which assignment diverged. But "v2
  matches v1" is not the contract. If v1 has a quirk Chrome doesn't,
  v2 should diverge from v1 there. The Phase 0.5 suite is what
  decides which side is right.

- **Cabinet smoke run.** Loading a real cabinet (`output/rogue.css`
  if it exists, else a `tests/fixtures/*.css`) and running for N
  ticks under v2 without panic is a useful "does the walker handle
  shapes that exist in the wild" check. Crashes are bugs; state
  divergence vs v1 is a Phase 0.5 question, not a gate.

- **No perf regression** on `calcite-bench` with v1 (default
  backend). v2 ships dormant in Phase 1; if it's pulling shared
  helpers that slow v1, that's a bug. Catch with `cargo bench` before
  and after.

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

The split: v2 is the new evaluator, derived from CSS. v1 stays in the
tree as a backstop and as a fast smoke-check oracle during
development, but it is not a peer implementation v2 has to match.
Where they disagree, Chrome decides. Both consume the same
`ParsedProgram` and read/write the same `State`, which is why running
them side-by-side on the same cabinet is cheap and useful as a
diagnostic.

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

1. **Phase 0.5 primitive conformance suite — port or build fresh.**
   The suite is the Phase 1 correctness gate. The additive worktree
   (`.claude/worktrees/calcite-v2/`) reportedly has it. Phase 1
   sub-task: locate it, port to this worktree, run against v2 once
   the walker exists. If the additive version is incomplete, build
   fresh from the catalogue of CSS primitives the cabinets use
   (per `compiler-mission.md` § Phase 0.5).

2. **Walker mechanism: two-pool vs cache-plus-sort.** Both produce
   CSS-consistent results (see § State model). Phase 1 sub-task:
   prototype one, gate against Phase 0.5, switch if Phase 0.5 fails
   in a way the other mechanism would catch. Default: cache-plus-
   sort, because it reuses `State` directly with no per-tick clone.

3. **Whether to call v1's `recognise_broadcast` directly or
   reimplement.** v1's recognisers operate on `Expr` shape, not on
   v1's bytecode — so calling them is using shape-matching code, not
   inheriting v1's bytecode decisions. Default: call v1's, on the
   grounds that broadcast-shape recognition is the same job either
   way and there's no value in two implementations. The cardinal-
   rule contract is preserved as long as the recogniser's matcher is
   pure CSS-shape over `Expr` (which it is — see
   `pattern/broadcast_write.rs:61`).

## File layout

```
crates/calcite-core/src/dag/
  mod.rs           Public entry: build_dag(&ParsedProgram, &State) -> Dag.
  types.rs         DagNode, NodeId, SlotId, StyleCondNode, Dag.
  lowering.rs      Expr → DagNode. Mechanical, no idiom recognition.
  walker.rs        walk(&Dag, &mut State) — one tick of evaluation.
```

Phase 1 unit tests live alongside the module (`#[cfg(test)] mod tests`
in `lowering.rs`). End-to-end tests against Chrome (the Phase 0.5
suite) and against v1 (smoke diagnostic) live under
`crates/calcite-core/tests/`.

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
