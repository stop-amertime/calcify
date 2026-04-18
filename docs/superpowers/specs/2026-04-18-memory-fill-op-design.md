# Op::MemoryFill — compile-time pattern lowering for REP STOSB

**Date:** 2026-04-18
**Status:** design

## Problem

Calcite currently evaluates REP STOSB one byte per tick. CSS-DOS emits a
tight byte-fill loop (`store mem[DI]=AL; DI++; CX--; branch-if-CX≠0`) that
the evaluator ticks through at ~200K ticks/s — a 64 KiB clear is still
tens of thousands of ticks. A single `memory.fill()` call would do the
same work in microseconds.

The prior commit (`eef36e3`) added a runtime period projector that gets
~1.3× on this workload by detecting the cycle empirically. That's a
fallback for patterns we can't recognise statically. REP STOSB has a
crisp, recognisable shape; a compile-time pattern match should collapse
an entire fill into one op — a much higher ceiling than runtime
observation can reach.

This spec describes the new `Op::MemoryFill` plus two independent
detectors (one bytecode peephole, one CSS-level) that both lower to it.
The two detectors are built side-by-side and benchmarked independently.

## Cardinal-rule check

Calcite must not have x86 knowledge. This design is safe because the
pattern we match is "a loop that writes a constant to an incrementing
address while a counter decrements to zero" — that's a generic CSS/
bytecode shape, not an x86 instruction. It happens to correspond to REP
STOSB today, but the detector doesn't know or care. Any CSS that
expresses the same arithmetic pattern gets the same optimisation.

Same principle as `BroadcastWrite` and `DispatchChain`: we recognise CSS
structure, not x86 opcodes.

## The op

```rust
/// Bulk-fill memory: state.memory[dst..dst+count] = value (all bytes).
/// After executing, advances dst_slot by count and zeros count_slot, so
/// the surrounding CSS state matches what N iterations of the recognised
/// loop body would have produced. Jumps to exit_target.
MemoryFill {
    dst_slot: Slot,      // holds the starting address; updated to dst+count
    val_slot: Slot,      // holds the fill byte (read once, masked to u8)
    count_slot: Slot,    // holds N; zeroed after the fill
    exit_target: u32,    // PC to jump to after the fill (the loop-exit branch)
}
```

Semantics in the evaluator:

```rust
Op::MemoryFill { dst_slot, val_slot, count_slot, exit_target } => {
    let dst = slots[*dst_slot] as i32;
    let val = (slots[*val_slot] & 0xFF) as u8;
    let count = slots[*count_slot] as i32;
    if count > 0 && dst >= 0 {
        let end = dst.saturating_add(count);
        // bounds-check against state.memory.len() — silently clip or skip
        // if out of range (same discipline as StoreMem, which no-ops on
        // oversized addresses).
        let lo = dst as usize;
        let hi = (end as usize).min(state.memory.len());
        if lo < state.memory.len() {
            state.memory[lo..hi].fill(val);
        }
    }
    slots[*dst_slot] = dst.wrapping_add(count) as i64;
    slots[*count_slot] = 0;
    pc = *exit_target;
    continue;
}
```

Correctness obligations:

- Post-state must match what running the loop N times would produce.
  Specifically: `dst_slot += count`, `count_slot = 0`, memory written.
- Any other slot the loop body touches (e.g., a flags/compare slot) must
  be written to its loop-exit value by the op or by the exit branch.
  Detection must verify that no other slots are carried out of the loop.
- The write must also go through `state.write_mem` instrumentation if
  `state.write_log` is `Some` — i.e., we need a bulk-path that either
  emits one synthetic `(dst, val)` per byte (expensive) or skips the log
  with a note (acceptable — the log is diagnostic, not correctness).
  Decision: **skip write-logging for bulk fills; document it.**

## Detector A — bytecode peephole

Runs as a post-compile pass over `CompiledProgram.ops`. Scans for the
fill-loop shape and rewrites the loop head as a single `Op::MemoryFill`.

**Target shape** (approximate — detector must tolerate op ordering within
the body, different fused variants, etc.):

```
LOOP:
  BranchIfNotEqLit { a: count_slot, val: 0, target: EXIT }  // or LoadStateAndBranchIfNotEqLit variant
  StoreMem { addr_slot: dst_slot, src: val_slot }            // constant val_slot — must be
                                                            //   loop-invariant
  AddLit { dst: dst_slot, a: dst_slot, val: +1 }
  SubLit { dst: count_slot, a: count_slot, val: -1 }
  Jump { target: LOOP }
EXIT:
```

Generalised criteria:

1. A basic block that ends in an unconditional jump back to its own head.
2. The head is a conditional branch that exits when some slot is zero.
3. The body contains exactly one memory write `StoreMem { addr_slot: D, src: V }`.
4. `D` is incremented by a constant stride (currently `+1`; future: any
   non-zero constant, doc note for now).
5. `V` is not written within the loop (loop-invariant).
6. The branch-exit slot is decremented by 1.
7. No other slots are mutated by the loop body.

Rewrite: replace the loop-head branch with `Op::MemoryFill { dst_slot: D, val_slot: V, count_slot: C, exit_target: EXIT }`, where `C` is the slot being decremented and tested against zero at the loop head. Replace
the rest of the loop body (store / inc / dec / jump) with `Jump { target: exit_target }` or dead-code-eliminate them — we'll start by
patching the head and leaving the body unreachable (simpler, safe).

**Edge cases to think about during implementation:**

- Fused `LoadStateAndBranchIfNotEqLit` at loop head (count_slot lives in
  memory, not a slot). Detector needs to accept both `LoadState+branch`
  and the fused form, treating the address as the "count slot" in an
  abstract sense. If count lives only in memory, we need `val_slot` /
  `dst_slot` resolution for memory-resident operands too — possibly
  requires extending the op to `MemoryFillMem` variant. Defer: start by
  only recognising the slot-resident form.
- `DispatchChain` landing pad: the loop entry may be reached via
  `DispatchChain`. The pattern matcher works on bytecode, so as long as
  the loop is a self-contained block, dispatch entry is fine.
- Fall-through: if the loop has `continue`-style early exits, reject.

## Detector B — CSS-level (`@function` / assignment pattern)

Runs at the same layer as `pattern/broadcast_write.rs` and
`pattern/dispatch_table.rs`. Inspects `ParsedProgram.assignments` and/or
`@function` definitions for the REP STOSB shape.

Details (to be filled in after reading the actual emitted CSS for
splash-fill — `output/bootle-ctest.css` is the reference fixture):

1. Identify the CSS pattern the REP STOSB rewrite emits. Likely an
   `@function` with a store-if-count-nonzero body, or a set of
   assignments with a specific conditional shape.
2. Recognise the dest-address slot, value slot, count slot by matching
   the CSS idioms (e.g., `var(--di)`, `var(--al)`, `var(--cx)`).
3. Emit `Op::MemoryFill` directly from the compiler when lowering the
   recognised pattern, bypassing normal bytecode generation for that
   block.

Cardinal-rule note: we match on CSS *structure* (address increment, byte
store, count decrement). We do not match on property names like `--di`,
`--al`, `--cx` — those are incidental to CSS-DOS's conventions. The
matcher uses structural position, not names.

**Detector B is implemented in a second PR after A ships.** Benchmarked
independently against A to decide whether to keep both, one, or neither.

## Detection infrastructure — shared

Both detectors need:

- Slot use/def analysis within a basic block (does slot X get written?
  does it get read?). We don't have this yet; build a small helper.
- A reliable way to identify "the loop body" — walk from a back-edge
  target up to the back-edge, collect all ops in between, reject if any
  op leaves the block except the back-edge and a single forward exit.

These live in a new `crates/calcite-core/src/pattern/memory_fill.rs`
module, reusable by both detectors.

## Testing

### Unit tests

- `peephole_recognises_simple_byte_fill` — craft a bytecode stream
  matching the canonical shape, run the peephole, assert the head op
  becomes `MemoryFill` and operands are correctly identified.
- `peephole_rejects_non_invariant_value` — if `val_slot` is written
  inside the loop, no rewrite.
- `peephole_rejects_non_unit_stride` — if address increments by 2, no
  rewrite (for the first iteration; future variants add word-stride).
- `peephole_rejects_extra_writes` — if another memory write exists in
  the body, no rewrite.
- `memory_fill_op_semantics` — evaluator test: set up slots + memory,
  execute `Op::MemoryFill`, verify memory range + slot post-state.
- `memory_fill_op_bounds` — out-of-range dst silently clipped, no panic.
- `memory_fill_op_zero_count` — no writes, slots unchanged.

### Integration tests

- Run `bootle-ctest.css` to halt-tick with and without the new op; tick
  counts will differ (the point), but **memory hash must match**
  baseline at halt. This is the critical correctness gate.
- Existing calcite-core tests must continue to pass — especially the
  conformance suite if any splash-fill workload is covered.

### Benchmark

- `calcite-bench` on splash-fill workload — compare ticks/s and wall
  time baseline vs peephole-enabled. Expectation: 10× or better on any
  workload dominated by fill loops. (Splash clears ~64 KiB; current
  cost is ~200K ticks at ~200K ticks/s = 1 second. After: one op,
  microseconds.)

## Non-goals

- We do not detect `memcpy`-style patterns (load from one address, store
  to another, both incrementing). Future work.
- We do not detect fills with non-unit stride (REP STOSW, REP STOSD).
  The address-stride generalisation can come later without changing the
  op signature — just accept any constant stride > 0 and multiply.
- We do not speculatively fire partial fills on the first iteration. The
  op assumes detection is safe: count > 0 ⇒ count full iterations.

## Rollout

1. **PR 1**: `Op::MemoryFill` added to the op enum, evaluator case added,
   unit tests for op semantics only. No detector yet. Merge.
2. **PR 2**: Peephole detector A + integration test + benchmark numbers.
   Merge if memory-hash matches baseline and benchmark shows a real win.
3. **PR 3**: CSS-level detector B, built alongside A (not replacing).
   Benchmarked and compared. Decide A-only / B-only / both based on
   data.

## Open questions (resolve during implementation)

- Exact fused-op variants the peephole must handle. Inspect compiled
  `bootle-ctest.css` bytecode before writing the matcher.
- Should we extend `Op::MemoryFill` to a `MemoryFillMem` variant
  immediately, or defer until we see the fused shape? Defer until data
  justifies it.
- `write_log` bulk-fill handling — confirm diagnostic-only status with
  grep of current consumers.
