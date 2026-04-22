//! WASM bindings for calc(ite).
//!
//! This crate compiles to a WASM module that runs inside a Web Worker.
//! The main thread sends CSS text, the worker parses/compiles/evaluates,
//! and sends back property diffs for DOM application.

use wasm_bindgen::prelude::*;

/// Initialise the WASM module (sets up logging, etc.).
#[wasm_bindgen(start)]
pub fn init() {
    std::panic::set_hook(Box::new(|info| {
        let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic".to_string()
        };
        let loc = info.location().map(|l| format!(" at {}:{}", l.file(), l.line())).unwrap_or_default();
        web_sys::console::error_1(&format!("WASM panic: {msg}{loc}").into());
    }));
    console_log::init_with_level(log::Level::Info).ok();
    log::info!("calc(ite) WASM module initialised");
}

/// The main engine handle exposed to JavaScript.
#[wasm_bindgen]
pub struct CalciteEngine {
    state: calcite_core::State,
    evaluator: calcite_core::Evaluator,
    // Initial property defs, cached at construction. reset() rebuilds
    // `state` from this without rebuilding `evaluator` (which would
    // require reparsing + recompiling the CSS — expensive for large
    // cabinets).
    initial_properties: Vec<calcite_core::types::PropertyDef>,
}

#[wasm_bindgen]
impl CalciteEngine {
    /// Create a new engine instance from CSS source as raw UTF-8 bytes.
    /// Use this for large files that exceed JS string limits.
    pub fn new_from_bytes(css_bytes: &[u8]) -> Result<CalciteEngine, JsError> {
        let css = std::str::from_utf8(css_bytes)
            .map_err(|e| JsError::new(&format!("Invalid UTF-8: {e}")))?;
        Self::new(css)
    }

    /// Create a new engine instance from CSS source text.
    #[wasm_bindgen(constructor)]
    pub fn new(css: &str) -> Result<CalciteEngine, JsError> {
        log::info!("Parsing {} bytes of CSS", css.len());

        let parsed =
            calcite_core::parser::parse_css(css).map_err(|e| JsError::new(&e.to_string()))?;

        log::info!(
            "Parsed: {} @property, {} @function, {} assignments",
            parsed.properties.len(),
            parsed.functions.len(),
            parsed.assignments.len(),
        );

        log::info!("Loading properties...");
        let mut state = calcite_core::State::default();
        state.load_properties(&parsed.properties);
        log::info!("Properties loaded, memory size: {} bytes", state.memory.len());

        log::info!("Creating evaluator...");
        let evaluator = calcite_core::Evaluator::from_parsed(&parsed);
        log::info!("Evaluator created");

        // Copy literal BIOS-byte entries out of the --readMem dispatch table
        // into state memory so read_mem() / debugger tooling can see ROM. Without
        // this, BIOS bytes exist only as CSS literal branches and read_mem()
        // returns 0 for every F0000+ address.
        if let Some(table) = evaluator.dispatch_tables.get("--readMem") {
            state.populate_memory_from_readmem(table);
        }

        let initial_properties = parsed.properties;
        Ok(CalciteEngine { state, evaluator, initial_properties })
    }

    /// Reset the engine's runtime state without recompiling the CSS.
    /// Equivalent to `new CalciteEngine(css)` but skips the parse +
    /// compile steps, which are the expensive ones for large cabinets.
    /// Used by the bridge worker to restart the machine on each
    /// viewer-connect without paying multi-second compile cost.
    pub fn reset(&mut self) {
        self.state = calcite_core::State::default();
        self.state.load_properties(&self.initial_properties);
        if let Some(table) = self.evaluator.dispatch_tables.get("--readMem") {
            self.state.populate_memory_from_readmem(table);
        }
    }

    /// Run a batch of ticks and return the property changes as a JSON string.
    ///
    /// Returns `[[name, value], ...]` pairs.
    pub fn tick_batch(&mut self, count: u32) -> Result<String, JsError> {
        let result = self.evaluator.run_batch(&mut self.state, count);

        // Serialize changes as JSON array of [name, value] pairs
        let json_parts: Vec<String> = result
            .changes
            .iter()
            .map(|(name, value)| format!("[\"{name}\",\"{value}\"]"))
            .collect();
        Ok(format!("[{}]", json_parts.join(",")))
    }

    /// Inject a keypress into the BDA ring buffer.
    /// Pass (scancode << 8 | ascii), matching the standard PC keyboard convention.
    /// The BIOS INT 16h handler will pop it when the program next reads keyboard input.
    pub fn set_keyboard(&mut self, key: i32) {
        self.state.bda_push_key(key);
    }

    /// Get the current value of a register (for debugging).
    pub fn get_register(&self, index: usize) -> i32 {
        if index < self.state.state_vars.len() {
            self.state.state_vars[index]
        } else {
            0
        }
    }

    /// Get a state variable by name (e.g. "cycleCount", "IP", "AX").
    /// Returns 0 if the variable doesn't exist.
    pub fn get_state_var(&self, name: &str) -> i32 {
        self.state.get_var(name).unwrap_or(0)
    }

    /// Read text-mode video memory (character bytes only).
    ///
    /// Returns `width * height` bytes from video memory at `base_addr`.
    /// Default for DOS text mode: `read_video_memory(0xB8000, 40, 25)`.
    pub fn read_video_memory(&self, base_addr: usize, width: usize, height: usize) -> Vec<u8> {
        self.state.read_video_memory(base_addr, width, height)
    }

    /// Read a contiguous byte range from memory. Returns `len` bytes starting
    /// at `base_addr`. Out-of-range reads return 0.
    ///
    /// Used by the browser renderer when it needs the raw VGA/CGA framebuffer
    /// bytes (char+attr pairs for text mode, 2-bpp packed scanlines for CGA
    /// mode 0x04). Calcite stays x86-ignorant: it just hands over the bytes,
    /// the caller decodes.
    pub fn read_memory_range(&self, base_addr: usize, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        for i in 0..len {
            let addr = base_addr + i;
            if addr < self.state.memory.len() {
                buf[i] = self.state.memory[addr];
            }
        }
        buf
    }

    /// Render text-mode video memory as a string (for debugging).
    pub fn render_screen(&self, base_addr: usize, width: usize, height: usize) -> String {
        self.state.render_screen(base_addr, width, height)
    }

    /// Render text-mode video memory as HTML with CGA color spans.
    pub fn render_screen_html(&self, base_addr: usize, width: usize, height: usize) -> String {
        self.state.render_screen_html(base_addr, width, height)
    }

    /// Render a graphics-mode framebuffer as a PPM P6 image.
    ///
    /// Each byte at `base_addr + i` is a palette index; the returned buffer
    /// is a complete PPM P6 file including header. For VGA Mode 13h:
    /// `render_framebuffer(0xA0000, 320, 200)`.
    pub fn render_framebuffer(&self, base_addr: usize, width: usize, height: usize) -> Vec<u8> {
        self.state.render_framebuffer(base_addr, width, height)
    }

    /// Read a graphics-mode framebuffer as raw RGBA bytes.
    ///
    /// Returns `width * height * 4` bytes suitable for direct use with
    /// `new ImageData(new Uint8ClampedArray(bytes), width, height)` and
    /// `ctx.putImageData()` in the browser.
    pub fn read_framebuffer_rgba(
        &self,
        base_addr: usize,
        width: usize,
        height: usize,
    ) -> Vec<u8> {
        self.state.read_framebuffer_rgba(base_addr, width, height)
    }

    /// Read the current video mode from the BDA (0x0449).
    ///
    /// Returns the byte at flat address 0x0449 (BDA segment 0x0040, offset 0x49).
    /// This is written by INT 10h AH=00h (set mode) and read by AH=0Fh (get mode).
    /// Common values: 0x03 = 80x25 text, 0x13 = VGA Mode 13h (320x200x256).
    pub fn get_video_mode(&self) -> u8 {
        self.state.read_mem(0x0449) as u8
    }

    /// Read the last video mode the program REQUESTED, before any silent
    /// remap. Written by the corduroy BIOS to linear 0x04F2 on every
    /// INT 10h AH=00h call. If this differs from `get_video_mode()` the
    /// program asked for a mode CSS-DOS doesn't implement (EGA/VGA planar,
    /// CGA, Hercules, etc.) and was silently remapped to text mode 0x03.
    pub fn get_requested_video_mode(&self) -> u8 {
        self.state.read_mem(0x04F2) as u8
    }

    /// Read the sticky "unknown opcode" latch. 0 means none seen yet.
    /// A non-zero value is the opcode byte of the first instruction the
    /// CPU hit that has no dispatch entry — typically a 286/386/486
    /// instruction the 8086 core doesn't implement. Execution is
    /// effectively halted because IP can't advance through it.
    pub fn get_halt_code(&self) -> u8 {
        self.state.get_var("haltCode").unwrap_or(0) as u8
    }

    /// Return string properties as a JSON object string, e.g. `{"textBuffer":"Hello"}`.
    pub fn get_string_properties(&self) -> String {
        let pairs: Vec<String> = self
            .state
            .string_properties
            .iter()
            .map(|(k, v)| {
                let escaped = v
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('\n', "\\n")
                    .replace('\r', "\\r")
                    .replace('\t', "\\t");
                format!("\"{k}\":\"{escaped}\"")
            })
            .collect();
        format!("{{{}}}", pairs.join(","))
    }
}
