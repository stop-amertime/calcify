//! Build a `Dag` from a `CompiledProgram`.
//!
//! Standard textbook CFG construction:
//!  1. Walk the op stream once to collect **leader** PCs. PC 0 is a leader;
//!     every branch/jump target is a leader; the PC immediately after every
//!     terminator op is a leader.
//!  2. Sort leaders, assign each a `BlockId` in PC order.
//!  3. For each block, find its terminator and resolve target PCs to
//!     `BlockId`s.
//!
//! `Op::Dispatch`, `Op::DispatchFlatArray`, `Op::Call` are not terminators —
//! they execute nested ops inline (against the same slot array) and fall
//! through. They stay inside the block; the walker delegates their nested
//! op execution to the same per-op evaluator the bytecode interpreter uses.

use crate::compile::{CompiledProgram, Op};

use super::types::*;

/// Returns true iff this op ends a basic block (i.e. changes `pc` away from
/// the linear `pc + 1` fall-through).
fn is_terminator(op: &Op) -> bool {
    matches!(
        op,
        Op::Jump { .. }
            | Op::BranchIfZero { .. }
            | Op::BranchIfNotEqLit { .. }
            | Op::LoadStateAndBranchIfNotEqLit { .. }
            | Op::DispatchChain { .. }
            | Op::MemoryFill { .. }
            | Op::MemoryCopy { .. }
    )
}

/// Append every PC this op transfers control to (excluding fall-through)
/// to `out`. `program` is needed for `DispatchChain` to read the chain
/// table's body PCs.
fn terminator_targets(op: &Op, program: &CompiledProgram, out: &mut Vec<u32>) {
    match op {
        Op::Jump { target } => out.push(*target),
        Op::BranchIfZero { target, .. }
        | Op::BranchIfNotEqLit { target, .. }
        | Op::LoadStateAndBranchIfNotEqLit { target, .. } => out.push(*target),
        Op::DispatchChain { chain_id, miss_target, .. } => {
            out.push(*miss_target);
            let table = &program.chain_tables[*chain_id as usize];
            // Both the HashMap and the optional flat-array view of the chain
            // table list the same body PCs; iterating the entries map covers
            // every distinct destination.
            for (_, body_pc) in table.entries.iter() {
                out.push(*body_pc);
            }
        }
        Op::MemoryFill { exit_target, .. } | Op::MemoryCopy { exit_target, .. } => {
            out.push(*exit_target)
        }
        _ => {}
    }
}

/// Build the DAG for a compiled program's main `ops` stream.
///
/// Function bodies (`CompiledProgram::functions[i].body_ops`) are NOT lifted
/// into the DAG in Phase 1 — they execute via the same nested `exec_ops`
/// path the bytecode interpreter uses. Phase 2 may bring them in once the
/// rewrites need cross-call visibility.
pub fn build_dag(program: &CompiledProgram) -> Dag {
    let ops = &program.ops;
    let n = ops.len() as u32;

    // 1. Collect leaders.
    let mut leader_set: Vec<bool> = vec![false; n as usize + 1];
    if n > 0 {
        leader_set[0] = true;
    }
    let mut tgt_buf = Vec::with_capacity(8);
    for (pc, op) in ops.iter().enumerate() {
        if is_terminator(op) {
            // PC after the terminator is a leader (for the fall-through edge
            // of conditional terminators, and as a hard cutoff for
            // unconditional ones — that PC will start the next block whether
            // or not anyone reaches it).
            let after = pc as u32 + 1;
            if (after as usize) <= n as usize {
                leader_set[after as usize] = true;
            }
            tgt_buf.clear();
            terminator_targets(op, program, &mut tgt_buf);
            for &t in &tgt_buf {
                if (t as usize) < n as usize {
                    leader_set[t as usize] = true;
                } else if t == n {
                    // Branch-to-end is legitimate — it acts as a Halt edge.
                    leader_set[n as usize] = true;
                } else {
                    // Out-of-range target. The bytecode interpreter would
                    // step past `ops.len()` and stop the loop. We model this
                    // by clamping to a synthetic Halt block at PC=n.
                    leader_set[n as usize] = true;
                }
            }
        }
    }

    // 2. Build the ordered leader list. We always include PC=n as a virtual
    //    "halt" leader so blocks have a bounded `pc_hi`.
    let mut leaders: Vec<u32> = (0..=n).filter(|&pc| leader_set[pc as usize]).collect();
    if leaders.is_empty() {
        // Empty program — emit a single Halt block with empty body.
        return Dag {
            blocks: vec![BasicBlock { pc_lo: 0, pc_hi: 0, term: Terminator::Halt }],
            leader_to_block: vec![(0, 0)],
        };
    }
    if *leaders.last().unwrap() != n {
        leaders.push(n);
    }

    // Map PC → BlockId. The trailing virtual leader (PC=n) is the Halt block
    // and gets the last id. Real blocks span leaders[i] .. leaders[i+1].
    let block_count = leaders.len() - 1; // last leader is the halt-only block
    let halt_block: BlockId = block_count as BlockId;

    let leader_to_block: Vec<(OpIndex, BlockId)> = leaders
        .iter()
        .enumerate()
        .map(|(i, &pc)| (pc, i as BlockId))
        .collect();

    let pc_to_block = |pc: u32| -> BlockId {
        if pc >= n {
            return halt_block;
        }
        // Binary search by leader pc.
        match leader_to_block.binary_search_by_key(&pc, |&(p, _)| p) {
            Ok(i) => leader_to_block[i].1,
            Err(_) => {
                // Shouldn't happen — every reachable PC was marked as a
                // leader above. If it does, return halt as a safety valve.
                halt_block
            }
        }
    };

    // 3. Build blocks.
    let mut blocks = Vec::with_capacity(block_count + 1);
    for i in 0..block_count {
        let pc_lo = leaders[i];
        let pc_hi = leaders[i + 1];

        // Find the terminator op (if any) — always the last op in the block,
        // because "PC after terminator" is itself a leader.
        let last_pc = pc_hi.saturating_sub(1);
        let last_op = if pc_lo < pc_hi { Some(&ops[last_pc as usize]) } else { None };

        let term = match last_op {
            Some(Op::Jump { target }) => Terminator::Jump(pc_to_block(*target)),
            Some(Op::BranchIfZero { target, .. }) => Terminator::Branch {
                kind: BranchKind::IfZero,
                taken: pc_to_block(*target),
                fallthru: pc_to_block(pc_hi),
            },
            Some(Op::BranchIfNotEqLit { target, .. }) => Terminator::Branch {
                kind: BranchKind::IfNotEqLit,
                taken: pc_to_block(*target),
                fallthru: pc_to_block(pc_hi),
            },
            Some(Op::LoadStateAndBranchIfNotEqLit { target, .. }) => Terminator::Branch {
                kind: BranchKind::LoadStateIfNotEqLit,
                taken: pc_to_block(*target),
                fallthru: pc_to_block(pc_hi),
            },
            Some(Op::DispatchChain { chain_id, miss_target, .. }) => {
                let table = &program.chain_tables[*chain_id as usize];
                let mut succs: Vec<BlockId> = table
                    .entries
                    .iter()
                    .map(|(_, body_pc)| pc_to_block(*body_pc))
                    .collect();
                succs.sort_unstable();
                succs.dedup();
                Terminator::DispatchChain {
                    successors: succs,
                    miss: pc_to_block(*miss_target),
                }
            }
            Some(Op::MemoryFill { exit_target, .. })
            | Some(Op::MemoryCopy { exit_target, .. }) => {
                Terminator::BulkExit(pc_to_block(*exit_target))
            }
            // Block ends at a leader (next iteration's pc_lo) without a
            // control-flow op — sequential fall-through.
            _ => Terminator::Fall(pc_to_block(pc_hi)),
        };

        blocks.push(BasicBlock { pc_lo, pc_hi, term });
    }

    // Halt block: empty body, no terminator. PC=n .. PC=n.
    blocks.push(BasicBlock { pc_lo: n, pc_hi: n, term: Terminator::Halt });

    Dag { blocks, leader_to_block }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn empty_program_has_one_halt_block() {
        let p = empty_program();
        let dag = build_dag(&p);
        assert_eq!(dag.blocks.len(), 1);
        assert!(matches!(dag.blocks[0].term, Terminator::Halt));
    }

    #[test]
    fn straight_line_program_one_block() {
        let mut p = empty_program();
        p.ops = vec![
            Op::LoadLit { dst: 0, val: 1 },
            Op::LoadLit { dst: 1, val: 2 },
            Op::Add { dst: 2, a: 0, b: 1 },
        ];
        p.slot_count = 3;
        let dag = build_dag(&p);
        // One real block + halt.
        assert_eq!(dag.blocks.len(), 2);
        assert_eq!(dag.blocks[0].pc_lo, 0);
        assert_eq!(dag.blocks[0].pc_hi, 3);
        assert!(matches!(dag.blocks[0].term, Terminator::Fall(_)));
    }

    #[test]
    fn jump_creates_terminator_block() {
        let mut p = empty_program();
        // pc 0: load
        // pc 1: jump to pc 3
        // pc 2: dead op (still gets its own block because pc 2 is leader-after-jump)
        // pc 3: load
        p.ops = vec![
            Op::LoadLit { dst: 0, val: 1 },
            Op::Jump { target: 3 },
            Op::LoadLit { dst: 1, val: 99 },
            Op::LoadLit { dst: 2, val: 7 },
        ];
        p.slot_count = 3;
        let dag = build_dag(&p);
        // Blocks: [0..2 jump], [2..3 dead-fall], [3..4 fall], halt
        assert_eq!(dag.blocks.len(), 4);
        assert!(matches!(dag.blocks[0].term, Terminator::Jump(_)));
    }

    #[test]
    fn conditional_branch_two_successors() {
        let mut p = empty_program();
        p.ops = vec![
            Op::LoadLit { dst: 0, val: 0 },
            Op::BranchIfZero { cond: 0, target: 3 },
            Op::LoadLit { dst: 1, val: 99 },
            Op::LoadLit { dst: 2, val: 7 },
        ];
        p.slot_count = 3;
        let dag = build_dag(&p);
        match &dag.blocks[0].term {
            Terminator::Branch { taken, fallthru, .. } => {
                assert_ne!(taken, fallthru);
            }
            _ => panic!("expected Branch terminator"),
        }
    }
}
