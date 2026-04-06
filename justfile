# calc(ify) development commands

# Run all checks (test, clippy, fmt)
check:
    cargo test --workspace
    cargo clippy --workspace -- -D warnings
    cargo fmt --all -- --check

# Run tests
test:
    cargo test --workspace

# Run tests with output
test-verbose:
    cargo test --workspace -- --nocapture

# Lint
clippy:
    cargo clippy --workspace -- -D warnings

# Format
fmt:
    cargo fmt --all

# Build WASM (requires wasm-pack: cargo install wasm-pack)
wasm:
    wasm-pack build crates/calcify-wasm --target web --out-dir ../../web/pkg

# Run CLI against a CSS file
run file ticks="1":
    cargo run -p calcify-cli -- --input {{file}} --ticks {{ticks}}

# Run CLI in parse-only mode
parse file:
    cargo run -p calcify-cli -- --input {{file}} --parse-only
