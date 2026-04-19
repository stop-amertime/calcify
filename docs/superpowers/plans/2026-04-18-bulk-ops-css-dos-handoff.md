# Handoff prompt for cloud agent — CSS-DOS side of bulk-ops PR 1

Copy the text below verbatim as the initial prompt to the cloud agent.
Everything above the horizontal rule is for you (the human); everything
below is for the agent.

The agent needs access to BOTH repos:
- **calcite** (JIT for computational CSS)
- **CSS-DOS** (8086-in-CSS + the transpiler/BIOS that generates it)

The calcite-side of PR 1 is already landed on branch
`claude/bulk-ops-implementation-P9zE7` and pushed. This prompt picks up
at the CSS-DOS half.

---

You are picking up a cross-repo performance project. Two sibling repos:

- **calcite** — Rust JIT for computational CSS.
- **CSS-DOS** (your cwd) — 8086 PC implemented in pure CSS, plus the
  transpiler/BIOS that generate the CSS.

The calcite-side of PR 1 is already done (branch
`claude/bulk-ops-implementation-P9zE7` on the calcite repo). Your job
is to implement the CSS-DOS half so the whole pipeline lights up
end-to-end.

## Before you do anything

Read these files in order, in full:

1. `CLAUDE.md` (CSS-DOS repo) — cardinal rule, git-interaction
   restrictions, mandatory logbook updates.
2. `docs/logbook/LOGBOOK.md` (CSS-DOS repo) — current status.
3. `../calcite/CLAUDE.md` — the sibling repo's rules.
4. `../calcite/docs/superpowers/specs/2026-04-18-bulk-ops-design.md` —
   the design spec for what the whole feature is. Read the WHOLE thing.
   It explains why compile-time pattern-matching failed and why
   range-predicate CSS works.
5. `../calcite/docs/superpowers/plans/2026-04-18-bulk-ops.md` —
   the task-by-task plan. You are doing **Tasks 1.1 through 1.5, plus
   1.8 and 1.9** (the CSS-DOS side + the cross-repo correctness gate
   and bench).
6. **`../calcite/docs/superpowers/plans/2026-04-18-bulk-ops-css-dos-contract.md`**
   — the self-contained spec for what CSS shape calcite's detector
   recognises. THIS IS THE AUTHORITATIVE SPEC for your emission work.
   If it conflicts with anything else, it wins.
7. `../calcite/docs/superpowers/plans/2026-04-18-bulk-ops-pr1-status.md`
   — what's already landed on the calcite side (AST extensions,
   detector, pre-tick execution, tests) and what's still pending. Note
   especially the "Parser extension" section — you may trigger this
   work as a side-effect.

Do not start coding until all seven are read.

## What you're picking up (30-second version)

Calcite already has:
- An AST that can represent `style(--bulkOpKind: 1) and calc(X >=
  var(--bulkDst)) and calc(X < var(--bulkDst) + var(--bulkCount))` as
  a condition.
- A detector that collapses every matching memory cell into a single
  `CssMemoryFillBinding`.
- A pre-tick executor that runs one native `memory.fill` per binding
  when `--bulkOpKind == 1`.
- 7 passing tests (5 unit, 2 integration) exercising the calcite side
  end-to-end with a hand-crafted AST.

What's missing: the CSS-DOS side that actually emits the CSS calcite
recognises, plus the BIOS handler that writes the bulk-op state into
memory, plus a real splash path that calls it.

## The tasks in scope

From `2026-04-18-bulk-ops.md`, you are doing:

- **Task 1.1** — Add `INT 2F/AH=FE/AL=0` handler in the BIOS. Writes
  `{kind=1, dst, count, value}` to BIOS-reserved memory (the plan
  suggests `0x500..0x505` — verify that range is clear first). This
  is the single-instruction handler whose body appears atomic from
  the guest's perspective.

- **Task 1.2** — Transpiler emission. Two parts:
  - Emit the four (or six, for forward-compat with PR 2/3)
    `@property` declarations and the `:root` derivation from BIOS
    memory.
  - Prepend the range-predicate clause to every memory-cell
    assignment, per the exact AST contract.

- **Task 1.3** — Chrome conformance test. Write a minimal `.com` that
  calls INT 2F once to fill 256 bytes, generate CSS, load into Chrome
  via Playwright, read back memory via `getComputedStyle`. This is a
  **hard gate**: if Chrome doesn't produce the right bytes, stop.

- **Task 1.4** — Rewrite `bios/splash.c` to use the new BIOS instead
  of REP STOSB for the framebuffer clear.

- **Task 1.5** — Regenerate `../calcite/output/bootle-ctest.css` and
  confirm it still boots in Chrome (second hard gate — don't break
  the existing boot path).

- **Task 1.8** — Calcite-side correctness gate. Run
  `probe-splash-project` against the regenerated CSS; memory hash
  must match Chrome's. This is the **end-to-end gate** — if it shows
  `DIFFER`, something in the pipeline is wrong.

- **Task 1.9** — Bench the splash halt tick before/after, write
  results to `../calcite/docs/log.md`.

## Critical rules

- **Chrome is the source of truth.** If Chrome and calcite disagree,
  calcite is wrong. Verify every CSS change in Chrome first.
- **CSS-DOS git rules are strict.** Do NOT commit or push on the
  CSS-DOS repo without explicit user permission. You may freely
  commit on the calcite repo (e.g. to update `docs/log.md` with
  bench results).
- **Update CSS-DOS's LOGBOOK.md** before stopping any session where
  you did CSS-DOS work. Mandatory per their CLAUDE.md.
- **No dummy properties, no side-channels.** Everything you emit in
  CSS must do honest work in Chrome. The range-predicate clause
  qualifies — Chrome really does evaluate it per cell per tick.
- **No x86 knowledge in calcite.** If you find yourself wanting to
  add anything x86-specific to the calcite side, stop and ask. The
  calcite detector is purely structural and must stay that way.

## The parser gotcha

Calcite has the AST to represent range-predicate conditions, but its
**parser** doesn't yet accept `style(...) and calc(...)` syntax —
only `style(...) and style(...)`. When you emit the new CSS, calcite
will fail to parse it until the parser is extended.

Resolution depends on what Chrome accepts (see Task 1.3 — run the
probe first and document which form works). Expected paths:

- If Chrome accepts bare `calc(...)` in `and` chains: extend
  `calcite::parser::css_functions::parse_style_condition` to also
  accept `calc()` as a leg of the `and` chain, parsing it into
  `StyleTest::Compare`.
- If Chrome requires `= 1` or similar: use that form, and extend
  the parser to recognise it.

This parser extension is genuine calcite-side work. The calcite
branch `claude/bulk-ops-implementation-P9zE7` is where to do it.
Commit on that branch; no need for a new branch.

## Gates — do not proceed past these if red

1. **Task 1.3 Chrome probe**: reads back correct memory from a tiny
   test program. If 256 bytes aren't 0x42 after the fill, stop.
2. **Task 1.5 bootle-ctest still boots**: `fulldiff.mjs` shows no
   divergence in the first 100 ticks against the reference emulator.
3. **Task 1.8 memory hash match**: `probe-splash-project` reports
   `match` not `DIFFER`. After the parser extension is in place.

If any gate fails, report the specific symptom. Don't paper over.

## Ground-truth references (when in doubt)

- **What CSS shape does calcite detect?**
  `../calcite/crates/calcite-core/tests/memory_fill_css.rs` —
  `mk_bulk_fill_cell` builds the canonical AST. If your emitted CSS
  parses to this AST, you're good.
- **What does the contract say about X?**
  `../calcite/docs/superpowers/plans/2026-04-18-bulk-ops-css-dos-contract.md`
  — 11 sections covering every structural requirement, failure mode,
  and checklist item.
- **What's already done on calcite?**
  `../calcite/docs/superpowers/plans/2026-04-18-bulk-ops-pr1-status.md`.

## Verifying calcite picks up your emission

After Task 1.5 (CSS regenerated), run in the calcite repo:

```
cd ../calcite
cargo run --release --bin probe-bytecode-shape
```

Look for a log line like:

```
Recognised bulk-fill binding: kind=--bulkOpKind=1 dst=--bulkDst count=--bulkCount val=--bulkValue range=... (N cells)
```

with `N > 0`. If N = 0, the detector didn't match — your emission
isn't the right shape. Debug against the contract doc section 6.

If the calcite binary fails to even parse the new CSS, that's the
parser-extension problem — expected, not a bug. Extend the parser.

## Workflow expectations

- **Use TDD where the plan specifies it.** Tasks 1.3 (Chrome probe)
  and 1.8 (memory-hash gate) are correctness tests; get them running
  before declaring a task done.
- **Commit frequently on the calcite side** (parser extension, log
  updates). Clear messages: `feat(parser): accept calc() legs in
  style-condition and-chains` and similar.
- **Commit CSS-DOS work locally but do NOT push** without user
  permission. Each CSS-DOS task in the plan has a commit message
  suggestion — use it as a reasonable default.
- **Update CSS-DOS LOGBOOK.md** at the end of each session.
- **Stop and ask** if:
  - A gate fails and you can't diagnose it from the contract doc.
  - Chrome does something unexpected you can't explain.
  - The BIOS build breaks in a way that looks like toolchain drift
    (see `bios/NOTES.md` — OpenWatcom has CRLF sensitivities).
  - You want to push CSS-DOS commits.
  - You want to deviate from the contract's AST shape (it's fine to
    deviate as long as you also update the calcite detector to match;
    but check with the user first).

## First action

1. Read the seven files listed.
2. Report back a one-paragraph summary of what you understand:
   - What CSS shape you'll emit.
   - Where in the transpiler (`transpiler/src/emit-css.mjs`,
     `memory.mjs`, etc.) each piece goes.
   - Whether you expect the parser extension to be needed, and
     which of the two resolutions (bare calc vs `= 1`) you'll try
     first in Chrome.
   - Any concerns with the contract or plan before you start.
3. Wait for user confirmation.
4. Start Task 1.1.

Good luck.
