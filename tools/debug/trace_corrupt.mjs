import { readFileSync } from 'fs';
import { resolve } from 'path';

const calciteDir = 'C:/Users/AdmT9N0CX01V65438A/Documents/src/calcite';
const js8086Source = readFileSync(resolve(calciteDir, 'tools/js8086.js'), 'utf-8');
const evalSource = js8086Source.replace("'use strict';", '').replace('let CPU_186 = 0;', 'var CPU_186 = 1;');
const Intel8086 = new Function(evalSource + '\nreturn Intel8086;')();

const cssDir = resolve(calciteDir, '..', 'CSS-DOS');
const biosBin = readFileSync(resolve(cssDir, 'build', 'gossamer-dos.bin'));
const kernelBin = readFileSync(resolve(cssDir, 'dos', 'bin', 'kernel.sys'));
const diskBin = readFileSync(resolve(cssDir, 'dos', 'disk.img'));

const memory = new Uint8Array(1024 * 1024);
for (let i = 0; i < kernelBin.length; i++) memory[0x600 + i] = kernelBin[i];
for (let i = 0; i < diskBin.length && 0xD0000 + i < memory.length; i++) memory[0xD0000 + i] = diskBin[i];
for (let i = 0; i < biosBin.length; i++) memory[0xF0000 + i] = biosBin[i];

const cpu = Intel8086(
  (addr, val) => { memory[addr & 0xFFFFF] = val & 0xFF; },
  (addr) => memory[addr & 0xFFFFF],
);
cpu.reset();

let biosInitOffset = 0x038A;
try {
  const lst = readFileSync(resolve(cssDir, 'build', 'gossamer-dos.lst'), 'utf-8');
  for (const line of lst.split('\n')) {
    if (line.includes('bios_init:')) {
      const idx = lst.split('\n').indexOf(line);
      const m = lst.split('\n')[idx + 1]?.match(/([0-9A-Fa-f]{8})/);
      if (m) biosInitOffset = parseInt(m[1], 16);
      break;
    }
  }
} catch {}

cpu.setRegs({ cs: 0xF000, ip: biosInitOffset, ss: 0, sp: 0xFFF8, ds: 0, es: 0,
  ah: 0, al: 0, bh: 0, bl: 0, ch: 0, cl: 0, dh: 0, dl: 0 });

function hex(v, w = 4) { return v.toString(16).toUpperCase().padStart(w, '0'); }
function rd16(a) { return memory[a] | (memory[a + 1] << 8); }

// Run to just before the first corruption at tick 1060960
for (let tick = 0; tick < 1060950; tick++) cpu.step();

// Now trace 50 ticks with full detail
console.log('=== Tracing around sfthead corruption (ticks 1060950-1061000) ===');
console.log('sfthead_seg is at linear 0xBADC (0BAB:002C)');
console.log();

for (let tick = 1060950; tick < 1061000; tick++) {
  const r = cpu.getRegs();
  const cs = r.cs, ip = r.ip;
  const flat = (cs * 16 + ip) & 0xFFFFF;
  const bytes = Array.from({ length: 8 }, (_, i) => hex(memory[(flat + i) & 0xFFFFF], 2)).join(' ');
  const ax = (r.ah << 8) | r.al;
  const bx = (r.bh << 8) | r.bl;
  const cx = (r.ch << 8) | r.cl;
  const dx = (r.dh << 8) | r.dl;

  // Decode the instruction if it writes to [BX] with DS=0BAB and BX=002C
  const opcode = memory[flat];
  let note = '';
  if (r.ds === 0x0BAB && bx === 0x002C) {
    if (opcode === 0x00) note = ' *** ADD [BX], AL -> writes to sfthead_seg! ***';
    if (opcode === 0x89) note = ' *** MOV [BX], reg -> writes to sfthead_seg! ***';
  }

  console.log(`T${tick}: ${hex(cs)}:${hex(ip)} [${bytes}] AX=${hex(ax)} BX=${hex(bx)} CX=${hex(cx)} DX=${hex(dx)} SI=${hex(r.si)} DI=${hex(r.di)} DS=${hex(r.ds)} ES=${hex(r.es)} SS=${hex(r.ss)} SP=${hex(r.sp)} BP=${hex(r.bp)}${note}`);

  cpu.step();
}

// Check sfthead value
console.log();
console.log(`sfthead_seg after: ${hex(rd16(0xBADC))}`);
