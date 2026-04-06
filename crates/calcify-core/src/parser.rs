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
pub mod property;
pub mod stylesheet;

use crate::error::Result;
use crate::types::ParsedProgram;

/// Parse a CSS string into a `ParsedProgram`.
///
/// This is the main entry point for Phase 1. It extracts all computational
/// constructs from the CSS and builds the intermediate representation.
pub fn parse_css(_input: &str) -> Result<ParsedProgram> {
    // TODO(phase-1): Implement CSS parsing pipeline.
    //
    // 1. Tokenise with cssparser
    // 2. Extract @property declarations
    // 3. Extract @function definitions
    // 4. Extract .cpu property assignments (in declaration order)
    // 5. Parse if(style()), calc(), var() expressions into Expr trees
    todo!("Phase 1: CSS parsing")
}
