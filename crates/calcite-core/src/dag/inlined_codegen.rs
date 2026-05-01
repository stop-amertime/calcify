//! Phase 3 (c+) prototype: per-terminal straight-line emission.
//!
//! Per `docs/log.md` 2026-05-01 visit-count breakdown, the v2 walker
//! does ~33× fewer work units per tick than v1 but each unit costs
//! ~167× more (15 ns vs 0.09 ns). The closure spike showed the
//! ~167× isn't dispatch-shape — it's the recursion + memo + payload-
//! unpack of the per-node executor model.
//!
//! This module emits, per `WriteVar` terminal, a topologically-ordered
//! flat op vec over per-terminal local slots. The executor is a
//! v1-style `pc++` loop. No recursion, no memo, no function pointers
//! per node — the cone is computed once, each local written once,
//! consumers read locals as plain array indices.
//!
//! Coverage in this prototype is intentionally narrow: literal, LoadVar
//! (current/prev, scalar/transient), and arity-2 arithmetic. Terminals
//! whose cones contain anything else (Call/Param, If/Switch, BitField,
//! BitwiseOp, LoadVarDynamic, Concat) are not emitted; the driver
//! falls back to the walker for those terminals on a per-terminal
//! basis. That's the minimum surface needed to measure whether the
//! straight-line emit shape clears v1 on the terminals it covers.
//!
//! Cardinal rule: this module knows nothing about CSS-DOS, x86, or
//! cabinets. It walks `Dag` shapes — same vocabulary as the walker
//! and the closure prototype.

use std::collections::HashMap;

use crate::dag::types::{BitwiseKind, CalcKind, Dag, DagNode, NodeId, TickPosition};
use crate::dag::TRANSIENT_BASE;
use crate::state::State;
use crate::types::RoundStrategy;

/// A single op in the per-terminal straight-line program.
///
/// Locals are u16 indices into a per-terminal scratch buffer; values
/// are i32 throughout (matching the rest of v2).
#[derive(Debug, Clone)]
pub(crate) enum InlinedOp {
    /// Write `v` into local `dst`.
    LoadLit { dst: u16, v: i32 },
    /// Read state at `slot` (kind-aware: scalar/transient × current/prev)
    /// into local `dst`. Slot encoding matches `dag::SlotId`.
    LoadVar { dst: u16, slot: i32, kind: TickPosition },
    /// `dst = locals[a] + locals[b]` (wrapping i32).
    Add { dst: u16, a: u16, b: u16 },
    /// `dst = locals[a] - locals[b]` (wrapping i32).
    Sub { dst: u16, a: u16, b: u16 },
    /// `dst = (locals[a] * locals[b])` via f64 (matches walker semantics).
    Mul { dst: u16, a: u16, b: u16 },
    /// `dst = locals[a] / locals[b]` via f64; 0 on zero divisor.
    Div { dst: u16, a: u16, b: u16 },
    /// `dst = locals[a] mod locals[b]` (Euclidean-ish, matches walker).
    Mod { dst: u16, a: u16, b: u16 },
    // ---- Arity-1 arithmetic ----
    Neg { dst: u16, a: u16 },
    Abs { dst: u16, a: u16 },
    Sign { dst: u16, a: u16 },
    // ---- Arity-2 + ----
    /// `dst = pow(locals[a], locals[b])` via f64.
    Pow { dst: u16, a: u16, b: u16 },
    /// `dst = min(locals[a], locals[b])` via f64.
    Min2 { dst: u16, a: u16, b: u16 },
    /// `dst = max(locals[a], locals[b])` via f64.
    Max2 { dst: u16, a: u16, b: u16 },
    /// `dst = clamp(locals[lo], locals[v], locals[hi])` via f64.
    Clamp { dst: u16, lo: u16, v: u16, hi: u16 },
    /// `dst = round(strategy, locals[v], locals[interval])`. Encoded
    /// inline as the small RoundStrategy enum copy.
    Round { dst: u16, v: u16, interval: u16, strategy: RoundStrategy },
    // ---- BitField (Phase 2 super-node) ----
    /// `dst = bitfield(locals[src], shift, width)`. Width=None means
    /// pure right shift (sign-preserving).
    BitField { dst: u16, src: u16, shift: u8, width: Option<u8> },
    // ---- BitwiseOp (Phase 2 super-node) ----
    /// 16-bit bitwise op. For Not, only `a` is used.
    Bitwise { dst: u16, kind: BitwiseKind, a: u16, b: u16 },
}

/// One emitted terminal: a straight-line program plus the slot it
/// writes to.
#[derive(Debug, Clone)]
pub(crate) struct EmittedTerminal {
    pub ops: Vec<InlinedOp>,
    /// Local index holding the final value (the WriteVar's `value` cone
    /// root). Stored in `locals[result]` after the program runs.
    pub result: u16,
    /// Number of locals the program needs.
    pub local_count: usize,
}

/// Compiled DAG with per-terminal straight-line ops where supported.
///
/// `terminals[i] == Some(emitted)` means terminal `i` of `dag.terminals`
/// has an emitted program; `None` means the terminal's cone contains a
/// node kind this prototype doesn't yet handle and the driver should
/// fall back to the walker.
pub struct InlinedDag {
    pub(crate) dag: Dag,
    /// One slot per `dag.terminals` entry, in declaration order.
    pub(crate) emitted: Vec<Option<EmittedTerminal>>,
    /// Per-terminal scratch buffer sized to the largest local_count
    /// across all emitted terminals. Reused across ticks; lives on
    /// `Evaluator` for ownership reasons but built here for sizing.
    pub(crate) max_locals: usize,
}

impl InlinedDag {
    pub fn build(dag: Dag) -> Self {
        let mut emitted: Vec<Option<EmittedTerminal>> = Vec::with_capacity(dag.terminals.len());
        let mut max_locals = 0usize;
        for &term_id in &dag.terminals {
            let result = match &dag.nodes[term_id as usize] {
                DagNode::WriteVar { slot, value } => emit_terminal(&dag, *slot, *value),
                _ => None,
            };
            if let Some(ref e) = result {
                if e.local_count > max_locals {
                    max_locals = e.local_count;
                }
            }
            emitted.push(result);
        }
        InlinedDag { dag, emitted, max_locals }
    }

    pub fn into_dag(self) -> Dag {
        self.dag
    }

    pub fn node_count(&self) -> usize {
        self.dag.nodes.len()
    }

    /// Diagnostic: (n_terminals_total, n_terminals_emitted).
    pub fn coverage(&self) -> (usize, usize) {
        let total = self.emitted.len();
        let covered = self.emitted.iter().filter(|x| x.is_some()).count();
        (total, covered)
    }
}

/// Walk the cone of `value_root`; if every node is a kind we emit,
/// produce a topologically-ordered op vec. Returns `None` on any
/// unsupported kind.
fn emit_terminal(dag: &Dag, _slot: i32, value_root: NodeId) -> Option<EmittedTerminal> {
    // Topo order via post-order DFS. Each node appears once; parents
    // emitted after their deps. Use an explicit stack to avoid a
    // recursion blow-up on deep DAGs.
    let mut order: Vec<NodeId> = Vec::new();
    let mut local_of: HashMap<NodeId, u16> = HashMap::new();
    let mut visited: HashMap<NodeId, bool> = HashMap::new();
    // Stack frame: (node, child_idx). child_idx == 0 means we haven't
    // pushed deps yet; otherwise it's the index of the next dep to
    // visit. Once child_idx == n_children, we emit the node.
    let mut stack: Vec<(NodeId, usize)> = Vec::new();
    stack.push((value_root, 0));
    while let Some(&(node_id, ci)) = stack.last() {
        if let Some(&true) = visited.get(&node_id) {
            // Already fully emitted — drop the frame.
            stack.pop();
            continue;
        }
        let deps = node_deps(&dag.nodes[node_id as usize])?;
        if ci < deps.len() {
            // Advance the parent's child cursor, then descend.
            stack.last_mut().unwrap().1 = ci + 1;
            let child = deps[ci];
            // Skip if the child is already done.
            if visited.get(&child).copied().unwrap_or(false) {
                continue;
            }
            stack.push((child, 0));
        } else {
            // All deps emitted — emit this node.
            stack.pop();
            visited.insert(node_id, true);
            let local = order.len() as u16;
            local_of.insert(node_id, local);
            order.push(node_id);
        }
    }

    // Emit ops in topo order.
    let mut ops: Vec<InlinedOp> = Vec::with_capacity(order.len());
    for &node_id in &order {
        let dst = local_of[&node_id];
        let op = match &dag.nodes[node_id as usize] {
            DagNode::Lit(v) => InlinedOp::LoadLit { dst, v: *v as i32 },
            DagNode::LoadVar { slot, kind } => {
                InlinedOp::LoadVar { dst, slot: *slot, kind: *kind }
            }
            DagNode::Calc { op, args } => match (op, args.len()) {
                (CalcKind::Add, 2) => {
                    let (a, b) = (local_of[&args[0]], local_of[&args[1]]);
                    InlinedOp::Add { dst, a, b }
                }
                (CalcKind::Sub, 2) => {
                    let (a, b) = (local_of[&args[0]], local_of[&args[1]]);
                    InlinedOp::Sub { dst, a, b }
                }
                (CalcKind::Mul, 2) => {
                    let (a, b) = (local_of[&args[0]], local_of[&args[1]]);
                    InlinedOp::Mul { dst, a, b }
                }
                (CalcKind::Div, 2) => {
                    let (a, b) = (local_of[&args[0]], local_of[&args[1]]);
                    InlinedOp::Div { dst, a, b }
                }
                (CalcKind::Mod, 2) => {
                    let (a, b) = (local_of[&args[0]], local_of[&args[1]]);
                    InlinedOp::Mod { dst, a, b }
                }
                (CalcKind::Pow, 2) => {
                    let (a, b) = (local_of[&args[0]], local_of[&args[1]]);
                    InlinedOp::Pow { dst, a, b }
                }
                (CalcKind::Min, 2) => {
                    let (a, b) = (local_of[&args[0]], local_of[&args[1]]);
                    InlinedOp::Min2 { dst, a, b }
                }
                (CalcKind::Max, 2) => {
                    let (a, b) = (local_of[&args[0]], local_of[&args[1]]);
                    InlinedOp::Max2 { dst, a, b }
                }
                (CalcKind::Clamp, 3) => InlinedOp::Clamp {
                    dst,
                    lo: local_of[&args[0]],
                    v: local_of[&args[1]],
                    hi: local_of[&args[2]],
                },
                (CalcKind::Round(strategy), 2) => InlinedOp::Round {
                    dst,
                    v: local_of[&args[0]],
                    interval: local_of[&args[1]],
                    strategy: *strategy,
                },
                (CalcKind::Sign, 1) => InlinedOp::Sign { dst, a: local_of[&args[0]] },
                (CalcKind::Abs, 1) => InlinedOp::Abs { dst, a: local_of[&args[0]] },
                (CalcKind::Neg, 1) => InlinedOp::Neg { dst, a: local_of[&args[0]] },
                // Variadic Min/Max with >2 args: not yet emitted.
                _ => return None,
            },
            DagNode::BitField { src, shift, width } => InlinedOp::BitField {
                dst,
                src: local_of[src],
                shift: *shift,
                width: *width,
            },
            DagNode::BitwiseOp { kind, args } => {
                let a = args.first().copied().map(|id| local_of[&id]).unwrap_or(0);
                let b = args.get(1).copied().map(|id| local_of[&id]).unwrap_or(0);
                InlinedOp::Bitwise { dst, kind: *kind, a, b }
            }
            // Anything else in the cone bails the whole terminal.
            _ => return None,
        };
        ops.push(op);
    }

    let result = local_of[&value_root];
    let local_count = order.len();
    Some(EmittedTerminal { ops, result, local_count })
}

/// Return the dependency NodeIds of a node, or `None` if the node
/// kind isn't supported by this prototype.
fn node_deps(node: &DagNode) -> Option<Vec<NodeId>> {
    match node {
        DagNode::Lit(_) => Some(Vec::new()),
        DagNode::LoadVar { .. } => Some(Vec::new()),
        DagNode::Calc { op, args } => match (op, args.len()) {
            // Arity-1
            (CalcKind::Sign | CalcKind::Abs | CalcKind::Neg, 1) => Some(args.clone()),
            // Arity-2
            (
                CalcKind::Add
                | CalcKind::Sub
                | CalcKind::Mul
                | CalcKind::Div
                | CalcKind::Mod
                | CalcKind::Pow
                | CalcKind::Min
                | CalcKind::Max
                | CalcKind::Round(_),
                2,
            ) => Some(args.clone()),
            // Arity-3
            (CalcKind::Clamp, 3) => Some(args.clone()),
            // Variadic Min/Max with >2 args not handled — would need a
            // dedicated variadic op; bail on the terminal for now.
            _ => None,
        },
        DagNode::BitField { src, .. } => Some(vec![*src]),
        DagNode::BitwiseOp { args, .. } => Some(args.clone()),
        // Unsupported in this prototype: Call, Param, If, Switch,
        // Concat, LitStr, IndirectStore, WriteVar (handled at
        // terminal level), LoadVarDynamic.
        _ => None,
    }
}

/// Run one emitted terminal's straight-line program against `locals`.
///
/// Returns the final value (also written into `locals[emitted.result]`).
/// `locals` must be sized to at least `emitted.local_count`.
#[inline]
pub(crate) fn run_terminal(
    emitted: &EmittedTerminal,
    locals: &mut [i32],
    state_var_cache: &[Option<i32>],
    memory_cache: &HashMap<i32, i32>,
    transient_cache: &[Option<i32>],
    state: &State,
) -> i32 {
    for op in &emitted.ops {
        match *op {
            InlinedOp::LoadLit { dst, v } => {
                locals[dst as usize] = v;
            }
            InlinedOp::LoadVar { dst, slot, kind } => {
                locals[dst as usize] = read_slot_dispatch(
                    slot,
                    kind,
                    state_var_cache,
                    memory_cache,
                    transient_cache,
                    state,
                );
            }
            InlinedOp::Add { dst, a, b } => {
                let av = locals[a as usize];
                let bv = locals[b as usize];
                locals[dst as usize] = av.wrapping_add(bv);
            }
            InlinedOp::Sub { dst, a, b } => {
                let av = locals[a as usize];
                let bv = locals[b as usize];
                locals[dst as usize] = av.wrapping_sub(bv);
            }
            InlinedOp::Mul { dst, a, b } => {
                let av = locals[a as usize] as f64;
                let bv = locals[b as usize] as f64;
                locals[dst as usize] = (av * bv) as i32;
            }
            InlinedOp::Div { dst, a, b } => {
                let bv = locals[b as usize] as f64;
                locals[dst as usize] = if bv == 0.0 {
                    0
                } else {
                    let av = locals[a as usize] as f64;
                    (av / bv) as i32
                };
            }
            InlinedOp::Mod { dst, a, b } => {
                let bv = locals[b as usize] as f64;
                locals[dst as usize] = if bv == 0.0 {
                    0
                } else {
                    let av = locals[a as usize] as f64;
                    (av - (av / bv).floor() * bv) as i32
                };
            }
            InlinedOp::Neg { dst, a } => {
                let v = -(locals[a as usize] as f64);
                locals[dst as usize] = v as i32;
            }
            InlinedOp::Abs { dst, a } => {
                let v = (locals[a as usize] as f64).abs();
                locals[dst as usize] = v as i32;
            }
            InlinedOp::Sign { dst, a } => {
                let av = locals[a as usize] as f64;
                let v = if av > 0.0 { 1.0 } else if av < 0.0 { -1.0 } else { 0.0 };
                locals[dst as usize] = v as i32;
            }
            InlinedOp::Pow { dst, a, b } => {
                let av = locals[a as usize] as f64;
                let bv = locals[b as usize] as f64;
                locals[dst as usize] = av.powf(bv) as i32;
            }
            InlinedOp::Min2 { dst, a, b } => {
                let av = locals[a as usize] as f64;
                let bv = locals[b as usize] as f64;
                locals[dst as usize] = av.min(bv) as i32;
            }
            InlinedOp::Max2 { dst, a, b } => {
                let av = locals[a as usize] as f64;
                let bv = locals[b as usize] as f64;
                locals[dst as usize] = av.max(bv) as i32;
            }
            InlinedOp::Clamp { dst, lo, v, hi } => {
                let lov = locals[lo as usize] as f64;
                let vv = locals[v as usize] as f64;
                let hiv = locals[hi as usize] as f64;
                locals[dst as usize] = vv.clamp(lov, hiv) as i32;
            }
            InlinedOp::Round { dst, v, interval, strategy } => {
                let vv = locals[v as usize] as f64;
                let iv = locals[interval as usize] as f64;
                let result = if iv == 0.0 {
                    vv
                } else {
                    let q = vv / iv;
                    let rounded = match strategy {
                        RoundStrategy::Nearest => {
                            let f = q.floor();
                            let frac = q - f;
                            if frac < 0.5 {
                                f
                            } else if frac > 0.5 {
                                f + 1.0
                            } else if (f as i64) % 2 == 0 {
                                f
                            } else {
                                f + 1.0
                            }
                        }
                        RoundStrategy::Up => q.ceil(),
                        RoundStrategy::Down => q.floor(),
                        RoundStrategy::ToZero => q.trunc(),
                    };
                    rounded * iv
                };
                locals[dst as usize] = result as i32;
            }
            InlinedOp::BitField { dst, src, shift, width } => {
                let sv = locals[src as usize];
                let shifted = if shift >= 32 { 0 } else { sv >> shift };
                locals[dst as usize] = match width {
                    None => shifted,
                    Some(w) if w >= 32 => shifted,
                    Some(w) => shifted & ((1i32 << w) - 1),
                };
            }
            InlinedOp::Bitwise { dst, kind, a, b } => {
                let av = locals[a as usize] as u32 & 0xFFFF;
                let bv = locals[b as usize] as u32 & 0xFFFF;
                locals[dst as usize] = match kind {
                    BitwiseKind::Not => ((!av) & 0xFFFF) as i32,
                    BitwiseKind::And => (av & bv) as i32,
                    BitwiseKind::Or => (av | bv) as i32,
                    BitwiseKind::Xor => (av ^ bv) as i32,
                };
            }
        }
    }
    locals[emitted.result as usize]
}

#[inline]
fn read_slot_dispatch(
    slot: i32,
    kind: TickPosition,
    state_var_cache: &[Option<i32>],
    memory_cache: &HashMap<i32, i32>,
    transient_cache: &[Option<i32>],
    state: &State,
) -> i32 {
    if slot >= TRANSIENT_BASE {
        if matches!(kind, TickPosition::Prev) {
            return 0;
        }
        let idx = (slot - TRANSIENT_BASE) as usize;
        return transient_cache.get(idx).and_then(|x| *x).unwrap_or(0);
    }
    if matches!(kind, TickPosition::Prev) {
        return state.read_mem(slot);
    }
    if slot < 0 {
        let idx = (-slot - 1) as usize;
        if let Some(Some(v)) = state_var_cache.get(idx) {
            return *v;
        }
    } else if let Some(&v) = memory_cache.get(&slot) {
        return v;
    }
    state.read_mem(slot)
}
