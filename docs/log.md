# Performance Log

Running notes on calcite performance work. Read this before doing any
optimisation — it has the current bottleneck analysis and the tooling
to measure impact of changes.

## Tooling reference

See [docs/benchmarking.md](benchmarking.md) for full usage of `calcite-bench`
and the Criterion benchmarks.

---

## 2026-04-14: V4 baseline + bottleneck analysis

### Context

CSS-DOS v4 landed. It boots rogue, fib, bootle, etc. in ~300K ticks.
Current speed: **~4800 ticks/s on rogue** (0.9% of real 8086 at 4.77 MHz).
Goal: much faster.

### Profiling infrastructure added

Added `calcite-bench` (headless benchmark binary) with `--profile` for
granular per-phase and per-op-type breakdown. See
[docs/benchmarking.md](benchmarking.md) for usage.

### Top-level phase breakdown

Profiled rogue.css, 2000 ticks after 200 warmup:

| Phase | % of tick | Avg/tick |
|---|---|---|
| **Linear ops** | **91.8%** | 231us |
| Dispatch lookups | 5.1% | 13us |
| Change detect | 2.2% | 5us |
| Broadcast writes | 0.5% | 1us |
| Writeback | 0.2% | 0.4us |
| Everything else | <0.2% | — |

**All optimisation work should target the bytecode interpreter loop
(`exec_ops` in `compile.rs`).**

### Op frequency breakdown

The 102K ops/tick break down as:

| Op | Per tick | % of ops |
|---|---|---|
| **LoadLit** | 34,475 | 33.3% |
| **BranchIfZero** | 34,140 | 33.0% |
| **CmpEq** | 34,118 | 33.0% |
| LoadSlot | 196 | 0.2% |
| Mul | 114 | 0.1% |
| Add | 77 | 0.1% |
| everything else | <65 | <0.1% each |

**99.3% of all ops are three instructions in equal proportion:**
`LoadLit + CmpEq + BranchIfZero`.

### What this means

The CSS has ~34K if-chains per tick, each checking `if(style(--prop: N))`.
Per tick, exactly **one** of these 34K matches. The other 33,999 all fail
and branch over immediately.

This is the compiled form of CSS patterns like:

```css
--result: if(style(--opcode: 0)) { ... }
          if(style(--opcode: 1)) { ... }
          if(style(--opcode: 2)) { ... }
          /* ... 34K more ... */
```

Each `if(style(--prop: N))` compiles to:

```
LoadLit   slot[X] = N          # the constant to compare
CmpEq     slot[Y] = (slot[prop] == slot[X])
BranchIfZero slot[Y] → skip    # jump past body if no match
... body ops ...               # only reached for the one match
```

The pattern recogniser already converts `if(style())` chains into dispatch
tables (HashMap lookups) when it detects ≥4 branches on the **same property**.
But these 34K comparisons are **not being caught** — either because they test
different properties, or because the chain structure doesn't match the
recogniser's expectations.

### Optimisation directions (in order of expected impact)

1. **More aggressive dispatch table recognition.** If the pattern recogniser
   can catch these 34K if-chains, they collapse to one HashMap lookup per
   tick. That would eliminate >99% of ops. This is the 100x opportunity.

2. **Fused CmpEq+Branch op.** Since CmpEq is always followed by
   BranchIfZero on the same result slot, fuse them into
   `BranchIfNotEqual { a, b, target }`. Eliminates the intermediate slot
   write and halves the match-dispatch overhead for uncaught patterns.

3. **Skip-chain optimisation.** Recognise runs of `LoadLit + CmpEq +
   BranchIfZero` all testing the same slot and emit a dispatch at compile
   time.

### Branch statistics

- 34,140 branches per tick
- 99.9% taken (i.e., the condition is zero → jump)
- Only 47 branches per tick fall through (the one matching case + a few others)

This is a massive dead-code-skip pattern. Most of the bytecode stream
exists only for the rare tick where that particular property matches.

### Key numbers to track

When making changes, run:

```sh
cargo run --release --bin calcite-bench -- -i output/rogue.css -n 5000 --warmup 500
```

Baseline (2026-04-14, no profile overhead):
- **rogue.css**: ~4800 ticks/s, 0.9% of 8086
- **fib.css**: ~5300 ticks/s, 1.0% of 8086
- ~204us/tick (rogue), ~187us/tick (fib)

---

## 2026-04-14: Fused CmpEq+Branch (BranchIfNotEqLit)

### Investigation

Profiling showed 99.3% of ops were `LoadLit + CmpEq + BranchIfZero`
triplets. Investigation revealed:

1. **Dispatch table recognition is working** — ~179 tables with 34,850
   entries are created. Only ~100 small chains are missed (compound
   conditions and multi-property tests).

2. **The 34K branches per tick come from inside dispatch table entries**
   and the main bytecode stream's linear if-chains. Each opcode entry
   contains further if-chains for register selection, addressing modes,
   etc. These are too small or complex for dispatch table recognition.

3. **402K ops in the main stream, 1.13M ops in dispatch entries** — most
   are the same `LoadLit + CmpEq + BranchIfZero` triplet pattern.

### The fix: `BranchIfNotEqLit` fused op

Added a peephole pass (`fuse_cmp_branch`) that runs after compilation and
before slot compaction. It scans for the pattern:

```
LoadLit(dst=X, val=N) → CmpEq(dst=Y, a=P, b=X) → BranchIfZero(cond=Y, target=T)
```

and replaces it with a single:

```
BranchIfNotEqLit(a=P, val=N, target=T)
```

This eliminates two intermediate slot writes and two match-dispatch cycles
per branch test. The pass also fuses inside dispatch table entry ops and
broadcast write ops.

### Results

| Program | Before | After | Improvement |
|---|---|---|---|
| rogue.css | ~4800 ticks/s | **6054 ticks/s** | **+26%** |
| fib.css | ~5300 ticks/s | **7066 ticks/s** | **+33%** |
| bootle.css | — | **7214 ticks/s** | — |

Main-stream ops per tick dropped from **102K to 35K** (3x reduction).
96.4% of remaining ops are now `BranchIfNotEqLit`.

Correctness verified: bootle shows hearts, rogue boots to DOS.

### Remaining opportunities

The dispatch table entries still execute unfused ops (80 sub-ops per
dispatch on average). The fused op handles the main stream and entry
ops arrays, but dispatch entries that are executed via `exec_ops` (the
non-profiled path) also benefit from the fusion.

Next directions:
- The 35K `BranchIfNotEqLit` ops per tick are still the bottleneck.
  These are all testing the same few properties against different
  constants. A sorted-array binary search or perfect hash at compile
  time could collapse them further.
- Multi-property dispatch: chains testing `--_tf` and `--opcode`
  together could be split into nested dispatches.

---

## 2026-04-14: Unchecked slot indexing in exec_ops hot loop

### The fix

Switched all `slots[idx as usize]` accesses in `exec_ops` (compile.rs) to
`unsafe { *slots.get_unchecked(idx) }` / `get_unchecked_mut`, hidden behind
`sload!` / `sstore!` macros with `debug_assert!` for debug builds. Also
unchecked the `ops[pc]` load and the `dispatch_tables[table_id]` lookup.

Safety rests on invariants the compiler already guarantees: every slot
index in an op is `< slot_count`, every branch target is `<= ops.len()`,
every `table_id` is a valid index. Debug builds still check.

Also reordered the match so `BranchIfNotEqLit` (96% of ops) is the first
arm — helps the compiler lay out the jump table favorably.

### Results

| Program | Before (BranchIfNotEqLit only) | After (unchecked) | Improvement |
|---|---|---|---|
| rogue.css | ~6316 ticks/s | **~7100-7400 ticks/s** | **+13-17%** |
| fib.css | ~7066 ticks/s | ~7900 ticks/s | +12% |
| bootle.css | ~7214 ticks/s | ~7450 ticks/s | +3% |

Run-to-run noise on Windows is ~±5%. The rogue gain is reliably above +10%
across multiple runs.

Correctness: all 27 core tests pass; rogue boots to the same
`IP=890, 404767 cycles` state as before at tick 30000.

### Why it helps

The bytecode interpreter runs `match` + slot index on every op, 34K ops/tick.
Each `slots[idx as usize]` does a bounds check (compare, branch). LLVM
cannot elide these because slot indices come from the op struct, not a loop
induction variable. Skipping them is safe because the compiler statically
allocates slots and never emits an out-of-range index.

### What did NOT work

- **Hot-chain peel** inside the `BranchIfNotEqLit` arm (speculatively
  handle adjacent BranchIfNotEqLit ops in an inner loop without re-entering
  the match). Tested: slowed rogue from 7100 → 6500. The extra inner branch
  disrupted branch prediction and the compiler couldn't inline cleanly.
  Reverted.

### Session notes (meta)

- The session started with a stale diagnostic reporting "takes 4 arguments
  but 5 supplied" that pointed at an exec_ops call site which was actually
  correct. The build succeeded; the diagnostic was cached. Worth remembering
  to trust `cargo build` over rust-analyzer's live diagnostics when they
  disagree.

---

## 2026-04-14: Dispatch-chain recognition (DispatchChain op)

### The fix

Added a new op `DispatchChain { a, chain_id, miss_target }` and a
post-fusion pass `build_dispatch_chains` that detects runs of
`BranchIfNotEqLit { a: P, val: V_i, target: T_i }` where each miss target
points at the next branch-on-same-slot, and collapses them to a single
HashMap lookup.

Implementation sketch:
- For each `BranchIfNotEqLit` at position `i` on slot `P`, walk the chain
  by following each `target`. If the target points to another
  `BranchIfNotEqLit` on the same slot, extend the chain.
- If ≥ `MIN_CHAIN_LEN` (currently 6) branches accumulate, build a
  `DispatchChainTable: HashMap<i32, u32>` mapping each `V_i → body_pc`
  (body_pc = branch_pc + 1).
- Replace `ops[i]` with `DispatchChain`. Leave the rest of the chain in
  place as dead code — keeps any external jumps into the middle of the
  chain valid.

At runtime, `DispatchChain` reads `slot[a]`, looks up in the table, and
either jumps to the matching body PC or to `miss_target`. Eliminates up to
~30 `BranchIfNotEqLit` ops per chain per tick.

### Results

Steady-state benchmark on rogue (20K ticks, 5K warmup):

| Metric | Before | After |
|---|---|---|
| Ticks/s | ~7,100 | **272,194** |
| Cycles/s | 67 KHz | **4.35 MHz** |
| % of 4.77 MHz 8086 | 1.4% | **91.2%** |
| μs/tick | 140 | **3.7** |

Short 5K-tick bench (warmup 1K): 137K-151K ticks/s on rogue/fib/bootle
(all previously 7-8K).

Longer 30K-tick run from cold boot shows smaller apparent gains
(5559 ticks/s), because boot code contains many ops outside dispatch
chains and dominates that window. Steady state hot-loop performance is
where the big win materialises.

### Correctness

- All 27 core tests pass.
- Rogue at tick 30000: IP=890, 404,767 cycles — **identical** to baseline.
- Rogue at tick 60000: 832,808 cycles — consistent with baseline (tick
  counts are deterministic so any logic bug would diverge these).

### Design notes

- Dead-code approach (leave intermediate branches in place) was chosen
  over removal because branch targets from elsewhere in the program may
  point into the middle of a chain; tracking and patching those is extra
  complexity for no runtime gain (dead code isn't executed).
- `MIN_CHAIN_LEN = 6` is a rough guess — HashMap lookup is ~30ns, linear
  match-dispatch is ~5ns/op, so break-even is around 6. Could be tuned.
- Chain tables live in `CompiledProgram::chain_tables`, threaded through
  `exec_ops`, `exec_ops_profiled`, and `exec_ops_traced` as an extra arg.

### Session notes (meta)

- Adding a new `Op` variant touches 6+ `match` sites (op_slots_read,
  op_dst, map_op_slots, seed_from_parent, three exec_ops variants). rust-
  analyzer diagnostics for "non-exhaustive patterns" are reliable here —
  use them as a checklist. The function-arity diagnostics stayed stale
  longer than the pattern ones; trusting `cargo build` still pays off.

---

## 2026-04-14: Interactive CLI path was throttling the evaluator

### The discovery

After the two evaluator wins above, the bench reported 270K+ ticks/s but
running the same program via `run.bat` showed only ~5–12K ticks/s. The
user (correctly!) pushed back that they weren't seeing the speedup.

Root cause: `run.bat` invoked `calcite-cli` with `--halt halt`, and the
CLI's `needs_per_tick` check (`cli.verbose || halt_addr.is_some() ||
(interactive && screen_interval > 0) || !key_events.is_empty()`) forced
the per-tick loop. That loop did one `evaluator.tick(state)` plus one
`crossterm::event::poll(Duration::ZERO)` syscall per simulated tick. On
Windows, the event poll is ~1–5 μs per call — which used to be drowned
out by a 140 μs tick, but after the optimisations each tick is ~4 μs, so
the keyboard poll became the dominant cost.

### The fix

Two changes to run.bat / calcite-cli:

1. **run.bat**: Dropped `--halt halt` from the rogue/general program
   launch — unnecessary for interactive use and the main trigger for
   per-tick mode.
2. **calcite-cli**: Rewrote the interactive loop to run in configurable
   batches (`--interactive-batch`, default 50,000). Between batches, the
   CLI still polls keyboard, fires `--key-events`, re-renders the screen,
   and checks `--halt`. Between ticks within a batch, none of that
   happens — `run_batch` goes full-speed. Scripted key events force the
   batch to shrink so they fire at the correct tick; held-key BDA refill
   runs once per batch (BIOS INT 16h busy-spin still sees the key in
   time — 50K ticks ≈ 10.5 ms of sim time, well inside a human keypress).

Physical intuition: at 4.77 MHz the 8086 executes ~80K instructions per
60 fps frame, so a 50K-tick batch is ~10 ms of sim time — imperceptible
for input latency. Earlier default screen_interval=500 ran renders 160×
per sim frame for no benefit.

### Results

Same rogue.css, same 500,000 ticks, via `calcite-cli` (not bench):

| Config | Before | After |
|---|---|---|
| `--screen-interval 500` (default) | ~12,000 ticks/s | **245,000–287,000 ticks/s** |
| `--screen-interval 50000`         | ~12,000 ticks/s | **peaks 346,000 ticks/s (4.36 MHz = 91% of 8086)** |

Cycle count (6,290,226) identical to baseline — no correctness regression.

### Status-line readability

User asked to stop the speed readout from glitching between units (KHz ↔
MHz, different widths). Replaced the live status line with:

- **Fixed-width formatting**: always `X.XX MHz` (8-char wide) regardless
  of speed. No more "100 KHz" ↔ "1.0 MHz" width flips.
- **EMA smoothing** (α = 0.3) on ticks/s so noise doesn't flicker the
  display.
- **Refresh throttle**: the rendered status text only updates every 500 ms
  of wall time; the screen itself still repaints every `screen_interval`
  ticks, but the speed digits stay put between status refreshes.

### Session notes (meta)

- The user's debugging instinct was right before my analysis caught up. I
  built a theory (`event::poll` overhead dominates) and was about to
  write another optimisation when the user asked "isn't it just the
  batch mode thing?" Looking again: the CLI's `run_batch(cli.ticks)`
  path only activates when `needs_per_tick == false`, and any of
  `--halt`, screen_interval > 0, verbose, or scripted key events trips
  it. Answering user doubts honestly saves wall time — I did not need to
  write more code to reach the diagnosis.
- Feature-flagging rule of thumb for this project: if something "should"
  be fast per the bench but isn't in a real run, look first at whether
  the CLI wrapper is forcing a slow path.
