//! Phase 2 — first batch of structural recognisers.
//!
//! Every pattern in this module passes the genericity probe: each one
//! would fire on a 6502 cabinet, brainfuck cabinet, or non-emulator
//! cabinet whose CSS happens to share the structural shape. None of
//! them inspect specific property names, specific opcodes, or any
//! other cabinet-payload value.
//!
//! The patterns are intentionally minimal — we want a small, well-
//! understood set first, then add more once Phase 3 codegen exists to
//! consume the annotations. Phase 2's decision-gate ("≥30 % node-count
//! reduction on Doom") is the headline metric; node count is the
//! number of *un-annotated* ops the codegen would have to lower
//! individually.

use crate::compile::{CompiledProgram, Op};

use super::normalise::{Annotation, IdiomKind, Pattern};
use super::types::{BasicBlock, OpIndex};

/// `AddLit(AddLit(x, k1), k2)` → merged `AddLit(x, k1+k2)`. Same shape
/// applies to `MulLit` and `SubLit` chains. Three independent matchers
/// share this struct via the `op` constructor parameter — all three
/// instances are added to the default pattern set.
///
/// ## Genericity probe
///
/// "Const-folding adjacent literal arithmetic" is one of the oldest
/// compiler passes there is — it doesn't know or care what numbers
/// it's folding. A Brainfuck cabinet that emits `pointer + 3 + 4`
/// for two adjacent `>` operators produces this shape; a 6502 cabinet
/// that emits `addr + opcode_size + operand_count` produces this
/// shape; a CSS gradient interpolation that emits
/// `(t * scale) * factor` produces this shape (for the multiplicative
/// variant). Pass.
pub struct LitMerge {
    /// Which op this matcher folds across. Only `AddLit`, `SubLit`,
    /// `MulLit`, `ShlLit`, `ShrLit`, `AndLit`, `ModLit` are accepted.
    op_kind: LitOp,
}

#[derive(Debug, Clone, Copy)]
enum LitOp {
    Add,
    Sub,
    Mul,
    Shl,
    Shr,
    And,
}

impl LitMerge {
    pub fn add() -> Self { Self { op_kind: LitOp::Add } }
    pub fn sub() -> Self { Self { op_kind: LitOp::Sub } }
    pub fn mul() -> Self { Self { op_kind: LitOp::Mul } }
    pub fn shl() -> Self { Self { op_kind: LitOp::Shl } }
    pub fn shr() -> Self { Self { op_kind: LitOp::Shr } }
    pub fn and() -> Self { Self { op_kind: LitOp::And } }

    fn read(&self, op: &Op) -> Option<(u32, u32, i32)> {
        match (self.op_kind, op) {
            (LitOp::Add, Op::AddLit { dst, a, val }) => Some((*dst, *a, *val)),
            (LitOp::Sub, Op::SubLit { dst, a, val }) => Some((*dst, *a, *val)),
            (LitOp::Mul, Op::MulLit { dst, a, val }) => Some((*dst, *a, *val)),
            (LitOp::Shl, Op::ShlLit { dst, a, val }) => Some((*dst, *a, *val)),
            (LitOp::Shr, Op::ShrLit { dst, a, val }) => Some((*dst, *a, *val)),
            (LitOp::And, Op::AndLit { dst, a, val }) => Some((*dst, *a, *val)),
            _ => None,
        }
    }

    fn merge(&self, k1: i32, k2: i32) -> Option<i32> {
        match self.op_kind {
            LitOp::Add => k1.checked_add(k2),
            LitOp::Sub => k1.checked_add(k2),
            LitOp::Mul => k1.checked_mul(k2),
            // Shifts: combined shift count must fit in i32 range AND in i8
            // (CSS expression semantics use byte-sized shift counts; the
            // bytecode evaluator clamps via `as u32`). 64 is a safe upper
            // bound that covers any sane shift sequence.
            LitOp::Shl | LitOp::Shr => {
                let s = k1.checked_add(k2)?;
                if s >= 0 && s < 64 { Some(s) } else { None }
            }
            // AND: Op::AndLit val is the literal mask bits applied as
            // `slot & val`. Composing two AndLits is `slot & k1 & k2`,
            // which is `slot & (k1 & k2)`.
            LitOp::And => Some(k1 & k2),
        }
    }
}

impl Pattern for LitMerge {
    fn name(&self) -> &'static str {
        match self.op_kind {
            LitOp::Add => "litmerge:add",
            LitOp::Sub => "litmerge:sub",
            LitOp::Mul => "litmerge:mul",
            LitOp::Shl => "litmerge:shl",
            LitOp::Shr => "litmerge:shr",
            LitOp::And => "litmerge:and",
        }
    }

    fn try_match(
        &self,
        program: &CompiledProgram,
        block: &BasicBlock,
        start_pc: OpIndex,
    ) -> Option<Annotation> {
        if start_pc + 1 >= block.pc_hi {
            return None;
        }
        let (dst1, _a1, k1) = self.read(&program.ops[start_pc as usize])?;
        let (_dst2, a2, k2) = self.read(&program.ops[start_pc as usize + 1])?;

        // Second op must read the slot the first op wrote. dst slots can
        // legitimately be the same (in-place fold) or different (copy
        // forward). Both are fine.
        if a2 != dst1 {
            return None;
        }

        // No other op in the block can read dst1 between the two ops —
        // they're adjacent so this is automatically satisfied — and no
        // op outside the block can read it either (because slot
        // liveness is per-block in the bytecode shape we emit). The
        // adjacent-op invariant is enough; any non-adjacent merge is a
        // separate pattern.

        let merged = self.merge(k1, k2)?;
        Some(Annotation { pc: start_pc, span: 2, kind: IdiomKind::ConstFold { merged_lit: merged } })
    }
}

/// `ShrLit(_, shift)` followed by `AndLit(_, mask)` → bit-field
/// extraction `(x >> shift) & mask`. `Op::AndLit`'s `val` field is a
/// precomputed mask (the compiler emits the mask bits directly, not
/// the bit-width). The matcher reasons about op shape, not source
/// CSS — so any cabinet emitting this op shape gets the annotation.
///
/// ## Genericity probe
///
/// Bit-field extraction is universal in numerical CSS — palette index
/// extraction, fixed-point arithmetic, packed colour decoding, multi-
/// digit number rendering. A non-emulator cabinet computing
/// `(value >> 4) & 0xF` for a hex digit produces this shape. Pass.
pub struct BitFieldMatch;

impl Pattern for BitFieldMatch {
    fn name(&self) -> &'static str {
        "bitfield:shr-and"
    }

    fn try_match(
        &self,
        program: &CompiledProgram,
        block: &BasicBlock,
        start_pc: OpIndex,
    ) -> Option<Annotation> {
        if start_pc + 1 >= block.pc_hi {
            return None;
        }
        let (shr_dst, _shr_a, shift) = match &program.ops[start_pc as usize] {
            Op::ShrLit { dst, a, val } => (*dst, *a, *val),
            _ => return None,
        };
        let (_and_dst, and_a, mask_lit) = match &program.ops[start_pc as usize + 1] {
            Op::AndLit { dst, a, val } => (*dst, *a, *val),
            _ => return None,
        };
        if and_a != shr_dst {
            return None;
        }
        // Shift must be in [0, 31]. The CSS / bytecode evaluator
        // already enforces this on the live path.
        if !(0..32).contains(&shift) {
            return None;
        }
        // Mask must be non-negative — Op::AndLit val is the literal
        // mask bits, applied as `slot[a] & val`. Negative values
        // would just be sign-extended ones; treating them as a
        // BitField annotation is meaningless.
        if mask_lit < 0 {
            return None;
        }
        Some(Annotation {
            pc: start_pc,
            span: 2,
            kind: IdiomKind::BitField { shift: shift as u8, mask: mask_lit as u32 },
        })
    }
}

/// `LoadState { dst, addr }` followed (eventually within the block) by
/// `StoreState { addr, src: dst }` with no intervening op writing to
/// `dst` and no intervening `StoreState` to the same address. This is
/// the "self-hold" idiom — a state property that's preserved this tick
/// without modification.
///
/// The annotation goes on the `LoadState` op; codegen elides both ops.
///
/// ## Genericity probe
///
/// "Read a value, write it back unchanged" is the most fundamental
/// idle-state pattern in any double-buffered system. CSS Houdini
/// custom properties, animation interpolation no-ops, any cabinet
/// with a hold-state register — all emit this shape. Pass.
pub struct HoldMatch;

impl Pattern for HoldMatch {
    fn name(&self) -> &'static str {
        "hold:load-store-pair"
    }

    fn try_match(
        &self,
        program: &CompiledProgram,
        block: &BasicBlock,
        start_pc: OpIndex,
    ) -> Option<Annotation> {
        let (load_dst, load_addr) = match &program.ops[start_pc as usize] {
            Op::LoadState { dst, addr } => (*dst, *addr),
            _ => return None,
        };

        // Scan forward within the block for the matching StoreState. Bail
        // on any op that writes to `load_dst` (that breaks the hold) or
        // any StoreState to a different addr (we don't model store
        // ordering across addresses here — keeping the matcher
        // conservative).
        let mut pc = start_pc + 1;
        while pc < block.pc_hi {
            let op = &program.ops[pc as usize];
            // Any op writing to load_dst breaks the hold (the value is
            // no longer the original load).
            if let Some(d) = op_writes_slot(op) {
                if d == load_dst {
                    return None;
                }
            }
            match op {
                Op::StoreState { addr, src } if *addr == load_addr => {
                    if *src == load_dst {
                        // Found the matching store — this is a hold.
                        // Span covers both ops (load at start_pc, store
                        // at pc), but `span` semantics require a
                        // contiguous range. Since the two ops may be
                        // non-adjacent, we record only the load op's
                        // PC and span=1; codegen looks up the matching
                        // store via a separate annotation field.
                        //
                        // For Phase 2 simplicity, only annotate the
                        // adjacent case (pc == start_pc + 1). The
                        // non-adjacent case needs a richer annotation
                        // that we'll add when Phase 3 codegen needs it.
                        if pc == start_pc + 1 {
                            return Some(Annotation {
                                pc: start_pc,
                                span: 2,
                                kind: IdiomKind::Hold,
                            });
                        } else {
                            return None;
                        }
                    } else {
                        // Same addr but different source — not a self-hold.
                        return None;
                    }
                }
                Op::StoreState { addr, .. } if *addr != load_addr => {
                    // Store to another addr — fine, keep scanning.
                }
                Op::StoreMem { .. } => {
                    // Runtime-addressed store could alias load_addr.
                    // Be conservative.
                    return None;
                }
                _ => {}
            }
            pc += 1;
        }
        None
    }
}

/// Two `LoadState` ops within one block reading the same address with
/// no intervening `StoreState` to that address. The second load is
/// redundant — its value is already in the first load's `dst`.
///
/// The annotation goes on the *second* load op; codegen replaces it
/// with a slot copy (or elides entirely if the consumer can be
/// re-pointed to the first load's slot).
///
/// ## Genericity probe
///
/// Common-subexpression elimination on memory loads is universal.
/// Any CSS that reads `var(--__1foo)` twice in one cascade has this
/// shape. A non-emulator cabinet computing
/// `var(--__1color) + var(--__1color) * 0.5` for a brightness blend
/// has it. Pass.
pub struct RepeatedLoadMatch;

impl Pattern for RepeatedLoadMatch {
    fn name(&self) -> &'static str {
        "cse:repeated-load-state"
    }

    fn try_match(
        &self,
        program: &CompiledProgram,
        block: &BasicBlock,
        start_pc: OpIndex,
    ) -> Option<Annotation> {
        let (this_dst, this_addr) = match &program.ops[start_pc as usize] {
            Op::LoadState { dst, addr } => (*dst, *addr),
            _ => return None,
        };

        // Scan backward within this block for an earlier LoadState with
        // the same addr. Stop on any StoreState to that addr (the value
        // could differ) or any aliasing runtime store.
        if start_pc == block.pc_lo {
            return None;
        }
        let mut pc = start_pc;
        while pc > block.pc_lo {
            pc -= 1;
            let op = &program.ops[pc as usize];
            match op {
                Op::LoadState { dst, addr } if *addr == this_addr => {
                    // Earlier load found — but only annotate if the
                    // earlier `dst` slot still holds that value at
                    // start_pc. We approximate by requiring that no op
                    // in (pc, start_pc) writes to that slot. We've been
                    // walking backward; do a forward sanity check.
                    let prior_dst = *dst;
                    if prior_dst == this_dst {
                        // Same dst slot — the second load would just
                        // overwrite it with the same value. Still a CSE
                        // candidate (codegen elides the second load
                        // entirely), but we need to confirm no write to
                        // that slot in between.
                    }
                    let mut q = pc + 1;
                    let mut clobbered = false;
                    while q < start_pc {
                        if let Some(d) = op_writes_slot(&program.ops[q as usize]) {
                            if d == prior_dst {
                                clobbered = true;
                                break;
                            }
                        }
                        // Any StoreState to the same addr invalidates
                        // the value (the memory changed).
                        if let Op::StoreState { addr, .. } = &program.ops[q as usize] {
                            if *addr == this_addr {
                                clobbered = true;
                                break;
                            }
                        }
                        // Runtime-addressed stores could alias —
                        // conservative: bail.
                        if matches!(&program.ops[q as usize], Op::StoreMem { .. }) {
                            clobbered = true;
                            break;
                        }
                        q += 1;
                    }
                    if !clobbered {
                        return Some(Annotation {
                            pc: start_pc,
                            span: 1,
                            kind: IdiomKind::RepeatedLoad { prior_dst },
                        });
                    }
                    return None;
                }
                Op::StoreState { addr, .. } if *addr == this_addr => {
                    // Store in between invalidates the load.
                    return None;
                }
                Op::StoreMem { .. } => {
                    // Aliasing — bail.
                    return None;
                }
                _ => {}
            }
        }
        None
    }
}

/// Helper: does this op write to a slot? Returns the dst slot if yes.
///
/// Phase 2 only needs this for the simple ops the matchers above
/// inspect. As more patterns get added that scan op streams, this
/// helper grows. Bulk ops (`MemoryFill` / `MemoryCopy`) and stores
/// do not write to a *slot* (they touch state directly), so they
/// return None.
fn op_writes_slot(op: &Op) -> Option<u32> {
    match op {
        Op::LoadLit { dst, .. }
        | Op::LoadSlot { dst, .. }
        | Op::LoadState { dst, .. }
        | Op::LoadMem { dst, .. }
        | Op::LoadMem16 { dst, .. }
        | Op::LoadPackedByte { dst, .. }
        | Op::Add { dst, .. }
        | Op::AddLit { dst, .. }
        | Op::Sub { dst, .. }
        | Op::SubLit { dst, .. }
        | Op::Mul { dst, .. }
        | Op::MulLit { dst, .. }
        | Op::Div { dst, .. }
        | Op::Mod { dst, .. }
        | Op::Neg { dst, .. }
        | Op::Abs { dst, .. }
        | Op::Sign { dst, .. }
        | Op::Pow { dst, .. }
        | Op::Min { dst, .. }
        | Op::Max { dst, .. }
        | Op::Clamp { dst, .. }
        | Op::Round { dst, .. }
        | Op::Floor { dst, .. }
        | Op::And { dst, .. }
        | Op::AndLit { dst, .. }
        | Op::Shr { dst, .. }
        | Op::ShrLit { dst, .. }
        | Op::Shl { dst, .. }
        | Op::ShlLit { dst, .. }
        | Op::ModLit { dst, .. }
        | Op::Bit { dst, .. }
        | Op::BitAnd16 { dst, .. }
        | Op::BitOr16 { dst, .. }
        | Op::BitXor16 { dst, .. }
        | Op::BitNot16 { dst, .. }
        | Op::CmpEq { dst, .. }
        | Op::LoadStateAndBranchIfNotEqLit { dst, .. }
        | Op::Dispatch { dst, .. }
        | Op::DispatchFlatArray { dst, .. }
        | Op::Call { dst, .. } => Some(*dst),
        Op::BranchIfZero { .. }
        | Op::BranchIfNotEqLit { .. }
        | Op::Jump { .. }
        | Op::DispatchChain { .. }
        | Op::StoreState { .. }
        | Op::StoreMem { .. }
        | Op::MemoryFill { .. }
        | Op::MemoryCopy { .. }
        | Op::ReplicatedBody { .. } => None,
    }
}

/// The Phase 2 default pattern set. Order matters — earlier patterns
/// claim a PC first; later ones see only un-claimed PCs.
///
/// We list the most specific patterns first (BitField — two ops with
/// strict shape), then the shortest-effective ones (LitMerge — two
/// adjacent same-kind lits), then the scanning patterns (Hold,
/// RepeatedLoad — look at multiple ops). This keeps the simpler
/// matchers from accidentally claiming ops that would have made
/// stronger annotations.
pub fn phase2_patterns() -> Vec<Box<dyn Pattern>> {
    vec![
        Box::new(BitFieldMatch),
        Box::new(LitMerge::add()),
        Box::new(LitMerge::sub()),
        Box::new(LitMerge::mul()),
        Box::new(LitMerge::shl()),
        Box::new(LitMerge::shr()),
        Box::new(LitMerge::and()),
        Box::new(HoldMatch),
        Box::new(RepeatedLoadMatch),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::{CompiledProgram, Op};
    use crate::dag::{build_dag, normalise_with};

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

    #[test]
    fn const_fold_add_lit_chain() {
        let mut p = empty_program();
        p.ops = vec![
            Op::LoadLit { dst: 0, val: 100 },
            Op::AddLit { dst: 1, a: 0, val: 5 },
            Op::AddLit { dst: 1, a: 1, val: 7 },
        ];
        p.slot_count = 2;
        let dag = build_dag(&p);
        let norm = normalise_with(&p, &dag, &phase2_patterns());
        let ann = norm.annotation_at(1).expect("AddLit chain should fold");
        match ann.kind {
            IdiomKind::ConstFold { merged_lit } => assert_eq!(merged_lit, 12),
            _ => panic!("wrong idiom kind: {:?}", ann.kind),
        }
        assert_eq!(ann.span, 2);
    }

    #[test]
    fn bitfield_shr_and() {
        let mut p = empty_program();
        p.ops = vec![
            Op::LoadLit { dst: 0, val: 0xABCD },
            Op::ShrLit { dst: 1, a: 0, val: 8 },
            Op::AndLit { dst: 2, a: 1, val: 0xFF }, // mask: low 8 bits
        ];
        p.slot_count = 3;
        let dag = build_dag(&p);
        let norm = normalise_with(&p, &dag, &phase2_patterns());
        let ann = norm.annotation_at(1).expect("ShrLit+AndLit should fold");
        match ann.kind {
            IdiomKind::BitField { shift, mask } => {
                assert_eq!(shift, 8);
                assert_eq!(mask, 0xFF);
            }
            _ => panic!("wrong idiom kind: {:?}", ann.kind),
        }
    }

    #[test]
    fn hold_load_store_pair() {
        let mut p = empty_program();
        p.ops = vec![
            Op::LoadState { dst: 0, addr: 42 },
            Op::StoreState { addr: 42, src: 0 },
        ];
        p.slot_count = 1;
        let dag = build_dag(&p);
        let norm = normalise_with(&p, &dag, &phase2_patterns());
        let ann = norm.annotation_at(0).expect("Load+Store same addr should be Hold");
        assert!(matches!(ann.kind, IdiomKind::Hold));
        assert_eq!(ann.span, 2);
    }

    #[test]
    fn hold_does_not_match_when_load_value_modified() {
        let mut p = empty_program();
        p.ops = vec![
            Op::LoadState { dst: 0, addr: 42 },
            Op::AddLit { dst: 0, a: 0, val: 1 }, // mutates dst → not a hold
            Op::StoreState { addr: 42, src: 0 },
        ];
        p.slot_count = 1;
        let dag = build_dag(&p);
        let norm = normalise_with(&p, &dag, &phase2_patterns());
        // Load may still be claimed by another pattern, but it must not
        // be a Hold annotation.
        match norm.annotation_at(0) {
            Some(a) => assert!(!matches!(a.kind, IdiomKind::Hold)),
            None => {}
        }
    }

    #[test]
    fn repeated_load_match() {
        let mut p = empty_program();
        p.ops = vec![
            Op::LoadState { dst: 0, addr: 100 },
            Op::AddLit { dst: 1, a: 0, val: 5 }, // use of slot 0
            Op::LoadState { dst: 2, addr: 100 }, // redundant — value at slot 0 is still valid
        ];
        p.slot_count = 3;
        let dag = build_dag(&p);
        let norm = normalise_with(&p, &dag, &phase2_patterns());
        let ann = norm.annotation_at(2).expect("redundant LoadState should be CSE-annotated");
        match ann.kind {
            IdiomKind::RepeatedLoad { prior_dst } => assert_eq!(prior_dst, 0),
            _ => panic!("wrong idiom kind: {:?}", ann.kind),
        }
    }

    #[test]
    fn repeated_load_invalidated_by_intervening_store() {
        let mut p = empty_program();
        p.ops = vec![
            Op::LoadState { dst: 0, addr: 100 },
            Op::LoadLit { dst: 1, val: 99 },
            Op::StoreState { addr: 100, src: 1 }, // memory at addr 100 changed
            Op::LoadState { dst: 2, addr: 100 },  // not redundant any more
        ];
        p.slot_count = 3;
        let dag = build_dag(&p);
        let norm = normalise_with(&p, &dag, &phase2_patterns());
        // The second LoadState must NOT be annotated as RepeatedLoad.
        match norm.annotation_at(3) {
            Some(a) => assert!(!matches!(a.kind, IdiomKind::RepeatedLoad { .. })),
            None => {}
        }
    }

    #[test]
    fn shl_lit_chain_folds() {
        let mut p = empty_program();
        p.ops = vec![
            Op::LoadLit { dst: 0, val: 1 },
            Op::ShlLit { dst: 1, a: 0, val: 4 },
            Op::ShlLit { dst: 1, a: 1, val: 4 },
        ];
        p.slot_count = 2;
        let dag = build_dag(&p);
        let norm = normalise_with(&p, &dag, &phase2_patterns());
        let ann = norm.annotation_at(1).expect("ShlLit chain should fold");
        match ann.kind {
            IdiomKind::ConstFold { merged_lit } => assert_eq!(merged_lit, 8),
            _ => panic!("wrong idiom kind: {:?}", ann.kind),
        }
    }

    #[test]
    fn pattern_set_is_cardinal_rule_clean() {
        // Genericity probe (smoke): every pattern in the set must
        // match purely on Op shape, never on specific addr/val
        // payloads. We cannot enforce this at compile time, so this
        // test instead asserts that swapping all literal values in a
        // matched program produces the same annotation count — a
        // sanity check that no pattern is gated on a specific number.
        let make = |vals: [i32; 4]| -> CompiledProgram {
            let mut p = empty_program();
            p.ops = vec![
                Op::LoadLit { dst: 0, val: vals[0] },
                Op::AddLit { dst: 1, a: 0, val: vals[1] },
                Op::AddLit { dst: 1, a: 1, val: 7 },
                Op::ShrLit { dst: 2, a: 1, val: vals[2] },
                Op::AndLit { dst: 3, a: 2, val: vals[3] },
            ];
            p.slot_count = 4;
            p
        };
        let p1 = make([1, 5, 2, 4]);
        let p2 = make([999, 13, 3, 5]);
        let dag1 = build_dag(&p1);
        let dag2 = build_dag(&p2);
        let n1 = normalise_with(&p1, &dag1, &phase2_patterns()).count();
        let n2 = normalise_with(&p2, &dag2, &phase2_patterns()).count();
        assert_eq!(n1, n2, "annotation count varies with literal payload — pattern is overfit");
    }
}
