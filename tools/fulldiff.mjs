#!/usr/bin/env node
//
// ====================================================================
// BROKEN — DO NOT USE. Imports ../CSS-DOS/transpiler/ which was deleted
// in the builder/Kiln rewrite. Use the new harness instead:
//
//   node ../CSS-DOS/tests/harness/fulldiff.mjs <cabinet.css>
//
// The new harness uses sidecar .bios.bin / .kernel.bin / .disk.bin that
// the new builder emits alongside each cabinet. See
// ../CSS-DOS/tests/harness/README.md.
// ====================================================================
//
// fulldiff.mjs — Find the FIRST divergence between JS reference emulator and calcite.
//
// Compares ALL registers including ALL 16 bits of FLAGS (no masking).
// Handles REP sync (JS ref does entire REP in one step, calcite expands per tick).
// On first divergence: prints previous state, instruction, and full comparison, then stops.
//
// Usage: node tools/fulldiff.mjs [--ticks=N] [--skip=N]
//
// Requires: calcite-debugger running on localhost:3333

import { readFileSync } from 'fs';
import { resolve, dirname } from 'path';
import { fileURLToPath, pathToFileURL } from 'url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const args = process.argv.slice(2);
const flags = Object.fromEntries(
  args.filter(a => a.startsWith('--')).map(a => {
    const [k, v] = a.split('=');
    return [k.replace(/^--/, ''), v ?? 'true'];
  })
);
const maxTicks = parseInt(flags.ticks || '500');
const skipTicks = parseInt(flags.skip || '0');
const port = parseInt(flags.port || '3333');
const BASE = `http://localhost:${port}`;

// --- Reference emulator setup (microcode BIOS path) ---
// Matches generate-dos.mjs: kernel at 0x600, disk at 0xD0000, IVT/BDA from JS,
// start at CS=0x0060:IP=0, with JS BIOS handlers intercepting interrupts.
const cssDir = resolve(__dirname, '..', '..', 'CSS-DOS');
const js8086Source = readFileSync(resolve(cssDir, 'tools', 'js8086.js'), 'utf-8');
const evalSource = js8086Source.replace("'use strict';", '').replace('let CPU_186 = 0;', 'var CPU_186 = 1;');
const Intel8086 = new Function(evalSource + '\nreturn Intel8086;')();

const { PIC, PIT, KeyboardController } = await import(pathToFileURL(resolve(cssDir, 'tools', 'peripherals.mjs')).href);
const { createBiosHandlers } = await import(pathToFileURL(resolve(cssDir, 'tools', 'lib', 'bios-handlers.mjs')).href);
const { buildBiosRom } = await import(pathToFileURL(resolve(cssDir, 'transpiler', 'src', 'patterns', 'bios.mjs')).href);

const refMem = new Uint8Array(1024 * 1024);

const kernelBin = readFileSync(resolve(cssDir, 'dos', 'bin', 'kernel.sys'));
const diskBin = readFileSync(resolve(cssDir, 'dos', 'disk.img'));

// Load kernel at 0x600 and disk image at 0xD0000
for (let i = 0; i < kernelBin.length; i++) refMem[0x600 + i] = kernelBin[i];
for (let i = 0; i < diskBin.length && 0xD0000 + i < refMem.length; i++) refMem[0xD0000 + i] = diskBin[i];

// Build BIOS ROM and IVT (matching generate-dos.mjs exactly)
const { handlers: biosRomHandlers, romBytes: biosRomBytes } = buildBiosRom();
const BIOS_SEG = 0xF000;
const biosBytes = [0xCF, ...biosRomBytes]; // byte 0 = IRET for dummy handler
const romStubBase = 1;
for (const intNum of Object.keys(biosRomHandlers)) {
  biosRomHandlers[intNum] += romStubBase;
}
// Load BIOS ROM at F000:0000
for (let i = 0; i < biosBytes.length; i++) refMem[0xF0000 + i] = biosBytes[i];

// IVT: all 256 entries default to the dummy IRET at F000:0000
for (let i = 0; i < 256; i++) {
  refMem[i * 4 + 0] = 0x00;
  refMem[i * 4 + 1] = 0x00;
  refMem[i * 4 + 2] = BIOS_SEG & 0xFF;
  refMem[i * 4 + 3] = (BIOS_SEG >> 8) & 0xFF;
}
// Override with microcode handler stubs
for (const [intNum, stubOffset] of Object.entries(biosRomHandlers)) {
  const idx = parseInt(intNum);
  refMem[idx * 4 + 0] = stubOffset & 0xFF;
  refMem[idx * 4 + 1] = (stubOffset >> 8) & 0xFF;
  refMem[idx * 4 + 2] = BIOS_SEG & 0xFF;
  refMem[idx * 4 + 3] = (BIOS_SEG >> 8) & 0xFF;
}

// Populate BDA (matching generate-dos.mjs exactly)
const BDA = 0x0400;
refMem[BDA + 0x10] = 0x21; refMem[BDA + 0x11] = 0x00;  // equipment list
refMem[BDA + 0x13] = 640 & 0xFF; refMem[BDA + 0x14] = (640 >> 8) & 0xFF;  // memory size
refMem[BDA + 0x1A] = 0x1E; refMem[BDA + 0x1B] = 0x00;  // kbd head
refMem[BDA + 0x1C] = 0x1E; refMem[BDA + 0x1D] = 0x00;  // kbd tail
refMem[BDA + 0x80] = 0x1E; refMem[BDA + 0x81] = 0x00;  // kbd buf start
refMem[BDA + 0x82] = 0x3E; refMem[BDA + 0x83] = 0x00;  // kbd buf end
refMem[BDA + 0x49] = 0x03;  // video mode 3
refMem[BDA + 0x4A] = 80; refMem[BDA + 0x4B] = 0;  // columns
refMem[BDA + 0x4C] = 0x00; refMem[BDA + 0x4D] = 0x10;  // page size
refMem[BDA + 0x60] = 0x07; refMem[BDA + 0x61] = 0x06;  // cursor shape
refMem[BDA + 0x63] = 0xD4; refMem[BDA + 0x64] = 0x03;  // CRT port
refMem[BDA + 0x84] = 24;   // rows minus 1
refMem[BDA + 0x85] = 16;   // char height

// Peripherals
const pic = new PIC();
const pit = new PIT(pic);
const kbd = new KeyboardController(pic);

let int_handler = null;

const refWritesThisTick = [];
const cpu = Intel8086(
  (addr, val) => {
    addr = addr & 0xFFFFF;
    refWritesThisTick.push({ addr, val: val & 0xFF, old: refMem[addr] });
    refMem[addr] = val & 0xFF;
  },
  (addr) => refMem[addr & 0xFFFFF],
  pic,
  pit,
  (type) => int_handler ? int_handler(type) : false,
);
cpu.reset();
cpu.setRegs({
  cs: 0x0060, ip: 0x0000,   // kernel entry point
  ss: 0x0030, sp: 0x0100,   // matching generate-dos.mjs
  ds: 0, es: 0,
  ah: 0, al: 0, bh: 0, bl: 0, ch: 0, cl: 0, dh: 0, dl: 0,
});

int_handler = createBiosHandlers(
  refMem, pic, kbd,
  () => cpu.getRegs(),
  (regs) => cpu.setRegs(regs),
);

const REG_NAMES = ['AX', 'CX', 'DX', 'BX', 'SP', 'BP', 'SI', 'DI', 'IP', 'CS', 'DS', 'ES', 'SS', 'FLAGS'];

function getRefRegs() {
  const r = cpu.getRegs();
  return {
    AX: (r.ah << 8) | r.al, CX: (r.ch << 8) | r.cl,
    DX: (r.dh << 8) | r.dl, BX: (r.bh << 8) | r.bl,
    SP: r.sp, BP: r.bp, SI: r.si, DI: r.di,
    IP: r.ip, CS: r.cs, DS: r.ds, ES: r.es, SS: r.ss, FLAGS: r.flags,
  };
}

// --- REP detection ---
const STRING_OPS = new Set([0xA4, 0xA5, 0xA6, 0xA7, 0xAA, 0xAB, 0xAC, 0xAD, 0xAE, 0xAF]);
const SEG_PREFIXES = new Set([0x26, 0x2E, 0x36, 0x3E]);

function detectREP(cs, ip) {
  const base = (cs * 16 + ip) & 0xFFFFF;
  let off = 0;
  let hasRep = false;
  for (let i = 0; i < 4; i++) {
    const b = refMem[(base + off) & 0xFFFFF];
    if (b === 0xF2 || b === 0xF3) { hasRep = true; off++; }
    else if (SEG_PREFIXES.has(b)) { off++; }
    else break;
  }
  if (!hasRep) return null;
  const opcode = refMem[(base + off) & 0xFFFFF];
  if (!STRING_OPS.has(opcode)) return null;
  return { opcode };
}

// --- HTTP helpers ---
async function post(path, body) {
  const resp = await fetch(`${BASE}${path}`, {
    method: 'POST', headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  });
  return resp.json();
}
async function get(path) { return (await fetch(`${BASE}${path}`)).json(); }

function hex(v, w = 4) { return v.toString(16).padStart(w, '0'); }
function hexAddr(cs, ip) { return `${hex(cs)}:${hex(ip)} (${hex(cs * 16 + ip, 5)})`; }

function flagBits(f) {
  const names = ['CF','','PF','','AF','','ZF','SF','TF','IF','DF','OF'];
  return names.map((n, i) => n && (f & (1 << i)) ? n : '').filter(Boolean).join('|') || '(none)';
}

// --- Calcite instruction retirement ---
// Advance calcite until one instruction retires.
//
// An instruction has retired when uOp=0 and IP has moved past the instruction.
// "Past the instruction" means IP is outside the range [startIP-2, startIP] —
// the 2-byte prefix window. REP loops rewind IP to the prefix byte (startIP-1
// or startIP-2), which keeps IP inside this range. When the REP finishes, IP
// advances past the instruction and leaves the range.
//
// Multi-μop instructions (PUSH, INT, CALL) hold IP at startIP during mid-μop
// ticks, then advance past it on retirement. The range check handles both cases.
async function advanceCalciteOneInstruction(startIP) {
  let ticks = 0;
  const MAX_TICKS = 100000;

  while (ticks < MAX_TICKS) {
    await post('/tick', { count: 1 });
    ticks++;
    const st = await get('/state');
    const uOp = st.registers.uOp;
    const ip = st.registers.IP;

    // IP is still within the instruction's prefix+opcode range — not retired yet
    if (ip >= startIP - 2 && ip <= startIP) continue;

    // IP moved past the instruction. Wait for uOp=0 if mid-μop.
    if (uOp === 0) {
      return { state: st, ticks };
    }
  }
  const st = await get('/state');
  return { state: st, ticks };
}

// --- Main ---
async function main() {
  try { await get('/info'); } catch {
    console.error(`Cannot connect to debugger at ${BASE}. Start it first.`);
    process.exit(1);
  }

  await post('/seek', { tick: 0 });
  console.error(`Full diff: up to ${maxTicks} instructions, instruction-retirement alignment`);
  if (skipTicks > 0) console.error(`Skipping first ${skipTicks} instructions...`);

  let prevRefRegs = getRefRegs();
  let calciteTick = 0;

  // Skip phase
  if (skipTicks > 0) {
    for (let t = 0; t < skipTicks; t++) {
      const ipBefore = prevRefRegs.IP;
      cpu.step();
      prevRefRegs = getRefRegs();
      const { ticks } = await advanceCalciteOneInstruction(ipBefore);
      calciteTick += ticks;
      if (t > 0 && t % 5000 === 0) console.error(`  skipped ${t}... (calcite tick ${calciteTick})`);
    }
    console.error(`  Skip done. Calcite at tick ${calciteTick}.`);
  }

  for (let inst = 0; inst < maxTicks; inst++) {
    const refInst = skipTicks + inst;
    const flatIP = prevRefRegs.CS * 16 + prevRefRegs.IP;
    const instBytes = [];
    for (let i = 0; i < 8; i++) instBytes.push(refMem[(flatIP + i) & 0xFFFFF]);

    // Step reference by one instruction
    refWritesThisTick.length = 0;
    cpu.step();
    const refAfter = getRefRegs();

    // Advance calcite to next instruction retirement
    const calciteIP = prevRefRegs.IP; // IP before this instruction (calcite should match)
    const { state: calState, ticks: stepTicks } = await advanceCalciteOneInstruction(calciteIP);
    calciteTick += stepTicks;

    const cal = calState.registers;
    if (cal.flags !== undefined && cal.FLAGS === undefined) cal.FLAGS = cal.flags;

    // Compare ALL registers, ALL FLAGS bits
    const regDiffs = [];
    for (const r of REG_NAMES) {
      if (refAfter[r] !== cal[r]) regDiffs.push(r);
    }

    // Check memory writes
    const memDiffs = [];
    const maxMemChecks = 200;
    const writesSample = refWritesThisTick.length > maxMemChecks
      ? refWritesThisTick.filter((_, i) => i < 100 || i >= refWritesThisTick.length - 100)
      : refWritesThisTick;
    for (const w of writesSample) {
      const calMem = await post('/memory', { addr: w.addr, len: 1 });
      const calVal = calMem.bytes[0];
      if (calVal !== refMem[w.addr]) {
        memDiffs.push({ addr: w.addr, refVal: refMem[w.addr], calVal, old: w.old });
      }
    }

    if (regDiffs.length > 0 || memDiffs.length > 0) {
      console.log(`\n${'='.repeat(78)}`);
      console.log(`FIRST DIVERGENCE at instruction ${refInst} (calcite tick ${calciteTick}, ${stepTicks} ticks for this inst)`);
      console.log(`${'='.repeat(78)}`);

      console.log(`\n  Before: ${hexAddr(prevRefRegs.CS, prevRefRegs.IP)}`);
      console.log(`  Instruction bytes: ${instBytes.map(b => hex(b, 2)).join(' ')}`);
      console.log(`  Pre-FLAGS: ref=${hex(prevRefRegs.FLAGS)} [${flagBits(prevRefRegs.FLAGS)}]`);

      console.log(`\n  Register        Reference    Calcite      Match`);
      console.log('  ' + '─'.repeat(60));
      for (const r of REG_NAMES) {
        const rv = refAfter[r], cv = cal[r];
        const match = rv === cv ? '  ✓' : '  ✗ DIFF';
        let extra = '';
        if (r === 'FLAGS' && rv !== cv) {
          const xor = rv ^ cv;
          extra = `  (diff bits: ${flagBits(xor)})`;
        }
        console.log(`  ${r.padEnd(14)}  ${hex(rv).padEnd(12)} ${hex(cv).padEnd(12)} ${match}${extra}`);
      }

      if (regDiffs.includes('FLAGS')) {
        console.log(`\n  Ref FLAGS:     ${hex(refAfter.FLAGS)} = ${refAfter.FLAGS.toString(2).padStart(16, '0')} [${flagBits(refAfter.FLAGS)}]`);
        console.log(`  Calcite FLAGS: ${hex(cal.FLAGS)} = ${cal.FLAGS.toString(2).padStart(16, '0')} [${flagBits(cal.FLAGS)}]`);
      }

      if (memDiffs.length > 0) {
        console.log(`\n  Memory mismatches (${memDiffs.length} of ${writesSample.length} checked, ${refWritesThisTick.length} total writes):`);
        for (const d of memDiffs.slice(0, 20)) {
          console.log(`    ${hex(d.addr, 6)}: ref=${hex(d.refVal, 2)} cal=${hex(d.calVal, 2)} (was ${hex(d.old, 2)})`);
        }
      } else if (refWritesThisTick.length > 0) {
        console.log(`\n  Memory: ${refWritesThisTick.length} writes, all match ✓`);
      }

      const changed = [];
      for (const r of REG_NAMES) {
        if (prevRefRegs[r] !== refAfter[r]) changed.push(`${r}: ${hex(prevRefRegs[r])}→${hex(refAfter[r])}`);
      }
      if (changed.length) console.log(`\n  Ref deltas: ${changed.join(', ')}`);

      const props = calState.properties || {};
      const interestingProps = ['opcode', 'hasREP', 'repType', 'prefixLen', 'mod', 'reg', 'rm', 'ea', 'hasSegOverride', 'uOp', '_repActive', '_repContinue'];
      const propLines = interestingProps
        .filter(p => `--${p}` in props)
        .map(p => `${p}=${props[`--${p}`]}`);
      if (propLines.length) console.log(`\n  Calcite props: ${propLines.join(', ')}`);

      console.log(`\n${'='.repeat(78)}`);
      console.log(`\nStopped at first divergence. ${refInst} instructions matched before this.`);
      break;
    }

    prevRefRegs = refAfter;

    if (inst > 0 && inst % 1000 === 0) {
      console.error(`  ${inst} instructions OK (calcite tick ${calciteTick})...`);
    }
  }
}

main().catch(e => { console.error(e); process.exit(1); });
