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
    evaluator: calcify_core::Evaluator,
}

#[wasm_bindgen]
impl CalcifyEngine {
    /// Create a new engine instance from CSS source text.
    #[wasm_bindgen(constructor)]
    pub fn new(css: &str) -> Result<CalcifyEngine, JsError> {
        log::info!("Parsing {} bytes of CSS", css.len());

        let parsed =
            calcify_core::parser::parse_css(css).map_err(|e| JsError::new(&e.to_string()))?;

        log::info!(
            "Parsed: {} @property, {} @function, {} assignments",
            parsed.properties.len(),
            parsed.functions.len(),
            parsed.assignments.len(),
        );

        let evaluator = calcify_core::Evaluator::from_parsed(&parsed);

        Ok(CalcifyEngine {
            state: calcify_core::State::default(),
            evaluator,
        })
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
