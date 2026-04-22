// Video-mode decode table for CSS-DOS players.
//
// The CSS cabinet doesn't know or care about video modes — the guest
// software writes bytes to VRAM (0xA0000 for Mode 13h, 0xB8000 for text
// and CGA gfx) and the BIOS stores the active mode byte at BDA 0x0449.
// This file is what turns that raw state into pixels on the player's
// framebuffer.
//
// Adding a mode is one entry in MODE_TABLE plus (usually) one rasteriser
// below. Players consume the table through pickMode() and dispatch to
// kind:'text'|'mode13'|'cga4'|'cga2' accordingly.
//
// Addresses:
//   0x0449 — BDA video mode byte (what get_video_mode() reads)
//   0x04F2 — BDA intra-app shadow of the RAW requested mode (before BIOS
//            remap). Lets the player warn about unimplemented modes.
//   0x04F3 — kiln shadow of OUT 0x3D9 (CGA palette register). Bits:
//              3..0  border colour / gfx-mode colour 0
//              4     intensity (bright/dim palette)
//              5     palette set (0=green/red/yellow, 1=cyan/magenta/white)

// Standard VGA 16-colour RGBA palette (alpha 0xFF, byte 0=R). Used for
// text modes and as the source for CGA palette slots.
export const VGA_PALETTE_U32 = new Uint32Array([
  0xFF000000, 0xFFAA0000, 0xFF00AA00, 0xFFAAAA00,
  0xFF0000AA, 0xFFAA00AA, 0xFF0055AA, 0xFFAAAAAA,
  0xFF555555, 0xFFFF5555, 0xFF55FF55, 0xFFFFFF55,
  0xFF5555FF, 0xFFFF55FF, 0xFF55FFFF, 0xFFFFFFFF,
]);

// 70 Hz VGA retrace cadence at the 4.77 MHz 8086 timebase. Same constant
// as kiln/patterns/misc.mjs (CYCLES_PER_FRAME) — used here for blink
// phases so cursor / attribute-bit-7 blink advance on simulated-time.
export const CYCLES_PER_FRAME = 68182;

// One entry per renderable mode. Unknown modes are not in the table —
// pickMode() returns null and the player falls back to a warning.
//
// Fields:
//   kind     — decoder flavour (see below)
//   width, height — pixel output geometry
//   vramAddr — base address the decoder reads
//   textCols, textRows — only for kind:'text'; drives the font-rasteriser grid.
export const MODE_TABLE = {
  0x00: { kind: 'text',   width: 320, height: 400, vramAddr: 0xB8000, textCols: 40, textRows: 25 },
  0x01: { kind: 'text',   width: 320, height: 400, vramAddr: 0xB8000, textCols: 40, textRows: 25 },
  0x02: { kind: 'text',   width: 640, height: 400, vramAddr: 0xB8000, textCols: 80, textRows: 25 },
  0x03: { kind: 'text',   width: 640, height: 400, vramAddr: 0xB8000, textCols: 80, textRows: 25 },
  0x04: { kind: 'cga4',   width: 320, height: 200, vramAddr: 0xB8000 },
  0x05: { kind: 'cga4',   width: 320, height: 200, vramAddr: 0xB8000, mono: true },
  0x07: { kind: 'text',   width: 720, height: 400, vramAddr: 0xB8000, textCols: 80, textRows: 25 },
  0x13: { kind: 'mode13', width: 320, height: 200, vramAddr: 0xA0000 },
};

// Human-readable names for status bars and diagnostic warnings.
export const MODE_NAMES = {
  0x00: 'CGA 40×25 text (mono)',
  0x01: 'CGA 40×25 text (colour)',
  0x02: 'CGA 80×25 text (mono)',
  0x03: 'CGA 80×25 text (colour)',
  0x04: 'CGA 320×200×4',
  0x05: 'CGA 320×200×4 (mono)',
  0x06: 'CGA 640×200×2',
  0x07: 'MDA/Hercules 80×25 mono text',
  0x0D: 'EGA 320×200×16 (planar)',
  0x0E: 'EGA 640×200×16 (planar)',
  0x0F: 'EGA 640×350 mono (planar)',
  0x10: 'EGA 640×350×16 (planar)',
  0x11: 'VGA 640×480×2',
  0x12: 'VGA 640×480×16 (planar)',
  0x13: 'VGA 320×200×256 (Mode 13h)',
};

export function modeName(m) {
  return MODE_NAMES[m] || `unknown mode 0x${m.toString(16).padStart(2, '0').toUpperCase()}`;
}

export function pickMode(modeByte) {
  return MODE_TABLE[modeByte] || null;
}

// ---------- CGA mode 0x04 decoder ----------
//
// 320×200 at 2 bits per pixel, with even/odd scanline interleave:
//   0xB8000 + offset  even scanlines (0, 2, 4, ... 198)
//   0xBA000 + offset  odd  scanlines (1, 3, 5, ... 199)
// Each byte holds 4 pixels, MSB-first:
//   bits 7..6 = pixel 0, bits 5..4 = pixel 1, 3..2 = pixel 2, 1..0 = pixel 3
// Pixel value 0 is the background (palette reg bits 3..0). Values 1..3
// index into one of the two fixed CGA palettes, brightened if bit 4 set:
//   palette 0 (bit 5 = 0): 1=green 2=red     3=brown/yellow
//   palette 1 (bit 5 = 1): 1=cyan  2=magenta 3=light-grey/white
// In real CGA "brown" becomes "yellow" when the intensity bit flips for
// colour 3 — we use the same VGA_PALETTE_U32 indices DOSBox picks
// (index 6 = brown, 14 = yellow).
const CGA_PAL_VGA_INDICES = [
  // [_, colour1, colour2, colour3], colour 0 is the bg from reg bits 3..0
  // palette 0, intensity 0
  [null, 2, 4, 6],
  // palette 0, intensity 1
  [null, 10, 12, 14],
  // palette 1, intensity 0
  [null, 3, 5, 7],
  // palette 1, intensity 1
  [null, 11, 13, 15],
];

export function decodeCga4(vram16k, paletteReg, outRGBA) {
  const palette1 = (paletteReg >> 5) & 1;
  const intensity = (paletteReg >> 4) & 1;
  const bgIdx = paletteReg & 0x0F;
  const bank = CGA_PAL_VGA_INDICES[palette1 * 2 + intensity];
  const pal = [
    VGA_PALETTE_U32[bgIdx],
    VGA_PALETTE_U32[bank[1]],
    VGA_PALETTE_U32[bank[2]],
    VGA_PALETTE_U32[bank[3]],
  ];
  const out32 = new Uint32Array(outRGBA.buffer, outRGBA.byteOffset, (outRGBA.byteLength / 4) | 0);
  // Two planes: evens at offset 0, odds at offset 0x2000.
  // Each plane holds 100 scanlines × 80 bytes = 8000 bytes; 192 bytes
  // of padding to fill out the 0x2000 window.
  const SCANLINE_BYTES = 80;
  for (let plane = 0; plane < 2; plane++) {
    const planeBase = plane * 0x2000;
    for (let py = 0; py < 100; py++) {
      const y = py * 2 + plane;
      const srcRow = planeBase + py * SCANLINE_BYTES;
      const dstRow = y * 320;
      for (let bx = 0; bx < SCANLINE_BYTES; bx++) {
        const b = vram16k[srcRow + bx];
        const px = dstRow + bx * 4;
        out32[px    ] = pal[(b >> 6) & 3];
        out32[px + 1] = pal[(b >> 4) & 3];
        out32[px + 2] = pal[(b >> 2) & 3];
        out32[px + 3] = pal[ b       & 3];
      }
    }
  }
}

// ---------- Text-mode rasteriser (8×16 VGA ROM font) ----------
//
// Was duplicated between calcite-bridge.js and calcite/web/calcite-worker.js;
// centralised here. The font atlas is the 4096-byte VGA ROM font (one
// glyph = 16 bytes, bit 7 = leftmost pixel). Blink phases are driven by
// opts.cycleCount so attribute bit-7 blink (~2 Hz) and cursor blink
// (~4 Hz) match real VGA timings regardless of how fast the guest runs.
export function rasteriseText(buf, cols, rows, outRGBA, fontAtlas, opts) {
  const pxW = cols * 8;
  const out32 = new Uint32Array(outRGBA.buffer, outRGBA.byteOffset, (outRGBA.byteLength / 4) | 0);
  const frame = Math.floor((opts?.cycleCount || 0) / CYCLES_PER_FRAME);
  const attrBlinkOn  = (frame & 16) === 0;
  const cursorBlinkOn = (frame & 8) === 0;
  const blinkMode = opts?.blinkMode !== false;
  for (let cy = 0; cy < rows; cy++) {
    for (let cx = 0; cx < cols; cx++) {
      const off = (cy * cols + cx) * 2;
      const ch = buf[off];
      const attr = buf[off + 1];
      let fgIdx = attr & 0x0F;
      let bgIdx = (attr >> 4) & 0x0F;
      if (blinkMode && (attr & 0x80)) {
        bgIdx &= 0x07;
        if (!attrBlinkOn) fgIdx = bgIdx;
      }
      const fg = VGA_PALETTE_U32[fgIdx];
      const bg = VGA_PALETTE_U32[bgIdx];
      const glyphBase = ch * 16;
      const pxX = cx * 8;
      for (let gy = 0; gy < 16; gy++) {
        const row = fontAtlas[glyphBase + gy];
        const outRow = (cy * 16 + gy) * pxW + pxX;
        out32[outRow + 0] = (row & 0x80) ? fg : bg;
        out32[outRow + 1] = (row & 0x40) ? fg : bg;
        out32[outRow + 2] = (row & 0x20) ? fg : bg;
        out32[outRow + 3] = (row & 0x10) ? fg : bg;
        out32[outRow + 4] = (row & 0x08) ? fg : bg;
        out32[outRow + 5] = (row & 0x04) ? fg : bg;
        out32[outRow + 6] = (row & 0x02) ? fg : bg;
        out32[outRow + 7] = (row & 0x01) ? fg : bg;
      }
    }
  }
  if (opts?.cursorEnabled && cursorBlinkOn
      && opts.cursorRow >= 0 && opts.cursorRow < rows
      && opts.cursorCol >= 0 && opts.cursorCol < cols) {
    const cx = opts.cursorCol, cy = opts.cursorRow;
    const attr = buf[(cy * cols + cx) * 2 + 1];
    const cursorColor = VGA_PALETTE_U32[attr & 0x0F];
    const startScan = 13, endScan = 14;
    const pxX = cx * 8;
    for (let gy = startScan; gy <= endScan; gy++) {
      const outRow = (cy * 16 + gy) * pxW + pxX;
      for (let k = 0; k < 8; k++) out32[outRow + k] = cursorColor;
    }
  }
}
