//! Phase 3 spike: function-pointer-per-node DAG executor.
//!
//! A measurement-only prototype: takes a built `Dag` and produces a
//! parallel `CompiledDag` whose `Vec<NodeHandler>` is one `fn` pointer
//! per node. Per-node `match`-on-enum dispatch is replaced by one
//! indirect call. Each handler is small and monomorphic, which lets
//! LLVM specialise; the whole thing tests whether eliminating the
//! enum-tag dispatch recovers a meaningful fraction of the
//! interpreter overhead before we commit to real wasm codegen.
//!
//! Per `docs/compiler-mission.md` Phase 3, the recommended sequence is
//! (c) Rust-closure prototype → (a) hand-emitted wasm. This module is
//! the (c) artefact. If it doesn't beat the v2 walker by enough to
//! justify wasm codegen — or if the lowering shape turns out to be
//! awkward — we redesign before sinking weeks into wasm.
//!
//! Cardinal rule: this module knows nothing about CSS-DOS, x86, or
//! cabinets. It walks `Dag` shapes — same vocabulary the v2 walker
//! consumes.

use std::collections::HashMap;

use crate::dag::types::{BitwiseKind, CalcKind, Dag, DagNode, NodeId, StyleCondNode, TickPosition};
use crate::dag::TRANSIENT_BASE;
use crate::state::State;
use crate::types::RoundStrategy;

/// Per-node handler. Returns the i32 value of the node.
///
/// Handlers receive the compiled program by reference so they can
/// recurse via `eval_node`. The `node_id` lets a handler look up its
/// own payload in the parallel `payloads` array.
type NodeHandler = fn(&CompiledDag, NodeId, &mut DagCtx<'_>, &State) -> i32;

/// Per-node payload. Variants mirror `DagNode` but flatten the data
/// into a representation each handler can read with one indirection.
#[derive(Clone)]
enum NodePayload {
    Lit(i32),
    LitStr,
    LoadVarCurrent { slot: i32 },
    LoadVarPrev { slot: i32 },
    LoadVarCurrentTransient { idx: u32 },
    LoadVarPrevTransient,
    Calc { op: CalcKind, args: Vec<NodeId> },
    Call { fn_id: u32, args: Vec<NodeId> },
    Param(u32),
    If { branches: Vec<(StyleCondNode, NodeId)>, fallback: NodeId },
    Switch { key: NodeId, table: HashMap<i64, NodeId>, fallback: NodeId },
    Concat,
    Terminal,
    BitField { src: NodeId, shift: u8, width: Option<u8> },
    LoadVarDynamic {
        slot_expr: NodeId,
        valid_lo: i32,
        valid_hi: i32,
        kind: TickPosition,
        fallback: NodeId,
    },
    BitwiseOp { kind: BitwiseKind, args: Vec<NodeId> },
}

/// Compiled DAG: a `Dag` plus a parallel handler-pointer table and a
/// flattened-payload table.
pub struct CompiledDag {
    /// Source DAG. Owned so the handler closures can read it (e.g. for
    /// the function-call sub-DAG roots).
    pub(crate) dag: Dag,
    /// One handler per `NodeId`, indexed identically to `dag.nodes`.
    handlers: Vec<NodeHandler>,
    /// One payload per `NodeId`. Flattened so handlers can take it
    /// without re-matching on the `DagNode` enum.
    payloads: Vec<NodePayload>,
}

impl CompiledDag {
    /// Build a `CompiledDag` from a `Dag`. One-shot at backend selection
    /// time.
    pub fn build(dag: Dag) -> Self {
        let n = dag.nodes.len();
        let mut handlers: Vec<NodeHandler> = Vec::with_capacity(n);
        let mut payloads: Vec<NodePayload> = Vec::with_capacity(n);
        for node in &dag.nodes {
            let (h, p) = lower_node(node);
            handlers.push(h);
            payloads.push(p);
        }
        CompiledDag { dag, handlers, payloads }
    }

    /// Tear back down so the `Evaluator` can reclaim the source DAG
    /// (used when switching backends).
    pub fn into_dag(self) -> Dag {
        self.dag
    }

    /// Total node count, for diagnostics.
    pub fn node_count(&self) -> usize {
        self.dag.nodes.len()
    }
}

/// Mutable context threaded through every handler. Mirrors the v2
/// walker's `DagWalkBuf` plus the epoch bump-counter that the walker
/// keeps on `Evaluator`.
pub(crate) struct DagCtx<'a> {
    pub memo: &'a mut [i32],
    pub memo_epoch: &'a mut [u32],
    pub epoch: u32,
    pub state_var_cache: &'a [Option<i32>],
    pub memory_cache: &'a HashMap<i32, i32>,
    pub transient_cache: &'a [Option<i32>],
    pub call_stack: &'a mut Vec<Vec<i32>>,
    /// Globally-monotonic epoch counter (lives on `Evaluator`). Bumped
    /// on entry to every `Call` so sibling calls don't collide in the
    /// memo. Mirrors `dag_v2_epoch`.
    pub epoch_counter: &'a mut u32,
}

/// Diagnostic counters. Only incremented when `STATS_ENABLE` is true;
/// release builds keep the overhead at one branch per visit.
pub mod stats {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    pub static ENABLE: AtomicBool = AtomicBool::new(false);
    pub static VISITS: AtomicU64 = AtomicU64::new(0);
    pub static MEMO_HITS: AtomicU64 = AtomicU64::new(0);
    pub static HANDLER_CALLS: AtomicU64 = AtomicU64::new(0);

    pub fn enable() {
        ENABLE.store(true, Ordering::Relaxed);
    }
    pub fn snapshot() -> (u64, u64, u64) {
        (
            VISITS.load(Ordering::Relaxed),
            MEMO_HITS.load(Ordering::Relaxed),
            HANDLER_CALLS.load(Ordering::Relaxed),
        )
    }
    pub fn reset() {
        VISITS.store(0, Ordering::Relaxed);
        MEMO_HITS.store(0, Ordering::Relaxed);
        HANDLER_CALLS.store(0, Ordering::Relaxed);
    }
}

/// Evaluate a node, with per-walk memoisation. The single dispatch
/// site for the whole prototype.
///
/// One atomic-relaxed read of `stats::ENABLE` gates all three
/// counter increments; on x86-64 this lowers to a single uncached
/// MOV. When stats are disabled (default) the atomic-add is skipped,
/// so the diagnostic infrastructure costs roughly one branch
/// (predicted taken-not-taken since ENABLE is false-most-of-the-time)
/// per visit on the hot path.
#[inline]
pub(crate) fn eval_node(cdag: &CompiledDag, id: NodeId, ctx: &mut DagCtx<'_>, state: &State) -> i32 {
    use std::sync::atomic::Ordering;
    let stats_on = stats::ENABLE.load(Ordering::Relaxed);
    if stats_on {
        stats::VISITS.fetch_add(1, Ordering::Relaxed);
    }
    let idx = id as usize;
    if ctx.memo_epoch[idx] == ctx.epoch {
        if stats_on {
            stats::MEMO_HITS.fetch_add(1, Ordering::Relaxed);
        }
        return ctx.memo[idx];
    }
    if stats_on {
        stats::HANDLER_CALLS.fetch_add(1, Ordering::Relaxed);
    }
    let v = (cdag.handlers[idx])(cdag, id, ctx, state);
    ctx.memo[idx] = v;
    ctx.memo_epoch[idx] = ctx.epoch;
    v
}

// ---- Lowering: DagNode → (handler, payload) ---------------------

fn lower_node(node: &DagNode) -> (NodeHandler, NodePayload) {
    match node {
        DagNode::Lit(v) => (h_lit, NodePayload::Lit(*v as i32)),
        DagNode::LitStr(_) => (h_zero, NodePayload::LitStr),
        DagNode::LoadVar { slot, kind } => {
            let slot = *slot;
            // Specialise on slot region + tick position so each handler
            // is monomorphic on the read path it actually takes.
            match (slot >= TRANSIENT_BASE, *kind) {
                (true, TickPosition::Current) => {
                    let idx = (slot - TRANSIENT_BASE) as u32;
                    (h_load_transient_curr, NodePayload::LoadVarCurrentTransient { idx })
                }
                (true, TickPosition::Prev) => (h_zero, NodePayload::LoadVarPrevTransient),
                (false, TickPosition::Current) => {
                    (h_load_curr, NodePayload::LoadVarCurrent { slot })
                }
                (false, TickPosition::Prev) => {
                    (h_load_prev, NodePayload::LoadVarPrev { slot })
                }
            }
        }
        DagNode::Calc { op, args } => {
            let p = NodePayload::Calc { op: op.clone(), args: args.clone() };
            // Specialise the common arities. Hot-path handlers are
            // worth their own dispatch slot.
            match (op, args.len()) {
                (CalcKind::Add, 2) => (h_add2, p),
                (CalcKind::Sub, 2) => (h_sub2, p),
                (CalcKind::Mul, 2) => (h_mul2, p),
                (CalcKind::Div, 2) => (h_div2, p),
                (CalcKind::Mod, 2) => (h_mod2, p),
                _ => (h_calc_generic, p),
            }
        }
        DagNode::Call { fn_id, args } => (
            h_call,
            NodePayload::Call { fn_id: *fn_id, args: args.clone() },
        ),
        DagNode::Param(i) => (h_param, NodePayload::Param(*i)),
        DagNode::If { branches, fallback } => (
            h_if,
            NodePayload::If { branches: branches.clone(), fallback: *fallback },
        ),
        DagNode::Switch { key, table, fallback } => (
            h_switch,
            NodePayload::Switch { key: *key, table: table.clone(), fallback: *fallback },
        ),
        DagNode::Concat(_) => (h_zero, NodePayload::Concat),
        DagNode::WriteVar { .. } | DagNode::IndirectStore { .. } => {
            (h_zero, NodePayload::Terminal)
        }
        DagNode::BitField { src, shift, width } => (
            h_bitfield,
            NodePayload::BitField { src: *src, shift: *shift, width: *width },
        ),
        DagNode::LoadVarDynamic { slot_expr, valid_lo, valid_hi, kind, fallback } => (
            h_load_dynamic,
            NodePayload::LoadVarDynamic {
                slot_expr: *slot_expr,
                valid_lo: *valid_lo,
                valid_hi: *valid_hi,
                kind: *kind,
                fallback: *fallback,
            },
        ),
        DagNode::BitwiseOp { kind, args } => (
            h_bitwise,
            NodePayload::BitwiseOp { kind: *kind, args: args.clone() },
        ),
    }
}

// ---- Handlers ---------------------------------------------------

fn h_zero(_: &CompiledDag, _: NodeId, _: &mut DagCtx<'_>, _: &State) -> i32 {
    0
}

fn h_lit(cdag: &CompiledDag, id: NodeId, _: &mut DagCtx<'_>, _: &State) -> i32 {
    match &cdag.payloads[id as usize] {
        NodePayload::Lit(v) => *v,
        _ => unreachable!(),
    }
}

fn h_load_curr(cdag: &CompiledDag, id: NodeId, ctx: &mut DagCtx<'_>, state: &State) -> i32 {
    let slot = match &cdag.payloads[id as usize] {
        NodePayload::LoadVarCurrent { slot } => *slot,
        _ => unreachable!(),
    };
    if slot < 0 {
        let idx = (-slot - 1) as usize;
        if let Some(Some(v)) = ctx.state_var_cache.get(idx) {
            return *v;
        }
    } else if let Some(&v) = ctx.memory_cache.get(&slot) {
        return v;
    }
    state.read_mem(slot)
}

fn h_load_prev(cdag: &CompiledDag, id: NodeId, _: &mut DagCtx<'_>, state: &State) -> i32 {
    let slot = match &cdag.payloads[id as usize] {
        NodePayload::LoadVarPrev { slot } => *slot,
        _ => unreachable!(),
    };
    state.read_mem(slot)
}

fn h_load_transient_curr(
    cdag: &CompiledDag,
    id: NodeId,
    ctx: &mut DagCtx<'_>,
    _: &State,
) -> i32 {
    let idx = match &cdag.payloads[id as usize] {
        NodePayload::LoadVarCurrentTransient { idx } => *idx as usize,
        _ => unreachable!(),
    };
    ctx.transient_cache.get(idx).and_then(|x| *x).unwrap_or(0)
}

fn h_param(cdag: &CompiledDag, id: NodeId, ctx: &mut DagCtx<'_>, _: &State) -> i32 {
    let i = match &cdag.payloads[id as usize] {
        NodePayload::Param(i) => *i as usize,
        _ => unreachable!(),
    };
    ctx.call_stack
        .last()
        .and_then(|frame| frame.get(i).copied())
        .unwrap_or(0)
}

// Specialised arithmetic — the hot path on every cabinet.
fn h_add2(cdag: &CompiledDag, id: NodeId, ctx: &mut DagCtx<'_>, state: &State) -> i32 {
    let (a, b) = calc_args2(cdag, id);
    let av = eval_node(cdag, a, ctx, state);
    let bv = eval_node(cdag, b, ctx, state);
    av.wrapping_add(bv)
}

fn h_sub2(cdag: &CompiledDag, id: NodeId, ctx: &mut DagCtx<'_>, state: &State) -> i32 {
    let (a, b) = calc_args2(cdag, id);
    let av = eval_node(cdag, a, ctx, state);
    let bv = eval_node(cdag, b, ctx, state);
    av.wrapping_sub(bv)
}

fn h_mul2(cdag: &CompiledDag, id: NodeId, ctx: &mut DagCtx<'_>, state: &State) -> i32 {
    let (a, b) = calc_args2(cdag, id);
    let av = eval_node(cdag, a, ctx, state) as f64;
    let bv = eval_node(cdag, b, ctx, state) as f64;
    (av * bv) as i32
}

fn h_div2(cdag: &CompiledDag, id: NodeId, ctx: &mut DagCtx<'_>, state: &State) -> i32 {
    let (a, b) = calc_args2(cdag, id);
    let bv = eval_node(cdag, b, ctx, state) as f64;
    if bv == 0.0 {
        return 0;
    }
    let av = eval_node(cdag, a, ctx, state) as f64;
    (av / bv) as i32
}

fn h_mod2(cdag: &CompiledDag, id: NodeId, ctx: &mut DagCtx<'_>, state: &State) -> i32 {
    let (a, b) = calc_args2(cdag, id);
    let bv = eval_node(cdag, b, ctx, state) as f64;
    if bv == 0.0 {
        return 0;
    }
    let av = eval_node(cdag, a, ctx, state) as f64;
    (av - (av / bv).floor() * bv) as i32
}

#[inline]
fn calc_args2(cdag: &CompiledDag, id: NodeId) -> (NodeId, NodeId) {
    match &cdag.payloads[id as usize] {
        NodePayload::Calc { args, .. } => (args[0], args[1]),
        _ => unreachable!(),
    }
}

fn h_calc_generic(cdag: &CompiledDag, id: NodeId, ctx: &mut DagCtx<'_>, state: &State) -> i32 {
    let (op, args): (CalcKind, Vec<NodeId>) = match &cdag.payloads[id as usize] {
        NodePayload::Calc { op, args } => (op.clone(), args.clone()),
        _ => unreachable!(),
    };
    let walk = |ctx: &mut DagCtx<'_>, id: NodeId| -> f64 {
        eval_node(cdag, id, ctx, state) as f64
    };
    let result: f64 = match op {
        CalcKind::Add => walk(ctx, args[0]) + walk(ctx, args[1]),
        CalcKind::Sub => walk(ctx, args[0]) - walk(ctx, args[1]),
        CalcKind::Mul => walk(ctx, args[0]) * walk(ctx, args[1]),
        CalcKind::Div => {
            let b = walk(ctx, args[1]);
            if b == 0.0 { 0.0 } else { walk(ctx, args[0]) / b }
        }
        CalcKind::Mod => {
            let b = walk(ctx, args[1]);
            if b == 0.0 {
                0.0
            } else {
                let a = walk(ctx, args[0]);
                a - (a / b).floor() * b
            }
        }
        CalcKind::Pow => walk(ctx, args[0]).powf(walk(ctx, args[1])),
        CalcKind::Min => {
            let mut acc = f64::INFINITY;
            for &id in &args {
                acc = acc.min(walk(ctx, id));
            }
            acc
        }
        CalcKind::Max => {
            let mut acc = f64::NEG_INFINITY;
            for &id in &args {
                acc = acc.max(walk(ctx, id));
            }
            acc
        }
        CalcKind::Clamp => {
            let lo = walk(ctx, args[0]);
            let v = walk(ctx, args[1]);
            let hi = walk(ctx, args[2]);
            v.clamp(lo, hi)
        }
        CalcKind::Round(strategy) => {
            let v = walk(ctx, args[0]);
            let interval = walk(ctx, args[1]);
            if interval == 0.0 {
                v
            } else {
                let q = v / interval;
                let rounded = match strategy {
                    RoundStrategy::Nearest => {
                        let f = q.floor();
                        let frac = q - f;
                        if frac < 0.5 { f }
                        else if frac > 0.5 { f + 1.0 }
                        else if (f as i64) % 2 == 0 { f } else { f + 1.0 }
                    }
                    RoundStrategy::Up => q.ceil(),
                    RoundStrategy::Down => q.floor(),
                    RoundStrategy::ToZero => q.trunc(),
                };
                rounded * interval
            }
        }
        CalcKind::Sign => {
            let v = walk(ctx, args[0]);
            if v > 0.0 { 1.0 } else if v < 0.0 { -1.0 } else { 0.0 }
        }
        CalcKind::Abs => walk(ctx, args[0]).abs(),
        CalcKind::Neg => -walk(ctx, args[0]),
    };
    result as i32
}

fn h_call(cdag: &CompiledDag, id: NodeId, ctx: &mut DagCtx<'_>, state: &State) -> i32 {
    let (fn_id, args): (u32, Vec<NodeId>) = match &cdag.payloads[id as usize] {
        NodePayload::Call { fn_id, args } => (*fn_id, args.clone()),
        _ => unreachable!(),
    };
    let Some(&body_root) = cdag.dag.function_roots.get(fn_id as usize) else {
        return 0;
    };
    if body_root == NodeId::MAX {
        return 0;
    }

    // Pre-evaluate args in caller context.
    let mut frame: Vec<i32> = Vec::with_capacity(args.len());
    for arg_id in &args {
        frame.push(eval_node(cdag, *arg_id, ctx, state));
    }

    // Bump epoch globally so sibling Calls don't collide in the memo.
    let saved_epoch = ctx.epoch;
    ctx.call_stack.push(frame);
    *ctx.epoch_counter = ctx.epoch_counter.wrapping_add(1);
    ctx.epoch = *ctx.epoch_counter;
    let result = eval_node(cdag, body_root, ctx, state);
    ctx.call_stack.pop();
    ctx.epoch = saved_epoch;
    result
}

fn h_if(cdag: &CompiledDag, id: NodeId, ctx: &mut DagCtx<'_>, state: &State) -> i32 {
    let (nbranches, fallback) = match &cdag.payloads[id as usize] {
        NodePayload::If { branches, fallback } => (branches.len(), *fallback),
        _ => unreachable!(),
    };
    for i in 0..nbranches {
        let matches = eval_style_cond(cdag, id, i, ctx, state);
        if matches {
            let then_id = match &cdag.payloads[id as usize] {
                NodePayload::If { branches, .. } => branches[i].1,
                _ => unreachable!(),
            };
            return eval_node(cdag, then_id, ctx, state);
        }
    }
    eval_node(cdag, fallback, ctx, state)
}

fn h_switch(cdag: &CompiledDag, id: NodeId, ctx: &mut DagCtx<'_>, state: &State) -> i32 {
    let key_id = match &cdag.payloads[id as usize] {
        NodePayload::Switch { key, .. } => *key,
        _ => unreachable!(),
    };
    let key_val = eval_node(cdag, key_id, ctx, state);
    let target = match &cdag.payloads[id as usize] {
        NodePayload::Switch { table, fallback, .. } => {
            table.get(&(key_val as i64)).copied().unwrap_or(*fallback)
        }
        _ => unreachable!(),
    };
    eval_node(cdag, target, ctx, state)
}

fn h_bitfield(cdag: &CompiledDag, id: NodeId, ctx: &mut DagCtx<'_>, state: &State) -> i32 {
    let (src, shift, width) = match &cdag.payloads[id as usize] {
        NodePayload::BitField { src, shift, width } => (*src, *shift, *width),
        _ => unreachable!(),
    };
    let v = eval_node(cdag, src, ctx, state);
    let shifted = if shift >= 32 { 0 } else { v >> shift };
    match width {
        None => shifted,
        Some(w) if w >= 32 => shifted,
        Some(w) => shifted & ((1i32 << w) - 1),
    }
}

fn h_load_dynamic(cdag: &CompiledDag, id: NodeId, ctx: &mut DagCtx<'_>, state: &State) -> i32 {
    let (slot_expr, valid_lo, valid_hi, kind, fallback) = match &cdag.payloads[id as usize] {
        NodePayload::LoadVarDynamic {
            slot_expr,
            valid_lo,
            valid_hi,
            kind,
            fallback,
        } => (*slot_expr, *valid_lo, *valid_hi, *kind, *fallback),
        _ => unreachable!(),
    };
    let slot = eval_node(cdag, slot_expr, ctx, state);
    if slot >= valid_lo && slot < valid_hi {
        read_slot_dispatch(slot, kind, ctx, state)
    } else {
        eval_node(cdag, fallback, ctx, state)
    }
}

fn h_bitwise(cdag: &CompiledDag, id: NodeId, ctx: &mut DagCtx<'_>, state: &State) -> i32 {
    let (kind, arg0, arg1) = match &cdag.payloads[id as usize] {
        NodePayload::BitwiseOp { kind, args } => (
            *kind,
            args.first().copied().unwrap_or(0),
            args.get(1).copied().unwrap_or(0),
        ),
        _ => unreachable!(),
    };
    match kind {
        BitwiseKind::Not => {
            let a = eval_node(cdag, arg0, ctx, state) as u32 & 0xFFFF;
            ((!a) & 0xFFFF) as i32
        }
        BitwiseKind::And => {
            let a = eval_node(cdag, arg0, ctx, state) as u32 & 0xFFFF;
            let b = eval_node(cdag, arg1, ctx, state) as u32 & 0xFFFF;
            (a & b) as i32
        }
        BitwiseKind::Or => {
            let a = eval_node(cdag, arg0, ctx, state) as u32 & 0xFFFF;
            let b = eval_node(cdag, arg1, ctx, state) as u32 & 0xFFFF;
            (a | b) as i32
        }
        BitwiseKind::Xor => {
            let a = eval_node(cdag, arg0, ctx, state) as u32 & 0xFFFF;
            let b = eval_node(cdag, arg1, ctx, state) as u32 & 0xFFFF;
            (a ^ b) as i32
        }
    }
}

#[inline]
fn read_slot_dispatch(
    slot: i32,
    kind: TickPosition,
    ctx: &mut DagCtx<'_>,
    state: &State,
) -> i32 {
    if slot >= TRANSIENT_BASE {
        if matches!(kind, TickPosition::Prev) {
            return 0;
        }
        let idx = (slot - TRANSIENT_BASE) as usize;
        return ctx.transient_cache.get(idx).and_then(|x| *x).unwrap_or(0);
    }
    if matches!(kind, TickPosition::Prev) {
        return state.read_mem(slot);
    }
    if slot < 0 {
        let idx = (-slot - 1) as usize;
        if let Some(Some(v)) = ctx.state_var_cache.get(idx) {
            return *v;
        }
    } else if let Some(&v) = ctx.memory_cache.get(&slot) {
        return v;
    }
    state.read_mem(slot)
}

// ---- StyleCondition evaluation ---------------------------------

fn eval_style_cond(
    cdag: &CompiledDag,
    if_node_id: NodeId,
    branch_idx: usize,
    ctx: &mut DagCtx<'_>,
    state: &State,
) -> bool {
    let mut path: Vec<usize> = Vec::new();
    eval_style_cond_path(cdag, if_node_id, branch_idx, &mut path, ctx, state)
}

fn eval_style_cond_path(
    cdag: &CompiledDag,
    if_node_id: NodeId,
    branch_idx: usize,
    path: &mut Vec<usize>,
    ctx: &mut DagCtx<'_>,
    state: &State,
) -> bool {
    enum CondShape {
        Single(NodeId, NodeId),
        And(usize),
        Or(usize),
    }
    let shape = {
        let mut cur: &StyleCondNode = match &cdag.payloads[if_node_id as usize] {
            NodePayload::If { branches, .. } => &branches[branch_idx].0,
            _ => unreachable!(),
        };
        for &idx in path.iter() {
            cur = match cur {
                StyleCondNode::And(parts) | StyleCondNode::Or(parts) => &parts[idx],
                StyleCondNode::Single { .. } => unreachable!("path past Single"),
            };
        }
        match cur {
            StyleCondNode::Single { lhs, value } => CondShape::Single(*lhs, *value),
            StyleCondNode::And(parts) => CondShape::And(parts.len()),
            StyleCondNode::Or(parts) => CondShape::Or(parts.len()),
        }
    };

    match shape {
        CondShape::Single(lhs, rhs) => {
            let l = eval_node(cdag, lhs, ctx, state);
            let r = eval_node(cdag, rhs, ctx, state);
            l == r
        }
        CondShape::And(n) => {
            for i in 0..n {
                path.push(i);
                let ok = eval_style_cond_path(cdag, if_node_id, branch_idx, path, ctx, state);
                path.pop();
                if !ok {
                    return false;
                }
            }
            true
        }
        CondShape::Or(n) => {
            for i in 0..n {
                path.push(i);
                let ok = eval_style_cond_path(cdag, if_node_id, branch_idx, path, ctx, state);
                path.pop();
                if ok {
                    return true;
                }
            }
            false
        }
    }
}
