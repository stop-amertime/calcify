import { readFileSync } from 'fs';
import { resolve } from 'path';

const calciteDir = 'C:/Users/AdmT9N0CX01V65438A/Documents/src/calcite';
const js8086Source = readFileSync(resolve(calciteDir, 'tools/js8086.js'), 'utf-8');
const evalSource = js8086Source.replace("'use strict';", '').replace('let CPU_186 = 0;', 'var CPU_186 = 1;');
const Intel8086 = new Function(evalSource + '\nreturn Intel8086;')();

const cssDir = resolve(calciteDir, '..', 'CSS-DOS');
const biosBin = readFileSync(resolve(cssDir, 'gossamer-dos.bin'));
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
  const lst = readFileSync(resolve(cssDir, 'gossamer-dos.lst'), 'utf-8');
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

// Run past init
for (let tick = 0; tick < 120000; tick++) cpu.step();

// CS for DOS code = 0x0002
const CS = 0x0002;
const csBase = CS * 16;

// codeSeg is at DGROUP offset, read from pcmode_dseg
const dosdseg = 0x0BAB;
const dosBase = dosdseg * 16;

// codeSeg offset in DGROUP from header.asm data layout
// Looking at the source: codeSeg is deep in the data segment
// Let me find it by searching for the pattern
// codeSeg is set by pcmode_reinit: mov ds:codeSeg, cs
// So it should contain 0x0002
let codeSegOffset = -1;
// From the map: codeSeg might be near other known fields
// Let me check the map
console.log('Looking for codeSeg in DOS data...');
// codeSeg is at offset 0x000E in the func52_data area according to the source comments
// func52_data is at dos_data + 0x26
// So codeSeg might be at dos_data + 0x26 + 0x0E... but let me verify from the source
// Actually the source shows codeSeg at offset 762 in PCMODE_DATA: "codeSeg dw 0 ; 000E BDOS code segment"
// But "000E" is relative to an internal structure, not dos_data offset

// Let me just search for the value 0x0002 in the data segment that looks like codeSeg
// codeSeg should be a word containing CS (= 0x0002 after pcmode_reinit)
// It's near other known fields

// From the map: codeSeg is probably near other HMA-related fields
// Let me read the map for codeSeg
console.log('pcmode_dseg (at 0002:3806): ' + hex(rd16(csBase + 0x3806)));
console.log('codeSeg value in DGROUP should be 0x0002');

// Let me search for all 0x0002 words in DGROUP
const matches = [];
for (let off = 0; off < 0x1910; off += 2) {
  if (rd16(dosBase + off) === 0x0002) {
    matches.push(off);
  }
}
console.log(`Found ${matches.length} words with value 0x0002 in DGROUP`);
console.log('Offsets: ' + matches.map(o => hex(o)).join(', '));

// Now check: what value does the INT 21h entry point JMPF use?
// From the def_data_vecs code in pcmode_init:
// INT 21h handler is set up via stub entries
// The stub at int_stubs_seg contains: EA offset segment
// where segment = dos_dseg and offset = Int21Entry offset in data seg

// Let me trace the actual INT 21h call chain
const int21off = rd16(0x84);
const int21seg = rd16(0x86);
console.log(`\nINT 21h vector: ${hex(int21seg)}:${hex(int21off)}`);

// Follow the JMP FAR chain
let lin = int21seg * 16 + int21off;
console.log(`  At ${hex(int21seg)}:${hex(int21off)} (linear ${hex(lin, 5)}): ${hex(memory[lin], 2)} ${hex(memory[lin+1], 2)} ${hex(memory[lin+2], 2)} ${hex(memory[lin+3], 2)} ${hex(memory[lin+4], 2)}`);
if (memory[lin] === 0xEA) {
  const t1off = rd16(lin + 1);
  const t1seg = rd16(lin + 3);
  console.log(`  JMP FAR ${hex(t1seg)}:${hex(t1off)}`);
  lin = t1seg * 16 + t1off;
  console.log(`  At ${hex(t1seg)}:${hex(t1off)} (linear ${hex(lin, 5)}): ${hex(memory[lin], 2)} ${hex(memory[lin+1], 2)} ${hex(memory[lin+2], 2)} ${hex(memory[lin+3], 2)} ${hex(memory[lin+4], 2)}`);
  if (memory[lin] === 0xEA) {
    const t2off = rd16(lin + 1);
    const t2seg = rd16(lin + 3);
    console.log(`  JMP FAR ${hex(t2seg)}:${hex(t2off)}`);
  }
}

// Now check: the bytes at 0002:4FA2 that get executed as code
// What are they in the kernel binary?
// CS=0002, offset=4FA2
// Linear = 0x20 + 0x4FA2 = 0x4FC2
console.log(`\nBytes at 0002:4FA2 (linear 0x4FC2):`);
const corruptAddr = 0x4FC2;
let bytes = '';
for (let i = 0; i < 32; i++) bytes += hex(memory[corruptAddr + i], 2) + ' ';
console.log(`  ${bytes}`);

// Where is this in the kernel file?
// Kernel at BIO_SEG=0x0070, linear 0x700
// File offset = 0x4FC2 - 0x700 = 0x48C2
console.log(`  File offset: 0x${hex(0x4FC2 - 0x700)}`);

// But wait - was the kernel relocated by init0?
// After init0, kernel is at BIO_SEG = 0x0070 (linear 0x700)
// But init0 also decompresses! The decompressed layout differs from the file
// Let me check what's at this linear address AFTER decompression
console.log(`  This is DECOMPRESSED kernel data (not raw file bytes)`);

// The real question: why does a RET at 0002:4F4F return to 0002:4FA2?
// The stack had 0x4FA2 as the return address
// That was pushed by some CALL instruction
// Let me look at 0002:4FA2 - what function would have called there?
// If this is a dispatch table (like f58_tbl), the CALL cs:f58_tbl[si] would
// read an offset from the table and call it
// f58_tbl contains offsets to f58_get_strategy, f58_set_strategy, etc.
// If the table at runtime contains 0x4FA2, that's wrong

// Let me check: the func58 dispatch does call cs:f58_tbl[si]
// For AH=58h AL=02h (get UMB link): SI = 2*2 = 4
// f58_tbl[4] should point to f58_get_link

// The issue might be that cs:f58_tbl addresses the wrong memory
// because CS (0x0002) was set during pcmode_reinit and the code
// was decompressed to a location that doesn't match

// Let me find f58_tbl in the decompressed kernel
// func58 source has: call cs:f58_tbl[si]
// This is encoded as: 2E FF 94 xx xx (CALL FAR [SI + xxxx] with CS prefix)
// Or: 2E FF 14 xx xx for near call

// Let me search for the pattern "2E FF" near the dispatch area
console.log(`\nSearching for CALL CS:[...] pattern near func58...`);
for (let off = 0x3800; off < 0x6000; off++) {
  const a = csBase + off;
  if (memory[a] === 0x2E && memory[a+1] === 0xFF && memory[a+2] === 0x94) {
    // CALL cs:[SI + disp16]
    const disp = rd16(a + 3);
    console.log(`  ${hex(CS)}:${hex(off)} CALL CS:[SI+${hex(disp)}]`);
    // Read the table entries
    for (let entry = 0; entry < 4; entry++) {
      const tableAddr = csBase + disp + entry * 2;
      const target = rd16(tableAddr);
      console.log(`    [${entry}] = ${hex(target)}`);
    }
  }
}
