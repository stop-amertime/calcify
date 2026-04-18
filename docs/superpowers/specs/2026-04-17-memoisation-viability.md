# Memoisation / trace / affine / period viability — splash fill (bootle-ctest.css)

*2026-04-17, opus 4.7 worktree `unruffled-lichterman-ee33f5`.*

Four probes, asked in escalating order of generality. Only the last one
finds a real optimisation target. The first three are recorded so the same
dead ends don't get re-explored.

## Setup

Instrumented `State` with `read_log: RefCell<Option<Vec<(i32,i32)>>>` and
`write_log: Option<Vec<(i32,i32)>>`. `read_mem`/`write_mem` push to these
when enabled; otherwise zero overhead. Four standalone probe binaries in
`crates/calcite-cli/src/bin/`. Each runs `output/bootle-ctest.css` end-to-end
until `memory[719359] != 0` (confirmed halt at tick 1,828,538).

## Assumption checks (from the original brief)

| Claim | Verdict |
|---|---|
| Halt at tick ~1.85M via `memory[719359] != 0` | **Confirmed exactly 1,828,538.** |
| State vector ~34 × i32 | **34 confirmed** (I initially mis-counted 32; forgot `memAddr`, `memVal`). |
| All reads via `read_mem`/`read_mem16` | **Confirmed.** Only `state.memory[` access in `crates/calcite-core/src` is inside render helpers and tests. |
| `CALCITE_TICK_FUSION=0` disables fusion | **N/A in this worktree** — no `tick_fusion.rs`, no env var, no `probe_splash.rs`. Built fresh. |
| `state_vars` is the complete tick-input | **Effectively yes.** `compile::execute` never touches `string_properties`/`extended`/raw memory. |
| Iterating-var guess list | **Mostly right**; per-slot unique counts: `cycleCount 1.6M`, `memAddr 65K`, `SI 64K`, `CX/DI 32K`, `AX 766`, `BX 514`, `IP 290`, `biosAL/memVal 256`, `biosAH 201`. Everything else ≤18. |

## Probe 1 — value-keyed tick memoisation: **dead**

Per tick: snapshot `state_vars`, capture `(addr, value)` from `read_mem`,
run the tick, hash both. Compute "recurrence rate" (state-key repeats) and
"memory-validated hit rate" (state-key AND memory-read-set both repeat).

| Key definition | Unique keys | Naive recurrence | Memory-validated hit |
|---|---:|---:|---:|
| All 34 state vars | 1,828,538 | 0.00 % | **0.00 %** |
| Drop 14 iterators (20 vars kept) | 15 | 100.00 % | **0.00 %** |
| Only vars with ≤3 unique values | 4 | 100.00 % | **0.00 %** |
| V2 + low byte of DI | 269 | 99.99 % | **0.00 %** |

**Blocker: memory-read divergence.** Every tick reads a different set of
framebuffer bytes; the naive recurrence numbers in V2/V3 are the cache
silently returning wrong answers. Don't build this.

## Probe 2 — trace specialisation (LuaJIT-style consecutive-run): **dead**

Per-tick fingerprint `(uOp, IP, hash of 21 non-iterating vars)`. Measure
run-length of identical fingerprints in consecutive ticks.

| Metric | Value |
|---|---|
| Ticks in runs of length ≥ 1 | 100.00 % (trivially) |
| Ticks in runs of length ≥ 4 | **0.00 %** |
| Avg run length | **1.00** |
| Fingerprints needed for 90% coverage | 4,648 out of 6,765 |
| Top-5 fingerprints | each exactly **32,000 ticks** |

**Blocker: the microcode interleaves stages.** No two consecutive ticks
share a stage — but each stage fires ~32K times across the run. That's
32K iterations of a multi-stage inner loop, not a hot trace. Calcite's
existing dispatch-chain compilation is already the trace-specialisation
equivalent. The "top-5 fingerprints at 32,000 each" hint is what Probe 4
follows up on.

## Probe 3 — affine stores (consecutive writes): **dead**

Per tick, capture all `(addr, value)` from `write_mem`. Measure run-length
of identical `(addr_delta, val_delta)` tuples between consecutive ticks.

| Metric | Value |
|---|---|
| Writes per tick | 2–7 (avg ~4.5) |
| Ticks with any memory write (`addr ≥ 0`) | 28 % |
| First-write variant, longest consecutive run | 5 |
| First-memory-write variant, longest consecutive run | **1** |
| Biggest (addr, val) delta tuple by coverage | `(+1, −242)` × 64,200 ticks |

**Blocker: same as Probe 2.** The pixel store isn't on consecutive ticks,
it's on every 26-th tick. Per-tick granularity destroys the structure.
But note the `(+1, −242)` tuple covering 64,200 ticks — that's the 64K
framebuffer fill hiding behind a loop period. Probe 4 finds it.

## Probe 4 — loop-period detection: **the real opportunity**

Autocorrelate the fingerprint stream from Probe 2: for each candidate
period `P ∈ [2..512]`, count `t` in the middle-half window where
`fingerprint[t] == fingerprint[t+P]`.

### Period detection

| Period | Autocorrelation match |
|---:|---:|
| **26** | **99.60 %** |
| 52 | 99.28 % |
| 78 | 98.97 % |
| 104 | 98.65 % |
| 130 | 98.34 % |

Clean winner at P=26, harmonics at 52/78/104/… confirm it's the true period.
Longest strictly-contiguous P=26 region: 8,292 ticks (318 iterations). The
99.60% score means P=26 holds almost everywhere, with occasional single-tick
breaks (probably flags-toggle ticks).

### Per-state-var evolution across iterations (step = 26 ticks)

29 of 34 vars are constant at iteration boundaries. The rest:

| Var | Offset 0 value | Δ per iteration | Classification |
|---|---:|---:|---|
| CX | 0 | **+1** | affine (iteration index) |
| cycleCount | 1,056,522 | **+148** | affine (5.69 CPI × 26 ticks) |
| flags | 86 | — | non-affine (3 unique values over 318 iters) |

### Memory writes within one 26-tick iteration

7 of 26 offsets write to memory; 5 have fully affine (addr, val) across
all 318 iterations:

| Offset | Base addr | addr stride | val stride | Note |
|---:|---:|---:|---:|---|
| 1 | 655,344 | 0 | 0 | constant-address scratch |
| 2 | 655,345 | 0 | 0 | constant-address scratch |
| 5 | 655,340 | 0 | 0 | constant-address scratch |
| 6 | 655,341 | 0 | 0 | constant-address scratch |
| **17** | **655,680** | **+1** | **0** | **framebuffer STOSB — this is the pixel** |

`0xA0000 = 655,360`; `655,680 = 0xA0000 + 320` = start of the second
video scanline, consistent with the mode-13h fill already being in
progress at the region we sampled.

### So: how much is available?

- **~99.60 % of splash ticks fit the P=26 affine pattern.** The remaining
  0.4% are the disruption points (flags transitions, loop-boundary
  bookkeeping).
- An optimisation that detects P, verifies affinity over a small warmup
  (e.g. 3 iterations), then projects N iterations forward as
  `memset(655680..655680+N, fillbyte); CX += N; cycleCount += 148*N`,
  would collapse those 8K-tick runs into a handful of scalar ops plus a
  `memcpy`/`memset` call.
- Realistic upper-bound speedup on the splash phase: **~20–30×** (not quite
  the 26× geometric limit, because of warmup + fallback overhead +
  disruption-point handling). That would take the splash from 13 s to
  roughly 0.5 s.
- The existing `broadcast_writes` infrastructure already does the analogous
  thing at CSS compile time for literal broadcasts. This is the runtime
  equivalent: runtime-detected affine evolution over a runtime-detected
  period, specialised into the same memset-style lowering.
- **Cardinal-rule-safe**: calcite observes "the bytecode has a stable
  runtime period of 26 ticks and an affine memory-store pattern within it"
  — no x86 knowledge, no opcode awareness. Same shape of reasoning as
  dispatch-chain pattern detection, just inducted over the tick axis
  instead of the ops axis.

## Recommendation

1. **Don't build**: memoisation (Probe 1), trace specialisation (Probe 2),
   or consecutive-tick affine-store detection (Probe 3). The data rules
   them out independently.
2. **Do build**: runtime period-and-affine-evolution detection. The
   algorithm is straightforward (autocorrelate over a sliding window,
   verify affinity over a small sample, project forward until an
   invariant breaks), and the payoff is ~20–30× on the workload that
   currently dominates the splash bench.
3. The `flags` non-affinity is the only subtlety worth flagging — either
   include it in the invariant check (specialise multiple flag variants)
   or treat it as recomputable on loop exit.

## Artifacts

All in the worktree, not committed:

- `crates/calcite-core/src/state.rs` — added `read_log`, `write_log`.
- `crates/calcite-cli/src/bin/probe_splash_memo.rs` — Probe 1.
- `crates/calcite-cli/src/bin/probe_splash_trace.rs` — Probe 2.
- `crates/calcite-cli/src/bin/probe_splash_affine.rs` — Probe 3.
- `crates/calcite-cli/src/bin/probe_splash_period.rs` — Probe 4.
- `probe.*.stdout.log`, `probe.*.stderr.log` — raw output.

Build: `cargo build --release -p calcite-cli --bin probe-splash-{memo,trace,affine,period}`.
Run any binary with no args — each defaults to `output/bootle-ctest.css`
and halt 719359.
