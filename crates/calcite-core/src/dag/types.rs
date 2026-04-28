//! Phase 1 DAG types — basic blocks, terminators, and the top-level `Dag`.
//!
//! See `docs/phase1-dag-ir.md` for the design rationale. The Phase 1 node
//! vocabulary mirrors `compile::Op` 1:1; the contribution of this module
//! is the explicit CFG (basic blocks + terminators) that lifts `Vec<Op>`
//! out of "linear bytecode with embedded jumps" into "DAG of blocks."
//! Phase 2 starts replacing per-op dispatch with rewritten DAG nodes.
//!
//! The body of each basic block is the original op slice (`pc_lo..pc_hi`
//! into `CompiledProgram::ops`). Phase 1's walker re-uses the same op
//! semantics as the bytecode interpreter — proving the CFG is sound is
//! the deliverable, not yet a different evaluation strategy.
//!
//! Terminator structure (Phase 1):
//! - `Fall(BlockId)`              — sequential fall-through, no branch op.
//! - `Jump(BlockId)`              — `Op::Jump` at end of block.
//! - `Branch{cond_kind, taken,
//!     fallthru}`                 — `Op::BranchIfZero` / `BranchIfNotEqLit`
//!                                   / `LoadStateAndBranchIfNotEqLit`.
//! - `DispatchChain{chain_id,
//!     entries, miss}`            — `Op::DispatchChain`. Successors = every
//!                                   distinct chain target + miss.
//! - `Halt`                       — block reaches end of `ops` with no
//!                                   terminator (the program exit).
//! - `BulkExit(BlockId)`          — `Op::MemoryFill` / `Op::MemoryCopy`
//!                                   tail-jumps to `exit_target`.
//!
//! `Op::Dispatch`, `Op::DispatchFlatArray`, `Op::Call` are NOT terminators —
//! they are pure-fallthrough super-ops with embedded sub-execution. They
//! stay inside their block.

use crate::compile::{CompiledProgram, Op};

/// Index into `Dag::blocks`.
pub type BlockId = u32;

/// Index of an op within `CompiledProgram::ops`. Same as the v1 `pc`.
pub type OpIndex = u32;

/// Conditional-branch flavour. The terminator op is the last op of the
/// block; we cache a small enum here so the walker doesn't have to
/// re-match on `Op` to know which condition to evaluate.
#[derive(Debug, Clone, Copy)]
pub enum BranchKind {
    /// `Op::BranchIfZero { cond, .. }` — branch taken iff `slot[cond] == 0`.
    IfZero,
    /// `Op::BranchIfNotEqLit { a, val, .. }` — branch taken iff `slot[a] != val`.
    IfNotEqLit,
    /// `Op::LoadStateAndBranchIfNotEqLit { dst, addr, val, .. }` — load then
    /// branch taken iff loaded value != val. The load side-effect is part of
    /// the op; the walker must execute it whether the branch is taken or not.
    LoadStateIfNotEqLit,
}

/// What happens at the end of a basic block.
#[derive(Debug, Clone)]
pub enum Terminator {
    /// Fall through to the next block in source order. No terminator op.
    Fall(BlockId),
    /// Unconditional jump (`Op::Jump`).
    Jump(BlockId),
    /// Two-way branch.
    Branch {
        kind: BranchKind,
        /// Block to enter when the branch is taken.
        taken: BlockId,
        /// Block to enter on fall-through.
        fallthru: BlockId,
    },
    /// `Op::DispatchChain` — multi-way jump.
    DispatchChain {
        /// Distinct successor blocks (one per chain entry; deduped).
        /// Stored for CFG-completeness; the walker uses the chain table
        /// directly via the terminator op's `chain_id`.
        successors: Vec<BlockId>,
        /// Block reached on chain miss.
        miss: BlockId,
    },
    /// `Op::MemoryFill` / `Op::MemoryCopy` — tail-jump to `exit_target`.
    BulkExit(BlockId),
    /// End of program (no terminator op; PC reaches `ops.len()`).
    Halt,
}

/// A maximal straight-line region of ops with a single entry.
#[derive(Debug, Clone)]
pub struct BasicBlock {
    /// Inclusive start PC into `CompiledProgram::ops`.
    pub pc_lo: OpIndex,
    /// Exclusive end PC. Block body is `ops[pc_lo..pc_hi]`.
    pub pc_hi: OpIndex,
    /// What flow does at the end of the block.
    pub term: Terminator,
}

impl BasicBlock {
    /// Returns the ops slice for this block's body, including the terminator
    /// op if any. The walker advances over the body op-by-op then consults
    /// `term` to decide the next block.
    pub fn body<'a>(&self, program: &'a CompiledProgram) -> &'a [Op] {
        &program.ops[self.pc_lo as usize..self.pc_hi as usize]
    }
}

/// The top-level DAG: a list of basic blocks with the entry block at index 0.
#[derive(Debug, Clone)]
pub struct Dag {
    pub blocks: Vec<BasicBlock>,
    /// Map from each leader PC to its block id. Used during construction
    /// and exposed for diagnostic dumps.
    pub leader_to_block: Vec<(OpIndex, BlockId)>,
}

impl Dag {
    pub fn entry(&self) -> BlockId {
        0
    }

    pub fn block(&self, id: BlockId) -> &BasicBlock {
        &self.blocks[id as usize]
    }

    /// Look up the block id for a leader PC. Returns None if the PC is not a
    /// leader (which would be a CFG-construction bug — every branch target
    /// must be a leader).
    pub fn block_for_pc(&self, pc: OpIndex) -> Option<BlockId> {
        self.leader_to_block
            .binary_search_by_key(&pc, |&(p, _)| p)
            .ok()
            .map(|i| self.leader_to_block[i].1)
    }
}
