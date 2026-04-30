# calc(ite)

A load-time compiler for computational CSS. Calcite is a JIT for CSS:
parses a `.css` cabinet, recognises patterns, and evaluates them orders
of magnitude faster than Chrome would — same results, faster.


## STOP — read this first, every session, no exceptions

**You MUST read `../CSS-DOS/CLAUDE.md` before doing anything else in
this repo.** It owns the rules that govern both repos: the cardinal
rule, the checkpoint system, coding guidelines, git/collaboration
rules, documentation rules, debugging rules, vocabulary (cart, floppy,
cabinet, Kiln, builder, BIOSes, player), the conformance harness, the
2-minute wall-clock cap, NASM path, `CALCITE_REPO` worktree handling,
and `tests/harness/` tooling.

Those rules are NOT repeated below. If you skipped CSS-DOS's CLAUDE.md
and are looking for any of them here, go back and read it. This file
covers only what's calcite-specific.

@docs/log.md

`ls docs/` for the rest. `docs/log.md` is the perf logbook with
current bottleneck analysis and recent optimisation work — read before
any perf work. `docs/benchmarking.md` is the full `calcite-bench`
reference. `docs/debugger.md` covers the HTTP debug server.
`docs/v2-rewrite-design.md` and `docs/v2-self-review-2026-04-29.md`
cover the v2 DAG walker.


## Web is the only delivery method

`calcite-wasm` in the browser is the product. `calcite-cli` is a test
harness. **Anything that works in calcite-cli but not in calcite-wasm
is broken**, and any code path that exists in calcite-cli but not in
calcite-wasm (sidecar files, native-only state setup, fs reads at
startup) is a bug to be removed, not a feature to preserve. Route the
same logic through compile-time data on `CompiledProgram` (which both
targets share), or delete it.

If you reach for `#[cfg(not(target_arch = "wasm32"))]` for anything
beyond what the wasm-safety section permits (timing, threads, fs/net),
stop and find a different design. Both targets must produce identical
results from the same cabinet.


## Performance and conformance are measured on real carts

Boot DOS or a real game. **Always.** Micro-fixtures (`hello-text.css`,
primitives, anything in `tests/conformance/`) can pass with enormous
structural bugs latent (broadcast spillover, huge dispatch chains,
function inlining cascades, slot-cache cross-tick interactions) and
can show flattering speedups that vanish on real workloads. **A perf
claim with no real-cart number behind it is not a claim.** Never
write micro-fixture numbers into `docs/log.md` as results.

The benchmark of record is `../CSS-DOS/player/bench.html`, driven
headless by `node ../CSS-DOS/tests/harness/bench-web.mjs`. It builds
the cabinet in-browser via the same path the site uses, so it measures
the engine the user actually runs (calcite-wasm, not calcite-cli).
Dev server (`node ../CSS-DOS/web/scripts/dev.mjs`) must be running.

Workloads, increasing cost:

1. **Smoke suite** — fail fast, not declare success.
2. **Zork (`carts/zork1`)** — lightest "real program" baseline.
   Default for `bench-web.mjs --cart=zork`. First stop for "is this
   actually faster?".
3. **Rogue, bootle, bootle-ctest** — established perf workloads with
   baselines in `docs/log.md`. Bootle-ctest exercises the splash-fill
   regression magnet.
4. **Doom8088** — the ultimate test (`bench-web.mjs --cart=doom8088`,
   ~28M ticks to first frame). **Doom8088 numbers are the only ones
   that ship.** A change that doesn't help (or hurts) doom8088 isn't
   a perf win regardless of micro-bench output.

Conformance is the same: passing primitives + matching v1 on a
2000-tick `hello-text` walk tells you almost nothing. Boot a cart and
diff against v1 (or the reference emulator via the conformance
harness) before claiming correctness.


## v1 → v2

### v1 pipeline (still the default)

`CSS text → parse → pattern recognition → compile → bytecode → evaluate`

1. **Parser** (`parser/`) — tokenises via `cssparser`. Parses
   `@property`, `@function`, `if(style())`, `calc()`, `var()`, math
   functions, string literals. Output: `ParsedProgram`.
2. **Pattern recognition** (`pattern/`) — detects optimisable shapes:
   - Dispatch tables: `if(style(--prop: N))` chains (≥4 branches) →
     HashMap.
   - Broadcast writes: `if(style(--dest: N): val; else: keep)` (≥10
     entries) → direct store, with word-write spillover.
3. **Compiler** (`compile.rs`) — flattens `Expr` trees → flat `Op`
   bytecode against indexed slots. Function-body patterns (identity,
   bitmask, shifts, bit extraction) detected for interpreter fast paths.
4. **Evaluator** (`eval.rs`) — runs the tick loop. Two paths: compiled
   linear bytecode (fast), recursive `Expr` walking (fallback).
   Topological sort on assignments. Pre-tick hooks for side effects.
5. **State** (`state.rs`) — flat mutable replacement for CSS triple
   buffer. Unified address space (negative = registers, non-negative =
   memory). Split-register merging (AH/AL ↔ AX). Auto-sized from
   `@property` decls.

Key types in `types.rs`: `Expr`, `ParsedProgram`, `Assignment`,
`FunctionDef`, `PropertyDef`, `CssValue` (Integer | String).

### v2 is underway

A DAG-based walker is being built in parallel — see
`docs/v2-rewrite-design.md`, `docs/v2-self-review-2026-04-29.md`, and
the `calcite-v2-rewrite` worktree under `.claude/worktrees/`.
Correctness-clean on `hello-text` for ≥2000 ticks; ~18.5× slower than
v1 on the same workload. Stacking order to close that gap is in
`docs/log.md`. Per the testing rule, those numbers are debug
telemetry — the real comparison is bench-web.mjs against v1 on zork,
bootle, and doom8088.


## Project layout

```
crates/
  calcite-core/      Engine: parser, pattern compiler, evaluator, state
  calcite-cli/       CLI tool (calcite-cli) + benchmark runner (calcite-bench)
                       + perf/diagnostic probe bins (probe_*.rs)
  calcite-debugger/  HTTP debug server — see docs/debugger.md
  calcite-wasm/      wasm-bindgen layer for the browser Web Worker
docs/                Architecture, perf logbook, debugger, benchmarking,
                       v2-rewrite docs
output/              Pre-built `.css` cabinets (run directly, no rebuild)
tools/               Mostly retired; `js8086.js` still useful as
                       reference 8086 emulator. The old `fulldiff.mjs`
                       / `ref-dos.mjs` / `diagnose.mjs` scripts no
                       longer work — use the CSS-DOS harness instead.
tests/
  conformance/       Primitive .css unit fixtures
  fixtures/          Pre-compiled CSS imported from CSS-DOS
```

The CSS-DOS player, web frontend, builder, carts, and integration
tests live in `../CSS-DOS/`. There is no crate-level dependency
between the two repos and there must never be one — calcite's only
interface to CSS-DOS is the `.css` output.


## Quick reference

```sh
cargo test --workspace                    # tests
cargo clippy --workspace                  # lint
cargo fmt --all                           # format
just check                                # all three

cargo run --release --bin calcite-bench -- -i output/rogue.css \
                  -n 5000 --warmup 500    # native bench (debug telemetry)
cargo bench -p calcite-core               # criterion benches (fixture needed)

wasm-pack build crates/calcite-wasm --target web --out-dir ../../web/pkg
                                          # wasm; verify via web player

# The benchmark of record (CSS-DOS dev server must be running):
node ../CSS-DOS/tests/harness/bench-web.mjs                   # zork, 1 run
node ../CSS-DOS/tests/harness/bench-web.mjs --runs=3
node ../CSS-DOS/tests/harness/bench-web.mjs --cart=doom8088

# Native CLI for ad-hoc runs (test harness, not the product):
target/release/calcite-cli.exe -i ../CSS-DOS/output/rogue.css

# Debugger
cargo run --release -p calcite-debugger -- -i program.css
# Then: curl localhost:3333/state, /tick, /seek, /compare-paths, etc.
```


## Wasm safety — calcite-core must build and run on wasm32

`calcite-wasm` wraps `calcite-core` and runs in the browser. Anything
in `calcite-core` the web build exercises must not panic or fail to
link on `wasm32-unknown-unknown`.

- **Never use `std::time::Instant` / `std::time::SystemTime`** anywhere
  `calcite-core` or `calcite-wasm` can reach. Raw `std::time` panics on
  wasm with "time not implemented on this platform". Use
  `web_time::Instant` (already a workspace dep — maps to
  `performance.now()` on wasm and `std::time::Instant` on native).
- **Same for hidden `SystemTime`/`Instant` deps:** `std::thread::sleep`,
  random seeding from `SystemTime`, `std::sync::Mutex` timeouts.
- **No `std::thread::spawn`** or OS-thread APIs. Wasm is single-threaded
  here. Native-only concurrency must be `#[cfg(not(target_arch =
  "wasm32"))]`.
- **No `std::fs`, `std::net`, `std::process::Command`, or `std::env`
  writes** in core paths. Reads (`std::env::var`) are tolerated by the
  stub but avoid them in hot paths.
- **Dev-only timing/logging (`CALCITE_PROFILE_COMPILE`, env-driven
  dumps) must still use `web_time`** — they run on every `compile()`
  including the wasm one (the scope ctor touches `Instant` even when
  the feature is "disabled").

Native-only escape hatch when truly needed:

```rust
#[cfg(not(target_arch = "wasm32"))]
{
    // native-only code
}
```

Verify: `wasm-pack build crates/calcite-wasm --target web --out-dir
../../web/pkg` must succeed with no errors, and the cabinet must load
in the web player without a `WASM panic:` line in the console.

When in doubt: the crates the wasm build pulls in are `calcite-core`
and `calcite-wasm`. Everything else (`calcite-cli`, `calcite-debugger`,
benches, probes) is native-only and can use `std::time` freely.
