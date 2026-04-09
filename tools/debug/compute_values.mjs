import { readFileSync } from 'fs';
import { resolve } from 'path';

const calciteDir = 'C:/Users/AdmT9N0CX01V65438A/Documents/src/calcite';
const cssDir = resolve(calciteDir, '..', 'CSS-DOS');
const kernelBin = readFileSync(resolve(cssDir, 'dos', 'bin', 'kernel.sys'));

function rd16(data, off) { return data[off] | (data[off+1] << 8); }
function hex(v, w=4) { return v.toString(16).toUpperCase().padStart(w, '0'); }

const CODE_START = 0x5E53;
const PADDING = rd16(kernelBin, CODE_START + 8);
const DOS_CODE_SIZE = rd16(kernelBin, CODE_START + 0x1E);
const DOS_DATA_LEN = rd16(kernelBin, CODE_START + 0x20);
const PCMODE_INIT_OFF = rd16(kernelBin, CODE_START + 0x0E);
const COMPRESSED = rd16(kernelBin, CODE_START + 0x1C);

console.log('=== Kernel Header Values ===');
console.log(`PADDING = 0x${hex(PADDING)}`);
console.log(`DOS_CODE_SIZE = 0x${hex(DOS_CODE_SIZE)} (${DOS_CODE_SIZE} bytes)`);
console.log(`DOS_DATA_LEN = 0x${hex(DOS_DATA_LEN)} (${DOS_DATA_LEN} bytes)`);
console.log(`pcmode_init from PADDING = 0x${hex(PCMODE_INIT_OFF)}`);
console.log(`Compressed = ${COMPRESSED}`);

const BDOS_FILE_OFFSET = CODE_START - PADDING;
const BDOS_CODE_TOTAL = PADDING + DOS_CODE_SIZE;
const DATA_FILE_OFFSET = CODE_START + DOS_CODE_SIZE;

console.log('\n=== Layout ===');
console.log(`BDOS segment base in file: 0x${hex(BDOS_FILE_OFFSET)}`);
console.log(`BDOS code total: 0x${hex(BDOS_CODE_TOTAL)} (${BDOS_CODE_TOTAL} bytes)`);
console.log(`DOS data at file offset: 0x${hex(DATA_FILE_OFFSET)}`);
console.log(`DOS data end: 0x${hex(DATA_FILE_OFFSET + DOS_DATA_LEN)}`);
console.log(`File size: 0x${hex(kernelBin.length)}`);

const BDOS_LINEAR = 0x600 + BDOS_FILE_OFFSET;
console.log(`\nBDOS at linear (kernel@0060): 0x${hex(BDOS_LINEAR)} (mod 16 = ${BDOS_LINEAR % 16})`);

// If we copy BDOS to right after the kernel load area:
const KERNEL_END = 0x600 + kernelBin.length;
const COPY_SEG = Math.ceil(KERNEL_END / 16);
console.log(`\nKernel end linear: 0x${hex(KERNEL_END)}`);
console.log(`Copy BDOS to segment: 0x${hex(COPY_SEG)}`);

const DOS_CSEG_ADJUSTED = COPY_SEG - (PADDING >> 4);
console.log(`\ndos_cseg (adjusted) = 0x${hex(DOS_CSEG_ADJUSTED)}`);
console.log(`dos_coff = 0x${hex(PADDING)}`);

const MEM_SIZE = 640 * 64;
const DOS_DATA_PARAS = Math.ceil(DOS_DATA_LEN / 16);
const DOS_DSEG = MEM_SIZE - DOS_DATA_PARAS;
console.log(`\nmem_size = 0x${hex(MEM_SIZE)}`);
console.log(`DOS_DATA paragraphs = 0x${hex(DOS_DATA_PARAS)}`);
console.log(`dos_dseg = 0x${hex(DOS_DSEG)}`);

const DATA_SRC_LINEAR = 0x600 + DATA_FILE_OFFSET;
console.log(`Data source linear: 0x${hex(DATA_SRC_LINEAR)}`);

const INT_STUBS_SEG = COPY_SEG + Math.ceil(BDOS_CODE_TOTAL / 16);
const FREE_SEG = INT_STUBS_SEG + 11;
console.log(`\nint_stubs_seg = 0x${hex(INT_STUBS_SEG)}`);
console.log(`free_seg = 0x${hex(FREE_SEG)}`);

console.log('\n=== Memory Layout ===');
console.log(`0x0060:0000 - Kernel load (${kernelBin.length} bytes)`);
console.log(`0x${hex(COPY_SEG)}:0000 - BDOS code copy (${BDOS_CODE_TOTAL} bytes)`);
console.log(`0x${hex(INT_STUBS_SEG)}:0000 - INT stubs (${11*16} bytes)`);
console.log(`0x${hex(FREE_SEG)}:0000 - Free memory starts`);
console.log(`0x${hex(DOS_DSEG)}:0000 - DOS data (${DOS_DATA_LEN} bytes)`);
console.log(`0x${hex(MEM_SIZE)}:0000 - End of conventional memory`);

// Verify data integrity
const nulOff = DATA_FILE_OFFSET + 0x48 + 10;
let nulName = '';
for (let i = 0; i < 8; i++) {
  const c = kernelBin[nulOff + i];
  nulName += c >= 0x20 && c < 0x7F ? String.fromCharCode(c) : '?';
}
console.log(`\nNUL device check: "${nulName}" (should be "NUL     ")`);

const sfthead_off = rd16(kernelBin, DATA_FILE_OFFSET + 0x2A);
const sfthead_seg = rd16(kernelBin, DATA_FILE_OFFSET + 0x2C);
console.log(`Initial sfthead: ${hex(sfthead_seg)}:${hex(sfthead_off)}`);
const sftt_count = rd16(kernelBin, DATA_FILE_OFFSET + 0xCC + 4);
console.log(`firstsftt count: ${sftt_count}`);

// Check if BDOS code copy would overlap with anything
const COPY_END = COPY_SEG * 16 + BDOS_CODE_TOTAL;
const DOS_DSEG_START = DOS_DSEG * 16;
console.log(`\nBDOS copy end: 0x${hex(COPY_END)}`);
console.log(`DOS data start: 0x${hex(DOS_DSEG_START)}`);
console.log(`Overlap? ${COPY_END > DOS_DSEG_START ? 'YES - PROBLEM!' : 'No - OK'}`);
