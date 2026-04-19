# Handoff prompt for cloud agent — bulk memory ops

Copy the text below verbatim as the initial prompt to the cloud agent.
Everything above the horizontal rule is for you (the human); everything
below is for the agent.

---

You are picking up a cross-repo performance project. Two sibling repos:

- **calcite** (your cwd) — Rust JIT for computational CSS.
- **CSS-DOS** (at `../CSS-DOS/`) — 8086 PC implemented in pure CSS, plus
  the transpiler/BIOS that generate the CSS.

## Before you do anything

Read these files in order, in full:

1. `CLAUDE.md` — project rules, cardinal rule, architecture summary.
2. `../CSS-DOS/CLAUDE.md` — CSS-DOS's rules, which are stricter than
   calcite's (git-interaction restrictions, mandatory logbook updates).
3. `../CSS-DOS/docs/logbook/LOGBOOK.md` — current CSS-DOS status.
4. `docs/superpowers/specs/2026-04-18-bulk-ops-design.md` — the design
   spec for what you're building. Read the WHOLE thing. It explains
   why a prior compile-time approach failed and why this one works.
5. `docs/superpowers/plans/2026-04-18-bulk-ops.md` — the task-by-task
   plan you will execute. Contains the required starting state, file
   paths, code snippets, test structure, and commit conventions.

Do not start coding until all five are read. If anything in the plan
references a file path or concept you haven't seen, re-read the relevant
doc before proceeding.

## What you're building (30-second version)

Today, REP STOSB in CSS-DOS emits one byte per calcite tick. A 64 KiB
splash clear takes ~64,000 ticks. The fix: change CSS-DOS to express
bulk memory ops (fill, memcpy, masked blit) as a single CPU instruction
(`INT 2F/AH=FE`) whose CSS evaluates in Chrome by having every memory
cell do a parallel range check. In Chrome, one tick fills 64 KiB
because all ~1M memory-cell properties re-evaluate in parallel. In
calcite, a new CSS-level detector recognises the range-predicate shape
across all memory cells and lowers it to a single `Op::MemoryFill` /
`Op::MemoryCopy` / `Op::MemoryBlitMasked`, executed as one native
`memory.fill` / `copy_within` call per tick.

## Critical rules (from CLAUDE.md files)

- **Chrome is the source of truth.** If Chrome and calcite disagree,
  calcite is wrong. Verify every CSS change in Chrome (Playwright)
  before moving on.
- **No side-channels.** Every property you add to CSS must be consumed
  by Chrome for real work, not just to signal to calcite. The range-
  predicate clause in the plan qualifies because Chrome really does
  evaluate it per cell per tick.
- **No x86 knowledge in calcite.** The detector matches CSS
  *structure* (a conjunction of `style(--bulkOpKind: 1)`, two range
  `calc()` comparisons, and a value reference). It does not know what
  REP STOSB is. It matches a CSS shape.
- **CSS-DOS git rules are strict.** Do NOT commit or push on the
  CSS-DOS repo without explicit user permission. Ask. You may commit
  freely on the calcite repo.
- **Update CSS-DOS's LOGBOOK.md** before stopping any session where you
  did CSS-DOS work. This is mandatory per their CLAUDE.md.

## Scope

Execute **PR 1 only** from the plan (the fill MVP). Do NOT attempt PR
2 or PR 3 without user direction. PR 1 by itself should produce a 10–
100× speedup on splash-fill-dominated workloads and prove the
mechanism end-to-end.

Concretely, PR 1 has 10 tasks (1.1 through 1.10). Execute them in
order. Each task has sub-steps with explicit code snippets, test
structure, and expected outputs. Do not skip correctness gates:

- Task 1.3 (Chrome conformance test) is a hard gate. If Chrome doesn't
  produce the expected memory contents, stop and debug. Don't proceed
  to calcite work.
- Task 1.5 (Chrome still renders bootle.com after regenerating CSS) is
  a hard gate. If the DOS boot broke, stop and investigate.
- Task 1.8 (memory hash matches on probe-splash-project) is a hard
  gate. If `DIFFER` instead of `match`, the calcite-side execution is
  writing wrong bytes.

If any gate fails, report the specific symptom to the user. Do not
paper over it.

## Known tricky bits

- **CSS-DOS big rename (session 11b) happened after the plan was written.**
  See the "2026-04-19 addendum" at the top of `2026-04-18-bulk-ops.md` for
  the path-translation table. In short: `transpiler/` → `kiln/`,
  `bios/css-emu-bios.asm` → `bios/corduroy/handlers.asm`, `generate-dos.mjs`
  → `builder/build.mjs`, memory cells are `--m<addr>` (not `--memByte_<addr>`),
  and the BIOS-reserved bulk-op scratch region must move from `0x500..0x505`
  to `0x510..0x515` (the old range collides with corduroy's rom-disk LBA
  and halt flag).
- **CSS-DOS build**: check `bios/corduroy/README.md` before running any
  build. OpenWatcom has CRLF sensitivities (see recent commit `1679553`).
- **Range-predicate CSS syntax**: `calc(X >= var(--bulkDst))` in a
  CSS `if()` condition — verify early (Task 1.3) whether Chrome needs
  `= 1` appended explicitly. If your first Chrome test shows the
  wrong bytes, this is the likely culprit.
- **Op::MemoryFill already exists** in calcite (commit `d813801`).
  You're not adding it; you're building the CSS-level detector that
  emits it. The op's evaluator unit tests are in
  `crates/calcite-core/tests/memory_fill.rs`.
- **Pre-tick vs bytecode**: the plan executes the bulk fill pre-tick
  (in `Evaluator::tick` before the normal compiled path runs), NOT by
  splicing `Op::MemoryFill` into the bytecode. This is deliberate —
  bytecode-level insertion requires rewriting the broadcast-write
  path, while pre-tick execution coexists with it cleanly.

## Workflow expectations

- **Use TDD** where the plan specifies it (Task 1.6, 1.7). Write the
  failing test first, confirm it fails for the right reason, then
  implement.
- **Commit frequently**, one logical unit per commit. The plan
  specifies commit messages and scope per task. Follow them.
- **Each commit uses the signed-off trailer** the plan shows
  (`Co-Authored-By: Claude ...`).
- **Run `just check` after every task group**. If it goes red, fix
  before continuing.
- **Stop and ask** if:
  - A correctness gate fails and you can't diagnose it from the plan.
  - CSS-DOS or Chrome does something unexpected you can't explain.
  - You need to deviate from the plan in a way that affects scope.
  - You'd need to commit/push on CSS-DOS (always ask first).

## Starting state

Branch `feat/bulk-ops` on the calcite repo (your cwd) contains:
- `Op::MemoryFill` op + evaluator + unit tests (commit `d813801`).
- Diagnostic probe `probe-bytecode-shape` (commit `badb0c7`).
- Updated CLAUDE.md wording (commit `c62b76a`).
- Spec + this plan (commits `e9ff205` and `86725a8`).

On CSS-DOS's `master`, the matching CLAUDE.md wording update is
commit `52d9de2`. Their `dos/config.sys` also has uncommitted changes
from prior sessions — leave those alone.

## First action

1. Read the five files listed at the top.
2. Report back a one-paragraph summary of what you understand and
   any concerns with the plan before you start coding. This is a
   check that you have context, not a gate — just describe what
   you're about to do.
3. Wait for user confirmation.
4. Start Task 1.1.

Good luck.
