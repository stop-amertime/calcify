# Benchmarking

## calcite-bench (primary tool)

Headless benchmark runner — no terminal rendering, no keyboard input,
pure evaluation speed measurement. Lives in `crates/calcite-cli/src/bench.rs`.

### Quick start

```sh
# Basic speed measurement (ticks/sec, cycles/sec, % of 8086)
cargo run --release --bin calcite-bench -- -i output/rogue.css -n 5000

# With warmup (skip BIOS init overhead)
cargo run --release --bin calcite-bench -- -i output/rogue.css -n 5000 --warmup 500

# Granular profiling: where time goes inside each tick
cargo run --release --bin calcite-bench -- -i output/rogue.css -n 2000 --warmup 200 --profile

# Per-tick histogram (variance analysis)
cargo run --release --bin calcite-bench -- -i output/rogue.css -n 5000 --warmup 500 --histogram

# Statistical multi-run (mean +/- stddev)
cargo run --release --bin calcite-bench -- -i output/rogue.css -n 2000 --warmup 200 --iterations 5

# Parse + compile timing
cargo run --release --bin calcite-bench -- -i output/rogue.css -n 2000 --phase-breakdown

# Halt on flag (useful for "time to boot" benchmarks)
cargo run --release --bin calcite-bench -- -i output/rogue.css -n 999999 --halt 0xFFFF0
```

### Flags

| Flag | Description |
|---|---|
| `-i, --input PATH` | CSS file to benchmark |
| `-n, --ticks N` | Number of ticks to run (default: 10000) |
| `--warmup N` | Ticks to run before measurement starts |
| `--profile` | Granular per-phase + per-op-type breakdown |
| `--histogram` | Per-tick timing distribution (min/p50/p90/p99/max) |
| `--phase-breakdown` | Show parse/compile/warmup timing |
| `--iterations N` | Run N times and report mean +/- stddev |
| `--batch N` | Use `run_batch(N)` instead of single ticks (faster, less profiling visibility) |
| `--halt ADDR` | Stop when memory at ADDR becomes non-zero. Compatible with `--batch`: checked between batches, so granularity-bounded overshoot up to `batch` ticks |

### Reading `--profile` output

The profile breaks the tick into granular phases:

```
--- Per-phase breakdown (2000 ticks) ---
  Phase                     Total   Avg/tick      %
    Linear ops             463ms     231us     91.8%  ← main bytecode loop
    Dispatch lookups        26ms      13us      5.1%  ← HashMap table lookups
  Change detect             11ms       5us      2.2%
  Broadcast writes           3ms       1us      0.5%
  Writeback                  1ms      0.4us     0.2%
```

Below the timing table you get **op counters** (per-tick averages) and an
**op frequency table** showing which bytecode instructions dominate:

```
--- Op frequency (per tick avg, sorted) ---
  LoadLit             34475  33.3%
  BranchIfZero        34140  33.0%
  CmpEq               34118  33.0%
  ...
```

And derived metrics:

```
--- Derived ---
  ns/linear-op:         2.3    ← time per bytecode op (excluding dispatches)
  us/dispatch:          0.6    ← time per dispatch table lookup
```

**Note:** `--profile` adds overhead from per-op HashMap counting (~10-15x
slower than unprofiled). Use it for analysis, not for measuring absolute
speed. For speed measurement, run without `--profile`.

### What to track

When measuring the impact of a change, use unprofiled mode:

```sh
# Before your change
cargo run --release --bin calcite-bench -- -i output/rogue.css -n 5000 --warmup 500
# ... make changes, rebuild ...
# After your change
cargo run --release --bin calcite-bench -- -i output/rogue.css -n 5000 --warmup 500
```

Key metrics:
- **ticks/s** — primary throughput (higher = better)
- **% of 4.77 MHz** — how close to real 8086 speed
- **us/tick** — latency per evaluation cycle

Test with multiple programs to check for regressions:

```sh
for css in output/rogue.css output/fib.css output/bootle.css; do
  echo "=== $css ==="
  cargo run --release --bin calcite-bench -- -i "$css" -n 5000 --warmup 500
done
```

## Splash-fill benchmark (mode 13h grey-fill)

**The primary end-to-end number** for optimisation work on the pixel-blit
path. Measures wall-clock time for the C BIOS splash to fill the 320x200
mode 13h framebuffer grey (colour 8).

`bootle-ctest.css` is the fixture: it uses the C-variant BIOS which runs
the `splash_show()` routine (see `../CSS-DOS/bios/splash.c`). The fill is
64000 calls to `vga_pixel` — a triple-nested C for loop, not REP STOSB —
which is exactly the workload we're optimising.

The halt address is the last pixel of the framebuffer: physical
0xA0000 + 320*200 - 1 = **719359**. When that byte transitions from 0
to 8, the fill is done.

```sh
cargo run --release --bin calcite-bench -- \
    -i output/bootle-ctest.css \
    -n 5000000 --halt 719359 --batch 50000
```

Expected output includes `Ticks:`, `Elapsed:`, and `Ticks/sec:`. The fill
completes at tick **1,828,538** (invariant — do not optimise this number;
it is the conformant op count). What we're driving down is `Elapsed`.

Recorded results:

| Commit | Change | Elapsed | Ticks/sec |
|---|---|---|---|
| 6acc696 | pre-(a) baseline | ~27 s | ~70 K |
| ba11194 | (a) native bitwise | ~16 s | ~120 K |
| 8e2ccd8 | fuse LoadState+BranchIfNotEqLit | ~12 s | ~145 K |
| 32e8479 | skip per-tick state_vars clone | — | — |
| 1930b52 | skip per-tick slot zeroing | — | — |
| a7b1625 | dense-array DispatchChain fast path | ~10 s | ~175 K |

(Earlier rows updated: the ~9s number for ba11194 was a single-best run on a
cold machine. Rerunning three-at-a-time in warm state gives ~15–17 s — a
fairer baseline for subsequent comparisons.)

Overshoot: `--halt` is checked between batches, so `Ticks` may report
slightly more than 1,828,538 (up to `batch - 1` over). Elapsed is
unaffected because `run_batch` stops on the batch that tripped the halt.

## Criterion benchmarks

Micro-benchmarks for parse/compile/tick using the legacy v1 fixture
(`tests/fixtures/x86css-main.css`). Also supports v4 CSS from `output/`
if available.

```sh
cargo bench -p calcite-core
```

Results saved to `target/criterion/` with HTML reports. Subsequent runs
show percentage change from previous baseline.

| Benchmark | What it measures |
|---|---|
| `parse_x86css` | CSS text → `ParsedProgram` |
| `evaluator_from_parsed` | `ParsedProgram` → `Evaluator` (pattern recognition + compilation) |
| `single_tick` | One tick on pre-warmed state (v1 fixture) |
| `batch_ticks/{10,100}` | N ticks from cold start (v1 fixture) |
| `v4/single_tick` | One tick on v4 CSS (if available in `output/`) |
| `v4/batch/{10,100}` | N ticks on v4 CSS |

## Flamegraphs

`cargo-flamegraph` is installed for sampling profiling. On Windows it
requires an **admin terminal** (ETW tracing):

```sh
# In an admin terminal:
cargo flamegraph --release --bin calcite-bench -- -i output/fib.css -n 5000 --warmup 500
# Produces flamegraph.svg in the repo root
```

Debug symbols are enabled in release builds (`profile.release.debug = true`
in workspace `Cargo.toml`) so function names appear in flamegraphs.

## Profiled tick API

The evaluator exposes `tick_profiled()` which returns a `TickProfile` struct
alongside the normal `TickResult`. This is what `calcite-bench --profile`
uses internally. You can also call it directly:

```rust
let (result, profile) = evaluator.tick_profiled(&mut state);
// profile.main_ops     — time in bytecode execution
// profile.dispatch_time — time in dispatch table lookups
// profile.linear_ops   — main_ops minus dispatch_time
// profile.broadcast    — time in broadcast writes
// profile.writeback    — time in slot→state writeback
// profile.op_counts    — HashMap<&str, u64> of per-op-type counts
```

## Baseline numbers (2026-04-14, post BranchIfNotEqLit fusion)

| Program | Ticks/s | % of 8086 | us/tick |
|---|---|---|---|
| rogue.css | ~6050 | 1.1% | ~165 |
| fib.css | ~7070 | 1.3% | ~142 |
| bootle.css | ~7210 | 1.3% | ~139 |

See [docs/log.md](log.md) for the full bottleneck analysis and
optimisation roadmap.
