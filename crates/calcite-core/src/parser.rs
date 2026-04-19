//! CSS parser for the computational subset.
//!
//! Built on Servo's `cssparser` crate for tokenisation. Handles:
//! - `@property` declarations
//! - `@function` definitions (parameters, locals, result)
//! - `if(style(...): ...; else: ...)` conditionals
//! - `calc()`, `mod()`, `round()`, `min()`, `max()`, `clamp()`, `pow()`, `sign()`, `abs()`
//! - `var()` references with fallbacks
//! - Property assignments on `.cpu`

pub mod css_functions;
pub mod fast_path;
pub mod property;
pub mod stylesheet;

pub use css_functions::parse_expr;
pub use stylesheet::parse_stylesheet;

use crate::error::Result;
use crate::types::ParsedProgram;

/// Parse a CSS string into a `ParsedProgram`.
///
/// This is the main entry point. It extracts all computational constructs
/// from the CSS and builds the intermediate representation.
pub fn parse_css(input: &str) -> Result<ParsedProgram> {
    parse_stylesheet(input)
}
