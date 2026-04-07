# *calc(ite)*

A JIT compiler for computational CSS. Parses real CSS, recognises computational
patterns, and compiles them into efficient native operations.

Primary target: running [i8086-css](https://github.com/stop-amertime/i8086-css)
(a static binary translator that converts Intel 8086 machine code to CSS) faster
than Chrome's native style resolver. Ultimate goal:
[doom.css](https://github.com/stop-amertime/doom.css) — Doom running in CSS at
playable speed.

## How it works

i8086-css encodes an entire 8086 CPU in CSS custom properties. Each "tick" is
one recalculation of ~3,000 CSS properties. Chrome's style engine evaluates
these via brute-force pattern matching. Calcite replaces that with efficient
native operations:

| Pattern | CSS cost | Calcite cost |
|---|---|---|
| `if(style(--key: 0): ...; style(--key: 1): ...; ...)` (1500+ branches) | O(n) linear scan | O(1) HashMap lookup |
| Per-cell memory writes (`--m0: if(style(--dest: 0): val; else: keep)`) | O(cells) per tick | O(1) direct store |
| Triple-buffer pipeline (`--__0AX`, `--__1AX`, `--__2AX`) | 3x property copies | Eliminated (mutable state) |

Calcite is ~100x faster than Chrome at executing x86 instructions. See
`docs/benchmarking.md` for methodology and numbers.

## The cardinal rule

**Calcite must never do anything the CSS couldn't.** It is to CSS what V8 is to
JavaScript — an optimising evaluator that produces identical results faster.
It reads CSS, it doesn't know what the CSS computes. No x86 knowledge, no
opcode inspection, no instruction-specific logic.

Pattern recognition (dispatch tables, broadcast writes) is fine because it
produces the same result the CSS would, just faster. If the CSS is wrong, the
fix goes in the CSS.

## Project layout

```
crates/
  calcite-core/       Core engine: parser, compiler, evaluator, state
    src/
      parser/         CSS tokenisation and expression parsing
      pattern/        Computational pattern recognition
      compile.rs      Expr trees → flat bytecode
      eval.rs         Tick loop (compiled + interpreted paths)
      state.rs        Flat machine state (registers + memory)
      types.rs        IR type definitions
  calcite-cli/        CLI tool for running CSS through the engine
  calcite-wasm/       WASM bindings for browser Web Worker
web/
  index.html          Browser UI
  calcite-worker.js   Web Worker bridge
tests/
  fixtures/           Pre-compiled CSS from i8086-css (test inputs)
```

## Building

```sh
cargo check --workspace         # typecheck
cargo test --workspace          # run tests
cargo clippy --workspace        # lint
cargo fmt --all                 # format
just check                      # all of the above
```

### WASM (requires [wasm-pack](https://rustwasm.github.io/wasm-pack/))

```sh
wasm-pack build crates/calcite-wasm --target web --out-dir ../../web/pkg
```

### CLI

```sh
# Run N ticks against a CSS file
cargo run -p calcite-cli -- --input path/to/x86css.html --ticks 1000

# Parse only (no execution)
cargo run -p calcite-cli -- --input path/to/x86css.html --parse-only

# With screen rendering (text-mode video memory)
cargo run -p calcite-cli -- --input path/to/x86css.html --ticks 1000 --screen "0xB8000 40x25"
```

### Benchmarks

```sh
cargo bench -p calcite-core
```

Requires `tests/fixtures/x86css-main.css`. See `docs/benchmarking.md`.

## Architecture

### Parser

Built on Servo's `cssparser` crate for tokenisation. Parses the computational
CSS subset: `@property`, `@function` (with parameters/locals/result),
`if(style())`, `calc()`, `var()`, and all CSS math functions (`mod`, `round`,
`min`, `max`, `clamp`, `pow`, `sign`, `abs`). String literals supported for
text output properties.

### Pattern recognition

Detects computational patterns at compile time:

- **Dispatch tables**: `if(style(--prop: N))` chains with 4+ branches on the
  same property → `HashMap<i64, Expr>` for O(1) lookup. This is the single
  biggest optimisation — transforms `readMem()` from O(1600) to O(1).
- **Broadcast writes**: Per-cell `if(style(--dest: N): val; else: keep)`
  assignments with 10+ entries → direct `state[dest] = value`. Handles
  word-write spillover (16-bit writes across adjacent bytes).

### Compiler

Flattens `Expr` trees into linear bytecode (`Op` sequences) operating on
indexed slots instead of string-keyed HashMaps. Also detects function body
patterns (identity, bitmask, shifts, bit extraction) for interpreter fast-paths.

### Evaluator

Two execution modes:
- **Compiled** (default): Runs flat bytecode against a slot array. ~2.85ms/tick.
- **Interpreted**: Walks `Expr` trees directly. Fallback for patterns the
  compiler can't yet handle.

Assignments are topologically sorted by data dependencies. Pre-tick hooks allow
external side effects. Change diffing minimises DOM updates in browser mode.

### State

Flat mutable state replacing CSS's triple-buffered custom properties:
- 14 registers as `i32` (AX through FLAGS), with split-register merging (AH/AL)
- Byte-addressable memory (`Vec<u8>`)
- Unified address space: negative = registers, non-negative = memory
- Auto-sized from `@property` declarations

## License

GNU GPLv3
