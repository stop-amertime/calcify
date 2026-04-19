# calcite-debugger

HTTP debug server for stepping through CSS execution, inspecting state, and
comparing compiled vs interpreted evaluation paths.

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
