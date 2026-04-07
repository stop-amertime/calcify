# Benchmarking

## Running benchmarks

```bash
cargo bench -p calcite-core
```

This runs [criterion](https://github.com/bhavsec/criterion.rs) benchmarks against the x86CSS fixture (`tests/fixtures/x86css-main.css`). Results are saved to `target/criterion/` with HTML reports. Subsequent runs automatically show percentage change from the previous baseline.

### Benchmarks

| Benchmark | What it measures |
|---|---|
| `parse_x86css` | CSS text to `ParsedProgram` (tokenisation, @property, @function, assignment parsing) |
| `evaluator_from_parsed` | `ParsedProgram` to `Evaluator` (dispatch table recognition, broadcast write detection) |
| `single_tick` | One tick on a pre-warmed state (the hot path) |
| `batch_ticks/10_ticks` | 10 ticks from cold start |
| `batch_ticks/100_ticks` | 100 ticks from cold start |

## Current numbers (2026-04-06)

Measured on Windows 11, release build (`cargo bench`):

| Benchmark | Time |
|---|---|
| parse_x86css | 10.8 ms |
| evaluator_from_parsed | 2.1 ms |
| **single_tick** | **2.85 ms** |
| batch 10 ticks | 29.3 ms |
| batch 100 ticks | 297 ms |

## Chrome comparison

Chrome's x86CSS runs as CSS animations at [lyra.horse/x86css](https://lyra.horse/x86css/). The two engines are not directly comparable per-tick because Chrome uses a triple-buffer animation pipeline where most ticks cycle through buffer phases without advancing the CPU state. Calcite eliminates the triple buffer and directly mutates state, so each calcite tick does real work.

The fair comparison is **observable instruction throughput** — how fast each engine executes actual x86 instructions as measured by register changes.

### Methodology

**Chrome**: Playwright captures SI register transitions over 60 seconds via `setInterval` sampling at 50ms. SI increments by 8 each time the main loop completes one iteration (one x86 instruction batch). Timestamps on each transition give wall-clock time per instruction.

**Calcite**: `--verbose` output shows SI changing every 7 ticks. Criterion measures 2.85ms/tick, giving wall-clock time per instruction.

### Results (2026-04-06)

Chrome 146, lyra.horse/x86css, headless via Playwright (2026-04-06):

| Metric | Chrome | Calcite |
|---|---|---|
| Wall time per x86 instruction cycle | ~2.0 s | ~20 ms |
| Instruction cycles per second | ~0.5 | ~50 |

**Calcite is ~100x faster than Chrome at executing x86 instructions.**

### Why the gap

Chrome does ~1,000 CSS animation iterations per second (1ms animation duration), but only ~0.5 of those produce an observable state change. The triple-buffer pipeline (`--__0`, `--__1`, `--__2` prefixes) means each value takes multiple animation cycles to propagate from the write buffer through to the read buffer. Chrome's style engine recalculates all ~3,200 custom properties on every animation iteration regardless.

Calcite skips the triple buffer entirely — one tick = one style recalculation = one state change. It evaluates ~3,200 assignments per tick at 2.85ms/tick, where Chrome's style engine takes ~1ms per recalc but needs ~4,000 recalcs to achieve the same result.

### Raw Chrome data

18 SI transitions observed over 60 seconds. Transition timestamps (ms from start) and SI values:

```
  1839  1235 → 1243
  3694  1243 → 1251
  5235  1251 → 0      (string boundary wrap)
  5685  0    → 1120
  8682  1120 → 1128
 10997  1128 → 1136
 12816  1136 → 1144
 14850  1144 → 1152
 16955  1152 → 1160
 18925  1160 → 1168
 21058  1168 → 1176
 23071  1176 → 1184
 25042  1184 → 1192
 27091  1192 → 1200
 29092  1200 → 1208
 31334  1208 → 1216
 33911  1216 → 1224
 35497  1224 → 1120   (loop restart)
```

Chrome baseline captured from lyra.horse/x86css/ — snapshots in `tests/fixtures/chrome-baseline.json`.
