//! WASM-safe timing for compile/eval logging.
//!
//! `std::time::Instant::now()` panics on `wasm32-unknown-unknown`.
//! This module provides a drop-in replacement that returns 0.0 on WASM.

pub struct Timer {
    #[cfg(not(target_arch = "wasm32"))]
    start: std::time::Instant,
}

impl Timer {
    pub fn now() -> Self {
        Timer {
            #[cfg(not(target_arch = "wasm32"))]
            start: std::time::Instant::now(),
        }
    }

    pub fn elapsed_secs(&self) -> f64 {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.start.elapsed().as_secs_f64()
        }
        #[cfg(target_arch = "wasm32")]
        {
            0.0
        }
    }
}
