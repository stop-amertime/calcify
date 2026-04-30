# calc(ite)
A load-time compiler for computational CSS.


NOTE: this workspace covers multiple repos:
- `./` — calcite (usually the cwd)
- `../CSS-DOS/` — the CSS-DOS transpiler, BIOS, and tools
- `../doom.css/` — Doom port (future)

You may need to work in CSS-DOS when fixing transpiler bugs or updating BIOS
handlers. **If you do, read `../CSS-DOS/CLAUDE.md` first.** It has critical
rules about the architecture, the cardinal rule (CSS is source of truth), and
the canonical PC memory layout. Do not guess at how CSS-DOS works — read its docs.


## Web is the only delivery method

calcite-wasm in the browser is the product. calcite-cli is a test harness.

A consequence of that: **anything that works in calcite-cli but not in
calcite-wasm is broken**, and any code path that exists in calcite-cli but
not in calcite-wasm (loading sidecar files, native-only state setup,
filesystem reads at startup, etc.) is a bug to be removed, not a feature to
preserve. If you find a CLI-only mechanism, your first instinct should be
to delete it and route the same logic through compile-time data on
`CompiledProgram` (which both targets share). Don't add CLI-only
optimisations or workarounds — fix it for the web or don't fix it.

If you are tempted to write `#[cfg(not(target_arch = "wasm32"))]` for
anything beyond what's already documented in the wasm-safety section
below (timing, threads, fs/net), stop and find a different design. The
two targets must produce identical results from the same cabinet.


## Quick reference

```sh
cargo test --workspace          # tests
cargo clippy --workspace        # lint
cargo fmt --all                 # format
just check                      # all three

cargo bench -p calcite-core     # criterion benchmarks (needs fixture)
wasm-pack build crates/calcite-wasm --target web --out-dir ../../web/pkg

See benchmarking and debugging docs for ingo on those. 
```

## Project layout

```
crates/
  calcite-core/      Core engine: parser, pattern compiler, evaluator, state
  calcite-cli/       CLI tool (calcite-cli) + benchmark runner (calcite-bench)
  calcite-debugger/  HTTP debug server — see docs/debugger.md
  calcite-wasm/      WASM bindings (wasm-bindgen) for browser Web Worker
web/
  MOVED TO CSS-DOS/web and /player, see there
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

The CSS-DOS directory beside this one also contains a lot of useful stuff. Its CARTS 

### v2 is underway! 
Work on v2 is underway - read the relevant docs and logs for info on this. 

### v1 Pipeline

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

# CSS-DOS

A complete Intel 8086 PC implemented in pure CSS. The CSS runs in Chrome (in theory - in practise it crashes it)

[Calcite](../calcite) is a JIT compiler that
makes it fast enough to be usable.

## Before starting. 

1. Read the logbook and doc index (auto-loaded below via @ links)
2. Understand the current status, active blocker, and priority list
3. Read the docs relevant to your specific task (the index tells you which)

@docs/log.md

ls the docs/ folder to see what's in it. 

## Mandatory rules

### The checkpoint system

If your task and success criteria are clear, try to be autonomous and not stop working unless you either reach a checkpoint or have a
blocking question for the user. 

A checkpoint requires ALL of:

- [x] Task complete and tested *properly* from a user perspective via web, end-to-end (or user confirmed they tested it)
- [x] Logbook updated (status, entry, what's next)
- [x] New code/features documented in the appropriate docs/ file
- [x] No leftover debris (debug logging, temp files, unclear names)
- [x] GitHub issues updated if relevant

Only then may you stop looping - your task is not finished unless these things are done, just because the code works. 

### Coding Guidelines

Tradeoff: These guidelines bias toward caution over speed. For trivial tasks, use judgment.

1. Think Before Coding
Don't assume. Don't hide confusion. Surface tradeoffs.

Before implementing:

State your assumptions explicitly. If uncertain, ask.
If multiple interpretations exist, present them - don't pick silently.
If a simpler approach exists, say so. Push back when warranted.
If something is unclear, stop. Name what's confusing. Ask.

2. Simplicity First
Minimum code that solves the problem. 
No error handling for impossible scenarios.
If you write 200 lines and it could be 50, rewrite it.
Ask yourself: "Would a senior engineer say this is overcomplicated?" If yes, simplify.

3. Goal-Driven Execution
Define success criteria. Loop until verified.

Transform tasks into verifiable goals:
"Add validation" → "Write tests for invalid inputs, then make them pass"
"Fix the bug" → "Write a test that reproduces it, then make it pass"
"Refactor X" → "Ensure tests pass before and after"
For multi-step tasks, state a brief plan:
1. [Step] → verify: [check]
2. [Step] → verify: [check]
3. [Step] → verify: [check]
Strong success criteria let you loop independently. Weak criteria ("make it work") require constant clarification.

4. - **DO NOT GUESS OR ASSUME FUNCTIONALITY, or unnecessarily reverse-engineer** We have the source code for DOS, 8086 manual, BIOS interrupts,
  FAT12, or kernel behavior in documentation, Doom8088 itself, and so on. Consult the right documentation. Try NOT to reverse-engineer assembly for debugging Use the kernel map file, edrdos source (`../edrdos/`), and Ralf Brown's Interrupt List.

### Git and collaborative coding rules

**Commit and push frequently — it's encouraged.** Plain
`git commit` and `git push` of your own changes don't disturb other
agents' working trees, and stacking up uncommitted work just makes
merge conflicts and lost-work scenarios more likely. Always push to
origin once you've committed.

What requires explicit permission, especially when running
autonomously, is anything that mutates shared state another agent
might be in the middle of using. These commands can wipe their
uncommitted work, rewrite history they've built on, or pollute the
shared index:

- `git stash` (their uncommitted changes vanish into your stash)
- `git add` of files you didn't author / didn't intend
- `git rebase`, `git reset --hard`, `git checkout --` / `git restore`
- `git clean -f`, `git branch -D`
- `git push --force` (especially to main/master — never)
- Any `--no-verify`, `--no-gpg-sign`, or other safety-bypass flag

If you find yourself wanting one of these as a shortcut around an
obstacle, stop and ask — the obstacle is usually a sign of state you
should investigate, not bulldoze.

### Documentation rules
- **Log findings and progress concisely** in the logbook for future agents. It's in docs/log.md

Documentation is incredibly important and an unspoken part of working in this repo. This project is particularly silly and dense, across two repos. Documentation must be automatic, without the user asking specifically for it. Documentation must be epistemically honest. Documentation must be frequent and concise - tokens add up if you waffle. 

### Debugging rules

Your biggest failure mode is coming up with individual candidates for where the bug is, saying 'That's it!' then realising you were wrong, then repeating this multiple timmes. In this particular repo, that is a horrible idea. Checking 5000 places in a few seconds is longer than taking a minute to think deeply in advance about how to isolate the bug holistically, seeing the forest for the trees. 

When debugging, take a second to think what you would advise a senior engineer to do to find the bug. 

- **DO NOT chase bugs speculatively.** Use the debug infrastructure. Add features to the debugger
  if what you need doesn't exist.
- **DO NOT take shortcuts** that accrue tech debt or leave debris in the repo.
- **PREFER creating or updating debugging infrastructure** over speculative individual fixes.
- **Every command needs an explicit ≤2-minute wall-clock cap.** Boot reaches the
  A:\> prompt around tick 2-4M; the slow `pipeline.mjs shoot` path does
  ~1500 ticks/s and will not terminate inside that budget. Use `fast-shoot`
  (calcite-cli, ~375K ticks/s) for late-tick screenshots, or pick a tick
  count the chosen path can reach. Never fire-and-forget a tool hoping it'll
  come back — if there's no path that fits the budget, build one (that's
  how `fast-shoot` and `--dump-mem-range` came to exist).

## The cardinal rule

The CSS is a working program that theoretically runs in Chrome - at least, it's CSS spec-compliant (in reality it crashes Chrome, but that's because it wasn't designed to handle massive CSS files).

The fun of the project comes from doing it in a full-CSS source code. Therefore, Calcite must produce the same results Chrome would (or a theoretically spec-compliant CSS evaluator), just faster.

This means that

- **The CSS must work in Chrome.** If Chrome can't evaluate it, it's wrong.
- **Calcite can't change the CSS.** Only faster evaluation of the same expressions.
- **You may restructure CSS to be easier to JIT-optimise**, as long as
  Chrome still evaluates it the same way and produces the same results.
  Expressing the same computation in a different, more
  pattern-recognisable shape is fine. What is NOT fine: dummy code,
  metadata properties, or side-channels whose only purpose is to 'signal' to calcite or sneak
  information to calcite. The CSS must pay for itself in Chrome.
- **If calcite runs CSS differently to a reference interpreter e.g. Chrome, calcite is wrong.**

### Calcite knows nothing above the CSS layer

Calcite reasons about CSS structural shape and nothing else. The
moment a recogniser, rewrite rule, codegen path, or optimisation knows
about something *above* the CSS — x86, BIOS, DOS, Doom, a specific
cabinet's addresses, Kiln's current emit choices, what `program.json`
says, what the cart is *trying to do* — it has crossed the line.

Operational test: could a calcite engineer who has never seen a CPU
emulator, never read Kiln's source, and doesn't know what a cabinet
contains, derive this rule / recogniser / pass by staring at CSS shape
alone? If yes, fair. If no, it's overfit and one-way-doors the engine
toward Doom. Pattern recognition is welcome — pattern recognition over
*shapes CSS forces emitters into* generalises across cabinets.
Recognition tied to *what those shapes mean upstream* does not.

Genericity probe: would the same rule fire on a 6502 cabinet, a
brainfuck cabinet, a non-emulator cabinet whose CSS happens to share
the structural shape? If no on all three, you've encoded a specific
program into calcite, and every future cabinet will have to fight that
bias.

This is a sharpening of "calcite can't know about x86", not a
replacement. x86 is one example of an upstream layer; the rule covers
all of them.

### The workflow is sacred: load-time compilation only

Calcite must accept any spec-compliant `.css` cabinet at load time and
make it fast — in the browser, on the user's machine, with no build
step on the cabinet author's side, no pre-baked artifact, no allowlist,
no asset pipeline. "Open a `.css` URL, it runs" is the contract.

Compile-once-per-load, run-many is allowed (and is how calcite gets
fast). Distributing pre-compiled cabinets is not — that breaks the
contract. The compile budget is bounded by user patience (a cold open
that takes minutes loses the user) and by the runtime floor it has to
unlock (steady-state must clear playability). Within those, the
compile/run tradeoff is a knob.

## Vocabulary

See [`docs/architecture.md`](docs/architecture.md#vocabulary). In short:

- **cart** — input folder or zip (program + data + optional `program.json`).
- **floppy** — FAT12 disk image the builder assembles internally.
- **cabinet** — output `.css` file, runnable in Chrome/Calcite.
- **Kiln** — the CSS transpiler (`kiln/`).
- **builder** — the orchestrator (`builder/`).
- **BIOSes** — Gossamer (hack), Muslin (assembly DOS BIOS), Corduroy (default C DOS BIOS).
- **player** — static HTML shell for running cabinets in Chrome. 

## Testing and debugging infrastructure

When you're about to run tests, diff against a reference emulator, or
check whether a cart still works: `tests/harness/` is the unified
entrypoint. Start with `node tests/harness/run.mjs smoke` and read
[`docs/TESTING.md`](docs/TESTING.md) for the full tool list.

For "what's on screen at tick N?" against a fresh cabinet, use
`pipeline.mjs fast-shoot <cabinet> --tick=N` — drives `calcite-cli`
directly, ~375K ticks/s, fits boot-completion ticks (2-4M) inside a
~10s budget. The older `pipeline.mjs shoot` path goes through
`calcite-debugger` at ~1500 ticks/s and only terminates for early ticks.
For raw byte dumps without rendering, `calcite-cli --dump-mem-range=ADDR:LEN:PATH`
writes guest memory to a file at end-of-run (repeatable for multiple regions).

In particular: **do not reach for the old `fulldiff.mjs` / `ref-dos.mjs`
/ `compare-dos.mjs` scripts** under `tools/` or `../calcite/tools/` —
they import the deleted `transpiler/` directory and don't work. The
replacement is `node tests/harness/pipeline.mjs fulldiff <cabinet>.css`.

## Quick orientation for CSS-DOS:
from CSS-DOS folder as the CWD
- **Current architecture:** V4 single-cycle. Every instruction completes
  in one CSS tick with a configurable number of memory write slots (minimum 6)
- **Default BIOS:** Corduroy (`bios/corduroy/`). Muslin (`bios/muslin/muslin.asm`) still available.  
- **Build entry:** `node builder/build.mjs <cart>`.
- **Transpiler:** [`kiln/`](kiln/) — see [`kiln/README.md`](kiln/README.md).
- **Cart format:** [`docs/cart-format.md`](docs/cart-format.md).
- **Architecture overview:** [`docs/architecture.md`](docs/architecture.md).
- **Memory layout:** [`docs/memory-layout.md`](docs/memory-layout.md).
- **BIOS details:** [`docs/bios-flavors.md`](docs/bios-flavors.md).
- **Debugging workflow:** [`docs/debugging/workflow.md`](docs/debugging/workflow.md).

## Tools

**NASM** (assembler): `C:\Users\AdmT9N0CX01V65438A\AppData\Local\bin\NASM\nasm.exe`.
Not in PATH. Override via `NASM=` env var.

## Build quick start

from the CSS-DOS folder: 
```sh
# Build a cabinet from a cart
node builder/build.mjs carts/rogue -o rogue.css

# Play it in Chrome
open player/index.html?cabinet=../rogue.css
Or use Playwright! 

# Play it fast via Calcite
cd ../calcite && target/release/calcite-cli.exe -i ../CSS-DOS/rogue.css

# Debug it - use the Calcite MCP server 
```

### Working in a git worktree

When you check CSS-DOS out into a worktree (e.g.
`.claude/worktrees/foo/`), the `../calcite` sibling-repo assumption no
longer holds — relative path resolution from inside the worktree won't
find calcite. Set the `CALCITE_REPO` environment variable to the calcite
repo (or worktree) you want to use:

```sh
# From a CSS-DOS worktree, point at a matching calcite worktree
export CALCITE_REPO=/abs/path/to/calcite/.claude/worktrees/foo
```

`CALCITE_REPO` is honoured by:

- `web/scripts/dev.mjs` — vite aliases (`/calcite/`, `/bench-assets/`)
  and the `_reset` step that rebuilds the calcite WASM.
- `tests/harness/bench-doom-stages-cli.mjs`, `lib/fast-shoot.mjs`,
  `lib/debugger-client.mjs` — locate the `calcite-cli` /
  `calcite-debugger` binaries.

`CALCITE_CLI_BIN` and `CALCITE_DEBUGGER_BIN` still take precedence over
`CALCITE_REPO` if you need to point at a specific binary directly
(useful when the binary's been pre-built somewhere outside the worktree).


### Wasm safety — everything calcite-core touches must build and run on wasm32

`calcite-wasm` wraps `calcite-core` and runs in the browser via the web
player. `calcite-core` therefore has to stay wasm32-clean: anything the web
build exercises must not panic or fail to link on `wasm32-unknown-unknown`.

**Rules:**

- **Never use `std::time::Instant` or `std::time::SystemTime`** anywhere
  `calcite-core` or `calcite-wasm` can reach. Raw `std::time` panics on
  `wasm32-unknown-unknown` with "time not implemented on this platform".
  Use `web_time::Instant` instead — it transparently maps to `performance.now()`
  on wasm and `std::time::Instant` on native. (The `web_time` crate is already
  a workspace dependency.)
- **Same applies to anything with a hidden `SystemTime`/`Instant` dependency:**
  `std::thread::sleep`, random seeding from `SystemTime`, `std::sync::Mutex`
  timeouts, etc. Audit before adding.
- **No `std::thread::spawn`** or OS-thread APIs. Wasm is single-threaded in
  our setup. If you need concurrency for native-only code paths, gate it
  with `#[cfg(not(target_arch = "wasm32"))]`.
- **No `std::fs`, `std::net`, `std::process::Command`, or `std::env` writes**
  in core paths. Reads (`std::env::var`) are tolerated by the wasm stub but
  avoid them in hot paths.
- **Dev-only timing and logging (`CALCITE_PROFILE_COMPILE`, env-driven debug
  dumps) must still use `web_time`** — they run on every `compile()` call,
  including the one invoked from wasm, even when the feature is "disabled"
  (the scope constructor still touches `Instant`).

**If you must use something native-only**, cfg-gate it:

```rust
#[cfg(not(target_arch = "wasm32"))]
{
    // native-only code here
}
```

**How to verify:** `wasm-pack build crates/calcite-wasm --target web --out-dir ../../web/pkg`
must succeed with no errors, and the resulting cabinet must load in the web
player without a `WASM panic:` line in the browser console.

When in doubt: the crates a wasm build pulls in are `calcite-core` and
`calcite-wasm`. Anything else (`calcite-cli`, `calcite-debugger`, the
benchmark and probe bins) is native-only and can use `std::time` freely.

### Relationship to CSS-DOS

[CSS-DOS](../CSS-DOS) is a sibling repo that generates 8086 CSS. It uses a
JS→CSS transpiler (`transpiler/`) with a v3 cycle-accurate microcode execution
model. BIOS handlers are microcode (not assembly). See its `CLAUDE.md` and
`V3-PLAN-1.md` for the full architecture.

Calcite's only interface with CSS-DOS is the `.css` output — test fixtures in
`tests/fixtures/` are pre-compiled CSS. There is no crate dependency and must
never be one.


### Conformance testing — the main debugging workflow

**The conformance harness lives in `../CSS-DOS/tests/harness/`.** Use

    node ../CSS-DOS/tests/harness/pipeline.mjs fulldiff <cabinet>.css --max-ticks=10000

as the first-line divergence finder. See
[`../CSS-DOS/docs/TESTING.md`](../CSS-DOS/docs/TESTING.md) and
`../CSS-DOS/tests/harness/README.md` for the full tool list.

The legacy tools in `tools/fulldiff.mjs`, `tools/ref-dos.mjs`, and
`tools/diagnose.mjs` depend on the deleted `../CSS-DOS/transpiler/` and
don't run — their headers now say so. Don't try to "fix" them; the
harness is the replacement.

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
