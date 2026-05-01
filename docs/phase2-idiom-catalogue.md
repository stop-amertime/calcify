# Phase 2 idiom catalogue

**Status:** initial enumeration from a fresh end-to-end read of
`../CSS-DOS/kiln/emit-css.mjs` and supporting kiln modules
(`template.mjs`, `css-lib.mjs`, `decode.mjs`, plus skim of `patterns/*`).
Living doc; revise as Phase 2 recognisers land and surface shapes the
catalogue missed (per `compiler-mission.md` § Phase 2, the catalogue
earns its keep through the same "every Doom-found bug becomes a unit
test" discipline — every shape that should fire a recogniser but
doesn't feeds back into this doc as a new entry or a refined operational
test).

**Audience:** Phase 2 recogniser-writers. Each idiom below is a
candidate for a DAG shape-matcher and a normalised replacement
super-node. The catalogue describes structural shapes only, NOT
recogniser implementations; recogniser code, IR types, and matcher
algorithms live elsewhere.

**Source of truth:** `../CSS-DOS/kiln/emit-css.mjs` read on 2026-04-30
(1082 lines), with cross-references into `template.mjs`, `css-lib.mjs`,
`decode.mjs`, and `memory.mjs`. Citations below as `kiln:LINE` (in
`emit-css.mjs` unless prefixed `template:`, `css-lib:`, `decode:`,
`memory:`).

## Cardinal-rule contract for this catalogue

The CLAUDE.md rule, enforced per-idiom: each shape must be derivable
from CSS structure alone. No idiom names what the cabinet uses the
shape *for*; idiom names describe the CSS surface form. Each entry
includes:

- **Shape (in CSS):** an abstract example of the surface form.
- **Where kiln emits this:** line refs into the kiln source, for
  re-readability without re-reading kiln.
- **Frequency / scale signal:** rough order-of-magnitude per cabinet,
  to help recogniser-writers prioritise. No exact numbers.
- **Operational test (CSS-only):** how a calcite engineer who has
  never seen kiln would recognise this shape from staring at a `.css`
  alone. If this paragraph reaches for x86 / BIOS / opcode / cabinet
  semantics, the idiom is overfit.
- **Genericity probe:** would this fire on a brainfuck-cabinet,
  6502-cabinet, calculator-cabinet, or rules-engine cabinet? At least
  one yes is required. All-no means reframe or drop.
- **Notes for Phase 2:** optional — pitfalls, near-miss shapes the
  recogniser must reject, interactions with other idioms.

The point of these probes is operational discipline: they keep the
recogniser-writer from accidentally encoding "this is the opcode
dispatch in 8086" as a hard assumption that breaks every other cabinet
calcite might ever load.

---

## Idiom 1: Long literal-keyed `if(style(--P: N))` cascade with default fallback

**Shape (in CSS):**

```css
--out: if(
  style(--key: 0):  expr0;
  style(--key: 1):  expr1;
  style(--key: 2):  expr2;
  /* … many entries, all on the same key prop, all integer literals … */
  style(--key: 254): expr254;
  else: defaultExpr);
```

Every branch tests the **same** property against an integer literal;
branches are sorted by key; one default fallback closes the cascade.

**Where kiln emits this:** `kiln:107-133` (`emitRegisterDispatch`, the
per-output dispatch builder); `kiln:64-74` (`emitUnknownOpFlag`);
`kiln:855-911` (`emitReadMemStreaming`, address-keyed cascade);
`kiln:919-934` (`emitReadDiskByteStreaming`, index-keyed cascade);
`css-lib:158-193` (`--pow2`, dense small-key cascade);
`decode:43-51, 73-82, 89-99, 105-111` (register-index cascades inside
`@function` bodies).

**Frequency / scale signal:** at least one per output property in the
.cpu rule (~25-ish); each can have hundreds of branches. The big ones
are `--readMem` (one branch per writable byte; up to ~1 MB), the
register-output dispatches (~256 entries each), and `--readDiskByte`
(one branch per non-zero disk byte; tens of thousands to millions).
Easily the most-emitted shape in the file by branch count.

**Operational test (CSS-only):** an `if(...)` whose branches are all
`style(--P: <integer literal>): <expr>` for the same property `--P`,
with a single trailing `else: <expr>`. Branch count ≥ 4 in practice
(below that the linear-compare lowering is faster than a table).

**Genericity probe:**
- Brainfuck cabinet: yes — a brainfuck cabinet would dispatch on the
  current command byte (`+`, `-`, `>`, `<`, `[`, `]`, `.`, `,`) the same
  way; small N, same shape.
- 6502 cabinet: yes — register/opcode/addressing-mode cascades collapse
  to the identical form.
- Calculator cabinet: yes — keypad-key → action lookup, function-key →
  formula lookup are sparse-key→value cascades.
- Rules-engine cabinet: yes — rule-id → action body is the same shape.

**Notes for Phase 2:** v1 already recognises this in
`pattern/dispatch.rs` (≥4 branches threshold) and emits
`Op::Dispatch` / `Op::DispatchFlatArray`. Phase 2's job is to hold the
shape as a first-class DAG node so Phase 3 codegen can lower it
directly — not to re-discover it. Recognise the cascade BEFORE other
shape-matchers run; many of the remaining idioms appear inside a
single cascade entry's body, and pre-collapsing the cascade gives them
clean per-entry sub-DAGs to chew on.

---

## Idiom 2: Self-hold default in a cascade (`else: var(--__1<self>)`)

**Shape (in CSS):**

```css
--x: if(
  style(--cond1): val1;
  style(--cond2): val2;
  else: var(--__1x));
```

Or as a top-level no-cascade hold:

```css
--x: var(--__1x);
```

The fallback is structurally `var(--__1<sameName>)`: the cell holds its
previous-tick value when no branch fires. The same-name relationship is
the structural signature.

**Where kiln emits this:** `kiln:107-165` (`emitRegisterDispatch`'s
`defaultExpr = var(--__1${reg})` defaults at `kiln:753`);
`kiln:982-996` (per-byte memory write rule's `else: var(--__1m${addr})`
fall-through, unpacked); `kiln:1020-1032` (packed cell cascade
bottoms out at `var(--__1mc${idx})`); `kiln:744-755` (the `regOrder`
loop emits self-hold defaults for every register and state var that
has no custom default).

**Frequency / scale signal:** every double-buffered output has a
self-hold somewhere — registers, state vars, and every memory cell.
Per-cell self-holds dominate by count: one per memory address (~1 MB
for Doom-class cabinets).

**Operational test (CSS-only):** an assignment whose fallback (the
`else:` branch of the topmost `if`, or the bare RHS of a no-`if`
assignment) is `var(--__1<X>)` where `<X>` matches the LHS property
name (modulo the kiln's structural prefix scheme: a kiln writes `--X`
on the LHS and the previous-tick read of the same slot is `--__1X`).
Detection is purely lexical: does the fallback expression read the
"previous-tick" form of the same property the assignment writes?

**Genericity probe:**
- Brainfuck cabinet: yes — every brainfuck tape cell holds its value
  unless the current command writes it. Same self-hold cascade per cell.
- 6502 cabinet: yes — registers and memory cells with no-op behaviour
  on the current opcode hold.
- Calculator cabinet: yes — display register holds across keypress
  ticks that don't update it.
- Rules-engine cabinet: yes — fact slots hold across rule firings that
  don't touch them.

**Notes for Phase 2:** the v1 broadcast-write recogniser detects this
on the per-cell side; the per-register side is currently emitted as a
dispatch-with-fallback whose fallback happens to be a self-hold. Phase
2 should unify them so a single normalised "no-op hold" shape is
recognised both inside and outside cascades. Two near-misses to reject:
(a) custom defaults in `customDefaults` (`kiln:747-751`) like
`pitCounter` and `picPending` are NOT self-holds — they evaluate to a
different expression that happens to incorporate the previous-tick
value; the recogniser must check structural equality to
`var(--__1<self>)`, not "fallback contains the previous-tick read." (b)
the prefix scheme (`--__0`, `--__1`, `--__2`) is structural in the
kiln's output, so the matcher must understand the prefix family; do
not hard-code `--__1` as a magic string — recognise the family.

---

## Idiom 3: Address-keyed broadcast write (sparse-decoder per memory cell)

**Shape (in CSS, unpacked PACK_SIZE=1):**

```css
--mA: if(
  style(--gateLive0: 1) and style(--addrPort0: A): valExpr0;
  style(--gateLive0: 1) and style(--addrPort0: A-1) and style(--widthGate: 2): hiHalfExpr0;
  style(--gateLive1: 1) and style(--addrPort1: A): valExpr1;
  /* … one branch per (slot, half) for each of the K write ports … */
  else: var(--__1mA));
```

The same template is emitted for every memory cell, with only the
literal address `A` (and `A-1`) and the LHS cell name varying. From
the cell's perspective: a sparse one-of-N decoder over a small set of
address-output ports. From the writer's perspective: the K ports
collectively form an indirect store across the address space.

**Where kiln emits this:** `kiln:962-998`
(`emitMemoryWriteRulesStreaming`, unpacked path); `kiln:190-273`
(`emitMemoryWriteSlots` builds the K address/value port properties
that the per-cell rule queries against).

**Frequency / scale signal:** one cascade per memory cell × K ports ×
2 halves (lo + hi). For PACK_SIZE=1 with 1 MB writable and 6 ports:
~12 M branch evaluations per tick if naïvely walked, which is exactly
why this is the most consequential recogniser to keep.

**Operational test (CSS-only):** a contiguous run of N assignments
with the same structural template, where each assignment's RHS is an
`if()` whose every branch is a `style(--P: <literal>)`-style equality
test on one of a small set of "address-port" properties, and the
literal compared in branch *i* of cell *j* equals branch *i* of cell
*j+1* shifted by one. The give-away: shape-template-equivalence across
N cells with only the literal address differing.

**Genericity probe:**
- Brainfuck cabinet: yes — per-tape-cell write rules indexing on a
  pointer port have the same shape.
- 6502 cabinet: yes — same memory model.
- Calculator cabinet: partial — only if it has a multi-register
  scratchpad with indexed writes; small calculators wouldn't fire.
- Rules-engine cabinet: yes — fact-slot write ports indexing into a
  fact array fire the same shape.

**Notes for Phase 2:** v1 recognises this in
`pattern/broadcast_write.rs`. Two near-misses to reject: (a) cascades
where the LHS cells are NOT a contiguous address range (the recogniser
should check, not assume); (b) cascades whose port-property name
varies between cells (these are not the same broadcast port and must
not collapse). See idiom 4 for the packed variant; Phase 2 should
unify them under one normal form.

---

## Idiom 4: Function-cascade store via N-deep call chain (packed-cell broadcast)

**Shape (in CSS, packed PACK_SIZE=2):**

```css
--mc{idx}: --applySlot(
  --applySlot(
    --applySlot(
      --applySlot(
        --applySlot(
          --applySlot(var(--__1mc{idx}), var(--gate5), …),
          var(--gate4), …),
        var(--gate3), …),
      var(--gate2), …),
    var(--gate1), …),
  var(--gate0), …);
```

K nested calls of the **same** function, deepest argument is the
self-hold (`var(--__1<self>)`), each call's other args are gate /
offset / value / width expressions that vary by call depth. Same
template emitted for every cell, with only the cell index in the
self-hold and the offset arithmetic differing.

**Where kiln emits this:** `kiln:999-1033`
(`emitMemoryWriteRulesStreaming`, packed path); the `--applySlot`
function itself is defined at `css-lib:120-152`.

**Frequency / scale signal:** one cascade per packed cell. Half as many
cells as bytes (PACK_SIZE=2), but each cell's RHS is K function calls
deep — same total port-evaluation cost as idiom 3, lower per-cell
overhead because the 6 ports collapse into one nested expression
instead of 12 cascade branches.

**Operational test (CSS-only):** a run of N assignments where each RHS
is an N-deep cascade of `--<fnName>(<innerCall>, <args>)` calls of the
**same** function, the innermost call's first arg is `var(--__1<self>)`
matching the LHS, the cascade depth K is the same across cells, and
the arg expressions differ only in literal offsets that scale
linearly with the cell index.

**Genericity probe:**
- Brainfuck cabinet: no, in practice — brainfuck tape cells write at
  most one byte per tick, so the K-port cascade collapses to K=1 and
  this idiom degenerates into idiom 3.
- 6502 cabinet: yes — same packed-memory technique applies.
- Calculator cabinet: no — too small to motivate packing.
- Rules-engine cabinet: yes — packed fact-record writes fire the same
  shape.

**Notes for Phase 2:** v1 recognises this in
`pattern/packed_broadcast_write.rs`. Phase 2 should fold idiom 3 and
idiom 4 into one normal form — the packing factor and the half-write
splice arithmetic are details, not separate idioms. The recogniser
must NOT key off the function name `--applySlot`; instead it should
match on the call-cascade-with-inner-self-hold shape and verify that
every call goes to the same function definition. Implementing it that
way satisfies the cardinal rule and lets the same recogniser fire on
any cabinet that uses an N-port aggregate-cell update via a
`--<something>` helper.

---

## Idiom 5: Compound-AND short-circuit guard on a cascade branch

**Shape (in CSS):**

```css
if(
  style(--gateProp: gateVal) and style(--<port>: <addr>): <body>;
  …)
```

A cascade branch (typically inside idiom 3's broadcast write, but also
in `--applySlot` and similar) is conjunction-guarded: leftmost operand
is a "gate" (a small-cardinality property; usually `0`/`1`), rightmost
operand is the meaningful key test. Per the CSS spec
`style(A) and style(B)` short-circuits left-to-right, so when the gate
is 0 the rest of the conjunction is never evaluated.

**Where kiln emits this:** `kiln:990-992` (per-byte broadcast-write
gate `style(--_slotNLive: 1) and style(--memAddrN: <addr>)`);
`css-lib:144-151` (`--applySlot` body uses `style(--width: 2) and
style(--loOff: 0) and style(--hiOff: 1)` etc.); `decode:26, 42, 62-63,
163-…` (`--modrmLen`, `--eaOffset`, `--defaultSeg` and other decode
helpers use compound-AND for sub-mode selection).

**Frequency / scale signal:** every branch of every memory-cell write
cascade is gate-prefixed (idiom 3's canonical form) — so this gate
multiplies through the broadcast-write footprint. Plus per-decode
function: typically 1-3 compound-AND branches per `@function` body.

**Operational test (CSS-only):** any branch in a cascade whose
condition is a top-level `style(...) and style(...) [and ...]` chain.
Detection is purely structural; no value semantics needed. The
significant sub-case is "one operand is a 0/1-valued style property
that can be used as a hoistable gate."

**Genericity probe:**
- Brainfuck cabinet: yes — `style(--haveCommand: 1) and style(--cmd: '+')`
  is the same structural shape.
- 6502 cabinet: yes.
- Calculator cabinet: yes — `style(--shiftPressed: 1) and style(--key: …)`.
- Rules-engine cabinet: yes — `style(--ruleEnabled: 1) and
  style(--input: …)`.

**Notes for Phase 2:** v1 already exploits short-circuit through CFG
linearisation; the gate hoist for idiom 3 is also done. Phase 2's
question is whether to model compound-AND as a first-class DAG node
or keep relying on basic-block fall-through. The additive-stream
catalogue says "no recogniser needed"; the kiln read here suggests the
*structural* idiom is worth naming because the gate-hoist optimisation
that idiom 3 depends on is essentially "see compound-AND, hoist the
0/1-valued operand as a port-level skip." Naming the shape avoids a
future engineer un-doing the hoist by accident.

---

## Idiom 6: Wide-value split across adjacent cells (`--lowerBytes` / `--rightShift` pair)

**Shape (in CSS), pre-fusion:**

```css
/* lo half at addr A */     --memValN_lo: --lowerBytes(X, 8);
/* hi half at addr A+1 */   --memValN_hi: --rightShift(X, 8);
```

Or equivalently as paired branches inside one cascade:

```css
if(
  style(--key: K1): --lowerBytes(X1, 8);
  style(--key: K2): --lowerBytes(X2, 8);
  else: 0)
/* paired with */
if(
  style(--key: K1): --rightShift(X1, 8);
  style(--key: K2): --rightShift(X2, 8);
  else: 0)
```

Two adjacent cells (or two paired cascade branches) reference the
**same** wider value `X`, one extracting low byte via `--lowerBytes(X,
8)`, the other extracting high byte via `--rightShift(X, 8)`. Kiln's
own emit-time fuser already collapses this in many cases (see
`tryFuseWordPair` at `kiln:382-411` and `fuseWordVal` /
`matchLoHiPair` / `parseIfBranches` / `splitTopLevel` /
`findTopLevelColon` / `isAddrPlusOne` / `isAddrPlusOneAtomic` at
`kiln:425-635`); calcite sees the fused width-2 shape in those cases
and the unfused width-1+1 shape otherwise.

**Where kiln emits this:** `kiln:76-97` (`addMemWrite` /
`addMemWriteWord`); `kiln:382-635` (the entire fuser pipeline);
`kiln:861-864` (`--readMem` bridges keyboard via the same
lo/hi-of-the-same-word convention); `kiln:898-906` (rom-disk-window
reads compose `lba_low + lba_high * 256` from two adjacent cells).

**Frequency / scale signal:** every wider-than-cell value the cabinet
stores produces this pair. Stack pushes, indirect-word writes, INT
frames are all 2-byte writes split across two adjacent cells.

**Operational test (CSS-only):** two assignments to consecutive cells
(addr A, addr A+1, in the same emitter region) where the RHS of one
is `--lowerBytes(<X>, 8)` and the RHS of the other is
`--rightShift(<X>, 8)` for the same `<X>`. For dispatch-conditional
shapes: corresponding branches across two cascades that pass the same
condition keys and whose bodies pair as `--lowerBytes(Xn, 8)` /
`--rightShift(Xn, 8)`. The recogniser should also accept the
`--rightShift(--lowerBytes(X, 16), 8)` double-wrap (kiln emits this
when X arithmetic could overflow i32 — see `kiln:457-466`).

**Genericity probe:**
- Brainfuck cabinet: no — single-byte tape, no wider values.
- 6502 cabinet: yes — 16-bit address pair stores fire the same shape.
- Calculator cabinet: yes — multi-byte BCD or float storage in 8-bit
  cells fires this.
- Rules-engine cabinet: yes — multi-field facts crossing cell
  boundaries fire this.

**Notes for Phase 2:** kiln's emit-time fuser is a structural-match
algorithm written in JS; calcite's recogniser should reproduce it on
the DAG side so calcite can fuse pairs that kiln's fuser missed (e.g.
unusual address shapes or future cabinets that don't run through
kiln). The shape is "two adjacent cells with low/high byte of the
same expression"; everything else is encoding details. Recogniser
must reject mismatched `X` (different sub-expressions on the two
sides): even though the values may be equivalent semantically, a
structural-only matcher cannot prove that.

---

## Idiom 7: Bit-field extraction (`--bit`, `--lowerBytes`, `--rightShift`)

**Shape (in CSS):**

```css
--zeroFlag: --bit(var(--flags), 6);
--low8:     --lowerBytes(var(--word), 8);
--shifted:  --rightShift(var(--val), 4);
```

These three helper functions (defined at `css-lib:7-13` and
`css-lib:49-51`) lower at usage time to standard `(x >> shift) & mask`
shapes. They appear inline in dispatch entry bodies, decode pre-
computes, and any expression that needs bit slicing.

**Where kiln emits this:** `css-lib:7-13, 49-51` (function defs);
heavily used everywhere — `template:114-122` (register-half aliases
`--AL`, `--AH`, etc. are pure `--lowerBytes` and `--rightShift`);
`decode:289-292` (`--_cf`, `--_zf` extracted via `--bit`);
`patterns/flags.mjs:62-83` (sign-bit extraction across all flag
helpers).

**Frequency / scale signal:** universal in any tick. Hundreds of bit
extractions per tick across decode, flag compute, dispatch, and
memory-write splitting.

**Operational test (CSS-only):** a function call to one of a small set
of "bit slicer" helpers (`--bit(X, n)`, `--lowerBytes(X, n)`,
`--rightShift(X, n)`) where the second arg is a literal integer.
Alternatively, the inlined shapes: `mod(X, pow(2, n))` for `--lowerBytes`,
`round(down, X / pow(2, n))` for `--rightShift`,
`mod(--rightShift(X, n), 2)` for `--bit`. Detection should match either
the wrapper-call form OR the post-inlining arithmetic shape.

**Genericity probe:**
- Brainfuck cabinet: partial — would only fire if the cabinet packs
  multiple bf cells per CSS cell.
- 6502 cabinet: yes — flag bit extraction, byte slicing.
- Calculator cabinet: yes — BCD digit extraction is the same shape.
- Rules-engine cabinet: yes — bit-flag fact storage.

**Notes for Phase 2:** v1 has function-body fast-paths in `compile.rs`
that recognise these helpers and lower to `Op::Bit`, `Op::AndLit`,
`Op::ShrLit`. Phase 2's question is whether to give the DAG a single
`BitField { src, shift, mask }` super-node or keep them as separate
shift+mask DAG nodes for the codegen pass to fuse. Both work; the
super-node is friendlier to the genericity claim ("any bit slicer
matches one node" vs "any bit slicer matches a 2-node pattern"). The
matcher must also collapse `ShrLit(X, n) → AndLit(_, mask)` into the
super-node so chained extraction is single-shot.

---

## Idiom 8: Native bit-decomposition function body
   (16-local + bit-by-bit reconstruction sum)

**Shape (in CSS):** an `@function` body with the structure

```css
@function --combiner(--a <integer>, --b <integer>) returns <integer> {
  --a1: mod(var(--a), 2);
  --a2: mod(round(down, var(--a) / 2), 2);
  --a3: mod(round(down, var(--a) / 4), 2);
  /* … 16 bits of a … */
  --b1: mod(var(--b), 2);
  /* … 16 bits of b … */
  result: calc(
    <combine(a1,b1)> +
    calc(<combine(a2,b2)>) * 2 +
    calc(<combine(a3,b3)>) * 4 +
    /* … 16 reconstruction terms … */
  );
}
```

Each input is decomposed into N (typically 16) per-bit locals via
`mod(round(down, var/2^k), 2)`; the result is rebuilt as a sum where
each per-bit combine expression is multiplied by `2^k`.

**Where kiln emits this:** `css-lib:198-269` (the
`emitBitDecomp` / `emitBitwiseXor` / `emitBitwiseAnd` /
`emitBitwiseOr` / `emitBitwiseNot` family). The unary form (`--not`)
is the same shape minus the second-input decomposition.

**Frequency / scale signal:** 4 functions per cabinet. Each invocation
(per tick) is hundreds of arithmetic ops if interpreted naively;
recognising the body shape collapses it to one native bitwise op.

**Operational test (CSS-only):** an `@function` body whose locals are
N successive `mod(round(down, var(--<input>) / <pow2>), 2)` extractions
(or `mod(var(--<input>), 2)` for bit 0), and whose `result:` expression
is a sum of N terms where term `k` is the per-bit combine multiplied
by `2^k`. The combine expression is what selects which bitwise op:
- `var(--ak) * var(--bk)` → AND
- `min(1, var(--ak) + var(--bk))` → OR
- `min(1, var(--ak) + var(--bk)) - var(--ak) * var(--bk)` → XOR
- `1 - var(--ak)` → NOT (unary form, no `b` decomposition)

**Genericity probe:**
- Brainfuck cabinet: no — wouldn't have wide bitwise ops.
- 6502 cabinet: yes — same decomposition strategy applies (CSS lacks
  native bitwise everywhere).
- Calculator cabinet: yes — bitwise mode would emit the same.
- Rules-engine cabinet: yes — bitfield fact merging emits this shape.

**Notes for Phase 2:** v1 has `classify_bitwise_decomposition` in
`compile.rs` which already classifies these shapes (logged 2026-04-15:
1.9× speedup on bootle-ctest, 45% fewer ops/tick). Phase 2 should
preserve the recognition as a DAG super-node (`BitwiseAnd16`,
`BitwiseOr16`, `BitwiseXor16`, `BitwiseNot16` or one parameterised
node) so Phase 3 codegen emits a single wasm bitwise op. The matcher
must NOT key on the function name (`--and`, `--or`, etc.) — it must
match on body shape, exactly as v1 does today. That keeps the cardinal
rule clean and makes the recogniser fire on any cabinet whose
bitwise helpers happen to be named differently.

---

## Idiom 9: Small priority cascade with non-uniform conditions

**Shape (in CSS):**

```css
--out: if(
  style(--gate1: 1): valIfGate1;
  style(--gate2: 1): valIfGate2;
  else: normalExpr);
```

Branch count is small (typically 2-3) and the conditions test
**different** properties (unlike idiom 1 which tests the same property
against literals). The cascade encodes a priority: first matching
branch wins. Often appears wrapping a larger idiom-1 cascade as the
`else:` branch.

**Where kiln emits this:** `kiln:135-165` (every register dispatch
gets wrapped in a TF/IRQ priority pair: `if(style(--_tf: 1): vTF;
style(--_irqActive: 1): vIRQ; else: <normalDispatch>)`); `kiln:255-269`
(memory write slots have the same TF/IRQ pre-cases); `kiln:294-302,
369-377` (`emitSlotLiveGates`, `emitWriteWidthGate` add `--_tf` and
`--_irqActive` priority cases ahead of the per-key cascade);
`decode:23-27` (`--modrmLen` is itself a priority cascade);
`decode:37-40` (`--disp` is a priority cascade on `--mod`).

**Frequency / scale signal:** universal. Every register dispatch and
every memory port has a 2-3-deep priority head.

**Operational test (CSS-only):** an `if(...)` cascade whose branches
test **different** properties from each other (each `style(--P_i: V_i)`
has a different `P_i`), with branch count ≤ 4 and a final `else:`. The
distinguishing feature vs idiom 1 is the per-branch property variation.

**Genericity probe:**
- Brainfuck cabinet: yes — `if(style(--bracketActive: 1): jumpBack;
  style(--zero: 1): skipForward; else: normalStep)`.
- 6502 cabinet: yes — interrupt priority cascades fire the same.
- Calculator cabinet: yes — error/overflow override on normal compute.
- Rules-engine cabinet: yes — exception rule overrides normal rule.

**Notes for Phase 2:** v1 leaves these as `BranchIfNotEqLit` chains
(too small to qualify for the dispatch-table threshold). Phase 2's
DAG node should be a "priority mux" — same shape as idiom 2's hold
fall-through but with non-self-hold bodies. Important: this idiom
**wraps** idiom 1 in many cases. The recogniser must run idiom 9
*before* it inspects the inner cascade, so the inner idiom-1 cascade
appears as a self-contained sub-DAG that idiom-1's recogniser can
match cleanly. Get the ordering wrong and the priority head's tail
gets misclassified as part of the inner cascade.

---

## Idiom 10: `@function`-body call site whose body is one cascade

**Shape (in CSS):** a `@function --F(--x) returns <integer> { result:
if(style(--x: K1): v1; … else: vN); }` whose body is structurally a
single idiom-1 cascade keyed on a parameter, called from the .cpu rule
as `--y: --F(<arg>)` (possibly many call sites, possibly with the same
arg expression repeated).

**Where kiln emits this:** `kiln:855-911`, `kiln:919-934` (`--readMem`
and `--readDiskByte` are exactly this shape); `css-lib:158-193`
(`--pow2` is a small dense cascade in a function body); the
`--getReg16` / `--getReg8` / `--getSegReg` helpers in `decode:69-111`
are dense small cascades. Also: `--read2` at `css-lib:94-96` is a
function whose body composes two `--readMem` calls — an outer call
that wraps two cascades.

**Frequency / scale signal:** `--readMem` is the hottest call (every
memory load goes through it); `--readDiskByte` is hot when running
disk-heavy workloads. The decode helpers are called ~once per tick
each.

**Operational test (CSS-only):** an `@function` definition whose
`result:` expression is exactly one `if(...)` matching the idiom-1
shape on a parameter; AND a call site of that function in some other
expression. No other helper locals, no top-level `calc`, just `result:
if(...)` over a parameter. The matcher should also recognise the
"compose two cascades via a wrapper" shape (`--read2`-style) as a
special case worth inlining the wrapper to expose the inner cascades.

**Genericity probe:**
- Brainfuck cabinet: yes — a `--commandTable(--c)` helper would have
  this shape.
- 6502 cabinet: yes.
- Calculator cabinet: yes — function-key dispatch via a helper.
- Rules-engine cabinet: yes — rule-id → action helper.

**Notes for Phase 2:** v1's flat-array dispatch fast path handles the
literal-only case (logged 2026-04-15) but does NOT inline the call
frame. Phase 2 should inline single-cascade function bodies into the
call site so the resulting DAG has no `Call` node — it's just an
idiom-1 cascade with the call's argument expression substituted for
the parameter. Be careful with multi-argument helpers like
`--applySlot`: those are NOT this idiom (the body is a multi-condition
priority cascade with several parameters, see idiom 4 / idiom 9).

---

## Idiom 11: Pure derived-property chain (no `var(--__1<self>)`,
   no cell write)

**Shape (in CSS):** a sequence of property assignments each pure with
respect to the previous-tick state, and depending only on already-
declared assignments earlier in the same tick:

```css
--p1: f(var(--__1A), var(--__1B));
--p2: g(var(--p1), var(--__1C));
--p3: h(var(--p1), var(--p2), <literal>);
```

No assignment reads `var(--__1<self>)`; no assignment is a memory-cell
or register output (no double-buffer participation); no `if(...)`
self-hold fallback. Side-effect-free function of the inputs.

**Where kiln emits this:** `kiln:712-720, 722-724`
(`emitDecodeProperties`, `emitPeripheralCompute`, `emitIRQCompute`
declare the decode pipeline `--opcode` → `--mod` → `--reg` → `--rm` →
`--ea` → `--rmVal16` → … and the `--_cf`, `--_zf`, `--_tf`,
`--_irqActive`, `--unknownOp`, `--haltCode` derives); `decode:280-384`
(the bulk of decode pre-computes are pure derivations);
`patterns/flags.mjs:43-…` (flag-helper functions are pure).

**Frequency / scale signal:** ~30 derived properties per cabinet, each
re-evaluated per tick.

**Operational test (CSS-only):** any assignment whose RHS contains no
`var(--__1<sameLHS>)` and whose LHS is not also a declared-cell name
(no `@property --<thatLHS>` with double-buffer behaviour). The
detection is "this property is computed fresh every tick from inputs
that are not its own previous value." For a *run* of such assignments,
the matcher should tag the whole run as a pure region.

**Genericity probe:**
- Brainfuck cabinet: yes — `--curCell` derived from `--ptr` and tape
  state.
- 6502 cabinet: yes — flag derives.
- Calculator cabinet: yes — display-string derived from numeric
  registers.
- Rules-engine cabinet: yes — derived facts.

**Notes for Phase 2:** Phase 1 doesn't model purity; the walker treats
every node as side-effecting. Phase 2 should tag pure regions so Phase
3 codegen can hoist common subexpressions, constant-fold inside the
region, and reorder freely. The recogniser must be conservative on the
"no self-hold" check: any branch — not just the top-level fallback —
that reads `var(--__1<sameLHS>)` disqualifies the assignment.

---

## Idiom 12: Compile-time-flat byte/cell address-space template

**Shape (in CSS):** a long run of `@property` declarations whose names
follow a structural pattern like `--m0`, `--m1`, …, `--mN` (or
`--mc0`, `--mc1`, … for the packed variant), each with the same body
template (same `syntax`, same `inherits`, an `initial-value` per
address). Followed by a parallel run of buffer-read assignments
(`--__1m0: var(--__2m0, init0); …`), keyframe stashes (`--__2m0:
var(--__0m0, init0); …`, `--__0m0: var(--m0); …`), and per-cell write
rules (idiom 3).

**Where kiln emits this:** `kiln:826-851`
(`emitMemoryPropertiesStreaming` declares one `@property` per cell);
`kiln:936-960` (`emitMemoryBufferReadsStreaming`); `kiln:1036-1060`
(`emitMemoryStoreKeyframeStreaming`); `kiln:1062-1082`
(`emitMemoryExecuteKeyframeStreaming`); plus the corresponding
`emitPropertyDecls` / `emitStoreKeyframe` / `emitExecuteKeyframe` /
`emitBufferReads` for the non-memory state vars
(`template:75-153`).

**Frequency / scale signal:** dominates cabinet size — ~99% of the
file is this. For 1 MB writable memory: ~4 M assignments and ~1 M
@property decls just for the memory mirror.

**Operational test (CSS-only):** a contiguous run of `@property`
declarations whose names are a structurally-templated index family
(common prefix, integer suffix, monotonically increasing); paired with
a contiguous run of assignments to the same family of names with a
templated RHS where only the integer suffix and the literal initial
value vary across cells. The "templated address-space" shape is the
combination — declarations + buffer-read mirror + keyframe stashes +
write rules all over the same name family.

**Genericity probe:**
- Brainfuck cabinet: yes — tape cells declared with the same pattern
  (`--t0`, `--t1`, …).
- 6502 cabinet: yes — same memory model.
- Calculator cabinet: partial — only if multi-register; small
  calculators don't fire.
- Rules-engine cabinet: yes — fact slots.

**Notes for Phase 2:** v1's parser already handles this efficiently
(logged 2026-04-15: parser fast-path for "Addr hole" templating). The
DAG should not store one node per cell; instead it should hold a
`AddressSpace { len, init: Vec<i32> }` super-node that the broadcast
write (idiom 3 / idiom 4) and the per-cell hold (idiom 2) reference.
Without this, Doom-class cabinets blow up the DAG node count by a
factor of ~10⁶. The recogniser is structural: detect a templated name
family with parallel assignments across the four lifecycle slots
(`__0`, `__1`, `__2`, RHS). Reject runs where any cell's body breaks
the template (those cells are special and stay as singleton DAG nodes).

---

## Comparison vs additive-stream catalogue

Mapping table from this catalogue (left) to the additive-stream
catalogue under `calcite-v2/docs/phase2-idiom-catalogue.md` (right):

| This idiom | Additive idiom | Status | Note |
|------------|----------------|--------|------|
| 1 (literal-keyed cascade)          | 1  | MATCH    | Same shape and frequency story. |
| 2 (self-hold default)              | 2 + 8 | MERGED | Additive splits "cascade-default self-hold" (its 2) from "top-level self-hold" (its 8). The fresh kiln read sees one structural shape (`else: var(--__1<self>)` or the bare-RHS degenerate case) — the only difference is whether a cascade wraps it. Worth folding into one recogniser; the cascade is a no-op envelope when it reduces to "fallback only." |
| 3 (address-keyed broadcast)        | 3  | MATCH    | Same. |
| 4 (function-cascade store)         | 4  | MATCH    | Same; both catalogues recommend unifying with idiom 3 under one normal form. |
| 5 (compound-AND short-circuit)     | 5 + 6 | MERGED | Additive idiom 5 is "slot-live gate" (a specialisation) and idiom 6 is "general AND short-circuit." Fresh read sees one structural shape — compound-AND with a 0/1-valued operand — and the slot-live gate is just the most-emitted instance. Recogniser is the same in both cases: hoist the gate. |
| 6 (wide-value split)               | 7  | MATCH    | Renamed for clarity ("word-write splitting" → "wide-value split across adjacent cells") to keep cabinet-agnostic. The design doc reference to "idiom 7" should track this rename, or equivalently the catalogue numbering can keep matching the additive's. |
| 7 (bit-field extraction)           | 11 | MATCH    | Same. Renumbered. |
| 8 (bit-decomposition function body) | —  | NEW      | Additive doesn't list this. It's currently recognised by v1's `classify_bitwise_decomposition` (logged 2026-04-15) and worth keeping in the catalogue as its own idiom; without explicit naming, a Phase 2 engineer might unwind the recognition while restructuring. |
| 9 (small priority cascade)         | 9  | MATCH    | Same. |
| 10 (`@function` cascade-body call) | 10 | MATCH    | Same; both note the inlining opportunity. |
| 11 (pure derived chain)            | 12 | MATCH    | Renumbered. |
| 12 (templated address space)       | —  | NEW      | Additive catalogue doesn't list this as an idiom; it's an implicit assumption beneath the broadcast write (its idiom 3). Calling it out separately matters because the DAG storage strategy depends on it: without an `AddressSpace` super-node, Doom-class cabinets explode the DAG. |
| —                                  | 6 (style-AND short-circuit) | MERGED into 5 | See note on this catalogue's idiom 5. |
| —                                  | 13 (repeated-load CSE)      | MISSING | Additive idiom 13 is "repeated `var(--__1xxx)` reads." This is real — happens whenever decode reads `--__1opcode` or `--__1AX` from multiple consumers. Fresh read missed it because it's a *consumer* pattern, not a kiln *emit* shape. The kiln doesn't dedupe consumers; it just emits each consumer's reference. The shape exists in the .css but it's a CSE opportunity, not a kiln-side idiom. Recommend keeping it on the Phase 2 list as a value-numbering pass under that name; it's distinguishable from the catalogue idioms because it's an emergent shape across many idiom-1 / idiom-9 / idiom-11 evaluations rather than a kiln-emit template. |
| —                                  | 14 (constant fold)          | MISSING | Same reasoning as above — emergent, not emitted as such. The kiln does emit `calc(var(--__1SS) * 16 + … - 2 + 65536)` (`kiln:241`) which is partially-foldable, so the *opportunity* exists in kiln output, but it's a generic compiler pass not a structural idiom. Worth keeping on Phase 2's list as a pass, separate from the structural idioms. |

Differences summary: this catalogue is shorter (12 vs 14 numbered
entries) because it merges shapes that are structurally one idiom even
when their kiln call sites differ (idioms 2+8 → 2; idioms 5+6 → 5),
and it adds two idioms (8: bit-decomp body shape, 12: templated address
space) that the additive read either folded into "function bodies" or
left implicit beneath the broadcast write. The two missing items (CSE,
constant fold) are real Phase 2 work but they're emergent patterns over
the DAG, not structural emit-time idioms — they belong on the Phase 2
work list as compiler passes, distinct from the shape-matcher
catalogue.

The numbering this catalogue uses departs from the additive one in
several places (bit extraction is 7 here vs 11 there, etc.). The v2
design doc super-node section currently references the additive
numbering (idioms 1, 3+4+5+7, 8, 9, 11). When Phase 2 lands on a
shared numbering, the design doc should be updated to track. In the
interim, recogniser-writers should treat the **names** (not numbers)
as the stable identifiers across both catalogues.

## What I am uncertain about

- **String literal handling.** Kiln's emit-css.mjs writes only integer
  literals; string properties exist in CSS-DOS for debug/display
  purposes (`template:188-198`'s `emitDebugDisplay`) but I didn't see
  them in any execution-side cascade. If a future emitter introduces
  string-keyed cascades, idiom 1's "integer literal" operational test
  needs to be widened to include string literals; the matcher is the
  same.

- **Whether `--applySlot`'s structural shape is one idiom or two.**
  Idiom 4 (the call cascade) treats `--applySlot` calls as a uniform
  N-deep chain. But the function body itself (`css-lib:120-152`) is
  a 6-branch priority cascade (idiom 9) with compound-AND guards
  (idiom 5). Once the cascade chain is recognised as idiom 4, do we
  still want to fire idiom 9 inside its body, or do we treat the
  whole `--applySlot(...)` chain as one opaque super-node? My read is
  the latter — once idiom 4's normal form has the relevant structural
  fields (gate, lo-offset, hi-offset, val, width per slot), the body's
  internal priority cascade is a Phase 3 codegen detail, not a Phase 2
  idiom recognition concern. Worth confirming with the recogniser-
  writer when they get to idiom 4.

- **Idiom 11's "no self-hold" requirement vs derived properties that
  reference previous-tick state of a *different* slot.** A pure
  derive may legitimately read `var(--__1AX)` to compute `--_sAX`;
  that's fine — the rule is no self-hold (`var(--__1<sameLHS>)`),
  not "no previous-tick read at all." The operational test in idiom
  11 is worded to allow this, but a recogniser writer should
  double-check the test on a few real assignments from
  `decode:289-384` before committing.

- **Boundary between idiom 9 (priority cascade) and idiom 1 (literal
  cascade).** Some cascades start with 1-2 priority branches and
  continue with N literal branches on the same property
  (`kiln:135-165` wraps the literal cascade inside the priority head).
  My recommendation in idiom 9 is to recognise the priority head
  first; the alternative is one combined "head-and-body" recogniser.
  Recogniser-writer's choice — both should land on the same final
  super-node graph.

- **Whether kiln's emit-time fuser (`tryFuseWordPair` and friends)
  leaves any lo/hi pairs unfused that Phase 2 should pick up.** The
  fuser handles many shapes (direct, dispatch-conditional, ea-based,
  generic +1, wrap-in-calc +1) but only at emit time, only on
  call-order-adjacent writes. If a future emitter or non-kiln
  cabinet produces lo/hi pairs across non-adjacent writes, idiom 6's
  recogniser would catch them on the calcite side. I haven't audited
  the full kiln to confirm there are no kiln-side missed fusions
  worth catching; the operational test in idiom 6 is conservative
  and would fire on whatever the fuser missed.
