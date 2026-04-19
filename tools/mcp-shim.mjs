#!/usr/bin/env node
// mcp-shim.mjs — stdio ↔ TCP bridge for the calcite-debugger daemon.
//
// The MCP client (Claude Code, etc.) spawns ONE `.mcp.json` command per
// client session and communicates with it over stdio. When that subprocess
// dies (timeout, crash, client restart), all its in-memory state goes
// with it — unacceptable for a debugger holding 9+s of parse/compile
// plus hours of tick progress.
//
// This shim is that subprocess. It does nothing but forward bytes:
// everything on stdin goes to a TCP socket, everything from the socket
// goes to stdout. The real server is `calcite-debugger --listen 127.0.0.1:3334`,
// launched separately (see scripts/start-debugger-daemon.bat) and kept
// running across client reconnects. State lives in the daemon; the shim
// is disposable.
//
// If the daemon isn't running, the shim autostarts it, waits for the
// port to open, then proceeds. One daemon serves all clients — only the
// first shim pays the startup cost.
//
// Usage (for .mcp.json):
//   "command": "node",
//   "args": ["C:/path/to/calcite/tools/mcp-shim.mjs"]
//
// Env:
//   CALCITE_DEBUGGER_ADDR   host:port, default 127.0.0.1:3334
//   CALCITE_DEBUGGER_BIN    path to calcite-debugger.exe; used for autostart

import net from 'node:net';
import { spawn } from 'node:child_process';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { existsSync } from 'node:fs';

const __dirname = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(__dirname, '..');

const ADDR = process.env.CALCITE_DEBUGGER_ADDR ?? '127.0.0.1:3334';
const [HOST, PORT_STR] = ADDR.split(':');
const PORT = Number.parseInt(PORT_STR, 10);

const DEFAULT_BIN = resolve(REPO_ROOT, 'target', 'release', 'calcite-debugger.exe');
const BIN = process.env.CALCITE_DEBUGGER_BIN ?? DEFAULT_BIN;

// stderr is for diagnostic output. stdout is reserved for MCP frames
// going to the client — writing anything else there breaks the protocol.
function log(msg) {
  process.stderr.write(`[mcp-shim] ${msg}\n`);
}

// Try to connect. Returns the socket on success, null on ECONNREFUSED,
// throws on any other error.
function tryConnect() {
  return new Promise((resolvePromise, rejectPromise) => {
    const sock = net.createConnection({ host: HOST, port: PORT });
    sock.once('connect', () => {
      sock.removeAllListeners('error');
      resolvePromise(sock);
    });
    sock.once('error', (err) => {
      if (err.code === 'ECONNREFUSED') resolvePromise(null);
      else rejectPromise(err);
    });
  });
}

function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

async function waitForDaemon({ timeoutMs = 60000 } = {}) {
  const deadline = Date.now() + timeoutMs;
  let lastErr;
  while (Date.now() < deadline) {
    try {
      const sock = await tryConnect();
      if (sock) return sock;
    } catch (err) {
      lastErr = err;
    }
    await sleep(250);
  }
  throw new Error(
    `timed out after ${timeoutMs}ms waiting for calcite-debugger at ${ADDR}` +
      (lastErr ? ` (last error: ${lastErr.message})` : ''),
  );
}

function autostartDaemon() {
  if (!existsSync(BIN)) {
    throw new Error(
      `calcite-debugger binary not found at ${BIN}. ` +
        `Set CALCITE_DEBUGGER_BIN, or build it: cargo build --release -p calcite-debugger`,
    );
  }
  log(`autostarting daemon: ${BIN} --listen ${ADDR}`);
  // Detach so the daemon survives this shim exiting. stdio ignored — its
  // logs go to its own stderr which is inherited so we can still see them
  // in the client's log pane, but it doesn't hold us alive.
  const child = spawn(BIN, ['--listen', ADDR], {
    detached: true,
    stdio: ['ignore', 'ignore', 'inherit'],
    windowsHide: true,
  });
  child.unref();
  child.on('error', (err) => {
    log(`autostart spawn error: ${err.message}`);
  });
}

async function connectOrStart() {
  // First try — if the daemon is already up, this succeeds immediately.
  let sock = await tryConnect();
  if (sock) {
    log(`connected to existing daemon at ${ADDR}`);
    return sock;
  }
  autostartDaemon();
  sock = await waitForDaemon();
  log(`connected to fresh daemon at ${ADDR}`);
  return sock;
}

async function main() {
  let sock;
  try {
    sock = await connectOrStart();
  } catch (err) {
    log(`fatal: ${err.message}`);
    process.exit(1);
  }

  // Bidirectional pipe. Don't end either side when the other ends —
  // let the daemon see EOF on its read side only when our stdin
  // actually closes (client disconnect). Keep stdout open so late
  // frames from the daemon still flush.
  process.stdin.pipe(sock);
  sock.pipe(process.stdout);

  sock.on('close', () => {
    // Daemon died or closed the TCP connection. Nothing to forward to
    // anymore — exit so the MCP client sees us go away and can respawn
    // a fresh shim (which will auto-restart the daemon).
    log('socket closed by daemon; exiting');
    process.exit(0);
  });
  sock.on('error', (err) => {
    log(`socket error: ${err.message}`);
    process.exit(1);
  });

  process.stdin.on('end', () => {
    // MCP client closed our stdin — client is going away. Close the
    // socket politely; the daemon keeps running and its session state
    // persists for the next client.
    log('stdin ended by client; closing socket');
    sock.end();
  });
}

main().catch((err) => {
  log(`unhandled error: ${err.message}`);
  process.exit(1);
});
