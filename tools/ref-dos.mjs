#!/usr/bin/env node
//
// ====================================================================
// BROKEN — DO NOT USE. Imports ../CSS-DOS/transpiler/ which was deleted
// in the builder/Kiln rewrite. The new harness stands up a reference
// emulator per-cabinet using that cabinet's own BIOS/kernel/disk sidecar
// bytes. See ../CSS-DOS/tests/harness/lib/ref-machine.mjs.
// ====================================================================
//
// ref-dos.mjs — Run the JS reference 8086 emulator in DOS boot mode.
// No calcite, no CSS — just the JS emulator with our BIOS and DOS kernel.
// Useful for debugging BIOS issues independently of CSS or calcite.
//
// Usage: node tools/ref-dos.mjs [--ticks=N] [--vga] [--trace] [--trace-from=N]
//
// --ticks=N       Max instruction ticks (default 1000000)
// --vga           Dump VGA text buffer at end
// --trace         Print register state every tick
// --trace-from=N  Start tracing at tick N
// --halt-detect   Stop when IP loops or HLT flag set (default on)
// --int-trace     Log all INT calls (number, AH function, CS:IP caller)
// --int-trace-from=N  Start INT tracing at tick N
// --port-trace    Log all I/O port accesses

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

const maxTicks = parseInt(flags.ticks || '1000000');
const showVga = flags.vga === 'true';
const traceAll = flags.trace === 'true';
const traceFrom = parseInt(flags['trace-from'] || '-1');
const intTrace = flags['int-trace'] === 'true';
const intTraceFrom = parseInt(flags['int-trace-from'] || '-1');
const portTrace = flags['port-trace'] === 'true';
const watchAddr = flags['watch'] ? parseInt(flags['watch']) : -1;  // --watch=0xA0730

// --- Load JS 8086 ---
const js8086Source = readFileSync(resolve(__dirname, '..', '..', 'CSS-DOS', 'tools', 'js8086.js'), 'utf-8');
const evalSource = js8086Source.replace("'use strict';", '').replace('let CPU_186 = 0;', 'var CPU_186 = 1;');
const Intel8086 = new Function(evalSource + '\nreturn Intel8086;')();

// --- Load binaries ---
const cssDir = resolve(__dirname, '..', '..', 'CSS-DOS');
const kernelBin = readFileSync(resolve(cssDir, 'dos', 'bin', 'kernel.sys'));
const diskBin = readFileSync(resolve(cssDir, 'dos', 'disk.img'));

// --- Setup memory (microcode BIOS path — matches generate-dos.mjs) ---
const memory = new Uint8Array(1024 * 1024);

// Load kernel at 0060:0000 (linear 0x600)
for (let i = 0; i < kernelBin.length; i++) memory[0x600 + i] = kernelBin[i];
// Load disk image at D000:0000 (linear 0xD0000)
for (let i = 0; i < diskBin.length && 0xD0000 + i < memory.length; i++) memory[0xD0000 + i] = diskBin[i];

// Build BIOS ROM and IVT (matching generate-dos.mjs exactly)
const { buildBiosRom } = await import(pathToFileURL(resolve(cssDir, 'transpiler', 'src', 'patterns', 'bios.mjs')).href);
const { handlers: biosRomHandlers, romBytes: biosRomBytes } = buildBiosRom();
const BIOS_SEG = 0xF000;
const biosBytes = [0xCF, ...biosRomBytes];
const romStubBase = 1;
for (const intNum of Object.keys(biosRomHandlers)) {
  biosRomHandlers[intNum] += romStubBase;
}
for (let i = 0; i < biosBytes.length; i++) memory[0xF0000 + i] = biosBytes[i];

// IVT: all 256 entries default to dummy IRET at F000:0000
for (let i = 0; i < 256; i++) {
  memory[i * 4 + 0] = 0x00; memory[i * 4 + 1] = 0x00;
  memory[i * 4 + 2] = BIOS_SEG & 0xFF; memory[i * 4 + 3] = (BIOS_SEG >> 8) & 0xFF;
}
for (const [intNum, stubOffset] of Object.entries(biosRomHandlers)) {
  const idx = parseInt(intNum);
  memory[idx * 4 + 0] = stubOffset & 0xFF; memory[idx * 4 + 1] = (stubOffset >> 8) & 0xFF;
  memory[idx * 4 + 2] = BIOS_SEG & 0xFF; memory[idx * 4 + 3] = (BIOS_SEG >> 8) & 0xFF;
}

// BDA (matching generate-dos.mjs)
const BDA = 0x0400;
memory[BDA + 0x10] = 0x21; memory[BDA + 0x11] = 0x00;
memory[BDA + 0x13] = 640 & 0xFF; memory[BDA + 0x14] = (640 >> 8) & 0xFF;
memory[BDA + 0x1A] = 0x1E; memory[BDA + 0x1B] = 0x00;
memory[BDA + 0x1C] = 0x1E; memory[BDA + 0x1D] = 0x00;
memory[BDA + 0x80] = 0x1E; memory[BDA + 0x81] = 0x00;
memory[BDA + 0x82] = 0x3E; memory[BDA + 0x83] = 0x00;
memory[BDA + 0x49] = 0x03;
memory[BDA + 0x4A] = 80; memory[BDA + 0x4B] = 0;
memory[BDA + 0x4C] = 0x00; memory[BDA + 0x4D] = 0x10;
memory[BDA + 0x60] = 0x07; memory[BDA + 0x61] = 0x06;
memory[BDA + 0x63] = 0xD4; memory[BDA + 0x64] = 0x03;
memory[BDA + 0x84] = 24;
memory[BDA + 0x85] = 16;

// --- Peripherals and BIOS handlers ---
import { pathToFileURL } from 'url';
const { PIC, PIT, KeyboardController } = await import(pathToFileURL(resolve(cssDir, 'tools', 'peripherals.mjs')).href);
const { createBiosHandlers } = await import(pathToFileURL(resolve(cssDir, 'tools', 'lib', 'bios-handlers.mjs')).href);

const pic = new PIC();
const pit = new PIT(pic);
const kbd = new KeyboardController(pic);

let int_handler = null;

// --- CPU ---
let vgaWriteCount = 0;
let vgaWriteTickStart = parseInt(flags['vga-trace-from'] || '-1');
let vgaWriteTickEnd = parseInt(flags['vga-trace-to'] || '-1');
let currentTick = 0;
const cpu = Intel8086(
  (addr, val) => {
    const a = addr & 0xFFFFF;
    memory[a] = val & 0xFF;
    if (watchAddr >= 0 && (a >= watchAddr && a < watchAddr + 4)) {
      const r = cpu.getRegs();
      const word = memory[watchAddr] | (memory[watchAddr+1] << 8);
      const word2 = memory[watchAddr+2] | (memory[watchAddr+3] << 8);
      console.error(`  [T${currentTick}] WATCH ${hex(watchAddr,5)}: byte[${a - watchAddr}]=${hex(val & 0xFF, 2)} -> dword=${hex(word2)}:${hex(word)} from ${hex(r.cs)}:${hex(r.ip)}`);
    }
    if (a >= 0xB8000 && a < 0xB9000 && vgaWriteTickStart >= 0 && currentTick >= vgaWriteTickStart && (vgaWriteTickEnd < 0 || currentTick <= vgaWriteTickEnd)) {
      const offset = a - 0xB8000;
      const row = Math.floor(offset / 160);
      const col = Math.floor((offset % 160) / 2);
      const isChar = (offset % 2) === 0;
      if (isChar) {
        const ch = val & 0xFF;
        const c = ch >= 0x20 && ch < 0x7F ? String.fromCharCode(ch) : `\\x${hex(ch, 2)}`;
        console.error(`  [T${currentTick}] VGA write row=${row} col=${col} char='${c}' (0x${hex(ch, 2)})`);
      }
      vgaWriteCount++;
    }
  },
  (addr) => memory[addr & 0xFFFFF],
  pic,
  pit,
  (type) => int_handler ? int_handler(type) : false,
);

cpu.reset();
cpu.setRegs({
  cs: 0x0060, ip: 0x0000,
  ss: 0x0030, sp: 0x0100,
  ds: 0, es: 0,
  ah: 0, al: 0, bh: 0, bl: 0, ch: 0, cl: 0, dh: 0, dl: 0,
});

int_handler = createBiosHandlers(
  memory, pic, kbd,
  () => cpu.getRegs(),
  (regs) => cpu.setRegs(regs),
);

function hex(v, w = 4) { return v.toString(16).toUpperCase().padStart(w, '0'); }

function getRegs() {
  const r = cpu.getRegs();
  return {
    AX: (r.ah << 8) | r.al, CX: (r.ch << 8) | r.cl,
    DX: (r.dh << 8) | r.dl, BX: (r.bh << 8) | r.bl,
    SP: r.sp, BP: r.bp, SI: r.si, DI: r.di,
    IP: r.ip, CS: r.cs, DS: r.ds, ES: r.es, SS: r.ss, FLAGS: r.flags,
  };
}

function flagBits(f) {
  const names = ['CF', '', 'PF', '', 'AF', '', 'ZF', 'SF', 'TF', 'IF', 'DF', 'OF'];
  return names.map((n, i) => n && (f & (1 << i)) ? n : '').filter(Boolean).join('|') || '(none)';
}

function dumpVGA() {
  console.log('\n--- VGA Text Buffer (B800:0000) ---');
  for (let row = 0; row < 25; row++) {
    let line = '';
    for (let col = 0; col < 80; col++) {
      const addr = 0xB8000 + (row * 80 + col) * 2;
      const ch = memory[addr];
      line += ch >= 0x20 && ch < 0x7F ? String.fromCharCode(ch) : ' ';
    }
    // Print all rows, with line number
    const trimmed = line.trimEnd();
    if (trimmed.length > 0) {
      console.log(`  ${String(row).padStart(2)}: ${line.trimEnd()}`);
    }
  }
  // Also show BDA cursor position (0040:0050-0051)
  const curRow = memory[0x451];
  const curCol = memory[0x450];
  console.log(`  Cursor: row=${curRow} col=${curCol}`);
  console.log('--- End VGA ---\n');
}

// --- Teletype capture: intercept INT 10h AH=0Eh ---
let ttyOutput = '';

// --- Run ---
console.error(`Running JS reference emulator in DOS mode (microcode BIOS), up to ${maxTicks} ticks...`);
console.error(`Kernel entry at 0060:0000`);

let prevIP = -1;
let prevCS = -1;
let stuckCount = 0;
let lastProgressTick = 0;

// Milestones to watch for
const milestones = new Map();

for (let tick = 0; tick < maxTicks; tick++) {
  currentTick = tick;
  const r = getRegs();
  const flatIP = r.CS * 16 + r.IP;

  // Print trace if requested
  if (traceAll || (traceFrom >= 0 && tick >= traceFrom)) {
    const instBytes = [];
    for (let i = 0; i < 6; i++) instBytes.push(hex(memory[(flatIP + i) & 0xFFFFF], 2));
    console.log(
      `T${tick}: ${hex(r.CS)}:${hex(r.IP)} [${instBytes.join(' ')}] ` +
      `AX=${hex(r.AX)} BX=${hex(r.BX)} CX=${hex(r.CX)} DX=${hex(r.DX)} ` +
      `SP=${hex(r.SP)} BP=${hex(r.BP)} SI=${hex(r.SI)} DI=${hex(r.DI)} ` +
      `DS=${hex(r.DS)} ES=${hex(r.ES)} SS=${hex(r.SS)} F=${hex(r.FLAGS)}[${flagBits(r.FLAGS)}]`
    );
  }

  // Detect key moments
  if (r.CS === 0x0060 && r.IP === 0x0000 && !milestones.has('kernel_entry')) {
    milestones.set('kernel_entry', tick);
    console.error(`  [T${tick}] Kernel entry at 0060:0000`);
  }

  // Detect INT 21h installation (kernel writes to IVT[0x21])
  // We'll check periodically
  if (tick % 10000 === 0 && tick > 0) {
    const int21ip = memory[0x84] | (memory[0x85] << 8);
    const int21cs = memory[0x86] | (memory[0x87] << 8);
    if (int21cs !== 0xF000 && !milestones.has('int21_installed')) {
      milestones.set('int21_installed', tick);
      console.error(`  [T${tick}] INT 21h installed at ${hex(int21cs)}:${hex(int21ip)}`);
    }
    // Check INT 29h handler
    if (tick === 10000 || tick === 100000 || tick === 500000 || tick === 1000000) {
      const int29ip = memory[0xA4] | (memory[0xA5] << 8);
      const int29cs = memory[0xA6] | (memory[0xA7] << 8);
      console.error(`  [T${tick}] INT 29h handler: ${hex(int29cs)}:${hex(int29ip)}`);
    }
  }

  // Detect COMMAND.COM execution (CS changes to something in low memory, not kernel segment)
  if (tick % 10000 === 0 && tick > 100000) {
    // Check if we've left the kernel segment range
    if (r.CS !== 0x0060 && r.CS !== 0xF000 && r.CS > 0x0060 && !milestones.has('command_com')) {
      // Could be COMMAND.COM or other code
      // Check: is the current CS:IP in a region that suggests COMMAND.COM?
      // COMMAND.COM would typically be loaded above the kernel
    }
  }

  // Check for halt
  const haltByte = memory[0x0504];
  if (haltByte === 1) {
    console.error(`  [T${tick}] HALT flag set at 0000:0504`);
    break;
  }

  // INT 10h teletype detection - check if we're entering INT 10h with AH=0Eh
  if (flatIP === 0xF0000 && r.AX >= 0x0E00 && r.AX < 0x0F00) {
    const ch = r.AX & 0xFF;
    if (ch === 13) { /* CR */ }
    else if (ch === 10) { ttyOutput += '\n'; }
    else if (ch >= 0x20 && ch < 0x7F) { ttyOutput += String.fromCharCode(ch); }
  }

  // INT call tracing
  if (intTrace || (intTraceFrom >= 0 && tick >= intTraceFrom)) {
    const opByte = memory[flatIP & 0xFFFFF];
    if (opByte === 0xCD) { // INT imm8
      const intNum = memory[(flatIP + 1) & 0xFFFFF];
      const ah = (r.AX >> 8) & 0xFF;
      const al = r.AX & 0xFF;
      let extra = '';
      // For INT 21h AH=40h (write), show the data being written
      if (intNum === 0x21 && ah === 0x40) {
        const base = r.DS * 16 + r.DX;
        const len = r.CX;
        let data = '';
        for (let i = 0; i < Math.min(len, 64); i++) {
          const b = memory[(base + i) & 0xFFFFF];
          data += b >= 0x20 && b < 0x7F ? String.fromCharCode(b) : `\\x${hex(b, 2)}`;
        }
        extra = ` DATA="${data}"`;
      }
      // For INT 21h AH=09h (print string), show the string
      if (intNum === 0x21 && ah === 0x09) {
        const base = r.DS * 16 + r.DX;
        let data = '';
        for (let i = 0; i < 80; i++) {
          const b = memory[(base + i) & 0xFFFFF];
          if (b === 0x24) break; // '$' terminator
          data += b >= 0x20 && b < 0x7F ? String.fromCharCode(b) : `\\x${hex(b, 2)}`;
        }
        extra = ` DATA="${data}"`;
      }
      // For INT 10h AH=0Eh (teletype), show the character
      if (intNum === 0x10 && ah === 0x0E) {
        const ch = al;
        extra = ` CHAR='${ch >= 0x20 && ch < 0x7F ? String.fromCharCode(ch) : `\\x${hex(ch, 2)}`}'`;
      }
      console.error(`  [T${tick}] INT ${hex(intNum, 2)}h AH=${hex(ah, 2)}h AL=${hex(al, 2)}h from ${hex(r.CS)}:${hex(r.IP)} BX=${hex(r.BX)} CX=${hex(r.CX)} DX=${hex(r.DX)} DS=${hex(r.DS)} ES=${hex(r.ES)}${extra}`);
    }
  }

  // Detect stuck (same CS:IP for too long)
  if (r.CS === prevCS && r.IP === prevIP) {
    stuckCount++;
    if (stuckCount >= 3) {
      console.error(`  [T${tick}] STUCK at ${hex(r.CS)}:${hex(r.IP)} for ${stuckCount} ticks`);
      console.error(`    AX=${hex(r.AX)} BX=${hex(r.BX)} CX=${hex(r.CX)} DX=${hex(r.DX)}`);
      console.error(`    SP=${hex(r.SP)} BP=${hex(r.BP)} FLAGS=${hex(r.FLAGS)} [${flagBits(r.FLAGS)}]`);
      break;
    }
  } else {
    stuckCount = 0;
  }

  prevCS = r.CS;
  prevIP = r.IP;

  // Progress
  if (tick - lastProgressTick >= 50000) {
    console.error(`  [T${tick}] ${hex(r.CS)}:${hex(r.IP)} AX=${hex(r.AX)} SP=${hex(r.SP)} FLAGS=${hex(r.FLAGS)}`);
    lastProgressTick = tick;
  }

  try {
    cpu.step();
  } catch (e) {
    const r2 = getRegs();
    console.error(`  [T${tick}] CPU ERROR: ${e.message} at ${hex(r2.CS)}:${hex(r2.IP)} (linear ${hex(r2.CS * 16 + r2.IP, 5)})`);
    console.error(`    AX=${hex(r2.AX)} BX=${hex(r2.BX)} CX=${hex(r2.CX)} DX=${hex(r2.DX)}`);
    console.error(`    SP=${hex(r2.SP)} DS=${hex(r2.DS)} ES=${hex(r2.ES)} SS=${hex(r2.SS)}`);
    const flat = r2.CS * 16 + r2.IP;
    const bytes = [];
    for (let i = 0; i < 8; i++) bytes.push(hex(memory[(flat + i) & 0xFFFFF], 2));
    console.error(`    Bytes: ${bytes.join(' ')}`);
    dumpVGA();
    process.exit(1);
  }
}

// Final state
const r = getRegs();
console.error(`\nFinal state at ${hex(r.CS)}:${hex(r.IP)}:`);
console.error(`  AX=${hex(r.AX)} BX=${hex(r.BX)} CX=${hex(r.CX)} DX=${hex(r.DX)}`);
console.error(`  SP=${hex(r.SP)} BP=${hex(r.BP)} SI=${hex(r.SI)} DI=${hex(r.DI)}`);
console.error(`  DS=${hex(r.DS)} ES=${hex(r.ES)} SS=${hex(r.SS)} FLAGS=${hex(r.FLAGS)}`);

// Milestones summary
if (milestones.size > 0) {
  console.error('\nMilestones:');
  for (const [name, tick] of milestones) {
    console.error(`  ${name}: tick ${tick}`);
  }
}

// TTY output
if (ttyOutput.length > 0) {
  console.error('\nTTY output captured:');
  console.error(ttyOutput);
}

// Always dump VGA at end
dumpVGA();
