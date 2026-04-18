# Bulk-ops PR 1 — CSS-DOS-facing contract

Audience: the developer implementing the CSS-DOS half of PR 1 (BIOS
handler, transpiler emission, `splash.c` rewrite) who does NOT have
access to the calcite source tree.

Purpose: specify **exactly** what CSS calcite's bulk-fill detector
expects, so the transpiler emits something calcite will recognise the
first time.

If your emitted CSS deviates from this contract, calcite will silently
fall back to the slow per-tick path — no error, just no speedup. The
only way to notice is the "recognised N bulk-fill bindings" log line
will show N=0, and the splash halt tick won't drop.

Ground truth: the integration test
`crates/calcite-core/tests/memory_fill_css.rs` in the calcite repo
builds the exact AST calcite recognises. If the contract below
conflicts with that test, the test wins — ask for an updated spec.

---

## 1. The four bulk-op state properties

Emit these four `@property` declarations in the CSS header. Names are
fixed — the detector resolves them by name via calcite's property-address
map:

```css
@property --bulkOpKind { syntax: "<integer>"; initial-value: 0; inherits: false; }
@property --bulkDst    { syntax: "<integer>"; initial-value: 0; inherits: false; }
@property --bulkCount  { syntax: "<integer>"; initial-value: 0; inherits: false; }
@property --bulkValue  { syntax: "<integer>"; initial-value: 0; inherits: false; }
```

Calcite maps property names to state addresses via its existing
`property_to_address` routine, which consumes the `@property`
declarations. If any of these four are missing or renamed, the pre-tick
executor's `property_to_address` lookup returns `None` and the fill is
silently skipped.

(The spec mentions `--bulkSrc` and `--bulkMask` — those are for PR 2
and PR 3 and can be declared now for forward-compat, but PR 1 only
reads the four above.)

## 2. Bulk-op state derivation from BIOS-reserved memory

Still in the header, derive the four state properties from whatever
BIOS-reserved memory addresses your INT 2F/FE handler writes into. The
spec picks `0x500..0x505`. Example shape:

```css
:root {
  --bulkOpKind: calc(var(--memByte_1280));                                      /* 0x500 */
  --bulkDst:    calc(var(--memByte_1281) + var(--memByte_1282) * 256);          /* 0x501-0x502 */
  --bulkCount:  calc(var(--memByte_1283) + var(--memByte_1284) * 256);          /* 0x503-0x504 */
  --bulkValue:  calc(var(--memByte_1285));                                      /* 0x505 */
}
```

Calcite's detector does not inspect these four assignments. It only
cares that the four `@property`s exist and end up mapped to state
addresses so the pre-tick executor can `read_mem` them. This
derivation pattern is fine; anything that produces the same values in
Chrome works.

**One-shot semantics come from the BIOS memory being overwritten each
tick.** The handler writes `0x500 = 1` during the INT; on any tick
where no INT ran, `0x500` stays whatever it was — typically 0 after
your handler's own cleanup or after the fallthrough path clears it.

## 3. The per-cell range-predicate clause

This is the only shape calcite recognises. **Read this section carefully**
— tiny structural deviations cause silent fallback.

### 3.1 Cell property naming

Each memory cell's custom property must be named using one of:

- **`--memByte_<N>`** (v4 DOS convention) — `<N>` is a **base-10 integer**
  literal suffix, e.g. `--memByte_655360`.
- **`--m<N>`** (legacy hack-path convention) — same naming rule.

No hex. No leading zeros that change the value. `N` is the linear byte
address the cell represents, in the same units everything else uses.

### 3.2 Top-of-chain branch shape

Each cell's `if()` chain must have the range-predicate branch **as the
very first branch**, before any `--memAddr0/1/2` broadcast write
branches. Exact shape:

```css
--memByte_<N>: if(
  style(--bulkOpKind: 1) and calc(<N> >= var(--bulkDst)) and calc(<N> < var(--bulkDst) + var(--bulkCount)):
    var(--bulkValue);
  style(--memAddr0: <N>): var(--memVal0);
  style(--memAddr1: <N>): var(--memVal1);
  /* ... further broadcast ports ... */
  else: var(--__1memByte_<N>));
```

### 3.3 The three clauses of the `and` — strict ordering

The first branch's condition must be exactly three tests joined with
`and`, **in this exact order** (the detector rejects re-orderings):

1. **Kind gate, first**: `style(--bulkOpKind: 1)`
   - Property name **must be `--bulkOpKind`** (matches the `@property`
     declaration).
   - The value `1` **must be a whole-number integer literal**. Not
     `calc(1)`, not `var(--one)`. If you want to support PR 2/PR 3
     kinds (2 = memcpy, 3 = masked), use a different literal; the
     detector keys on it.

2. **Lower bound, second**: `calc(<N> >= var(--bulkDst))`
   - **Left-hand side must be the cell's literal address** `<N>` as an
     integer literal. Not `var(--addr)`, not a calc expression — just
     the same integer that appears in the cell's property name.
   - **Comparison operator must be `>=`**, not `>` and not `<=`.
   - **Right-hand side must be exactly `var(--bulkDst)`**. No
     fallback, no wrapping calc.

3. **Upper bound, third**: `calc(<N> < var(--bulkDst) + var(--bulkCount))`
   - Left-hand side is again the cell's literal address `<N>`.
   - Operator must be `<` (strict less-than). Not `<=`.
   - Right-hand side must be exactly `var(--bulkDst) + var(--bulkCount)`
     in that operand order. Not `var(--bulkCount) + var(--bulkDst)`,
     not `(var(--bulkDst) + var(--bulkCount)) + 0`, not
     `var(--bulkEnd)`. Calcite pattern-matches `Add(Var(dst), Var(count))`
     literally — swapping the operands or factoring through an
     intermediate variable breaks the match.

### 3.4 The then-branch

The value written when the condition matches must be:

- **Exactly `var(--bulkValue)`** — a `var()` reference with no
  fallback and no wrapping. Not `calc(var(--bulkValue))`, not
  `var(--bulkValue, 0)`.

### 3.5 Everything after the first branch is CSS-DOS's call

The second and later branches (your `--memAddr0/1/2` broadcast ports
and the `else: keep`) are yours. Calcite's existing broadcast-write
detector handles them independently; calcite does not require any
particular shape there. The bulk-fill detector only inspects
`branches[0]`.

## 4. What "cell addresses must form a dense contiguous range" means

Calcite groups cells by the tuple `(--bulkOpKind kind value, --bulkDst,
--bulkCount, --bulkValue)` and then checks: **within each group, the
cell addresses must form a contiguous range** — no gaps.

If you emit the range-predicate clause on cells `0xA0000..0xA0100`
*except* `0xA0050` (for any reason — maybe that cell has special-case
CSS), calcite rejects the entire group. No binding, no speedup.

In practice: either emit the range-predicate clause on **every** cell,
or on **no** cells in the region you want bulk-filled. Don't partially
emit.

## 5. The `calc(X >= Y)` open question

CSS Values 4 `calc()` doesn't spec comparison operators. The design
spec flagged this as a Chrome-compatibility open question. Possible
resolutions a CSS-DOS developer might try:

- Plain `calc(<N> >= var(--bulkDst))` inside an `and`-chain of
  conditions — requires Chrome's `if()` to accept a calc value as
  truthy in an `and` chain. Probably works in current Chrome but
  **verify with a small probe first**.
- Explicit equality: `calc(<N> >= var(--bulkDst)) = 1` — reads as
  "this calc evaluates to 1". Depending on CSS `if()` syntax, this may
  parse differently.
- Pre-compute the boolean as a CSS value: impractical at per-cell
  scale.

**Before wiring the splash path, run the Chrome probe in task 1.3 of
the plan** and verify that a small test program actually writes the
expected bytes. If Chrome reports the wrong bytes, the range-predicate
syntax is broken — fix that before continuing.

Whichever form Chrome accepts, calcite's parser will need a matching
extension. That's a calcite-side follow-up, tracked in
`docs/superpowers/plans/2026-04-18-bulk-ops-pr1-status.md`.

## 6. The CSS-DOS-side failure modes and how to diagnose

If, after emitting everything, the splash halt tick doesn't drop:

1. **Check calcite's detector fired.** Calcite logs at INFO:
   ```
   Recognised bulk-fill binding: kind=--bulkOpKind=1 dst=--bulkDst count=--bulkCount val=--bulkValue range=0xA0000..0xB0000 (65536 cells)
   ```
   If the log line is missing or shows N=0, the detector didn't match.

2. **If no binding detected**, the per-cell shape is wrong. Common
   causes, in order of likelihood:
   - Operand order in the third `and` test is swapped (see 3.3 item 3).
   - `<N>` on the LHS of a Compare is an expression instead of a
     literal.
   - The kind-gate value isn't a whole-number literal.
   - Cell property names aren't `--memByte_<N>` or `--m<N>`.
   - The range-predicate branch isn't the first branch in the `if()`
     chain.
   - The range isn't dense (section 4).

3. **If binding detected but memory still wrong**, the four state
   properties aren't mapping to the right BIOS addresses. Verify:
   - All four `@property` declarations present with integer syntax.
   - The BIOS handler actually wrote the six bytes at `0x500..0x505`
     (or wherever you picked) before `iret`.
   - The `:root` derivation in section 2 reads the same addresses the
     BIOS writes.
   - Calcite's `property_to_address("--bulkDst")` returns `Some(addr)`
     — if this is None, the state-var region doesn't include
     `--bulkDst`. Usually means the `@property` was parsed as a
     non-integer syntax.

4. **If Chrome renders wrong bytes** (before any calcite involvement),
   the CSS itself is broken. Most likely the `calc(X >= Y)` form
   doesn't work in the Chrome version you're testing — try the `= 1`
   form.

## 7. Minimum Chrome verification before declaring done

Even without the full `probe-splash-project` gate, you can smoke-test
the CSS side independently:

- A minimal `.com` program that executes INT 2F/AH=FE/AL=0 once,
  filling 256 bytes at a known address, then halts.
- Generate CSS from it.
- Load the CSS into Chrome, force-style ticks until the halt state,
  and read the target memory bytes via `getComputedStyle`.
- Expected: the 256 bytes = fill value; bytes just before and after
  untouched.

This is task 1.3 in the plan. **Do it first** — calcite won't help you
debug a Chrome-side CSS bug.

## 8. What calcite guarantees in exchange

Given CSS that conforms to this contract, calcite:

- Emits one log line per recognised binding at `Evaluator::from_parsed`.
- Before every tick, reads `--bulkOpKind`; if equal to the binding's
  `kind_value`, executes `state.memory[lo..hi].fill(value as u8)` —
  one native Rust fill per binding, regardless of range size.
- Clips the fill to (a) the binding's validated cell range and (b) the
  state memory bounds. A runaway `--bulkCount` can't corrupt memory
  outside the range the detector saw at compile time.
- Runs the per-cell bytecode afterward. For bulk-active ticks this is
  redundant (it computes the same bulk value) but correct. For
  bulk-inactive ticks, the per-cell path runs normally and the
  detector's fill path is a no-op.

## 9. Things calcite explicitly does NOT do

- It does not clear `--bulkOpKind` after firing. That's the CSS's job
  (the `:root` derivation in section 2 re-reads the BIOS byte each
  tick; if the BIOS clears or overwrites `0x500`, `--bulkOpKind` goes
  back to 0 naturally).
- It does not enforce one-shot semantics. If `--bulkOpKind` stays 1
  for N ticks, calcite runs the fill N times. That's equivalent to what
  Chrome does, so it's correct — but if the BIOS forgets to clear
  `0x500`, the fill repeats every tick. Free-ish in calcite (one fill
  op), expensive in Chrome (re-evaluates every cell).
- It does not handle non-contiguous ranges, alternate kind values
  within a group, or multiple concurrent bulk ops of the same kind.
  One kind × one dst range per binding.
- It does not validate the fill shape against the actual memory map.
  If `--bulkDst = 0` and `--bulkCount = 1_000_000`, calcite fills the
  whole state memory (clipped to length). That's consistent with the
  CSS semantics, and Chrome would do the same. Just don't set garbage
  values.

---

## 10. The canonical AST fixture (for reference)

The calcite test `memory_fill_css.rs::mk_bulk_fill_cell` constructs
this AST per cell — it's the ground truth for what the detector
matches. In pseudo-CSS:

```css
--memByte_<N>: if(
  /* StyleTest::And of exactly 3: */
  style(--bulkOpKind: 1)       /* StyleTest::Single, value = Expr::Literal(1.0) */
  and calc(<N> >= var(--bulkDst))
                                /* StyleTest::Compare { Lit(N), Ge, Var(--bulkDst) } */
  and calc(<N> < var(--bulkDst) + var(--bulkCount)):
                                /* StyleTest::Compare { Lit(N), Lt,
                                    Calc(Add(Var(--bulkDst), Var(--bulkCount))) } */
    var(--bulkValue);           /* branch.then = Expr::Var { --bulkValue, no fallback } */
  /* ... your normal broadcast branches ... */
  else: <your keep> );
```

If your emitted CSS's AST doesn't match this shape exactly, the
detector won't fire.

## 11. Quick checklist before you ship

- [ ] Six `@property` declarations emitted (`--bulkOpKind`, `--bulkDst`,
  `--bulkSrc`, `--bulkCount`, `--bulkValue`, `--bulkMask`) with
  `syntax: "<integer>"`.
- [ ] `:root` derivations from BIOS-reserved addresses emitted
  per-tick (not gated — must evaluate every tick).
- [ ] Every cell in the bulk-fillable region has the range-predicate
  branch as its first `if()` branch with the exact shape from section
  3.
- [ ] No gaps in the range. Either every cell or no cell.
- [ ] Cell naming is `--memByte_<N>` or `--m<N>`.
- [ ] BIOS handler writes all six bytes at the chosen scratch address
  and the CSS derivation reads the same address.
- [ ] Chrome smoke test (task 1.3) passes: small program fills N bytes
  of memory, `getComputedStyle` reads back the correct values.
- [ ] Calcite log on a fresh run shows `Recognised bulk-fill binding:
  ... (N cells)` with N > 0.

If all check, the `probe-splash-project` memory-hash gate (task 1.8)
should pass; if not, one of the above went wrong.
