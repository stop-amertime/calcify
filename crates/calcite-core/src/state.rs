//! Emulator state — flat representation of the CSS machine.
//!
//! State variables are discovered from CSS `@property` declarations at load time.
//! Calcite has no hardcoded knowledge of what those variables represent — they
//! could be x86 registers, halt flags, micro-op counters, or anything else the
//! CSS program defines. The negative address convention (slot 0 → -1, slot 1 → -2,
//! etc.) simply separates state variables from memory bytes in the unified
//! address space.

/// Default memory size for x86CSS (0x600 bytes = 1,536).
pub const DEFAULT_MEM_SIZE: usize = 0x600;

/// 16-color CGA palette used by [`State::render_framebuffer`].
///
/// Standard IRGB CGA colors: the low 8 entries are the dark variants,
/// the high 8 are the bright variants. These also form the first 16
/// entries of the full VGA palette, so a future 256-entry upgrade is
/// backward-compatible with programs that only use indices 0-15.
pub const CGA_PALETTE: [(u8, u8, u8); 16] = [
    (0x00, 0x00, 0x00), // 0  black
    (0x00, 0x00, 0xAA), // 1  blue
    (0x00, 0xAA, 0x00), // 2  green
    (0x00, 0xAA, 0xAA), // 3  cyan
    (0xAA, 0x00, 0x00), // 4  red
    (0xAA, 0x00, 0xAA), // 5  magenta
    (0xAA, 0x55, 0x00), // 6  brown
    (0xAA, 0xAA, 0xAA), // 7  light gray
    (0x55, 0x55, 0x55), // 8  dark gray
    (0x55, 0x55, 0xFF), // 9  light blue
    (0x55, 0xFF, 0x55), // 10 light green
    (0x55, 0xFF, 0xFF), // 11 light cyan
    (0xFF, 0x55, 0x55), // 12 light red
    (0xFF, 0x55, 0xFF), // 13 light magenta
    (0xFF, 0xFF, 0x55), // 14 yellow
    (0xFF, 0xFF, 0xFF), // 15 white
];

/// The flat machine state that replaces CSS's triple-buffered custom properties.
#[derive(Debug, Clone)]
pub struct State {
    /// State variables discovered from CSS `@property` declarations.
    /// Addressed via negative numbers: slot 0 → addr -1, slot 1 → addr -2, etc.
    pub state_vars: Vec<i32>,
    /// State variable names, in slot order (for display/debugging).
    pub state_var_names: Vec<String>,
    /// Name → slot index lookup.
    pub(crate) state_var_index: std::collections::HashMap<String, usize>,
    /// Flat memory (byte-addressable, default 1,536 bytes).
    pub memory: Vec<u8>,
    /// Extended properties: full-width i32 storage for addresses above the byte
    /// memory range (e.g., file I/O counters at 0xFFFF0+). Reads/writes bypass
    /// the u8 truncation of the memory array.
    pub extended: std::collections::HashMap<i32, i32>,
    /// String property values (e.g., `--textBuffer` for text output).
    pub string_properties: std::collections::HashMap<String, String>,
    /// Tick counter (incremented each evaluation cycle).
    pub frame_counter: u32,
    /// Optional per-tick read log. When the inner `Option` is `Some`, every
    /// `read_mem`/`read_mem16` call appends `(addr, value)`. `None` = no
    /// overhead beyond a single `is_some()` check. Interior mutability so
    /// `read_mem(&self)` can log without changing its signature.
    pub read_log: std::cell::RefCell<Option<Vec<(i32, i32)>>>,
    /// Optional per-tick write log. When `Some`, every `write_mem` call
    /// appends `(addr, value)`. Used by the memoisation-viability probe.
    pub write_log: Option<Vec<(i32, i32)>>,
}

impl State {
    /// Create a new state with the given memory size.
    pub fn new(mem_size: usize) -> Self {
        Self {
            state_vars: Vec::new(),
            state_var_names: Vec::new(),
            state_var_index: std::collections::HashMap::new(),
            memory: vec![0; mem_size],
            extended: std::collections::HashMap::new(),
            string_properties: std::collections::HashMap::new(),
            frame_counter: 0,
            read_log: std::cell::RefCell::new(None),
            write_log: None,
        }
    }

    /// Apply a delta forward — overwrites channels with `new_*` values.
    /// Panics in debug if slot/addr is out of range (caller must apply
    /// deltas against the same State shape that produced them).
    pub fn apply_delta(&mut self, d: &StateDelta) {
        for (slot, _, new) in &d.state_vars {
            self.state_vars[*slot as usize] = *new;
        }
        for (addr, _, new) in &d.memory {
            self.memory[*addr as usize] = *new;
        }
        for (addr, _, new) in &d.extended {
            match new {
                Some(v) => {
                    self.extended.insert(*addr, *v);
                }
                None => {
                    self.extended.remove(addr);
                }
            }
        }
        for (name, _, new) in &d.string_properties {
            match new {
                Some(v) => {
                    self.string_properties.insert(name.clone(), v.clone());
                }
                None => {
                    self.string_properties.remove(name);
                }
            }
        }
        self.frame_counter = d.frame_counter_after;
    }

    /// Apply a delta in reverse — overwrites channels with `old_*` values.
    /// This is what makes reverse seek possible without re-execution.
    pub fn revert_delta(&mut self, d: &StateDelta) {
        for (slot, old, _) in &d.state_vars {
            self.state_vars[*slot as usize] = *old;
        }
        for (addr, old, _) in &d.memory {
            self.memory[*addr as usize] = *old;
        }
        for (addr, old, _) in &d.extended {
            match old {
                Some(v) => {
                    self.extended.insert(*addr, *v);
                }
                None => {
                    self.extended.remove(addr);
                }
            }
        }
        for (name, old, _) in &d.string_properties {
            match old {
                Some(v) => {
                    self.string_properties.insert(name.clone(), v.clone());
                }
                None => {
                    self.string_properties.remove(name);
                }
            }
        }
        self.frame_counter = d.frame_counter_before;
    }

    /// Look up a state variable by name. Returns None if not found.
    pub fn get_var(&self, name: &str) -> Option<i32> {
        self.state_var_index.get(name).map(|&i| self.state_vars[i])
    }

    /// Set a state variable by name. Returns false if not found.
    pub fn set_var(&mut self, name: &str, value: i32) -> bool {
        if let Some(&i) = self.state_var_index.get(name) {
            self.state_vars[i] = value;
            true
        } else {
            false
        }
    }

    /// Look up the slot index for a state variable name.
    pub fn var_slot(&self, name: &str) -> Option<usize> {
        self.state_var_index.get(name).copied()
    }

    /// Number of state variables.
    pub fn state_var_count(&self) -> usize {
        self.state_vars.len()
    }

    /// Read from the unified address space.
    ///
    /// Address conventions:
    /// - Negative: state variables (slot index = -addr - 1)
    /// - `0..`: memory bytes
    pub fn read_mem(&self, addr: i32) -> i32 {
        let v = if addr < 0 {
            let idx = (-addr - 1) as usize;
            if idx < self.state_vars.len() {
                self.state_vars[idx]
            } else {
                0
            }
        } else {
            let addr_u = addr as usize;
            if addr_u >= 0xF0000 {
                self.extended.get(&addr).copied().unwrap_or(0)
            } else if addr_u < self.memory.len() {
                self.memory[addr_u] as i32
            } else {
                0
            }
        };
        // Optional read log for memoisation-viability probing.
        if let Ok(mut borrow) = self.read_log.try_borrow_mut() {
            if let Some(ref mut log) = *borrow {
                log.push((addr, v));
            }
        }
        v
    }

    /// Read a 16-bit little-endian word from memory.
    pub fn read_mem16(&self, addr: i32) -> i32 {
        let lo = self.read_mem(addr);
        let hi = self.read_mem(addr + 1);
        lo + hi * 256
    }

    /// Write a value to the unified address space.
    pub fn write_mem(&mut self, addr: i32, value: i32) {
        if let Some(ref mut log) = self.write_log {
            log.push((addr, value));
        }
        if addr < 0 {
            let idx = (-addr - 1) as usize;
            if idx < self.state_vars.len() {
                self.state_vars[idx] = value;
            }
        } else {
            let addr_u = addr as usize;
            if addr_u >= 0xF0000 {
                self.extended.insert(addr, value);
                return;
            }
            if addr_u < self.memory.len() {
                self.memory[addr_u] = (value & 0xFF) as u8;
            }
        }
    }

    /// Push a key into the BDA keyboard ring buffer.
    ///
    /// `key` is packed as `(scancode << 8) | ascii`, matching the IBM PC
    /// BIOS convention. This is what the 8042 keyboard controller / INT 9
    /// handler does on real hardware.
    ///
    /// The buffer lives at segment 0x40 (linear 0x400 + BDA offset):
    /// - 0x41A: head pointer (offset into buffer)
    /// - 0x41C: tail pointer
    /// - 0x480: buffer start offset
    /// - 0x482: buffer end offset
    ///
    /// If the buffer is full the key is silently dropped (matching real hardware).
    pub fn bda_push_key(&mut self, key: i32) {
        let tail      = self.read_mem16(0x41C);
        let buf_end   = self.read_mem16(0x482);
        let buf_start = self.read_mem16(0x480);
        let linear    = 0x400 + tail;
        self.write_mem(linear,     key & 0xFF);
        self.write_mem(linear + 1, (key >> 8) & 0xFF);
        let mut new_tail = tail + 2;
        if new_tail >= buf_end { new_tail = buf_start; }
        let head = self.read_mem16(0x41A);
        if new_tail != head {
            // Not full — advance tail.
            self.write_mem(0x41C, new_tail & 0xFF);
            self.write_mem(0x41D, (new_tail >> 8) & 0xFF);
        }
    }

    /// Get the low byte of a 16-bit register value.
    pub fn lo8(value: i32) -> i32 {
        value & 0xFF
    }

    /// Get the high byte of a 16-bit register value.
    pub fn hi8(value: i32) -> i32 {
        (value >> 8) & 0xFF
    }

    /// Render text-mode video memory as a string.
    ///
    /// Reads `width * height` character cells from `base_addr` in text-mode
    /// format (2 bytes per cell: character byte + attribute byte). Returns
    /// the screen contents as a string with newline-separated rows.
    ///
    /// For DOS text mode at 0xB8000, call:
    /// `state.render_screen(0xB8000, 40, 25)`
    pub fn render_screen(&self, base_addr: usize, width: usize, height: usize) -> String {
        let mut lines = Vec::with_capacity(height);
        for y in 0..height {
            let mut row = String::with_capacity(width);
            for x in 0..width {
                let addr = base_addr + (y * width + x) * 2;
                let ch = if addr < self.memory.len() {
                    self.memory[addr]
                } else {
                    b' '
                };
                row.push(cp437_to_unicode(ch));
            }
            lines.push(row);
        }
        lines.join("\n")
    }

    /// Render text-mode video memory with ANSI 24-bit color escapes.
    ///
    /// Reads char+attribute pairs; the attribute byte is
    /// `BBBFFFFF` where the low nibble is foreground (0-15) and the high
    /// nibble's low 3 bits are background (0-7). Bit 7 is treated as the
    /// bright-background (no blink). Only emits a color-change escape when
    /// the attribute changes, so the output stays compact.
    pub fn render_screen_ansi(&self, base_addr: usize, width: usize, height: usize) -> String {
        let mut out = String::with_capacity(width * height * 4);
        let mut last_attr: Option<u8> = None;
        for y in 0..height {
            for x in 0..width {
                let addr = base_addr + (y * width + x) * 2;
                let (ch, attr) = if addr + 1 < self.memory.len() {
                    (self.memory[addr], self.memory[addr + 1])
                } else {
                    (b' ', 0x07)
                };
                if last_attr != Some(attr) {
                    let fg_idx = (attr & 0x0F) as usize;
                    // High bit often means "blink"; most programs really want
                    // bright BG, so treat bits 4-7 as a 4-bit BG index.
                    let bg_idx = ((attr >> 4) & 0x0F) as usize;
                    let (fr, fg, fb) = CGA_PALETTE[fg_idx];
                    let (br, bg_, bb) = CGA_PALETTE[bg_idx];
                    use std::fmt::Write;
                    let _ = write!(
                        out,
                        "\x1b[38;2;{};{};{};48;2;{};{};{}m",
                        fr, fg, fb, br, bg_, bb
                    );
                    last_attr = Some(attr);
                }
                out.push(cp437_to_unicode(ch));
            }
            // Reset before newline so the trailing background doesn't bleed
            // across the whole terminal line.
            out.push_str("\x1b[0m");
            last_attr = None;
            if y + 1 < height {
                out.push('\n');
            }
        }
        out
    }

    /// Render text-mode video memory as HTML with inline color spans.
    ///
    /// Same attribute decoding as [`Self::render_screen_ansi`]; emits a
    /// `<span style="color:#rrggbb;background:#rrggbb">` run whenever the
    /// attribute changes. Characters are CP437→Unicode translated, with
    /// `<`, `>`, `&` HTML-escaped. Rows are separated by `<br>`.
    pub fn render_screen_html(&self, base_addr: usize, width: usize, height: usize) -> String {
        use std::fmt::Write;
        let mut out = String::with_capacity(width * height * 8);
        let mut last_attr: Option<u8> = None;
        for y in 0..height {
            for x in 0..width {
                let addr = base_addr + (y * width + x) * 2;
                let (ch, attr) = if addr + 1 < self.memory.len() {
                    (self.memory[addr], self.memory[addr + 1])
                } else {
                    (b' ', 0x07)
                };
                if last_attr != Some(attr) {
                    if last_attr.is_some() {
                        out.push_str("</span>");
                    }
                    let fg_idx = (attr & 0x0F) as usize;
                    let bg_idx = ((attr >> 4) & 0x0F) as usize;
                    let (fr, fg, fb) = CGA_PALETTE[fg_idx];
                    let (br, bg_, bb) = CGA_PALETTE[bg_idx];
                    let _ = write!(
                        out,
                        "<span style=\"color:#{:02x}{:02x}{:02x};background:#{:02x}{:02x}{:02x}\">",
                        fr, fg, fb, br, bg_, bb
                    );
                    last_attr = Some(attr);
                }
                let c = cp437_to_unicode(ch);
                match c {
                    '<' => out.push_str("&lt;"),
                    '>' => out.push_str("&gt;"),
                    '&' => out.push_str("&amp;"),
                    _ => out.push(c),
                }
            }
            if last_attr.is_some() {
                out.push_str("</span>");
                last_attr = None;
            }
            if y + 1 < height {
                out.push_str("<br>");
            }
        }
        out
    }

    /// Render a graphics-mode framebuffer as a PPM P6 image.
    ///
    /// Reads `width * height` bytes from `base_addr` where each byte is a
    /// palette index (byte-per-pixel, like VGA Mode 13h). Indices are looked
    /// up in [`CGA_PALETTE`] modulo 16 (palette is 16 colors for now; upgrade
    /// to a 256-entry VGA palette is a drop-in change). Returns a complete
    /// PPM P6 byte buffer that can be written to a file.
    ///
    /// For Mode 13h at 0xA0000, call:
    /// `state.render_framebuffer(0xA0000, 320, 200)`
    pub fn render_framebuffer(&self, base_addr: usize, width: usize, height: usize) -> Vec<u8> {
        let header = format!("P6\n{} {}\n255\n", width, height);
        let mut out = Vec::with_capacity(header.len() + width * height * 3);
        out.extend_from_slice(header.as_bytes());
        for i in 0..(width * height) {
            let addr = base_addr + i;
            let byte = if addr < self.memory.len() {
                self.memory[addr]
            } else {
                0
            };
            let (r, g, b) = CGA_PALETTE[(byte as usize) & 0x0F];
            out.push(r);
            out.push(g);
            out.push(b);
        }
        out
    }

    /// Read a graphics-mode framebuffer as raw RGBA bytes for canvas blit.
    ///
    /// Returns `width * height * 4` bytes (R, G, B, A per pixel) suitable
    /// for `new ImageData(new Uint8ClampedArray(bytes), width, height)`
    /// in the browser. Alpha is always 255.
    ///
    /// This is the same pixel data as [`render_framebuffer`] but without
    /// the PPM header and with an alpha channel, so the browser can stuff
    /// it straight into a canvas without reformatting.
    pub fn read_framebuffer_rgba(
        &self,
        base_addr: usize,
        width: usize,
        height: usize,
    ) -> Vec<u8> {
        let mut out = vec![0u8; width * height * 4];
        for i in 0..(width * height) {
            let addr = base_addr + i;
            let byte = if addr < self.memory.len() {
                self.memory[addr]
            } else {
                0
            };
            let (r, g, b) = CGA_PALETTE[(byte as usize) & 0x0F];
            let o = i * 4;
            out[o] = r;
            out[o + 1] = g;
            out[o + 2] = b;
            out[o + 3] = 255;
        }
        out
    }

    /// Read raw video memory bytes (character bytes only, no attributes).
    ///
    /// Returns `width * height` bytes from text-mode video memory.
    /// Useful for WASM/browser rendering where JS handles display.
    pub fn read_video_memory(&self, base_addr: usize, width: usize, height: usize) -> Vec<u8> {
        let mut buf = vec![0u8; width * height];
        for y in 0..height {
            for x in 0..width {
                let addr = base_addr + (y * width + x) * 2;
                buf[y * width + x] = if addr < self.memory.len() {
                    self.memory[addr]
                } else {
                    0
                };
            }
        }
        buf
    }

    /// Discover state variables and load initial values from `@property` declarations.
    ///
    /// Any `@property --X` where X is not `m{digits}` and not `clock` is a state
    /// variable. State variables get auto-assigned negative addresses: the first
    /// one gets -1, the second -2, etc. Memory bytes (`--m{N}`) are stored in
    /// the flat byte array at address N.
    ///
    /// This method populates the state var map AND installs the address map into
    /// the eval module's thread-local so that `property_to_address()` works for
    /// all subsequent operations.
    pub fn load_properties(&mut self, properties: &[crate::types::PropertyDef]) {
        use crate::types::CssValue;

        // Classify properties: state vars vs memory vs other
        let mut state_var_defs: Vec<(&str, i32)> = Vec::new(); // (bare_name, initial_value)
        let mut memory_defs: Vec<(usize, u8)> = Vec::new();    // (addr, value)
        let mut max_mem_addr: usize = 0;

        for prop in properties {
            let value = match &prop.initial_value {
                Some(CssValue::Integer(v)) => *v as i32,
                _ => 0,
            };

            // Strip leading "--"
            let bare = if prop.name.starts_with("--") {
                &prop.name[2..]
            } else {
                &prop.name
            };

            // Skip clock — it's animation plumbing, not state
            if bare == "clock" {
                continue;
            }

            // Check if it's a memory byte: m{digits}
            if let Some(rest) = bare.strip_prefix('m') {
                if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
                    if let Ok(addr) = rest.parse::<usize>() {
                        if addr < 0xF0000 {
                            max_mem_addr = max_mem_addr.max(addr + 1);
                        }
                        memory_defs.push((addr, (value & 0xFF) as u8));
                        continue;
                    }
                }
            }

            // It's a state variable
            state_var_defs.push((bare, value));
        }

        // Set up state vars
        self.state_vars.clear();
        self.state_var_names.clear();
        self.state_var_index.clear();

        let mut address_map = std::collections::HashMap::new();
        for (i, &(name, init)) in state_var_defs.iter().enumerate() {
            let addr = -(i as i32 + 1);
            self.state_vars.push(init);
            self.state_var_names.push(name.to_string());
            self.state_var_index.insert(name.to_string(), i);
            address_map.insert(name.to_string(), addr);
        }

        log::info!(
            "Discovered {} state variables from CSS: {:?}",
            self.state_vars.len(),
            self.state_var_names,
        );

        // Install the state var address map so property_to_address() works.
        // This REPLACES any existing map — call load_properties BEFORE
        // Evaluator::from_parsed() so that dispatch-table entries are merged
        // on top of state var entries (not the other way around).
        super::eval::set_address_map(address_map);

        // Set up memory
        if max_mem_addr > self.memory.len() {
            log::info!("Auto-sizing memory: {} → {} bytes", self.memory.len(), max_mem_addr);
            self.memory.resize(max_mem_addr, 0);
        }
        for (addr, value) in &memory_defs {
            if *addr >= 0xF0000 {
                self.extended.insert(*addr as i32, *value as i32);
            } else if *addr < self.memory.len() {
                self.memory[*addr] = *value;
            }
        }
    }
}

impl Default for State {
    fn default() -> Self {
        Self::new(DEFAULT_MEM_SIZE)
    }
}

/// Map a CP437 byte to a Unicode character for terminal rendering.
fn cp437_to_unicode(b: u8) -> char {
    // CP437 low control range (0x00-0x1F) and 0x7F mapped to common glyphs
    const LOW: [char; 32] = [
        ' ', '\u{263A}', '\u{263B}', '\u{2665}', '\u{2666}', '\u{2663}', '\u{2660}', '\u{2022}',
        '\u{25D8}', '\u{25CB}', '\u{25D9}', '\u{2642}', '\u{2640}', '\u{266A}', '\u{266B}', '\u{263C}',
        '\u{25BA}', '\u{25C4}', '\u{2195}', '\u{203C}', '\u{00B6}', '\u{00A7}', '\u{25AC}', '\u{21A8}',
        '\u{2191}', '\u{2193}', '\u{2192}', '\u{2190}', '\u{221F}', '\u{2194}', '\u{25B2}', '\u{25BC}',
    ];
    // CP437 high range (0x80-0xFF)
    const HIGH: [char; 128] = [
        '\u{00C7}', '\u{00FC}', '\u{00E9}', '\u{00E2}', '\u{00E4}', '\u{00E0}', '\u{00E5}', '\u{00E7}',
        '\u{00EA}', '\u{00EB}', '\u{00E8}', '\u{00EF}', '\u{00EE}', '\u{00EC}', '\u{00C4}', '\u{00C5}',
        '\u{00C9}', '\u{00E6}', '\u{00C6}', '\u{00F4}', '\u{00F6}', '\u{00F2}', '\u{00FB}', '\u{00F9}',
        '\u{00FF}', '\u{00D6}', '\u{00DC}', '\u{00A2}', '\u{00A3}', '\u{00A5}', '\u{20A7}', '\u{0192}',
        '\u{00E1}', '\u{00ED}', '\u{00F3}', '\u{00FA}', '\u{00F1}', '\u{00D1}', '\u{00AA}', '\u{00BA}',
        '\u{00BF}', '\u{2310}', '\u{00AC}', '\u{00BD}', '\u{00BC}', '\u{00A1}', '\u{00AB}', '\u{00BB}',
        '\u{2591}', '\u{2592}', '\u{2593}', '\u{2502}', '\u{2524}', '\u{2561}', '\u{2562}', '\u{2556}',
        '\u{2555}', '\u{2563}', '\u{2551}', '\u{2557}', '\u{255D}', '\u{255C}', '\u{255B}', '\u{2510}',
        '\u{2514}', '\u{2534}', '\u{252C}', '\u{251C}', '\u{2500}', '\u{253C}', '\u{255E}', '\u{255F}',
        '\u{255A}', '\u{2554}', '\u{2569}', '\u{2566}', '\u{2560}', '\u{2550}', '\u{256C}', '\u{2567}',
        '\u{2568}', '\u{2564}', '\u{2565}', '\u{2559}', '\u{2558}', '\u{2552}', '\u{2553}', '\u{256B}',
        '\u{256A}', '\u{2518}', '\u{250C}', '\u{2588}', '\u{2584}', '\u{258C}', '\u{2590}', '\u{2580}',
        '\u{03B1}', '\u{00DF}', '\u{0393}', '\u{03C0}', '\u{03A3}', '\u{03C3}', '\u{00B5}', '\u{03C4}',
        '\u{03A6}', '\u{0398}', '\u{03A9}', '\u{03B4}', '\u{221E}', '\u{03C6}', '\u{03B5}', '\u{2229}',
        '\u{2261}', '\u{00B1}', '\u{2265}', '\u{2264}', '\u{2320}', '\u{2321}', '\u{00F7}', '\u{2248}',
        '\u{00B0}', '\u{2219}', '\u{00B7}', '\u{221A}', '\u{207F}', '\u{00B2}', '\u{25A0}', ' ',
    ];
    match b {
        0x00..=0x1F => LOW[b as usize],
        0x20..=0x7E => b as char,
        0x7F => '\u{2302}',
        _ => HIGH[(b - 0x80) as usize],
    }
}

/// A per-tick change record over [`State`]. Stores both the pre- and post-tick
/// value for every mutated channel so deltas can be replayed forward OR
/// backward without re-executing the tick.
///
/// Channels:
/// - `state_vars`: (slot_index, old, new) — state_var_names/state_var_index
///   are program-static and not recorded.
/// - `memory`: (byte_addr, old, new) — only for byte memory. Extended/string
///   channels handled separately.
/// - `extended`: (addr, old, new) where `None` means "entry absent".
/// - `string_properties`: (name, old, new) where `None` means "entry absent".
///
/// Empty-delta ticks are still produced (frame_counter always changes).
#[derive(Debug, Clone, Default)]
pub struct StateDelta {
    /// Tick at the START of this tick (what frame_counter equalled before tick).
    pub frame_counter_before: u32,
    /// Tick at the END of this tick (frame_counter_before + 1 in practice).
    pub frame_counter_after: u32,
    pub state_vars: Vec<(u32, i32, i32)>,
    pub memory: Vec<(u32, u8, u8)>,
    pub extended: Vec<(i32, Option<i32>, Option<i32>)>,
    pub string_properties: Vec<(String, Option<String>, Option<String>)>,
}

impl StateDelta {
    pub fn total_changes(&self) -> usize {
        self.state_vars.len()
            + self.memory.len()
            + self.extended.len()
            + self.string_properties.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a state with some state variables for testing.
    fn test_state() -> State {
        let mut state = State::new(DEFAULT_MEM_SIZE);
        // Simulate discovering state vars from CSS
        let names = vec!["AX", "CX", "DX", "BX"];
        for (i, name) in names.iter().enumerate() {
            state.state_vars.push(0);
            state.state_var_names.push(name.to_string());
            state.state_var_index.insert(name.to_string(), i);
        }
        state
    }

    #[test]
    fn state_var_addressing() {
        let mut state = test_state();
        // AX is slot 0 → address -1
        state.state_vars[0] = 0x1234;
        assert_eq!(state.read_mem(-1), 0x1234);

        state.write_mem(-1, 0xABCD);
        assert_eq!(state.state_vars[0], 0xABCD);
    }

    #[test]
    fn state_var_by_name() {
        let mut state = test_state();
        state.set_var("AX", 42);
        assert_eq!(state.get_var("AX"), Some(42));
        assert_eq!(state.get_var("nonexistent"), None);
    }

    #[test]
    fn memory_addressing() {
        let mut state = State::default();
        state.write_mem(0x100, 0x42);
        assert_eq!(state.read_mem(0x100), 0x42);
    }

    #[test]
    fn read16_little_endian() {
        let mut state = State::default();
        state.write_mem(0x10, 0x34); // lo
        state.write_mem(0x11, 0x12); // hi
        assert_eq!(state.read_mem16(0x10), 0x1234);
    }

    #[test]
    fn byte_extraction() {
        assert_eq!(State::lo8(0x1234), 0x34);
        assert_eq!(State::hi8(0x1234), 0x12);
    }

    #[test]
    fn out_of_range_state_var() {
        let state = test_state();
        // Address -100 is beyond the 4 state vars
        assert_eq!(state.read_mem(-100), 0);
    }

    #[test]
    fn state_var_stores_full_value() {
        let mut state = test_state();
        // No masking — state vars store whatever the CSS computes
        state.write_mem(-1, 0x1_ABCD);
        assert_eq!(state.state_vars[0], 0x1_ABCD);

        state.write_mem(-2, -2);
        assert_eq!(state.state_vars[1], -2);
    }
}
