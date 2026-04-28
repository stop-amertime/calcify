//! Phase 1 DAG walker — alternative evaluation backend.
//!
//! Walks a `Dag` block-by-block. For each block:
//! 1. Hand the entire block body (including the terminator op) to
//!    `exec_ops`. Branch targets in the terminator point outside the
//!    block — they push `pc` past the slice length, causing `exec_ops`
//!    to exit cleanly. All slot/state side-effects are applied
//!    (notably `LoadStateAndBranchIfNotEqLit`'s state read, and
//!    `MemoryFill`/`MemoryCopy`'s bulk operation + slot adjustment).
//! 2. Inspect the block's `Terminator` and re-evaluate the branch
//!    condition (or chain-table lookup) against the now-updated slots
//!    to decide the next block.
//!
//! Same observable behaviour as `compile::execute`. Phase 2 will replace
//! step 1 with a per-DAG-node evaluator that doesn't go through the
//! bytecode dispatch table; the CFG infrastructure that step 2 builds on
//! is what's being proved sound here.

use crate::compile::{exec_ops, execute_post_main_phases, CompiledProgram, Op};
use crate::state::State;

use super::types::{BasicBlock, BlockId, BranchKind, Dag, Terminator};

/// Execute one tick of `program` against `state` via the DAG walker.
///
/// Mirror of `compile::execute`: the main-ops phase walks the DAG; every
/// post-main-ops phase (writeback, broadcast writes, packed broadcasts,
/// REP fast-forward) is identical because they don't operate on PCs.
/// We re-implement those phases inline rather than depend on a
/// `compile::execute_after_main_ops` helper that doesn't exist yet —
/// keeping the bytecode path's hot loop unchanged is more important than
/// dedup for Phase 1.
pub fn dag_execute(
    program: &CompiledProgram,
    dag: &Dag,
    state: &mut State,
    slots: &mut Vec<i32>,
) {
    if slots.len() != program.slot_count as usize {
        slots.resize(program.slot_count as usize, 0);
    }

    walk_main_ops(program, dag, state, slots);

    execute_post_main_phases(program, state, slots);
}

/// Drive the DAG: pick the entry block, run its body, consult the
/// terminator for the next block, repeat until Halt.
fn walk_main_ops(
    program: &CompiledProgram,
    dag: &Dag,
    state: &mut State,
    slots: &mut [i32],
) {
    let mut block_id: BlockId = dag.entry();
    // Visit each block at most a few times. The bytecode form has no
    // back-edges in the main op stream — every `if(style())` cascade
    // compiles to forward-only branches — so the CFG is a DAG and we
    // never re-enter a block. A generous bound catches regressions
    // (e.g. if Phase 2 introduces a back-edge by accident).
    let max_steps = (dag.blocks.len() as u64).saturating_mul(2).saturating_add(16);
    let mut steps: u64 = 0;

    loop {
        let block = dag.block(block_id);
        run_block_body(program, block, state, slots);

        steps += 1;
        debug_assert!(
            steps < max_steps,
            "DAG walker exceeded step bound — possible cycle in main-ops CFG"
        );

        block_id = match next_block(program, dag, block, slots) {
            Some(next) => next,
            None => return,
        };
    }
}

fn run_block_body(
    program: &CompiledProgram,
    block: &BasicBlock,
    state: &mut State,
    slots: &mut [i32],
) {
    let body = block.body(program);
    if body.is_empty() {
        return;
    }
    // Hand the body — terminator and all — to exec_ops. Branch targets
    // in the terminator are PCs into the original `program.ops` array;
    // when applied to this slice they exceed `body.len()`, which causes
    // exec_ops to exit. This is a sound way to apply the terminator's
    // data side-effects without re-implementing per-op semantics.
    exec_ops(
        body,
        &program.dispatch_tables,
        &program.chain_tables,
        &program.flat_dispatch_arrays,
        &program.functions,
        &program.packed_cell_tables,
        &program.packed_exception_tables,
        state,
        slots,
    );
}

fn next_block(
    program: &CompiledProgram,
    dag: &Dag,
    block: &BasicBlock,
    slots: &[i32],
) -> Option<BlockId> {
    match &block.term {
        Terminator::Halt => None,
        Terminator::Fall(b) | Terminator::Jump(b) | Terminator::BulkExit(b) => Some(*b),
        Terminator::Branch { kind, taken, fallthru } => {
            let last_op = &program.ops[(block.pc_hi - 1) as usize];
            if branch_taken(*kind, last_op, slots) {
                Some(*taken)
            } else {
                Some(*fallthru)
            }
        }
        Terminator::DispatchChain { miss, .. } => {
            let last_op = &program.ops[(block.pc_hi - 1) as usize];
            if let Op::DispatchChain { a, chain_id, .. } = last_op {
                let key = slots[*a as usize];
                let table = &program.chain_tables[*chain_id as usize];
                if let Some(ref flat) = table.flat_table {
                    let idx = key.wrapping_sub(flat.base);
                    if (idx as u32) < flat.targets.len() as u32 {
                        let t = flat.targets[idx as usize];
                        if t != u32::MAX {
                            return Some(pc_to_block(dag, t));
                        }
                    }
                    return Some(*miss);
                }
                match table.entries.get(&key) {
                    Some(&body_pc) => Some(pc_to_block(dag, body_pc)),
                    None => Some(*miss),
                }
            } else {
                debug_assert!(false, "DispatchChain terminator without DispatchChain op");
                Some(*miss)
            }
        }
    }
}

fn branch_taken(kind: BranchKind, op: &Op, slots: &[i32]) -> bool {
    match (kind, op) {
        (BranchKind::IfZero, Op::BranchIfZero { cond, .. }) => slots[*cond as usize] == 0,
        (BranchKind::IfNotEqLit, Op::BranchIfNotEqLit { a, val, .. }) => {
            slots[*a as usize] != *val
        }
        (
            BranchKind::LoadStateIfNotEqLit,
            Op::LoadStateAndBranchIfNotEqLit { dst, val, .. },
        ) => {
            // The load side-effect was applied by run_block_body — slot[dst]
            // now holds the loaded value. Comparing slot[dst] reproduces the
            // bytecode interpreter's `if v != *val` check exactly.
            slots[*dst as usize] != *val
        }
        _ => {
            debug_assert!(false, "Branch kind / op mismatch in DAG terminator");
            false
        }
    }
}

fn pc_to_block(dag: &Dag, pc: u32) -> BlockId {
    dag.block_for_pc(pc).unwrap_or_else(|| {
        debug_assert!(false, "PC {} is not a leader — DAG construction bug", pc);
        dag.blocks.len() as BlockId - 1
    })
}
