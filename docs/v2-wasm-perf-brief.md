# v2 wasm codegen — perf rewrite brief

**Status:** open. Companion to [`compiler-mission.md`](compiler-mission.md)
and [`v2-rewrite-design.md`](v2-rewrite-design.md). Read those first —
the cardinal rule, the phase plan, and the DAG contract live there.

---

## TL;DR

`Backend::DagV2Wasm` is correct now (200k-tick lockstep against the
walker, bit-identical, on both zork1 and doom8088). It is not faster
than the walker on the cabinet that ships. Per Phase 3 of
`compiler-mission.md`, codegen is *the* mechanism for the perf wins
the project exists to deliver, and the current shape can't get there.

The architecture needs to change from "one wasm function per
`WriteVar` terminal, called once each per tick from a Rust driver,
with host imports for memory" to "one wasm function for the whole
tick (or close to it), all state in linear memory, zero host
crossings on the hot path." This is the diagnosis the previous agent
arrived at by the end of their session and is documented (with
honest numbers) below.

The previous session's work is your foundation: the DAG vocabulary,
the Phase 2 recognisers, and the wasm-emit shape factor cleanly.
Don't throw them away. Replace the *driver* and the *function-shape*,
not the lowering.

---

## What you inherit

- `crates/calcite-core/src/dag/wasm_codegen.rs` — emitter. Builds one
  per-`@function` body and one per-`WriteVar` terminal as separate
  wasm functions inside one module.
- `crates/calcite-core/src/dag/wasm_host.rs` — wasmtime runtime
  wrapper. Owns the `Engine`, `Store`, exported memory, per-terminal
  `TypedFunc`. `reset_tick` + per-write `poke_state_var` /
  `poke_transient` synchronise the wasm linear-memory cache from the
  driver after each `WriteVar`.
- `crates/calcite-core/src/eval.rs::dag_v2_wasm_tick` — driver. Three
  phases: scalar `WriteVar`s (host calls one wasm function per
  emitted terminal, walker fallback otherwise), writeback to `State`,
  then broadcasts via the walker.

State live in wasm linear memory:

- state-var values + presence flags (one i32 each)
- transient values + presence flags (one i32 each)

State *not* in wasm linear memory:

- the per-tick sparse memory cache (`HashMap<i32, i32>`) — read via
  `host_read_memory` import.
- previous-tick reads — `host_read_prev` import.
- `Pow` and `Round` — `host_calc_*` imports (need real f64).

The other parts of the cabinet's per-tick state — `state.memory[]`
(typically 1 MB), the packed-cell table, `state.extended` for
out-of-1-MB regions — live entirely in the host and are reachable
from wasm only through `host_read_memory`.

## The numbers

Real DOS cabinets, native release, 60-second budget × 3 runs each,
bytes vary per run because of timing, taking the median:

| Cabinet | Bytecode (v1) | Walker (v2) | Wasm (v2) | wasm vs walker | wasm vs v1 |
|---|---|---|---|---|---|
| zork1 (288 MB) | 186 K t/s | 3.9 K t/s | 4.2 K t/s | +8 % | 44× slower |
| doom8088 (332 MB) | 221 K t/s | 4.3 K t/s | 3.0 K t/s | **−30 %** | 73× slower |

Coverage: ~110 of ~150 emit-eligible terminals (75 %) per cabinet.
The other 25 % bail to walker fallback (broadcast `IndirectStore` —
always — plus terminals over `LOCAL_LIMIT=30,000` or
`INSTRUCTION_LIMIT=200,000`).

Bytecode runs doom8088 at 285 % of real 8086 because v1's
`rep_fast_forward` collapses doom's tight `REP MOVS/STOS` loops
into bulk operations; v2 has no equivalent (yet).

Reproduce: `./target/release/calcite-bench --backend X --budget-secs 60 -i CABINET`
where `X` is one of `bytecode`, `dag-v2`, `dag-v2-wasm`. The
`bench-zork-3backends.sh` shell script in the repo root drives all
three for a given cabinet path.

## Why the current shape can't get there

Five concrete reasons, easy to verify:

1. **Per-terminal wasm function call overhead.** `dag_v2_wasm_tick`
   loops over ~150 terminals per tick and calls `host.run_terminal(i,
   ...)` for each emitted one. Each call crosses the wasmtime
   boundary (~5–10 ns even with `TypedFunc`). At ~110 emitted
   terminals per tick that's ~600–1100 ns of per-tick boundary tax
   before any work. Walker has no such boundary.

2. **Per-`LoadVar` host import for memory reads.** Every `LoadVar`
   with `slot >= 0 && < TRANSIENT_BASE` (i.e. memory-cell reads,
   which dominate emulation tick work) lowers to a `call
   host_read_memory`. Each call is another wasmtime boundary plus a
   `HashMap::get` in the host. On a cabinet that does dozens of
   memory reads per tick, this alone is more host crossings than the
   actual compute.

3. **Sparse memory cache stays on the host.** The walker's
   `memory_cache: HashMap<i32, i32>` lives in the host and is
   consulted via `host_read_memory`. Even if the per-tick wasm
   function has every other piece of state in linear memory, every
   memory-cell read pays the host trampoline.

4. **`Pow` and `Round` host trampolines.** Cheap to make pure-wasm
   with `f64.{floor,ceil,trunc}` instructions, but currently route
   through the host for "real f64 semantics."

5. **Broadcasts handled by the walker.** Every `IndirectStore` —
   typically 8 of them per cabinet, and they do real work (memory
   spans of thousands of cells per tick) — runs through
   `dag_exec_broadcast_with` / `dag_exec_packed_broadcast`, i.e. the
   walker. The wasm path delegates them entirely. So even if the
   wasm-emitted scalar terminals were free, broadcasts cap the
   speedup.

The structural cause of all five: the implementation treats wasm
like an interpreter target (per-op functions called from a Rust
driver, host imports for "anything inconvenient") rather than a JIT
target (one big block of wasm with all state in memory and no
crossings during the hot path).

## What to build

The Phase 3 endpoint shape, in order of how much each step matters:

### 1. One wasm function per tick (the big one)

Replace the per-terminal loop in `dag_v2_wasm_tick` with a single
exported `tick()` wasm function that:

- topologically orders all `WriteVar` terminals (compile-time)
- emits each terminal's value cone as straight-line wasm
- writes the result directly into the state-var / transient regions
  of linear memory
- when complete, returns

The driver becomes "call `tick()` once per tick" instead of "loop
over 150 terminals calling each one." Eliminates per-terminal
boundary crossings (reason 1).

Caveat: the per-function size limits (currently ~200K instructions /
~30K locals) become per-*tick* limits. You'll need to either:
(a) accept that very large cabinets still bail terminals to walker
fallback, OR (b) split the tick across multiple wasm functions (e.g.
"slab 0 = first N terminals, slab 1 = next N…", call them in order
from a thin wasm `tick()` that just does N `call`s), OR (c) compress
the lowering enough to fit (CSE, peephole) before emit.

(b) is the pragmatic choice. The slab boundaries become rare
crossings; with ~5 slabs you have ~5 crossings per tick instead of
150. Slab boundaries can also be drawn so each slab's reads see only
caches written by prior slabs (preserving `LoadVar Current`
semantics).

### 2. State.memory + memory cache in linear memory

Move `state.memory[]` (typically 1 MB) and the per-tick sparse
memory cache into the wasm module's linear memory. Layout: extend
`WasmMemoryLayout` with `memory_base` and `memory_cache_base`
regions. Driver writes `state.memory` once at backend instantiation
(it changes only across program loads) and the per-tick memory cache
is replaced by direct linear-memory writes during the tick.

`emit_load_var` for `slot >= 0 && < TRANSIENT_BASE` becomes a single
`i32.load` from `memory_base + slot`. Eliminates reasons 2 and 3.

For cabinets that touch addresses outside the 1-MB packed region
(VGA DAC at 0x100000+, rom-disk at 0xD0000–0xD01FF), keep a
host-import fallback — those are infrequent and don't dominate the
tick cost. But the *common* memory access (`state.memory[addr]`
where `addr < 0xA0000`) must be a single `i32.load`.

Cross-tick reads (`LoadVar { kind: Prev }`) use the same memory
region — pre-tick state is exactly what `state.memory` contains
before any of the tick's writes have run.

### 3. Pure-wasm Pow and Round

Use `f64.convert_i32_s`, `f64.{floor, ceil, trunc, nearest}`, and
the existing wasm f64 multiplication for Round. Pow is harder
without a primitive — implement via `f64.pow` if available, else
inline-extend (`expand_pow_via_logexp` is the standard trick).
Eliminates reason 4.

### 4. Pure-wasm broadcasts

`IndirectStore` lowers to a wasm function that walks its address
table and writes each cell directly into the linear-memory
`memory_base` region. Broadcasts already have the address map +
value expression in `DagBroadcast` / `DagPackedBroadcast` — the
walker's `dag_exec_broadcast_with` is the spec. Eliminates reason 5.

### 5. Re-test as you go

After each step, the lockstep `probe-wasm-vs-walker` probe must
still report `no divergence` at 200k ticks on zork1 and doom8088.
The probe is fast — ~3 minutes per cabinet — and catches a
regression immediately. Rerun the 60-second × 3 perf bench at each
step and put the median in [`log.md`](log.md), with a one-paragraph
honest read of whether the step paid off.

Expected end state, on doom8088: wasm in the 30–80 K t/s range
(7–25× over the walker). The mission's ≥5× over the *interpreter*
target (i.e. ≥5× over bytecode = 1.1 M t/s on doom8088) is
ambitious — get there if you can, but if you're stuck at "matches
v1 within 2×" the work is still a meaningful win because the wasm
path is what runs in the browser.

## Hard constraints

These are inherited from the project's cardinal rules
([`compiler-mission.md`](compiler-mission.md)) and from the
broader CSS-DOS contract. Don't violate any of them.

- **Calcite is CSS-generic.** No x86, BIOS, DOS, or cabinet-specific
  knowledge in the codegen. Every recogniser and lowering reasons
  about CSS structural shape only. Genericity probe: would the same
  rule fire on a 6502 cabinet, a brainfuck cabinet, a calculator
  cabinet? If no on all three, you've encoded a specific program
  into calcite.
- **Both targets must build and run.** `wasm-pack build
  crates/calcite-wasm --target web` must succeed, and the cabinet
  must load in the web player without a wasm panic. The browser
  path runs the engine through *its own* wasm JIT (not wasmtime); a
  wasmtime-only feature isn't a feature.
- **Bit-identical to walker.** The Phase 0.5 conformance suite
  passes against v2 walker; wasm must match walker. No "close
  enough" semantics.
- **Real-cart benchmarks only.** Per CLAUDE.md, doom8088 numbers
  are the only ones that ship. Micro-fixtures and short-window
  microbenchmarks are not perf claims. 60-second windows minimum,
  median of 3 runs, machine cooled between runs.

## Tools you have

- `probe-wasm-vs-walker <cabinet> [max_ticks]` — lockstep divergence
  finder. Run after every change.
- `probe-v2-long <cabinet> [seconds]` — fixed wall-clock per backend,
  five-backend speed comparison.
- `probe-dump-termid <cabinet> <term_idx>` and
  `probe-dump-node <cabinet> <node_id>` — structural DAG dump,
  including conditions + super-nodes. Use when a divergence localises
  to a specific terminal.
- `calcite-bench --backend bytecode|dag-v2|dag-v2-wasm
  --budget-secs 60` — the production bench. The
  `bench-zork-3backends.sh` script in the repo root drives all
  three backends across three runs.

## What to NOT do

- **Don't replace the DAG IR.** Phase 2's recognisers (Switch,
  IndirectStore, BitField, BitwiseOp, LoadVarDynamic) are doing
  real work. The wasm rewrite is about the codegen target, not
  the input.
- **Don't touch the walker.** It's the correctness oracle.
- **Don't ship "wasm with helpful host imports."** That's the
  current shape and the data shows it doesn't pay. If you find
  yourself adding a new host import, stop and find another way.
- **Don't bench micro-fixtures.** `tests/conformance/primitives/`
  is for correctness; perf claims must come from real cabinets.

## Pre-flight

Before opening an editor:

- [ ] Read [`compiler-mission.md`](compiler-mission.md) top to
      bottom. Re-read § Cardinal rules.
- [ ] Read [`v2-rewrite-design.md`](v2-rewrite-design.md) end to
      end. Re-read § State model and tick semantics.
- [ ] Run `probe-wasm-vs-walker` on doom8088 to confirm the
      current `Backend::DagV2Wasm` is bit-identical to walker —
      that's the green baseline you must not regress.
- [ ] Run `bench-zork-3backends.sh` on doom8088 to capture the
      "before" numbers for your work. Median of 3, machine cooled.
      Put them in [`log.md`](log.md) before you start changing
      code.

When in doubt, stop and ask — the project owner is the final call
on architecture-shaping decisions.
