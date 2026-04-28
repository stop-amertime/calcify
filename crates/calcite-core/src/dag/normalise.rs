//! Phase 2 — DAG normalisation framework.
//!
//! Phase 1 builds a CFG of basic blocks whose body is the original `Op`
//! slice; the walker re-uses `exec_ops` for execution. Phase 2 introduces
//! a *parallel* per-op annotation that records the canonical shape each
//! op (or run of ops) belongs to. The walker is unchanged for now —
//! Phase 3 codegen is what consumes the annotations and emits faster
//! code for them.
//!
//! ## Why "annotation parallel to ops" rather than "replace ops"
//!
//! Two reasons:
//!
//! 1. **Bit-identical gate.** Phase 2 must not change observable
//!    behaviour. Annotating preserves the existing `exec_ops` path
//!    untouched; the rewriter framework cannot accidentally break
//!    semantics because the rewriter doesn't run code. Phase 3 picks
//!    the fast-path or falls back to the op stream.
//! 2. **Genericity.** Replacing ops with super-ops would require us
//!    to ship a new `Op` variant per idiom — that's the peephole road
//!    the mission doc is moving away from. Annotations let us
//!    enumerate idioms without adding executor code per idiom.
//!
//! ## Cardinal-rule contract for `Pattern` impls
//!
//! Every `Pattern::try_match` impl in this module — and any future
//! impl — must reason about CSS-structural shape only. Concretely:
//!
//! - **No specific property names.** A matcher must not check whether
//!   a slot maps to `--AX` or `--memAddr0`; only its op-stream
//!   structural role (how it's read/written, by which op kinds,
//!   in which order).
//! - **No specific literal values.** Matchers may inspect *that* a
//!   literal exists and *how it relates* to other literals (equal,
//!   sequential, +1), but never compare against a hard-coded constant
//!   from a cabinet (an opcode number, an address, a flag bit).
//! - **No payload semantics.** The op-stream's literals encode CPU
//!   opcodes / memory addresses / flag bits in CSS-DOS cabinets; in
//!   another cabinet they encode something else entirely. Matchers
//!   that pattern-match on payload values are program-specific and
//!   forbidden by cardinal rule #1.
//!
//! Genericity probe (per-pattern, mandatory): "Would this matcher fire
//! on a 6502 cabinet, a brainfuck cabinet, a non-emulator cabinet whose
//! CSS shares the structural shape?" If the answer is no on any of
//! those, the matcher is overfit and must be rewritten or rejected.
//!
//! See `docs/phase2-idiom-catalogue.md` for the enumerated structural
//! idioms. Phase 2's first batch of patterns implements a subset of
//! those; future patterns extend the list.

use crate::compile::{CompiledProgram, Op};

use super::types::{BasicBlock, Dag, OpIndex};

/// A canonical shape recognised over the op stream. The variants are
/// CSS-structural — none of them encode cabinet-specific knowledge.
///
/// Phase 2 starts with a minimal vocabulary; new variants are added as
/// patterns are implemented. Each variant carries the data Phase 3
/// codegen needs to emit a fast path. The op-stream is kept verbatim;
/// `IdiomKind` is purely an annotation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdiomKind {
    /// A `LoadState` followed by a same-address `StoreState`. Equivalent
    /// to "this slot's value is unchanged this tick" (idiom 8 in the
    /// catalogue).
    ///
    /// Codegen target: omit the read+write entirely; the slot already
    /// holds the previous-tick value.
    Hold,
    /// A pair of arithmetic ops that fold to a single bit-field
    /// extraction `(x >> shift) & mask` (idiom 11 in the catalogue).
    /// Captures `Shr(_, k)` then `AndLit(_, mask)` and similar shapes.
    ///
    /// `shift` and `mask` are both literal CSS values, not cabinet
    /// payload — a 6502 cabinet emitting bit-field reads via `--bit`
    /// would produce the same shape with different numbers.
    BitField { shift: u8, mask: u32 },
    /// A `LoadState` whose value is already loaded into another slot
    /// earlier in the same block by an identical `LoadState` with no
    /// intervening `StoreState` to the same address (idiom 13 in the
    /// catalogue — repeated-load CSE).
    ///
    /// `prior_dst` is the slot that already holds the value. Codegen
    /// can replace this op with a `LoadSlot` (or elide it if liveness
    /// permits).
    RepeatedLoad { prior_dst: u32 },
    /// A literal-merging arithmetic identity: `AddLit(AddLit(x, k1),
    /// k2)` collapsing to `AddLit(x, k1+k2)`, or the equivalent for
    /// `MulLit`. The matcher records the merged constant so codegen
    /// can emit one op instead of two (idiom 14 in the catalogue).
    ConstFold { merged_lit: i32 },
}

/// Annotation attached to a specific op (or to the trailing op of a
/// short run). Phase 3 codegen reads `Annotation` to decide what to emit
/// for that op; absent an annotation, codegen falls back to the op's
/// default lowering.
#[derive(Debug, Clone)]
pub struct Annotation {
    /// PC of the op being annotated (into `CompiledProgram::ops`).
    pub pc: OpIndex,
    /// Number of ops the annotation covers, starting at `pc` (inclusive).
    /// `1` for single-op annotations; `>1` for ConstFold or
    /// RepeatedLoad-from-CSE patterns that want to elide the original
    /// chain.
    pub span: u32,
    pub kind: IdiomKind,
}

/// Result of running the rewriter over a `Dag`.
#[derive(Debug, Default, Clone)]
pub struct NormalisedDag {
    /// Annotations indexed by their op PC. At most one annotation per PC.
    pub annotations: Vec<Annotation>,
}

impl NormalisedDag {
    /// O(log n) lookup: find an annotation covering `pc`, if any.
    pub fn annotation_at(&self, pc: OpIndex) -> Option<&Annotation> {
        match self.annotations.binary_search_by_key(&pc, |a| a.pc) {
            Ok(i) => Some(&self.annotations[i]),
            Err(_) => None,
        }
    }

    /// Total annotation count — the headline metric for Phase 2's
    /// decision gate ("≥30 % node-count reduction on Doom").
    pub fn count(&self) -> usize {
        self.annotations.len()
    }
}

/// One shape-matcher.
///
/// Implementors inspect `block.body(program)` starting at `start_pc`
/// (a PC into `program.ops`, guaranteed to lie within the block) and
/// return either an `Annotation` or `None`. The driver visits every
/// op in source order and gives every registered pattern a chance.
///
/// Patterns are pure: they read but do not mutate `program` or `block`.
/// Mutations happen only on the returned `NormalisedDag`.
///
/// **Cardinal-rule contract.** A `Pattern` impl must not branch on
/// specific property names, specific literal values, or anything else
/// that wouldn't fire on a structurally-identical non-CSS-DOS cabinet.
/// The genericity probe is mandatory; the doc-comment on the impl must
/// answer it explicitly.
pub trait Pattern: Send + Sync {
    /// Diagnostic name (used by `--dump-normalisation`).
    fn name(&self) -> &'static str;

    /// Try to match a shape starting at `program.ops[start_pc]`.
    /// `block` is the enclosing basic block — patterns may inspect
    /// preceding ops within the same block but must not reach across
    /// block boundaries (control-flow could re-enter the prior block
    /// from another path, breaking the assumption).
    fn try_match(
        &self,
        program: &CompiledProgram,
        block: &BasicBlock,
        start_pc: OpIndex,
    ) -> Option<Annotation>;
}

/// The default pattern set. Lives in `dag::patterns` so this module
/// stays free of cabinet-shape opinions; the framework is just the
/// driver. Imported via the top-level `dag::phase2_patterns()`.
fn default_patterns() -> Vec<Box<dyn Pattern>> {
    super::patterns::phase2_patterns()
}

/// Run a pattern set over a single block. Annotations are appended to
/// `out` in PC order. The driver enforces the "first match wins" rule:
/// once an op range is covered by an annotation, no later pattern can
/// claim any PC in that range.
fn normalise_block(
    program: &CompiledProgram,
    block: &BasicBlock,
    patterns: &[Box<dyn Pattern>],
    out: &mut Vec<Annotation>,
) {
    let mut pc = block.pc_lo;
    while pc < block.pc_hi {
        let mut claimed = false;
        for pattern in patterns {
            if let Some(ann) = pattern.try_match(program, block, pc) {
                debug_assert!(
                    ann.pc == pc,
                    "Pattern {:?} returned annotation at pc {} but driver expected {}",
                    pattern.name(),
                    ann.pc,
                    pc,
                );
                debug_assert!(
                    ann.span >= 1 && ann.pc + ann.span <= block.pc_hi,
                    "Pattern {:?} returned annotation that extends past the block",
                    pattern.name(),
                );
                let advance = ann.span;
                out.push(ann);
                pc += advance;
                claimed = true;
                break;
            }
        }
        if !claimed {
            pc += 1;
        }
    }
}

/// Run the default pattern set across every block in `dag`. Returns a
/// `NormalisedDag` whose annotations are sorted by PC (so `annotation_at`
/// can binary-search).
pub fn normalise(program: &CompiledProgram, dag: &Dag) -> NormalisedDag {
    normalise_with(program, dag, &default_patterns())
}

/// Run an arbitrary pattern set. Test/debug entry point.
pub fn normalise_with(
    program: &CompiledProgram,
    dag: &Dag,
    patterns: &[Box<dyn Pattern>],
) -> NormalisedDag {
    let mut annotations = Vec::new();
    for block in &dag.blocks {
        normalise_block(program, block, patterns, &mut annotations);
    }
    // Annotations are pushed in (block-order, pc-order) — block order is
    // already pc-sorted because basic blocks are built in pc order, and
    // within a block we walk pc ascending. So the result is already
    // sorted; the assert below catches a regression.
    debug_assert!(
        annotations.windows(2).all(|w| w[0].pc < w[1].pc),
        "annotations not sorted by pc — driver invariant violated"
    );
    NormalisedDag { annotations }
}

/// Helper for `Pattern` impls that want the op slice the block exposes.
#[allow(dead_code)]
pub(crate) fn block_op(program: &CompiledProgram, pc: OpIndex) -> &Op {
    &program.ops[pc as usize]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::{CompiledProgram, Op};
    use crate::dag::build_dag;

    fn empty_program() -> CompiledProgram {
        CompiledProgram {
            ops: Vec::new(),
            slot_count: 0,
            writeback: Vec::new(),
            broadcast_writes: Vec::new(),
            dispatch_tables: Vec::new(),
            chain_tables: Vec::new(),
            flat_dispatch_arrays: Vec::new(),
            functions: Vec::new(),
            property_slots: Default::default(),
            packed_cell_tables: Vec::new(),
            packed_exception_tables: Vec::new(),
            packed_broadcast_writes: Vec::new(),
            disk_window: None,
            has_rep_machinery: false,
        }
    }

    /// A test-only pattern that claims every op as `Hold`. Used to
    /// exercise the driver's "first match wins" behaviour without
    /// committing to a real shape detector.
    struct ClaimAll;
    impl Pattern for ClaimAll {
        fn name(&self) -> &'static str {
            "test:claim-all"
        }
        fn try_match(
            &self,
            _program: &CompiledProgram,
            _block: &BasicBlock,
            start_pc: OpIndex,
        ) -> Option<Annotation> {
            Some(Annotation { pc: start_pc, span: 1, kind: IdiomKind::Hold })
        }
    }

    #[test]
    fn empty_default_pattern_set_produces_no_annotations() {
        let mut p = empty_program();
        p.ops = vec![Op::LoadLit { dst: 0, val: 1 }, Op::LoadLit { dst: 1, val: 2 }];
        p.slot_count = 2;
        let dag = build_dag(&p);
        let norm = normalise(&p, &dag);
        assert_eq!(norm.count(), 0);
    }

    #[test]
    fn driver_visits_every_pc_exactly_once_with_unit_pattern() {
        let mut p = empty_program();
        p.ops = vec![
            Op::LoadLit { dst: 0, val: 1 },
            Op::LoadLit { dst: 1, val: 2 },
            Op::LoadLit { dst: 2, val: 3 },
        ];
        p.slot_count = 3;
        let dag = build_dag(&p);
        let patterns: Vec<Box<dyn Pattern>> = vec![Box::new(ClaimAll)];
        let norm = normalise_with(&p, &dag, &patterns);
        assert_eq!(norm.count(), 3);
        assert_eq!(norm.annotations[0].pc, 0);
        assert_eq!(norm.annotations[1].pc, 1);
        assert_eq!(norm.annotations[2].pc, 2);
    }

    #[test]
    fn annotation_lookup_is_correct() {
        let mut p = empty_program();
        p.ops = vec![
            Op::LoadLit { dst: 0, val: 1 },
            Op::LoadLit { dst: 1, val: 2 },
        ];
        p.slot_count = 2;
        let dag = build_dag(&p);
        let patterns: Vec<Box<dyn Pattern>> = vec![Box::new(ClaimAll)];
        let norm = normalise_with(&p, &dag, &patterns);
        assert!(norm.annotation_at(0).is_some());
        assert!(norm.annotation_at(1).is_some());
        assert!(norm.annotation_at(2).is_none());
    }
}
