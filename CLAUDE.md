NOTE: this workspace covers: 
./ calcite (usually the cwd)
../i8086-css (the next folder over)
more subfolders/repos for individual other games like doom.css eventually

As an agent, you are working in all three. If a fix needs to go in i8086-css (hopefully soon to be CSS-DOS) put it there. Just because you are in the calcite cwd doesn't mean you can only work on calcite. 

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
tests/
  fixtures/            Pre-compiled CSS from i8086-css
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
  in Chrome. Telling i8086-css to "not emit properties" or restructure
  things for calcite's benefit is backwards — like telling a JS developer
  to write worse code so V8 can optimise it.
- **No features that don't exist in CSS.** If Chrome can't evaluate it,
  calcite can't rely on it. Calcite can be smarter about evaluating CSS
  patterns, but it can't invent new semantics.
- **If calcite disagrees with Chrome, calcite is wrong.**
- **Pattern recognition is the whole game.** Recognising that 6000
  assignments all check the same property and converting them to a HashMap
  lookup is exactly what a JIT does. Same results, faster. That's the job.

### Relationship to CSS-DOS (formerly i8086-css)

[CSS-DOS](../i8086-css) is a sibling repo that generates 8086 CSS. It is
undergoing an architecture pivot from a JSON instruction database approach
(now in `legacy/`) to a JS→CSS transpiler (see `transpiler/`). See its
`CLAUDE.md` for details.

Calcite's only interface with CSS-DOS is the `.css` output — test fixtures in
`tests/fixtures/` are pre-compiled CSS. There is no crate dependency and must
never be one.

### Conformance testing — the main debugging workflow

The ground truth is `tools/js8086.js`, a reference 8086 emulator in JavaScript.
The workflow for finding and fixing bugs is:

1. **Run the JS reference emulator against calcite and find where they diverge.**
   The primary tool is `../i8086-css/tools/compare-dos.mjs` (for DOS
   boot) or `tools/compare.mjs` (for simple BIOS .COM programs). These run
   both emulators tick-by-tick and report the first register mismatch with
   full context (instruction bytes, register diffs, trace around divergence).

2. **Investigate the divergence.** Use the HTTP debugger to inspect calcite's
   CSS property state at the divergent tick. `tools/diagnose.mjs` automates
   this — it cross-references every CSS property against what the reference
   emulator expects, showing exactly which property computed wrong.

3. **Fix the bug.** Bugs are OFTEN in the CSS generator (i8086-css transpiler),
   not in calcite. When compiled and interpreted paths agree but diverge from
   the reference emulator, it's a CSS bug — fix it in i8086-css regardless of
   which repo you're working in. Fix, re-run the comparison, repeat.

```sh
# DOS boot conformance (runs in ../i8086-css):
cd ../i8086-css
node tools/compare-dos.mjs --ticks=5000

# Simple .COM program conformance:
node tools/compare.mjs program.com bios.bin program.css --ticks=5000

# Deep diagnosis at a specific divergence (start debugger first):
cargo run --release -p calcite-debugger -- -i program.css
node tools/diagnose.mjs program.com bios.bin --ticks=5000
# or for DOS boot:
node tools/diagnose.mjs --dos --ticks=5000
```

Key tools:
- `../i8086-css/tools/compare-dos.mjs` — **primary**: JS vs calcite for DOS boot
- `../i8086-css/tools/ref-emu-dos.mjs` — run JS emulator standalone (DOS)
- `tools/compare.mjs` — JS vs calcite for simple BIOS programs
- `tools/diagnose.mjs` — property-level diagnosis via HTTP debugger
- `tools/ref-emu.mjs` — run JS emulator standalone (simple BIOS)

### Running programs

Drop any `.com` file into `programs/` and use `run.bat`:

```
run.bat              Interactive menu — pick a program by number
run.bat diagnose     Conformance diagnosis menu
```

CSS is auto-generated via the DOS transpiler pipeline and cached in
`programs/.cache/`. The generator lives at `../i8086-css/transpiler/generate-dos.mjs`.
