import { readFileSync } from 'fs';
const k = readFileSync('C:/Users/AdmT9N0CX01V65438A/Documents/src/CSS-DOS/dos/bin/kernel.sys');
function hex(v, w = 4) { return v.toString(16).toUpperCase().padStart(w, '0'); }

// Search for MOV AX, 0x5E53 (B8 53 5E)
for (let i = 0; i < k.length - 3; i++) {
  if (k[i] === 0xB8 && k[i + 1] === 0x53 && k[i + 2] === 0x5E) {
    const bytes = Array.from(k.slice(i, i + 20)).map(b => hex(b, 2)).join(' ');
    console.log(`Found MOV AX, 0x5E53 at file offset 0x${hex(i)}: ${bytes}`);
  }
}

// Also search for the value 0x5E53 as a 16-bit word anywhere
let count = 0;
for (let i = 0; i < k.length - 1; i++) {
  if (k[i] === 0x53 && k[i + 1] === 0x5E) {
    count++;
    if (count <= 10) {
      console.log(`Word 0x5E53 at offset 0x${hex(i)}: context = ${Array.from(k.slice(Math.max(0,i-2), i+6)).map(b => hex(b,2)).join(' ')}`);
    }
  }
}
console.log(`Total occurrences of word 0x5E53: ${count}`);
