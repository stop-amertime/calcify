# calcite-debugger daemon mode

Keeps session state alive across MCP client reconnects.

## The problem it solves

An MCP client (Claude Code, etc.) spawns the calcite-debugger binary as
a child process and talks to it over stdio. When the client restarts,
crashes, or kills the child after a timeout, the child dies — and with
it, every loaded CSS program, every snapshot, every in-flight run_until
job. Reloading a DOS cabinet costs 9–18s of parse+compile, plus however
many ticks of execution the client had advanced.

Daemon mode splits the process in two:

- **Daemon** (`calcite-debugger --listen HOST:PORT`) runs once, keeps
  all state in memory, accepts MCP-over-TCP connections serially.
- **Shim** (`tools/mcp-shim.mjs`) is what `.mcp.json` spawns. It's a
  ~100-line Node script that forwards stdio frames to the daemon's TCP
  socket. The shim is disposable; the daemon outlives it.

When a client reconnects it spawns a fresh shim, which connects to the
same daemon and sees the same sessions. No reload.

## Usage

### First time / after a reboot

Let the shim autostart the daemon. Nothing to do — the first client
that connects triggers `mcp-shim.mjs` to spawn the daemon detached,
wait for the port to open, then forward.

### Manual control

```sh
# Pre-warm the daemon before launching a client
./start-debugger-daemon.bat

# Stop the daemon (releases sessions; in-memory state is lost)
taskkill /F /IM calcite-debugger.exe

# Rebuild + restart (during debugger development)
./kill-and-rebuild.bat
```

### Configuration

| Env var                   | Default              | Purpose                          |
|---------------------------|----------------------|----------------------------------|
| `CALCITE_DEBUGGER_ADDR`   | `127.0.0.1:3334`     | Host:port for the daemon         |
| `CALCITE_DEBUGGER_BIN`    | `target/release/…`   | Override binary path for autostart |

The shim reads these; so does `start-debugger-daemon.bat`.

## Wiring `.mcp.json`

Point the `command` at the shim, not the binary:

```json
{
  "mcpServers": {
    "calcite-debugger": {
      "command": "node",
      "args": ["C:/path/to/calcite/tools/mcp-shim.mjs"]
    }
  }
}
```

With the binary directly (old, state-loses-on-reconnect):

```json
{
  "mcpServers": {
    "calcite-debugger": {
      "command": "C:/path/to/calcite/target/release/calcite-debugger.exe"
    }
  }
}
```

Both still work. The difference is whether you want persistent sessions.

## Session state is still volatile

Daemon mode survives **client** restarts. It does **not** survive:

- Killing the daemon process (obvious)
- Rebuilding the binary with `kill-and-rebuild.bat` (it kills the daemon)
- OS reboot
- Daemon crash

For a persistent debugger across those events, you'd need on-disk
serialization of snapshots + compile cache — that's not implemented.
For now, if any of these happen, open the session again with the same
name. The parse/compile cost is one-shot, then you're back.

## Session-name contract

Every tool call requires a `session` field. The daemon keeps sessions
keyed by that name. Across clients sharing one daemon:

- Agents using **different** names get isolated sessions.
- Agents using the **same** name share state — deliberate collaboration.

After a reconnect, calling `open` with an existing session name
**replaces** the loaded program in that slot. Use `info` to enumerate
live sessions first if you're unsure what's already there.

## Single-client-at-a-time caveat

The current daemon accepts TCP connections serially: one active MCP
connection at a time. If a second shim connects while the first is
still alive, it blocks on accept until the first disconnects. This is
intentional — the underlying `DebuggerHandler::serve(transport)`
awaits until disconnect, and `rmcp` doesn't multiplex connections
onto one handler by default.

Two-agent-at-once scenarios still work as long as both agents share
**one** MCP client (one shim process), addressing distinct sessions
by name. That's the common case.

If genuine concurrent clients become a need, we'd spawn one
`handler.serve()` task per accepted connection. The handler is `Clone`
and all its state is `Arc`-wrapped, so this is ~5 lines of code in
`main.rs`. Deferred until someone asks.
