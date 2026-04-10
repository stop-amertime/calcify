#!/usr/bin/env node
// codebug.mjs — co-execution debugger for calcite vs js8086 reference.
//
// Runs both the JS reference emulator and calcite-debugger in lockstep against
// the same CSS program, exposing a unified HTTP API so you can step, inspect,
// send keyboard input, and diff both sides to find the exact tick at which
// calcite (or more likely, the CSS) diverges from ground truth.
//
// Usage:
//   node tools/codebug.mjs <program.css> [--port=3334] [--calcite-port=3333]
//
// Phase 1 endpoints (this file):
//   GET  /info                  Both sides' metadata, current ticks, agreement flag.
//   POST /step  {count}         Advance both sides by N ticks. Returns divergence info.
//   POST /key   {ascii,scancode|value}
//                               Queue a key; next /step flushes it to BOTH sides.
//   GET  /regs                  Both sides' registers plus a diffs array.
//   GET  /screen                Both sides' VGA text buffers + a side-by-side diff.
//   POST /compare  {memory?}    Diff registers (always) and memory ranges (optional).
//                               memory = [{addr, len}, ...]. Uses each side's memory.
//   POST /seek  {tick}          Reset both sides and advance to `tick`. Expensive!
//                               (JS side has no checkpoints in phase 1 — full replay.)
//   POST /shutdown              Stop the calcite-debugger child and exit.
//
// Design notes:
// - The JS emulator is in-process. calcite-debugger runs as a child process
//   and is driven over its HTTP API on --calcite-port (default 3333).
// - Keyboard routing: both sides' BIOS (gossamer-dos) polls linear 0x500 for
//   keys (word: ASCII in low byte, scancode in high byte). So on each /step,
//   if a key is queued we write it to both sides before executing the batch.
//   The BIOS's INT 16h handler clears 0x500 when the key is consumed.
// - Divergence checking in phase 1 is explicit (/compare or /regs). Phase 2
//   adds /run-until-diverge with auto-bisection.

import { readFileSync } from 'fs';
import { resolve, dirname } from 'path';
import { fileURLToPath } from 'url';
import { spawn } from 'child_process';
import http from 'http';
const { createServer } = http;

const __dirname = dirname(fileURLToPath(import.meta.url));
const args = process.argv.slice(2);
const positional = args.filter(a => !a.startsWith('--'));
const flags = Object.fromEntries(
  args.filter(a => a.startsWith('--')).map(a => {
    const [k, v] = a.split('=');
    return [k.replace(/^--/, ''), v ?? 'true'];
  })
);

if (positional.length < 1) {
  console.error('Usage: node tools/codebug.mjs <program.css> [--port=3334] [--calcite-port=3333]');
  process.exit(1);
}

const cssPath = resolve(positional[0]);
const port = parseInt(flags.port || '3334');
const calcitePort = parseInt(flags['calcite-port'] || '3333');
const calciteBin = flags['calcite-bin'] || resolve(__dirname, '..', 'target', 'release', 'calcite-debugger.exe');

// ---------------------------------------------------------------------------
// JS reference boot (mirrors ref-dos.mjs)
// ---------------------------------------------------------------------------

const js8086Source = readFileSync(resolve(__dirname, '..', '..', 'CSS-DOS', 'tools', 'js8086.js'), 'utf-8');
const evalSource = js8086Source.replace("'use strict';", '').replace('let CPU_186 = 0;', 'var CPU_186 = 1;');
const Intel8086 = new Function(evalSource + '\nreturn Intel8086;')();

const cssDir = resolve(__dirname, '..', '..', 'CSS-DOS');
const biosBin = readFileSync(resolve(cssDir, 'build', 'gossamer-dos.bin'));
const kernelBin = readFileSync(resolve(cssDir, 'dos', 'bin', 'kernel.sys'));
const diskBin = readFileSync(resolve(cssDir, 'dos', 'disk.img'));

const jsMemory = new Uint8Array(1024 * 1024);
for (let i = 0; i < kernelBin.length; i++) jsMemory[0x600 + i] = kernelBin[i];
for (let i = 0; i < diskBin.length && 0xD0000 + i < jsMemory.length; i++) jsMemory[0xD0000 + i] = diskBin[i];
for (let i = 0; i < biosBin.length; i++) jsMemory[0xF0000 + i] = biosBin[i];

const jsCpu = Intel8086(
  (addr, val) => { jsMemory[addr & 0xFFFFF] = val & 0xFF; },
  (addr) => jsMemory[addr & 0xFFFFF],
);
jsCpu.reset();

// Initial registers are set after calcite-debugger is ready (see initJsCpu below),
// so they match the CSS @property initial-values exactly.
let jsTick = 0;

function jsGetRegs() {
  const r = jsCpu.getRegs();
  return {
    AX: (r.ah << 8) | r.al,
    CX: (r.ch << 8) | r.cl,
    DX: (r.dh << 8) | r.dl,
    BX: (r.bh << 8) | r.bl,
    SP: r.sp, BP: r.bp, SI: r.si, DI: r.di,
    IP: r.ip, CS: r.cs, DS: r.ds, ES: r.es, SS: r.ss, FLAGS: r.flags,
  };
}

function jsStep() {
  jsCpu.step();
  jsTick++;
}

async function jsReset() {
  // Re-zero memory and reload BIOS/kernel/disk.
  jsMemory.fill(0);
  for (let i = 0; i < kernelBin.length; i++) jsMemory[0x600 + i] = kernelBin[i];
  for (let i = 0; i < diskBin.length && 0xD0000 + i < jsMemory.length; i++) jsMemory[0xD0000 + i] = diskBin[i];
  for (let i = 0; i < biosBin.length; i++) jsMemory[0xF0000 + i] = biosBin[i];
  jsCpu.reset();
  jsTick = 0;
  // Re-seed registers from calcite tick-0 state (canonical source).
  await initJsCpu();
}

function jsRenderScreen(base = 0xB8000, width = 80, height = 25) {
  // Match calcite's render_screen: printable chars kept, non-printables become spaces.
  // Trim trailing spaces per row, then drop trailing blank lines.
  const rows = [];
  for (let y = 0; y < height; y++) {
    let line = '';
    for (let x = 0; x < width; x++) {
      const a = base + (y * width + x) * 2;
      const ch = jsMemory[a & 0xFFFFF];
      line += ch >= 0x20 && ch < 0x7F ? String.fromCharCode(ch) : ' ';
    }
    rows.push(line.trimEnd());
  }
  while (rows.length && rows[rows.length - 1] === '') rows.pop();
  return rows.join('\n');
}

// BDA ring buffer constants (matching gossamer-dos.asm / IBM PC BIOS).
// All offsets are relative to segment 0x40 (linear 0x400 + offset).
const BDA_KBD_HEAD  = 0x41A;  // word — head pointer (offset into buffer)
const BDA_KBD_TAIL  = 0x41C;  // word — tail pointer
const BDA_KBD_START = 0x480;  // word — buffer start offset (0x1E)
const BDA_KBD_END   = 0x482;  // word — buffer end offset (0x3E)

function jsReadWord(addr) {
  return jsMemory[addr] | (jsMemory[addr + 1] << 8);
}
function jsWriteWord(addr, val) {
  jsMemory[addr]     = val & 0xFF;
  jsMemory[addr + 1] = (val >> 8) & 0xFF;
}

function jsWriteKey(value) {
  // Write into the BDA ring buffer — the standard PC keyboard buffer.
  // This is exactly what the 8042 keyboard controller / INT 9 handler does on real hardware.
  const tail    = jsReadWord(BDA_KBD_TAIL);
  const bufEnd  = jsReadWord(BDA_KBD_END);
  const bufStart= jsReadWord(BDA_KBD_START);

  // The buffer lives at segment 0x40, so linear = 0x400 + offset.
  jsWriteWord(0x400 + tail, value & 0xFFFF);

  let newTail = tail + 2;
  if (newTail >= bufEnd) newTail = bufStart;

  // Only write if buffer isn't full (head == newTail means full).
  const head = jsReadWord(BDA_KBD_HEAD);
  if (newTail !== head) {
    jsWriteWord(BDA_KBD_TAIL, newTail);
  } else {
    console.error('[codebug] WARNING: BDA keyboard buffer full, key dropped');
  }
}

// ---------------------------------------------------------------------------
// Calcite side — spawn + HTTP client
// ---------------------------------------------------------------------------

let calciteProc = null;

function httpRequest(method, path, body) {
  return new Promise((res, rej) => {
    const data = body == null ? '' : JSON.stringify(body);
    const req = http.request({
      host: '127.0.0.1', port: calcitePort, path, method,
      headers: body == null ? {} : { 'content-type': 'application/json', 'content-length': Buffer.byteLength(data) },
    }, (r) => {
      const chunks = [];
      r.on('data', c => chunks.push(c));
      r.on('end', () => {
        const txt = Buffer.concat(chunks).toString('utf-8');
        if (r.statusCode >= 400) return rej(new Error(`calcite ${path} ${r.statusCode}: ${txt}`));
        try { res(txt ? JSON.parse(txt) : {}); }
        catch { res(txt); }
      });
    });
    req.on('error', rej);
    if (data) req.write(data);
    req.end();
  });
}

// Seed the JS CPU registers from calcite's tick-0 state so they exactly
// match the @property initial-values in the CSS, regardless of which BIOS
// version is currently built.
async function initJsCpu() {
  const state = await httpRequest('GET', '/state');
  const r = state.registers;
  jsCpu.setRegs({
    cs: r.CS, ip: r.IP & 0xFFFF,
    ss: r.SS, sp: r.SP,
    ds: r.DS, es: r.ES,
    ah: (r.AX >> 8) & 0xFF, al: r.AX & 0xFF,
    bh: (r.BX >> 8) & 0xFF, bl: r.BX & 0xFF,
    ch: (r.CX >> 8) & 0xFF, cl: r.CX & 0xFF,
    dh: (r.DX >> 8) & 0xFF, dl: r.DX & 0xFF,
    bp: r.BP, si: r.SI, di: r.DI,
  });
  // FLAGS can't be set via setRegs in js8086 — it starts from cpu.reset() defaults,
  // which is close enough (usually 0x0002, matching the CSS initial value).
  console.error(`[codebug] JS CPU seeded from CSS: CS=${r.CS.toString(16)} IP=${(r.IP&0xFFFF).toString(16)} SP=${r.SP.toString(16)}`);
}

async function waitForCalcite(maxMs = 60000) {
  const deadline = Date.now() + maxMs;
  while (Date.now() < deadline) {
    try {
      await httpRequest('GET', '/info');
      return true;
    } catch {
      await new Promise(r => setTimeout(r, 200));
    }
  }
  throw new Error('calcite-debugger did not come up in time');
}

function startCalcite() {
  console.error(`[codebug] starting calcite-debugger: ${calciteBin} -i ${cssPath} -p ${calcitePort}`);
  calciteProc = spawn(calciteBin, ['-i', cssPath, '-p', String(calcitePort)], {
    stdio: ['ignore', 'inherit', 'inherit'],
  });
  calciteProc.on('exit', (code, signal) => {
    console.error(`[codebug] calcite-debugger exited code=${code} signal=${signal}`);
    calciteProc = null;
  });
}

// ---------------------------------------------------------------------------
// Shared key queue
// ---------------------------------------------------------------------------

const keyQueue = [];

function pushKey(value) {
  keyQueue.push(value & 0xFFFF);
}

// ---------------------------------------------------------------------------
// Register diff helper
// ---------------------------------------------------------------------------

const REG_NAMES = ['AX','CX','DX','BX','SP','BP','SI','DI','IP','ES','CS','SS','DS','FLAGS'];

function diffRegs(jsRegs, calciteRegs) {
  const diffs = [];
  for (const name of REG_NAMES) {
    const j = jsRegs[name] | 0;
    const c = calciteRegs[name] | 0;
    // Normalize 16-bit view for comparison (calcite's IP holds a flat address sometimes).
    if (name === 'IP') {
      // Compare IP as (flat - CS*16) on the calcite side if needed.
      const jIp = j & 0xFFFF;
      const cIp = c & 0xFFFF;
      if (jIp !== cIp) diffs.push({ name, js: jIp, calcite: cIp });
      continue;
    }
    if ((j & 0xFFFF) !== (c & 0xFFFF)) {
      diffs.push({ name, js: j & 0xFFFF, calcite: c & 0xFFFF });
    }
  }
  return diffs;
}

// ---------------------------------------------------------------------------
// Core operations
// ---------------------------------------------------------------------------

async function flushKey() {
  if (keyQueue.length === 0) return null;
  const value = keyQueue.shift();
  jsWriteKey(value);
  await httpRequest('POST', '/key', { value });
  return value;
}

async function stepBoth(count) {
  // Flush one queued key per /step batch — matches how real input behaves
  // (one keypress per poll window).
  const flushed = await flushKey();

  // Calcite side: single HTTP call for the whole batch (cheap).
  const calciteResp = await httpRequest('POST', '/tick', { count });

  // JS side: loop in-process.
  let jsError = null;
  for (let i = 0; i < count; i++) {
    try { jsStep(); }
    catch (e) { jsError = e.message || String(e); break; }
  }

  return {
    ticks_requested: count,
    ticks_js: jsTick,
    ticks_calcite: calciteResp.tick,
    key_flushed: flushed,
    js_error: jsError,
  };
}

async function getBothRegs() {
  const calcite = await httpRequest('GET', '/state');
  const js = jsGetRegs();
  return {
    tick_js: jsTick,
    tick_calcite: calcite.tick,
    js,
    calcite: calcite.registers,
    diffs: diffRegs(js, calcite.registers),
  };
}

// Normalize a screen text for comparison: non-printable → space, rstrip each
// row, drop trailing blank rows. Also maps common CP437 glyphs back to spaces
// since calcite renders CP437 and JS renders ASCII.
function normalizeScreen(text) {
  const rows = text.split('\n').map(row => {
    let out = '';
    for (const ch of row) {
      const cp = ch.codePointAt(0);
      if (cp >= 0x20 && cp < 0x7F) out += ch;
      else out += ' ';
    }
    return out.replace(/\s+$/, '');
  });
  while (rows.length && rows[rows.length - 1] === '') rows.pop();
  return rows.join('\n');
}

async function getBothScreens() {
  const [calciteResp, calciteState] = await Promise.all([
    httpRequest('POST', '/screen', { addr: 0xB8000, width: 80, height: 25 }),
    httpRequest('GET', '/state'),
  ]);
  const jsText = normalizeScreen(jsRenderScreen());
  const calciteText = normalizeScreen(calciteResp.text || '');
  // Side-by-side: zip lines, mark diffs with '|' else ' '.
  const jl = jsText.split('\n');
  const cl = calciteText.split('\n');
  const n = Math.max(jl.length, cl.length);
  const lines = [];
  for (let i = 0; i < n; i++) {
    const a = (jl[i] || '').padEnd(80);
    const b = (cl[i] || '').padEnd(80);
    const mark = a === b ? ' ' : '|';
    lines.push(`${String(i).padStart(2)} ${a}  ${mark}  ${b}`);
  }
  return {
    tick_js: jsTick,
    tick_calcite: calciteState.tick,
    js: jsText,
    calcite: calciteText,
    side_by_side: lines.join('\n'),
    agrees: jsText === calciteText,
  };
}

async function compareMemoryRanges(ranges) {
  // Query calcite for each range, then diff against jsMemory.
  const results = [];
  for (const r of ranges) {
    const calciteResp = await httpRequest('POST', '/memory', { addr: r.addr, len: r.len });
    const cbytes = calciteResp.bytes; // array of u8
    const diffs = [];
    for (let i = 0; i < r.len; i++) {
      const a = r.addr + i;
      const js = jsMemory[a & 0xFFFFF];
      const c = cbytes[i];
      if (js !== c) diffs.push({ addr: a, js, calcite: c });
    }
    results.push({ addr: r.addr, len: r.len, diff_count: diffs.length, diffs });
  }
  return results;
}

async function seekBoth(targetTick) {
  // To replay JS cleanly, we need calcite at tick 0 so jsReset() can seed
  // the JS CPU from the canonical CSS initial state. Then we fast-forward
  // both sides to the target tick.
  if (targetTick < jsTick) {
    await httpRequest('POST', '/seek', { tick: 0 });
    await jsReset();
  }
  await httpRequest('POST', '/seek', { tick: targetTick });
  while (jsTick < targetTick) {
    try { jsStep(); }
    catch (e) { return { ok: false, error: e.message, tick_js: jsTick }; }
  }
  return { ok: true, tick_js: jsTick };
}

// ---------------------------------------------------------------------------
// Run-until-diverge: step both sides in batches; on first diff, bisect down
// to the exact tick where calcite and JS first disagree on registers.
// ---------------------------------------------------------------------------

async function calciteRegs() {
  return (await httpRequest('GET', '/state')).registers;
}

function regsAgree(a, b) {
  for (const name of REG_NAMES) {
    let av = (a[name] | 0) & 0xFFFF;
    let bv = (b[name] | 0) & 0xFFFF;
    if (av !== bv) return false;
  }
  return true;
}

async function runUntilDiverge(opts = {}) {
  const maxTicks   = opts.max_ticks   ?? 1_000_000;
  const batchSize  = opts.batch_size  ?? 1000;
  const startTick  = jsTick;

  // Coarse phase: step in batches until diff or max.
  let stop = null;  // tick at which divergence is observed (calcite tick after batch)
  while (jsTick - startTick < maxTicks) {
    const remaining = maxTicks - (jsTick - startTick);
    const batch = Math.min(batchSize, remaining);
    const before_tick_js = jsTick;
    await stepBoth(batch);
    const cReg = await calciteRegs();
    const jReg = jsGetRegs();
    if (!regsAgree(cReg, jReg)) {
      stop = { batch_start: before_tick_js, batch_end: jsTick };
      break;
    }
  }

  if (!stop) {
    return {
      diverged: false,
      ticks_run: jsTick - startTick,
      tick_js: jsTick,
      tick_calcite: (await httpRequest('GET', '/info')).current_tick,
    };
  }

  // Bisection phase: we know they agreed at stop.batch_start and disagreed at
  // stop.batch_end. Reset and replay to batch_start, then single-step.
  // Replay JS by reset+step (no checkpoints); calcite uses /seek (it has snapshots).
  // IMPORTANT: seek calcite FIRST so that jsReset()'s call to initJsCpu reads
  // the tick-0 state, not whatever tick we left it at.
  await httpRequest('POST', '/seek', { tick: 0 });
  await jsReset();
  await httpRequest('POST', '/seek', { tick: stop.batch_start });
  while (jsTick < stop.batch_start) jsStep();

  // Single-step until first diff.
  let firstDivergeTick = null;
  let lastAgreeRegs = { js: jsGetRegs(), calcite: await calciteRegs() };
  while (jsTick < stop.batch_end) {
    // Step exactly 1 tick.
    await httpRequest('POST', '/tick', { count: 1 });
    try { jsStep(); }
    catch (e) {
      return {
        diverged: true,
        diverge_tick: jsTick,
        js_error: e.message,
        before: lastAgreeRegs,
      };
    }
    const cReg = await calciteRegs();
    const jReg = jsGetRegs();
    if (!regsAgree(cReg, jReg)) {
      firstDivergeTick = jsTick;
      // Build a register diff list.
      const diffs = [];
      for (const name of REG_NAMES) {
        const jv = (jReg[name] | 0) & 0xFFFF;
        const cv = (cReg[name] | 0) & 0xFFFF;
        if (jv !== cv) diffs.push({ name, js: jv, calcite: cv, js_hex: '0x' + jv.toString(16), calcite_hex: '0x' + cv.toString(16) });
      }
      return {
        diverged: true,
        diverge_tick: firstDivergeTick,
        before: {
          tick: firstDivergeTick - 1,
          js: lastAgreeRegs.js,
          calcite: lastAgreeRegs.calcite,
          // Useful: where was execution about to happen?
          js_csip: `${(lastAgreeRegs.js.CS).toString(16)}:${(lastAgreeRegs.js.IP & 0xFFFF).toString(16)}`,
          calcite_csip: `${(lastAgreeRegs.calcite.CS).toString(16)}:${(lastAgreeRegs.calcite.IP & 0xFFFF).toString(16)}`,
        },
        after: {
          tick: firstDivergeTick,
          js: jReg,
          calcite: cReg,
          js_csip: `${jReg.CS.toString(16)}:${(jReg.IP & 0xFFFF).toString(16)}`,
          calcite_csip: `${cReg.CS.toString(16)}:${(cReg.IP & 0xFFFF).toString(16)}`,
        },
        register_diffs: diffs,
      };
    }
    lastAgreeRegs = { js: jReg, calcite: cReg };
  }

  // Shouldn't reach here — coarse phase saw a diff in this batch.
  return {
    diverged: true,
    diverge_tick: stop.batch_end,
    note: 'bisection failed to localize — coarse saw diff but single-step did not',
  };
}

// ---------------------------------------------------------------------------
// HTTP server
// ---------------------------------------------------------------------------

function sendJson(res, code, obj) {
  const body = JSON.stringify(obj, null, 2);
  res.writeHead(code, { 'content-type': 'application/json', 'content-length': Buffer.byteLength(body) });
  res.end(body);
}

async function readBody(req) {
  const chunks = [];
  for await (const c of req) chunks.push(c);
  const s = Buffer.concat(chunks).toString('utf-8');
  return s ? JSON.parse(s) : {};
}

const server = createServer(async (req, res) => {
  const path = req.url.split('?')[0];
  try {
    if (req.method === 'GET' && path === '/info') {
      const calciteInfo = await httpRequest('GET', '/info').catch(e => ({ error: e.message }));
      return sendJson(res, 200, {
        css: cssPath,
        calcite_port: calcitePort,
        tick_js: jsTick,
        tick_calcite: calciteInfo.current_tick ?? null,
        agreed: (calciteInfo.current_tick ?? -1) === jsTick,
        calcite: calciteInfo,
        key_queue_depth: keyQueue.length,
        endpoints: [
          'GET /info', 'POST /step', 'POST /key', 'GET /regs',
          'GET /screen', 'POST /compare', 'POST /seek', 'POST /shutdown',
        ],
      });
    }

    if (req.method === 'POST' && path === '/step') {
      const body = await readBody(req);
      const count = body.count ?? 1;
      const r = await stepBoth(count);
      return sendJson(res, 200, r);
    }

    if (req.method === 'POST' && path === '/key') {
      const body = await readBody(req);
      let value;
      if (typeof body.value === 'number') {
        value = body.value;
      } else {
        const scan = body.scancode | 0;
        const ascii = body.ascii | 0;
        value = (scan << 8) | (ascii & 0xFF);
      }
      pushKey(value);
      return sendJson(res, 200, { queued: value, depth: keyQueue.length });
    }

    if (req.method === 'GET' && path === '/regs') {
      return sendJson(res, 200, await getBothRegs());
    }

    if (req.method === 'GET' && path === '/screen') {
      return sendJson(res, 200, await getBothScreens());
    }

    if (req.method === 'POST' && path === '/compare') {
      const body = await readBody(req);
      const regs = await getBothRegs();
      const memory = body.memory ? await compareMemoryRanges(body.memory) : [];
      const memDiffCount = memory.reduce((a, m) => a + m.diff_count, 0);
      return sendJson(res, 200, {
        tick_js: jsTick,
        tick_calcite: regs.tick_calcite,
        register_diffs: regs.diffs,
        memory_diffs: memory,
        total_diffs: regs.diffs.length + memDiffCount,
        agrees: regs.diffs.length + memDiffCount === 0,
      });
    }

    if (req.method === 'POST' && path === '/seek') {
      const body = await readBody(req);
      const r = await seekBoth(body.tick | 0);
      return sendJson(res, 200, r);
    }

    if (req.method === 'POST' && path === '/run-until-diverge') {
      const body = await readBody(req);
      const r = await runUntilDiverge({
        max_ticks: body.max_ticks,
        batch_size: body.batch_size,
      });
      return sendJson(res, 200, r);
    }

    if (req.method === 'POST' && path === '/shutdown') {
      sendJson(res, 200, { ok: true });
      await httpRequest('POST', '/shutdown').catch(() => {});
      if (calciteProc) calciteProc.kill();
      setTimeout(() => process.exit(0), 100);
      return;
    }

    sendJson(res, 404, { error: `unknown endpoint ${req.method} ${path}` });
  } catch (e) {
    sendJson(res, 500, { error: e.message || String(e), stack: e.stack });
  }
});

// ---------------------------------------------------------------------------
// Bootstrap
// ---------------------------------------------------------------------------

startCalcite();
await waitForCalcite();
await initJsCpu();
console.error(`[codebug] calcite-debugger ready on :${calcitePort}`);

server.listen(port, '127.0.0.1', () => {
  console.error(`[codebug] listening on http://localhost:${port}`);
  console.error(`[codebug] try: curl localhost:${port}/info`);
});

process.on('SIGINT', () => {
  console.error('\n[codebug] SIGINT, shutting down');
  if (calciteProc) calciteProc.kill();
  process.exit(0);
});
