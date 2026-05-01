# Performance Log

Running notes on calcite performance work. Read this before doing any
optimisation — it has the current bottleneck analysis and the tooling
to measure impact of changes.

## Tooling reference

See [docs/benchmarking.md](benchmarking.md) for full usage of `calcite-bench`
and the Criterion benchmarks.

---

## 2026-05-01: v2 Phase 3 (c+) — inlined-Rust prototype, coverage is the wall

Per the previous entry, (c+) was framed as a one-week prototype: per
`WriteVar` terminal, walk its reachable cone, topo-sort it, emit a
flat op vec over per-terminal locals, run with a `pc++` loop. No
recursion, no memo, no per-node function pointers. Built as
`Backend::DagV2Inlined`; terminals whose cones contain unsupported
nodes fall back to the walker per-terminal.

Op set covered: `Lit`, `LoadVar` (current/prev × scalar/transient),
all arity-1/2/3 `Calc` (Add/Sub/Mul/Div/Mod/Pow/Sign/Abs/Neg/Min2/
Max2/Clamp/Round), `BitField`, `BitwiseOp`. Bailed: `Call`/`Param`,
`If`, `Switch`, `Concat`, `LitStr`, `LoadVarDynamic`, variadic Min/Max.

Correctness: 6/6 differentials green
(`v2_inlined_differential.rs`) — covers both emit path (simple_calc,
arith_chain, div_mod) and walker fallback (nested_calls, dispatch,
mixed_cabinet). Bit-identical to the walker.

Speed (5K bench, 500 warmup, native release, single run):

| Cabinet | Coverage | v1 | walker | closures | inlined |
|---|---|---|---|---|---|
| rogue | 36.5% | 152K | 51K | 61K | 54K |
| zork1 | 39.4% | 203K | 61K | 67K | 60K |
| bootle | 39.4% | 344K | 101K | 110K | 109K |

Inlined ≈ closures ≈ walker. Same 3-5× gap to v1 as before.

### What this tells us

**Coverage is the binding constraint, not per-op cost.** ~37% of
`WriteVar` terminals on every tested cabinet. Pushing op coverage up
(arity-1, BitField, BitwiseOp on top of the initial arity-2 set)
**doubled emitted-terminal count** (14% → 36%) but didn't move
ticks/sec — because the missing 63% are the work-bearing terminals.
Specifically the ones whose cones contain `Switch` (the dispatch
tables Phase 2 built), `Call` (function bodies), and `If` (branching
cascades). Those are exactly where the runtime cost lives.

So the (c+) hypothesis "straight-line emission with locals will clear
v1" is **plausible** (the architecture is correct; emitted terminals
are bit-identical) but **not demonstrated** (we never ran the
hot-cone terminals through the straight-line path). To actually clear
v1, the inlined backend would need:

1. **Switch lowering**: jump-table op + per-branch sub-cones with
   phi-style merge into the parent cone's locals.
2. **Call lowering**: per-`fn_id` body programs emitted once,
   `LoadParam` op, frame stack at runtime.
3. **If lowering**: short-circuit branches over StyleCondition trees.

Each is several days. Each is structurally close to what wasm
codegen needs (Switch → `br_table`, Call → wasm `call`, If → `br_if`),
so doing them in interpreted Rust first and then translating to wasm
is throwaway work.

### Updated Phase 3 sequence

Old (from prev entry): (c+) inlined-Rust → (a) wasm if (c+) clears v1.

Revised: **skip ahead to (a) wasm.** The inlined prototype already
tells us what we needed to know — the codegen shape factors (per-
terminal straight-line ops over locals, walker fallback for
unsupported), and coverage is what gates the win. Doing Switch/Call/
If lowering in interpreted Rust first then re-doing them in wasm is
sunk-cost work; wasm needs the same lowering logic either way.

What we keep from (c+): `inlined_codegen.rs` and the
`Backend::DagV2Inlined` driver as a Rust-side reference executor for
the wasm output. Same emitted op vocabulary, run interpretively to
differential-test wasm against.

What we don't claim: any speedup. Inlined ≈ walker on this cabinet
set; the prototype's job was to factor the shape, not move the
needle.

### Artefacts

- `crates/calcite-core/src/dag/inlined_codegen.rs` — emitter +
  flat-op runner. Per-terminal scratch buffer reused across terminals
  (topo-order emit guarantees no read-before-write across terminals).
- `crates/calcite-core/src/eval.rs` — `Backend::DagV2Inlined` variant
  + `dag_v2_inlined_tick` driver (mirrors `dag_v2_closures_tick`'s
  phase structure; broadcasts + uncovered terminals delegate to the
  walker).
- `crates/calcite-core/tests/v2_inlined_differential.rs` — 6
  synthetic differentials vs the walker.
- `crates/calcite-cli/src/bin/probe_v2_inlined.rs` — coverage
  report + four-backend speed.

---

## 2026-05-01: v2 Phase 3 spike — closure-tree prototype, dispatch isn't the bottleneck

Per `compiler-mission.md` Phase 3, recommended sequence is (c) Rust-
closure prototype → (a) hand-emitted wasm. This entry covers the (c)
artefact: a function-pointer-per-node executor, built as
`Backend::DagV2Closures` so it shares the v2 walker's state model and
broadcast helpers but replaces the per-node `match`-on-enum dispatch
with one indirect call through a parallel `Vec<NodeHandler>` table.

Each node lowers to `(handler, payload)` at backend-selection time;
the run loop is one shared `eval_node` that does memo + dispatch via
`(cdag.handlers[id])(cdag, id, ctx, state)`. Hot-path arithmetic
(Add/Sub/Mul/Div/Mod with arity-2) gets specialised handlers; the
generic Calc handler covers the long tail.

Correctness: 4 synthetic differentials green
(`v2_closures_differential.rs`) — simple calc, nested function calls,
literal-key dispatch, BitwiseOp shape. Bit-identical to the v2 walker.

Speed on rogue.css (5-run median, 500 warmup + 4000 bench, native
release):

| Backend | Ticks/s | vs v1 | vs walker |
|---|---|---|---|
| v1 Bytecode | 313K | — | — |
| v2 walker | 56K | 5.6× slower | — |
| v2 closures | 64K | 4.9× slower | **1.14× faster** |

Closure dispatch buys ~15% over the walker, well inside the run-to-run
noise floor on this Windows laptop (±20%). The v1-vs-v2 ratio barely
moves.

### What the prototype tells us

This was a **measurement spike**, not a shipping path — its job was
to test whether eliminating per-node enum-tag dispatch recovers a
meaningful fraction of the v2 interpreter overhead before committing
to wasm codegen. The answer is **no**.

If dispatch were the bottleneck, replacing the 14-arm `match` with a
function-pointer table should have moved the needle by 50-200%
(cf. v1's `BranchIfNotEqLit` and `DispatchChain` wins, both of which
were dispatch-elimination plays). 15% says dispatch is a small
slice. The actual cost lives elsewhere — most likely:

- **Per-node memo overhead.** Two array reads + epoch compare on every
  visit. With ~30K-50K nodes per terminal traversal and broad fan-in,
  the memo is doing real work — but its per-hit cost is paid even when
  the underlying compute is one ALU op.
- **Recursion through `eval_node`.** Rust doesn't inline recursive
  function calls, so every node visit is a real call frame. The
  closure version is also recursive through `eval_node`; the
  vtable swap doesn't change that.
- **Walking 30K-50K nodes per tick** when v1's bytecode interpreter
  walks ~35K *flat ops* per tick. Same workload, different
  granularity — the DAG shape pays an O(node) call-graph traversal
  where the bytecode pays an O(op) `pc++` step. Most ticks the work
  per node and the work per op are comparable, but the per-node
  call/return + memo cost is the constant factor v1 doesn't pay.

### What this means for Phase 3

Hand-emitting wasm with the same shape — one wasm function per node,
recursion through a shared dispatcher — will produce roughly the same
result as the closure prototype. The only thing wasm changes vs Rust
closures is the runtime (browser vs native), not the dispatch shape.

The real Phase 3 win has to come from **codegen that flattens** the
DAG: emit one straight-line wasm function per terminal that inlines
the entire backwards reachability cone, dropping the per-node memo
table and the recursive dispatch entirely. This is the shape v1's
bytecode interpreter approximates by emitting a single flat
`Vec<Op>` and walking it with `pc++`. The win mechanism isn't
"replace match with function pointer", it's "replace 30K function
calls per terminal with one straight-line emitted function".

That's a much bigger lift than (c) → (a). Concretely:

- Per-terminal scheduling: pick a topological order over the
  reachable cone, emit each node once into a straight-line block.
- Shared sub-expressions: when a node is reachable from two
  terminals, either inline at both sites (size cost) or hoist into a
  separate wasm function called once and cached in a wasm `local`
  (memo cost we already pay, but per-terminal not per-node).
- StyleCondNode and Switch lowering: branch tables and break-blocks,
  not recursive calls.

The (c) prototype validates that the IR factors: every node lowered
cleanly to a (handler, payload) pair, no shape needed to fall back
to the walker. So the IR is right; the issue is the executor model.

### What didn't move

- **Closure dispatch alone**: +15% on rogue, inside noise on
  zork/bootle.
- **Hot-path Calc specialisation** (Add2/Sub2/Mul2/Div2/Mod2 each
  with their own monomorphic handler): folded into the +15% above.
  Splitting them out vs going through `h_calc_generic` for everything
  would let us measure that contribution alone, but the headline
  result wouldn't change.

### What this is NOT

- Not a perf win to ship. The +15% is sometimes +25%, sometimes 0%,
  inside Windows noise. Closure backend is `#[cfg(test)]`-equivalent
  diagnostic infrastructure, not a default.
- Not evidence that v2 is fundamentally slower than v1. v1 has years
  of peephole work piled on top of `BranchIfNotEqLit` /
  `DispatchChain` / unchecked indexing. v2's walker is the naive
  recursive-DAG case; the closure prototype is the same naive case
  with a different dispatch shape.

### Visit-count breakdown (follow-up profile)

Sampling profilers on Windows native need admin (`xperf` for samply,
`blondie_dtrace` for cargo-flamegraph). Pivoted to lightweight visit
counters in `closure_codegen::eval_node` behind an atomic enable
flag. The closure backend has the same memo + recursion shape as the
walker, so its visit pattern is the walker's visit pattern.

Result on rogue, deterministic across runs:

```
visits/tick:         1,243   (every eval_node entry)
memo hits/tick:        191   (15.4%)
handler calls/tick:  1,052   (memo misses)
ns/tick (no stats):  ~16,000
ns/tick (with stats): ~27,000   (atomic-add overhead)
```

Compare against v1 bytecode on the same cabinet (`docs/log.md`
2026-04-14): **~35,000 ops/tick at ~3 µs/tick = ~0.09 ns/op**.

The walker on closures: **1,052 handler calls × ~15 ns/call =
~15.7 µs/tick** ≈ measured 16 µs.

### What that means: the granularity is right, the per-call cost is wrong

v2 does **33× fewer units of work per tick than v1** (1,052 vs 35,000).
That's the whole *point* of moving from a flat bytecode interpreter
to a DAG IR — recognisers collapse 30+ ops into one super-node.

But each v2 unit costs **~167× more than each v1 unit** (15 ns vs
0.09 ns). Net effect: 167 / 33 ≈ **5.05× slower**. Exactly the gap.

Where the 15 ns goes per handler call (rough split):
- function-pointer indirect call: ~3-5 ns
- recursive entry into `eval_node`: ~5 ns
- memo array reads + epoch compare + write: ~3 ns
- payload unpack from Vec/HashMap inside payload: ~3 ns

No single dominant component — it's the *whole closure-style runtime*,
not one piece. Removing the function-pointer indirection (Phase 3 (a)
hand-emit wasm with the same shape) trims ~5 ns at best; gets us to
maybe ~10 ns/call = 10.5 µs/tick = ~95K ticks/s. Still 3× slower
than v1.

### What Phase 3 actually has to do

The v1 bytecode interpreter pays ~0.09 ns/op because each op is a
match arm with bounds-checks elided, slot indices read from
contiguous memory, and `pc++` instead of recursive descent. **That's
the cost ceiling we're shooting for, not the closure-tree's
~10 ns/call.**

The way to get there from the v2 DAG: **per-terminal codegen with
inlining**. For each `WriteVar` terminal:

1. Collect the reachable cone (closure-codegen already lowers each
   node, so we have the per-node payloads).
2. Topologically order the cone — every node's deps are emitted before
   it. With topo order, no memo is needed; each node is computed
   exactly once into a local before its consumers run.
3. Emit one straight-line block (wasm function or Rust closure):
   per node, generate "compute X into local L_X" code. Lits become
   immediate operands. Calc becomes one `i32.add` / `i32.mul` op.
   LoadVar becomes one `state.read_mem(slot)` call. No recursion,
   no function pointers, no memo bookkeeping.

Theoretical ceiling under this plan: 1,052 nodes/tick × ~0.5 ns/node
(closer to v1's per-op cost, accounting for wasm vs native overhead)
≈ **2 µs/tick** = ~500K ticks/s on rogue. Even capturing 30% of that
beats v1.

The Phase 3 codegen target is clear now: **straight-line per-terminal
emission, not per-node functions**. This is what the closure prototype
ruled out *before* we sank wasm time on the wrong shape.

### Updated Phase 3 sequence

Old (from mission doc): (c) closures → (a) hand-emit wasm.
Revised based on this measurement:

1. **(c+) Inlined-Rust prototype**: same shape as the wasm endpoint,
   easier to debug. For each terminal, generate a Rust closure that
   inlines its reachable cone (no recursion, no memo). Measure
   ticks/s; should clear v1 if the model is right.
2. **(a) Hand-emit wasm**: only if (c+) clears v1. Runtime is
   browser/wasmtime, codegen shape is the same as (c+).

(c+) is a one-week prototype. (a) is the multi-week shipping work.
The codegen target finally has an honest hypothesis behind it.

### Artefacts

- `crates/calcite-core/src/dag/closure_codegen.rs` — function-pointer
  executor + visit-count `stats` module.
- `crates/calcite-core/src/eval.rs` — `Backend::DagV2Closures` variant
  + `dag_v2_closures_tick` driver (mirrors `dag_v2_tick`'s phase
  structure; broadcasts delegate to walker helpers).
- `crates/calcite-core/tests/v2_closures_differential.rs` — 4
  synthetic differentials vs the v2 walker.
- `crates/calcite-cli/src/bin/probe_v2_speed.rs` — three-backend speed.
- `crates/calcite-cli/src/bin/probe_v2_flame.rs` — speed + visit-count
  breakdown (atomic counters in `closure_codegen::eval_node`,
  enable/disable via `stats::enable()`).

---

## 2026-05-01: v2 Phase 2 — recognisers landed, gate cleared

Four recognisers on top of the Phase 1 walker. All bit-identical to v1
on the cabinet differential; conformance suite + smoke tests green;
wasm-pack clean.

Reachable-node count on rogue/bootle/zork1 drops 98–99% vs the
unrecognised baseline. `dag.nodes.len()` barely moves (the arena keeps
unreferenced nodes), but the walker only visits reachable nodes, so
that's the metric Phase 3 codegen will consume.

| Recogniser | Idiom | What it matches | Real-cabinet firing |
|---|---|---|---|
| Priority-head + Switch peel | 9 + 1 | `If` whose tail is a same-key integer-literal cascade | +14 Switches/cabinet, max single-If branches 60→14 |
| `BitField` super-node | 7 | `@function` body shape `mod`/`pow2`/`round-down`-`div` | 964–1042 BitFields/cabinet (~40% of Calls collapse) |
| `LoadVarDynamic` peel | 12 (read side) | `Switch` with ≥4 LoadVar bodies on one (key, slot) line | 3 LoadVarDynamics/cabinet, Switch entry total 430K→63K (rogue), 795K→3.5K (bootle/zork) |
| `BitwiseOp` super-node | 8 | `@function` body shape: 16-bit decomposition + reconstruction sum | 259–282 BitwiseOps/cabinet |

Speed sanity (rogue.css, 500 warmup + 2K bench):
- v1 (Bytecode):  ~315K ticks/s — fully-optimised interpreter, baseline.
- v2 after Phase 1: ~44K ticks/s (7.3× slower than v1).
- v2 after Phase 2: ~93K ticks/s (3.4× slower than v1).

Per the mission doc Phase 2 has no perf gate (perf wins live in Phase 3
codegen), but the ~2× v2 speedup from BitwiseOp alone is a useful
signal that the recognisers are doing real work, not just shuffling
node IDs.

### Not accidentally cheating

The catalogue's cardinal rule (calcite knows nothing above the CSS
layer) is easy to violate by accident when writing recognisers. One
near-miss this session: the LoadVarDynamic peel was originally drafted
with a doc-comment that named kiln, "memory cells", and "Doom-class
cabinets" as motivation, and would have been gated on a >99% LoadVar-
fraction threshold tuned to what the cabinets in `output/` happen to
emit. That's overfitting to the emitter, not a structural recogniser.

The version that landed describes the structural shape only —
"`Switch` whose entry bodies are arithmetic-progression `LoadVar`s" —
and uses the same ≥4-entry threshold the rest of the dispatch
recogniser uses. Threshold isn't tuned to cabinet size; the peel
fires on any size that meets the dispatch minimum. The doc-comment
describes the CSS shape, not what kiln does with it.

Operational test that caught it: imagine a brainfuck cabinet, a 6502
cabinet, a calculator cabinet, a rules-engine cabinet. If the
recogniser only makes sense for "the thing kiln emits for the 8086
cabinet's `--readMem`", it's encoding a cabinet into calcite. If the
shape is something a 6502 cabinet's `--readByte` would also produce
(yes, because there's no other CSS shape for an address-keyed lookup),
the recogniser is structural and ships.

Worth re-reading the catalogue's per-idiom Genericity Probe before any
future Phase 2 work — the failure mode is naming what kiln does in
your head and writing the recogniser around that, even when the code
looks shape-only.

### Stopping point for Phase 2

Reasonable place to pause and start a Phase 3 codegen spike. The
remaining catalogue idioms split into:
- Already absorbed: idioms 3, 4 (broadcast/packed broadcast) via v1
  pattern recognisers feeding `IndirectStore`/`DagBroadcast`.
- Trivially absorbed or low-value: idioms 2 (self-hold), 5 (compound-
  AND), 6 (wide-value split residue after kiln's emit-time fuser).
- Phase 3 codegen passes, not Phase 2 shape-matchers: idioms 11
  (pure-region tagging for CSE), 14 (constant fold).

The ≥30% node-count gate from the mission doc is met (98–99% reachable-
node reduction on the cabinets measured). Whether more recognisers
are needed is best answered by a Phase 3 wasm codegen prototype: if
specific shapes blow up the wasm size or codegen time, those tell us
which Phase 2 work is still owed. Adding more recognisers without a
codegen target to inform them is guessing.

Artefacts:
- `crates/calcite-core/src/dag/types.rs` — `BitField`, `LoadVarDynamic`,
  `BitwiseOp` super-nodes.
- `crates/calcite-core/src/dag/lowering.rs` — recognisers + body
  classifiers; reuses v1's `classify_bitwise_decomposition`.
- `crates/calcite-core/src/eval.rs` — three new walker arms.
- `crates/calcite-core/tests/v2_cabinet_differential.rs` — synthetic
  v1-vs-v2 differentials for each recogniser.
- `crates/calcite-cli/src/bin/probe_dag_node_counts.rs` — node-kind
  bucketing + reachability sweep.
- `crates/calcite-cli/src/bin/probe_v2_speed.rs` — v1-vs-v2 throughput
  comparison.

---

## 2026-04-30: v2 DAG backend — Phase 1 correctness landed

v2 produces identical state to v1 for ≥2000 ticks on `hello-text.css`,
and the Phase 0.5 primitive conformance suite (Chrome as oracle) passes
under both backends with the same 41-pass / 5-skip / 3-xfail split.
Phase 1 acceptance gates from `docs/v2-rewrite-design.md` are met:
Phase 0.5 conformance ✓, `cargo test -p calcite-core` green ✓,
`wasm-pack build` clean ✓.

The work that landed since the previous v2 self-review:

- **Three-phase tick split** in `dag_v2_tick`: scalar WriteVars
  populate cascade caches → caches commit to State → broadcasts run.
  Mirrors v1's `compile.rs::execute` ordering. Required so that a
  `LoadVar { kind: Prev }` reading mid-tick sees pre-tick committed
  state, not a partially-applied scalar write.
- **Native broadcast value evaluation.** Both unpacked
  (`BroadcastWrite`) and packed (`PackedSlotPort`) ports lower at
  build time to v2-native records (`DagBroadcast`, `DagPackedBroadcast`)
  with every property pre-resolved to a slot and the value expression
  lowered to a NodeId. The walker calls v2's broadcast executors,
  not v1's interpreter — closes the v1-bridge that was on the hot
  path before.
- **Packed splice updates `state.memory[]` in lock-step** with the
  cell state-var, matching v1's `packed_splice_byte` /
  `packed_splice_word_aligned`. Without this, readers that go through
  `state.memory` directly (differential test, renderer, debugger) saw
  stale zeros even though state-vars were correct.
- **Sibling Calls no longer collide** in the per-tick memo. Epoch is
  now a globally-monotonic counter on `Evaluator`; each Call entry
  bumps it and saves/restores the caller's local tag. Caught by a
  synthetic reproducer (`--inner(x)` and `--inner(x+1)` in the same
  caller).
- **Function bodies as shared sub-DAGs** with `Param`/`Call` nodes
  (replacing the inlining-per-call-site approach). One body, multiple
  call sites, frame-keyed parameter resolution.

What's next, per `docs/compiler-mission.md` Phase 1 → Phase 2:

- **Phase 1 cleanup.** Bridge debt to remove: the `FuncCall` variant
  (dead in any correct program now that `Call` exists), the
  `name_to_slot` map (used only by the v1 bridge for transient reads,
  redundant once the bridge is gone), `absorbed_properties`
  (lowering-time set leaking into the runtime `Dag` struct).
  Mechanical removals; no design questions.
- **Phase 2 idiom recognisers.** First deliverable is the catalogue
  itself — read `kiln/emit-css.mjs` and enumerate the structural CSS
  idioms (broadcast write, packed-cell write, conditional cascade,
  paragraph-relocate cascade, applySlot cascade, …). For each idiom:
  shape-matcher over the DAG + a normalised replacement super-node.
  Validation is bit-identical end-of-tick state under
  normalised-DAG vs raw-DAG, plus a meaningful node-count
  reduction. Per the mission, **node count is the intermediate metric;
  runtime perf stays interpreter-bound until Phase 3 codegen**.
- **Phase 3 codegen** is where perf wins live. Per
  `docs/compiler-mission.md`, "no perf gain expected" in Phase 1, and
  Phase 2's gate is node-count reduction, not ticks/second. Anything
  that motivates v2 work with a `v1 vs v2 ticks/sec` ratio is solving
  the wrong problem — the comparison that matters is v2 vs Chrome on
  the conformance suite, plus eventually doom8088 wall-clock per the
  mission's headline target.

---

## 2026-04-16: big round of peephole/specialization wins

Cumulative commits:
- 8e2ccd8 — fuse LoadState + BranchIfNotEqLit
- 32e8479 — skip per-tick state_vars.clone() in run_batch
- 1930b52 — skip per-tick slot zeroing in execute()
- a7b1625 — dense-array fast path for DispatchChain (had latent bug)
- cb4cbae — AddLit/SubLit/MulLit variants
- 2ccceb2 — AndLit/ShrLit/ShlLit/ModLit variants
- ae1ae51 — fix: flat_table targets weren't remapped in fuse_loadstate_branch

Measured rogue.css: ~6K → ~190K ticks/s (**~32×**).
Measured fib.css: ~7K → ~280K ticks/s (**~40×**).
Measured bootle.css: ~7K → ~190K ticks/s (**~26×**).
Measured splash-fill (bootle-ctest.css): ~17s → ~11s (1,828,538 conformant
ticks at ~170K t/s).

Biggest single win was the dense-array DispatchChain. Most other wins are
5–15% each.

**Lesson: verify halt/conformance on every perf commit.** The dense-array
bug was invisible from pure ticks/s numbers (they got better) but the
program was producing wrong results post-fusion. Caught only when I
noticed `Cycles: 0` in the final bench writeup. Add cycleCount/halt
sanity checks to the perf workflow.

Session notes (meta): most "regressions" of smaller changes were
thermal-throttle noise; once the machine cooled down, several were
actually neutral. Consistently alternating change/baseline runs
interleaved is the only reliable way to separate signal from noise
on this laptop. Three runs per side, take the mean.

### Things that didn't work (saved for reference)
- LoadSlot + BranchIfNotEqLit fusion: only 682 fusions found, neutral.
- Shrinking Op enum (Box<Vec<Slot>> for Min/Max): 32→24 bytes regressed bench.
- LTO=thin: neutral; LTO=fat: -30%.
- MIN_CHAIN_LEN=3: regression (3-chain lookup slower than linear compare).
- LoadStateVar specialization: regressed (match dispatch pressure outweighed).
- Local copy-propagation / dead LoadSlot elim: either neutral (no dead-code
  pass) or broke or-conditions test (too aggressive liveness heuristic).

---

## Current priority: Mode 13h blitting is painfully slow

Filling the 320×200 framebuffer once takes roughly a minute — you can
watch the pixels scroll into place. Even after the compound-AND fuser
landed (3× overall speedup, see below), the mode 13h splash in
`bootle-ctest.css` is nowhere near usable. We need something like a 100×
improvement on tight inner loops like this.

Working through candidates in [docs/optimisation-ideas.md](optimisation-ideas.md).
Stacking order: ~~native bitwise recognition~~ (done) → dead LoadLit
sinking → wider dispatch-chain recognition → change-gated ops → affine
self-loop fixed-point recognition (the big structural move) →
value-keyed region memoisation.

---

## 2026-04-15: Native 16-bit bitwise recognition (idea (a))

CSS-DOS `--and`, `--or`, `--xor`, `--not` are 32-local bit-decomposition
functions (16 `mod(round(down, var/2^k), 2)` ops per input, then a
16-term reconstruction sum). Each call compiled to ~100 ops.

Added a body-shape recogniser `classify_bitwise_decomposition` and four
native ops `BitAnd16` / `BitOr16` / `BitXor16` / `BitNot16`. The
recogniser never looks at function names — it matches on the bit-extract
shape of the locals and the per-bit combine pattern in the result sum
(`a*b` → AND, `min(1,a+b)` → OR, `min(1,a+b)-a*b` → XOR, `1-a` → NOT).

Result on bootle-ctest cold (300K ticks):

- Previous baseline: 134K ticks/s (17.1% of 8086), 2472 main-stream ops/tick.
- After: **259K ticks/s** (32.3% of 8086), **1342 main-stream ops/tick**.
- 1.9× speedup; 45% fewer ops per tick.
- Conformance: compile vs interpret diffs at ticks 500K/1M/2M are
  unchanged (6/10/3) vs baseline — those are a pre-existing divergence,
  not introduced by this change.

---

## 2026-04-15: Flat-array fast path for single-param literal dispatch

### Context

CSS-DOS's rom-disk feature emits `@function --readDiskByte(--idx)` with
one branch per disk byte — ~68K branches for a ~68 KB floppy, ~1.5M
for Doom8088 (future). The generic dispatch compile path at
`compile_dispatch_call` walks every entry and produces a per-entry
`Vec<Op>` sequence, which froze compile for tens of minutes and blew
through 48 GB of RAM on bootle+rom-disk.

### Change

Wired up `Op::DispatchFlatArray`, a pre-existing-but-inert op. The
builder (`try_build_flat_dispatch`) fires when a dispatch has:

- ≤1 parameter, non-empty,
- every entry is an integer literal representable as i32,
- fallback is an integer literal,
- `max_key - min_key + 1 ≤ 10_000_000` (caps worst-case array at ~40 MB).

When all guards pass, the whole table compiles to a single `Vec<i32>`
stored on the program, and the call site becomes a single op doing a
bounds-checked array index. Multi-parameter, non-literal, and sparse
dispatches fall through to the old path unchanged.

Also added a name-keyed cache: repeated call sites of the same function
share the same array. Critical because the rom-disk window has 512
dispatch sites (one per byte in the 0xD0000–0xD01FF window), each
calling `--readDiskByte`.

### Results

Rogue (unrelated to rom-disk, just the standard benchmark):
- Compile: unchanged (no literal single-param dispatches in plain rogue).

Bootle + rom-disk (457 MB CSS, 723K properties, 1.45M assignments,
65 functions including the 56794-entry `--readDiskByte`):
- Parse: 4.7s
- Compile: **29s → 16s** after the name-keyed cache fix; was previously
  frozen indefinitely with the 48 GB allocation on the 2-parameter
  form, and ~79s on the first 1-parameter form before the cache.
- 1 tick: 74 µs.
- Bootle boots end-to-end through the rom-disk path, verified live in
  the interactive CLI.

### Open follow-ups

- Profile the runtime cost of the array lookup under heavy INT 13h load
  (REP MOVSW through the window does 256 reads per sector). Should be
  negligible vs. the slow-path HashMap it replaces.
- Retest Zork+FROTZ (~284 KB disk → ~284K dispatch branches); within
  the i32 literal + 10M span guards, so should take the fast path.

### Other changes bundled with this

Unrelated to the fast path but shipped together:
- `calcite-cli` gained an interactive program picker (grid menu,
  arrow-key navigation) when invoked without `--input`.
- Parse and compile phases now render progress bars to stderr (can be
  disabled with `CALCITE_NO_PROGRESS=1`).
- `--ticks` is now optional; omitting it runs indefinitely in
  interactive mode.

---

## 2026-04-14: V4 baseline + bottleneck analysis

### Context

CSS-DOS v4 landed. It boots rogue, fib, bootle, etc. in ~300K ticks.
Current speed: **~4800 ticks/s on rogue** (0.9% of real 8086 at 4.77 MHz).
Goal: much faster.

### Profiling infrastructure added

Added `calcite-bench` (headless benchmark binary) with `--profile` for
granular per-phase and per-op-type breakdown. See
[docs/benchmarking.md](benchmarking.md) for usage.

### Top-level phase breakdown

Profiled rogue.css, 2000 ticks after 200 warmup:

| Phase | % of tick | Avg/tick |
|---|---|---|
| **Linear ops** | **91.8%** | 231us |
| Dispatch lookups | 5.1% | 13us |
| Change detect | 2.2% | 5us |
| Broadcast writes | 0.5% | 1us |
| Writeback | 0.2% | 0.4us |
| Everything else | <0.2% | — |

**All optimisation work should target the bytecode interpreter loop
(`exec_ops` in `compile.rs`).**

### Op frequency breakdown

The 102K ops/tick break down as:

| Op | Per tick | % of ops |
|---|---|---|
| **LoadLit** | 34,475 | 33.3% |
| **BranchIfZero** | 34,140 | 33.0% |
| **CmpEq** | 34,118 | 33.0% |
| LoadSlot | 196 | 0.2% |
| Mul | 114 | 0.1% |
| Add | 77 | 0.1% |
| everything else | <65 | <0.1% each |

**99.3% of all ops are three instructions in equal proportion:**
`LoadLit + CmpEq + BranchIfZero`.

### What this means

The CSS has ~34K if-chains per tick, each checking `if(style(--prop: N))`.
Per tick, exactly **one** of these 34K matches. The other 33,999 all fail
and branch over immediately.

This is the compiled form of CSS patterns like:

```css
--result: if(style(--opcode: 0)) { ... }
          if(style(--opcode: 1)) { ... }
          if(style(--opcode: 2)) { ... }
          /* ... 34K more ... */
```

Each `if(style(--prop: N))` compiles to:

```
LoadLit   slot[X] = N          # the constant to compare
CmpEq     slot[Y] = (slot[prop] == slot[X])
BranchIfZero slot[Y] → skip    # jump past body if no match
... body ops ...               # only reached for the one match
```

The pattern recogniser already converts `if(style())` chains into dispatch
tables (HashMap lookups) when it detects ≥4 branches on the **same property**.
But these 34K comparisons are **not being caught** — either because they test
different properties, or because the chain structure doesn't match the
recogniser's expectations.

### Optimisation directions (in order of expected impact)

1. **More aggressive dispatch table recognition.** If the pattern recogniser
   can catch these 34K if-chains, they collapse to one HashMap lookup per
   tick. That would eliminate >99% of ops. This is the 100x opportunity.

2. **Fused CmpEq+Branch op.** Since CmpEq is always followed by
   BranchIfZero on the same result slot, fuse them into
   `BranchIfNotEqual { a, b, target }`. Eliminates the intermediate slot
   write and halves the match-dispatch overhead for uncaught patterns.

3. **Skip-chain optimisation.** Recognise runs of `LoadLit + CmpEq +
   BranchIfZero` all testing the same slot and emit a dispatch at compile
   time.

### Branch statistics

- 34,140 branches per tick
- 99.9% taken (i.e., the condition is zero → jump)
- Only 47 branches per tick fall through (the one matching case + a few others)

This is a massive dead-code-skip pattern. Most of the bytecode stream
exists only for the rare tick where that particular property matches.

### Key numbers to track

When making changes, run:

```sh
cargo run --release --bin calcite-bench -- -i output/rogue.css -n 5000 --warmup 500
```

Baseline (2026-04-14, no profile overhead):
- **rogue.css**: ~4800 ticks/s, 0.9% of 8086
- **fib.css**: ~5300 ticks/s, 1.0% of 8086
- ~204us/tick (rogue), ~187us/tick (fib)

---

## 2026-04-14: Fused CmpEq+Branch (BranchIfNotEqLit)

### Investigation

Profiling showed 99.3% of ops were `LoadLit + CmpEq + BranchIfZero`
triplets. Investigation revealed:

1. **Dispatch table recognition is working** — ~179 tables with 34,850
   entries are created. Only ~100 small chains are missed (compound
   conditions and multi-property tests).

2. **The 34K branches per tick come from inside dispatch table entries**
   and the main bytecode stream's linear if-chains. Each opcode entry
   contains further if-chains for register selection, addressing modes,
   etc. These are too small or complex for dispatch table recognition.

3. **402K ops in the main stream, 1.13M ops in dispatch entries** — most
   are the same `LoadLit + CmpEq + BranchIfZero` triplet pattern.

### The fix: `BranchIfNotEqLit` fused op

Added a peephole pass (`fuse_cmp_branch`) that runs after compilation and
before slot compaction. It scans for the pattern:

```
LoadLit(dst=X, val=N) → CmpEq(dst=Y, a=P, b=X) → BranchIfZero(cond=Y, target=T)
```

and replaces it with a single:

```
BranchIfNotEqLit(a=P, val=N, target=T)
```

This eliminates two intermediate slot writes and two match-dispatch cycles
per branch test. The pass also fuses inside dispatch table entry ops and
broadcast write ops.

### Results

| Program | Before | After | Improvement |
|---|---|---|---|
| rogue.css | ~4800 ticks/s | **6054 ticks/s** | **+26%** |
| fib.css | ~5300 ticks/s | **7066 ticks/s** | **+33%** |
| bootle.css | — | **7214 ticks/s** | — |

Main-stream ops per tick dropped from **102K to 35K** (3x reduction).
96.4% of remaining ops are now `BranchIfNotEqLit`.

Correctness verified: bootle shows hearts, rogue boots to DOS.

### Remaining opportunities

The dispatch table entries still execute unfused ops (80 sub-ops per
dispatch on average). The fused op handles the main stream and entry
ops arrays, but dispatch entries that are executed via `exec_ops` (the
non-profiled path) also benefit from the fusion.

Next directions:
- The 35K `BranchIfNotEqLit` ops per tick are still the bottleneck.
  These are all testing the same few properties against different
  constants. A sorted-array binary search or perfect hash at compile
  time could collapse them further.
- Multi-property dispatch: chains testing `--_tf` and `--opcode`
  together could be split into nested dispatches.

---

## 2026-04-14: Unchecked slot indexing in exec_ops hot loop

### The fix

Switched all `slots[idx as usize]` accesses in `exec_ops` (compile.rs) to
`unsafe { *slots.get_unchecked(idx) }` / `get_unchecked_mut`, hidden behind
`sload!` / `sstore!` macros with `debug_assert!` for debug builds. Also
unchecked the `ops[pc]` load and the `dispatch_tables[table_id]` lookup.

Safety rests on invariants the compiler already guarantees: every slot
index in an op is `< slot_count`, every branch target is `<= ops.len()`,
every `table_id` is a valid index. Debug builds still check.

Also reordered the match so `BranchIfNotEqLit` (96% of ops) is the first
arm — helps the compiler lay out the jump table favorably.

### Results

| Program | Before (BranchIfNotEqLit only) | After (unchecked) | Improvement |
|---|---|---|---|
| rogue.css | ~6316 ticks/s | **~7100-7400 ticks/s** | **+13-17%** |
| fib.css | ~7066 ticks/s | ~7900 ticks/s | +12% |
| bootle.css | ~7214 ticks/s | ~7450 ticks/s | +3% |

Run-to-run noise on Windows is ~±5%. The rogue gain is reliably above +10%
across multiple runs.

Correctness: all 27 core tests pass; rogue boots to the same
`IP=890, 404767 cycles` state as before at tick 30000.

### Why it helps

The bytecode interpreter runs `match` + slot index on every op, 34K ops/tick.
Each `slots[idx as usize]` does a bounds check (compare, branch). LLVM
cannot elide these because slot indices come from the op struct, not a loop
induction variable. Skipping them is safe because the compiler statically
allocates slots and never emits an out-of-range index.

### What did NOT work

- **Hot-chain peel** inside the `BranchIfNotEqLit` arm (speculatively
  handle adjacent BranchIfNotEqLit ops in an inner loop without re-entering
  the match). Tested: slowed rogue from 7100 → 6500. The extra inner branch
  disrupted branch prediction and the compiler couldn't inline cleanly.
  Reverted.

### Session notes (meta)

- The session started with a stale diagnostic reporting "takes 4 arguments
  but 5 supplied" that pointed at an exec_ops call site which was actually
  correct. The build succeeded; the diagnostic was cached. Worth remembering
  to trust `cargo build` over rust-analyzer's live diagnostics when they
  disagree.

---

## 2026-04-14: Dispatch-chain recognition (DispatchChain op)

### The fix

Added a new op `DispatchChain { a, chain_id, miss_target }` and a
post-fusion pass `build_dispatch_chains` that detects runs of
`BranchIfNotEqLit { a: P, val: V_i, target: T_i }` where each miss target
points at the next branch-on-same-slot, and collapses them to a single
HashMap lookup.

Implementation sketch:
- For each `BranchIfNotEqLit` at position `i` on slot `P`, walk the chain
  by following each `target`. If the target points to another
  `BranchIfNotEqLit` on the same slot, extend the chain.
- If ≥ `MIN_CHAIN_LEN` (currently 6) branches accumulate, build a
  `DispatchChainTable: HashMap<i32, u32>` mapping each `V_i → body_pc`
  (body_pc = branch_pc + 1).
- Replace `ops[i]` with `DispatchChain`. Leave the rest of the chain in
  place as dead code — keeps any external jumps into the middle of the
  chain valid.

At runtime, `DispatchChain` reads `slot[a]`, looks up in the table, and
either jumps to the matching body PC or to `miss_target`. Eliminates up to
~30 `BranchIfNotEqLit` ops per chain per tick.

### Results

Steady-state benchmark on rogue (20K ticks, 5K warmup):

| Metric | Before | After |
|---|---|---|
| Ticks/s | ~7,100 | **272,194** |
| Cycles/s | 67 KHz | **4.35 MHz** |
| % of 4.77 MHz 8086 | 1.4% | **91.2%** |
| μs/tick | 140 | **3.7** |

Short 5K-tick bench (warmup 1K): 137K-151K ticks/s on rogue/fib/bootle
(all previously 7-8K).

Longer 30K-tick run from cold boot shows smaller apparent gains
(5559 ticks/s), because boot code contains many ops outside dispatch
chains and dominates that window. Steady state hot-loop performance is
where the big win materialises.

### Correctness

- All 27 core tests pass.
- Rogue at tick 30000: IP=890, 404,767 cycles — **identical** to baseline.
- Rogue at tick 60000: 832,808 cycles — consistent with baseline (tick
  counts are deterministic so any logic bug would diverge these).

### Design notes

- Dead-code approach (leave intermediate branches in place) was chosen
  over removal because branch targets from elsewhere in the program may
  point into the middle of a chain; tracking and patching those is extra
  complexity for no runtime gain (dead code isn't executed).
- `MIN_CHAIN_LEN = 6` is a rough guess — HashMap lookup is ~30ns, linear
  match-dispatch is ~5ns/op, so break-even is around 6. Could be tuned.
- Chain tables live in `CompiledProgram::chain_tables`, threaded through
  `exec_ops`, `exec_ops_profiled`, and `exec_ops_traced` as an extra arg.

### Session notes (meta)

- Adding a new `Op` variant touches 6+ `match` sites (op_slots_read,
  op_dst, map_op_slots, seed_from_parent, three exec_ops variants). rust-
  analyzer diagnostics for "non-exhaustive patterns" are reliable here —
  use them as a checklist. The function-arity diagnostics stayed stale
  longer than the pattern ones; trusting `cargo build` still pays off.

---

## 2026-04-14: Interactive CLI path was throttling the evaluator

### The discovery

After the two evaluator wins above, the bench reported 270K+ ticks/s but
running the same program via `run.bat` showed only ~5–12K ticks/s. The
user (correctly!) pushed back that they weren't seeing the speedup.

Root cause: `run.bat` invoked `calcite-cli` with `--halt halt`, and the
CLI's `needs_per_tick` check (`cli.verbose || halt_addr.is_some() ||
(interactive && screen_interval > 0) || !key_events.is_empty()`) forced
the per-tick loop. That loop did one `evaluator.tick(state)` plus one
`crossterm::event::poll(Duration::ZERO)` syscall per simulated tick. On
Windows, the event poll is ~1–5 μs per call — which used to be drowned
out by a 140 μs tick, but after the optimisations each tick is ~4 μs, so
the keyboard poll became the dominant cost.

### The fix

Two changes to run.bat / calcite-cli:

1. **run.bat**: Dropped `--halt halt` from the rogue/general program
   launch — unnecessary for interactive use and the main trigger for
   per-tick mode.
2. **calcite-cli**: Rewrote the interactive loop to run in configurable
   batches (`--interactive-batch`, default 50,000). Between batches, the
   CLI still polls keyboard, fires `--key-events`, re-renders the screen,
   and checks `--halt`. Between ticks within a batch, none of that
   happens — `run_batch` goes full-speed. Scripted key events force the
   batch to shrink so they fire at the correct tick; held-key BDA refill
   runs once per batch (BIOS INT 16h busy-spin still sees the key in
   time — 50K ticks ≈ 10.5 ms of sim time, well inside a human keypress).

Physical intuition: at 4.77 MHz the 8086 executes ~80K instructions per
60 fps frame, so a 50K-tick batch is ~10 ms of sim time — imperceptible
for input latency. Earlier default screen_interval=500 ran renders 160×
per sim frame for no benefit.

### Results

Same rogue.css, same 500,000 ticks, via `calcite-cli` (not bench):

| Config | Before | After |
|---|---|---|
| `--screen-interval 500` (default) | ~12,000 ticks/s | **245,000–287,000 ticks/s** |
| `--screen-interval 50000`         | ~12,000 ticks/s | **peaks 346,000 ticks/s (4.36 MHz = 91% of 8086)** |

Cycle count (6,290,226) identical to baseline — no correctness regression.

### Status-line readability

User asked to stop the speed readout from glitching between units (KHz ↔
MHz, different widths). Replaced the live status line with:

- **Fixed-width formatting**: always `X.XX MHz` (8-char wide) regardless
  of speed. No more "100 KHz" ↔ "1.0 MHz" width flips.
- **EMA smoothing** (α = 0.3) on ticks/s so noise doesn't flicker the
  display.
- **Refresh throttle**: the rendered status text only updates every 500 ms
  of wall time; the screen itself still repaints every `screen_interval`
  ticks, but the speed digits stay put between status refreshes.

### Session notes (meta)

- The user's debugging instinct was right before my analysis caught up. I
  built a theory (`event::poll` overhead dominates) and was about to
  write another optimisation when the user asked "isn't it just the
  batch mode thing?" Looking again: the CLI's `run_batch(cli.ticks)`
  path only activates when `needs_per_tick == false`, and any of
  `--halt`, screen_interval > 0, verbose, or scripted key events trips
  it. Answering user doubts honestly saves wall time — I did not need to
  write more code to reach the diagnosis.
- Feature-flagging rule of thumb for this project: if something "should"
  be fast per the bench but isn't in a real run, look first at whether
  the CLI wrapper is forcing a slow path.


## 2026-04-17 — Memoisation viability: Probes 1-4 + runtime period projector prototype

Spec: `docs/superpowers/specs/2026-04-17-memoisation-viability.md`.

### Probes summary

Four probes built as `probe-splash-{memo,trace,affine,period}` binaries
against bootle-ctest.css. First three (per-tick value-keyed memoisation;
LuaJIT-style trace specialisation; consecutive-tick affine store detection)
are **dead ends** — the data rules them out.

Probe 4 (loop-period autocorrelation over the fingerprint stream) **found
the real signal**: splash-fill is a 26-tick microcode iteration, 99.6% of
the splash phase, one affine memory write per iteration (`base +
iter * 1`, constant value = pixel colour). See spec for numbers.

### Runtime projector prototype

New module `crates/calcite-core/src/tick_period.rs` + bench binary
`probe-splash-project`. Pipeline:

1. **Cold phase**: collect 4096 samples (`(pre_tick_vars, first_mem_write)`).
2. **Calibration**: identify "cyclic" slots (≤ min(32, len/64) unique values);
   vote per cyclic slot for its best absolute-value period under
   autocorrelation. Quorum = ≥ half the voting slots agreeing.
3. **Affinity verification**: across `CONFIRM_ITERS+1` candidate iterations,
   verify each state var evolves as `base + k * per_iter_delta` and each
   offset's memory write evolves as `(base_addr + k * addr_stride,
   constant_value)`. Non-affine vars are only tolerated if their delta is 0.
4. **Projection**: at an iteration boundary, advance state vars scalarly
   and fill memory with a `memset` over `N` iterations of writes.
5. **Validation**: after projection, run one real iteration and check that
   post-iteration state matches `anchor + (iters_since_lock+1) * delta`
   absolutely. Miss → cooldown 64 ticks, re-enter Cold.
6. **Rollback**: driver snapshots `state.memory`, `state.state_vars`,
   `state.extended`, `state.string_properties`, `frame_counter` before
   every projection; on validation miss, restores all five.

### Current status — honest assessment

**Correctness: bit-identical to baseline.** Halt tick, memory hash, and
state_vars hash all match baseline (`1828538 / 94e2a9a5d967e282 /
a7d99bf7857452b2`) on bootle-ctest end-to-end.

Early attempts had silent state drift: memory hashed correctly (rollback
caught every miss) but halt tick drifted by 104 and state_vars hash
mismatched. Fixed by changing `validate_iteration` to compare **absolute**
state against `anchor + iters * delta`, not just the incremental `post ==
pre + delta`. Under an affine workload, one real iteration advances by
`delta` from ANY starting state — incremental check is trivially true
even when the projected `pre` is wrong. The absolute check catches it.

But the projector still doesn't pay off:
- **156 of 157 locks miss validation.** The detector locks after
  CONFIRM_ITERS=3 iterations of affine behaviour, but 4 iterations is not
  enough evidence that the next N will ALSO be affine. The spec's data
  backs this up: longest P=26 contiguous run is only 8292 ticks (318
  iterations), so locks happen mostly on shorter runs where projection
  quickly outruns the regime.
- **Net speedup: 1.02×, inside noise.**

This is a prototype, not a ship-ready optimisation. Remaining work to get
a real win:

1. **Stronger lock gate.** Require CONFIRM_ITERS ≥ 10 and a high match
   ratio in the full calibration window (not just the anchor region). In
   theory this moves the false-positive rate below the disruption rate,
   so locks stick.
2. **Reduce calibration cost.** O(n_vars × max_period × window) per
   attempt is ~5ms per 4096 ticks at current settings — on par with the
   tick cost itself. Only recompute when `state_vars` hash rings a bell
   (i.e., skip calibration while the workload looks unchanged).
3. **Smarter projection budget.** Current code doubles on success, resets
   to 4 on miss. In a workload where ~1 in 300 iterations is a disruption,
   the expected-value-optimal starting budget is much higher — but we need
   the lock gate to be reliable first.
4. **Consider whether the detector overhead can be made pay-per-use**: if
   Cold-mode observation costs > compiled-tick cost, we're net-negative.
   The probe's observation adds a state_vars.clone() + first-mem-write
   scan per tick — that's already a significant fraction of tick cost.

### Artifacts

- `crates/calcite-core/src/tick_period.rs` — detector + projector.
- `crates/calcite-cli/src/bin/probe_splash_{memo,trace,affine,period,project}.rs`
  — the four research probes + the end-to-end projector driver.
- `crates/calcite-cli/src/bin/probe_{full_vs_sub,cyclic_slots}.rs` —
  diagnostic probes for fingerprint-strategy selection (kept for future
  reference; they were how I discovered that delta-fingerprint gives 45%
  match rate while a subset-of-cyclic-slots absolute fingerprint gives
  100%).
- `probe.*.log` — raw probe outputs (not committed).

### What the prototype establishes

- The detection pipeline (fingerprint → voting → affine verify) does find
  P=26 on bootle-ctest in a 4096-sample window. With a wider window it
  would also find the harmonics (52, 78, 104, …).
- The rollback pathway keeps memory bit-identical across projection
  attempts even when the projection is wrong.
- The spec's 20–30× upper bound is **not** demonstrated by this code —
  the validation gap collapses every projection back to a rollback.

The easy validation fix landed (absolute-anchor comparison). The hard
remaining work is detector discipline: locks are firing far too
optimistically, so almost every projection gets rolled back. A longer
confirm window or a more stringent agreement threshold should move the
false-positive rate below the workload's natural disruption rate — at
which point the 20–30× upper bound from the spec becomes reachable in
principle. Until that lands, the detector is instrumentation, not
optimisation.


## 2026-04-18 — Splash fill: REP STOSB rewrite + runtime projector stabilised + signature-based detector WIP

Two parallel workstreams this session. One shipped (on CSS-DOS side); one
partially landed (runtime projector fixes); one is research-quality code
that needs a final correctness pass before it's useful.

### CSS-DOS: REP STOSB splash rewrite — shipped

Commit `2acc748` on `../CSS-DOS/master`. `bios/splash.c`'s per-pixel C
loop for the dark-gray fill replaced with an OpenWatcom `#pragma aux
vga_fill` wrapper emitting `rep stosb`. **Splash ticks: 1,828,538 →
194,918. 9.4× fewer CSS ticks** for the 64,000-byte fill. Output CSS
rebuilt via `generate-dos-c.mjs`.

### Runtime projector (`PeriodTracker`) — correctness fix + opts

Prior session left the projector in a broken state: correct detection but
`project()` didn't cap N by counter-zero-crossing, so it over-filled past
REP STOSB end by ~1,500 bytes and corrupted post-fill memory. Fixed:

1. **Zero-crossing cap in `project()`.** For any state slot with non-zero
   delta, bound N so the slot doesn't cross zero during projection. Pure
   observation of slot values, no x86 knowledge — cardinal-rule-safe.
   **Result: memory hash matches baseline bit-identically.**

2. **Opt: no per-tick heap allocations in `observe()`.** `Sample.vars`
   boxes pre-allocated at construction; hot path just `copy_from_slice`s
   into the next ring slot. Probe's `pre_vars` clone replaced with a
   reusable `Vec<i32>` scratch.

3. **Opt: `CALIB_LEN` 4096 → 256** with scaled `MIN_MATCHES` (512 → 32)
   and a cyclic-threshold clamp of `(len/8).clamp(8, 32)`. Gives 16× more
   calibration attempts per workload; still passes the in-module tests.

4. **Opt: `INITIAL_BUDGET` 4 → 64.** Zero-crossing cap is now the safety
   net against overfill, so we can start projecting more aggressively.

**Combined result: stable ~1.25–1.30× median splash speedup (best
~1.42×), memory hash bit-identical to baseline, no missed validations on
the hot path.**

### Why only 1.30× — honest bottleneck analysis

Expected 5–10×, got 1.30×. Measured decomposition:

- **67% of splash** is burnt in Cold-mode calibration before lock. The
  voting-based autocorrelation needs many samples to disambiguate a real
  period from dispatchy-slot coincidences. With MIN_PERIOD=1 (necessary
  for the REP STOSB workload after the rewrite), only one in ~32
  calibration buffers lands with an affine-verifiable window — all the
  others fail `non-affine var with nonzero delta`.
- **Remaining 33%** is projected. Bulk memset saves ~30% of that 33% =
  ~10% of total. Plus some validation overhead saved from the small
  opts.
- **Tracker observation overhead** is only ~5% per tick after opt 1, not
  the ~18% I'd initially guessed from a noisy run.

The wall: lock timing isn't deterministic — it depends on buffer
end-alignment landing inside a phase of the microcode cycle where 5
consecutive tick-to-tick deltas happen to be self-consistent. That only
happens ~1/32 of the time. Shrinking the buffer gives more attempts but
doesn't change per-attempt success rate.

### Structural critique → signature-based cycle detector (WIP)

The real fix is to stop inducing cycles statistically from state_vars
autocorrelation and instead use structural execution signatures: two
ticks that wrote to the same set of state slots with the same
relative-address mem-write pattern did mechanically the same work.
Cycle period falls out in O(ticks), not O(ticks²).

New module `crates/calcite-core/src/cycle_tracker.rs`. Per-tick
signature = hash of (slot-change-set, relative-mem-write-offsets).
Last-seen-at map gives a period candidate in O(1); `CONFIRM_CYCLES=3`
cycles of matching confirms. Handles harmonics via a third-cycle
affine-consistency check at lock time.

**Detection works**: on the real workload (bootle-ctest), the tracker
locks on the period-4 REP STOSB cycle at tick **3,263** instead of tick
131,072. 40× reduction in time-to-lock, correctly identifies
addr_stride=+2/cycle and writes/cycle=2. Unit tests pass.

**Projection is broken**: after the harmonic-rejection gate, the detector
falls through to locking on a DIFFERENT pattern later in the trace (tick
130,948) and my hand-rolled `project()` writes wrong bytes. Memory hash
diverges from baseline. Phase alignment between captured anchor and
current state_vars is the source of the bug — I tried several fixes but
didn't converge in-session.

### Next step (unfinished)

The right shape of the fix: keep `CycleTracker`'s detection primitive
(it solves the real "observe for 131K ticks" problem); throw away my
hand-rolled `project()`; wire CycleTracker's output (period,
addr_stride, write_offsets, per_cycle_delta, anchor_vars) directly into
`PeriodTracker::Mode::Locked` to use the proven projection code. That
gives fast lock + correct project. Expected: the 5–10× that didn't land
this session.

### Honest scope comparison with the original brief

Original brief (from prior session handover) was to pattern-match the
REP STOSB shape at **compile time** and lower it to a new
`Op::MemoryFill` bytecode. I did not do that. I iterated on the
**runtime** detector instead — a different architectural choice with a
different risk profile (less invasive to `compile.rs`, but lower ceiling
than symbolic compile-time analysis). The 9.4× from Task 1 (CSS-DOS
side) is real; the 10×-ceiling of Task 2 is not here.

### Artifacts (this session)

- `crates/calcite-core/src/tick_period.rs` — zero-crossing cap +
  opts 1/2/4. Stable, correct, bench-worthy.
- `crates/calcite-core/src/cycle_tracker.rs` — signature-based detector.
  Detection works, projection broken. Marked as experimental.
- `crates/calcite-cli/src/bin/probe_write_sig.rs`,
  `probe_cycle_detect.rs`, `probe_cycle_project.rs` — diagnostic
  + bench probes for the new detector.
- `../CSS-DOS/bios/splash.c` (commit `2acc748`) — REP STOSB rewrite,
  shipped.
