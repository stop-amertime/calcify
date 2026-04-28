# Calcite compiler — implementation spec

**Status:** ready to start. **Companion to:** [`compiler-mission.md`](compiler-mission.md)
(strategy / "why") and [`../../CSS-DOS/docs/agent-briefs/doom-perf-mission.md`](../../CSS-DOS/docs/agent-briefs/doom-perf-mission.md)
(the user-facing perf target).

This doc is the "what to build first." It translates the mission doc
into concrete deliverables, file paths, and acceptance gates so the
next agent can open an editor and start typing. **Read
[`compiler-mission.md`](compiler-mission.md) first.** The cardinal
rules there override anything here that drifts.

---

## TL;DR for the next agent

1. The mission doc commits us to four phases. **Only Phase 0 and Phase
   0.5 are scoped here.** Don't start Phase 1 until Phase 0's decision
   gate fires and Phase 0.5's harness is green on Chrome + v1.
2. Phase 0 is a measurement, not a product. You write a hand-derived
   normal form for one hot region, microbench it against today's
   interpreter, and write up whether the ≥10× ceiling is real. You
   throw the code away after.
3. Phase 0.5 is a permanent piece of infrastructure: a CSS-primitive
   conformance suite that runs against Chrome (oracle, via Playwright)
   and the v1 interpreter. Every CSS primitive calcite uses gets at
   least one unit test. This becomes the correctness oracle for every
   later phase.
4. **Do not touch `compile.rs` or `eval.rs` in these two phases beyond
   read-only inspection.** The DAG extraction (Phase 1) is what
   modifies them; landing it before Phase 0 / 0.5 are done means
   building on an unmeasured foundation against an unverified oracle.

If at any point a step here conflicts with the mission doc, the
mission doc wins. If a step contradicts the cardinal rules in
`CSS-DOS/CLAUDE.md` § The cardinal rule, **stop and ask** — those are
the project-level constraints, not negotiable.

---

## Where the existing code lives

Concrete entry points the next agent will read but not modify in
Phase 0 / 0.5. Numbers from the current tree (commit on `main`):

| Concept | File | Anchor |
|---|---|---|
| Parse → `ParsedProgram` | `crates/calcite-core/src/parser.rs` and `parser/` | (top-level) |
| `Expr` IR | `crates/calcite-core/src/types.rs` | `pub enum Expr` line 86 |
| Bytecode `Op` enum | `crates/calcite-core/src/compile.rs` | `pub enum Op` line 110 |
| `CompiledProgram` | `crates/calcite-core/src/compile.rs` | line 440 |
| `compile()` entrypoint | `crates/calcite-core/src/compile.rs` | line 3113 |
| `execute()` (bytecode interp) | `crates/calcite-core/src/compile.rs` | line 4877 |
| Tick loop | `crates/calcite-core/src/eval.rs` | `run_batch` line 1006 |
| State (memory, registers) | `crates/calcite-core/src/state.rs` | |
| Wasm bridge | `crates/calcite-wasm/` | |
| CLI bench harness | `crates/calcite-cli/` (`calcite-bench` binary) | |

The "DAG" Phase 1 will build is *between* `compile()` and `execute()`
— a new module that consumes `Vec<Op>` (or `Expr` directly; decision
deferred to Phase 1) and produces an explicit dataflow graph the
walker / later codegen can chew on. Phase 0 hand-codes the answer the
DAG path *should* produce, without yet building the DAG infrastructure.

---

## Phase 0 — confirm the ≥10× ceiling is real

**Goal.** Pick one hot region. Write — by hand, in idiomatic Rust —
the code a perfect compiler would emit for that region. Microbench
it against the current interpreter on identical input state. Decide
whether to commit to the road.

**Budget.** 2–4 days of work. If you're past day 5 still wiring the
microbench together, stop and ask — the harness should not be the
hard part.

**Decision gate (mission doc § Phase 0).**
- ≥10× over interpreter on the chosen region → commit to the road,
  proceed to Phase 0.5.
- 3–10× → road is viable but the 30–100× ceiling estimate is generous;
  recalibrate expectations in the logbook before proceeding.
- <3× → abandon the compiler road; the interpreter peephole road is
  the better bet. Write up *why* in the logbook (op-mix breakdown,
  what's left on the table, what the ceiling actually looks like).

### 0.1 — Pick the hot region

The mission doc names two candidates from the existing profiler
output (see `docs/log.md` and `docs/benchmarking.md`):

- **Segment 0x55** — ~67.8 % of level-load CPU on Doom8088. Largest
  single target.
- **Segment 0x2D96** — BIOS dispatch, ~15 %. Smaller and more uniform;
  may be easier to hand-derive cleanly.

Pick one. Document the pick + reasoning in the logbook. Confirm with
fresh `calcite-bench --profile` numbers from the current `main` that
the segment is still hot (these numbers are months old; they may have
shifted).

### 0.2 — Capture an input snapshot

The microbench needs a deterministic starting state. Use the existing
snapshot machinery:

- `State::snapshot` / `State::restore` in `crates/calcite-core/src/state.rs`.
- Run the cabinet to a tick where the chosen segment is the next
  thing about to execute (use `calcite-debugger` or `calcite-cli
  --dump-ticks` to find the right tick — see `docs/debugger.md` and
  `docs/benchmarking.md`).
- Save the snapshot as a fixture under
  `crates/calcite-core/benches/fixtures/phase0/<region>.snapshot.bin`
  (or wherever the existing Criterion benches keep their inputs —
  follow the established pattern, don't invent a new one).

The snapshot captures full state (memory + registers + state-vars).
Both the interpreter run and the hand-derived run start from the same
restore.

### 0.3 — Hand-derive the normal form

Read the bytecode (`Op` stream) for the chosen segment using
`calcite-debugger`'s disassembly endpoint or by adding a temporary
`println!` in `compile()` that dumps ops for a named segment. **Do
not commit the dump tooling unless you intend it as a permanent
debugger feature.**

Write the equivalent Rust code as a free function:

```rust
// crates/calcite-core/benches/phase0_<region>.rs (new bench file, throwaway)
fn handcoded_<region>(state: &mut State) { /* ... */ }
```

Constraints on the hand-coded version:
- Reads from and writes to `State` via the same `read_mem` / `write_mem`
  / state-var APIs the interpreter uses. The "compiler" being simulated
  doesn't get to invent new state primitives.
- Inlines, constant-folds, and dead-code-eliminates aggressively. This
  is what the compiler *should* be able to do; you're proving the
  ceiling, so be greedy. Document each non-obvious folding step in a
  comment so a reviewer can sanity-check that the transformation is
  semantics-preserving.
- Must produce **bit-identical state** to the interpreter when run
  from the same snapshot for one tick of that segment. Add an
  assertion-mode comparison (described in 0.4) before you trust any
  speed number.

### 0.4 — Microbench harness

One Criterion bench (`criterion` is already a workspace dep):

```
crates/calcite-core/benches/phase0_<region>.rs
```

Two groups, same input snapshot:

1. `interpreter` — restore snapshot, call `Evaluator::run_batch(state, 1)`
   limited to the chosen segment. (If "limit to one segment" is
   awkward in the existing tick loop, instead measure full ticks and
   subtract the rest — but be honest about the methodology in the
   write-up.)
2. `handcoded` — restore snapshot, call `handcoded_<region>(state)`.

Run with `cargo bench -p calcite-core --bench phase0_<region>`.
Median of three full bench invocations, on a cooled machine; thermal
throttling is a known noise source on this laptop (see `docs/log.md`
2026-04-16 entry).

**Conformance check.** Before benching, in a separate `#[test]`:
restore snapshot → interpreter run → snapshot state A. Restore again
→ handcoded run → snapshot state B. `assert_eq!(A.state_vars,
B.state_vars)` and `assert_eq!(A.memory[..], B.memory[..])`. If this
fails, fix the handcoded version before reading speed numbers — a
fast-but-wrong handcoded version is worse than no measurement.

### 0.5 — Write up the result

Logbook entry in `../CSS-DOS/docs/logbook/LOGBOOK.md` (project source
of truth — mission doc § Logbook discipline). Includes:

- Region picked and why.
- Snapshot tick and how to reproduce it.
- Speedup ratio (median of three).
- Op-mix breakdown of the segment (which ops dominate; this informs
  Phase 1's DAG-node vocabulary).
- One-paragraph honest read on whether the ≥10× gate fired.
- Decision: proceed / recalibrate / abandon, with rationale.

Commit the bench file (so future agents can re-run the measurement)
but mark its docstring clearly: "Phase 0 ceiling probe — not part of
the compiler. Throwaway after Phase 3 lands."

### 0.6 — What success looks like

The next agent picking up Phase 1 can read the logbook entry and
know:
1. The road is worth taking (gate fired).
2. The op-mix they'll be lowering (informs the DAG vocabulary).
3. A reproducible benchmark to compare Phase 1's DAG walker against
   later (it should match the interpreter's number; the speedup
   doesn't come until Phase 3).

---

## Phase 0.5 — CSS-primitive conformance suite

**Goal.** Build a unit-test harness that runs hand-authored `.css`
snippets against Chrome (the oracle) and the v1 interpreter (the
incumbent), and asserts they produce the same computed values. Each
snippet exercises one CSS primitive in isolation. The harness becomes
the correctness oracle for every later phase — Phase 1's DAG walker,
Phase 2's normaliser, and Phase 3's codegen all run against this
suite, and a primitive-level divergence shows up here as a localised
unit-test failure rather than a 50M-tick mystery.

**Budget.** 1–2 weeks. The deliverable is the harness + an initial
suite covering the primitives calcite *actually uses today* (which is
itself an artifact — a one-page enumeration is part of the deliverable).

**Decision gate.** Suite passes on Chrome + v1. If v1 fails any
primitive, that's a pre-existing cardinal-rule violation (calcite
disagrees with Chrome on a primitive); document it, stop, and route
through the project owner before proceeding.

### 0.5.1 — Enumerate the primitives

Read [`crates/calcite-core/src/types.rs`](../crates/calcite-core/src/types.rs)
(the `Expr` enum) and [`crates/calcite-core/src/parser/`](../crates/calcite-core/src/parser/)
end-to-end. Enumerate, with examples, every CSS primitive that can
appear in calcite's input:

- `var()` — basic resolution, fallback, undefined → `initial-value`
  from `@property`.
- `calc()` — every operator that lands in `CalcOp` (add, sub, mul, div,
  mod, neg, abs, sign, min, max, clamp, round (every strategy), pow,
  floor, etc.). Edge cases: integer overflow, division by zero,
  rounding modes (`up` / `down` / `nearest` / `to-zero`), negative
  modulo (CSS spec is specific here), `NaN` and `infinity` inputs.
- `if(style(--p: v): then; else: ...)` — single test, compound
  `and`/`or`, fallback chain, no-match fallback.
- `@function` calls — single param, multi param, locals, nested calls,
  recursion (calcite supports it? check before assuming).
- String primitives — `--i2char` style display functions, `Concat`
  expressions, string literals.
- `@property` semantics — `syntax`, `initial-value`, `inherits` (does
  any of this matter for calcite's evaluation? document either way).
- Cascade ordering — does calcite preserve declaration order on
  conflicting assignments to the same custom property? Chrome does;
  build a test that cares.

Output: `docs/css-primitives.md` — one page, each primitive named,
example snippet, what calcite is supposed to do with it. This doc
*bounds the scope* of what Phase 1 has to lower.

### 0.5.2 — Harness shape

Two execution paths, same input file, results compared:

```
tests/conformance/primitives/
  <primitive>.css        # the snippet
  <primitive>.expect.json  # expected post-evaluation values
```

Each `.css` file is a complete, valid CSS file with one or more
`@property` declarations and one or more `:root`-scoped assignments.
The `.expect.json` lists `{ "--prop": <value>, ... }` for the
properties under test.

**Chrome path (oracle):**
- Playwright headless. Launch a blank page, inject the `.css` via
  `<style>`, read computed values via `getComputedStyle(document.documentElement).getPropertyValue('--prop')`.
- The harness lives in JS/TS to call Playwright. Suggested location:
  `tests/conformance/run-primitives.mjs` (similar to existing
  CSS-DOS conformance harness shape — see
  `../CSS-DOS/tests/harness/`).
- One Playwright launch, all snippets run against it (don't pay
  browser-launch cost per test).

**v1 calcite path (incumbent):**
- Direct Rust call. Parse the CSS, run one tick (or however many
  ticks the snippet specifies via a comment header — most primitives
  resolve in one tick; some `@function` recursion tests may need
  more), read computed property values.
- Suggested location: `crates/calcite-core/tests/primitive_conformance.rs`
  (a Cargo integration test that walks the same fixture directory
  the JS harness uses).

**Comparison:** the JS harness writes a `chrome.actual.json`; the
Rust test reads its own results. A small `compare.mjs` (or build it
into the JS harness) diffs Chrome's actual against the `.expect.json`
*and* against `v1.actual.json` (produced by the Rust test running
first and dumping its results). All three must agree.

### 0.5.3 — Initial test set

Cover, at minimum:

- Each `CalcOp` variant — one happy-path test, one edge-case test
  (overflow / div-zero / negative-mod / rounding / etc.).
- `var()` resolution: defined, undefined+fallback, undefined+no-fallback,
  recursive `var()` chain.
- `@property` `initial-value` honoured when the property isn't
  assigned.
- `if(style(...))` — single, compound `and`, compound `or`, multi-branch
  cascade, no-match fallback.
- `@function` — identity, single-arg, two-arg, nested call, the
  bit-decomposition shape calcite already recognises (sanity-check
  the recogniser produces Chrome-equivalent results).
- One `Concat` / string-formatting test.
- One cascade-order test (later declaration wins).

Aim for ~30–50 unit tests in the initial suite. More can be added
as bugs are discovered.

### 0.5.4 — Wire it into CI

`just check` (or whatever the workspace test entry point is — see
`crates/calcite-core/CLAUDE.md` if it exists, otherwise the workspace
root) should run the v1 path automatically. The Chrome path needs
Playwright installed; gate it behind an env var or a separate
`just conformance` target so contributors without Playwright don't
hit a hard failure on every test run, but make sure it runs in CI.

### 0.5.5 — Process commitment (forever rule)

From mission doc § Phase 0.5:

> The suite earns its keep only if every compiler bug found via Doom
> gets a unit test added before the fix lands.

Land this as a sentence in the suite's `README` and as a checklist
item in any future phase's deliverables. Without that discipline the
suite rots. This is more important than the initial coverage —
incomplete coverage gets fixed over time; a rotten harness gets
ignored.

### 0.5.6 — What success looks like

- Suite runs green against Chrome + v1.
- `docs/css-primitives.md` enumerates the input vocabulary Phase 1
  has to lower, with one example per primitive.
- Logbook entry confirming the gate fired, and listing any v1 ↔ Chrome
  divergences found (if any — these are bugs to file, not blockers
  for moving to Phase 1, unless they're so pervasive that v1 isn't
  trustworthy as a development sanity check).

---

## What Phase 1 will need from us (forward-looking notes, do not implement)

Listed here so Phase 0 / 0.5 work doesn't accidentally close doors
Phase 1 needs open. Everything below is a Phase 1 concern; mentioned
only so the spec's reader knows what's coming.

- A new module `crates/calcite-core/src/dag.rs` (name TBD) that
  builds the DAG. Likely consumes `Vec<Op>` from existing `compile()`
  rather than going back to `Expr`, because `Op`-level dataflow is
  already linearised — but the call belongs to Phase 1's author
  after they've read both representations.
- A flag on `Evaluator` to switch between bytecode and DAG-walker
  backends. Both must coexist during validation.
- The Phase 0.5 harness needs to grow a *third* execution path
  pointing at the DAG walker — script the harness now so adding a
  third target is mechanical, don't hardcode "Chrome vs v1."
- Snapshot compatibility (mission doc § Cross-cutting). Phase 1
  changes slot allocation (or eliminates it); existing snapshots may
  invalidate. Phase 1's PR includes either "still valid" justification
  or a snapshot-version bump.
- Both targets must agree (mission doc § Both targets must agree):
  Phase 1 PRs run `bench-doom-stages.mjs` and `bench-doom-stages-cli.mjs`
  and report both, not just one.

---

## What this spec deliberately does not say

- **Which codegen target Phase 3 picks.** Decided at Phase 3 entry,
  informed by what 0–2 learned (mission doc § Phase 3, open question
  #2). Don't pre-commit.
- **The Phase 2 idiom list.** Mission doc § Phase 2 is explicit: the
  list is *itself* the first deliverable of Phase 2, derived from
  reading `kiln/emit-css.mjs`, not pre-committed in any spec. Listing
  hypothetical idioms here would seed exactly the bias the cardinal
  rule (rule 1, generality) tells us not to seed.
- **A timeline.** Phase budgets are in the mission doc; calendar
  scheduling is a project-management call, not a spec call.
- **Whether v1 is removed at the end of Phase 3.** Mission doc § Non-
  goals + § Phase 3 say v1 stays as a conformance backstop / debug
  tool but not as a maintained peer execution path. The exact
  retirement mechanic is a Phase 3 deliverable.

---

## Pre-flight checklist for the next agent

Before opening an editor:

- [ ] Read [`compiler-mission.md`](compiler-mission.md) top to bottom.
- [ ] Read [`../../CSS-DOS/CLAUDE.md`](../../CSS-DOS/CLAUDE.md) § The
      cardinal rule (you are about to be tempted to violate it).
- [ ] Read [`../../CSS-DOS/docs/logbook/LOGBOOK.md`](../../CSS-DOS/docs/logbook/LOGBOOK.md)
      for current state of the world.
- [ ] Read [`docs/log.md`](log.md) for the existing perf profile and
      the lessons baked into the last round of optimisation work
      (especially the 2026-04-16 entry on thermal noise and the
      dense-array DispatchChain bug).
- [ ] Run `just check` (or `cargo test --workspace && cargo clippy
      --workspace`) on a clean tree to confirm baseline green.
- [ ] Run `cargo bench -p calcite-core` (with the existing fixture)
      to confirm baseline numbers and get a feel for the noise floor
      on your machine.
- [ ] Pick Phase 0 OR Phase 0.5 to start. They're parallelisable —
      different parts of the tree, no shared mutable state — but
      the gate ordering (Phase 0 first if you want to know whether
      to bother with 0.5) usually points to Phase 0 first.
- [ ] Open a logbook entry in `../CSS-DOS/docs/logbook/LOGBOOK.md`
      for "Calcite compiler — Phase 0 [or 0.5] starting" with the
      planned approach. Update it as work lands.

When in doubt, the mission doc's cardinal rules win, and "stop and
ask" is always a valid option.
