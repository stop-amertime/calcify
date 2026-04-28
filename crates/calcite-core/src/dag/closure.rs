//! Phase 3 prototype — option (c): lower the DAG to a tree of Rust
//! closures.
//!
//! Per `compiler-mission.md` § Phase 3, this is the one-day prototype
//! that validates the lowering shape before we commit to wasm codegen
//! (option (a)). Each block becomes a `BlockPlan` holding:
//!
//! - a `Vec<OpClosure>` — one closure per Op in the block's body,
//! - a `TerminatorPlan` — pre-resolved branch / chain descriptor.
//!
//! Each `OpClosure` is `Box<dyn Fn(&mut State, &mut [i32]) + Send + Sync>`
//! that runs that Op's semantics inline in Rust, with operands captured
//! by-value at build time. The walker just iterates closures, then
//! consults the terminator. There is no `match` on `Op` on the hot path
//! — that's the entire point of option (c) vs `Backend::Bytecode`.
//!
//! Coverage: this prototype lowers the small numerical Ops directly
//! (LoadLit, AddLit, ShrLit, AndLit, etc.). Anything not in the
//! per-op `lower` table falls through to a generic "run via
//! `exec_ops`" closure that calls the existing bytecode evaluator on
//! a single-op slice. That preserves correctness for the long tail
//! while measuring the lowering shape on the hot path.
//!
//! Cardinal rule: this module knows nothing CSS-DOS-specific. Every
//! lowering reasons about Op vocabulary alone — same Ops, same
//! closures, regardless of cabinet.

use std::ptr::NonNull;

use crate::compile::{exec_ops, CompiledProgram, Op};
use crate::state::State;

use super::types::{BlockId, BranchKind, Dag, Terminator};

type OpClosure = Box<dyn Fn(&mut State, &mut [i32]) + Send + Sync>;

/// Pointer wrapper carrying the SAFETY contract that the source
/// `CompiledProgram` outlives the `ClosureProgram`. Evaluator owns
/// both — the program in its `compiled` field, the closure tree in
/// its `closure` field — and never moves the program after building
/// the closure tree. That is the contract.
///
/// Using a raw pointer rather than `&'a CompiledProgram` avoids
/// threading a lifetime parameter through every Backend variant. We
/// pay for that with one `unsafe` block per closure that needs the
/// program; the unsafe is sound because the closure tree never
/// outlives the Evaluator that owns the program.
#[derive(Copy, Clone)]
struct ProgRef(NonNull<CompiledProgram>);

// SAFETY: We hand `ProgRef` to closures that will be called from any
// thread the user's `Evaluator` is on. The pointed-to `CompiledProgram`
// is shared read-only; concurrent readers are sound.
unsafe impl Send for ProgRef {}
unsafe impl Sync for ProgRef {}

impl ProgRef {
    fn new(p: &CompiledProgram) -> Self {
        // SAFETY: a reference is never null.
        ProgRef(unsafe { NonNull::new_unchecked(p as *const _ as *mut _) })
    }

    /// SAFETY: caller asserts the source `CompiledProgram` is still
    /// alive (i.e. the owning `Evaluator` hasn't been dropped).
    unsafe fn get<'a>(&self) -> &'a CompiledProgram {
        &*self.0.as_ptr()
    }
}

/// Pre-resolved terminator. Mirrors `dag::types::Terminator` but with
/// any closure-side data the walker needs to decide the next block
/// without re-reading the original op.
enum TerminatorPlan {
    Halt,
    Goto(BlockId),
    /// Branch on slot[cond] == 0 (BranchIfZero).
    IfZero { cond: u32, taken: BlockId, fallthru: BlockId },
    /// Branch on slot[a] != val (BranchIfNotEqLit, LoadStateIfNotEqLit).
    /// LoadStateIfNotEqLit's load side-effect is performed by an
    /// OpClosure earlier in the block body, so by the time the walker
    /// gets here slot[a] already holds the loaded value.
    IfSlotNeLit { a: u32, val: i32, taken: BlockId, fallthru: BlockId },
    /// Multi-way dispatch via chain table. `program` and `dag` are
    /// looked up at decision time because flat tables are large
    /// shared structures.
    DispatchChain { a: u32, chain_id: u32, miss: BlockId },
}

struct BlockPlan {
    ops: Vec<OpClosure>,
    term: TerminatorPlan,
}

/// Lowered DAG: one `BlockPlan` per `Dag` block.
///
/// Holds a `ProgRef` raw-pointer to the `CompiledProgram` that was
/// passed to `build`. SAFETY contract: the `CompiledProgram` must
/// outlive the `ClosureProgram`. `Evaluator` enforces this by owning
/// both fields and never reassigning `compiled` after `closure` is
/// built.
pub struct ClosureProgram {
    program: ProgRef,
    blocks: Vec<BlockPlan>,
    leader_to_block: Vec<(u32, BlockId)>,
}

impl ClosureProgram {
    /// Lower a `Dag` over `program` into a closure tree.
    ///
    /// SAFETY: the caller must guarantee that `program` outlives the
    /// returned `ClosureProgram`. In practice this means storing both
    /// in the same struct (Evaluator) and never moving the program.
    pub fn build(program: &CompiledProgram, dag: &Dag) -> Self {
        let prog_ref = ProgRef::new(program);
        let mut blocks = Vec::with_capacity(dag.blocks.len());
        for block in &dag.blocks {
            let body_lo = block.pc_lo as usize;
            let body_hi = block.pc_hi as usize;

            let mut op_closures: Vec<OpClosure> = Vec::with_capacity(body_hi - body_lo);

            // Determine where the body proper ends. Terminator ops with
            // side-effects (LoadStateAndBranchIfNotEqLit, MemoryFill,
            // MemoryCopy) have to keep their op in the lowered body so
            // their state mutation runs. Pure-CFG terminators (Jump,
            // BranchIfZero, BranchIfNotEqLit, DispatchChain) don't —
            // we extract the operands directly into the TerminatorPlan
            // and skip the op.
            let body_end = match &block.term {
                Terminator::Branch { kind: BranchKind::LoadStateIfNotEqLit, .. } => body_hi,
                Terminator::BulkExit(_) => body_hi,
                Terminator::Halt | Terminator::Fall(_) => body_hi,
                Terminator::Jump(_)
                | Terminator::Branch { kind: BranchKind::IfZero, .. }
                | Terminator::Branch { kind: BranchKind::IfNotEqLit, .. }
                | Terminator::DispatchChain { .. } => {
                    // Skip the terminator op — it's encoded in TerminatorPlan.
                    body_hi.saturating_sub(1)
                }
            };

            for pc in body_lo..body_end {
                op_closures.push(lower_op(&program.ops[pc], prog_ref));
            }

            let term = lower_terminator(&block.term, program, block.pc_hi);
            blocks.push(BlockPlan { ops: op_closures, term });
        }

        ClosureProgram {
            leader_to_block: dag.leader_to_block.clone(),
            program: prog_ref,
            blocks,
        }
    }

    /// Number of blocks (diagnostic).
    #[allow(dead_code)]
    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    fn pc_to_block(&self, pc: u32) -> BlockId {
        match self.leader_to_block.binary_search_by_key(&pc, |&(p, _)| p) {
            Ok(i) => self.leader_to_block[i].1,
            Err(_) => (self.blocks.len() as BlockId).saturating_sub(1),
        }
    }

    /// Drive one tick. Mirrors `dag::dag_execute` but uses the closure
    /// tree for the main-ops phase; post-main-ops phases are handled
    /// by the caller (eval.rs) so this stays focussed on the prototype's
    /// concern.
    pub fn run_main_ops(&self, state: &mut State, slots: &mut [i32]) {
        let mut block_id: BlockId = 0;
        let max_steps = (self.blocks.len() as u64).saturating_mul(2).saturating_add(16);
        let mut steps: u64 = 0;
        loop {
            let block = &self.blocks[block_id as usize];
            for op in &block.ops {
                op(state, slots);
            }
            steps += 1;
            debug_assert!(
                steps < max_steps,
                "Closure walker exceeded step bound — possible cycle in main-ops CFG"
            );

            block_id = match &block.term {
                TerminatorPlan::Halt => return,
                TerminatorPlan::Goto(b) => *b,
                TerminatorPlan::IfZero { cond, taken, fallthru } => {
                    if slots[*cond as usize] == 0 { *taken } else { *fallthru }
                }
                TerminatorPlan::IfSlotNeLit { a, val, taken, fallthru } => {
                    if slots[*a as usize] != *val { *taken } else { *fallthru }
                }
                TerminatorPlan::DispatchChain { a, chain_id, miss } => {
                    let key = slots[*a as usize];
                    // SAFETY: source CompiledProgram outlives self by
                    // the build-time contract.
                    let prog = unsafe { self.program.get() };
                    let table = &prog.chain_tables[*chain_id as usize];
                    if let Some(flat) = &table.flat_table {
                        let idx = key.wrapping_sub(flat.base);
                        if (idx as u32) < flat.targets.len() as u32 {
                            let t = flat.targets[idx as usize];
                            if t != u32::MAX {
                                self.pc_to_block(t)
                            } else {
                                *miss
                            }
                        } else {
                            *miss
                        }
                    } else {
                        match table.entries.get(&key) {
                            Some(&body_pc) => self.pc_to_block(body_pc),
                            None => *miss,
                        }
                    }
                }
            };
        }
    }
}

/// Translate a single `Op` into a closure that runs its semantics
/// against `(state, slots)`. The closure captures every operand it
/// needs by value, plus a `ProgRef` for ops that consult shared
/// tables (Dispatch, DispatchFlatArray, Call, packed reads).
///
/// For ops not yet specialised, the fallback is a closure that calls
/// `exec_ops` over a single-op slice — same observable semantics as
/// the bytecode interpreter, just one closure call instead of one
/// dispatch through the big match.
fn lower_op(op: &Op, program: ProgRef) -> OpClosure {
    match *op {
        Op::LoadLit { dst, val } => Box::new(move |_state, slots| {
            slots[dst as usize] = val;
        }),
        Op::LoadSlot { dst, src } => Box::new(move |_state, slots| {
            slots[dst as usize] = slots[src as usize];
        }),
        Op::LoadState { dst, addr } => Box::new(move |state, slots| {
            slots[dst as usize] = state.read_mem(addr);
        }),
        Op::Add { dst, a, b } => Box::new(move |_state, slots| {
            slots[dst as usize] = slots[a as usize].wrapping_add(slots[b as usize]);
        }),
        Op::AddLit { dst, a, val } => Box::new(move |_state, slots| {
            slots[dst as usize] = slots[a as usize].wrapping_add(val);
        }),
        Op::Sub { dst, a, b } => Box::new(move |_state, slots| {
            slots[dst as usize] = slots[a as usize].wrapping_sub(slots[b as usize]);
        }),
        Op::SubLit { dst, a, val } => Box::new(move |_state, slots| {
            slots[dst as usize] = slots[a as usize].wrapping_sub(val);
        }),
        Op::MulLit { dst, a, val } => Box::new(move |_state, slots| {
            slots[dst as usize] = slots[a as usize].wrapping_mul(val);
        }),
        // AndLit's val is the literal mask bits, applied as `slot[a] & val`.
        Op::AndLit { dst, a, val } => Box::new(move |_state, slots| {
            slots[dst as usize] = slots[a as usize] & val;
        }),
        // ShrLit / ShlLit use signed i32 shifts. val is the shift count.
        Op::ShrLit { dst, a, val } => Box::new(move |_state, slots| {
            slots[dst as usize] = slots[a as usize] >> val;
        }),
        Op::ShlLit { dst, a, val } => Box::new(move |_state, slots| {
            slots[dst as usize] = slots[a as usize] << val;
        }),
        // Anything else: defer to the bytecode interpreter on a one-op
        // slice. Same semantics, one closure call's worth of indirection.
        // Phase 3 main (option a) will lower these to wasm directly;
        // the prototype just needs to prove the shape works.
        _ => {
            let owned_op = op.clone();
            Box::new(move |state, slots| {
                let single = std::slice::from_ref(&owned_op);
                // SAFETY: source CompiledProgram outlives self by the
                // build-time contract.
                let prog = unsafe { program.get() };
                exec_ops(
                    single,
                    &prog.dispatch_tables,
                    &prog.chain_tables,
                    &prog.flat_dispatch_arrays,
                    &prog.functions,
                    &prog.packed_cell_tables,
                    &prog.packed_exception_tables,
                    state,
                    slots,
                );
            })
        }
    }
}

/// Map a `Terminator` from the DAG layer into a `TerminatorPlan`.
/// Operands are pre-resolved here (slot indices, target block ids)
/// so the walker doesn't have to re-read program.ops on the hot path.
fn lower_terminator(
    term: &Terminator,
    program: &CompiledProgram,
    pc_hi: u32,
) -> TerminatorPlan {
    match term {
        Terminator::Halt => TerminatorPlan::Halt,
        Terminator::Fall(b) | Terminator::Jump(b) | Terminator::BulkExit(b) => {
            TerminatorPlan::Goto(*b)
        }
        Terminator::Branch { kind, taken, fallthru } => {
            // The terminator op is the last op of the block — pc_hi - 1.
            let last_op = &program.ops[(pc_hi - 1) as usize];
            match (kind, last_op) {
                (BranchKind::IfZero, Op::BranchIfZero { cond, .. }) => TerminatorPlan::IfZero {
                    cond: *cond,
                    taken: *taken,
                    fallthru: *fallthru,
                },
                (BranchKind::IfNotEqLit, Op::BranchIfNotEqLit { a, val, .. }) => {
                    TerminatorPlan::IfSlotNeLit {
                        a: *a,
                        val: *val,
                        taken: *taken,
                        fallthru: *fallthru,
                    }
                }
                (
                    BranchKind::LoadStateIfNotEqLit,
                    Op::LoadStateAndBranchIfNotEqLit { dst, val, .. },
                ) => {
                    // The load was kept in the body (see body_end logic
                    // in build) — slot[dst] holds the loaded value by
                    // the time the walker reaches this terminator.
                    TerminatorPlan::IfSlotNeLit {
                        a: *dst,
                        val: *val,
                        taken: *taken,
                        fallthru: *fallthru,
                    }
                }
                _ => {
                    debug_assert!(false, "Branch kind / op mismatch in DAG terminator");
                    TerminatorPlan::Halt
                }
            }
        }
        Terminator::DispatchChain { miss, .. } => {
            let last_op = &program.ops[(pc_hi - 1) as usize];
            if let Op::DispatchChain { a, chain_id, .. } = last_op {
                TerminatorPlan::DispatchChain {
                    a: *a,
                    chain_id: *chain_id,
                    miss: *miss,
                }
            } else {
                debug_assert!(false, "DispatchChain terminator without DispatchChain op");
                TerminatorPlan::Halt
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::CompiledProgram;
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

    #[test]
    fn closure_runs_load_lit_and_add_lit() {
        let mut p = empty_program();
        p.ops = vec![
            Op::LoadLit { dst: 0, val: 100 },
            Op::AddLit { dst: 1, a: 0, val: 5 },
        ];
        p.slot_count = 2;
        let dag = build_dag(&p);
        let cp = ClosureProgram::build(&p, &dag);
        let mut state = State::default();
        let mut slots = vec![0i32; 2];
        cp.run_main_ops(&mut state, &mut slots);
        assert_eq!(slots[0], 100);
        assert_eq!(slots[1], 105);
    }

    #[test]
    fn closure_takes_branch_correctly() {
        let mut p = empty_program();
        p.ops = vec![
            Op::LoadLit { dst: 0, val: 0 },
            Op::BranchIfZero { cond: 0, target: 3 },
            Op::LoadLit { dst: 1, val: 99 }, // skipped
            Op::LoadLit { dst: 1, val: 7 },  // taken
        ];
        p.slot_count = 2;
        let dag = build_dag(&p);
        let cp = ClosureProgram::build(&p, &dag);
        let mut state = State::default();
        let mut slots = vec![0i32; 2];
        cp.run_main_ops(&mut state, &mut slots);
        assert_eq!(slots[1], 7);
    }

    #[test]
    fn closure_falls_through_when_branch_not_taken() {
        let mut p = empty_program();
        p.ops = vec![
            Op::LoadLit { dst: 0, val: 1 }, // non-zero
            Op::BranchIfZero { cond: 0, target: 3 },
            Op::LoadLit { dst: 1, val: 42 }, // executed
            Op::LoadLit { dst: 1, val: 7 },  // also executed (fall-through)
        ];
        p.slot_count = 2;
        let dag = build_dag(&p);
        let cp = ClosureProgram::build(&p, &dag);
        let mut state = State::default();
        let mut slots = vec![0i32; 2];
        cp.run_main_ops(&mut state, &mut slots);
        // Op order: branch not taken, fall through to pc=2 then pc=3.
        // Final slot[1] = 7.
        assert_eq!(slots[1], 7);
    }
}
