#!/usr/bin/env node
// boot-trace.mjs — High-level boot progress tracer
//
// Samples CPU state at intervals, detects loops, shows where the kernel is
// and what it's doing. Uses the calcite debugger HTTP API.
//
// Usage: node tools/boot-trace.mjs [--ticks=N] [--step=N] [--port=3333]
//
// Requires: calcite-debugger running on the given port.
//
// Output: a compact trace showing IP progression, function names (from the
// kernel map file where possible), INT calls, and loop detection.

import { readFileSync } from 'fs';
import { resolve, dirname } from 'path';
import { fileURLToPath } from 'url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const args = process.argv.slice(2);
const flags = Object.fromEntries(
  args.filter(a => a.startsWith('--')).map(a => {
    const [k, v] = a.split('=');
    return [k.replace(/^--/, ''), v ?? 'true'];
  })
);
const maxTicks = parseInt(flags.ticks || '600000');
const step = parseInt(flags.step || '1000');
const port = parseInt(flags.port || '3333');
const BASE = `http://localhost:${port}`;

// --- Load kernel map file for symbol lookup ---
const cssDir = resolve(__dirname, '..', '..', 'CSS-DOS');
const mapPath = resolve(cssDir, 'dos', 'bin', 'kwc8616.map');
let mapSymbols = []; // [{linear, name}] sorted by linear address
try {
  const mapText = readFileSync(mapPath, 'utf-8');
  const re = /^([0-9A-Fa-f]{4}):([0-9A-Fa-f]{4})[s*+ ]*\s+(\S+)/gm;
  let m;
  while ((m = re.exec(mapText)) !== null) {
    const seg = parseInt(m[1], 16);
    const off = parseInt(m[2], 16);
    const linear = seg * 16 + off;
    mapSymbols.push({ linear, name: m[3] });
  }
  mapSymbols.sort((a, b) => a.linear - b.linear);
} catch (e) {
  console.error('Warning: could not load map file, no symbol names');
}

function lookupSymbol(linearAddr, kernelBase) {
  // The kernel is loaded at kernelBase (0x600). The map file uses
  // addresses relative to the start of the kernel image.
  const offset = linearAddr - kernelBase;
  if (offset < 0) return null;
  // Binary search for the largest symbol address <= offset
  let lo = 0, hi = mapSymbols.length - 1, best = null;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (mapSymbols[mid].linear <= offset) {
      best = mapSymbols[mid];
      lo = mid + 1;
    } else {
      hi = mid - 1;
    }
  }
  if (!best) return null;
  const delta = offset - best.linear;
  return delta < 0x200 ? `${best.name}+0x${delta.toString(16)}` : null;
}

// Known regions for labeling
function describeRegion(cs, ip) {
  const csVal = cs;
  if (csVal === 0xF000) return 'BIOS ROM';
  const linear = csVal * 16 + ip;
  if (linear >= 0xB8000 && linear < 0xC0000) return 'VGA text buffer';
  if (linear >= 0xD0000) return 'disk image';
  // Try kernel map
  const sym = lookupSymbol(linear, 0x600);
  if (sym) return sym;
  return null;
}

// Opcode names for common ones we care about
const OPCODES = {
  0xCC: 'INT3', 0xCD: 'INT', 0xCE: 'INTO', 0xCF: 'IRET',
  0xF1: 'IRQ-sentinel', 0xD6: 'BIOS-ucode',
  0xF4: 'HLT', 0xFA: 'CLI', 0xFB: 'STI',
  0xE4: 'IN-AL', 0xE5: 'IN-AX', 0xE6: 'OUT-AL', 0xE7: 'OUT-AX',
  0xEC: 'IN-DX', 0xED: 'IN-DX', 0xEE: 'OUT-DX', 0xEF: 'OUT-DX',
};

async function post(path, body) {
  const res = await fetch(`${BASE}${path}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  });
  return res.json();
}
async function get(path) {
  const res = await fetch(`${BASE}${path}`);
  return res.json();
}

// --- Main ---
console.log(`Boot trace: ${maxTicks} ticks, sampling every ${step}`);
console.log(`${'tick'.padStart(8)}  ${'CS:IP'.padEnd(10)}  ${'linear'.padEnd(7)}  ${'op'.padEnd(5)}  ${'AX'.padEnd(6)}  ${'CX'.padEnd(6)}  ${'flags'.padEnd(6)}  description`);
console.log('-'.repeat(90));

let prevIP = -1;
let prevCS = -1;
let loopCount = 0;
let loopStart = -1;
const ipHistory = []; // last N samples for loop detection

for (let tick = 0; tick <= maxTicks; tick += step) {
  const data = await post('/seek', { tick });
  const r = data.registers;
  const props = data.properties;
  const cs = r.CS;
  const ip = r.IP;
  const opcode = props['--opcode'] ?? -1;
  const linear = cs * 16 + ip;
  const uOp = r.uOp;

  // Read instruction bytes at current IP for context
  let instrBytes = '';
  try {
    const mem = await post('/memory', { addr: linear, len: 6 });
    if (mem.hex) instrBytes = mem.hex.substring(0, 17); // first 6 bytes
  } catch (e) {}

  // Loop detection
  const ipKey = `${cs}:${ip}`;
  ipHistory.push(ipKey);
  if (ipHistory.length > 10) ipHistory.shift();

  if (cs === prevCS && Math.abs(ip - prevIP) < 32) {
    loopCount++;
    if (loopCount === 1) loopStart = tick - step;
  } else {
    if (loopCount >= 3) {
      console.log(`  ^^^ LOOP: ${prevCS.toString(16).toUpperCase()}:${prevIP.toString(16).toUpperCase().padStart(4,'0')} repeated ${loopCount}x (ticks ${loopStart}-${tick - step})`);
    }
    loopCount = 0;
  }

  const desc = describeRegion(cs, ip) || '';
  const opName = OPCODES[opcode] || `0x${opcode.toString(16).toUpperCase()}`;
  const flagStr = r.flags.toString(16).toUpperCase().padStart(4, '0');

  console.log(
    `${tick.toString().padStart(8)}  ` +
    `${cs.toString(16).toUpperCase()}:${ip.toString(16).toUpperCase().padStart(4, '0')}  ` +
    `${linear.toString(16).toUpperCase().padStart(7)}  ` +
    `${opName.padEnd(5)}  ` +
    `${r.AX.toString(16).toUpperCase().padStart(4, '0').padEnd(6)}  ` +
    `${r.CX.toString(16).toUpperCase().padStart(4, '0').padEnd(6)}  ` +
    `${flagStr.padEnd(6)}  ` +
    `${desc}  ${instrBytes}`
  );

  prevIP = ip;
  prevCS = cs;
}

// Final loop detection
if (loopCount >= 3) {
  console.log(`  ^^^ LOOP: ${prevCS.toString(16).toUpperCase()}:${prevIP.toString(16).toUpperCase().padStart(4,'0')} repeated ${loopCount}x (ticks ${loopStart}-${maxTicks})`);
}

console.log('\n--- Screen at final tick ---');
const screen = await post('/screen', {});
if (screen.text) console.log(screen.text);

console.log('\nDone.');
