//! S1.1 diagnostic: classify every `Op::LoadSlot` in the static op
//! stream of a cabinet to decide whether copy-coalescing is worth
//! writing.
//!
//! Runs in 3 passes per basic block:
//!  1. Build a set of "ops that read each slot" (intra-block).
//!  2. For each `LoadSlot { dst, src }`, classify:
//!       trivial-copy: every reader of `dst` between this op and
//!         either the next write of `dst` or end-of-block can be
//!         re-pointed at `src` without changing semantics — i.e.
//!         `src` is not written between this op and that reader.
//!       live-range-conflict: a reader of `dst` exists whose run
//!         comes after a write to `src`. The copy is genuinely
//!         preserving the value across mutation.
//!       cross-block: the slot `dst` is read by an op whose PC is
//!         outside the LoadSlot's block (terminator successor, or a
//!         later block). Intra-block coalesce alone won't kill this
//!         LoadSlot — needs cross-block liveness.
//!       dead: nothing ever reads `dst` after the LoadSlot. The
//!         LoadSlot is itself dead code; eliminating it is even
//!         simpler than coalescing.
//!
//! Cross-block readers are detected with a one-time pass that records,
//! for every slot, whether any op anywhere in the program reads it. We
//! then approximate "reads dst in some other block" by checking
//! whether `dst` has readers outside the current block's body. This is
//! a conservative over-count of cross-block (any same-block reader of
//! a different LoadSlot to dst counts as cross-block here too), so
//! treat the cross-block bucket as an upper bound.

use std::collections::HashMap;
use std::path::PathBuf;

use calcite_core::Evaluator;
use calcite_core::compile::{CompiledProgram, Op, Slot};
use calcite_core::dag::build_dag;

fn doom_path() -> PathBuf {
    if let Ok(p) = std::env::var("CALCITE_DOOM_CSS") {
        return PathBuf::from(p);
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for ancestor in manifest.ancestors() {
        let c = ancestor.join("CSS-DOS").join("doom8088.css");
        if c.is_file() { return c; }
        if let Some(p) = ancestor.parent() {
            let c = p.join("CSS-DOS").join("doom8088.css");
            if c.is_file() { return c; }
        }
    }
    PathBuf::from("doom8088.css")
}

/// Append every slot this op reads to `out`. Order doesn't matter.
fn op_reads(op: &Op, out: &mut Vec<Slot>) {
    match op {
        Op::LoadLit { .. } | Op::LoadState { .. } => {}
        Op::LoadSlot { src, .. } => out.push(*src),
        Op::LoadMem { addr_slot, .. } | Op::LoadMem16 { addr_slot, .. } => {
            out.push(*addr_slot);
        }
        Op::LoadPackedByte { key_slot, .. } => out.push(*key_slot),

        Op::Add { a, b, .. }
        | Op::Sub { a, b, .. }
        | Op::Mul { a, b, .. }
        | Op::Div { a, b, .. }
        | Op::Mod { a, b, .. }
        | Op::And { a, b, .. }
        | Op::Shr { a, b, .. }
        | Op::Shl { a, b, .. }
        | Op::BitAnd16 { a, b, .. }
        | Op::BitOr16 { a, b, .. }
        | Op::BitXor16 { a, b, .. }
        | Op::CmpEq { a, b, .. } => {
            out.push(*a);
            out.push(*b);
        }
        Op::AddLit { a, .. }
        | Op::SubLit { a, .. }
        | Op::MulLit { a, .. }
        | Op::ModLit { a, .. }
        | Op::AndLit { a, .. }
        | Op::ShrLit { a, .. }
        | Op::ShlLit { a, .. }
        | Op::BitNot16 { a, .. } => out.push(*a),

        Op::Neg { src, .. } | Op::Abs { src, .. } | Op::Sign { src, .. } | Op::Floor { src, .. } => {
            out.push(*src);
        }

        Op::Pow { base, exp, .. } => {
            out.push(*base);
            out.push(*exp);
        }
        Op::Min { args, .. } | Op::Max { args, .. } => out.extend_from_slice(args),
        Op::Clamp { min, val, max, .. } => {
            out.push(*min);
            out.push(*val);
            out.push(*max);
        }
        Op::Round { val, interval, .. } => {
            out.push(*val);
            out.push(*interval);
        }
        Op::Bit { val, idx, .. } => {
            out.push(*val);
            out.push(*idx);
        }

        Op::BranchIfZero { cond, .. } => out.push(*cond),
        Op::BranchIfNotEqLit { a, .. } => out.push(*a),
        Op::LoadStateAndBranchIfNotEqLit { .. } => {}
        Op::Jump { .. } => {}

        Op::DispatchChain { a, .. } => out.push(*a),
        Op::Dispatch { key, table_id, .. } => {
            out.push(*key);
            out.push(*table_id);
        }
        Op::DispatchFlatArray { key, .. } => out.push(*key),
        Op::Call { arg_slots, .. } => out.extend_from_slice(arg_slots),

        Op::StoreState { src, .. } => out.push(*src),
        Op::StoreMem { addr_slot, src } => {
            out.push(*addr_slot);
            out.push(*src);
        }

        Op::MemoryFill { dst_slot, val_slot, count_slot, .. } => {
            out.push(*dst_slot);
            out.push(*val_slot);
            out.push(*count_slot);
        }
        Op::MemoryCopy { src_slot, dst_slot, count_slot, .. } => {
            out.push(*src_slot);
            out.push(*dst_slot);
            out.push(*count_slot);
        }
    }
}

/// Returns the slot this op writes (the "dst") if any. MemoryFill /
/// MemoryCopy don't have a single dst; treat as None for this probe.
fn op_writes(op: &Op) -> Option<Slot> {
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
        | Op::ModLit { dst, .. }
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
        | Op::Bit { dst, .. }
        | Op::BitAnd16 { dst, .. }
        | Op::BitOr16 { dst, .. }
        | Op::BitXor16 { dst, .. }
        | Op::BitNot16 { dst, .. }
        | Op::CmpEq { dst, .. }
        | Op::Dispatch { dst, .. }
        | Op::DispatchFlatArray { dst, .. }
        | Op::Call { dst, .. } => Some(*dst),
        Op::LoadStateAndBranchIfNotEqLit { dst, .. } => Some(*dst),
        _ => None,
    }
}

#[derive(Default, Debug)]
struct Counts {
    trivial_copy: u64,
    live_range_conflict: u64,
    cross_block: u64,
    dead: u64,
    /// LoadSlots whose dst==src (no-op copies). Counted separately.
    self_copy: u64,
}

fn main() {
    let path = doom_path();
    eprintln!("probe-loadslot-coalesce: cabinet = {}", path.display());
    let css = std::fs::read_to_string(&path).expect("read cabinet");
    let parsed = calcite_core::parser::parse_css(&css).expect("parse");
    eprintln!("Building evaluator (runs compile)...");
    let evaluator = Evaluator::from_parsed(&parsed);
    let program = evaluator.compiled();
    eprintln!(
        "  main ops: {}, slots: {}, chain tables: {}, dispatch tables: {}",
        program.ops.len(),
        program.slot_count,
        program.chain_tables.len(),
        program.dispatch_tables.len(),
    );

    eprintln!("Building DAG...");
    let dag = build_dag(program);
    eprintln!("  blocks: {}", dag.blocks.len());

    // Pass 0: per-slot, count total reader-ops across the whole main op
    // stream. This gives us the "are there ANY readers of slot X" map
    // we need for the cross-block / dead distinction.
    eprintln!("Counting global slot-reader presence (whole-program pass)...");
    let mut total_reads_per_slot: Vec<u32> = vec![0; program.slot_count as usize];
    let mut buf = Vec::with_capacity(8);
    for op in &program.ops {
        buf.clear();
        op_reads(op, &mut buf);
        for &s in &buf {
            if (s as usize) < total_reads_per_slot.len() {
                total_reads_per_slot[s as usize] = total_reads_per_slot[s as usize].saturating_add(1);
            }
        }
    }

    // For every block, decrement local-reader counts so we can ask
    // "does this slot have any reader OUTSIDE this block?". To avoid
    // an O(blocks * slots) materialisation we instead use total_reads
    // as an upper bound and, per LoadSlot, count local readers within
    // the same block — if local count == total count then no external
    // reader. This is exact, not approximate.

    eprintln!("Classifying every Op::LoadSlot in every block...");
    let mut counts = Counts::default();
    let mut block_local_reads: HashMap<Slot, u32> = HashMap::new();
    let n = dag.blocks.len() - 1; // last block is virtual halt
    let mut report_every = (n / 20).max(1);
    for (bi, block) in dag.blocks[..n].iter().enumerate() {
        // Build per-block local reader count for slots that appear as
        // dst of any LoadSlot in this block (only those need external-
        // reader info). If no LoadSlot in block, skip.
        let pc_lo = block.pc_lo;
        let pc_hi = block.pc_hi;
        let body = &program.ops[pc_lo as usize..pc_hi as usize];
        let mut has_loadslot = false;
        for op in body {
            if matches!(op, Op::LoadSlot { .. }) {
                has_loadslot = true;
                break;
            }
        }
        if !has_loadslot {
            continue;
        }
        block_local_reads.clear();
        for op in body {
            buf.clear();
            op_reads(op, &mut buf);
            for &s in &buf {
                *block_local_reads.entry(s).or_insert(0) += 1;
            }
        }
        // For each slot in this block, has_external_reader[s] iff total
        // reads > local reads.
        // We materialise only the slots we need.
        let mut external_for_slot: HashMap<Slot, bool> = HashMap::new();
        for (&s, &local) in &block_local_reads {
            let total = total_reads_per_slot.get(s as usize).copied().unwrap_or(0);
            external_for_slot.insert(s, total > local);
        }
        // Also need entries for dst slots that have ZERO local readers
        // (e.g. dead within block). For those, external iff total > 0.
        // We collect dsts up-front:
        for op in body {
            if let Op::LoadSlot { dst, .. } = op {
                external_for_slot.entry(*dst).or_insert_with(|| {
                    total_reads_per_slot.get(*dst as usize).copied().unwrap_or(0) > 0
                });
            }
        }

        // Build a small slot-indexed view that classify_block can probe.
        // For simplicity, build a Vec<bool> sized to slot_count is too big
        // (399K). Use a HashMap-based wrapper instead.
        let has_external = HasExternalReader { map: &external_for_slot };
        classify_block_v2(program, pc_lo, pc_hi, &has_external, &mut counts, &mut buf);

        if bi % report_every == 0 && bi > 0 {
            eprintln!(
                "  ...block {}/{} ({}% done)",
                bi,
                n,
                bi * 100 / n
            );
            report_every = report_every.max(1);
        }
    }

    let total = counts.trivial_copy
        + counts.live_range_conflict
        + counts.cross_block
        + counts.dead
        + counts.self_copy;
    eprintln!();
    eprintln!("=== LoadSlot classification (doom8088) ===");
    eprintln!("  total LoadSlot ops:         {}", total);
    let pct = |v: u64| -> f64 {
        if total == 0 { 0.0 } else { v as f64 * 100.0 / total as f64 }
    };
    eprintln!("    trivial copy:             {:>10}  ({:>5.1} %)", counts.trivial_copy, pct(counts.trivial_copy));
    eprintln!("    dead (no reader at all):  {:>10}  ({:>5.1} %)", counts.dead, pct(counts.dead));
    eprintln!("    cross-block:              {:>10}  ({:>5.1} %)", counts.cross_block, pct(counts.cross_block));
    eprintln!("    live-range conflict:      {:>10}  ({:>5.1} %)", counts.live_range_conflict, pct(counts.live_range_conflict));
    eprintln!("    self-copy (dst==src):     {:>10}  ({:>5.1} %)", counts.self_copy, pct(counts.self_copy));
    eprintln!();
    let intra_killable = counts.trivial_copy + counts.dead + counts.self_copy;
    eprintln!(
        "  intra-block-killable (trivial+dead+self): {} ({:.1} % of LoadSlot, {:.2} % of all ops)",
        intra_killable,
        pct(intra_killable),
        intra_killable as f64 * 100.0 / program.ops.len() as f64,
    );
}

struct HasExternalReader<'a> {
    map: &'a HashMap<Slot, bool>,
}

impl<'a> HasExternalReader<'a> {
    fn get(&self, s: Slot) -> bool {
        *self.map.get(&s).unwrap_or(&false)
    }
}

fn classify_block_v2(
    program: &CompiledProgram,
    pc_lo: u32,
    pc_hi: u32,
    has_external: &HasExternalReader,
    counts: &mut Counts,
    reader_buf: &mut Vec<Slot>,
) {
    let ops = &program.ops;
    for i in pc_lo..pc_hi {
        let (dst, src) = match &ops[i as usize] {
            Op::LoadSlot { dst, src } => (*dst, *src),
            _ => continue,
        };
        if dst == src {
            counts.self_copy += 1;
            continue;
        }
        let mut any_reader_in_window = false;
        let mut reader_after_src_write = false;
        let mut src_written = false;
        let mut j = i + 1;
        while j < pc_hi {
            let op = &ops[j as usize];
            reader_buf.clear();
            op_reads(op, reader_buf);
            if reader_buf.contains(&dst) {
                any_reader_in_window = true;
                if src_written {
                    reader_after_src_write = true;
                }
            }
            if let Some(w) = op_writes(op) {
                if w == src {
                    src_written = true;
                }
                if w == dst {
                    break;
                }
            }
            j += 1;
        }

        let external = has_external.get(dst);

        if reader_after_src_write {
            counts.live_range_conflict += 1;
        } else if any_reader_in_window {
            if external {
                counts.cross_block += 1;
            } else {
                counts.trivial_copy += 1;
            }
        } else if external {
            counts.cross_block += 1;
        } else {
            counts.dead += 1;
        }
    }
}
