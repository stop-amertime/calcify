# Phase 3b scoping — descriptor-driven applier

**Branch:** `worktree-rep-3b` (in `.claude/worktrees/rep-3b/`)
**Base:** `cleanup-2026-05-01` at commit `de6b8c0` (phase 3a landed)
**Date:** 2026-05-07

## What this doc is

Phase 3a (recogniser + BulkClass + validator) shipped on `cleanup-2026-05-01`.
Phase 3b (the actual descriptor-driven applier flip) is bigger than
one session can responsibly land. This doc captures the analysis
done at the start of the worktree, the design choices that need to
be made, and the order in which the remaining work should land.

The goal of working in a worktree is exactly this: make space for
multiple landings without polluting `cleanup-2026-05-01` until the
work is ready.

## Pre-mission baseline (CLI, doom-loading)

`CALCITE_REP_GENERIC=0`, freshly-built binary in this worktree:

```
watch=loading tick=5100000 t_ms=32528 cycles=66648592
watch=ingame  tick=34650000 t_ms=216568 cycles=389935570 halted=1
Elapsed: 216.568s (34650000 ticks, 159996 ticks/sec)
```

The phase-3b perf gate (±1% of `runMsToInGame`) means the descriptor-
driven applier must reach in-game between **214.4s and 218.7s** wall.

Web-target baseline still owed (the user's `calcite-bench` was
holding the binary lock at session start; another agent may already
be running it).

## What's actually hard about 3b

### Hardcoded knowledge in the existing path

`compile.rs::rep_fast_forward` (and its `_cmps_scas` helper) hardcodes:

1. Eight opcode bytes, dispatched by `match`.
2. Per-opcode "this is byte-width vs word-width" (1/2 step bytes).
3. Per-opcode "this writes / reads from / compares" semantics.
4. Segment-base computation (`seg << 4` → `seg * 16`).
5. The `(IP_at_prefix + 1 + prefixLen)` post-fixup formula.
6. SUB-flag computation for CMPS/SCAS (`compute_sub_flags`).
7. Per-iter cycle cost (10 STOS, 17 MOVS, 22 CMPS, 15 SCAS).
8. The "REP / REPE / REPNE" semantics gating early exit.
9. Routing through `state.read_mem` for source bytes (so packed
   cells / windowed-byte-array / extended map all just work).
10. `bulk_fill` / `bulk_store_byte` for the destination side.
11. The CMPS/SCAS post-tick `rep_already_exited` short-circuit.

For 3b to retire `rep_fast_forward`, the descriptor must carry —
or the runtime must compute structurally — the equivalent of every
one of those.

### What the current descriptor provides

`LoopDescriptor` (after phase 3a):

- `counter`: counter slot + step.
- `pointers[]`: each pointer's slot, base step, direction flag slot,
  flag bit.
- `writes[]`: address expression + value expression (raw `Expr`).
- `predicate`: full IP-stay predicate as `StyleTest`.
- `ip_property`, `ip_self_property`, `ip_advance_literal`.
- `flag_conditioned: bool`.
- `bulk_class`: `ReadOnly | Fill | Copy | PerIter`.

### Gaps between descriptor and the runtime needs

| Runtime needs | Descriptor has? |
|---|---|
| Linear address per iter | `addr_expr` (raw) — needs structural decomposition into `(seg, pointer)` |
| Word vs byte width | Inferable from `pointer.base_step` (1 or 2). Not stored explicitly. |
| Constant fill value | `val_expr` — for STOS it's a single `Var`. Recogniser can resolve to slot. |
| Per-iter source byte for MOVS | `val_expr` is `var(--_strSrcByte)` — a derived slot whose value reflects PRE-fast-forward SI, not per-iter SI. **Core MOVS DRIFT problem.** |
| Predicate evaluation per-iter (CMPS/SCAS) | `predicate` (raw `StyleTest`) — needs an evaluator usable from within `compile.rs` that takes `(&CompiledProgram, &State, &[i32])` not `&mut Evaluator`. |
| Cycle cost | Not in descriptor. Would need a separate per-opcode-cycle-cost recogniser, or accept a fixed schedule. |
| `IP + 1 + prefixLen` formula | `ip_advance_literal=1` + a separate `prefixLen` lookup. |

## Three design questions that must be answered before code lands

### Q1: Where does the applier run from?

**Option A.** Keep the hook inside `compile.rs::rep_fast_forward`, where
it's called with `(&CompiledProgram, &mut State, &[i32])`. Means we
can't directly call `Evaluator::eval_expr`. Means everything the
applier needs has to be either a plain slot read or pre-resolved into
structural metadata on the descriptor. **Cardinal-rule cleanest** —
the descriptor carries everything; the applier is a small, dumb
walker.

**Option B.** Move the hook so it runs through `Evaluator`, with
`&mut self` access. Lets us call `eval_expr` on raw `Expr`s. But:
- Touches every caller of the per-tick pipeline.
- Couples the applier to evaluator internals (state.properties etc.).
- Calling `eval_expr` per-iter inside a 32 KB REP MOVS will tank
  perf in a way that bulk_fill/bulk_copy can't recover from.

**Recommendation:** Option A. The applier is a fast bytecode-style
walker over pre-resolved slot indices. Option B is a tar-pit.

### Q2: How does MOVS get its per-iter source bytes?

The cabinet's MOVS val_expr reads `--_strSrcByte`, a derived slot
whose dispatch body is something like:

```css
--_strSrcByte: --readByte(calc(--__1DS * 16 + --__1SI));
```

That dispatch is invoked normally during a CSS tick: SI is updated
first, then `--_strSrcByte` is recomputed. But the fast-forward
hook fires AFTER one CSS tick and bulk-applies the remaining N-1
iterations. It can't re-tick the CSS to recompute `--_strSrcByte`
with new SI values — that defeats the entire bulk premise.

**Option A.** Recogniser traces `--_strSrcByte`'s definition at
compile time, sees it's `read_mem(DS*16 + SI)`, and emits a
structured `IndirectReadDescriptor` on the loop. The applier reads
the source byte by computing the address itself and calling
`state.read_mem` per-iter. (`read_mem` is fast — just a handful of
ns; it routes through packed cells / windowed array / extended map
as needed. The hardcoded path already does this.)

**Option B.** Mirror the cabinet's `repGuardReg` trick: don't fast-
forward MOVS at all. Let the CSS run per-iter for MOVS (slow). This
is what regresses perf >> ±1%.

**Option C.** Inline-evaluate `--_strSrcByte`'s definition at the
applier. Means partial expression evaluation inside the applier.
Tar-pit again.

**Recommendation:** Option A. Phase 3a noted this as the "MOVS
DRIFT" — promoting Fill→Copy via intermediate-tracing. Add a
compile-time pass: for each `var(name)` reference in a `val_expr`,
look up `name`'s dispatch body. If the body is structurally
`read_mem(seg_slot * 16 + pointer_slot)` and `pointer_slot` is one
of the descriptor's pointer entries, capture that as
`val_expr.indirect_read = Some((seg_slot, pointer_slot))`.

### Q3: Predicate evaluation for CMPS/SCAS

CMPS/SCAS predicate is a disjunction of conjunctions:
```
(repContinue AND repType=1 AND repZF=1) OR
(repContinue AND repType=2 AND repZF=0)
```

Per-iter the applier needs to know "did the loop just exit because
the flag condition flipped?" That requires recomputing the equivalent
of `repZF` for each new (DI, SI) pair.

The hardcoded path solves this in `rep_fast_forward_cmps_scas` by
computing `(src_v == dst_v)` directly and applying `compute_sub_flags`
at the end. The descriptor-driven applier needs to either:

**Option A.** Use the same hardcoded comparison logic, gated on
`bulk_class=ReadOnly && flag_conditioned`. Keeps complexity bounded
but encodes "this is x86 SUB-flags" into the applier — cardinal-rule
violation reintroduced.

**Option B.** Recognise that the predicate's flag-bit terms map onto
a "compute byte-equality and check it" pattern, structurally extract
the comparison into the descriptor at compile time. The descriptor
gains a `comparison: Option<ComparisonShape>` field where
`ComparisonShape` describes "compare bytes at pointer A vs pointer B"
(CMPS) or "compare register vs byte at pointer" (SCAS). Evaluator
can then produce the boolean result without knowing it's a SUB.

The flag-word output (writing back to `--flags`) still needs
SUB-flag computation. But that lives inside the cabinet's `--flags`
dispatch body — not the loop. Phase 3b can pre-recognise that body
once and cache its computation as a `compute_subflags(dst, src)`
descriptor.

**Recommendation:** Option B is the cardinal-rule clean answer but
it's the most expensive. For the first 3b landing, accept Option A
as a temporary and document it as "the next cardinal-rule wart to
fix after `rep_fast_forward` itself is gone."

## Order of work for whoever picks 3b up

These should land as separate commits on `worktree-rep-3b`.

1. **Decompose `addr_expr` structurally.**
   - Scan each `WriteEntry.addr_expr` for shape `calc(seg_slot * 16 + pointer_slot)` or `calc(pointer_slot + seg_slot * 16)`.
   - Add `addr_decomposition: Option<(seg_property, pointer_property)>` to `WriteEntry`.
   - Verify on doom8088: all 4 STOS/MOVS write entries decompose cleanly.
   - Unit tests for both orientations of the addition, plus a "neither matches → None" negative test.

2. **Recognise indirect-read intermediates.**
   - For each `WriteEntry.val_expr`, walk the value's `Var` references.
   - For each `Var(name)` that isn't a pointer mirror, look up `name`'s dispatch body in the cabinet.
   - If the body is structurally `read_mem(seg * 16 + pointer)` (or any function-call shape that reduces to a memory read keyed on a pointer slot), emit `val_expr.indirect_read = Some((seg_property, pointer_property))`.
   - Promote `BulkClass::Fill` → `BulkClass::Copy` for descriptors whose val_expr resolves to an indirect read through a pointer mirror.
   - Verify on doom8088: 0xA4/A5 (MOVS) reclassify to Copy.

3. **Build the dual-execute harness.**
   - `CALCITE_REP_DUAL=1`: snapshot state, run new applier on a clone, run hardcoded path on real state, diff at end. Panic on divergence.
   - The applier itself is stub for the first commit — just walks the descriptor and calls a `unimplemented!()` per `BulkClass`. The dual harness is the safety net for landing implementations one variant at a time.

4. **Implement `BulkClass::Fill` for STOS.**
   - Reads the constant from the val_expr's resolved slot, computes
     destination range from the address decomposition + pointer
     value + counter + direction flag, calls `bulk_fill` /
     `bulk_store_byte` exactly as `rep_fast_forward` does.
   - Verify under dual mode: STOSB / STOSW agree with hardcoded path
     byte-for-byte through doom boot.

5. **Implement `BulkClass::Copy` for MOVS.**
   - Uses `val_expr.indirect_read` to know "source byte at
     (src_seg * 16 + src_pointer + iter*step)". Per-iter (or batched
     when packed-cells/windowed-array don't intersect) read+write.
   - Verify under dual mode: MOVSB / MOVSW agree byte-for-byte.

6. **Implement `BulkClass::ReadOnly` for CMPS/SCAS/LODS.**
   - LODS: read AL/AX, advance pointer, decrement CX, no exit.
   - CMPS/SCAS: as `rep_fast_forward_cmps_scas` but driven by
     descriptor metadata. Initially can call the hardcoded
     `compute_sub_flags` (Q3 Option A).
   - Verify under dual mode.

7. **Flip the data flow.**
   - Inside `rep_fast_forward`, when `CALCITE_REP_GENERIC=1`,
     dispatch on the descriptor's `BulkClass` and call the new
     applier instead of the hardcoded path.
   - Validator becomes redundant under this flag; remove or repurpose.

8. **Bench.**
   - `CALCITE_REP_GENERIC=1`: doom-loading CLI must reach in-game
     within ±1% of the 216568 ms baseline. Web target similarly.
   - If MOVS is slower than 17 cycles/iter via the new path, profile
     the per-iter `read_mem` calls and decide whether to add a
     true bulk_copy path that skips packed-cell routing for "known
     flat" address ranges (consult `state.virtual_regions`).

9. **Snapshot diff.**
   - Once perf is in budget, run with `CALCITE_REP_GENERIC=1` and
     dump memory snapshots every 100K ticks. Compare against
     baseline run with `CALCITE_REP_GENERIC=0`. Zero divergence.

10. **Land on `cleanup-2026-05-01`.**
    - Squash-merge or rebase the worktree branch.
    - Phase 3b complete. Plan moves to checkpoint 4 (flip default).

## Checkpoint hygiene for the worktree

Each step above is a separate commit. Smoke 7/7 must pass at every
commit, regardless of whether the new path is the active one. Default
flag remains OFF until step 7 ships.

If perf can't be hit at step 8, the right move is to pause and
either:
- Add the bulk-copy specialisation that motivated the original
  3a/3b split (faster than per-iter `read_mem`), or
- Roll back the flip and ship phase 3b as "applier exists, gated
  off; hardcoded path stays default" — which leaves checkpoint 4
  unable to fire.

## Why the previous agent's "ship 3b in one session" advice doesn't fit

The advice was structurally sound: don't ship per-iter without
bulk specs. But the advice assumed the descriptor already carried
enough metadata to drive the applier. It doesn't — that's items 1-2
above, which are themselves a meaningful body of work.

So 3b is genuinely a multi-commit effort. The worktree exists
precisely to let it land in steps without disturbing the main
branch.
