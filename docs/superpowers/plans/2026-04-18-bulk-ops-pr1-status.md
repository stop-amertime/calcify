# Bulk-ops PR 1 — calcite-side status (2026-04-18)

Status: **calcite-side landed; CSS-DOS side still pending.**

## What's in this branch

Tasks 1.6, 1.7, and 1.10 from `2026-04-18-bulk-ops.md` are complete
**on the calcite side**. This branch contains no CSS-DOS changes because
the environment the implementer was given didn't include the sibling
repo; the CSS-DOS half of PR 1 (tasks 1.1–1.5) still needs to be done
before the whole pipeline will light up end-to-end.

### AST extensions (needed to represent the new CSS shape)

- `StyleTest::Compare { left: Expr, op: CompareOp, right: Expr }` —
  represents `calc(X <op> Y)` as a boolean predicate inside an `if()`
  condition. Without this there's no way to type the bulk-op clause
  `style(--bulkOpKind: 1) and calc(<addr> >= var(--bulkDst))`.
- `CompareOp` enum: `Ge | Gt | Le | Lt | Eq | Ne`.
- Six new `Op::Cmp*` variants (`CmpNe`, `CmpGe`, `CmpGt`, `CmpLe`,
  `CmpLt`) alongside the existing `CmpEq`. All five hooks updated:
  `op_slots_read`, `op_dst`, `map_op_slots`, `seed_from_parent`, and
  the three execution functions (`exec_ops`, `exec_ops_profiled`,
  `exec_ops_traced`).
- Interpreter `eval_style_test` handles `StyleTest::Compare`; dependency
  walker `collect_style_test_deps` recurses into left/right.
- Compiler `compile_style_test` lowers `StyleTest::Compare` to the
  matching `Op::Cmp*` variant.
- Constant-folding `fold_test` traverses `Compare` operands.

These are honest calcite structural extensions. They cost nothing for
CSS that doesn't use them (no new ops in existing bytecode) and they
don't bake any x86 knowledge into the evaluator.

### Detector — `crates/calcite-core/src/pattern/memory_fill.rs`

`recognise_bulk_fills(&[Assignment]) -> BulkFillResult` scans memory-cell
assignments for the shape

```text
style(--<kind>: K)
  and calc(<addr> >= var(--<dst>))
  and calc(<addr> < var(--<dst>) + var(--<count>))
  : var(--<val>)
```

at the top of a cell's `if()` chain. Cells with matching bindings are
grouped by `(kind_property, kind_value, dst_property, count_property,
val_property)`; each group emits one `CssMemoryFillBinding` when its
cell addresses form a dense contiguous range. Sparse groups are
rejected (a bulk fill would clobber gaps the CSS doesn't actually fill).

Accepts both `--memByte_<N>` (v4 DOS output) and `--m<N>` (legacy
hack-path) cell naming — see `parse_cell_addr`.

Tests (all passing):
- `recognises_bulk_fill_across_cells` — 256-cell dense range collapses
  to a single binding with the expected property names.
- `sparse_range_rejected` — 256 cells missing one in the middle yields
  no binding.
- `distinct_bindings_emitted_separately` — two non-overlapping groups
  produce two bindings in address order.
- `non_bulk_assignment_ignored` — plain broadcast-write cells never
  match.
- `wrong_compare_op_rejected` — `>` instead of `>=` is rejected.

### Pipeline wiring

- `CompiledProgram.bulk_fill_bindings: Vec<CssMemoryFillBinding>` added.
- `Evaluator::from_parsed` calls `recognise_bulk_fills` after
  `compile::compile` and stores the result on the compiled program.
  Logs one line per recognised binding.
- New method `Evaluator::execute_bulk_fills(&State)` reads the
  kind/dst/count/val properties; when `state.read_mem(kind) == kind_value`
  it executes `state.memory[lo..hi].fill(val as u8)` where the range is
  clipped to the binding's validated cell range and memory bounds.
- All four tick entry points call `execute_bulk_fills` before the
  compiled bytecode pass: `tick`, `tick_no_diff`, `tick_profiled`,
  `tick_interpreted`.

### Integration tests — `crates/calcite-core/tests/memory_fill_css.rs`

- `bulk_fill_fires_on_active_tick_and_fills_range` — builds a
  `ParsedProgram` of 256 memory-cell assignments matching the bulk-fill
  shape, declares the four control `@property`s, writes `kind=1`,
  `dst=CELL_START+16`, `count=16`, `val=0x5A` into state, ticks once,
  and verifies memory[272..288] == 0x5A with surrounding bytes
  untouched.
- `bulk_fill_inactive_when_kind_is_zero` — same setup with `kind=0`;
  sentinel bytes 0xCD inside the would-be-fill range must survive the
  tick.

Both pass.

## What's deliberately NOT done here

- **Task 1.1 — BIOS INT 2F/AH=FE handler** (CSS-DOS repo). Writes the
  bulk-op control words to memory 0x500–0x505.
- **Task 1.2 — Transpiler emission** (CSS-DOS repo). Adds the 6
  `@property` declarations, derives `--bulkOpKind`/etc. from
  BIOS-reserved memory, and prepends the range-predicate clause to
  every memory cell.
- **Task 1.3 — Chrome conformance test** (cross-repo + headless
  browser). Verify a small program's memory matches.
- **Task 1.4 — `splash.c` rewrite** (CSS-DOS repo). Replaces REP STOSB
  with `INT 2F/AH=FE/AL=0`.
- **Task 1.5 — Regenerate `bootle-ctest.css`** (runs transpiler).
- **Task 1.8 — End-to-end `probe-splash-project` correctness gate**
  (requires regenerated CSS).
- **Task 1.9 — Benchmark numbers** (requires regenerated CSS).
- **Parser extension for bare `calc(...)` in style-test chains**. The
  detector matches an AST shape that the current calcite parser cannot
  produce — `parse_style_condition` only chains `style()` tests. When
  CSS-DOS starts emitting range-predicate clauses, the parser must
  grow to accept `style(...) and calc(...)` or whichever Chrome-legal
  form the transpiler chooses. Deferred because the exact CSS form
  isn't pinned down yet (spec risk-line item: "Chrome requires `= 1`
  explicitly?"). Integration tests build the AST directly, so the
  detector + execution can be exercised independently of parser work.

## Assumed follow-up order once CSS-DOS lands

1. Pick a concrete Chrome-legal form for the range-predicate clause
   (small HTML probe per Task 1.3 methodology).
2. Extend `parse_style_condition` to accept that form.
3. Run `probe-splash-project` against the regenerated bootle-ctest
   (Task 1.8); fix whatever misaligns (likely the `--bulkDst` /
   `--bulkCount` / `--bulkValue` property addresses — the detector
   resolves them via `property_to_address`, so they must be declared
   `@property` and land in the state-var address region).
4. Benchmark (Task 1.9) and commit numbers.

## Correctness notes

- The pre-tick execution is the "simpler path" described in the plan's
  Task 1.7 Step 3: per-cell branches still evaluate normally in the
  bytecode pass afterward. For bulk-active ticks this is redundant but
  idempotent (both paths write the same bulk value). The speedup comes
  from collapsing ~64 K separate calcite ticks (one per REP STOSB byte)
  into a single tick that fills 64 KiB natively.
- Skipping per-cell branches when the bulk fires would require the
  detector to strip the bulk-branch from each absorbed cell's
  assignment before compile. Left for a follow-up if profiling shows
  the per-cell overhead dominates.
- Calcite never reads x86 opcodes here. The detector matches a CSS
  structural shape; the executor does `memory.fill` on a validated
  slice. No opcode semantics, no instruction decode — just pattern
  recognition and an idiomatic bulk memory op.
