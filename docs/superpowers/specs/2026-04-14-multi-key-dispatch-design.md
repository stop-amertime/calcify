# Multi-Key Dispatch Tables

**Date:** 2026-04-14
**Status:** Design
**Scope:** calcite-core pattern recognition + bytecode

## Problem

`BranchIfNotEqLit` is 96% of executed ops, 34K/tick, 99.9% taken. Dispatch lookup itself is cheap (~0.6us per fire, ~22 fires/tick = 13us). The overhead is *inside* the dispatch entries: each entry body contains an `if()` chain whose branches are `And(StyleCondition, StyleCondition, ...)` on a small, shared set of keys (typically `--mod`, `--rm`, `--reg`), compiled today to a linear `BranchIfNotEqLit` chain.

Example, `rogue.css` line 1620 (opcode=128 body, 16 branches):

```
if(style(--mod: 3) and style(--rm: 0) and style(--reg: 0): ...;
   style(--mod: 3) and style(--rm: 4) and style(--reg: 0): ...;
   style(--mod: 3) and style(--rm: 0) and style(--reg: 1): ...;
   ...
   else: var(--__1AX))
```

Every branch tests the same ordered key set `(--mod, --rm, --reg)` against literal values. The existing single-key dispatch recognizer rejects this because the conditions are `StyleTest::And`, not `StyleTest::Single`. So the chain stays linear.

Agent-measured occurrences in rogue.css:
- 129 chains with 2-key `And` branches
- 38 with 3-key
- 20 with 4-key

That's 187 dispatch tables currently stuck in linear evaluation, which together account for most of the 34K branches/tick.

## Goal

Extend pattern recognition to detect `if()` chains where every branch's condition is a `StyleTest::And` testing the same ordered tuple of property keys against integer literals, and compile them to a single HashMap lookup keyed by the tuple.

Target on rogue: 34K `BranchIfNotEqLit`/tick → low thousands. Dispatch fires/tick stays roughly constant; work per fire drops by 10-100x.

## Non-goals

- No `Or` recognition. Not seen in the data.
- No set-membership pattern. Earlier hypothesis was wrong; the data is purely conjunctive.
- No JIT to native code (cranelift, etc.). Separate project, not justified until this lands and we re-measure.
- No changes to the broadcast-write recognizer.
- No CSS changes. Cardinal rule.

## Design

### Data types

New pattern struct, parallel to `DispatchTable`:

```rust
// crates/calcite-core/src/pattern/multi_dispatch.rs
pub struct MultiKeyDispatchTable {
    pub key_properties: Vec<String>,     // ordered, length 2..=N
    pub entries: HashMap<Vec<i64>, Expr>, // key tuple → result expr
    pub fallback: Expr,
}
```

The key type is `Vec<i64>` not a fixed tuple, because chain widths vary (2, 3, 4). Hash+eq on `Vec<i64>` is adequate — key widths are small (≤4 in observed data) and the hashmap is built once at compile time.

### Recognizer

```rust
pub fn recognise_multi_dispatch(branches: &[StyleBranch], fallback: &Expr)
    -> Option<MultiKeyDispatchTable>
```

Accepts when:
1. `branches.len() >= 4` (same threshold as single-key).
2. Every branch's condition is `StyleTest::And(inner)` with `inner.len() >= 2`.
3. Every inner test is `StyleTest::Single { property, value: Literal }`.
4. The ordered list of properties is **identical** across all branches.
   - "Identical" means same property names in same positions. If one branch has `(--mod, --rm, --reg)` and another has `(--rm, --mod, --reg)`, reject. (Observed data has consistent order per chain.)
5. No duplicate property within a single branch's condition (e.g., `--rm: 0 and --rm: 4` — would mean "always false"; reject defensively).

Returns `None` on any violation — caller falls back to existing single-key recognizer, then linear compilation. This is additive and cannot regress correctness.

**Call order in `compile_style_condition`:**
```rust
if let Some(table) = multi_dispatch::recognise_multi_dispatch(branches, fallback) {
    return self.compile_inline_multi_dispatch(&table, ops);
}
if let Some(table) = dispatch_table::recognise_dispatch(branches, fallback) {
    return self.compile_inline_dispatch(&table, ops);
}
self.compile_style_condition_linear(branches, fallback, ops)
```

Multi-key is tried first because it's strictly more specific. A chain can't match both.

### Bytecode

New op, parallel to `Op::Dispatch`:

```rust
Op::MultiDispatch {
    dst: Slot,
    keys: Vec<Slot>,       // one slot per key property
    table_id: Slot,        // index into compiled_multi_dispatches
    fallback_target: u32,  // unused, kept for structural parity
}
```

New compiled-table struct, parallel to `CompiledDispatchTable`:

```rust
pub struct CompiledMultiDispatchTable {
    pub key_width: u32,
    pub entries: HashMap<Vec<i64>, (Vec<Op>, Slot)>,
    pub fallback_ops: Vec<Op>,
    pub fallback_slot: Slot,
}
```

Stored on `CompiledProgram.multi_dispatch_tables: Vec<CompiledMultiDispatchTable>` — separate vector from `dispatch_tables` to keep op variants disjoint and preserve `table_id` indexing.

### Compilation

`compile_inline_multi_dispatch` mirrors `compile_inline_dispatch`:
1. Compile each key property to a read slot: `let key_slots: Vec<Slot> = table.key_properties.iter().map(|p| self.compile_var(p, None, ops)).collect();`
2. For each entry: compile the expr to its own `(entry_ops, result_slot)`.
3. Compile fallback to `(fallback_ops, fallback_slot)`.
4. Allocate a dst slot.
5. Emit `Op::MultiDispatch { dst, keys: key_slots, table_id, fallback_target: 0 }`.

### Evaluation

In `exec_ops` (and the profiled + debug variants):

```rust
Op::MultiDispatch { dst, keys, table_id, .. } => {
    let table = &multi_dispatch_tables[*table_id as usize];
    let key_tuple: Vec<i64> = keys.iter().map(|s| slots[*s as usize] as i64).collect();
    if let Some((entry_ops, result_slot)) = table.entries.get(&key_tuple) {
        exec_ops(entry_ops, /* ... */);
        slots[*dst as usize] = slots[*result_slot as usize];
    } else {
        exec_ops(&table.fallback_ops, /* ... */);
        slots[*dst as usize] = slots[table.fallback_slot as usize];
    }
}
```

Key-tuple allocation: a `Vec<i64>` per MultiDispatch fire is an allocation per lookup. With ~22 dispatch fires/tick and maybe ~20 MultiDispatch fires/tick nested within those, that's ~440 allocations/tick of 2-4 i64s. At ~6000 ticks/s that's ~2.6M allocations/s — potentially meaningful.

**Mitigation:** use a reusable thread-local `Vec<i64>` scratch buffer for the lookup key, or key on a fixed-size `[i64; 4]` with a sentinel for unused slots (since observed max width is 4). Start with the `Vec` version for simplicity; optimize the allocation if profiling shows it dominates. The 10-100x branch reduction dwarfs the allocation cost even unmitigated.

### Other integrations

Every place that walks over `Op` variants in compile.rs/eval.rs must handle `MultiDispatch`:
- `op_slots_read` — returns all `keys`.
- `op_dst` — returns `Some(dst)`.
- `map_op_slots` — remaps `dst` and each entry of `keys`.
- `seed_from_parent` — seeds each key slot.
- Liveness analysis — recurse into entries and fallback like `Dispatch` does (line ~2109).
- Slot-compaction nesting-depth analysis (line ~1940) — this pass assigns dispatch tables to layers so nested entries don't clobber outer slot ranges. Extend it to treat `Dispatch` and `MultiDispatch` tables as a single unified namespace for depth purposes: a `MultiDispatch` table referenced inside a `Dispatch` entry (or vice versa) must sit at depth+1. Concretely: index tables by `(Kind, id)`, collect cross-kind refs, compute depth over the union, and lay out both vectors' entries across depth tiers.
- Debug executor.
- Profile executor + add a `multi_dispatch_count` counter.

## Correctness verification

1. `cargo test --workspace` — unit tests must pass, including new tests:
   - Recognizes 3-key chain with 4+ branches.
   - Rejects when branches have differing key sets.
   - Rejects when inner test uses non-literal value.
   - Rejects chains with <4 branches.
   - Single-branch lookup returns the right value.
   - Unmatched key falls back.
2. `cargo clippy --workspace` clean.
3. Conformance: run `tools/compare-dos.mjs` or `tools/fulldiff.mjs` against the reference emulator on at least rogue and one other fixture (e.g., bootle). Zero divergence required. If anything diverges, the optimization is wrong.
4. `cargo run --release --bin calcite-bench -- -i output/rogue.css -n 5000 --warmup 500` before/after. Expect ≥3x throughput improvement based on the branch-count math.

## Risks

- **Key ordering bug.** If two branches list the same properties in different orders, the recognizer must reject (or normalize). The spec chooses reject — normalization adds complexity for a case we haven't observed.
- **Nested dispatch references.** The existing dispatch depth analysis (line 1960) assumes single `table_id` per op. Multi-dispatch adds a second table namespace. The fix: collect both types of references into the same dependency graph, indexed by (kind, id).
- **Slot aliasing across entries.** Existing `Dispatch` handles this via isolated `entry_ops`; same pattern applies. Just need to apply it uniformly in `seed_from_parent` and liveness for the new op.
- **f64 → i64 key conversion.** Slots are stored as `i32` (per `StoreState` docs) but compared as `i64` keys in the existing dispatch. Mirror exactly: `slots[key] as i64`.

## Rollout

Single PR. No feature flag — the fallback to linear is built into the recognizer, and correctness is bit-exact.

## Open questions

None at design stage. Implementation will answer:
- Does the `Vec<i64>` key allocation show up in the profile? If yes, switch to `[i64; 4]`.
- Do we hit any chains with inconsistent key ordering? If yes, add normalization.

---

## Postscript (2026-04-14): implemented, measured, removed

The design was implemented end-to-end and all tests passed (94 unit + integration, including `compiled_vs_interpreted` covering the new op). The recognizer fired at compile time — 116 tables matched on rogue.css — and the runtime correctly dispatched through them.

**But the speedup was zero within noise** (baseline 4786 ticks/s vs. with-opt 4690 ticks/s on rogue). The optimization was reverted.

Why: the `BranchIfNotEqLit` hot path on rogue isn't conjunctive-multi-key. Only ~2 MultiDispatch ops fired per tick. The 34K branches/tick that motivated this design come from chains that this recognizer rejects:
- Chains with fewer than 4 branches (too small to pass threshold, but collectively dominant).
- Chains mixing `StyleTest::Single` and `StyleTest::And` shapes (rejected by the same-ordered-keys requirement).
- Single-key chains below the 4-branch threshold in the existing single-key recognizer.

The earlier investigation correctly identified 3-key `And` chains in the CSS (e.g., rogue.css line 1620, the opcode-128 body with 14 branches of `(mod, rm, reg)`), but those chains are inside *outer dispatch entries* keyed on `--opcode`, and the specific opcodes containing them aren't on rogue's hot instruction stream.

### What the data actually says about next moves

After the build, program.ops post-fuse had 66K `BranchIfNotEqLit`, 0 `MultiDispatch`, 37 `Dispatch` at the top level. 22 outer dispatches fire per tick with ~6 sub-ops each. The 34K runtime branches live inside dispatch entry bodies as small linear chains — either <4 branches or mixed conditions.

Realistic levers from here:
1. Lower the single-key dispatch threshold and handle the overhead (small chains are cheap if the table is small).
2. Recognize mixed `Single`/`And` chains by normalizing — treat a missing property as a wildcard key.
3. Keep the CSS-side structure in mind: the transpiler generates these small chains; changing *what it emits* (more uniform shapes) is out of scope for calcite but would make a recognizer's job easier.

The spec itself is still a correct description of what multi-key dispatch *would* do; it just doesn't match rogue's dominant pattern. Keep this file as a reference for why this particular lever isn't the one.
