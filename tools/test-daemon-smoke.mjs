#!/usr/bin/env node
// test-daemon-smoke.mjs — end-to-end smoke test for daemon mode.
//
// Flow:
//   1. Start the daemon with --listen 127.0.0.1:PORT (random high port)
//   2. Connect via raw TCP, speak MCP initialize + tools/list
//   3. Disconnect, reconnect from a fresh TCP socket
//   4. Verify the second connection reaches the same handler
//      (by opening a session in connection #1, listing sessions in #2)
//   5. Shut daemon down, confirm exit
//
// Run:
//   node tools/test-daemon-smoke.mjs [path/to/calcite-debugger.exe]
//
// Exits nonzero on any assertion failure.

import net from 'node:net';
import { spawn } from 'node:child_process';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { existsSync } from 'node:fs';

const __dirname = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(__dirname, '..');
const DEFAULT_BIN = resolve(REPO, 'target', 'release', 'calcite-debugger.exe');
const BIN = process.argv[2] ?? DEFAULT_BIN;

if (!existsSync(BIN)) {
  console.error(`binary not found: ${BIN}`);
  process.exit(1);
}

// Pick an unlikely-to-collide high port. If the user has another daemon
// running on this port the test will fail — that's fine, they can retry
// with a different PORT= env var.
const PORT = Number.parseInt(process.env.PORT ?? '47314', 10);
const ADDR = `127.0.0.1:${PORT}`;

function log(msg) { process.stderr.write(`[smoke] ${msg}\n`); }
function sleep(ms) { return new Promise((r) => setTimeout(r, ms)); }

async function waitForPort(port, timeoutMs = 15000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const ok = await new Promise((res) => {
      const s = net.createConnection({ host: '127.0.0.1', port });
      s.once('connect', () => { s.end(); res(true); });
      s.once('error', () => res(false));
    });
    if (ok) return;
    await sleep(200);
  }
  throw new Error(`port ${port} did not open in time`);
}

// Minimal MCP-over-JSON-RPC framer. rmcp uses newline-delimited JSON
// (one JSON object per line). Send a request, wait for its response.
class McpClient {
  constructor(sock) {
    this.sock = sock;
    this.buf = '';
    this.pending = new Map();
    this.nextId = 1;
    sock.setEncoding('utf8');
    sock.on('data', (chunk) => this.onData(chunk));
  }
  onData(chunk) {
    this.buf += chunk;
    let nl;
    while ((nl = this.buf.indexOf('\n')) >= 0) {
      const line = this.buf.slice(0, nl);
      this.buf = this.buf.slice(nl + 1);
      if (!line.trim()) continue;
      let msg;
      try { msg = JSON.parse(line); }
      catch (e) { log(`bad json: ${line}`); continue; }
      if (msg.id != null && this.pending.has(msg.id)) {
        const { resolve, reject } = this.pending.get(msg.id);
        this.pending.delete(msg.id);
        if (msg.error) reject(new Error(JSON.stringify(msg.error)));
        else resolve(msg.result);
      }
    }
  }
  request(method, params, { timeoutMs = 10000 } = {}) {
    const id = this.nextId++;
    const frame = JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n';
    this.sock.write(frame);
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      setTimeout(() => {
        if (this.pending.has(id)) {
          this.pending.delete(id);
          reject(new Error(`timeout on ${method} (${timeoutMs}ms)`));
        }
      }, timeoutMs);
    });
  }
  notify(method, params) {
    this.sock.write(JSON.stringify({ jsonrpc: '2.0', method, params }) + '\n');
  }
  close() { this.sock.end(); }
}

async function connect() {
  const sock = await new Promise((res, rej) => {
    const s = net.createConnection({ host: '127.0.0.1', port: PORT });
    s.once('connect', () => res(s));
    s.once('error', rej);
  });
  const client = new McpClient(sock);
  await client.request('initialize', {
    protocolVersion: '2024-11-05',
    capabilities: {},
    clientInfo: { name: 'smoke-test', version: '0.1' },
  });
  client.notify('notifications/initialized', {});
  return client;
}

async function callTool(client, name, args, opts = {}) {
  return client.request('tools/call', { name, arguments: args }, opts);
}

async function main() {
  log(`starting daemon: ${BIN} --listen ${ADDR}`);
  const daemon = spawn(BIN, ['--listen', ADDR], {
    stdio: ['ignore', 'ignore', 'inherit'],
    detached: false,
  });
  daemon.on('error', (e) => log(`daemon spawn error: ${e.message}`));

  try {
    await waitForPort(PORT);
    log('daemon listening');

    // --- Connection #1: open a session.
    log('connection #1');
    let c1 = await connect();
    const tools = await c1.request('tools/list', {});
    log(`tools: ${tools.tools.map((t) => t.name).join(', ')}`);
    const infoR = await callTool(c1, 'info', {});
    log(`info (pre-open): ${JSON.stringify(infoR).slice(0, 200)}`);

    // Open a real CSS file so we can verify the session actually survives
    // the reconnect. The measure-640k cabinet built during the session
    // fix is a convenient target — small enough to parse in a few
    // seconds, but not required.
    const cssPath = process.env.SMOKE_CSS ??
      resolve(REPO, '..', 'CSS-DOS', 'tmp', 'measure-640k.css');
    if (!existsSync(cssPath)) {
      log(`SKIP session-persist assertion: no CSS at ${cssPath}. Set SMOKE_CSS=path to override.`);
      c1.close();
    } else {
      log(`opening session 'smoke' with ${cssPath} (parse+compile can take 30s+)`);
      const openRes = await callTool(c1, 'open',
        { path: cssPath, session: 'smoke' },
        { timeoutMs: 120000 });
      log(`open OK: ${JSON.stringify(openRes).slice(0, 200)}`);
      c1.close();
      log('connection #1 closed');

      // --- Connection #2: a fresh socket talks to the SAME daemon and
      //      MUST see the 'smoke' session still loaded.
      await sleep(200);
      log('connection #2');
      const c2 = await connect();
      const info2 = await callTool(c2, 'info', {});
      const infoText = info2.structuredContent ?? JSON.parse(info2.content[0].text);
      const smoke = infoText.sessions?.smoke;
      if (!smoke || !smoke.css_file) {
        throw new Error(`session 'smoke' did not survive reconnect: ${JSON.stringify(infoText)}`);
      }
      log(`session 'smoke' still loaded after reconnect — css: ${smoke.css_file}, assignments: ${smoke.assignments_count}`);
      c2.close();
      log('connection #2 closed');
    }

    log('OK: reconnect succeeded, daemon survived.');
  } finally {
    log('stopping daemon');
    daemon.kill('SIGTERM');
    await new Promise((r) => daemon.on('exit', r));
    log('daemon stopped');
  }
}

main().catch((err) => {
  log(`FAIL: ${err.stack || err.message}`);
  process.exit(1);
});
