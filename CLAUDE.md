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
```

## Project layout

```
crates/
  calcite-core/    Core engine: parser, pattern compiler, evaluator, state
  calcite-cli/     CLI tool for running CSS through the engine
  calcite-wasm/    WASM bindings (wasm-bindgen) for browser Web Worker
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

Calcite must NEVER have x86 knowledge. No opcode reading, no instruction
semantics, no emulation. It evaluates CSS expressions — it doesn't know or
care what they compute. Pattern recognition is fine (same results, faster).
If the CSS is wrong, the fix goes in the CSS, not in calcite.

### Relationship to i8086-css

[i8086-css](../i8086-css) is a sibling repo that transpiles 8086 binaries to
CSS. Calcite's only interface with it is the `.css` output — test fixtures in
`tests/fixtures/` are pre-compiled CSS from i8086-css. There is no crate
dependency and must never be one.

### Conformance testing

Optional `conformance` feature (enables serde). Captures `StateSnapshot`
(registers + memory) per tick, diffs against Chrome baseline in
`tests/fixtures/chrome-baseline.json`.
