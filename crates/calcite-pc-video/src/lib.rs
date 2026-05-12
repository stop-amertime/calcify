//! PC-specific video helpers over `calcite_core::State`.
//!
//! These functions know about IBM PC video conventions: the CGA palette,
//! the VGA DAC at a fixed linear address, two-byte char+attribute text-mode
//! cells, and the CP437 character set. None of this is calcite-engine
//! knowledge — it lives here so calcite-core stays cabinet-agnostic.
//!
//! Other cabinets (6502, brainfuck, anything not modelling an IBM PC)
//! simply do not link this crate.

use calcite_core::state::State;

/// 16-color CGA palette used by [`render_framebuffer`] and the text-mode
/// renderers when no DAC palette is present.
///
/// CSS-DOS shadows the VGA DAC palette (768 bytes: 256 entries × RGB) at
/// the linear address [`VGA_DAC_LINEAR`]. OUT 0x3C9 writes populate it via
/// the kiln port-decode machinery (kiln/patterns/misc.mjs). Values are
/// 6-bit (0..63), matching real VGA hardware; [`read_framebuffer_rgba`]
/// expands them to 8-bit when producing canvas pixels. Keep in sync with
/// `DAC_LINEAR` in CSS-DOS's kiln/memory.mjs.
pub const VGA_DAC_LINEAR: i32 = 0x100000;

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

/// Render text-mode video memory as a string.
///
/// Reads `width * height` character cells from `base_addr` in text-mode
/// format (2 bytes per cell: character byte + attribute byte). Returns
/// the screen contents as a string with newline-separated rows.
///
/// For DOS text mode at 0xB8000, call:
/// `render_screen(&state, 0xB8000, 40, 25)`
pub fn render_screen(state: &State, base_addr: usize, width: usize, height: usize) -> String {
    let mut lines = Vec::with_capacity(height);
    for y in 0..height {
        let mut row = String::with_capacity(width);
        for x in 0..width {
            let addr = base_addr + (y * width + x) * 2;
            let ch = if addr < state.memory.len() {
                state.memory[addr]
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
/// Reads char+attribute pairs; the attribute byte is `BBBFFFFF` where the
/// low nibble is foreground (0-15) and the high nibble's low 3 bits are
/// background (0-7). Bit 7 is treated as the bright-background (no blink).
/// Only emits a color-change escape when the attribute changes, so the
/// output stays compact.
pub fn render_screen_ansi(state: &State, base_addr: usize, width: usize, height: usize) -> String {
    let mut out = String::with_capacity(width * height * 4);
    let mut last_attr: Option<u8> = None;
    for y in 0..height {
        for x in 0..width {
            let addr = base_addr + (y * width + x) * 2;
            let (ch, attr) = if addr + 1 < state.memory.len() {
                (state.memory[addr], state.memory[addr + 1])
            } else {
                (b' ', 0x07)
            };
            if last_attr != Some(attr) {
                let fg_idx = (attr & 0x0F) as usize;
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
/// Same attribute decoding as [`render_screen_ansi`]; emits a
/// `<span style="color:#rrggbb;background:#rrggbb">` run whenever the
/// attribute changes. Characters are CP437→Unicode translated, with
/// `<`, `>`, `&` HTML-escaped. Rows are separated by `<br>`.
pub fn render_screen_html(state: &State, base_addr: usize, width: usize, height: usize) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(width * height * 8);
    let mut last_attr: Option<u8> = None;
    for y in 0..height {
        for x in 0..width {
            let addr = base_addr + (y * width + x) * 2;
            let (ch, attr) = if addr + 1 < state.memory.len() {
                (state.memory[addr], state.memory[addr + 1])
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
/// `render_framebuffer(&state, 0xA0000, 320, 200)`
pub fn render_framebuffer(state: &State, base_addr: usize, width: usize, height: usize) -> Vec<u8> {
    let header = format!("P6\n{} {}\n255\n", width, height);
    let mut out = Vec::with_capacity(header.len() + width * height * 3);
    out.extend_from_slice(header.as_bytes());
    for i in 0..(width * height) {
        let addr = base_addr + i;
        let byte = if addr < state.memory.len() {
            state.memory[addr]
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
    state: &State,
    base_addr: usize,
    width: usize,
    height: usize,
) -> Vec<u8> {
    // Snapshot the VGA DAC palette once per frame. CSS-DOS shadows OUT
    // 0x3C9 writes to 768 bytes starting at linear 0x100000, stored as
    // 6-bit values (0..63) just like real VGA hardware. If the cabinet
    // doesn't declare a DAC zone (old builds, hack carts without gfx
    // pruning), every byte comes back 0 (all-black palette); we fall
    // back to the hardcoded CGA palette in that case so existing
    // cabinets don't regress.
    let mut dac = [0u8; 768];
    let mut dac_populated = false;
    for i in 0..768 {
        let v = state
            .extended
            .get(&(VGA_DAC_LINEAR + i as i32))
            .copied()
            .unwrap_or(0);
        if v != 0 {
            dac_populated = true;
        }
        dac[i] = (v & 0x3F) as u8;
    }

    let mut out = vec![0u8; width * height * 4];
    for i in 0..(width * height) {
        let addr = base_addr + i;
        let byte = if addr < state.memory.len() {
            state.memory[addr]
        } else {
            0
        };
        let (r, g, b) = if dac_populated {
            let idx = byte as usize * 3;
            let r6 = dac[idx];
            let g6 = dac[idx + 1];
            let b6 = dac[idx + 2];
            (
                (r6 << 2) | (r6 >> 4),
                (g6 << 2) | (g6 >> 4),
                (b6 << 2) | (b6 >> 4),
            )
        } else {
            CGA_PALETTE[(byte as usize) & 0x0F]
        };
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
pub fn read_video_memory(state: &State, base_addr: usize, width: usize, height: usize) -> Vec<u8> {
    let mut buf = vec![0u8; width * height];
    for y in 0..height {
        for x in 0..width {
            let addr = base_addr + (y * width + x) * 2;
            buf[y * width + x] = if addr < state.memory.len() {
                state.memory[addr]
            } else {
                0
            };
        }
    }
    buf
}

/// Map a CP437 byte to a Unicode character for terminal rendering.
pub fn cp437_to_unicode(b: u8) -> char {
    const LOW: [char; 32] = [
        ' ', '\u{263A}', '\u{263B}', '\u{2665}', '\u{2666}', '\u{2663}', '\u{2660}', '\u{2022}',
        '\u{25D8}', '\u{25CB}', '\u{25D9}', '\u{2642}', '\u{2640}', '\u{266A}', '\u{266B}', '\u{263C}',
        '\u{25BA}', '\u{25C4}', '\u{2195}', '\u{203C}', '\u{00B6}', '\u{00A7}', '\u{25AC}', '\u{21A8}',
        '\u{2191}', '\u{2193}', '\u{2192}', '\u{2190}', '\u{221F}', '\u{2194}', '\u{25B2}', '\u{25BC}',
    ];
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
