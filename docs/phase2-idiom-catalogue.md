# Phase 2 — CSS idiom catalogue

**Status:** in progress. Companion to
[`compiler-mission.md`](compiler-mission.md) § Phase 2 and the Phase 1
DAG substrate (`crates/calcite-core/src/dag/`). This is the first
deliverable of Phase 2 per the mission doc: a hand-derived enumeration
of the structural idioms CSS forces emitters into, read off
`../CSS-DOS/kiln/emit-css.mjs` and the pattern files under
`../CSS-DOS/kiln/patterns/`.

The catalogue is the input to the DAG rewriter. Each idiom gets a
shape-matcher (Phase 2 step 2) and a normalised replacement node
(Phase 2 step 3). The catalogue itself is *cardinal-rule clean*: every
idiom is a structural shape over CSS expression form. None of them
require x86, BIOS, DOS, or cabinet knowledge to recognise. The
genericity probe ("would this fire on a 6502 cabinet, a brainfuck
cabinet, a non-emulator cabinet whose CSS shares the structural
shape?") is answered for each entry.

## Reading guide

For each idiom we record:

- **CSS shape**: the surface form the emitter generates.
- **Op-stream shape**: how `compile.rs` lowers it today (the substrate
  Phase 1 DAG sees).
- **Matcher hook**: where in the DAG the matcher runs and what it
  inspects.
- **Normal form**: the canonical replacement node we want to introduce.
- **Genericity**: why this is structural, not program-specific.
- **Status**: *recognised today* (already collapsed in the bytecode by
  the v1 pattern recognisers — Phase 2 just preserves the shape on the
  DAG side) vs *Phase 2 candidate* (currently un-collapsed in the
  Op stream; Phase 2 introduces the shape-matcher).

## Idiom 1 — Per-output dispatch table (`if(style(--key: N))` chains)

The single most common CSS idiom. Every per-register dispatch in
`emitRegisterDispatch` is an `if(style(--opcode: N): exprN; … else:
default)` cascade. Same shape appears in `--readMem` (one branch per
memory address), `--readDiskByte` (one branch per non-zero disk byte),
`--unknownOp`, and any other dense sparse-key→value map the emitter
produces.

- **CSS shape:**
  ```css
  --x: if(
    style(--key: 0):  expr0;
    style(--key: 1):  expr1;
    …
    style(--key: 255): expr255;
    else: fallback);
  ```
  All branches test the same property against integer literals. Branch
  count is large (4+ minimum to qualify; thousands in practice for
  `--readMem`).
- **Op-stream shape (today):** `Op::Dispatch` (HashMap-backed) or
  `Op::DispatchFlatArray` (dense u8/u16/u32 array). Both write to one
  destination slot and fall through. The original cascade is gone by
  the time it reaches Phase 1's DAG.
- **Matcher hook (Phase 2):** within a basic block, identify any
  `Op::Dispatch` / `Op::DispatchFlatArray` op whose all entries are
  `LoadLit`-equivalent (resolved to constants at compile time). Already
  done in v1 pre-DAG.
- **Normal form:** `DagNode::Switch { key: NodeId, table: TableId,
  fallback: NodeId }`. Single pure node, no control flow inside.
- **Genericity:** any program with sparse-key→value maps emits this.
  Brainfuck doesn't (no opcode dispatch); 6502 does; lookup-tables in
  any non-emulator cabinet (palettes, glyph tables, gradient tables,
  state machines) do.
- **Status:** *recognised today*. Phase 2 task: ensure the DAG node
  preserves dispatch semantics so Phase 3 codegen lowers it to a
  table-load instead of a chain.

## Idiom 2 — Cascade with default fall-through (`if() else: var(--__1x)`)

Every register and every memory cell has a "do nothing this tick"
fallthrough that returns `var(--__1x)` (the previous-tick double-buffer
value). When the dispatch finds no matching branch, the cell holds.

- **CSS shape:**
  ```css
  --x: if(
    style(--cond1): val1;
    style(--cond2): val2;
    else: var(--__1x));
  ```
  The fallback is structurally `var(--__1<sameProp>)`.
- **Op-stream shape (today):** the fallthrough lowers to a
  `LoadState`-into-dst pre-tick, then the dispatch overwrites it on a
  hit. The "self-hold" is implicit in the absence of a write.
- **Matcher hook:** detect dispatch nodes whose fallback is a
  same-slot self-load — i.e. `dst` and `LoadState(dst)` reference the
  same logical state var.
- **Normal form:** `DagNode::ConditionalUpdate { dst: SlotId, cases:
  Vec<(Cond, Expr)> }`. Equivalent semantics, with the no-op case
  factored out.
- **Genericity:** appears in any cabinet with stateful properties — a
  CSS animation that updates one of N keyframe slots per tick has the
  same shape.
- **Status:** *recognised today* via `--__1` self-load detection in
  the broadcast-write recogniser. Phase 2 task: lift the same
  recognition to register-cascade fallthrough so Phase 3 codegen can
  emit one branch instead of two ops.

## Idiom 3 — Broadcast write (one-of-N memory store)

The hallmark of a CSS-emulated address space. Every memory cell `--mN`
asks "does `--memAddr0..K-1` equal `N`?" — and *one* cell wins the
write. From the cell's perspective: a sparse one-of-N decoder. From
the writer's perspective: a regular indirect store.

- **CSS shape:** the per-byte case (`PACK_SIZE=1`, `emitMemoryWriteRulesStreaming`):
  ```css
  --m1024: if(
    style(--_slot0Live: 1) and style(--memAddr0: 1024): if(style(--_writeWidth: 2): --lowerBytes(var(--memVal0), 8); else: var(--memVal0));
    style(--_slot0Live: 1) and style(--memAddr0: 1023) and style(--_writeWidth: 2): --rightShift(var(--memVal0), 8);
    style(--_slot1Live: 1) and …
    else: var(--__1m1024));
  ```
  Same structural template repeated for every cell at every address —
  the dispatch property and value vary by address.
- **Op-stream shape (today):** `Op::CompiledBroadcastWrite` (recognised
  by `pattern/broadcast_write.rs`). The K cells across the address
  space collapse to a single conditional indirect store keyed on
  `--memAddrN`.
- **Matcher hook:** inspect each block for groups of N assignments to
  `mK..mK+N` whose only structural difference is the literal `N` in
  one branch and the cell name on the LHS. v1 already does this in
  `recognise_broadcast`.
- **Normal form:** `DagNode::IndirectStore { live: NodeId, addr:
  NodeId, val: NodeId, width: NodeId, base: u32, len: u32 }` — a
  region-tagged scatter store.
- **Genericity:** any CSS that emulates writable memory (any virtual
  machine, sprite buffer, register file rendered in CSS) has this
  shape. The emulated-CPU origin is incidental.
- **Status:** *recognised today*. Phase 2 task: model `IndirectStore`
  as a first-class DAG node so Phase 3 codegen lowers it to a single
  direct write rather than walking the broadcast op.

## Idiom 4 — Packed-cell broadcast write (`--applySlot` cascade)

The packed-memory variant. With `PACK_SIZE=2` each memory cell holds
two bytes, and writes go through an `--applySlot` function call
cascade — one call per write slot, threaded inside-out.

- **CSS shape (`PACK_SIZE=2`):**
  ```css
  --mc512: --applySlot(
              --applySlot(
                --applySlot(var(--__1mc512), var(--_slot2Live), …),
                var(--_slot1Live), …),
              var(--_slot0Live), …);
  ```
  N nested `--applySlot` calls, deepest call is `var(--__1mcN)`.
  Each call's args are: previous cell value, live gate, lo offset,
  hi offset, val, width. Same structural template every cell.
- **Op-stream shape (today):** `Op::CompiledBroadcastWrite` with
  `packed=true` — recognised by `pattern/packed_broadcast_write.rs`.
- **Matcher hook:** detect the N-deep `--applySlot` chain shape over a
  cell range; same idiom as idiom 3 with a different per-byte split
  routine.
- **Normal form:** same `DagNode::IndirectStore` as idiom 3, with a
  `pack: u8` field. Phase 3 codegen can specialise either way.
- **Genericity:** any cabinet using packed memory (4-bpp palette
  storage, 16-color framebuffers, attribute bytes) has the same
  cascade. The shape is "aggregate cell + per-element apply
  function" — orthogonal to what's stored.
- **Status:** *recognised today*. Phase 2 task: unify with idiom 3
  under one `IndirectStore` node.

## Idiom 5 — Slot-live gate (sparse-write enable)

Every memory write rule's per-slot branches lead with `style(--_slotNLive:
1) and …`. When the slot is idle (no opcode is using it this tick), the
gate evaluates to 0 in Chrome — the whole subsequent address-equal test
short-circuits and never runs. This is the CSS equivalent of "if
(write_enable) decode address". Calcite recognises the gate and lifts
it out of the per-cell scan so 6000+ cells skip in O(1) instead of
O(N).

- **CSS shape:** every broadcast-write branch begins with a
  `style(--gateProp: 1)` qualifier; the rest of the condition (`and
  style(--memAddrN: …)`) is meaningful only inside the gate.
- **Op-stream shape (today):** the broadcast recogniser peels gate
  conditions off into the `gate_property` field on `BroadcastWrite`.
  At eval time the executor reads the gate once per write port and
  skips the entire address scan when it's 0.
- **Matcher hook:** within a recognised broadcast/indirect-store, find
  the common `style(--P: 1)` factor across all branches. v1 already
  does this.
- **Normal form:** add `gate: Option<NodeId>` to
  `DagNode::IndirectStore` and to any future region scatter node.
- **Genericity:** any CSS that has "write port" semantics — a CSS
  carousel, a sprite engine, an animation timeline that updates
  conditional on a play/pause flag — emits this. The "this tick is
  idle" idea is universal.
- **Status:** *recognised today*. Phase 2 task: surface the gate as a
  DAG-node field rather than a side-channel on the executor.

## Idiom 6 — Multi-condition `style(A) and style(B)` short-circuit

CSS spec: `style(A) and style(B)` evaluates left to right and
short-circuits on first false. Our emitters lean on this for cheap
guarded address-equality (idiom 5 above is a special case).

- **CSS shape:** any branch condition with two or more
  `style(--prop: val)` joined by `and`.
- **Op-stream shape (today):** within a `Dispatch`/`BranchIfNotEqLit`
  cascade, multi-condition branches lower to nested
  `BranchIfNotEqLit` ops — the second condition is checked only when
  the first matches. Calcite already exploits this naturally because
  basic-block fall-through gives the same short-circuit behaviour.
- **Matcher hook:** none needed at the DAG level — the linearisation
  already encodes short-circuit in CFG edges. Listed here for
  completeness so a future engineer doesn't add an "AND combiner" node
  thinking it's missing.
- **Genericity:** universal CSS feature.
- **Status:** *no recogniser needed*. The CFG already encodes it.

## Idiom 7 — Word-write splitting (`--lowerBytes` / `--rightShift` pairs)

A CSS memory cell is 8-bit; a 16-bit write becomes a paired
(addr, lo) / (addr+1, hi) byte write. Emitters generate this pair
explicitly with `--lowerBytes(X, 8)` and `--rightShift(X, 8)` over the
same word `X`. The kiln itself fuses them at emit time
(`tryFuseWordPair`); calcite then sees the fused form.

- **CSS shape (post-fusion):** the emit-css fuser collapses the pair
  into one width=2 slot whose val is the un-split word `X`. The
  per-cell write rule then re-splits via `--lowerBytes` / `--rightShift`
  inside its `if` branches.
- **Op-stream shape (today):** the fused word travels through the
  broadcast-write op as a width=2 slot. Per-cell `--lowerBytes`/
  `--rightShift` re-splits are inlined as `AndLit(0xFF)` / `ShrLit(8)`.
- **Matcher hook:** within an `IndirectStore`, detect that the value
  expression in adjacent cells is `low_byte(X)` and `high_byte(X)` for
  the same `X`; collapse to a width=2 store node.
- **Normal form:** `DagNode::IndirectStore { ..., width: ConstWidth(2)
  }` with a single 16-bit value node.
- **Genericity:** any cabinet that stores wider-than-cell values has
  this shape (16-bit pixel buffers in 8-bit-cell schemes, etc.).
- **Status:** *partially recognised today* (the kiln fuses at emit
  time; calcite carries width=2 through). Phase 2 task: when calcite
  receives unfused width=1+1 pairs (e.g. from a non-kiln emitter),
  recognise the same shape and fuse on its own. Cardinal-rule probe:
  yes — this is "two adjacent cells with low/high byte of the same
  expression," fully structural.

## Idiom 8 — Self-hold (`var(--__1<sameVar>)`)

Any state property that doesn't change this tick must explicitly hold
its previous-tick value. The CSS shape: `var(--__1<self>)` as the
fallback. This is structurally identical to idiom 2's "default
fall-through" but appears at top level too.

- **CSS shape:** `--x: var(--__1x);` — appears as a top-level
  assignment when the property has no dispatch logic at all.
- **Op-stream shape (today):** `Op::LoadState { dst, addr }` followed
  by `Op::StoreState { dst, addr }` to the same address — a no-op
  assignment that the v1 compiler keeps because it carries the
  double-buffer semantic.
- **Matcher hook:** detect any pure load+store pair where load addr ==
  store addr. Replace with `DagNode::Hold { slot: SlotId }`.
- **Normal form:** `DagNode::Hold { slot }`. Phase 3 codegen lowers
  this to "do nothing" for slots that have no other writer this tick.
- **Genericity:** any double-buffered CSS with non-updated cells.
- **Status:** *Phase 2 candidate*. v1 leaves the load+store pair in
  the op stream.

## Idiom 9 — Constant register dispatch (peripheral compute / IRQ compute)

Some registers (peripheral state machines, IRQ delivery, cycle counts)
have small dense dispatch tables — `if(style(--_irqActive: 1): vIRQ;
style(--_tf: 1): vTF; else: normalDispatch)`. Tiny dispatches with
2–4 priority-ordered branches.

- **CSS shape:** small (≤4 branch) priority-ordered `if(style())`
  cascade where each branch tests a different property.
- **Op-stream shape (today):** stays as a `BranchIfNotEqLit` chain in
  the op stream — too small to qualify for the v1 dispatch-table
  threshold (4+).
- **Matcher hook:** detect 2- and 3-way priority cascades where each
  branch tests `style(--guard: 1)` and the bodies are pure
  expressions.
- **Normal form:** `DagNode::Mux { cases: Vec<(Cond, Expr)>, fallback:
  Expr }` — same shape as idiom 2's ConditionalUpdate but for pure
  expressions, not state-var holds.
- **Genericity:** universal — any "priority override" pattern in CSS
  has this shape.
- **Status:** *Phase 2 candidate*. Significant on Doom because the
  TF/IRQ override chain runs on every register write (16 registers ×
  per-tick).

## Idiom 10 — Function call with const-shaped body

`@function` declarations followed by `--readMem(at: K)` /
`--readDiskByte(idx: K)` calls are the CSS equivalent of out-of-line
helpers. The function body is a giant dispatch (idiom 1); the call
site just provides the key.

- **CSS shape:** `--x: --readMem(calc(...))` where `--readMem` is an
  `@function` body that's itself a dispatch table.
- **Op-stream shape (today):** `Op::Call { func_id, arg_slots,
  result_slot }`. The callee is also compiled but kept separate.
- **Matcher hook:** when an `Op::Call`'s callee is a single
  `Op::Dispatch` (no other ops), inline the dispatch into the call
  site, eliminating the call frame entirely.
- **Normal form:** `DagNode::Switch` (idiom 1's normal form). The Call
  node disappears.
- **Genericity:** any cabinet using `@function` for table lookups.
  Inlining single-dispatch helpers is a textbook compiler pass.
- **Status:** *Phase 2 candidate*. Bigger than it looks — `--readMem`
  is the hottest function in any cabinet.

## Idiom 11 — Bit decomposition (`--bit(X, n)`, `--lowerBytes(X, n)`)

The CSS library exposes helpers for bit field extraction. They appear
in flag computation (zero flag = `--bit(--cmp(...), 0)`), peripheral
compute, decode logic, and anywhere bytes need slicing.

- **CSS shape:** `--bit(X, n)` returns bit n of X; `--lowerBytes(X, n)`
  returns X mod 2^n; `--rightShift(X, n)` is logical shift right.
- **Op-stream shape (today):** function-body fast-paths in
  `compile.rs` recognise these specific function shapes and lower to
  `Op::Bit`, `Op::AndLit`, `Op::ShrLit` directly. No `Op::Call`
  frame.
- **Matcher hook:** none additional — the v1 fast-paths already
  do the lowering. Phase 2 task: ensure the DAG nodes have a clean
  representation (a single `BitField` node parameterised by lo/hi)
  so Phase 3 codegen emits a wasm `i32.and` / `i32.shr_u` directly.
- **Normal form:** `DagNode::BitField { src: NodeId, shift: u8, mask:
  u32 }`. Captures the common case `(x >> shift) & mask`.
- **Genericity:** universal. Bit extraction is a primitive in any
  numerical CSS.
- **Status:** *partially recognised today*. Phase 2 task: collapse
  `Op::ShrLit` followed by `Op::AndLit` into a single `BitField` node.

## Idiom 12 — Pure functional cascade (no side effects, no holds)

The decode pipeline (`emitDecodeProperties`) and several derived
properties (`--_irqActive`, `--_tf`, `--unknownOp`) are *pure*
functions of the previous-tick state. No self-holds, no writes — just
chained `calc()` and `if()`.

- **CSS shape:** dependency-ordered chain of `--p1: f(prev state)`,
  `--p2: g(--p1)`, `--p3: h(--p1, --p2)` etc. No `var(--__1pK)` on
  the right-hand side.
- **Op-stream shape (today):** sequence of `Op::Add` / `Op::Mul` /
  `Op::Bit` etc. with intermediate slots. Structurally a small DAG
  fragment.
- **Matcher hook:** detect *runs* of pure ops with no intervening
  branch / store / load-from-memory. Mark them as a single "pure
  fragment" so Phase 3 can hoist common subexpressions and constant-
  fold inside the fragment.
- **Normal form:** keep the per-op DAG nodes but tag the enclosing
  region as `Pure { ops: NodeRange }` so codegen scheduling knows it
  can reorder freely.
- **Genericity:** universal — every CSS cabinet has a derive chain.
- **Status:** *Phase 2 candidate*. Phase 1 doesn't model purity; the
  walker treats every op as side-effecting.

## Idiom 13 — Repeated-load CSE (same `var(--prop)` read multiple times)

Within one tick, the same state variable is often read at multiple
points (an opcode-byte read appears in dispatch *and* in flag compute
*and* in decode). The Op stream loads it from state each time.

- **CSS shape:** multiple `var(--__1xxx)` for the same `xxx` within
  one assignment cascade.
- **Op-stream shape (today):** N `Op::LoadState` ops with the same
  addr. Slot-allocator sometimes reuses slots, sometimes doesn't —
  depends on lexical proximity.
- **Matcher hook:** within a basic block, dedupe `Op::LoadState`s
  that have the same `addr` and reach the same use without an
  intervening `StoreState` to that addr.
- **Normal form:** standard SSA value-numbering. The duplicate loads
  collapse to one `DagNode::LoadState` reused by all consumers.
- **Genericity:** universal compiler pass.
- **Status:** *Phase 2 candidate*. Likely high-impact on Doom because
  every dispatch evaluates `--opcode` independently.

## Idiom 14 — Constant fold (`calc()` with all-literal operands)

Address arithmetic in `emitMemoryWriteSlots` ends up with expressions
like `calc(var(--__1SS) * 16 + ... - 2 + 65536)`. The literal parts
fold at parse time but mixed expressions don't.

- **CSS shape:** `calc(...)` with mixed literal and variable terms.
- **Op-stream shape (today):** `Op::Add` / `Op::Mul` / `Op::AddLit` /
  `Op::MulLit` chains. Some `+ K1 + K2` is collapsed (compile-time
  literal-merging in `compile.rs`); some isn't.
- **Matcher hook:** standard arithmetic identity rules over DAG
  nodes: `AddLit(AddLit(x, K1), K2) → AddLit(x, K1+K2)`,
  `MulLit(MulLit(x, K1), K2) → MulLit(x, K1*K2)`, etc.
- **Normal form:** the same op nodes, but with literals merged.
- **Genericity:** universal.
- **Status:** *partially recognised today*. Phase 2 task: post-DAG
  arithmetic-identity sweep. Cheap, high coverage.

---

## Summary table

| # | Idiom                                | Today           | Phase 2 work                                  |
|---|--------------------------------------|-----------------|-----------------------------------------------|
| 1 | Per-output dispatch table            | recognised      | DAG `Switch` node                             |
| 2 | Cascade with self-hold default       | recognised      | DAG `ConditionalUpdate` node                  |
| 3 | Broadcast write                      | recognised      | DAG `IndirectStore` node                      |
| 4 | Packed-cell broadcast write          | recognised      | unify with idiom 3                            |
| 5 | Slot-live gate                       | recognised      | surface as DAG field                          |
| 6 | `style(A) and style(B)` short-circuit | (no work)      | none                                          |
| 7 | Word-write splitting                 | partial (kiln)  | matcher for unfused pairs                     |
| 8 | Self-hold (`--__1<self>`)            | not recognised  | matcher + `Hold` node                         |
| 9 | Priority-cascade override            | not recognised  | matcher + `Mux` node                          |
| 10 | `@function` call with dispatch body | not recognised  | inline single-dispatch callees                |
| 11 | Bit-field extraction                | partial         | `BitField` normal form                        |
| 12 | Pure functional cascade             | not recognised  | tag pure regions                              |
| 13 | Repeated-load CSE                    | not recognised  | SSA value numbering                           |
| 14 | Constant fold                        | partial         | arithmetic-identity sweep                     |

## What this catalogue implies for Phase 2 scope

The 14 idioms cluster into three buckets:

**Already recognised (1–6).** Phase 2's job is to model them as
first-class DAG nodes (rather than executor side-channels) so Phase 3
codegen has a clean target. No new behaviour, no new performance.

**Partially recognised (7, 11, 14).** Phase 2 closes gaps —
unfused word pairs, missed bit-field shapes, missed arithmetic
identities.

**Not recognised yet (8, 9, 10, 12, 13).** These are the new
recognisers Phase 2 contributes. The biggest leverage is probably
idiom 13 (repeated-load CSE) and idiom 10 (`@function` inlining of
single-dispatch callees) — both touch hot paths.

The decision-gate metric ("≥30 % node-count reduction on Doom") will
mostly come from 13 and 10. The rest are correctness-and-shape work
that pays off in Phase 3, not Phase 2's intermediate metric.

## Cardinal-rule audit

Re-checking each idiom against the genericity probe:

| # | Would fire on a non-emulator cabinet? |
|---|----------------------------------------|
| 1 | yes — any sparse-key→value CSS         |
| 2 | yes — any stateful CSS animation       |
| 3 | yes — any writable virtual storage     |
| 4 | yes — any packed virtual storage       |
| 5 | yes — any "write enable" idiom         |
| 6 | yes — universal CSS                    |
| 7 | yes — any wide-value-in-narrow-cell    |
| 8 | yes — any double-buffered CSS          |
| 9 | yes — any priority override            |
| 10| yes — any `@function`-using CSS        |
| 11| yes — universal                        |
| 12| yes — universal                        |
| 13| yes — universal                        |
| 14| yes — universal                        |

All 14 pass. No idiom requires x86, BIOS, DOS, or specific cabinet
content to recognise. The catalogue is cardinal-rule clean.
