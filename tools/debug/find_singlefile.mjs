import { readFileSync } from 'fs';
const k = readFileSync('C:/Users/AdmT9N0CX01V65438A/Documents/src/CSS-DOS/dos/bin/kernel.sys');
function hex(v, w = 4) { return v.toString(16).toUpperCase().padStart(w, '0'); }
function rd16(d, o) { return d[o] | (d[o + 1] << 8); }

// The SINGLEFILE copy code in init.asm (biosinit.asm actually) does:
//   mov current_dos, ax      ; A3 xx xx (prevent read_dos)
//   mov es, ax               ; 8E C0
//   mov ax, offset CGROUP:DATAEND  ; B8 xx xx
//   mov cl, 4                ; B1 04
//   shr ax, cl               ; D3 E8
//   push cs                  ; 0E
//   pop si                   ; 5E (or pop ds = 1F?)
//   add ax, si               ; 03 C6 (or 01 F0)
//   mov ds, ax               ; 8E D8

// The pattern to search for: B1 04 D3 E8 0E (MOV CL,4; SHR AX,CL; PUSH CS)
// This should be unique enough

for (let i = 0; i < k.length - 10; i++) {
  if (k[i] === 0xB1 && k[i + 1] === 0x04 &&      // MOV CL, 4
      k[i + 2] === 0xD3 && k[i + 3] === 0xE8 &&   // SHR AX, CL
      k[i + 4] === 0x0E) {                          // PUSH CS
    // Show context
    const start = Math.max(0, i - 10);
    const bytes = Array.from(k.slice(start, i + 20)).map(b => hex(b, 2)).join(' ');
    console.log(`Found SHR pattern at file offset 0x${hex(i)}:`);
    console.log(`  Context: ${bytes}`);

    // Check if preceded by MOV AX, imm16 (B8 xx xx) within 10 bytes
    for (let j = i - 8; j < i; j++) {
      if (j >= 0 && k[j] === 0xB8) {
        const imm = rd16(k, j + 1);
        console.log(`  MOV AX, 0x${hex(imm)} at offset 0x${hex(j)} (${j - i} bytes before SHR)`);
        console.log(`  0x${hex(imm)} mod 16 = ${imm % 16}`);
        if (imm % 16 !== 0) {
          console.log(`  *** NOT paragraph-aligned! This is likely the DATAEND MOV ***`);
        }
      }
    }
    console.log();
  }
}

// Also look for the specific biosinit SINGLEFILE copy pattern more broadly
// It should be near: A3 xx xx 8E C0 B8 xx xx B1 04 D3 E8
// (mov [xxxx], ax; mov es, ax; mov ax, imm; mov cl, 4; shr ax, cl)
console.log('Searching for MOV [xxxx],AX; MOV ES,AX; MOV AX,imm pattern...');
for (let i = 0; i < k.length - 12; i++) {
  if (k[i] === 0xA3 &&           // MOV [xxxx], AX
      k[i + 3] === 0x8E && k[i + 4] === 0xC0 &&  // MOV ES, AX
      k[i + 5] === 0xB8 &&       // MOV AX, imm16
      k[i + 8] === 0xB1 && k[i + 9] === 0x04) {  // MOV CL, 4
    const storeAddr = rd16(k, i + 1);
    const dataend = rd16(k, i + 6);
    console.log(`Found at file offset 0x${hex(i)}:`);
    console.log(`  MOV [${hex(storeAddr)}], AX  (current_dos)`);
    console.log(`  MOV ES, AX`);
    console.log(`  MOV AX, 0x${hex(dataend)}  (DATAEND)`);
    console.log(`  MOV CL, 4`);
    const bytes = Array.from(k.slice(i, i + 20)).map(b => hex(b, 2)).join(' ');
    console.log(`  Bytes: ${bytes}`);
  }
}
