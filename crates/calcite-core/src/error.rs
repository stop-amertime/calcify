//! Error types for the calcite engine.

use thiserror::Error;

/// Convenience alias for `Result<T, CalciteError>`.
pub type Result<T> = std::result::Result<T, CalciteError>;

/// Errors that can occur during CSS parsing, evaluation, and pattern recognition.
#[derive(Debug, Error)]
pub enum CalciteError {
    /// CSS parsing failed.
    #[error("parse error: {0}")]
    Parse(String),

    /// Encountered an unrecognised `@property` syntax descriptor.
    #[error("unknown @property type: {0}")]
    UnknownPropertyType(String),

    /// Called an `@function` that was never defined.
    #[error("undefined function: {0}")]
    UndefinedFunction(String),

    /// Referenced a `var()` that was never declared.
    #[error("undefined variable: {0}")]
    UndefinedVariable(String),

    /// Runtime evaluation error.
    #[error("evaluation error: {0}")]
    Eval(String),

    /// Pattern recognition failed.
    #[error("pattern recognition error: {0}")]
    Pattern(String),

    /// Filesystem I/O error.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization error (conformance feature only).
    #[cfg(feature = "conformance")]
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}
