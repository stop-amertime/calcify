/**
 * JS8086 Web Worker — runs the JS reference 8086 emulator off the main thread.
 *
 * Protocol (same as calcite-worker.js):
 *   Main -> Worker:
 *     { type: 'init', kernel: ArrayBuffer, disk: ArrayBuffer, js8086Src: string }
 *     { type: 'tick', count: number }
 *     { type: 'keyboard', key: number }
 *
 *   Worker -> Main:
 *     { type: 'ready', video: { text, gfx } }
 *     { type: 'tick-result', screen, gfxBytes, videoMode, ticks }
 *     { type: 'error', message: string }
 */

let cpu = null;
let memory = null;
let pic = null;
let pit = null;
let kbd = null;
let halted = false;

// CGA text-mode color palette -> CSS colors (for reference, not used in worker)
const BIOS_SEG = 0xF000;
const BDA_BASE = 0x0400;

self.onmessage = function (event) {
  const { type, ...data } = event.data;
  try {
    switch (type) {
      case 'init': {
        // --- Load js8086 ---
        const evalSource = data.js8086Src
          .replace("'use strict';", '')
          .replace('let CPU_186 = 0;', 'var CPU_186 = 1;');
        const Intel8086 = new Function(evalSource + '\nreturn Intel8086;')();

        // --- Setup 1MB memory ---
        memory = new Uint8Array(1024 * 1024);

        // Load kernel at 0060:0000 (linear 0x600)
        const kernelBytes = new Uint8Array(data.kernel);
        for (let i = 0; i < kernelBytes.length; i++) memory[0x600 + i] = kernelBytes[i];

        // Load disk image at D000:0000 (linear 0xD0000)
        const diskBytes = new Uint8Array(data.disk);
        for (let i = 0; i < diskBytes.length && 0xD0000 + i < memory.length; i++) {
          memory[0xD0000 + i] = diskBytes[i];
        }

        // --- Build BIOS ROM (same as generate-dos.mjs) ---
        const IVT_ENTRIES = {
          0x08: 0x08, 0x09: 0x09, 0x10: 0x10, 0x11: 0x11,
          0x12: 0x12, 0x13: 0x13, 0x15: 0x15, 0x16: 0x16,
          0x19: 0x19, 0x1A: 0x1A, 0x20: 0x20,
        };
        const rom = [];
        const handlers = {};
        for (const [intNum, routineId] of Object.entries(IVT_ENTRIES)) {
          handlers[parseInt(intNum)] = rom.length;
          rom.push(0xD6, routineId, 0xCF);
        }
        const biosBytes = [0xCF, ...rom]; // leading IRET for dummy handler
        for (const intNum of Object.keys(handlers)) handlers[intNum] += 1;
        for (let i = 0; i < biosBytes.length; i++) memory[0xF0000 + i] = biosBytes[i];

        // --- IVT: 256 entries default to IRET at F000:0000 ---
        for (let i = 0; i < 256; i++) {
          memory[i * 4 + 0] = 0x00;
          memory[i * 4 + 1] = 0x00;
          memory[i * 4 + 2] = BIOS_SEG & 0xFF;
          memory[i * 4 + 3] = (BIOS_SEG >> 8) & 0xFF;
        }
        for (const [intNum, stubOffset] of Object.entries(handlers)) {
          const idx = parseInt(intNum);
          memory[idx * 4 + 0] = stubOffset & 0xFF;
          memory[idx * 4 + 1] = (stubOffset >> 8) & 0xFF;
          memory[idx * 4 + 2] = BIOS_SEG & 0xFF;
          memory[idx * 4 + 3] = (BIOS_SEG >> 8) & 0xFF;
        }

        // --- BDA ---
        memory[BDA_BASE + 0x10] = 0x21; memory[BDA_BASE + 0x11] = 0x00;
        memory[BDA_BASE + 0x13] = 640 & 0xFF; memory[BDA_BASE + 0x14] = (640 >> 8) & 0xFF;
        memory[BDA_BASE + 0x1A] = 0x1E; memory[BDA_BASE + 0x1B] = 0x00;
        memory[BDA_BASE + 0x1C] = 0x1E; memory[BDA_BASE + 0x1D] = 0x00;
        memory[BDA_BASE + 0x80] = 0x1E; memory[BDA_BASE + 0x81] = 0x00;
        memory[BDA_BASE + 0x82] = 0x3E; memory[BDA_BASE + 0x83] = 0x00;
        memory[BDA_BASE + 0x49] = 0x03;
        memory[BDA_BASE + 0x4A] = 80; memory[BDA_BASE + 0x4B] = 0;
        memory[BDA_BASE + 0x4C] = 0x00; memory[BDA_BASE + 0x4D] = 0x10;
        memory[BDA_BASE + 0x60] = 0x07; memory[BDA_BASE + 0x61] = 0x06;
        memory[BDA_BASE + 0x63] = 0xD4; memory[BDA_BASE + 0x64] = 0x03;
        memory[BDA_BASE + 0x84] = 24;
        memory[BDA_BASE + 0x85] = 16;

        // --- Peripherals ---
        pic = createPIC();
        pit = createPIT(pic);
        kbd = createKeyboardController(pic);

        let int_handler = null;

        // --- Create CPU ---
        cpu = Intel8086(
          (addr, val) => { memory[addr & 0xFFFFF] = val & 0xFF; },
          (addr) => memory[addr & 0xFFFFF],
          pic, pit,
          (t) => int_handler ? int_handler(t) : false,
        );

        cpu.reset();
        cpu.setRegs({
          cs: 0x0060, ip: 0x0000,
          ss: 0x0030, sp: 0x0100,
          ds: 0, es: 0,
          ah: 0, al: 0, bh: 0, bl: 0, ch: 0, cl: 0, dh: 0, dl: 0,
        });

        int_handler = createBiosHandlers(memory, pic, kbd,
          () => cpu.getRegs(),
          (regs) => cpu.setRegs(regs),
        );

        halted = false;

        self.postMessage({
          type: 'ready',
          video: {
            text: { addr: 0xB8000, size: 4000, width: 80, height: 25 },
            gfx: { addr: 0xA0000, size: 64000, width: 320, height: 200 },
          },
        });
        break;
      }

      case 'tick': {
        if (!cpu || halted) {
          self.postMessage({
            type: 'tick-result',
            screen: renderScreen(),
            gfxBytes: null,
            videoMode: memory[BDA_BASE + 0x49],
            ticks: 0,
          });
          break;
        }

        const count = data.count || 100;
        let ran = 0;
        for (let i = 0; i < count; i++) {
          if (memory[0x0504] === 1) { halted = true; break; }
          try {
            cpu.step();
            ran++;
          } catch (e) {
            const r = cpu.getRegs();
            console.error(`CPU ERROR at ${hex(r.cs)}:${hex(r.ip)}: ${e.message}`);
            halted = true;
            break;
          }
        }

        const videoMode = memory[BDA_BASE + 0x49];
        const isGfx = videoMode === 0x13;

        let screen = null;
        let gfxBytes = null;
        const transfer = [];

        if (!isGfx) {
          screen = renderScreen();
        } else {
          // Mode 13h: convert 8-bit palette to RGBA
          const buf = new ArrayBuffer(320 * 200 * 4);
          const rgba = new Uint8Array(buf);
          for (let i = 0; i < 64000; i++) {
            const palIdx = memory[0xA0000 + i];
            const [r, g, b] = defaultVgaPalette[palIdx] || [0, 0, 0];
            rgba[i * 4] = r;
            rgba[i * 4 + 1] = g;
            rgba[i * 4 + 2] = b;
            rgba[i * 4 + 3] = 255;
          }
          gfxBytes = buf;
          transfer.push(buf);
        }

        self.postMessage(
          { type: 'tick-result', screen, gfxBytes, videoMode, ticks: ran },
          transfer,
        );
        break;
      }

      case 'keyboard': {
        if (kbd) {
          const key = data.key || 0;
          if (key) kbd.feedKey(key);
        }
        break;
      }
    }
  } catch (err) {
    self.postMessage({ type: 'error', message: err.message || String(err) });
  }
};

function hex(v) { return v.toString(16).toUpperCase().padStart(4, '0'); }

function renderScreen() {
  if (!memory) return '';
  const cols = 80, rows = 25;
  let lines = [];
  for (let r = 0; r < rows; r++) {
    let line = '';
    for (let c = 0; c < cols; c++) {
      const ch = memory[0xB8000 + (r * cols + c) * 2];
      line += ch >= 0x20 && ch < 0x7F ? String.fromCharCode(ch) : ' ';
    }
    lines.push(line);
  }
  return lines.join('\n');
}

// ── Inline peripherals (same as CSS-DOS/tools/peripherals.mjs) ──

function createPIC() {
  return {
    mask: 0xFF, pending: 0, inService: 0,
    isConnected(port) { return port === 0x20 || port === 0x21; },
    portOut(w, port, val) {
      if (port === 0x20 && val === 0x20) {
        for (let i = 0; i < 8; i++) {
          if (this.inService & (1 << i)) { this.inService &= ~(1 << i); break; }
        }
      } else if (port === 0x21) { this.mask = val & 0xFF; }
    },
    portIn(w, port) {
      if (port === 0x21) return this.mask;
      if (port === 0x20) return this.inService;
      return 0;
    },
    raiseIRQ(n) { this.pending |= (1 << n); },
    hasInt() {
      for (let i = 0; i < 8; i++) {
        if (this.inService & (1 << i)) return false;
        if ((this.pending & (1 << i)) && !(this.mask & (1 << i))) return true;
      }
      return false;
    },
    nextInt() {
      for (let i = 0; i < 8; i++) {
        if (this.inService & (1 << i)) return 0;
        if ((this.pending & (1 << i)) && !(this.mask & (1 << i))) {
          this.pending &= ~(1 << i);
          this.inService |= (1 << i);
          return 0x08 + i;
        }
      }
      return 0;
    },
    tick() {},
  };
}

function createPIT(pic) {
  const channels = [];
  for (let i = 0; i < 3; i++) {
    channels.push({ counter: 0, reload: 0, mode: 0, latched: false, latchValue: 0,
                     rwMode: 3, readState: 0, writeState: 0 });
  }
  return {
    channels,
    isConnected(port) { return port >= 0x40 && port <= 0x43; },
    portOut(w, port, val) {
      if (port === 0x43) {
        const ch = (val >> 6) & 3;
        if (ch === 3) return;
        const rw = (val >> 4) & 3;
        const mode = (val >> 1) & 7;
        const channel = channels[ch];
        if (rw === 0) { if (!channel.latched) { channel.latched = true; channel.latchValue = channel.counter; } return; }
        channel.mode = mode; channel.rwMode = rw; channel.writeState = 0; channel.readState = 0; channel.counter = 0; channel.reload = 0;
      } else {
        const ch = port - 0x40;
        if (ch < 0 || ch > 2) return;
        const channel = channels[ch];
        if (channel.rwMode === 1) { channel.reload = val & 0xFF; channel.counter = channel.reload; }
        else if (channel.rwMode === 2) { channel.reload = (val & 0xFF) << 8; channel.counter = channel.reload; }
        else {
          if (channel.writeState === 0) { channel.reload = (channel.reload & 0xFF00) | (val & 0xFF); channel.writeState = 1; }
          else { channel.reload = (channel.reload & 0x00FF) | ((val & 0xFF) << 8); channel.writeState = 0; channel.counter = channel.reload; }
        }
      }
    },
    portIn(w, port) {
      const ch = port - 0x40; if (ch < 0 || ch > 2) return 0;
      const channel = channels[ch];
      let value = channel.latched ? channel.latchValue : channel.counter;
      if (channel.rwMode === 1) { channel.latched = false; return value & 0xFF; }
      if (channel.rwMode === 2) { channel.latched = false; return (value >> 8) & 0xFF; }
      if (channel.readState === 0) { channel.readState = 1; return value & 0xFF; }
      channel.readState = 0; channel.latched = false; return (value >> 8) & 0xFF;
    },
    tick() {
      for (let ch = 0; ch < 3; ch++) {
        const channel = channels[ch];
        if (channel.reload === 0) continue;
        if (channel.mode === 2) { channel.counter--; if (channel.counter <= 0) { channel.counter = channel.reload; if (ch === 0) pic.raiseIRQ(0); } }
        else if (channel.mode === 3) { channel.counter -= 2; if (channel.counter <= 0) { channel.counter = channel.reload; if (ch === 0) pic.raiseIRQ(0); } }
      }
    },
  };
}

function createKeyboardController(pic) {
  return {
    queue: [], currentWord: 0,
    isConnected(port) { return port === 0x60 || port === 0x61; },
    feedKey(keyWord) { this.queue.push(keyWord & 0xFFFF); pic.raiseIRQ(1); },
    portIn(w, port) {
      if (port === 0x60) {
        if (this.queue.length > 0) this.currentWord = this.queue.shift();
        return (this.currentWord >> 8) & 0xFF;
      }
      return 0;
    },
    portOut(w, port, val) {},
    tick() {},
  };
}

// ── Inline BIOS handlers (same as CSS-DOS/tools/lib/bios-handlers.mjs) ──

function createBiosHandlers(memory, pic, kbd, getRegs, setRegs) {
  const KBD_BUF_START = 0x001E, KBD_BUF_END = 0x003E;

  function int09h() {
    const scancode = kbd.portIn(0, 0x60);
    if (scancode === 0) { pic.portOut(0, 0x20, 0x20); return true; }
    const ascii = kbd.currentWord & 0xFF;
    const tail = memory[BDA_BASE + 0x1C] | (memory[BDA_BASE + 0x1D] << 8);
    const head = memory[BDA_BASE + 0x1A] | (memory[BDA_BASE + 0x1B] << 8);
    let newTail = tail + 2;
    if (newTail >= KBD_BUF_END) newTail = KBD_BUF_START;
    if (newTail !== head) {
      memory[BDA_BASE + tail] = ascii;
      memory[BDA_BASE + tail + 1] = scancode;
      memory[BDA_BASE + 0x1C] = newTail & 0xFF;
      memory[BDA_BASE + 0x1D] = (newTail >> 8) & 0xFF;
    }
    pic.portOut(0, 0x20, 0x20);
    return true;
  }

  function int16h() {
    const regs = getRegs();
    if (regs.ah === 0x00) {
      const head = memory[BDA_BASE + 0x1A] | (memory[BDA_BASE + 0x1B] << 8);
      const tail = memory[BDA_BASE + 0x1C] | (memory[BDA_BASE + 0x1D] << 8);
      if (head === tail) { setRegs({ ip: regs.ip - 2 }); return true; }
      const ascii = memory[BDA_BASE + head];
      const scancode = memory[BDA_BASE + head + 1];
      setRegs({ al: ascii, ah: scancode });
      let newHead = head + 2;
      if (newHead >= KBD_BUF_END) newHead = KBD_BUF_START;
      memory[BDA_BASE + 0x1A] = newHead & 0xFF;
      memory[BDA_BASE + 0x1B] = (newHead >> 8) & 0xFF;
      return true;
    }
    if (regs.ah === 0x01) {
      const head = memory[BDA_BASE + 0x1A] | (memory[BDA_BASE + 0x1B] << 8);
      const tail = memory[BDA_BASE + 0x1C] | (memory[BDA_BASE + 0x1D] << 8);
      if (head === tail) { setRegs({ flags: regs.flags | 0x0040 }); }
      else {
        setRegs({ al: memory[BDA_BASE + head], ah: memory[BDA_BASE + head + 1], flags: regs.flags & ~0x0040 });
      }
      return true;
    }
    return true;
  }

  function int10h() {
    const regs = getRegs();
    if (regs.ah === 0x0E) {
      const ch = regs.al;
      const cursorRow = memory[BDA_BASE + 0x51];
      const cursorCol = memory[BDA_BASE + 0x50];
      const cols = 80;
      if (ch === 0x0D) { memory[BDA_BASE + 0x50] = 0; }
      else if (ch === 0x0A) {
        if (cursorRow < 24) { memory[BDA_BASE + 0x51] = cursorRow + 1; }
        else { scrollUp(cols, 0x07); }
      } else if (ch === 0x08) { if (cursorCol > 0) memory[BDA_BASE + 0x50] = cursorCol - 1; }
      else if (ch === 0x07) { /* BEL */ }
      else {
        memory[0xB8000 + (cursorRow * cols + cursorCol) * 2] = ch;
        memory[0xB8000 + (cursorRow * cols + cursorCol) * 2 + 1] = 0x07;
        let newCol = cursorCol + 1;
        if (newCol >= cols) { newCol = 0; if (cursorRow < 24) memory[BDA_BASE + 0x51] = cursorRow + 1; else scrollUp(cols, 0x07); }
        memory[BDA_BASE + 0x50] = newCol;
      }
      return true;
    }
    if (regs.ah === 0x02) { memory[BDA_BASE + 0x51] = regs.dh; memory[BDA_BASE + 0x50] = regs.dl; return true; }
    if (regs.ah === 0x03) { setRegs({ dh: memory[BDA_BASE + 0x51], dl: memory[BDA_BASE + 0x50], cx: 0 }); return true; }
    if (regs.ah === 0x0F) { setRegs({ al: memory[BDA_BASE + 0x49], ah: memory[BDA_BASE + 0x4A], bh: 0 }); return true; }
    if (regs.ah === 0x00) {
      memory[BDA_BASE + 0x49] = regs.al;
      memory[BDA_BASE + 0x4A] = (regs.al === 0x13) ? 40 : 80;
      memory[BDA_BASE + 0x50] = 0; memory[BDA_BASE + 0x51] = 0;
      if (regs.al === 0x13) { for (let i = 0; i < 64000; i++) memory[0xA0000 + i] = 0; }
      else { for (let i = 0; i < 4000; i += 2) { memory[0xB8000 + i] = 0x20; memory[0xB8000 + i + 1] = 0x07; } }
      return true;
    }
    if (regs.ah === 0x06) {
      const lines = regs.al || 25; const attr = regs.bh;
      const top = (regs.cx >> 8) & 0xFF, left = regs.cx & 0xFF;
      const bottom = (regs.dx >> 8) & 0xFF, right = regs.dx & 0xFF;
      for (let r = top; r <= bottom; r++) {
        const srcRow = r + lines;
        for (let c = left; c <= right; c++) {
          const dstOff = (r * 80 + c) * 2;
          if (srcRow <= bottom) { const srcOff = (srcRow * 80 + c) * 2; memory[0xB8000 + dstOff] = memory[0xB8000 + srcOff]; memory[0xB8000 + dstOff + 1] = memory[0xB8000 + srcOff + 1]; }
          else { memory[0xB8000 + dstOff] = 0x20; memory[0xB8000 + dstOff + 1] = attr; }
        }
      }
      return true;
    }
    return true;
  }

  function scrollUp(cols, attr) {
    for (let r = 1; r <= 24; r++) {
      for (let c = 0; c < cols; c++) {
        const s = (r * cols + c) * 2, d = ((r - 1) * cols + c) * 2;
        memory[0xB8000 + d] = memory[0xB8000 + s];
        memory[0xB8000 + d + 1] = memory[0xB8000 + s + 1];
      }
    }
    for (let c = 0; c < cols; c++) { memory[0xB8000 + (24 * cols + c) * 2] = 0x20; memory[0xB8000 + (24 * cols + c) * 2 + 1] = attr; }
  }

  function int1ah() {
    const regs = getRegs();
    if (regs.ah === 0x00) { setRegs({ cx: 0, dx: 0, al: 0 }); return true; }
    if (regs.ah === 0x02) { setRegs({ ch: 0, cl: 0, dh: 0, dl: 0, flags: regs.flags & ~1 }); return true; }
    if (regs.ah === 0x04) { setRegs({ ch: 0x20, cl: 0x25, dh: 0x01, dl: 0x01, flags: regs.flags & ~1 }); return true; }
    return true;
  }

  function int20h() { memory[0x0504] = 1; const regs = getRegs(); setRegs({ ip: regs.ip - 2 }); return true; }

  function int08h() {
    const lo = memory[BDA_BASE + 0x6C] | (memory[BDA_BASE + 0x6D] << 8);
    const hi = memory[BDA_BASE + 0x6E] | (memory[BDA_BASE + 0x6F] << 8);
    let newLo = (lo + 1) & 0xFFFF, newHi = hi;
    if (newLo === 0) newHi = (newHi + 1) & 0xFFFF;
    memory[BDA_BASE + 0x6C] = newLo & 0xFF; memory[BDA_BASE + 0x6D] = (newLo >> 8) & 0xFF;
    memory[BDA_BASE + 0x6E] = newHi & 0xFF; memory[BDA_BASE + 0x6F] = (newHi >> 8) & 0xFF;
    pic.portOut(0, 0x20, 0x20);
    return true;
  }

  function int11h() { const v = memory[BDA_BASE + 0x10] | (memory[BDA_BASE + 0x11] << 8); setRegs({ al: v & 0xFF, ah: (v >> 8) & 0xFF }); return true; }
  function int12h() { const v = memory[BDA_BASE + 0x13] | (memory[BDA_BASE + 0x14] << 8); setRegs({ al: v & 0xFF, ah: (v >> 8) & 0xFF }); return true; }

  function int13h() {
    const regs = getRegs();
    const dl = regs.dl !== undefined ? regs.dl : (regs.dx & 0xFF);
    // Hard disk (DL >= 0x80): floppy-only machine, return clean "not present" (see issue #17)
    if (dl >= 0x80) {
      if (regs.ah === 0x08) { setRegs({ ah: 0x00, dl: 0, flags: regs.flags | 1 }); return true; }
      setRegs({ ah: 0x01, flags: regs.flags | 1 });
      return true;
    }
    if (regs.ah === 0x00) { setRegs({ ah: 0, flags: regs.flags & ~1 }); return true; }
    if (regs.ah === 0x02) {
      const count = regs.al, cyl = regs.ch, sector = regs.cl, head = regs.dh;
      const lba = (cyl * 2 + head) * 18 + (sector - 1);
      const srcBase = 0xD0000 + lba * 512;
      const dstBase = regs.es * 16 + (regs.bh * 256 + regs.bl);
      for (let i = 0; i < count * 512; i++) memory[dstBase + i] = memory[srcBase + i] || 0;
      setRegs({ ah: 0, al: count, flags: regs.flags & ~1 });
      return true;
    }
    if (regs.ah === 0x08) { setRegs({ ah: 0, bl: 0x04, ch: 79, cl: 18, dh: 1, dl: 1, flags: regs.flags & ~1 }); return true; }
    if (regs.ah === 0x15) { setRegs({ ah: 0x01, flags: regs.flags & ~1 }); return true; }
    setRegs({ ah: 0x01, flags: regs.flags | 1 });
    return true;
  }

  function int15h() {
    const regs = getRegs();
    if (regs.ah === 0x4F) return true;
    if (regs.ah === 0x88) { setRegs({ al: 0, ah: 0, flags: regs.flags & ~1 }); return true; }
    if (regs.ah === 0x90 || regs.ah === 0x91) { setRegs({ ah: 0 }); return true; }
    if (regs.ah === 0xC0) { setRegs({ ah: 0, flags: regs.flags & ~1 }); return true; }
    setRegs({ ah: 0x86, flags: regs.flags | 1 });
    return true;
  }

  function int19h() { memory[0x0504] = 1; const regs = getRegs(); setRegs({ ip: regs.ip - 2 }); return true; }

  return function(type) {
    switch (type) {
      case 0x08: return int08h();
      case 0x09: return int09h();
      case 0x10: return int10h();
      case 0x11: return int11h();
      case 0x12: return int12h();
      case 0x13: return int13h();
      case 0x15: return int15h();
      case 0x16: return int16h();
      case 0x19: return int19h();
      case 0x1A: return int1ah();
      case 0x20: return int20h();
      default: return true;
    }
  };
}

// ── Default VGA 256-color palette (standard Mode 13h) ──
const defaultVgaPalette = (() => {
  const p = new Array(256);
  // Standard 16 CGA colors
  const cga = [
    [0,0,0],[0,0,170],[0,170,0],[0,170,170],[170,0,0],[170,0,170],[170,85,0],[170,170,170],
    [85,85,85],[85,85,255],[85,255,85],[85,255,255],[255,85,85],[255,85,255],[255,255,85],[255,255,255],
  ];
  for (let i = 0; i < 16; i++) p[i] = cga[i];
  // 6x6x6 color cube (indices 16-231)
  for (let r = 0; r < 6; r++) for (let g = 0; g < 6; g++) for (let b = 0; b < 6; b++) {
    p[16 + r * 36 + g * 6 + b] = [r * 51, g * 51, b * 51];
  }
  // Grayscale ramp (232-255)
  for (let i = 0; i < 24; i++) { const v = 8 + i * 10; p[232 + i] = [v, v, v]; }
  return p;
})();
