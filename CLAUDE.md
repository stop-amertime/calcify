# calc(ite)

A JIT compiler for computational CSS. Parses real CSS, recognises computational
patterns, and compiles them into efficient native operations via WASM.

## Project Layout

```
crates/
  calcite-core/    Core engine: parser, pattern compiler, evaluator, state
  calcite-cli/     CLI tool for running CSS through the engine
  calcite-wasm/    WASM bindings (wasm-bindgen) for browser Web Worker
web/
  calcite-worker.js   JS Web Worker bridge
```

## Development

```sh
cargo check --workspace         # typecheck
cargo test --workspace          # run tests
cargo clippy --workspace        # lint
cargo fmt --all                 # format

# Build WASM (requires wasm-pack)
wasm-pack build crates/calcite-wasm --target web --out-dir ../../web/pkg
```

## Architecture

See `css-compute-engine-spec-v2-1.md` for the full spec. Key modules:

- **parser**: CSS tokenisation via `cssparser` crate, custom parsing for
  `@function`, `@property`, `if(style())`, `calc()`, `var()`.
- **pattern**: Recognises computational patterns (dispatch chains → hash maps,
  broadcast writes → direct stores, bit decomposition → native bitwise ops).
- **eval**: The tick loop — runs compiled programs against flat state.
- **state**: Flat machine state replacing CSS's triple-buffered custom properties.
  Uses x86CSS's address conventions (negative = registers, positive = memory).
- **types**: IR type definitions (Expr tree, parsed/compiled program structs).
