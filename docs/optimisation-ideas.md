# Optimisation Ideas — Potentials

Running log of optimisation ideas considered for the calcite bytecode
interpreter. These are **candidates**, not committed work. Each entry
sketches the shape of the change, the expected payoff, and — critically —
the check that keeps it generic rather than CSS-DOS-specific (cardinal
rule: calcite must never encode x86 knowledge).

Context at the time of writing (2026-04-15):

- Current baseline after the compound-AND fuser landed: bootle-ctest
  cold = 134K ticks/s, 17.1% of 4.77 MHz 8086, 2472 ops/tick.
- Mode 13h splash fill (64000 pixels) runs ~0–165K ticks → ~2.5 ticks per
  pixel write → visible "pixels scrolling" painting.
- Per-tick op mix: 826 LoadLit, ~900 arithmetic (Mod/Div/Round/Mul/Add/Sub),
  206 LoadSlot, 202 BranchIfNotEqLit, 108 LoadState, rest small.
- Goal: ~100× speedup on the fill. Per-op tuning alone won't get us there.
  The remaining big wins are temporal (fuse ticks, memoise regions) or
  structural (recognise more patterns, eliminate dead work per tick).

---

## Genericity test

Before promoting any idea below to a real change, it must pass this check:

- **Does the implementation reference anything x86-specific** — opcode
  numbers, register names, memory regions, mnemonics, CSS property names
  like `--opcode` / `--DI`?
- **Does the pass work purely on bytecode shape** — slots, literals,
  control-flow edges — with no mention of what the slots mean?
- **Does it fire on an unrelated CSS program** that happens to have the
  same shape (or, if no such program exists, could it in principle)?

If the pass only fires on one specific region of CSS-DOS output, it's
probably an x86 pass in disguise.

---

## (a) Native bitwise function recognition

**Status**: **done** 2026-04-15. 1.9× on bootle-ctest cold
(134K → 259K ticks/s; 2472 → 1342 ops/tick). Genericity preserved:
the recogniser matches body shape (bit-extract locals + combine pattern
in the reconstruction sum), never function names.

**Problem**: `--and`, `--or`, `--xor`, `--not` in CSS-DOS are implemented
as 32-local bit-by-bit decomposition (16 `mod(round(down, v/2^n), 2)` ops
per argument, then a reconstruction). A single 16-bit bitwise call
compiles to ~100 ops of `Mod/Div/Round/Mul/Add`. With 150 AND call sites,
44 XOR, 50 OR, 19 NOT across the CSS, any flag-update path that touches
one of these balloons the per-tick work.

**Fix**: extend `try_compile_by_body_pattern` with recognisers for the
32-local bit-decomposition form, emitting a single `Op::And16`, `Op::Or16`,
`Op::Xor16`, `Op::Not16` (native u32 bitwise in the evaluator).

**Expected payoff**: ~100 ops → 1 op per call. Per-tick impact depends on
how many bitwise calls fire per tick on the hot path; estimated 10–20%
tick speedup.

**Genericity check**: pattern-matches the *body shape*, not the function
name. Analogous to existing `is_left_shift` / `is_right_shift` /
`is_mod_pow2` / `is_bit_extract` recognisers in `try_compile_by_body_pattern`.
Any CSS function whose body is a 16-term bit-decomposition gets the fast
op. ✅ Clean.

**Risks**: the reconstruction shape has variants (XOR uses
`min(1, a+b) - a*b`, AND uses `a*b`, OR uses `min(1, a+b)`). Each needs
its own recogniser; easy to miss one and silently fall back to the slow
path.

---

## (b) Dead LoadLit sinking

**Status**: candidate. Standard compiler pass.

**Problem**: `LoadLit` is 826/tick = 33% of ops. Many load constants into
slots that are only read inside a branch; if the branch isn't taken, the
LoadLit was pure overhead. This is the residual of the compound-fuser win
— we removed the CmpEq/BranchIfZero/Mul but the constants for those
branches are still being loaded unconditionally.

**Fix**: dataflow pass that sinks a LoadLit past a branch when the slot is
only read on the other side. More generally, any op whose only consumers
are downstream of a conditional jump can be hoisted past that jump.

**Expected payoff**: up to 600 of the 826 LoadLits/tick could be
branch-gated. 25% tick speedup if achieved.

**Genericity check**: pure dataflow on the bytecode. "Op X writes slot S;
S is only read by ops Y, Z; Y and Z are past branch B; move X past B."
No semantics involved. Standard LLVM-style pass. ✅ Clean.

**Risks**: branch-target patching is fiddly; the sinker must not cross a
slot-aliasing boundary. The current fuser already walks this terrain
(via the index-remap after nop-stripping).

---

## (c) Affine self-loop fixed-point recognition

**Status**: candidate. Highest payoff, most design work, most risk.

**Problem**: tight 8086 loops (`REP STOSB` / `LOOP` / manual `JNZ` back-edges)
execute the same dispatch entry thousands of times in a row. Each
iteration runs the same ~2472 ops to rediscover the same opcode
dispatch, to produce the same-shape delta: one register + 1, another
register - 1, one memory cell written, loop back. The mode 13h fill is
64000 such iterations.

**Fix**: detect any dispatch-table entry (or dispatch-chain region) whose
effect is:

- `slot_A := slot_A + k1`
- `slot_B := slot_B + k2` (often -1)
- `state_mem[slot_C] := slot_D` (or similar single-store)
- control flow: dispatches back to itself until a guard slot reaches a
  literal (e.g. zero)

and replace it at runtime with a closed-form bulk update: compute N = how
many iterations until the guard fires, apply the affine deltas N times in
one step (slot_A += N*k1, slot_B += N*k2, memset-like store of N bytes
starting at slot_C initial value).

This is the classical compiler optimisation **induction-variable +
loop-idiom recognition** (LLVM's `IndVarSimplify` + `LoopIdiomRecognize`).

**Expected payoff**: 100× on the fill loop. A 64000-iteration loop
collapses to one dispatch cycle. Ticks per pixel goes from 2.5 → ~0.0001
(since calcite ticks align with 8086 cycles, but N iterations now
complete in one calcite step; we'd need to decide whether to advance the
tick counter by N to keep conformance with the cycle-count model or
treat the whole thing as one tick — separate question).

**Genericity check**: the pass never mentions "opcode", "REP", "DI", "CX",
or any CSS property by name. It works on:

- an op region that is the body of a single dispatch entry,
- producing at most one store + N register updates where each register
  update is `slot := slot + integer-literal`,
- whose terminal control flow re-enters the same dispatch entry key,
- gated by a `BranchIfNotEqLit` or `DispatchChain` on a slot that matches
  an induction variable.

**Sanity check**: run the pass on a non-CSS-DOS program with a synthetic
self-loop. If it only fires on CSS-DOS output, it's x86 in disguise.
✅ Clean if written carefully; ⚠️ easy to write a cheating version by
accident.

**Risks**:

- **Conformance**: if the closed-form gets the iteration count wrong or
  skips a side effect (interrupt check, I/O port access inside the loop
  body), the emulator diverges. Mitigation: the pass must prove the
  dispatch entry has *no* side effects other than the recognised affine
  pattern. Any unrecognised op in the body → bail out.
- **Tick-counting semantics**: CSS-DOS uses cycle counts for timing
  (interrupts, PIT). Collapsing 64000 iterations to one calcite step
  means interrupts fire late. Fix: advance `cycleCount` and `tickCount`
  by the correct fused amount so timing observers see continuity.
- **Memory-store fusion**: for STOSB-like patterns, need a fast
  "write N bytes starting at A" primitive. The broadcast-write system
  already has the right shape (addressable memory cells); extending it
  to range writes is a separate implementation problem.

---

## (d) Wider dispatch-chain recognition

**Status**: candidate. Moderate payoff, moderate work.

**Problem**: only 40 ops/tick hit `DispatchChain` (the already-recognised
multi-way dispatch from runs of `BranchIfNotEqLit` on the same slot).
202 `BranchIfNotEqLit` remain as linear ladders. Many of these come from
compound conditions like `style(--opcode: N) and style(--uOp: K)` — now
that the compound fuser emits a chain of `BranchIfNotEqLit` per branch
(one per conjunct), the chain recogniser could catch a 2-key dispatch
pattern if it were extended.

**Fix**: recognise runs of N pairs `BranchIfNotEqLit a=S1,v=X_i` +
`BranchIfNotEqLit a=S2,v=Y_i` (same two slots, different literal pairs)
and collapse to a 2-key dispatch table keyed on `(slot_S1, slot_S2)`.

**Expected payoff**: probably another 150 ops/tick → 1 table lookup.
~5–10% tick speedup.

**Genericity check**: structural, same category as existing single-slot
chain recognition. ✅ Clean.

**Risks**: the table size blows up as product of key cardinalities. Guard
with the same sparseness/size caps already used for single-key chains.

---

## (e) Value-keyed region memoisation

**Status**: speculative. Potentially huge payoff, architectural change.

**Problem**: even with (a)–(d), every tick re-runs the full dispatch from
scratch. If a region's inputs and outputs are both small, and the same
inputs recur, we're doing redundant work.

**Fix**: at compile time, partition the op stream into **regions** (one
per dispatch-table entry, plus the main-stream prologue/epilogue). For
each region, record which slots it reads (inputs) and which it writes
(outputs). At runtime, memoise `(region_id, tuple-of-input-slot-values)
→ tuple-of-output-slot-deltas`. On region entry, probe the cache; on
hit, apply the deltas and skip the ops.

**Expected payoff**: in a tight loop where the input tuple cycles through
a small set of values, hit rate approaches 100% → region cost drops from
N ops to 1 HashMap probe + small apply. Could be 10–100× on hot loops
without recognising the loop structure.

**Genericity check**: the cache is keyed on raw slot values — no
semantics. Memoisation is language-agnostic. PyPy and LuaJIT do similar
specialising-trace work. ✅ Clean.

**Risks**:

- **Correctness**: if the input-set computation misses a slot the region
  actually reads, silent divergence. The pass must be conservative — any
  op whose read-set isn't statically analysable (e.g. `LoadMem` with a
  runtime address) poisons the region (read-set includes "all of
  memory", so cache never hits).
- **Cache pressure**: input-tuple cardinality can be huge. Need eviction
  and a hit-rate threshold to back off if a region is non-memoisable.
- **Debuggability**: a cached region hides the op stream from profilers
  and the debugger. Need a debug mode that bypasses the cache.

---

## (f) Change-gated ops (per-tick dirty tracking)

**Status**: candidate. Partial infrastructure may already exist
(`Change detect` phase is 1.3% of tick).

**Problem**: many computed properties don't change tick-to-tick (e.g. CS
register during the fill loop). Recomputing them every tick wastes work.

**Fix**: maintain a dirty-slot set per tick. An op whose read set is
disjoint from the dirty set produces the same output as last tick —
skip. Requires a per-op "inputs" map (partially available) and a
fixed-point iteration over dirty propagation.

**Expected payoff**: unclear without instrumentation. In a tight loop
most of CPU state is dirty, but BIOS variables, segment registers,
IVT entries, etc. are stable — could save hundreds of ops/tick if
dependencies are granular enough.

**Genericity check**: pure dataflow, no semantics. ✅ Clean.

**Risks**: dependency tracking granularity. If op inputs include "the
entire memory array", dirty-propagation is useless. Need to split memory
reads by address when the address is a compile-time literal.

---

## Stacking order

If chasing the 100× systematically:

1. (a) native bitwise — quick win, clears the arithmetic noise.
2. (b) LoadLit sinking — clears the control-flow-gated overhead.
3. (d) 2-key dispatch chain — finishes the pattern-recognition pass.
4. (f) change-gated ops — cheap dirty tracking layer.
5. (c) affine self-loop fusion — the big structural move, designed
   against a clean baseline.
6. (e) value-keyed memoisation — only if (c) turns out not to generalise
   well, or as an additional layer beneath it.
7. (g) predictive event scheduling — extends PR-1-class bulk-op
   collapse to work even when periodic interrupts interleave.
   Additive to whatever bulk-op detectors ship (fill, memcpy, blit).
   Deferred until after PR 1–3 land and we have real numbers to
   measure a speculation framework against.

Each step has its own verification plan (register-state diff at fixed
ticks, plus conformance comparison against a reference emulator where
the CSS boots cleanly with `fulldiff.mjs`).

---

## (g) Predictive event-driven scheduling: enmesh bulk ops with periodic interrupts

**Status**: idea. 2026-04-18.

**The observation.** The program isn't running "N cycles of A, then 1
cycle of B, then N cycles of A." It's running **two deterministic
processes concurrently, interleaved**. For a splash clear with a timer
IRQ on:

- **A**: a tight fill loop. Each iteration writes one byte at a known
  address offset and costs a known number of CPU cycles (modulo minor
  variance from memory-access timing).
- **B**: a timer IRQ. Fires every T PIT cycles. Runs `int 8` (increment
  BDA tick count, EOI, iret) — also deterministic, also known cost.

The interleaving — *which byte of A gets written between which pair of
B invocations* — is algebra, not simulation. If A writes byte `i` at
global cycle `c0 + i·k` and B fires at cycles `T, 2T, 3T, …`, the
positions where A yields to B are computable in closed form.

Today's bulk-fill detector (PR 1 of `2026-04-18-bulk-ops.md`) is the
degenerate case: "assume B never fires during A, do A natively,
resume." It works for the splash because the fill typically completes
inside one timer window. The generalisation is: **keep the bulk-fill
collapse, but allow B to slot into it at predicted cycle offsets**.

**Prior art.** This is "event-driven scheduling" / "lazy synchronisation"
from emulator design. FCEUX runs the NES CPU forward to the next PPU
event, then catches the PPU up, rather than stepping both in lockstep.
MAME does the same with subordinate chips. In JIT terms it's also a
cousin of trace speculation (PyPy, LuaJIT): trace the hot path
assuming nothing interesting happens, bail out if something does.

**Mechanism sketch.**

1. Detector recognises a bulk op (today: fill) — same as PR 1.
2. Evaluator, on each tick where `--bulkOpKind` is active, asks: "how
   many *logical* fill iterations can happen before the next scheduled
   event?"
   - Scheduled events come from `cycle_tracker.rs` / `tick_period.rs`:
     the PIT period, any pending masked-unmask transitions, DMA
     deadlines, etc. Whatever periodic state already exists as
     observable CSS state.
   - "Logical iteration cost" comes from the CSS cycle cost of the
     bulk op — already tracked.
3. Run the fill natively for that many iterations (a slice of
   `memory.fill`, not the whole range).
4. Advance the cycle counter by `n · k`, fire the next event (run the
   IRQ handler's bytecode as normal), return.
5. Repeat until the fill completes.

**Why this matters beyond "fill faster with IRQs on."** It generalises:

- memcpy / blit vs. scan-line ticks in mode-change programs.
- Any tight compute loop with a periodic external observer
  (keyboard poll, timer, vertical-retrace flag).
- The framework for PR 2 / PR 3 to remain fast even when the guest
  turns IRQs on during graphics setup.

It's the difference between "we made one loop faster when it's
isolated" and "the JIT stops pretending it has to simulate time
linearly whenever anything interrupts anything."

**Hard parts, in order of gnarliness.**

1. **Cost-of-iteration estimation.** The bulk-fill detector sees a CSS
   *shape*, not the original REP STOSB. To know "each logical byte
   costs C CPU cycles" we'd need either (a) a per-binding
   cycle-cost annotation emitted structurally by the CSS (risk: cardinal
   rule — probably fine because cycle cost is an honest observable),
   or (b) a measurement from running the same fill cold once and
   extrapolating. (b) is hacky but genericity-safe.

2. **Event-set stability.** "Next event at cycle E" requires the set
   of pending events not to change during the bulk run. The obvious
   stabilisers are PIT register writes (would change T), PIC mask
   writes (would enable/disable interrupts), or the guest changing IF.
   All observable as state-var writes. Approach: speculation + bailout.
   Assume stable, run the slice, verify nothing that affects event
   timing was touched during the slice. If it was, bail out and run
   those ticks normally. This is standard trace-JIT deopt.

3. **IRQ handler side effects into the bulk region.** If the IRQ
   handler writes to the VGA buffer the fill is targeting, the fill's
   speculation is invalidated for that overlap. Detectable: the
   handler's bytecode has StoreMem / broadcast writes whose target
   slots intersect the binding's `addr_range`. Approach: pre-analyse
   the handler's write footprint at compile time; if it overlaps any
   known bulk-fill range, disable speculation for that (binding,
   handler) pair. Again structural, not x86-specific.

4. **Nested events.** Two periodic interrupts with different periods
   (timer + keyboard, say) mean the "next event" changes on every
   iteration. Priority-queue scheduling, same as any discrete-event
   simulator.

**Genericity check.**

- Does the mechanism reference x86 specifics? No — it references
  "scheduled events" (observable CSS state transitions at known
  cycles) and "bulk ops" (already-detected CSS patterns). Whether
  the event is an IRQ or a DMA completion or a video-mode change is
  opaque to the scheduler.
- Does it work on bytecode shape? Yes — the bulk-op bindings are
  shape-derived, the event schedule comes from the existing
  `cycle_tracker` / `tick_period` infrastructure which is also
  shape-derived.
- Could it fire on an unrelated CSS program? In principle yes —
  any CSS with a bulk op + a periodic state driver has the same
  shape. (No such program exists yet, but that's a popularity problem,
  not a genericity one.)

**Why this is NOT PR 1.** PR 1 is the zero-speculation case and ships
a measurable speedup on its own. Predictive scheduling is strictly
additive: it extends PR 1's fast path from "isolated bulk op" to
"bulk op enmeshed with scheduled events." Build PR 1, PR 2, PR 3 first
— measurable numbers matter — then reach for this. The intellectual
payoff is large enough that it deserves its own design doc when we
pick it up: speculation policy, bailout triggers, handler-footprint
analysis, and the cost-model for iteration time.

**Verification plan sketch.**

- Microbenchmark: synthetic CSS with a bulk fill + a fake periodic
  state-var driver ("every 1000 ticks, increment `--fakeIrq`").
  Confirm: (a) bulk collapses, (b) fakeIrq increments at the right
  positions during the collapsed range, (c) final memory matches a
  tick-by-tick reference run bit-exactly.
- Real workload: splash clear with timer IRQ enabled. Measure halt
  tick before/after and register-state-diff vs. `fulldiff.mjs`.
- Stress: same workload but with the guest writing to the PIT
  mid-fill, to exercise the bailout path.
