use thiserror::Error;

pub type Result<T> = std::result::Result<T, CalcifyError>;

#[derive(Debug, Error)]
pub enum CalcifyError {
    #[error("parse error: {0}")]
    Parse(String),

    #[error("unknown @property type: {0}")]
    UnknownPropertyType(String),

    #[error("undefined function: {0}")]
    UndefinedFunction(String),

    #[error("undefined variable: {0}")]
    UndefinedVariable(String),

    #[error("evaluation error: {0}")]
    Eval(String),

    #[error("pattern recognition error: {0}")]
    Pattern(String),
}
