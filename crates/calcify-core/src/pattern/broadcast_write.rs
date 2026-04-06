//! Broadcast write pattern: `if(style(--dest: {addr}): value; else: keep)` → direct store.
//!
//! After the CPU computes a destination and value, x86CSS broadcasts to all 1,583
//! variables, each checking if they're the target. This pattern replaces it with
//! `state[dest] = value`.
