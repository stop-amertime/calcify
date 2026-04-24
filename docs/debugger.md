# calcite-debugger

Debug server for stepping through CSS execution, inspecting state, and
comparing compiled vs interpreted evaluation paths. Speaks both HTTP (port
3333 by default) and MCP (stdio or TCP); the MCP surface is what agents and
the test harness drive.

> If you are an agent driving this from Claude Code or the harness, read the
> **[Agent-oriented tooling](#agent-oriented-tooling)** section first — it
> lists the tools that were added specifically to stop agents hanging on
> `run_until`, chasing bytes, or failing to isolate divergences.

## Quick start

```sh
cargo run --release -p calcite-debugger -- -i path/to/program.css
```

Server starts on port 3333 (change with `-p PORT`).

## Rebuilding after source edits

The release binary is often held open by a running debugger (the MCP
client keeps one resident across tool calls, and orphan runs from
interrupted sessions can linger). When that happens `cargo build
--release` fails at link time with:

```
ld.exe: cannot open output file ...\deps\calcite_debugger-HASH.exe: Permission denied
```

Use the convenience script:

```sh
./kill-and-rebuild.bat
```

It kills every `calcite-debugger.exe` process (including the one your
MCP client is attached to — it will respawn on the next tool call) and
then runs `cargo build --release -p calcite-debugger`. Exit code
propagates from cargo, so you can chain it.

## Usage

```sh
# Step forward 100 ticks
curl -X POST localhost:3333/tick -d '{"count":100}'

# See full register + property state
curl localhost:3333/state

# Jump to any tick (uses checkpoints for speed)
curl -X POST localhost:3333/seek -d '{"tick":8178}'

# Read IVT memory (INT 0x10 at 0x40 = addresses 64-71)
curl -X POST localhost:3333/memory -d '{"addr":64,"len":8}'

# Render video memory
curl -X POST localhost:3333/screen

# Compare compiled vs interpreted at current tick
curl localhost:3333/compare-paths

# Compare against a reference trace (stops at first divergence)
curl -X POST localhost:3333/compare -d '{"reference":[...],"stop_at_first":true}'

# Shutdown
curl -X POST localhost:3333/shutdown
```

## Endpoints

| Method | Path | Body | Description |
|--------|------|------|-------------|
| GET | `/info` | — | Session metadata, property/function counts, snapshot list |
| GET | `/state` | — | Current tick, all registers, all computed property values |
| POST | `/tick` | `{"count": N}` | Advance N ticks (default 1). Returns changes. |
| POST | `/seek` | `{"tick": N}` | Jump to tick N. Restores from nearest checkpoint, replays forward. |
| POST | `/memory` | `{"addr": N, "len": N}` | Hex + byte + word dump of memory region (default 256 bytes). |
| POST | `/screen` | `{"addr": N, "width": N, "height": N}` | Render text-mode video memory. Auto-detects config. |
| POST | `/compare` | `{"reference": [...], "stop_at_first": bool}` | Diff registers against a reference trace (JSON array of tick objects). |
| GET | `/compare-paths` | — | Run current tick through BOTH compiled and interpreted paths, diff all registers + memory. |
| POST | `/compare-state` | `{"registers": {...}, "memory": [{"addr": N, "len": N, "bytes": [...]}]}` | Diff current state against expected register values and/or memory byte ranges. |
| POST | `/dump-ops` | `{"start": N, "end": N}` | Dump a range of compiled bytecode ops as human-readable lines. |
| POST | `/ops` | `{"property": "--NAME"}` | Dump compiled ops for a single property. Returns 404 if the property isn't compiled or doesn't exist. |
| GET | `/slot-map` | — | List every compiled slot with its property name and current value. Useful for decoding `/dump-ops` output. |
| POST | `/trace-property` | `{"property": "--NAME"}` | Step forward one tick and report every op that wrote to the given property's slot. |
| POST | `/watchpoint` | `{"addr": N, "max_ticks": M, "from_tick": T?, "expected": V?}` | Run forward until the byte at `addr` changes (or reaches `expected` if set). Stops at `max_ticks`. |
| POST | `/key` | `{"value": N}` | Push a raw scancode/ASCII value directly into the BDA keyboard buffer. |
| POST | `/keyboard` | `{"value": N}` | Set the `--keyboard` CSS state variable (v3 microcode path: edge-detected → IRQ 1 → INT 09h). Use `(scancode<<8)\|ascii` for press, `0` for release. |
| POST | `/snapshot` | — | Create a manual checkpoint at the current tick. |
| GET | `/snapshots` | — | List all checkpoint ticks. |
| POST | `/shutdown` | — | Stop the server. |

## Checkpoints

Automatic checkpoints are created every `--snapshot-interval` ticks (default
1000). `/seek` uses the nearest checkpoint to avoid replaying from tick 0.
Create manual checkpoints with `/snapshot` before investigating a specific tick.

## Typical debugging workflow

### Finding a compiled vs interpreted divergence

```sh
# Start server
cargo run --release -p calcite-debugger -- -i program.css

# Binary search for first diverging tick
for tick in 0 10 100 1000 5000; do
    curl -sX POST localhost:3333/seek -d "{\"tick\":$tick}" > /dev/null
    curl -s localhost:3333/compare-paths | python3 -c "
import json,sys; d=json.load(sys.stdin)
print(f'tick {d[\"tick\"]}: {d[\"total_diffs\"]} diffs')
"
done

# Once found, inspect the divergence
curl -s localhost:3333/compare-paths | python3 -m json.tool
```

### Conformance testing against the reference emulator

The debugger is the backbone of conformance testing — tools like
`fulldiff.mjs` and `diagnose.mjs` drive it via HTTP. See
`docs/conformance-testing.md` for the full tool reference and workflows.

### Inspecting bytecode shape (for optimisation work)

```sh
# List every slot and its current value
curl -s localhost:3333/slot-map | python3 -m json.tool

# Dump the compiled ops for a specific property
curl -sX POST localhost:3333/ops -d '{"property":"--memory_A0000"}' | python3 -m json.tool

# Dump a range of ops from the global stream (use with /slot-map to decode)
curl -sX POST localhost:3333/dump-ops -d '{"start":0,"end":200}' | python3 -m json.tool

# Land on the tick where a specific memory address first changes,
# then inspect the ops that ran around that point
curl -sX POST localhost:3333/watchpoint -d '{"addr":655360,"max_ticks":200000}'
```

### Inspecting memory regions

```sh
# IVT (interrupt vector table) at 0x0000
curl -sX POST localhost:3333/memory -d '{"addr":0,"len":1024}'

# Stack (SP-relative)
SP=$(curl -s localhost:3333/state | python3 -c "import json,sys; print(json.load(sys.stdin)['registers']['SP'])")
curl -sX POST localhost:3333/memory -d "{\"addr\":$SP,\"len\":32}"

# Video memory
curl -sX POST localhost:3333/screen
```

## CLI options

```
-i, --input <PATH>              CSS file to debug
-p, --port <PORT>               HTTP port (default: 3333)
    --snapshot-interval <N>     Ticks between auto-checkpoints (default: 1000)
```

## Agent-oriented tooling

The debugger exposes an MCP surface alongside the HTTP server. Agents and
the CSS-DOS test harness drive it via MCP; the tools listed below were added
specifically to address the recurring failure modes of agentic debugging
(hanging on `run_until`, chasing individual bytes instead of isolating where
a divergence lives, having no known-good baseline to diff against).

For harness-side access, every tool here is wrapped in
[`CSS-DOS/tests/harness/lib/debugger-client.mjs`](../../CSS-DOS/tests/harness/lib/debugger-client.mjs),
which folds the `session` parameter in automatically. For agent access
directly from Claude Code, register the server (stdio or HTTP) and call
the `mcp__calcite-debugger__*` tools.

Every MCP tool takes a `session` parameter. Use `open` to create or rehydrate
a session and name it; subsequent calls must use the same name.

### Bug isolation

| Tool | When to use |
|------|-------------|
| `inspect_packed_cell` | Given a cell index, reports the state-var address, its current value, and whether the packed-cell table matches expectations. First tool to reach for when "a write appears to go nowhere" — proves whether the cell exists in the compiled program. |
| `diff_packed_memory` | Compares packed-cell backing across snapshots or sessions. Spot where two runs diverge without byte-scanning the whole address space. |
| `compare_paths` | Runs both the compiled and interpreted evaluators on the current state, reports which properties differ. First-line check for calcite correctness bugs. |
| `compare_reference` | Diffs calcite against the js8086 reference emulator. Use when chasing "is this our bug or the CSS's bug?". |
| `compare_state` | Diffs current registers/memory against an expected snapshot you supply. For regression tests. |
| `entry_state_check` | Validates the post-compile initial state matches what kiln emitted. Catches state-wiring regressions (e.g. `packed_cell_table` never populated from compiled program) that would otherwise show up as silent read-zeros much later in the run. |

### Navigating execution with safety rails

| Tool | When to use |
|------|-------------|
| `run_until` / `run_until_poll` / `run_until_cancel` | Job-based async `run_until` — kick off a long run, poll status, cancel cleanly. Replaces the synchronous `run_until` that hung indefinitely when conditions never matched. |
| `seek(tick)` | Jump directly to a target tick using the nearest checkpoint. Avoids stepping one tick at a time. |
| `watchpoint(addr, max_ticks, expected?)` | Runs until the byte at `addr` changes (or reaches `expected` if supplied). The fast way to find the tick a specific byte flips. |
| `tick(count)` | Advances N ticks. Count is bounded — agents can't ask for millions in one call without hitting the MCP-side budget. Prefer `seek` for bulk advancement. |
| `snapshots` | `{action: "create"}` checkpoints the current state; `{action: "list"}` shows all checkpoints. Makes `seek` cheap for repeated exploration. |

### Inspection

| Tool | When to use |
|------|-------------|
| `read_memory(addr, len)` | Reads through the unified address space (packed cells → extended HashMap → flat shadow, in that priority). Always authoritative for what the CPU would see. |
| `render_screen` | Returns framebuffer (mode 13h) or text VRAM as structured data plus a textual rendering. Visual inspection without round-tripping through screenshots. |
| `trace_property` | Records a property's value every tick across a range. For watching how a signal evolves. |
| `slot_map` / `dump_ops` | Exposes the compiled program's slot assignments and op sequence, so `run_until` / watchpoint output can be correlated to bytecode. |
| `execution_summary` / `summarise` | Compact per-tick summary (opcode, IP, key register deltas) for skimming thousands of ticks without drowning in JSON. |

### Session plumbing

| Tool | When to use |
|------|-------------|
| `open(path, session)` | Load a cabinet and name the session. Supports multiple concurrent sessions in one debugger process — pass different `session` names to diff PACK=1 vs PACK=2 side-by-side. |
| `close_session(session)` | Tears down a session (frees memory, closes file handle). |
| `info` | Server-wide session list and snapshot state. Cheap liveness check. |
| `send_key(value, target?)` | Injects a keystroke into the BDA/keyboard. For driving interactive programs through boot sequences. |

### Typical agent workflow

1. `open` the cabinet(s) under session names.
2. `seek` to the suspected divergence tick (or `watchpoint` on a specific byte if the tick is unknown).
3. `compare_reference` or `compare_paths` to localise which side is wrong.
4. `inspect_packed_cell` / `read_memory` / `trace_property` to confirm the exact misbehavior.
5. `snapshots` + `run_until_poll` to keep repeated exploration cheap.

### Concrete example — finding the PACK=2 framebuffer bug (2026-04-23)

A PACK=2 zork1 splash rendered black where PACK=1 rendered gray. The bug was
isolated in minutes rather than hours by following the workflow above:

1. `open` zork-p1.css as `p1`, zork-p2.css as `p2` (one debugger, two
   sessions).
2. `seek` both to tick 140000.
3. `read_memory(0xA0000, 128)` on both — `p1` showed `08 08 08 08 …`, `p2`
   showed all zeros. Framebuffer diverges.
4. `read_memory(0x100000, 48)` on both — DAC bytes byte-identical. Rules
   out a palette problem.
5. `inspect_packed_cell(327680)` on `p2` — `cell_addr=0`, meaning the
   framebuffer cell is missing from the compiled CSS entirely. Root
   cause: the cart's `memory.gfx: false` pruned the mode 13h zone even
   though Corduroy's splash always writes it.

The step that would otherwise have taken an agent hours (staring at memory
diffs, guessing) collapsed to a single `inspect_packed_cell` call.
