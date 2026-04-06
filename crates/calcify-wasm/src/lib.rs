//! WASM bindings for calc(ify).
//!
//! This crate compiles to a WASM module that runs inside a Web Worker.
//! The main thread sends CSS text, the worker parses/compiles/evaluates,
//! and sends back property diffs for DOM application.

use wasm_bindgen::prelude::*;

/// Initialise the WASM module (sets up logging, etc.).
#[wasm_bindgen(start)]
pub fn init() {
    console_log::init_with_level(log::Level::Info).ok();
    log::info!("calc(ify) WASM module initialised");
}

/// The main engine handle exposed to JavaScript.
#[wasm_bindgen]
pub struct CalcifyEngine {
    state: calcify_core::State,
    // evaluator will be added once parsing/compilation is implemented
}

#[wasm_bindgen]
impl CalcifyEngine {
    /// Create a new engine instance from CSS source text.
    #[wasm_bindgen(constructor)]
    pub fn new(css: &str) -> Result<CalcifyEngine, JsError> {
        log::info!("Parsing {} bytes of CSS", css.len());

        let _parsed =
            calcify_core::parser::parse_css(css).map_err(|e| JsError::new(&e.to_string()))?;

        // TODO(phase-3): compile and create evaluator
        // let program = calcify_core::pattern::compile(&parsed)
        //     .map_err(|e| JsError::new(&e.to_string()))?;

        Ok(CalcifyEngine {
            state: calcify_core::State::default(),
        })
    }

    /// Run a batch of ticks and return the property changes as a JS object.
    ///
    /// Returns a JSON string of `[[name, value], ...]` pairs.
    pub fn tick_batch(&mut self, _count: u32) -> Result<String, JsError> {
        // TODO(phase-3): run batch, collect changes, serialise
        Ok("[]".to_string())
    }

    /// Set the keyboard input state.
    pub fn set_keyboard(&mut self, key: u8) {
        self.state.keyboard = key;
    }

    /// Get the current value of a register (for debugging).
    pub fn get_register(&self, index: usize) -> i32 {
        if index < self.state.registers.len() {
            self.state.registers[index]
        } else {
            0
        }
    }
}
