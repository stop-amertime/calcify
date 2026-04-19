//! CSS-level detector: range-predicate bulk-fill → Op::MemoryFill binding.
//!
//! Recognises the shape emitted by CSS-DOS's kiln transpiler for INT 2F/AH=FE
//! bulk fills. Every memory cell gains a clause of the form:
//!
//! ```css
//! --m<N>: if(
//!   style(--bulkOpKind: 1):
//!     calc(
//!       sign(max(0, <N> - var(--bulkDst) + 1))
//!       * sign(max(0, var(--bulkDst) + var(--bulkCount) - <N>))
//!       * var(--bulkValue)
//!       + (1 - sign(max(0, <N> - var(--bulkDst) + 1))
//!            * sign(max(0, var(--bulkDst) + var(--bulkCount) - <N>)))
//!       * var(--__1m<N>)
//!     );
//!   style(--memAddr0: <N>): var(--memVal0);
//!   ...
//!   else: var(--__1m<N>));
//! ```
//!
//! When every cell in a dense address range has this identical top-clause
//! (differing only in the cell address literal), collapse to one binding:
//! `{ dst_prop="--bulkDst", count_prop="--bulkCount", val_prop="--bulkValue",
//!    kind_prop="--bulkOpKind", addr_range=<min..max+1> }`.
//!
//! Wired into Evaluator::tick as a pre-tick hook: when `--bulkOpKind == 1`,
//! execute `state.memory[dst..dst+count].fill(value & 0xFF)` natively in Rust
//! before the normal broadcast-write path runs. The per-cell CSS still
//! compiles (cells outside the range need their normal slot dispatch for
//! non-bulk ticks), but the in-range fill path no longer burns one calcite
//! tick per byte.
//!
//! **STATUS (PR 1 in-progress):** skeleton only. The detector body,
//! `CompiledProgram::bulk_fill_bindings`, and the pre-tick execution hook
//! are not yet wired — Task 1.7 completes them. The kiln emission exists
//! and parses, so bootle-ctest.css compiles into per-cell calc expressions
//! today (slower than before the detector is enabled; faster once it fires).

#![allow(dead_code)]

use crate::types::*;

/// One CSS-level bulk-fill binding recognised across a range of memory cells.
#[derive(Debug, Clone)]
pub struct CssMemoryFillBinding {
    pub kind_property: String,
    pub dst_property: String,
    pub count_property: String,
    pub val_property: String,
    pub addr_range: std::ops::Range<i64>,
}

/// Analyse assignments for the bulk-fill range-predicate pattern.
///
/// Stub: returns empty. Task 1.7 implements the matcher.
pub fn recognise_bulk_fills(assignments: &[Assignment]) -> Vec<CssMemoryFillBinding> {
    let _ = assignments;
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_yields_no_bindings() {
        assert!(recognise_bulk_fills(&[]).is_empty());
    }
}
