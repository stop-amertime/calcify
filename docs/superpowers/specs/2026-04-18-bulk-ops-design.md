# Bulk memory ops — range-predicate CSS shape + calcite lowering

**Date:** 2026-04-18
**Status:** design
**Supersedes:** `2026-04-18-memory-fill-op-design.md` (compile-time approach
proven impossible for REP STOSB — see that doc's "blocker" findings)

## Problem

Calcite ticks the whole CPU once per emitted byte. A 64 KiB `REP STOSB`
fill takes ~64 000 ticks. A 20 KiB Mode-13h blit copy takes ~20 000
ticks. Memory-bound graphics work dominates execution time even after
calcite's existing optimisations.

The prior spec tried to compile-time pattern-match REP STOSB in the
bytecode; a diagnostic probe (commit `badb0c7`) showed the bytecode
contains **zero `StoreMem` ops** on real CSS-DOS output — all memory
writes go through broadcast-write ports that emit **one byte per tick**.
The fill loop is not expressed in CSS at all. It is expressed as
"calcite's tick scheduler runs the whole CPU while REP is active." No
static pattern exists to match.

**Runtime projection** (`tick_period.rs`, `cycle_tracker.rs`) is the
fallback but its ceiling is bounded by how quickly it can lock onto a
cycle and by correctness risk in the projection step. We need
something structurally different.

## Approach

**Change the CSS so it expresses bulk memory operations in a single
Chrome tick, by parallel evaluation of every memory cell.** Chrome
evaluates all ~1 M memory-cell property assignments per tick — they are
already parallel. If each cell's assignment includes a "am I in the
bulk-op range?" clause, then one Chrome tick writes the whole range.

This is honest Chrome work. Every cell does a real range check every
tick the bulk op is active. Calcite recognises the range-predicate
broadcast shape and lowers it to a native `memory.fill`/`copy_within`.

**Rule compliance** (per updated `CLAUDE.md`):
- Chrome does real work: every cell does a range check and, if hit,
  computes a per-address value.
- No dummy properties, no side-channels, no metadata leaks to calcite.
- The CSS is a valid Chrome program in its own right.
- Calcite recognises structure; it does not cheat.

## The bulk-op mechanism

### CSS-DOS side: new BIOS service

A new interrupt dispatches bulk memory ops. Concrete choice:
`INT 0x2F, AH=0xFE` (reserved vendor space; unused in real DOS).
Subfunction in `AL`, parameters in other registers.

| AL | Operation | Parameters |
|----|-----------|------------|
| 0x00 | Fill | `ES:DI` = dst, `CX` = count, `AL → DL before INT` = byte value |
| 0x01 | memcpy (non-overlapping) | `DS:SI` = src, `ES:DI` = dst, `CX` = count |
| 0x02 | masked blit (transparent colour) | `DS:SI` = src, `ES:DI` = dst, `CX` = count, `DL` = transparent colour |
| 0x03 | palette-mapped blit | `DS:SI` = src (palette indices), `ES:DI` = dst, `CX` = count, `BX` = palette base address |

(Exact parameter packing is settable during implementation; listed
concretely so the plan has something to point at.)

`INT 2F/AH=FE` is a single CPU instruction. Chrome executes the
interrupt dispatch normally; the handler's CSS body does the bulk
memory write in the same tick.

### CSS-DOS side: state properties

Six new CSS custom properties, set atomically by the INT 2F/FE
handler:

```css
@property --bulkOpKind   { syntax: "<integer>"; initial-value: 0; inherits: false; }
@property --bulkDst      { syntax: "<integer>"; initial-value: 0; inherits: false; }
@property --bulkSrc      { syntax: "<integer>"; initial-value: 0; inherits: false; }
@property --bulkCount    { syntax: "<integer>"; initial-value: 0; inherits: false; }
@property --bulkValue    { syntax: "<integer>"; initial-value: 0; inherits: false; }
@property --bulkMask     { syntax: "<integer>"; initial-value: 0; inherits: false; }
```

`--bulkOpKind`: 0 = inactive, 1 = fill, 2 = memcpy, 3 = masked, 4 =
palette-mapped.

**One-shot semantics.** The INT 2F/FE handler sets `--bulkOpKind` and
the other bulk properties for exactly one tick. During that tick, every
memory cell's assignment evaluates its range-predicate clause. On the
NEXT tick, `--bulkOpKind` reverts to 0 — achieved by making
`--bulkOpKind` itself a `@property` with `initial-value: 0` and
re-assigning it each tick: `--bulkOpKind: if(<INT 2F active>): <kind>; else: 0`.
So Chrome treats the value as live for one tick only, which matches the
CPU-instruction semantics (the INT completes in one tick, the fill
completes in that same tick, the instruction advances, the property is
cleared on the next tick).

### CSS-DOS side: memory-cell assignment

Every memory cell's assignment gains a new top-level clause:

```css
--mem_X: if(
  style(--bulkOpKind: 1)
    and calc(X >= var(--bulkDst)) = 1
    and calc(X < var(--bulkDst) + var(--bulkCount)) = 1:
    var(--bulkValue);

  style(--bulkOpKind: 2)
    and calc(X >= var(--bulkDst)) = 1
    and calc(X < var(--bulkDst) + var(--bulkCount)) = 1:
    /* memcpy: read source cell at matching offset */
    /* Chrome needs this as a function dispatch or per-cell var() */
    <indexed lookup>;

  style(--bulkOpKind: 3)
    and calc(X >= var(--bulkDst)) = 1
    and calc(X < var(--bulkDst) + var(--bulkCount)) = 1:
    /* masked: if source byte != transparent, write it */
    <conditional lookup>;

  /* fallthrough to normal per-tick memory write */
  style(--memAddr0: X): var(--memVal0);
  style(--memAddr1: X): var(--memVal1);
  ...
  else: keep;
);
```

Critical Chrome-side question: **can CSS express "var(--mem_<expr>)"
where `<expr>` is computed?** Current CSS has no dynamic var()
indirection. If Chrome has no way to read `memory[bulkSrc + (X - bulkDst)]`
in a `@function` body, then memcpy / masked / palette variants don't
work in Chrome. Only fill (which doesn't need source indirection) is
feasible. **This is the first question to resolve during implementation.**

Fallback if dynamic var() is not possible: emit memcpy/blit as a
sequence of normal single-byte broadcast writes (same as today — no
improvement over status quo), BUT for fills we still win. And fills
alone are enough to beat the splash-fill bottleneck. So **fill is
the MVP; generalise only if Chrome permits**.

### Calcite side: detector + ops

New op variants (beyond the existing `Op::MemoryFill` from commit
`d813801`):

```rust
Op::MemoryCopy {
    dst_slot: Slot,     // holds dst address
    src_slot: Slot,     // holds src address
    count_slot: Slot,
    exit_target: u32,
}

Op::MemoryBlitMasked {
    dst_slot: Slot,
    src_slot: Slot,
    count_slot: Slot,
    transparent_slot: Slot,
    exit_target: u32,
}
```

Detector lives at the CSS/assignment level (same layer as
`pattern/broadcast_write.rs`). It recognises the specific range-predicate
branch at the top of a memory-cell assignment — same structural shape
emitted by every memory cell (because it's CSS-DOS's same generator
output). One match collapses ALL memory cells into one op.

Execution: on each tick, check `--bulkOpKind`; if non-zero, execute
the appropriate `MemoryCopy` / `MemoryFill` / `MemoryBlitMasked` op
against `state.memory[dst..dst+count]` in native Rust. Normal
broadcast-write path runs afterwards (to zero out bulkOpKind — Chrome
does this too).

## Why this works (cardinal-rule check)

- **Chrome executes honest code.** Every memory cell's new assignment
  really does check `bulkOpKind != 0 && addr in range` per tick. Chrome
  does this in parallel for all ~1 M cells. For a single fill tick,
  Chrome performs ~1 M range checks, ~64 K of them writing the fill
  value, rest passing through. That's real Chrome work.
- **Calcite recognises the pattern, doesn't invent it.** It sees that
  every memory cell has the same range-predicate clause at the top,
  keyed on `--bulkOpKind` + `--bulkDst` + `--bulkCount`, with per-cell
  address constants that form a dense range. That's structurally
  detectable without x86 knowledge.
- **No side-channels.** Every property in the design is evaluated in
  Chrome and used in the result. `--bulkOpKind` IS a state variable;
  every cell consumes it.

## Rollout

Three PRs. Fill first (MVP, proves architecture), then memcpy, then
masked blit. Each gated on Chrome conformance + calcite correctness +
measured speedup on a representative workload.

### PR 1: Fill (MVP)

Cross-repo. Steps:

1. **CSS-DOS**: add `INT 2F/AH=FE/AL=0` handler (subset: fill only).
2. **CSS-DOS transpiler**: emit the 6 bulk properties + memory-cell
   range-predicate clause for the fill case.
3. **CSS-DOS**: rewrite `bios/splash.c` (or whichever path currently
   does REP STOSB for the splash clear) to call the new BIOS.
4. **Chrome verification**: run the new CSS in Chrome against a small
   fill target; verify memory contents match expected. Gate on
   `docs/reference/conformance-testing.md` procedures.
5. **Calcite**: add `MemoryFill` detector at the broadcast-write
   recognition layer. Detector inspects memory-cell assignments for the
   range-predicate clause; if every cell in a contiguous range has it,
   emit a single `Op::MemoryFill` binding.
6. **Calcite**: integration correctness gate — run `probe-splash-project`
   on the new bootle-ctest.css; memory hash must match the Chrome-run
   result (NOT the old baseline — baseline changes because the CSS
   changes).
7. **Bench**: report splash halt tick + ticks/s vs. pre-bulk-op.
   Expected: 10-100× improvement (fill collapses from ~64 000 ticks to
   ~1 tick in Chrome and ~1 µs in calcite).

### PR 2: memcpy

1. **Chrome capability check**: can CSS read `var(--mem_<computed>)` in
   an `@function` body? Document the result. If NO, skip PR 2 entirely
   and go to PR 3 with a warning (we'd need per-cell emission — same
   cost as today).
2. **CSS-DOS**: extend INT 2F/FE with `AL=1` (memcpy).
3. **CSS-DOS**: extend memory-cell range-predicate clause with the
   memcpy case, using whatever Chrome-legal indirection mechanism PR 2
   step 1 identified.
4. **Calcite**: add `Op::MemoryCopy` + detector.
5. **Test workload**: pick a DOS program that does visible memcpy
   (Mode 13h blit? zero-bank copy?) and bench.

### PR 3: masked blit

1. **CSS-DOS**: extend with `AL=2` (masked copy with transparent
   colour).
2. **CSS-DOS**: memory-cell clause adds mask test.
3. **Calcite**: `Op::MemoryBlitMasked` + detector.
4. **Test workload**: pick a sprite-heavy program (Wolfenstein-like,
   or a synthetic test).

## Risks / open questions

- **Chrome dynamic `var()` indirection**: blocker for PR 2+3 if
  unsupported. Resolve early in PR 2.
- **`@property` registration cost**: adding 6 new `@property`
  declarations is cheap. Every memory-cell assignment gains a
  range-predicate branch — that's ~1 M properties × one extra branch =
  measurable CSS growth. Exact growth to be measured; if unreasonable,
  consider gating behind a transpiler flag so non-bulk-op builds stay
  small.
- **Range predicate in CSS `if()`**: current CSS-DOS uses `style(--X: N)`
  equality only. Range tests are `calc(X >= var(--bulkDst))` — need to
  verify syntax works in both Chrome and calcite's parser.
- **Boot path breakage**: splash.c rewrite + new BIOS path could break
  DOS boot. Conformance testing is mandatory per CSS-DOS workflow.
- **Calcite's parser**: needs to accept the range-predicate syntax.
  Likely already does (it parses `calc()`), but verify.

## Non-goals

- Overlapping memcpy (memmove). Out of scope for PR 2; can add later.
- Bulk ops larger than 64 KiB (CX is 16-bit). Use multiple INT calls.
- CPU mode changes inside a bulk op (e.g., switching ES:DI between the
  handler and the tick). Not possible — INT 2F/FE is atomic from the
  C/DOS caller's perspective.

## Acceptance criteria

- **Correctness**: every PR's test workload produces the same memory
  contents at halt in Chrome and in calcite. Bit-exact memory hash.
- **Performance**: every PR shows measurable ticks/s improvement on at
  least one representative program.
- **Regressions**: `just check` stays green. Existing conformance tests
  (timer-irq, rep-stosb, bcd, keyboard-irq) keep passing.
