# Op::MemoryFill Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Compile-time pattern match on byte-fill loops → single `Op::MemoryFill` that does `state.memory[dst..dst+count].fill(val)` in one evaluator step.

**Architecture:** Three PRs. PR 1 adds the op + evaluator support. PR 2 adds a bytecode peephole detector. PR 3 adds a CSS-level detector side-by-side. Spec: `docs/superpowers/specs/2026-04-18-memory-fill-op-design.md`.

**Tech Stack:** Rust, existing `calcite-core` compile/eval infra, `cargo test --workspace`, `cargo run --release --bin probe-splash-project` / `--bin calcite-bench`.

---

## Background engineers need to know

Calcite is a JIT-like evaluator for CSS. CSS text is parsed → `ParsedProgram` (assignments). The compiler lowers assignments to flat bytecode (`Vec<Op>`) operating on an i64 `slots` array and an `i32`-addressed `memory` buffer. Three op-dispatch loops exist in `crates/calcite-core/src/compile.rs`:

- `exec_ops` (~line 3700) — hot path, linear match on `Op`.
- execute_with_profile (~line 4047) — profiling wrapper, counts ops.
- execute_with_trace (~line 4404) — verbose tracing for the debugger.

Every op must be handled in all three plus in `compact_slots` (slot renumbering, ~line 3503) and `used_slots_for_op` (seeding, ~line 3573). The peephole passes (`fuse_cmp_branch`, `fuse_loadstate_branch`, `build_dispatch_chains`) live in the same file and run after assignment lowering.

**Cardinal rule:** calcite may not have x86 knowledge. The pattern matcher recognises *CSS/bytecode structure* (constant-value byte write at an incrementing address with a decrementing counter). The fact that this corresponds to REP STOSB is incidental.

**Correctness gate:** running `bootle-ctest.css` to halt must produce a **bit-identical memory hash** to the baseline (pre-this-plan) run. The probe `probe-splash-project` already implements the hash check end-to-end — use it.

**Slot/memory conventions (state.rs):**
- Slots are i64, but memory addresses are i32. `sload!(slot) as i32` is the idiom.
- `state.write_mem(addr: i32, val: i64)` masks to i8/i16 internally depending on address region. For bulk fill we bypass this and write raw u8 to `state.memory[lo..hi]`, matching what a byte-granular write loop would produce.
- `state.write_log: Option<Vec<(i32, i32)>>` is instrumentation used only by `cycle_tracker` probes. Bulk fill does NOT append to write_log (diagnostic-only; documented in spec).

---

# PR 1 — Op::MemoryFill + evaluator + unit tests (no detector)

### Task 1.1: Add `Op::MemoryFill` variant to the op enum

**Files:**
- Modify: `crates/calcite-core/src/compile.rs` (Op enum, ~line 43-295)

- [ ] **Step 1: Add the variant after the `StoreMem` entry**

In `pub enum Op`, after the existing `StoreMem { addr_slot: Slot, src: Slot }` variant (~line 291-294), add:

```rust
    /// Bulk byte-fill: state.memory[slots[dst_slot]..slots[dst_slot]+slots[count_slot]] = slots[val_slot] & 0xFF.
    /// After the fill: slots[dst_slot] += slots[count_slot]; slots[count_slot] = 0; pc = exit_target.
    /// Emitted by the memory-fill peephole / CSS-level pattern in place of a
    /// recognised byte-fill loop. Skip write_log: bulk path is correctness-
    /// equivalent to iterating the loop, but does not emit per-byte log entries.
    MemoryFill {
        dst_slot: Slot,
        val_slot: Slot,
        count_slot: Slot,
        exit_target: u32,
    },
```

- [ ] **Step 2: Check it compiles (it won't — match exhaustiveness errors expected)**

Run: `cargo build -p calcite-core 2>&1 | head -60`

Expected: compile errors in `compact_slots` (line ~3503), `used_slots_for_op` (~3573), `exec_ops` (~3700), `execute_with_profile` (~4047), `execute_with_trace` (~4404). Each complains about non-exhaustive match on `Op`.

- [ ] **Step 3: Commit the enum addition (not yet buildable — that's fine for an interim commit gating the next fix-up commit)**

Skip the commit for now and chain into Task 1.2 which will restore buildability. We commit once all three execute paths and the two helpers are handled.

### Task 1.2: Handle `MemoryFill` in `compact_slots`

**Files:**
- Modify: `crates/calcite-core/src/compile.rs` (~line 3358, 3403, 3503)

- [ ] **Step 1: Add to the "read slots" match** (used by the compactor; search for the existing `Op::StoreMem { addr_slot, src } => vec![*addr_slot, *src],` arm)

```rust
        Op::MemoryFill { dst_slot, val_slot, count_slot, .. } => vec![*dst_slot, *val_slot, *count_slot],
```

Insert immediately after the `StoreMem` arm.

- [ ] **Step 2: Add to the "writes pc/targets" match** (the one around line 3403 that lists branch-y ops). Targets are not slots; add `MemoryFill` alongside `Jump`/`BranchIfZero`/`BranchIfNotEqLit`/`DispatchChain`/`StoreState`/`StoreMem`:

```rust
        Op::BranchIfZero { .. } | Op::BranchIfNotEqLit { .. } | Op::Jump { .. } | Op::DispatchChain { .. } | Op::StoreState { .. } | Op::StoreMem { .. } | Op::MemoryFill { .. } => {
```

(Append `| Op::MemoryFill { .. }` to the existing arm.)

- [ ] **Step 3: Add to the `remap_op` match around line 3503** (which rewrites slot indices). Search for the existing `Op::StoreMem { addr_slot, src } => { *addr_slot = alloc.get_or_alloc(*addr_slot, slot_map); *src = alloc.get_or_alloc(*src, slot_map); }` arm and add directly after it:

```rust
        Op::MemoryFill { dst_slot, val_slot, count_slot, .. } => {
            *dst_slot = alloc.get_or_alloc(*dst_slot, slot_map);
            *val_slot = alloc.get_or_alloc(*val_slot, slot_map);
            *count_slot = alloc.get_or_alloc(*count_slot, slot_map);
        }
```

### Task 1.3: Handle `MemoryFill` in `used_slots_for_op`

**Files:**
- Modify: `crates/calcite-core/src/compile.rs` (~line 3573)

- [ ] **Step 1: Add to the seed match** (search for `Op::StoreMem { addr_slot, src } => { seed(*addr_slot); seed(*src); }`). Add directly after:

```rust
        Op::MemoryFill { dst_slot, val_slot, count_slot, .. } => {
            seed(*dst_slot); seed(*val_slot); seed(*count_slot);
        }
```

### Task 1.4: Handle `MemoryFill` in `exec_ops` (hot path)

**Files:**
- Modify: `crates/calcite-core/src/compile.rs` (~line 3963 — after `Op::StoreMem`)

- [ ] **Step 1: Add the execution arm**

After the `Op::StoreMem { addr_slot, src } => { state.write_mem(sload!(*addr_slot), sload!(*src)); }` arm (~line 3963), insert:

```rust
            Op::MemoryFill { dst_slot, val_slot, count_slot, exit_target } => {
                let dst_i64 = sload!(*dst_slot);
                let count_i64 = sload!(*count_slot);
                let val_byte = (sload!(*val_slot) & 0xFF) as u8;
                let mem_len = state.memory.len() as i64;
                if count_i64 > 0 && dst_i64 >= 0 && dst_i64 < mem_len {
                    let lo = dst_i64 as usize;
                    let end = dst_i64.saturating_add(count_i64).min(mem_len);
                    let hi = end as usize;
                    if hi > lo {
                        state.memory[lo..hi].fill(val_byte);
                    }
                }
                sstore!(*dst_slot, dst_i64.wrapping_add(count_i64));
                sstore!(*count_slot, 0);
                pc = *exit_target as usize;
                continue;
            }
```

**Note:** `sload!` returns i64 in this evaluator; `sstore!` takes i32 or i64 — check existing uses near this location. If `sstore!` requires i32, cast: `sstore!(*count_slot, 0i32);` and for the dst advance, ensure the result fits — it should, since addresses are i32-sized. If the macro signature complains, match the pattern of existing stores like `sstore!(*dst, ((!av) & 0xFFFF) as i32);` at line 3900.

### Task 1.5: Handle `MemoryFill` in `execute_with_profile`

**Files:**
- Modify: `crates/calcite-core/src/compile.rs` (~line 4365 — after `Op::StoreMem` profile arm)

- [ ] **Step 1: Add the profile arm**

After the `Op::StoreMem { addr_slot, src } => { count_op!(profile, "StoreMem"); state.write_mem(slots[*addr_slot as usize], slots[*src as usize]); }` arm (~line 4365), insert:

```rust
            Op::MemoryFill { dst_slot, val_slot, count_slot, exit_target } => {
                count_op!(profile, "MemoryFill");
                let dst_i64 = slots[*dst_slot as usize];
                let count_i64 = slots[*count_slot as usize];
                let val_byte = (slots[*val_slot as usize] & 0xFF) as u8;
                let mem_len = state.memory.len() as i64;
                if count_i64 > 0 && dst_i64 >= 0 && dst_i64 < mem_len {
                    let lo = dst_i64 as usize;
                    let end = dst_i64.saturating_add(count_i64).min(mem_len);
                    let hi = end as usize;
                    if hi > lo {
                        state.memory[lo..hi].fill(val_byte);
                    }
                }
                slots[*dst_slot as usize] = dst_i64.wrapping_add(count_i64);
                slots[*count_slot as usize] = 0;
                pc = *exit_target as usize;
                continue;
            }
```

### Task 1.6: Handle `MemoryFill` in `execute_with_trace`

**Files:**
- Modify: `crates/calcite-core/src/compile.rs` (~line 4821 — after `Op::StoreMem` trace arm)

- [ ] **Step 1: Add the trace arm**

After the `StoreMem` trace arm that starts at ~line 4821, insert:

```rust
            Op::MemoryFill { dst_slot, val_slot, count_slot, exit_target } => {
                let dst = slots[*dst_slot as usize];
                let count = slots[*count_slot as usize];
                let val = slots[*val_slot as usize];
                let val_byte = (val & 0xFF) as u8;
                let mem_len = state.memory.len() as i64;
                if count > 0 && dst >= 0 && dst < mem_len {
                    let lo = dst as usize;
                    let end = dst.saturating_add(count).min(mem_len);
                    let hi = end as usize;
                    if hi > lo {
                        state.memory[lo..hi].fill(val_byte);
                    }
                }
                slots[*dst_slot as usize] = dst.wrapping_add(count);
                slots[*count_slot as usize] = 0;
                if should_trace {
                    trace.push(TraceEntry {
                        pc,
                        op: format!("MemoryFill dst={}({}) val={}({:#x}) count={}({}) -> exit_target={}",
                            dst_slot, dst, val_slot, val_byte, count_slot, count, exit_target),
                        dst_slot: Some(*dst_slot),
                        dst_value: Some(dst.wrapping_add(count) as i32),
                        inputs: vec![(*dst_slot, dst as i32), (*val_slot, val as i32), (*count_slot, count as i32)],
                        branch_taken: None,
                        depth,
                    });
                }
                pc = *exit_target as usize;
                continue;
            }
```

**Note:** `TraceEntry` shape may differ slightly — check the surrounding StoreMem trace block and match its exact fields. If `dst_value` expects i32, `dst_value: Some(dst.wrapping_add(count) as i32)` is correct; if i64, drop the cast.

### Task 1.7: Verify the crate builds

- [ ] **Step 1: Build**

Run: `cargo build -p calcite-core`
Expected: success, no warnings.

- [ ] **Step 2: Run all existing tests (nothing should regress)**

Run: `cargo test -p calcite-core`
Expected: all pass.

### Task 1.8: Write evaluator unit test for op semantics

**Files:**
- Create: `crates/calcite-core/src/memory_fill_tests.rs` — new test-only module, declared from `lib.rs` under `#[cfg(test)]`.

Actually, use an existing test-host pattern: put tests next to related code. Search for other inline `#[cfg(test)] mod tests` blocks in `compile.rs`; if none, create a new test file:

- Create: `crates/calcite-core/tests/memory_fill.rs` — top-level integration-style test, since `compile.rs`'s internals can be exercised via the public `Op`, `CompiledProgram`, `Evaluator::execute`.

- [ ] **Step 1: Write the failing test**

Create `crates/calcite-core/tests/memory_fill.rs`:

```rust
//! Evaluator unit tests for Op::MemoryFill.
//!
//! We bypass parse/compile and construct a CompiledProgram by hand so the
//! test exercises only the evaluator arm.

use calcite_core::compile::{CompiledProgram, Op};
use calcite_core::State;

fn mk_program(ops: Vec<Op>, slot_count: u32) -> CompiledProgram {
    CompiledProgram {
        ops,
        slot_count,
        writeback: vec![],
        broadcast_writes: vec![],
        dispatch_tables: vec![],
        chain_tables: vec![],
        flat_dispatch_arrays: vec![],
        property_slots: Default::default(),
    }
}

fn fresh_state() -> State {
    let mut s = State::default();
    // Ensure memory is at least 1 MiB — big enough for the test fills.
    if s.memory.len() < 1_048_576 {
        s.memory.resize(1_048_576, 0);
    }
    s
}

#[test]
fn memory_fill_basic_fills_range_and_updates_slots() {
    // slots: 0=dst (0xA0000), 1=val (0x42), 2=count (128)
    // Op::MemoryFill followed by a no-op at pc=1 as the exit_target.
    let ops = vec![
        Op::MemoryFill { dst_slot: 0, val_slot: 1, count_slot: 2, exit_target: 2 },
        Op::LoadLit { dst: 3, val: 999 },   // should NOT run (we skip to exit_target=2)
        Op::LoadLit { dst: 3, val: 7 },     // pc=2; runs once then falls off the end
    ];
    let program = mk_program(ops, 4);
    let mut state = fresh_state();

    let mut slots: Vec<i64> = vec![0xA0000, 0x42, 128, 0];
    // Call whichever internal helper the eval path needs. Use the public Evaluator API:
    let mut eval = calcite_core::Evaluator::from_compiled(&program); // use whatever constructor exists — see note below

    eval.execute_with_slots(&mut state, &mut slots);

    // Memory at 0xA0000..0xA0080 must be 0x42; before and after must be 0.
    assert!(state.memory[0xA0000 - 1] == 0, "byte before fill should be untouched");
    for i in 0..128 {
        assert_eq!(state.memory[0xA0000 + i], 0x42, "byte {} in fill region", i);
    }
    assert_eq!(state.memory[0xA0000 + 128], 0, "byte immediately after fill untouched");

    // dst advanced by count, count zeroed.
    assert_eq!(slots[0], 0xA0000 + 128);
    assert_eq!(slots[2], 0);
    // We jumped to pc=2, skipping the LoadLit at pc=1.
    assert_eq!(slots[3], 7, "should have executed the op at exit_target=2, not pc=1");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p calcite-core --test memory_fill -- --nocapture`

Expected: **compile failure** — `Evaluator::from_compiled` and/or `execute_with_slots` likely do not exist. The test is a sketch — find the actual API the existing tests/probes use.

- [ ] **Step 3: Replace the Evaluator calls with the real API**

Look at `crates/calcite-cli/src/bin/probe_splash_project.rs` (which the summary shows uses `Evaluator::from_parsed` and `evaluator.tick(&mut state)`) and at `crates/calcite-core/src/eval.rs` to find the real public API for running a raw `CompiledProgram`.

If the only public entry is via a parsed CSS program, adjust the test: instead of `mk_program` directly, write a tiny CSS harness that emits exactly the bytecode you want via a synthetic `CompiledProgram` field poke, or expose a `pub fn execute_ops_for_test(...)` behind `#[cfg(test)]` in `compile.rs`. Prefer the cfg(test) helper — less invasive.

- [ ] **Step 4: If a test helper is needed, add it**

In `compile.rs`, near `exec_ops`, add:

```rust
#[cfg(test)]
pub fn exec_ops_for_test(
    ops: &[Op],
    state: &mut crate::State,
    slots: &mut [i64],
) {
    // Build empty ancillary vectors; memory_fill doesn't need them.
    let dispatch_tables: Vec<CompiledDispatchTable> = Vec::new();
    let chain_tables: Vec<DispatchChainTable> = Vec::new();
    let flat_dispatch_arrays: Vec<FlatDispatchArray> = Vec::new();
    exec_ops(ops, &dispatch_tables, &chain_tables, &flat_dispatch_arrays, state, slots);
}
```

Then in the test, call `calcite_core::compile::exec_ops_for_test(&program.ops, &mut state, &mut slots);` directly — bypassing the Evaluator.

- [ ] **Step 5: Run the test**

Run: `cargo test -p calcite-core --test memory_fill memory_fill_basic_fills_range_and_updates_slots`
Expected: PASS.

### Task 1.9: Add additional evaluator unit tests (edge cases)

**Files:**
- Modify: `crates/calcite-core/tests/memory_fill.rs`

- [ ] **Step 1: Zero-count test**

```rust
#[test]
fn memory_fill_zero_count_no_writes() {
    let ops = vec![
        Op::MemoryFill { dst_slot: 0, val_slot: 1, count_slot: 2, exit_target: 1 },
        Op::LoadLit { dst: 3, val: 1 },
    ];
    let program = mk_program(ops, 4);
    let mut state = fresh_state();
    let baseline = state.memory.clone();
    let mut slots: Vec<i64> = vec![0x1000, 0xFF, 0, 0];
    calcite_core::compile::exec_ops_for_test(&program.ops, &mut state, &mut slots);
    assert_eq!(state.memory, baseline, "zero count must not write any byte");
    assert_eq!(slots[0], 0x1000, "dst unchanged when count == 0");
    assert_eq!(slots[2], 0);
    assert_eq!(slots[3], 1, "exit_target reached");
}
```

- [ ] **Step 2: Out-of-bounds test**

```rust
#[test]
fn memory_fill_out_of_bounds_clips() {
    let ops = vec![
        Op::MemoryFill { dst_slot: 0, val_slot: 1, count_slot: 2, exit_target: 1 },
        Op::LoadLit { dst: 3, val: 1 },
    ];
    let program = mk_program(ops, 4);
    let mut state = fresh_state();
    let mem_len = state.memory.len() as i64;
    // Write 100 bytes starting 50 bytes before the end.
    let start = mem_len - 50;
    let mut slots: Vec<i64> = vec![start, 0x33, 100, 0];
    calcite_core::compile::exec_ops_for_test(&program.ops, &mut state, &mut slots);
    // The last 50 bytes should be 0x33; no panic.
    for i in 0..50 {
        assert_eq!(state.memory[(start + i) as usize], 0x33);
    }
    // dst still advances by the full count (matches what the loop body
    // would have done, even though some writes landed out of bounds).
    assert_eq!(slots[0], start + 100);
    assert_eq!(slots[2], 0);
}
```

- [ ] **Step 3: Negative dst test — no writes, slots still updated**

```rust
#[test]
fn memory_fill_negative_dst_noop_memory() {
    let ops = vec![Op::MemoryFill { dst_slot: 0, val_slot: 1, count_slot: 2, exit_target: 1 }];
    let program = mk_program(ops.clone(), 3);
    let mut state = fresh_state();
    let baseline = state.memory.clone();
    let mut slots: Vec<i64> = vec![-4, 0x77, 16, 0];
    calcite_core::compile::exec_ops_for_test(&program.ops, &mut state, &mut slots);
    assert_eq!(state.memory, baseline, "negative dst must not write");
    assert_eq!(slots[0], -4 + 16);
    assert_eq!(slots[2], 0);
}
```

- [ ] **Step 4: Value masked to byte**

```rust
#[test]
fn memory_fill_value_is_masked_to_byte() {
    let ops = vec![Op::MemoryFill { dst_slot: 0, val_slot: 1, count_slot: 2, exit_target: 1 }];
    let program = mk_program(ops, 3);
    let mut state = fresh_state();
    let mut slots: Vec<i64> = vec![0x2000, 0x12345678, 4, 0];
    calcite_core::compile::exec_ops_for_test(&program.ops, &mut state, &mut slots);
    for i in 0..4 {
        assert_eq!(state.memory[0x2000 + i], 0x78, "low byte only");
    }
}
```

- [ ] **Step 5: Run all memory_fill tests**

Run: `cargo test -p calcite-core --test memory_fill`
Expected: 4 tests pass.

### Task 1.10: Run full workspace tests

- [ ] **Step 1:**

Run: `cargo test --workspace`
Expected: all pass, no regressions.

### Task 1.11: Commit PR 1

- [ ] **Step 1: Stage**

```bash
git add crates/calcite-core/src/compile.rs crates/calcite-core/tests/memory_fill.rs
```

- [ ] **Step 2: Commit**

```bash
git commit -m "$(cat <<'EOF'
feat(core): Op::MemoryFill for bulk byte-fill lowering

Adds Op::MemoryFill { dst_slot, val_slot, count_slot, exit_target } and
its handling in exec_ops, execute_with_profile, execute_with_trace,
compact_slots, and used_slots_for_op. The op performs
state.memory[dst..dst+count].fill(val & 0xFF) in one evaluator step,
advances dst_slot by count, zeros count_slot, and jumps to exit_target.

No detector yet — the op is unreachable from compiled programs until
PR 2 adds the bytecode peephole matcher. Bulk path skips write_log
instrumentation (diagnostic-only, documented in spec).

Spec: docs/superpowers/specs/2026-04-18-memory-fill-op-design.md

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

# PR 2 — Bytecode peephole detector (Detector A)

### Task 2.1: Scaffold the detection module

**Files:**
- Create: `crates/calcite-core/src/pattern/memory_fill.rs`
- Modify: `crates/calcite-core/src/pattern/mod.rs` (or wherever the `pattern` module is declared)

- [ ] **Step 1: Find where `pattern` is declared**

Run: `grep -rn "pub mod pattern\|mod pattern" crates/calcite-core/src/`
The spec mentions `pattern/broadcast_write.rs` exists — find its sibling declaration point.

- [ ] **Step 2: Create the module file with doc header only**

```rust
//! Peephole detector: tight byte-fill loops → `Op::MemoryFill`.
//!
//! Matches a basic block of the form:
//!
//! ```text
//! LOOP:
//!   branch-if-count-zero -> EXIT       (head)
//!   StoreMem { addr_slot: D, src: V }  (one memory write)
//!   AddLit { dst: D, a: D, val: +1 }   (address increment, unit stride)
//!   SubLit { dst: C, a: C, val: 1 }    (counter decrement)
//!   Jump { target: LOOP }              (back edge)
//! EXIT:
//! ```
//!
//! Where V is loop-invariant (not written inside the body) and no other
//! slot is mutated. The head branch is replaced with
//! `Op::MemoryFill { dst_slot: D, val_slot: V, count_slot: C, exit_target: EXIT }`.
//! The body ops (store/inc/dec/jump) are left in place but become
//! unreachable, then cleaned up by later compaction passes.
//!
//! Cardinal-rule note: the detector reasons about bytecode shape only —
//! no x86 knowledge. The match is structural (constant-value byte store
//! at incrementing address with decrementing counter).

use crate::compile::Op;

/// Scan `ops` for byte-fill loops and rewrite the loop-head branch in-place.
/// Returns the number of fills detected.
pub fn recognise_byte_fills(ops: &mut Vec<Op>) -> usize {
    // Placeholder — implemented in Task 2.3.
    let _ = ops;
    0
}
```

- [ ] **Step 3: Register the module**

In the file that declares the `pattern` module (e.g. `crates/calcite-core/src/lib.rs` or a `pattern/mod.rs`), add:

```rust
pub mod memory_fill;
```

(Place next to the existing `pub mod broadcast_write;` / `pub mod dispatch_table;` line.)

- [ ] **Step 4: Verify it builds**

Run: `cargo build -p calcite-core`
Expected: success.

### Task 2.2: Write the first failing test (synthetic byte-fill loop → MemoryFill)

**Files:**
- Modify: `crates/calcite-core/src/pattern/memory_fill.rs` (add `#[cfg(test)] mod tests` at the bottom)

- [ ] **Step 1: Append the test module**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::Op;

    // Canonical shape:
    //   pc 0: BranchIfNotEqLit { a: count=2, val: 0, target: 5 }
    //   pc 1: StoreMem { addr_slot: 0, src: 1 }          // mem[dst] = val
    //   pc 2: AddLit  { dst: 0, a: 0, val: 1 }           // dst += 1
    //   pc 3: SubLit  { dst: 2, a: 2, val: 1 }           // count -= 1
    //   pc 4: Jump    { target: 0 }
    //   pc 5: (exit)
    //
    // After detection:
    //   pc 0: MemoryFill { dst_slot: 0, val_slot: 1, count_slot: 2, exit_target: 5 }
    //   pc 1..4: unchanged but unreachable
    //   pc 5: (exit)
    #[test]
    fn recognises_canonical_byte_fill_loop() {
        let mut ops = vec![
            Op::BranchIfNotEqLit { a: 2, val: 0, target: 5 },
            Op::StoreMem { addr_slot: 0, src: 1 },
            Op::AddLit { dst: 0, a: 0, val: 1 },
            Op::SubLit { dst: 2, a: 2, val: 1 },
            Op::Jump { target: 0 },
            Op::LoadLit { dst: 3, val: 999 }, // exit
        ];
        let found = recognise_byte_fills(&mut ops);
        assert_eq!(found, 1);
        match &ops[0] {
            Op::MemoryFill { dst_slot, val_slot, count_slot, exit_target } => {
                assert_eq!(*dst_slot, 0);
                assert_eq!(*val_slot, 1);
                assert_eq!(*count_slot, 2);
                assert_eq!(*exit_target, 5);
            }
            other => panic!("expected MemoryFill, got {:?}", other),
        }
    }
}
```

- [ ] **Step 2: Run test, confirm it fails**

Run: `cargo test -p calcite-core pattern::memory_fill::tests::recognises_canonical_byte_fill_loop`
Expected: FAIL — `found` is 0 because `recognise_byte_fills` returns 0.

### Task 2.3: Implement the detector

**Files:**
- Modify: `crates/calcite-core/src/pattern/memory_fill.rs`

- [ ] **Step 1: Replace the placeholder with a real matcher**

```rust
pub fn recognise_byte_fills(ops: &mut Vec<Op>) -> usize {
    let mut found = 0;
    let n = ops.len();
    if n < 5 {
        return 0;
    }
    // Scan every window of 5 ops for the canonical shape, anchored at a
    // BranchIfNotEqLit that skips to EXIT and a Jump at the end of the
    // window that targets the branch itself.
    let mut i = 0;
    while i + 5 <= n {
        if let Some((dst_slot, val_slot, count_slot, exit_target)) = match_byte_fill(&ops[i..i + 5], i as u32) {
            ops[i] = Op::MemoryFill { dst_slot, val_slot, count_slot, exit_target };
            // Leave pc i+1..i+4 as-is (unreachable; cleaned up later if ever).
            // Advance past the whole window.
            i += 5;
            found += 1;
        } else {
            i += 1;
        }
    }
    found
}

fn match_byte_fill(window: &[Op], head_pc: u32) -> Option<(u32, u32, u32, u32)> {
    // window[0] = head branch
    // window[1] = StoreMem { addr_slot: D, src: V }
    // window[2] = AddLit { dst: D, a: D, val: 1 }
    // window[3] = SubLit { dst: C, a: C, val: 1 }
    // window[4] = Jump { target: head_pc }
    let (count_slot, exit_target) = match &window[0] {
        Op::BranchIfNotEqLit { a, val: 0, target } => (*a, *target),
        _ => return None,
    };
    let (d, v) = match &window[1] {
        Op::StoreMem { addr_slot, src } => (*addr_slot, *src),
        _ => return None,
    };
    match &window[2] {
        Op::AddLit { dst, a, val: 1 } if *dst == d && *a == d => {}
        _ => return None,
    }
    match &window[3] {
        Op::SubLit { dst, a, val: 1 } if *dst == count_slot && *a == count_slot => {}
        _ => return None,
    }
    match &window[4] {
        Op::Jump { target } if *target == head_pc => {}
        _ => return None,
    }
    // Loop-invariant value: V must not be d or count_slot (the two slots
    // we know get mutated in the body). The only other body mutation is
    // StoreMem, which writes memory, not a slot, so this check is
    // sufficient.
    if v == d || v == count_slot {
        return None;
    }
    Some((d, v, count_slot, exit_target))
}
```

- [ ] **Step 2: Run test**

Run: `cargo test -p calcite-core pattern::memory_fill::tests::recognises_canonical_byte_fill_loop`
Expected: PASS.

### Task 2.4: Add rejection tests

**Files:**
- Modify: `crates/calcite-core/src/pattern/memory_fill.rs` (`tests` module)

- [ ] **Step 1: Reject when value is written inside the body**

```rust
#[test]
fn rejects_non_invariant_value() {
    // Same as canonical but val_slot (1) is overwritten in the body.
    let mut ops = vec![
        Op::BranchIfNotEqLit { a: 2, val: 0, target: 6 },
        Op::StoreMem { addr_slot: 0, src: 1 },
        Op::AddLit { dst: 0, a: 0, val: 1 },
        Op::SubLit { dst: 2, a: 2, val: 1 },
        Op::LoadLit { dst: 1, val: 42 },  // breaks invariance
        Op::Jump { target: 0 },
        Op::LoadLit { dst: 3, val: 999 },
    ];
    let found = recognise_byte_fills(&mut ops);
    assert_eq!(found, 0, "loop body mutates val_slot; should NOT match");
}
```

- [ ] **Step 2: Reject when the Jump does not close the loop**

```rust
#[test]
fn rejects_non_looping_jump() {
    let mut ops = vec![
        Op::BranchIfNotEqLit { a: 2, val: 0, target: 5 },
        Op::StoreMem { addr_slot: 0, src: 1 },
        Op::AddLit { dst: 0, a: 0, val: 1 },
        Op::SubLit { dst: 2, a: 2, val: 1 },
        Op::Jump { target: 99 },   // not pointing back to head
        Op::LoadLit { dst: 3, val: 999 },
    ];
    let found = recognise_byte_fills(&mut ops);
    assert_eq!(found, 0);
}
```

- [ ] **Step 3: Reject non-unit stride**

```rust
#[test]
fn rejects_non_unit_address_stride() {
    let mut ops = vec![
        Op::BranchIfNotEqLit { a: 2, val: 0, target: 5 },
        Op::StoreMem { addr_slot: 0, src: 1 },
        Op::AddLit { dst: 0, a: 0, val: 2 },  // stride 2, not 1
        Op::SubLit { dst: 2, a: 2, val: 1 },
        Op::Jump { target: 0 },
        Op::LoadLit { dst: 3, val: 999 },
    ];
    let found = recognise_byte_fills(&mut ops);
    assert_eq!(found, 0);
}
```

- [ ] **Step 4: Reject non-zero BranchIfNotEqLit threshold**

```rust
#[test]
fn rejects_non_zero_branch_threshold() {
    let mut ops = vec![
        Op::BranchIfNotEqLit { a: 2, val: 7, target: 5 },  // not !=0
        Op::StoreMem { addr_slot: 0, src: 1 },
        Op::AddLit { dst: 0, a: 0, val: 1 },
        Op::SubLit { dst: 2, a: 2, val: 1 },
        Op::Jump { target: 0 },
        Op::LoadLit { dst: 3, val: 999 },
    ];
    let found = recognise_byte_fills(&mut ops);
    assert_eq!(found, 0);
}
```

- [ ] **Step 5: Run all tests**

Run: `cargo test -p calcite-core pattern::memory_fill`
Expected: 5 tests pass.

### Task 2.5: Wire detector into the compile pipeline

**Files:**
- Modify: `crates/calcite-core/src/compile.rs` (~line 2365, after `fuse_loadstate_branch`)

- [ ] **Step 1: Add the call at the right point**

After `fuse_loadstate_branch` (~line 2366-2367), BEFORE `compact_slots`:

```rust
    let _ct = web_time::Instant::now();
    let fills = recognise_memory_fills(&mut program);
    log::info!("[compile detail] memory fills: {} detected, {:.2}s", fills, _ct.elapsed().as_secs_f64());
```

- [ ] **Step 2: Add the driver function**

Elsewhere in `compile.rs` (near the other pattern drivers — `fuse_loadstate_branch`, `fuse_cmp_branch`), add:

```rust
/// Run the byte-fill peephole detector over the main ops vector and over
/// every dispatch entry / broadcast body. Each of these contains its own
/// flat op stream that may contain an embedded fill loop.
fn recognise_memory_fills(program: &mut CompiledProgram) -> usize {
    let mut total = 0;
    total += crate::pattern::memory_fill::recognise_byte_fills(&mut program.ops);
    for table in &mut program.dispatch_tables {
        for (_k, (entry_ops, _s)) in &mut table.entries {
            total += crate::pattern::memory_fill::recognise_byte_fills(entry_ops);
        }
        total += crate::pattern::memory_fill::recognise_byte_fills(&mut table.fallback_ops);
    }
    for bw in &mut program.broadcast_writes {
        total += crate::pattern::memory_fill::recognise_byte_fills(&mut bw.value_ops);
        if let Some(ref mut sp) = bw.spillover {
            for (_k, (spill_ops, _s)) in &mut sp.entries {
                total += crate::pattern::memory_fill::recognise_byte_fills(spill_ops);
            }
        }
    }
    total
}
```

Match the pattern of `fuse_loadstate_branch` (line 2528) for scope coverage.

- [ ] **Step 3: Import / resolve the module path**

Ensure `use crate::pattern::memory_fill;` is at the top of `compile.rs` or use the fully qualified `crate::pattern::memory_fill::recognise_byte_fills` as written above. Either is fine.

- [ ] **Step 4: Build**

Run: `cargo build -p calcite-core`
Expected: success.

- [ ] **Step 5: Run all tests**

Run: `cargo test --workspace`
Expected: all pass.

### Task 2.6: End-to-end correctness check on `bootle-ctest.css`

**Files:**
- (no file changes)

- [ ] **Step 1: Baseline halt tick + memory hash (from PR 1, pre-detector)**

The probe at `crates/calcite-cli/src/bin/probe_splash_project.rs` already runs a baseline. We want to compare baseline (evaluator without detector-touched loops) against the compiled-with-detector run.

Easiest: use the bench binary at `calcite-bench` with the `--halt` flag and read the halt-tick / final memory hash. Run:

```bash
cargo run --release --bin calcite-bench -- -i ../../../output/bootle-ctest.css --halt 719359 --max-ticks 3000000
```

Expected output: reports halt tick and some summary. Memory hash may not be printed — if not, add a quick `--dump-mem-hash` or temporarily patch the bench binary to print `fnv1a(&state.memory)` at halt. Record the halt tick.

- [ ] **Step 2: Verify the halt tick is LOWER with the detector wired in**

The same command after Task 2.5 should halt in dramatically fewer ticks if the detector fires during splash fill (bulk-fill collapses thousands of ticks into one). If halt tick is identical to baseline, the detector is not matching — investigate. If halt tick is lower AND memory hash matches baseline, we win.

- [ ] **Step 3: Use probe_splash_project as the authoritative correctness gate**

```bash
cargo run --release --bin probe-splash-project -- --max-ticks 3000000
```

Expected: `mem: baseline XXXX vs projected XXXX (match)`. The probe runs baseline + projection independently and hashes both. Since the compiler runs the detector for both runs (it's post-compile), both runs should see the same lowering and halt at the same tick, with matching memory.

**If memory hashes differ**, the detector produced incorrect code. Revert Task 2.5 and investigate in the unit tests before re-enabling.

### Task 2.7: Benchmark

**Files:**
- (no file changes)

- [ ] **Step 1: Run the splash-fill bench**

```bash
./bench-splash.bat    # or the equivalent: cargo run --release --bin calcite-bench -- -i output/bootle-ctest.css -n 2000 --warmup 500
```

- [ ] **Step 2: Record numbers in `docs/log.md`**

Append a short entry dated 2026-04-18 with before/after ticks/s on splash-fill. Expected: order-of-magnitude improvement on any fill-dominated workload.

### Task 2.8: Commit PR 2

- [ ] **Step 1: Stage**

```bash
git add crates/calcite-core/src/pattern/memory_fill.rs crates/calcite-core/src/compile.rs crates/calcite-core/src/lib.rs docs/log.md
```

(Adjust the `src/lib.rs` / `pattern/mod.rs` path to whichever file declares the module.)

- [ ] **Step 2: Commit**

```bash
git commit -m "$(cat <<'EOF'
perf(core): bytecode peephole detector for Op::MemoryFill

Recognises the canonical byte-fill loop
  branch-if-count-zero; StoreMem; AddLit +1; SubLit 1; Jump-back
and rewrites the head branch as Op::MemoryFill, collapsing an entire
REP STOSB-style fill into one evaluator step.

Runs post-fuse_loadstate_branch, pre-compact_slots, over the main ops
vector, every dispatch-table entry, and every broadcast-write body —
the same coverage pattern fuse_loadstate_branch uses.

Cardinal-rule-safe: matches bytecode shape only (constant-value byte
store at incrementing address with decrementing counter). No x86
knowledge.

Correctness verified by probe-splash-project (memory hash matches
baseline on bootle-ctest.css). Benchmark numbers in docs/log.md.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

# PR 3 — CSS-level detector (Detector B)

PR 3 runs at the `Assignment` / `@function` layer. It cannot be fleshed out in detail until we've inspected what CSS the CSS-DOS REP STOSB rewrite actually emits. The tasks below are skeletal — fill in shape-specific details during implementation.

### Task 3.1: Inspect the emitted CSS

- [ ] **Step 1: Find the splash-fill section in `output/bootle-ctest.css`**

Run:
```bash
grep -n "STOSB\|memset\|splash_fill\|stosb" ../../../output/bootle-ctest.css | head -20
```

Or search for a tight-loop `@function` body with a memory-write pattern. The splash.c → REP STOSB rewrite commit message (summary: commit `2acc748`) should hint at the property/function names.

- [ ] **Step 2: Document the shape**

Write a short note in `docs/superpowers/specs/2026-04-18-memory-fill-op-design.md` under "Open questions" describing the exact CSS shape (property names, assignment form, whether it's an `@function` or a flat chain).

### Task 3.2: Write a failing CSS-level test

**Files:**
- Create: `crates/calcite-core/tests/memory_fill_css.rs`

- [ ] **Step 1: Construct a minimal CSS fixture reproducing the shape**

```rust
//! CSS-level memory-fill detector — parses real-world-shaped CSS and
//! verifies the compiled program contains Op::MemoryFill where expected.

use calcite_core::compile::Op;
use calcite_core::parser::parse_css;

const FILL_CSS: &str = r#"
/* Fill TBD based on Task 3.1 findings. Placeholder shape: */
@property --dst { syntax: "<integer>"; initial-value: 655360; inherits: false; }
@property --val { syntax: "<integer>"; initial-value: 66; inherits: false; }
@property --cnt { syntax: "<integer>"; initial-value: 128; inherits: false; }
/* ... the actual loop shape from bootle-ctest.css ... */
"#;

#[test]
#[ignore = "fill CSS source in during Task 3.2 step 1; un-ignore once shape confirmed"]
fn css_level_detector_recognises_splash_fill() {
    let parsed = parse_css(FILL_CSS).expect("parse");
    let program = calcite_core::compile::compile(&parsed);
    let has_fill = program.ops.iter().any(|op| matches!(op, Op::MemoryFill { .. }));
    assert!(has_fill, "CSS-level detector should emit Op::MemoryFill for the canonical shape");
}
```

- [ ] **Step 2: Fill in the real CSS shape from Task 3.1 and un-ignore**

Replace `FILL_CSS` with the minimum CSS that reproduces the splash-fill shape. Remove `#[ignore]`.

### Task 3.3: Implement the CSS-level detector

**Files:**
- Modify: `crates/calcite-core/src/pattern/memory_fill.rs`
- Modify: `crates/calcite-core/src/compile.rs` (~near `recognise_broadcast` call)

- [ ] **Step 1: Add a CSS-level recogniser**

At the bottom of `pattern/memory_fill.rs`, add:

```rust
use crate::types::{Assignment, Expr, ParsedProgram};

/// Result of CSS-level memory-fill detection.
pub struct CssFillResult {
    /// Recognised fills.
    pub fills: Vec<CssMemoryFill>,
    /// Assignment property names absorbed (should be skipped by the lowering pass).
    pub absorbed: std::collections::HashSet<String>,
}

#[derive(Debug, Clone)]
pub struct CssMemoryFill {
    pub dst_property: String,
    pub val_expr: Expr,
    pub count_property: String,
}

/// Analyse `assignments` for CSS-level byte-fill patterns. Structural
/// match only — no property-name semantics (cardinal rule).
pub fn recognise_css_fills(assignments: &[Assignment]) -> CssFillResult {
    // Scaffold — real implementation written after Task 3.1.
    let _ = assignments;
    CssFillResult { fills: Vec::new(), absorbed: Default::default() }
}
```

- [ ] **Step 2: Implement the matcher after Task 3.1 findings are in hand**

Structure:
1. Scan assignments for a group of three mutually-referring properties (dst, val, count) where:
   - dst is assigned `if(count != 0) then dst + 1 else dst` (or the CSS equivalent).
   - count is assigned `if(count != 0) then count - 1 else count`.
   - Some property is assigned `if(dst-condition) then val else keep` — the byte store.
2. Verify val is a loop-invariant reference (not updated inside this assignment group).
3. Emit a `CssMemoryFill` per detected group.

Cardinal-rule guard: structural pattern only. Do NOT hard-code property names from the CSS-DOS emit.

- [ ] **Step 3: Wire into the compile pipeline**

In `compile.rs`, near where `recognise_broadcast` is called (search for it), add:

```rust
let css_fill_result = crate::pattern::memory_fill::recognise_css_fills(&parsed.assignments);
```

Threaded through so the absorbed properties are skipped during assignment lowering and a `MemoryFill` op is emitted for each detected group.

- [ ] **Step 4: Make the test pass**

Run: `cargo test -p calcite-core --test memory_fill_css`
Expected: PASS.

### Task 3.4: Independent benchmark — A alone vs B alone vs both

**Files:**
- (no file changes; cfg gating)

- [ ] **Step 1: Add feature flags**

In `crates/calcite-core/Cargo.toml`, add:

```toml
[features]
default = ["memfill-peephole", "memfill-css"]
memfill-peephole = []
memfill-css = []
```

Wrap the two detector-dispatch call sites in `#[cfg(feature = "memfill-peephole")]` / `#[cfg(feature = "memfill-css")]`.

- [ ] **Step 2: Run four configurations**

```bash
cargo run --release --no-default-features --bin calcite-bench -- -i output/bootle-ctest.css --halt 719359 --max-ticks 3000000
cargo run --release --no-default-features --features memfill-peephole --bin calcite-bench -- -i output/bootle-ctest.css --halt 719359 --max-ticks 3000000
cargo run --release --no-default-features --features memfill-css --bin calcite-bench -- -i output/bootle-ctest.css --halt 719359 --max-ticks 3000000
cargo run --release --bin calcite-bench -- -i output/bootle-ctest.css --halt 719359 --max-ticks 3000000
```

Record halt ticks and ticks/s in `docs/log.md`.

- [ ] **Step 3: Decide**

Based on numbers:
- If A alone ≈ both, keep A only (simpler). Feature-gate B off by default.
- If B alone > A alone, make B default.
- If both > either alone, keep both on.

### Task 3.5: Commit PR 3

- [ ] **Step 1: Stage**

```bash
git add crates/calcite-core/src/pattern/memory_fill.rs crates/calcite-core/src/compile.rs crates/calcite-core/tests/memory_fill_css.rs crates/calcite-core/Cargo.toml docs/log.md docs/superpowers/specs/2026-04-18-memory-fill-op-design.md
```

- [ ] **Step 2: Commit**

```bash
git commit -m "$(cat <<'EOF'
perf(core): CSS-level memory-fill detector (Detector B)

Adds a second, independent detector at the Assignment layer (runs
alongside the bytecode peephole from PR 2). Both detectors lower to
the same Op::MemoryFill. Feature-gated so each can be benchmarked
alone and together.

Bench results in docs/log.md. Default feature set reflects the winning
configuration.

Cardinal-rule-safe: matches structural CSS shape (incrementing address,
decrementing counter, constant-value byte store) — no property-name
semantics.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-review

Spec coverage:
- Op definition → Tasks 1.1-1.7.
- Evaluator semantics (hot path + profile + trace) → Tasks 1.4, 1.5, 1.6.
- Op unit tests (basic, zero-count, OOB, negative dst, byte-mask) → Tasks 1.8, 1.9.
- Bytecode peephole detector → Tasks 2.1-2.5.
- Peephole unit tests (accept canonical; reject non-invariant val, non-looping jump, non-unit stride, non-zero branch threshold) → Tasks 2.2, 2.4.
- Integration correctness gate (memory hash match on bootle-ctest.css) → Task 2.6.
- Benchmark numbers → Task 2.7.
- CSS-level detector → PR 3 (Tasks 3.1-3.5).
- Side-by-side benchmark of A alone / B alone / both → Task 3.4.

Placeholder scan: Task 3.2 step 1 and Task 3.3 step 2 have intentional placeholders dependent on Task 3.1 findings — explicit and gated on inspection of real CSS. Not a plan failure; it's the correct structure for "can't know the shape until we look."

Type consistency: `Op::MemoryFill` signature used identically in every task (dst_slot, val_slot, count_slot, exit_target — all `u32` slot / `u32` target). `Slot` is u32 in the existing codebase (confirmed by the existing `compact_slots` code shape).
