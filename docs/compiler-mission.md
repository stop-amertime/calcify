# Calcite compiler — mission and plan

**Status:** proposed (not started). Supersedes the peephole-fusion road
for ongoing perf work, on the grounds described in [§ Why now](#why-now).
Living doc — update as phases land or as decision gates redirect the
plan. The truth-of-current-state is the LOGBOOK; this doc is the
direction of travel.

**Audience:** worker agents picking up calcite perf work, and anyone
deciding whether a proposed calcite change belongs on this road or on
the legacy interpreter road. Read top to bottom before touching code.
The cardinal rules below override your generic instincts.

**Companion doc:** [`../../CSS-DOS/docs/agent-briefs/doom-perf-mission.md`](../../CSS-DOS/docs/agent-briefs/doom-perf-mission.md)
defines the user-facing perf target this work is in service of. This
doc defines the *strategy* for hitting it.

---

## One-line thesis

The CSS calcite evaluates is a static pure-functional dataflow graph.
Compile it once at load time, don't interpret it every tick.

## Why now

The peephole-fusion road has diminishing returns. The doom-perf brief
identifies >60 % of executed ops as unfused load-then-compare-then-branch
chains; the existing fused `LoadStateAndBranchIfNotEqLit` only catches
0.7 %. The natural reaction is to add more fused ops. After ~5–10 of
those the engineering cost compounds (one Op variant + eval arm +
peephole matcher + profile arm + snapshot compatibility per shape) and
the tail of un-recognised shapes still dominates.

Underneath the peepholes is a more honest observation: the CSS that
calcite evaluates is a **static pure-functional dataflow graph**. No
mutation mid-tick, no runtime branching of the dispatch graph itself,
no aliasing — every "tick" is one DAG evaluation pass over a structure
that's known after parse and never changes during a run. That regime
is the easiest possible case for a compiler. Interpreting it pays
dispatch cost on every op, every tick; compiling it once at load and
running the compiled form pays dispatch cost zero times after compile.

Quantitative ceiling estimate (informal — see Phase 0 for the
empirical version): eliminating dispatch (~3–5×), whole-DAG inlining
and constant-folding (~2–5×), vectorisation of broadcast/packed
writes (~2–8×), and eliminating per-tick live-slot scheduling
(~1.5–3×) compound to a 30–100× ceiling on level-load throughput.
First-cut compiler quality realistically captures 5–15× of that. The
doom-perf headline target (≥30× on `runMsToInGame`) sits inside
first-cut territory.

The peephole road probably plateaus at 2–4×. The compiler road has a
ceiling 10× higher and the work compounds rather than fragments.

## Cardinal rules

These inherit from CSS-DOS's CLAUDE.md and sharpen them for this
project. If a proposed change conflicts with one of these in spirit,
stop and ask before shipping.

### 1. Calcite knows nothing above the CSS layer

Already in `CSS-DOS/CLAUDE.md` § The cardinal rule. Restated here
because it's the rule this project is most likely to violate by
accident.

Every recogniser, rewrite rule, codegen path, and optimisation must
reason about CSS structural shape and nothing else. Never x86, BIOS,
DOS, Doom, a specific cabinet's addresses, Kiln's current emit
choices, what `program.json` says, what the cart is *trying to do*.

**Operational test:** could a calcite engineer who has never seen a
CPU emulator, never read Kiln's source, and doesn't know what a
cabinet contains, derive this rule by staring at CSS shape alone? If
yes, fair. If no, it's overfit.

**Genericity probe:** would the same rule fire on a 6502 cabinet, a
brainfuck cabinet, a non-emulator cabinet whose CSS shares the
structural shape? If no on all three, you've encoded a specific
program into calcite.

### 2. The workflow is sacred — load-time compilation only

Calcite must accept any spec-compliant `.css` cabinet at load time and
make it fast — in the browser, on the user's machine, with no build
step on the cabinet author's side, no pre-baked artifact, no
allowlist, no asset pipeline. "Open a `.css` URL, it runs" is the
contract.

Compile-once-per-load, run-many is allowed and is how the speedup
happens. Distributing pre-compiled cabinets is **not** allowed — that
breaks the contract. Caching compiled output for a session is fine;
shipping it as the artifact is not.

### 3. Ground truth is Chrome / the JS reference, not the v1 interpreter

A spec-compliant CSS evaluator (Chrome, and the JS reference emulator
under `conformance/`) is the ultimate ground truth — that's the
cardinal-rule contract. The v1 bytecode interpreter is a *faster
proxy* for that ground truth, useful for differential testing during
Phase 2/3 because running it is cheap. It is **not** itself a forever-
contract.

The compiler ships standing alone. Once Phase 3 covers the DAG
vocabulary, calcite v2 is the only execution path; v1 stays in the
tree as a conformance backstop and as the interpreter you boot up
when something looks wrong, not as a peer implementation that every
cross-cutting change has to land twice.

Bit-identical results vs v1 are a cheap differential test during
development. The Phase 0.5 primitive-conformance suite (Chrome as
oracle, run via Playwright `getComputedStyle`) is the *direct* test
of the correctness contract — it catches at primitive granularity
exactly the divergences whole-cabinet tests can only catch
transitively.

### 4. Generality before performance

A compiled path that produces a 50× win on Doom by recognising a
Doom-shaped pattern is worse than an interpreted path. The
genericity probe (rule 1) gates everything before the perf bench
does.

## Success criteria

Outcome-shaped. The ways an optimisation can be correct are countable;
the ways it can be wrong are not.

- **Headline:** `headline.runMsToInGame` < 10 s on web and CLI on the
  current Doom8088 cabinet, measured by `bench-doom-stages.mjs` and
  `bench-doom-stages-cli.mjs`. Median of three runs each.
- **Playability floor:** steady-state Doom8088 in-game framerate ≥ 10
  fps. Currently ~0.2 fps. This is *the* user-facing test of whether
  the compiler did its job.
- **Compile-time budget:** page-load to first in-game frame fits in
  the user-patience window (target ≤60 s, soft). The compile/run
  tradeoff is a knob within this constraint, not a fixed parameter —
  if a 45 s compile gets 15 fps it's a win; a 5 s compile that gets
  4 fps is a fail.
- **No regressions:** smoke suite (`tests/harness/run.mjs smoke`)
  bit-identical pass. Conformance harness pass. No cabinet that
  works today fails after.
- **Generality preserved:** a non-Doom cabinet with similar
  structural shape (Zork, Prince of Persia, microbench carts) shows
  proportional speedup or no regression. If only Doom moves, the
  recognisers are overfit and the rule-1 probe is failing.

The bench JSON is the evidence. "Felt faster" is not. One-shot
measurements are not (variance is real).

## Non-goals

- **Not a tier-up JIT.** Calcite's input is static once loaded — no
  runtime tier-up, no on-stack replacement, no deopt, no profiling-
  guided recompilation. One compile pass at load, then run.
- **Not pre-baked AOT distribution.** Forbidden by rule 2.
- **Not a peer implementation alongside the interpreter forever.**
  The compiler's endgame is to stand alone — v1 stays available as a
  conformance backstop and a debug tool, but it is not maintained as
  a parallel execution path that every cross-cutting change has to
  land twice. Phase 3 may temporarily fall back to the interpreter on
  un-recognised shapes; the goal is to retire that fallback by the
  end of the phase.
- **Not Doom-specific.** Forbidden by rule 4. Doom is the headline
  metric because it's the slowest interesting cabinet, not because
  the compiler is built for it.
- **Not a CSS-DOS-only thing.** Calcite is a CSS JIT. Anything in this
  project that bleeds CSS-DOS-specific assumptions into calcite-core
  is wrong.

## Plan

Phased with explicit decision gates. Each phase has a budget; if a
phase doesn't show its expected win after ~2× the estimated time,
stop and re-examine the model rather than push through. Same rule
the doom-perf brief already imposes for changes that move <5 % when
20 %+ was predicted.

Per-phase deliverables: code, smoke-suite pass, bench JSON before and
after, logbook entry with honest read on whether the phase paid off.

### Phase 0 — confirm the ceiling is real

**Cost:** ~days. **Decision gate:** does hand-derived normal form on a
hot region beat the interpreter by ≥10×? If yes, commit to the road. If
3–10×, road is viable but the ceiling estimate is generous; recalibrate.
If <3×, abandon and go back to peepholes.

- Pick the segment-0x55 hot region (67.8 % of level-load CPU) or the
  segment-0x2D96 BIOS dispatch (15 %). Either works as a sample.
- Hand-derive the normal form by inspection — what would the
  compiler emit if it understood this region perfectly? Write it as
  a Rust function or a wasm snippet.
- Microbench against the current interpreted path on a snapshot-
  restored window. Same input state, same output state, measure
  ticks/sec.
- The Phase 0 artifact is *not* shipped. It's a measurement to
  decide whether Phase 1 is worth starting.

### Phase 0.5 — CSS-primitive conformance suite

**Cost:** ~2–4 peephole-equivalents (1–2 weeks). **Decision gate:**
the suite passes on Chrome (oracle) and v1 (incumbent). If v1 fails
any primitive, that's a pre-existing cardinal-rule violation and
needs investigating before Phase 1 starts.

Today the cardinal rule "calcite produces same results as Chrome" is
enforced only transitively — by running whole cabinets through both
and comparing framebuffers. That catches divergence; it does not
*localise* it. A primitive bug surfaces as a 50M-tick mystery, not as
a failing unit test. And it leaves the rule technically uncheckable
at primitive granularity: if calcite's `calc()` rounds negative-half
differently from Chrome, the existing tests don't see it unless a
cabinet happens to hit that case.

This phase builds a small, hand-authored unit suite of `.css` snippets,
each exercising one CSS primitive (`var()` resolution, `calc()`
arithmetic, cascade ordering, `:has()`, attribute selectors,
conditional matching, value coercions, edge cases like NaN /
infinity / overflow, rounding modes). Each test:

- A few lines of CSS.
- An expected post-evaluation state (one or two property values).
- Runs against Chrome (Playwright headless `getComputedStyle`) and
  against v1 calcite. Both must agree.
- Later: also runs against v2 (the compiler), which is how Phase 1+
  prove primitive-level correctness without needing v1 as oracle.

**Why before Phase 1.** Phase 1's validation harness needs a
correctness oracle that is *not v1*. If we validate Phase 1 against
v1, we entrench v1's role rather than displace it (which rule 3
explicitly says we won't). Building the conformance suite first means
Phase 1's "is the DAG-walker correct?" question is answered against
Chrome-via-units, with v1 as a sanity check — not the other way
around.

**Process commitment.** The suite earns its keep only if every
compiler bug found via Doom gets a unit test added before the fix
lands. Without that discipline the suite rots. This is a phase
deliverable in spirit but a forever rule in practice.

**Deliverables:** the suite itself; the harness that runs it against
Chrome + v1 (later v2); a one-page enumeration of which CSS
primitives calcite actually uses (this is itself useful — it bounds
the scope of what v2 has to lower).

### Phase 1 — DAG extraction

**Cost:** ~1–2 weeks. **Decision gate:** Phase 0.5's primitive
conformance suite passes on the DAG walker. Smoke suite still bit-
identical when DAG-walked instead of bytecode-interpreted. No perf
gain expected — this is foundation.

- After parse, build an explicit dataflow DAG from the bytecode IR.
  Nodes are pure expressions (load, lit, arithmetic, compare,
  conditional select, slot-write); edges are data dependencies; the
  DAG is per-tick (one DAG, evaluated once per calcite tick).
- Implement a trivial DAG walker as an alternative evaluation
  backend. Identical behaviour to the bytecode interpreter; switch
  via flag.
- Validate: smoke suite produces bit-identical state under DAG
  walker vs interpreter. bench-doom-stages numbers within noise (no
  perf gain or loss expected — same work, different shape).

The DAG is the substrate every later phase operates on. Get this
right; later phases are cheap. Get this wrong; later phases are
impossible.

### Phase 2 — bottom-layer recognisers

**Cost:** ~2–3 weeks. **Decision gate:** rewriting the DAG into
normalised form keeps bit-identical semantics and noticeably shrinks
DAG size on Doom (target: ≥30 % node count reduction).

The bottom-layer idiom list is itself the first deliverable of this
phase, **not pre-committed in this spec**. The work starts with
reading `kiln/emit-css.mjs` end-to-end, enumerating the structural
idioms CSS forces emitters into (broadcast write, packed-cell write,
conditional cascade, paragraph-relocate cascade, applySlot cascade,
etc.), and producing the canonical list as a doc artifact. Pre-
committing a list I haven't verified would be the "saying 'that's it!'
then realising you were wrong" failure mode the CSS-DOS CLAUDE.md
warns against.

For each idiom:
- Write a shape-matcher over the DAG.
- Define a normalised replacement node (a single super-op that
  captures the idiom's semantics, parameterised by the literals the
  matcher extracts).
- Prove genericity: would this matcher fire on a non-CSS-DOS cabinet
  with the same structural shape? If no, it's overfit.

Validation: smoke suite + Doom level-load produce bit-identical
state under normalised DAG vs raw DAG. Node-count reduction is the
intermediate metric; runtime perf stays interpreter-bound until
Phase 3.

### Phase 3 — codegen

**Cost:** ~3–6 weeks. **Decision gate:** ≥5× speedup on Doom level-
load vs interpreter baseline. If <3×, codegen quality is the
problem; profile and figure out what's leaving cycles on the table
before adding more recognisers.

The codegen target is decided at the start of this phase, informed by
what Phase 0–2 learned. Three plausible options on the table:

- **(a) Hand-emit wasm** in calcite-core. Browser runs it natively;
  calcite-cli runs it via embedded Wasmtime. Single backend, both
  targets, smaller artifact than (b).
- **(b) Cranelift IR.** Lowers to native (calcite-cli) or wasm
  (browser). More flexible, heavier dependency.
- **(c) Specialised Rust closures.** No real codegen — generate a
  tree of `Box<dyn Fn>`s from the normalised DAG. Cheapest to
  prototype; ceiling 3–5× rather than 30–100×. Useful as a
  validation step before committing to (a) or (b).

Recommended sequence: try (c) first as a one-day prototype to
validate the lowering shape. Then commit to (a) for the real
implementation. Cranelift (b) only if (a) hits a wall.

During Phase 3 the compiler may fall back to the v1 interpreter on
un-recognised shapes — that's how partial Phase 3 ships. Mixed-mode
evaluation in this transitional state must produce bit-identical
results to v1-only. The phase is not finished until every shape that
appears in the smoke suite + Doom is compiled directly and the
fallback is dead code; v1 then stays in the tree as conformance
backstop only (rule 3), not as a runtime peer.

### Phase 4 — composition and higher-layer recognition

**Cost:** open. **Decision gate:** higher-order patterns over the
normalised DAG yield meaningful additional speedup beyond Phase 3.

Once the normalised DAG exists (Phase 2's output), look for
**higher-order patterns** — patterns over already-canonical shapes,
not over raw CSS. The hypothesis from the workshopping that
produced this spec: the CSS solution space is small enough that
once bottom-layer idioms are normalised, higher layers compose from
a small finite set of normalised primitives, and those compositions
should be recognisable mechanically rather than by hand-enumeration.

This phase is partly research — the question "does this layer
exist and pay out, or is most of the win already captured in
Phase 3" is one we can't answer until Phase 3 lands. If Phase 3
hits the headline target, Phase 4 may be unnecessary.

### Phase 5 — equality saturation (only if needed)

**Cost:** open. **Decision gate:** Phase 4 produces real evidence
that rewrite rule order changes the final compiled output by more
than noise.

Replace the greedy rewriter with an e-graph + extraction step. The
egg/egglog research line is the obvious starting point. **Only do
this if Phase 4 produces evidence that rule ordering matters** —
e-graphs have their own complexity cost (rule-set blowup, extraction
heuristics) and are not free. Don't pre-build them.

## Cross-cutting concerns

These apply to every phase, not just one:

### Snapshot compatibility

`State::snapshot` / `State::restore` are tied to slot allocation,
which any compiler-side change can perturb. The contract: snapshots
are valid for a single (cabinet, calcite-build) pair. Each phase
must specify what happens to existing snapshots — either "still
valid" (rare) or "invalidated, regenerate." The harness needs a
post-restore phash sanity check; if it doesn't have one yet, that's
a Phase 1 prerequisite.

### Both targets must agree

The CSS-DOS contract: calcite-cli, calcite-wasm, and Chrome produce
identical results. A compiler change that helps one and regresses
another is a bug, not a tradeoff. Run both `bench-doom-stages.mjs`
and `bench-doom-stages-cli.mjs` after every phase. Disagreement >10 %
on a non-throughput metric is an investigation, not noise.

### Memory packing alignment

The "pack=2 + 3 word write-slots" work in
[`CSS-DOS/docs/logbook/LOGBOOK.md` § Open work](../../CSS-DOS/docs/logbook/LOGBOOK.md)
is *complementary* to this project — smaller, denser, more uniform
DAG is easier to compile. If pack=2 lands first, the compiler chews
on a tidier input. If the compiler lands first, pack=2 still
benefits the transitional v1-fallback path during Phase 3 and the
v1 backstop afterward. No sequencing conflict; pick
based on which lands cleanest.

### Logbook discipline

Each phase boundary gets a logbook entry in
`CSS-DOS/docs/logbook/LOGBOOK.md` (the project source of truth) with:
- bench JSON before and after (median of 3),
- node-count / op-count diffs where relevant,
- smoke suite pass confirmation,
- one-paragraph honest read on whether the phase paid off.

If a phase missed its decision gate, the logbook entry says so
plainly and the next phase doesn't start until the gap is
understood.

## Open questions to resolve

These are flagged so they're not silently assumed:

1. **Cardinal-rule check on codegen.** Does "compile calcite IR to
   wasm/native and run that" pass the cardinal rule? The CSS file
   on disk is unchanged and Chrome-evaluable; calcite already
   parses to an internal IR; the new step changes only how the IR
   is *executed*. Reading of the rule that produced this spec:
   yes, this passes. Final call: project owner.
2. **Which Phase-3 codegen target.** Spec recommends (c)→(a) but
   doesn't pre-commit. Phase 0 and Phase 2 results inform the
   decision.
3. **Phase 2 idiom list.** Deliverable of Phase 2's first week, not
   pre-committed here.
4. **Calcite-cli execution story.** If Phase 3 goes (a) (hand-emitted
   wasm), calcite-cli embeds a wasm runtime (Wasmtime or similar).
   Cost: dependency weight, compile time. Going (b) (Cranelift) gets
   calcite-cli native codegen for free. Going (c) (Rust closures)
   needs neither. Decided at Phase 3 entry alongside the codegen-
   target choice. Note: under rule 3, "calcite-cli stays interpreter-
   only" is *not* an acceptable long-term answer — both targets must
   be standalone-compilable, which is part of why hand-emitted wasm
   is the recommended path.

## Reference

- [`CSS-DOS/CLAUDE.md`](../../CSS-DOS/CLAUDE.md) § The cardinal rule —
  the constraint this project lives under.
- [`CSS-DOS/docs/agent-briefs/doom-perf-mission.md`](../../CSS-DOS/docs/agent-briefs/doom-perf-mission.md) —
  the user-facing perf target.
- [`CSS-DOS/docs/logbook/LOGBOOK.md`](../../CSS-DOS/docs/logbook/LOGBOOK.md) —
  current state of the world; updated per phase.
- [`optimisation-ideas.md`](optimisation-ideas.md) — the peephole
  road's catalogue. Some ideas there subsume into this project as
  bottom-layer recognisers; others remain interpreter-only wins.
- [`benchmarking.md`](benchmarking.md) — calcite throughput numbers,
  Chrome comparison, what "fast" means in calcite-land.
