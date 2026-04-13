NOTE: this workspace covers multiple repos:
- `./` — calcite (usually the cwd)
- `../CSS-DOS/` — the CSS-DOS transpiler, BIOS, and tools
- `../doom.css/` — Doom port (future)

You may need to work in CSS-DOS when fixing transpiler bugs or updating BIOS
handlers. **If you do, read `../CSS-DOS/CLAUDE.md` first.** It has critical
rules about the architecture, the cardinal rule (CSS is source of truth), and
the canonical PC memory layout. Do not guess at how CSS-DOS works — read its docs.

Similarly, if a CSS-DOS agent needs calcite changes, they should read this file
first. The repos are independent (no code dependency) but tightly coupled in
practice: CSS-DOS generates CSS, calcite runs it.


# calc(ite)

A JIT compiler for computational CSS.

## Quick reference

```sh
cargo test --workspace          # tests
cargo clippy --workspace        # lint
cargo fmt --all                 # format
just check                      # all three

cargo bench -p calcite-core     # criterion benchmarks (needs fixture)
wasm-pack build crates/calcite-wasm --target web --out-dir ../../web/pkg

# Debug server: parse once, step/inspect/compare via HTTP
cargo run --release -p calcite-debugger -- -i program.css
# Then: curl localhost:3333/state, /tick, /seek, /compare-paths, etc.
# See docs/debugger.md for full API
```

## Project layout

```
crates/
  calcite-core/      Core engine: parser, pattern compiler, evaluator, state
  calcite-cli/       CLI tool for running CSS through the engine
  calcite-debugger/  HTTP debug server — see docs/debugger.md
  calcite-wasm/      WASM bindings (wasm-bindgen) for browser Web Worker
web/
  index.html           Browser UI
  calcite-worker.js    Web Worker bridge
site/
  index.html           CSS-DOS showcase site
  programs/            Pre-compiled .css.gz program files for the site
programs/
  *.com, *.exe         DOS programs to run
output/
  *.css                Pre-built CSS files (run directly, no regeneration)
tools/
  js8086.js            Reference 8086 emulator (vendored from emu8)
  fulldiff.mjs         Primary: first-divergence finder (REP-aware, full FLAGS)
  diagnose.mjs         Property-level CSS diagnosis at divergence point
  compare.mjs          Tick-by-tick comparison for simple BIOS .COM programs
  ref-emu.mjs          Standalone reference emulator (simple BIOS programs)
  ref-dos.mjs          Standalone reference emulator (DOS boot mode)
tests/
  fixtures/            Pre-compiled CSS from CSS-DOS
run.bat                Interactive menu to run/diagnose DOS programs
```

## Architecture

### Pipeline

```
CSS text → parse → pattern recognition → compile → bytecode → evaluate (tick loop)
```

1. **Parser** (`parser/`): Tokenises via `cssparser` crate. Parses `@property`,
   `@function`, `if(style())`, `calc()`, `var()`, CSS math functions, string
   literals. Output: `ParsedProgram` (properties + functions + assignments).

2. **Pattern recognition** (`pattern/`): Detects optimisable structures:
   - Dispatch tables: `if(style(--prop: N))` chains (≥4 branches) → HashMap
   - Broadcast writes: `if(style(--dest: N): val; else: keep)` (≥10 entries)
     → direct store, with word-write spillover support

3. **Compiler** (`compile.rs`): Flattens `Expr` trees → flat `Op` bytecode
   with indexed slots. Function body patterns (identity, bitmask, shifts,
   bit extraction) detected for interpreter fast-paths.

4. **Evaluator** (`eval.rs`): Runs the tick loop. Two paths:
   - Compiled: linear bytecode against slot array (fast path)
   - Interpreted: recursive Expr walking (fallback)
   Topological sort on assignments. Pre-tick hooks for side effects.

5. **State** (`state.rs`): Flat mutable replacement for CSS triple-buffer.
   Unified address space (negative = registers, non-negative = memory).
   Split-register merging (AH/AL ↔ AX). Auto-sized from @property decls.

### Key types (`types.rs`)

- `Expr` — expression tree (Literal, Var, Calc, StyleCondition, FunctionCall, etc.)
- `ParsedProgram` — parser output (properties, functions, assignments)
- `Assignment` — property name → Expr
- `FunctionDef` — name, parameters, locals, result expression
- `PropertyDef` — name, syntax, initial value, inheritance
- `CssValue` — Integer | String

### The cardinal rule

The entire point of this project is to see what pure CSS can do. The CSS
is a working program that runs in Chrome — no JavaScript, no WebAssembly,
just CSS custom properties and `calc()` evaluating an 8086 CPU. That's the
joke and the demo: "Doom runs in a stylesheet."

The CSS must be completely self-contained and functional on its own.
Chrome is the reference implementation. If you open the HTML file in Chrome,
it works. Slowly — maybe one frame per year — but it works. Every memory
cell, every register, every instruction decode is a CSS expression.

Calcite exists to make that CSS fast enough to be usable. It is a JIT
compiler for CSS, analogous to V8 for JavaScript. It parses the CSS, finds
patterns it can execute more efficiently, and produces the same results
Chrome would — just orders of magnitude faster.

What this means in practice:

- **Calcite must NEVER have x86 knowledge.** No opcode reading, no
  instruction semantics, no emulation. It evaluates CSS expressions — it
  doesn't know or care what they compute.
- **The CSS dictates everything, not calcite.** If the CSS has 6000 memory
  cell properties, calcite evaluates 6000 memory cell properties. If it
  needs a million properties for 1MB of RAM, calcite handles a million
  properties. It can recognise patterns and optimise (broadcast writes,
  dispatch tables), but it cannot skip, remove, or alter what the CSS
  expresses — just like V8 can't skip your JavaScript, only run it faster.
- **Never suggest CSS changes to help calcite.** The CSS is written to work
  in Chrome. Telling CSS-DOS to "not emit properties" or restructure
  things for calcite's benefit is backwards — like telling a JS developer
  to write worse code so V8 can optimise it.
- **No features that don't exist in CSS.** If Chrome can't evaluate it,
  calcite can't rely on it. Calcite can be smarter about evaluating CSS
  patterns, but it can't invent new semantics.
- **If calcite disagrees with Chrome, calcite is wrong.**
- **Pattern recognition is the whole game.** Recognising that 6000
  assignments all check the same property and converting them to a HashMap
  lookup is exactly what a JIT does. Same results, faster. That's the job.

### Relationship to CSS-DOS

[CSS-DOS](../CSS-DOS) is a sibling repo that generates 8086 CSS. It uses a
JS→CSS transpiler (`transpiler/`) with a v3 cycle-accurate microcode execution
model. BIOS handlers are microcode (not assembly). See its `CLAUDE.md` and
`V3-PLAN-1.md` for the full architecture.

Calcite's only interface with CSS-DOS is the `.css` output — test fixtures in
`tests/fixtures/` are pre-compiled CSS. There is no crate dependency and must
never be one.

### Conformance testing — the main debugging workflow

**Read `docs/conformance-testing.md` before doing any debugging work.** It
has the full tool reference, usage examples, and step-by-step workflows.

The key principles:

- **Always diff against the reference.** `tools/js8086.js` is a reference
  8086 emulator that serves as ground truth. When calcite and the reference
  disagree, calcite is wrong (or the CSS is wrong). Never assume calcite is
  correct — always verify with the reference.
- **Bugs are often in the CSS generator**, not calcite. When compiled and
  interpreted paths agree but diverge from the reference, it's a transpiler
  bug — fix it in `../CSS-DOS/transpiler/`. Read CSS-DOS's CLAUDE.md first.
- **Use the debugger** (`docs/debugger.md`) to inspect calcite's internal
  state at any tick, compare compiled vs interpreted paths, and read memory.

### Running programs

Drop any `.com` or `.exe` file into `programs/` and use `run.bat`:

```
run.bat              Interactive menu — pick a program by number
run.bat diagnose     Conformance diagnosis menu
```

CSS is regenerated from source on every run via the DOS transpiler pipeline
(`../CSS-DOS/transpiler/generate-dos.mjs`). Pre-built `.css` files in `output/`
can also be run directly from the menu without regeneration.
