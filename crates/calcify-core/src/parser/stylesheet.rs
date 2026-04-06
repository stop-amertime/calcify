//! Top-level stylesheet parsing.
//!
//! Iterates through CSS rules, dispatching to specialised parsers for
//! `@property`, `@function`, `@keyframes`, `@container`, and style rules.
