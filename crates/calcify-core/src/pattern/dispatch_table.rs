//! Dispatch table pattern: large `if(style(--param: N))` chains → hash map / array index.
//!
//! The `readMem()` function in x86CSS has ~1,602 branches. Each call does a linear
//! scan. This pattern replaces it with an O(1) array index lookup.
