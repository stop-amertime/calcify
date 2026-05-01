#!/usr/bin/env node
// calc-mem.mjs — Calculate minimum --mem value for a DOS program
//
// Usage: node tools/calc-mem.mjs <program.com|program.exe>
//
// Outputs a hex --mem value suitable for generate-dos.mjs.
//
// For .COM files: kernel overhead + PSP + code + 4KB stack, rounded up to 16KB
// For .EXE files: kernel overhead + code + minalloc from MZ header, rounded up
//
// The DOS kernel needs:
//   Low area:  0x00000-0x30000 (192KB) — IVT + BDA + kernel + init workspace
//   High area: top 104KB (0x1A000) — relocated BIOS, COMMAND.COM, MCBs
// These must not overlap, so minimum memBytes = 0x30000 + 0x1A000 = 0x4A000 (296KB).
// Program loads in the gap between low and high areas.

import { readFileSync, statSync } from 'fs';

const file = process.argv[2];
if (!file) {
  console.error('Usage: node calc-mem.mjs <program.com|program.exe>');
  process.exit(1);
}

const KERNEL_OVERHEAD = 0x4A000; // 296KB minimum for kernel low + high areas
const PAGE_SIZE = 0x4000;        // 16KB rounding granularity

const ext = file.toLowerCase().split('.').pop();
const fileName = file.replace(/\\/g, '/').split('/').pop().toUpperCase();
let fileSize = statSync(file).size;

// shell.com is a tiny stub that launches COMMAND.COM — size the memory for COMMAND.COM
if (fileName === 'SHELL.COM' && fileSize <= 16) {
  const cmdComPath = new URL('../../CSS-DOS/dos/bin/command.com', import.meta.url).pathname.replace(/^\/([A-Z]:)/, '$1');
  try {
    fileSize = statSync(cmdComPath).size;
  } catch {
    fileSize = 32768; // fallback: assume 32KB
  }
}

let programMem;

if (ext === 'exe') {
  const buf = readFileSync(file);
  if (buf.length < 28 || (buf[0] !== 0x4D && buf[0] !== 0x5A)) {
    // Not a valid MZ, treat like .com
    programMem = fileSize + 0x1000;
  } else {
    const lastPageBytes = buf.readUInt16LE(2);
    const pages = buf.readUInt16LE(4);
    const hdrParas = buf.readUInt16LE(8);
    const minalloc = buf.readUInt16LE(10);
    const codeSize = (pages - (lastPageBytes ? 1 : 0)) * 512 + lastPageBytes - hdrParas * 16;
    // Program needs: code image + minalloc extra paragraphs (16 bytes each)
    programMem = codeSize + minalloc * 16 + 0x1000; // +4KB safety
  }
} else {
  // .COM: loads at 0x100, uses up to code size + stack
  programMem = 0x100 + fileSize + 0x1000; // PSP + code + 4KB stack
}

// V4 architecture uses contiguous memory (0 to memBytes). The kernel
// relocates to the top of conventional memory and needs the full 640KB.
// Always return 0xA0000 — the split-memory optimization from V3 doesn't
// apply to V4's contiguous layout.
console.log('0xA0000');
