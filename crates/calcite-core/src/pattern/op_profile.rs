//! Runtime op-adjacency profiler.
//!
//! Counts (prev_kind, curr_kind) pairs for ops actually dispatched at
//! runtime — including inside dispatch entries, function bodies, and
//! broadcast-write value_ops. Used to find fusion candidates that survived
//! the existing peephole + chain passes.
//!
//! Static op-distribution numbers (e.g. "27% LoadSlot + 25% BranchIfNotEqLit
//! in main.ops") are misleading because the dominant runtime ops live in
//! dispatch sub-trees, not the main op array. This profiler measures what
//! actually executes.
//!
//! Usage:
//!   1. Call `op_profile_enable()` before the run.
//!   2. Run. The hot-loop in `compile::exec_ops` checks the thread-local
//!      flag once per `exec_ops` call (cheap when disabled).
//!   3. Call `op_profile_snapshot()` to read the matrix.
//!
//! Cost: ~10ns/op when enabled (op-kind classification + 2 array indices).
//! Zero allocation per op. When disabled, one TLS Cell::get() per
//! `exec_ops` call.

use std::cell::RefCell;
use crate::compile::Op;

/// All `Op` variants. Order is stable — column/row index in the adjacency
/// matrix corresponds 1:1 with this list. Adding a new variant requires
/// extending `op_kind_index` to match.
pub const OP_KIND_NAMES: &[&str] = &[
    "LoadLit",
    "LoadSlot",
    "LoadState",
    "LoadMem",
    "LoadMem16",
    "LoadPackedByte",
    "Add",
    "AddLit",
    "Sub",
    "SubLit",
    "Mul",
    "MulLit",
    "Div",
    "Mod",
    "ModLit",
    "Neg",
    "Abs",
    "Sign",
    "Pow",
    "Min",
    "Max",
    "Clamp",
    "Round",
    "Floor",
    "And",
    "AndLit",
    "Shr",
    "ShrLit",
    "Shl",
    "ShlLit",
    "Bit",
    "BitAnd16",
    "BitOr16",
    "BitXor16",
    "BitNot16",
    "CmpEq",
    "BranchIfZero",
    "BranchIfNotEqLit",
    "LoadStateAndBranchIfNotEqLit",
    "BranchIfNotEqLit2",
    "Jump",
    "DispatchChain",
    "Dispatch",
    "DispatchFlatArray",
    "Call",
    "StoreState",
    "StoreMem",
    "MemoryFill",
    "MemoryCopy",
    "ReplicatedBody",
];

pub const NUM_OP_KINDS: usize = OP_KIND_NAMES.len();

/// Map an `Op` to its kind index.
#[inline]
pub fn op_kind_index(op: &Op) -> u8 {
    match op {
        Op::LoadLit { .. } => 0,
        Op::LoadSlot { .. } => 1,
        Op::LoadState { .. } => 2,
        Op::LoadMem { .. } => 3,
        Op::LoadMem16 { .. } => 4,
        Op::LoadPackedByte { .. } => 5,
        Op::Add { .. } => 6,
        Op::AddLit { .. } => 7,
        Op::Sub { .. } => 8,
        Op::SubLit { .. } => 9,
        Op::Mul { .. } => 10,
        Op::MulLit { .. } => 11,
        Op::Div { .. } => 12,
        Op::Mod { .. } => 13,
        Op::ModLit { .. } => 14,
        Op::Neg { .. } => 15,
        Op::Abs { .. } => 16,
        Op::Sign { .. } => 17,
        Op::Pow { .. } => 18,
        Op::Min { .. } => 19,
        Op::Max { .. } => 20,
        Op::Clamp { .. } => 21,
        Op::Round { .. } => 22,
        Op::Floor { .. } => 23,
        Op::And { .. } => 24,
        Op::AndLit { .. } => 25,
        Op::Shr { .. } => 26,
        Op::ShrLit { .. } => 27,
        Op::Shl { .. } => 28,
        Op::ShlLit { .. } => 29,
        Op::Bit { .. } => 30,
        Op::BitAnd16 { .. } => 31,
        Op::BitOr16 { .. } => 32,
        Op::BitXor16 { .. } => 33,
        Op::BitNot16 { .. } => 34,
        Op::CmpEq { .. } => 35,
        Op::BranchIfZero { .. } => 36,
        Op::BranchIfNotEqLit { .. } => 37,
        Op::LoadStateAndBranchIfNotEqLit { .. } => 38,
        Op::BranchIfNotEqLit2 { .. } => 39,
        Op::Jump { .. } => 40,
        Op::DispatchChain { .. } => 41,
        Op::Dispatch { .. } => 42,
        Op::DispatchFlatArray { .. } => 43,
        Op::Call { .. } => 44,
        Op::StoreState { .. } => 45,
        Op::StoreMem { .. } => 46,
        Op::MemoryFill { .. } => 47,
        Op::MemoryCopy { .. } => 48,
        Op::ReplicatedBody { .. } => 49,
    }
}

/// Adjacency matrix + 1D histogram. Indexed by kind index.
#[derive(Clone, Debug)]
pub struct OpProfile {
    pub enabled: bool,
    /// `matrix[prev * N + curr]` — count of (prev_kind, curr_kind) pairs.
    pub matrix: Vec<u64>,
    /// `histogram[k]` — count of times kind k was executed.
    pub histogram: Vec<u64>,
    pub total_ops: u64,
    /// Sentinel value for "no prev op" (start of sequence).
    pub start_marker_count: u64,
}

impl Default for OpProfile {
    fn default() -> Self {
        Self {
            enabled: false,
            matrix: vec![0; NUM_OP_KINDS * NUM_OP_KINDS],
            histogram: vec![0; NUM_OP_KINDS],
            total_ops: 0,
            start_marker_count: 0,
        }
    }
}

impl OpProfile {
    pub fn record(&mut self, prev: Option<u8>, curr: u8) {
        self.total_ops += 1;
        self.histogram[curr as usize] += 1;
        match prev {
            Some(p) => {
                self.matrix[(p as usize) * NUM_OP_KINDS + (curr as usize)] += 1;
            }
            None => {
                self.start_marker_count += 1;
            }
        }
    }

    /// Top-k adjacency pairs by count, descending. Returns
    /// `(prev_kind_idx, curr_kind_idx, count)`.
    pub fn top_pairs(&self, k: usize) -> Vec<(u8, u8, u64)> {
        let mut pairs: Vec<(u8, u8, u64)> = Vec::with_capacity(NUM_OP_KINDS * NUM_OP_KINDS);
        for p in 0..NUM_OP_KINDS {
            for c in 0..NUM_OP_KINDS {
                let count = self.matrix[p * NUM_OP_KINDS + c];
                if count > 0 {
                    pairs.push((p as u8, c as u8, count));
                }
            }
        }
        pairs.sort_by_key(|(_, _, n)| std::cmp::Reverse(*n));
        pairs.truncate(k);
        pairs
    }

    pub fn report_summary(&self) -> String {
        if !self.enabled {
            return String::from("op-profile: disabled\n");
        }
        if self.total_ops == 0 {
            return String::from("op-profile: no ops recorded\n");
        }
        let mut out = String::new();
        out.push_str(&format!("op-profile: total ops dispatched = {}\n", self.total_ops));
        out.push_str("\n  top op-kinds (count, % of total):\n");
        let mut hist: Vec<(usize, u64)> = self.histogram.iter().copied().enumerate().collect();
        hist.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        for (idx, count) in hist.iter().take(15) {
            if *count == 0 { break; }
            let pct = (*count as f64) * 100.0 / (self.total_ops as f64);
            out.push_str(&format!("    {:32} {:>14}  ({:.2}%)\n", OP_KIND_NAMES[*idx], count, pct));
        }
        out.push_str("\n  top adjacency pairs (prev -> curr, count, % of total):\n");
        for (p, c, count) in self.top_pairs(30) {
            let pct = (count as f64) * 100.0 / (self.total_ops as f64);
            out.push_str(&format!(
                "    {:28} -> {:28} {:>14}  ({:.2}%)\n",
                OP_KIND_NAMES[p as usize],
                OP_KIND_NAMES[c as usize],
                count,
                pct,
            ));
        }
        out
    }

    pub fn write_csv(&self, path: &std::path::Path) -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::File::create(path)?;
        writeln!(f, "prev_kind,curr_kind,count")?;
        for p in 0..NUM_OP_KINDS {
            for c in 0..NUM_OP_KINDS {
                let count = self.matrix[p * NUM_OP_KINDS + c];
                if count > 0 {
                    writeln!(f, "{},{},{}", OP_KIND_NAMES[p], OP_KIND_NAMES[c], count)?;
                }
            }
        }
        writeln!(f)?;
        writeln!(f, "# histogram")?;
        writeln!(f, "kind,count")?;
        for k in 0..NUM_OP_KINDS {
            if self.histogram[k] > 0 {
                writeln!(f, "{},{}", OP_KIND_NAMES[k], self.histogram[k])?;
            }
        }
        Ok(())
    }
}

thread_local! {
    pub(crate) static OP_PROFILE: RefCell<OpProfile> = RefCell::new(OpProfile::default());
    /// Fast-check flag — read at the top of every `exec_ops` call. Cell, not
    /// RefCell, so the hot-path read is a single TLS load + Cell::get.
    pub(crate) static OP_PROFILE_ENABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Last-dispatched op kind. Persists across recursive `exec_ops` calls
    /// so adjacency captures pairs that cross dispatch-entry / function-body
    /// / broadcast-write boundaries. -1 means "no prev op yet".
    pub(crate) static LAST_OP_KIND: std::cell::Cell<i8> = const { std::cell::Cell::new(-1) };
}

pub fn op_profile_enable() {
    OP_PROFILE.with(|p| {
        let mut p = p.borrow_mut();
        *p = OpProfile::default();
        p.enabled = true;
    });
    OP_PROFILE_ENABLED.with(|f| f.set(true));
    LAST_OP_KIND.with(|k| k.set(-1));
}

pub fn op_profile_disable() {
    OP_PROFILE_ENABLED.with(|f| f.set(false));
    OP_PROFILE.with(|p| p.borrow_mut().enabled = false);
}

/// Mark the start of a new logical sequence (e.g. tick boundary). After
/// calling this, the next recorded op will count as a "start" rather than
/// adjacent to whatever previously ran.
pub fn op_profile_break_sequence() {
    LAST_OP_KIND.with(|k| k.set(-1));
}

/// Record one op-dispatch event. Reads/writes thread-local state.
#[inline]
pub fn op_profile_tick(curr: u8) {
    let prev = LAST_OP_KIND.with(|k| {
        let v = k.get();
        k.set(curr as i8);
        v
    });
    let prev_opt = if prev < 0 { None } else { Some(prev as u8) };
    OP_PROFILE.with(|p| p.borrow_mut().record(prev_opt, curr));
}

pub fn op_profile_snapshot() -> OpProfile {
    OP_PROFILE.with(|p| p.borrow().clone())
}

#[inline]
pub fn op_profile_is_enabled() -> bool {
    OP_PROFILE_ENABLED.with(|f| f.get())
}

