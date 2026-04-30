//! Replicated-body recognition.
//!
//! When an emitter unrolls a straight-line kernel (e.g. a column drawer that
//! writes 16 pixels back-to-back), the compiled op stream contains a contiguous
//! run of identically-shaped op blocks repeated K times. Each repetition has
//! the same op kinds in the same order; operands either stay constant across
//! reps or vary linearly (a base + k*stride pattern: addresses advance, slot
//! indices may rotate, etc.).
//!
//! This module's job is to find those runs. It only looks at op-kind
//! periodicity here (Step 2 of the recogniser). Operand classification
//! (Constant vs Linear vs NonAffine) lives in a separate pass.
//!
//! The detector knows nothing about what the ops *do*. It would fire equally
//! on an unrolled 6502 sprite blit, an unrolled brainfuck `>+` chain, or any
//! emitter that produces periodic op sequences. The thing it recognises is
//! "emitter chose to unroll", which is shape-only.

use crate::compile::Op;
use crate::types::RoundStrategy;
use std::mem::discriminant;

/// Minimum repetition count before we bother folding. Below this, the
/// dispatch-cost saving doesn't justify the extra runtime indirection of
/// a replicated-body op.
pub const MIN_REPS: usize = 8;

/// Minimum body length. A 1-op "body" is just N copies of the same op,
/// which is rarely useful and overlaps with simpler peepholes.
pub const MIN_PERIOD: usize = 2;

/// Result of period detection on a contiguous op slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Periodicity {
    /// Length of the repeated body.
    pub period: usize,
    /// Number of repetitions (period * reps == slice length covered).
    pub reps: usize,
}

/// Find the smallest period P such that `ops[i]` and `ops[i+P]` have the same
/// op-kind for every i in `[0, len-P)`, where `len = ops.len()`, P divides
/// `len`, `len/P >= MIN_REPS`, and `P >= MIN_PERIOD`.
///
/// Returns `None` if no such P exists, or if the slice is too short.
///
/// "Op-kind" here means the enum discriminant only — operand values are not
/// compared. Operand-shape classification is a separate pass (see
/// stride classifier).
///
/// Complexity: O(len^2 / MIN_REPS) worst case, but the inner `MIN_REPS` early
/// bail keeps it close to O(len) for the common "no period" case.
pub fn detect_period(ops: &[Op]) -> Option<Periodicity> {
    let len = ops.len();
    if len < MIN_PERIOD * MIN_REPS {
        return None;
    }

    // Candidate periods: divisors of len that are >= MIN_PERIOD and yield
    // >= MIN_REPS repetitions. We iterate from smallest to largest because
    // we want the smallest period (the tightest body).
    let max_period = len / MIN_REPS;
    for period in MIN_PERIOD..=max_period {
        if len % period != 0 {
            continue;
        }
        let reps = len / period;
        if reps < MIN_REPS {
            // Once reps drops below threshold, all larger periods also fail.
            break;
        }
        if kinds_repeat(ops, period) {
            return Some(Periodicity { period, reps });
        }
    }
    None
}

/// Check whether `ops` is `period`-periodic in op-kind.
///
/// Returns true iff for every i in `[0, len-period)`, `ops[i]` and
/// `ops[i+period]` share the same enum discriminant.
fn kinds_repeat(ops: &[Op], period: usize) -> bool {
    let len = ops.len();
    debug_assert!(period > 0 && len % period == 0);
    for i in 0..(len - period) {
        if discriminant(&ops[i]) != discriminant(&ops[i + period]) {
            return false;
        }
    }
    true
}

/// Find the longest prefix of `ops` (starting at `start`) that is periodic
/// with period `period`. Used by the recogniser when a region's full length
/// isn't a clean multiple of the candidate period — we still want to fold
/// the prefix that *is* periodic.
///
/// Returns the number of complete repetitions found. The minimum useful
/// return is 2 (one matching body plus one repeat). Returns `<=1` when
/// no replication is present — callers should treat that as "not periodic
/// here".
pub fn longest_periodic_prefix(ops: &[Op], start: usize, period: usize) -> usize {
    if period == 0 || start + 2 * period > ops.len() {
        return 0;
    }
    let mut reps = 1;
    let mut at = start + period;
    while at + period <= ops.len() {
        let head = &ops[start..start + period];
        let next = &ops[at..at + period];
        if !slices_same_kinds(head, next) {
            break;
        }
        reps += 1;
        at += period;
    }
    reps
}

fn slices_same_kinds(a: &[Op], b: &[Op]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).all(|(x, y)| discriminant(x) == discriminant(y))
}

// ---------------------------------------------------------------------------
// Operand stride classification.
// ---------------------------------------------------------------------------
//
// Once the period detector has found a candidate (P, K), each of the K bodies
// has the same op-kind sequence. The classifier looks at the K instances of
// each operand position and decides whether it's:
//   - Constant: same value in every rep
//   - Linear { base, stride }: value at rep k is base + k*stride (must be
//     consistent across all K reps)
//   - NonAffine: doesn't fit either of the above — recogniser bails
//
// In addition, every Op variant has structural fields that must be byte-
// identical across reps (RoundStrategy, table_id, pack, fn_id, Vec arity,
// etc.) and may have control-flow fields (branch targets) that disqualify
// the body entirely.
//
// The classification table for every Op variant is encoded by the
// `extract_op_fields` function below. See the per-variant comments for
// the rationale.

/// Why a candidate body failed classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BailReason {
    /// An op in the body has a branch target (BranchIfZero, Jump, etc.).
    /// Control flow inside the body breaks the straight-line assumption.
    BranchInBody { op_index: usize },
    /// A structural field (RoundStrategy, table_id, pack, fn_id, Vec arity)
    /// differs between reps. Different shapes can't share one body.
    StructuralMismatch { op_index: usize, what: &'static str },
    /// An operand's values across reps don't fit Constant or Linear{base,stride}.
    NonAffineOperand { op_index: usize, operand_index: usize },
    /// Variable-arity vec (Min/Max args, Call arg_slots) has different lengths
    /// across reps.
    ArityMismatch { op_index: usize, what: &'static str },
}

/// Classification of a single operand position across the K reps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperandClass {
    Constant(i64),
    Linear { base: i64, stride: i64 },
}

/// A classified body: for each op-position j in [0, period), the structural
/// fingerprint (must match across reps) and the per-operand classifications
/// (Constant or Linear).
///
/// Operand order is the canonical extraction order from `extract_op_fields`,
/// stable across reps for the same op-kind. The codegen pass reads this back
/// to construct an Op::ReplicatedBody body template.
#[derive(Debug, Clone)]
pub struct BodyTemplate {
    pub period: usize,
    pub reps: usize,
    pub ops: Vec<OpTemplate>,
}

#[derive(Debug, Clone)]
pub struct OpTemplate {
    /// Discriminant of the op (proxy for "which variant"); value carries no
    /// semantic meaning, just used for assertions.
    pub discriminant_dbg: &'static str,
    /// Structural fields, byte-identical across reps. Order is per-variant.
    pub structural: Vec<u64>,
    /// Variable-arity field length (e.g. Min args.len()), if applicable.
    /// None for fixed-arity ops.
    pub arity: Option<usize>,
    /// One classification per operand position. Order is per-variant and
    /// matches `extract_op_fields` order.
    pub operands: Vec<OperandClass>,
}

/// Per-op extraction: pulls (variant tag, structural u64s, optional arity,
/// operand i64s, optional branch target). Canonical order is fixed per
/// variant; both extraction and codegen depend on it.
struct OpFields {
    /// Variant name for diagnostics.
    name: &'static str,
    /// Byte-identical-across-reps fields (encoded as u64s).
    structural: Vec<u64>,
    /// Variable-arity length if the variant has a Vec field, else None.
    arity: Option<usize>,
    /// Per-operand i64 values (Slots widened, i32s sign-extended).
    operands: Vec<i64>,
    /// True if this op has a branch target — recogniser bails on the body.
    is_branch: bool,
}

/// Encode `RoundStrategy` as u64 for structural matching.
fn round_strategy_u64(s: RoundStrategy) -> u64 {
    match s {
        RoundStrategy::Nearest => 0,
        RoundStrategy::Up => 1,
        RoundStrategy::Down => 2,
        RoundStrategy::ToZero => 3,
    }
}

/// Extract every Op variant into the canonical (structural, operands, branch?)
/// triple. **The classification table** — every Op variant lives here.
///
/// Operand order is the order callers should rely on. Adding a new Op variant
/// requires adding a case here and bumping any tests that round-trip.
fn extract_op_fields(op: &Op) -> OpFields {
    use Op::*;
    match op {
        // --- Loads (no structural, no branch) ---
        LoadLit { dst, val } => OpFields {
            name: "LoadLit",
            structural: vec![],
            arity: None,
            operands: vec![*dst as i64, *val as i64],
            is_branch: false,
        },
        LoadSlot { dst, src } => OpFields {
            name: "LoadSlot",
            structural: vec![],
            arity: None,
            operands: vec![*dst as i64, *src as i64],
            is_branch: false,
        },
        LoadState { dst, addr } => OpFields {
            name: "LoadState",
            structural: vec![],
            arity: None,
            operands: vec![*dst as i64, *addr as i64],
            is_branch: false,
        },
        LoadMem { dst, addr_slot } => OpFields {
            name: "LoadMem",
            structural: vec![],
            arity: None,
            operands: vec![*dst as i64, *addr_slot as i64],
            is_branch: false,
        },
        LoadMem16 { dst, addr_slot } => OpFields {
            name: "LoadMem16",
            structural: vec![],
            arity: None,
            operands: vec![*dst as i64, *addr_slot as i64],
            is_branch: false,
        },
        // table_id and pack are structural (compile-time-fixed table shape).
        LoadPackedByte { dst, key_slot, table_id, pack } => OpFields {
            name: "LoadPackedByte",
            structural: vec![*table_id as u64, *pack as u64],
            arity: None,
            operands: vec![*dst as i64, *key_slot as i64],
            is_branch: false,
        },

        // --- Arithmetic ---
        Add { dst, a, b } => OpFields {
            name: "Add", structural: vec![], arity: None,
            operands: vec![*dst as i64, *a as i64, *b as i64], is_branch: false,
        },
        AddLit { dst, a, val } => OpFields {
            name: "AddLit", structural: vec![], arity: None,
            operands: vec![*dst as i64, *a as i64, *val as i64], is_branch: false,
        },
        Sub { dst, a, b } => OpFields {
            name: "Sub", structural: vec![], arity: None,
            operands: vec![*dst as i64, *a as i64, *b as i64], is_branch: false,
        },
        SubLit { dst, a, val } => OpFields {
            name: "SubLit", structural: vec![], arity: None,
            operands: vec![*dst as i64, *a as i64, *val as i64], is_branch: false,
        },
        Mul { dst, a, b } => OpFields {
            name: "Mul", structural: vec![], arity: None,
            operands: vec![*dst as i64, *a as i64, *b as i64], is_branch: false,
        },
        MulLit { dst, a, val } => OpFields {
            name: "MulLit", structural: vec![], arity: None,
            operands: vec![*dst as i64, *a as i64, *val as i64], is_branch: false,
        },
        Div { dst, a, b } => OpFields {
            name: "Div", structural: vec![], arity: None,
            operands: vec![*dst as i64, *a as i64, *b as i64], is_branch: false,
        },
        Mod { dst, a, b } => OpFields {
            name: "Mod", structural: vec![], arity: None,
            operands: vec![*dst as i64, *a as i64, *b as i64], is_branch: false,
        },
        Neg { dst, src } => OpFields {
            name: "Neg", structural: vec![], arity: None,
            operands: vec![*dst as i64, *src as i64], is_branch: false,
        },
        Abs { dst, src } => OpFields {
            name: "Abs", structural: vec![], arity: None,
            operands: vec![*dst as i64, *src as i64], is_branch: false,
        },
        Sign { dst, src } => OpFields {
            name: "Sign", structural: vec![], arity: None,
            operands: vec![*dst as i64, *src as i64], is_branch: false,
        },
        Pow { dst, base, exp } => OpFields {
            name: "Pow", structural: vec![], arity: None,
            operands: vec![*dst as i64, *base as i64, *exp as i64], is_branch: false,
        },
        // Min/Max have variable-arity Vec<Slot>; arity must match across reps.
        Min { dst, args } => {
            let mut ops_vec = vec![*dst as i64];
            ops_vec.extend(args.iter().map(|s| *s as i64));
            OpFields {
                name: "Min",
                structural: vec![],
                arity: Some(args.len()),
                operands: ops_vec,
                is_branch: false,
            }
        }
        Max { dst, args } => {
            let mut ops_vec = vec![*dst as i64];
            ops_vec.extend(args.iter().map(|s| *s as i64));
            OpFields {
                name: "Max",
                structural: vec![],
                arity: Some(args.len()),
                operands: ops_vec,
                is_branch: false,
            }
        }
        Clamp { dst, min, val, max } => OpFields {
            name: "Clamp", structural: vec![], arity: None,
            operands: vec![*dst as i64, *min as i64, *val as i64, *max as i64],
            is_branch: false,
        },
        // strategy is structural — Nearest/Up/Down are different ops.
        Round { dst, strategy, val, interval } => OpFields {
            name: "Round",
            structural: vec![round_strategy_u64(*strategy)],
            arity: None,
            operands: vec![*dst as i64, *val as i64, *interval as i64],
            is_branch: false,
        },
        Floor { dst, src } => OpFields {
            name: "Floor", structural: vec![], arity: None,
            operands: vec![*dst as i64, *src as i64], is_branch: false,
        },

        // --- Bitwise ---
        And { dst, a, b } => OpFields {
            name: "And", structural: vec![], arity: None,
            operands: vec![*dst as i64, *a as i64, *b as i64], is_branch: false,
        },
        AndLit { dst, a, val } => OpFields {
            name: "AndLit", structural: vec![], arity: None,
            operands: vec![*dst as i64, *a as i64, *val as i64], is_branch: false,
        },
        Shr { dst, a, b } => OpFields {
            name: "Shr", structural: vec![], arity: None,
            operands: vec![*dst as i64, *a as i64, *b as i64], is_branch: false,
        },
        ShrLit { dst, a, val } => OpFields {
            name: "ShrLit", structural: vec![], arity: None,
            operands: vec![*dst as i64, *a as i64, *val as i64], is_branch: false,
        },
        Shl { dst, a, b } => OpFields {
            name: "Shl", structural: vec![], arity: None,
            operands: vec![*dst as i64, *a as i64, *b as i64], is_branch: false,
        },
        ShlLit { dst, a, val } => OpFields {
            name: "ShlLit", structural: vec![], arity: None,
            operands: vec![*dst as i64, *a as i64, *val as i64], is_branch: false,
        },
        ModLit { dst, a, val } => OpFields {
            name: "ModLit", structural: vec![], arity: None,
            operands: vec![*dst as i64, *a as i64, *val as i64], is_branch: false,
        },
        Bit { dst, val, idx } => OpFields {
            name: "Bit", structural: vec![], arity: None,
            operands: vec![*dst as i64, *val as i64, *idx as i64], is_branch: false,
        },
        BitAnd16 { dst, a, b } => OpFields {
            name: "BitAnd16", structural: vec![], arity: None,
            operands: vec![*dst as i64, *a as i64, *b as i64], is_branch: false,
        },
        BitOr16 { dst, a, b } => OpFields {
            name: "BitOr16", structural: vec![], arity: None,
            operands: vec![*dst as i64, *a as i64, *b as i64], is_branch: false,
        },
        BitXor16 { dst, a, b } => OpFields {
            name: "BitXor16", structural: vec![], arity: None,
            operands: vec![*dst as i64, *a as i64, *b as i64], is_branch: false,
        },
        BitNot16 { dst, a } => OpFields {
            name: "BitNot16", structural: vec![], arity: None,
            operands: vec![*dst as i64, *a as i64], is_branch: false,
        },

        // --- Comparisons (CmpEq is straight-line; branches are below) ---
        CmpEq { dst, a, b } => OpFields {
            name: "CmpEq", structural: vec![], arity: None,
            operands: vec![*dst as i64, *a as i64, *b as i64], is_branch: false,
        },

        // --- Branches (bail) ---
        BranchIfZero { .. }
        | BranchIfNotEqLit { .. }
        | LoadStateAndBranchIfNotEqLit { .. }
        | BranchIfNotEqLit2 { .. }
        | Jump { .. }
        | DispatchChain { .. }
        | Dispatch { .. } => OpFields {
            name: branch_name(op),
            structural: vec![],
            arity: None,
            operands: vec![],
            is_branch: true,
        },

        // DispatchFlatArray has no branch field — pure lookup.
        // array_id is structural (refers to a specific compile-time table).
        DispatchFlatArray { dst, key, array_id, base_key, default } => OpFields {
            name: "DispatchFlatArray",
            structural: vec![*array_id as u64],
            arity: None,
            operands: vec![
                *dst as i64,
                *key as i64,
                *base_key as i64,
                *default as i64,
            ],
            is_branch: false,
        },

        // Call: fn_id is structural; arg_slots arity must match across reps;
        // each arg is an operand. The callee's internal control flow is opaque
        // — fine because the call itself is straight-line.
        Call { dst, fn_id, arg_slots } => {
            let mut ops_vec = vec![*dst as i64];
            ops_vec.extend(arg_slots.iter().map(|s| *s as i64));
            OpFields {
                name: "Call",
                structural: vec![*fn_id as u64],
                arity: Some(arg_slots.len()),
                operands: ops_vec,
                is_branch: false,
            }
        }

        // --- Stores ---
        StoreState { addr, src } => OpFields {
            name: "StoreState", structural: vec![], arity: None,
            operands: vec![*addr as i64, *src as i64], is_branch: false,
        },
        StoreMem { addr_slot, src } => OpFields {
            name: "StoreMem", structural: vec![], arity: None,
            operands: vec![*addr_slot as i64, *src as i64], is_branch: false,
        },

        // Bulk memory ops have exit_target — bail.
        MemoryFill { .. } | MemoryCopy { .. } => OpFields {
            name: bulk_name(op),
            structural: vec![],
            arity: None,
            operands: vec![],
            is_branch: true,
        },

        // ReplicatedBody is the recogniser's *output* — it never appears as
        // input to the recogniser (recogniser runs over the original op
        // stream before any ReplicatedBody is inserted). Bail defensively if
        // we somehow see one (e.g. nested folding in a future iteration).
        ReplicatedBody { .. } => OpFields {
            name: "ReplicatedBody",
            structural: vec![],
            arity: None,
            operands: vec![],
            is_branch: true,
        },
    }
}

fn branch_name(op: &Op) -> &'static str {
    match op {
        Op::BranchIfZero { .. } => "BranchIfZero",
        Op::BranchIfNotEqLit { .. } => "BranchIfNotEqLit",
        Op::LoadStateAndBranchIfNotEqLit { .. } => "LoadStateAndBranchIfNotEqLit",
        Op::BranchIfNotEqLit2 { .. } => "BranchIfNotEqLit2",
        Op::Jump { .. } => "Jump",
        Op::DispatchChain { .. } => "DispatchChain",
        Op::Dispatch { .. } => "Dispatch",
        Op::ReplicatedBody { .. } => "ReplicatedBody",
        _ => "branch?",
    }
}

fn bulk_name(op: &Op) -> &'static str {
    match op {
        Op::MemoryFill { .. } => "MemoryFill",
        Op::MemoryCopy { .. } => "MemoryCopy",
        _ => "bulk?",
    }
}

/// Classify a candidate periodic region. `ops` is the full slice;
/// `(period, reps)` is what `detect_period` returned.
///
/// Returns `Ok(BodyTemplate)` if every op-position is well-formed across all
/// reps (structural fields match, arity matches, all operands are Constant or
/// Linear), or `Err(BailReason)` describing the first failure.
pub fn classify_body(
    ops: &[Op],
    period: usize,
    reps: usize,
) -> Result<BodyTemplate, BailReason> {
    debug_assert_eq!(ops.len(), period * reps);

    let mut op_templates = Vec::with_capacity(period);

    for j in 0..period {
        // Extract this op-position from each of the K reps.
        let extracted: Vec<OpFields> = (0..reps)
            .map(|k| extract_op_fields(&ops[k * period + j]))
            .collect();

        let head = &extracted[0];

        if head.is_branch {
            return Err(BailReason::BranchInBody { op_index: j });
        }

        // Structural fields must be byte-identical across reps.
        for (k, ef) in extracted.iter().enumerate().skip(1) {
            if ef.is_branch {
                return Err(BailReason::BranchInBody { op_index: j });
            }
            if ef.structural != head.structural {
                return Err(BailReason::StructuralMismatch {
                    op_index: j,
                    what: structural_label(head.name, k),
                });
            }
            if ef.arity != head.arity {
                return Err(BailReason::ArityMismatch {
                    op_index: j,
                    what: head.name,
                });
            }
            // Operand vector lengths must match (they will, given arity match,
            // but defensive).
            if ef.operands.len() != head.operands.len() {
                return Err(BailReason::ArityMismatch {
                    op_index: j,
                    what: head.name,
                });
            }
        }

        // Classify each operand across the K reps.
        let n_operands = head.operands.len();
        let mut classified = Vec::with_capacity(n_operands);
        for oi in 0..n_operands {
            let values: Vec<i64> = extracted.iter().map(|ef| ef.operands[oi]).collect();
            match classify_operand(&values) {
                Some(c) => classified.push(c),
                None => {
                    return Err(BailReason::NonAffineOperand {
                        op_index: j,
                        operand_index: oi,
                    })
                }
            }
        }

        op_templates.push(OpTemplate {
            discriminant_dbg: head.name,
            structural: head.structural.clone(),
            arity: head.arity,
            operands: classified,
        });
    }

    Ok(BodyTemplate {
        period,
        reps,
        ops: op_templates,
    })
}

/// A label for a structural mismatch — enough for the recogniser to log
/// which field broke without dragging the whole field name into BailReason.
fn structural_label(op_name: &'static str, _rep_index: usize) -> &'static str {
    op_name
}

/// Apply a stride list to a template op for a given rep index.
///
/// `template` is the rep-0 op. `strides` is the list of `(operand_index, stride)`
/// pairs for operands that are Linear. `k` is the rep index (0-based).
///
/// Returns a new `Op` with the same kind/structural fields and the same
/// operand values as `template`, except that for each `(idx, stride)` in
/// `strides` the operand at index `idx` is incremented by `k as i64 * stride
/// as i64` (saturating-cast to i32 / wrapping for u32 Slot fields).
///
/// Operand indices follow the canonical order from `extract_op_fields` —
/// keep this and that function in lockstep.
///
/// Variable-arity variants (Min, Max, Call) are not supported here — the
/// recogniser must bail before producing a ReplicatedBody containing them,
/// because cloning their `Vec<Slot>` per rep would defeat the perf goal.
/// This function panics on those variants in debug builds.
pub fn apply_strides(template: &Op, k: u32, strides: &[(u8, i32)]) -> Op {
    use Op::*;

    // Compute per-operand stride deltas as i64s; only operands listed in
    // `strides` deviate from the template value.
    let delta = |operand_idx: u8| -> i64 {
        for (idx, stride) in strides.iter() {
            if *idx == operand_idx {
                return (k as i64).wrapping_mul(*stride as i64);
            }
        }
        0
    };

    // Helpers for applying delta to a Slot or i32 field. Slot uses wrapping
    // u32 arithmetic; i32 uses wrapping_add.
    let slot_with = |s: u32, idx: u8| -> u32 {
        let d = delta(idx) as i32;
        s.wrapping_add(d as u32)
    };
    let i32_with = |v: i32, idx: u8| -> i32 {
        v.wrapping_add(delta(idx) as i32)
    };

    match template {
        LoadLit { dst, val } => LoadLit {
            dst: slot_with(*dst, 0),
            val: i32_with(*val, 1),
        },
        LoadSlot { dst, src } => LoadSlot {
            dst: slot_with(*dst, 0),
            src: slot_with(*src, 1),
        },
        LoadState { dst, addr } => LoadState {
            dst: slot_with(*dst, 0),
            addr: i32_with(*addr, 1),
        },
        LoadMem { dst, addr_slot } => LoadMem {
            dst: slot_with(*dst, 0),
            addr_slot: slot_with(*addr_slot, 1),
        },
        LoadMem16 { dst, addr_slot } => LoadMem16 {
            dst: slot_with(*dst, 0),
            addr_slot: slot_with(*addr_slot, 1),
        },
        LoadPackedByte { dst, key_slot, table_id, pack } => LoadPackedByte {
            dst: slot_with(*dst, 0),
            key_slot: slot_with(*key_slot, 1),
            table_id: *table_id,
            pack: *pack,
        },
        Add { dst, a, b } => Add {
            dst: slot_with(*dst, 0),
            a: slot_with(*a, 1),
            b: slot_with(*b, 2),
        },
        AddLit { dst, a, val } => AddLit {
            dst: slot_with(*dst, 0),
            a: slot_with(*a, 1),
            val: i32_with(*val, 2),
        },
        Sub { dst, a, b } => Sub {
            dst: slot_with(*dst, 0),
            a: slot_with(*a, 1),
            b: slot_with(*b, 2),
        },
        SubLit { dst, a, val } => SubLit {
            dst: slot_with(*dst, 0),
            a: slot_with(*a, 1),
            val: i32_with(*val, 2),
        },
        Mul { dst, a, b } => Mul {
            dst: slot_with(*dst, 0),
            a: slot_with(*a, 1),
            b: slot_with(*b, 2),
        },
        MulLit { dst, a, val } => MulLit {
            dst: slot_with(*dst, 0),
            a: slot_with(*a, 1),
            val: i32_with(*val, 2),
        },
        Div { dst, a, b } => Div {
            dst: slot_with(*dst, 0),
            a: slot_with(*a, 1),
            b: slot_with(*b, 2),
        },
        Mod { dst, a, b } => Mod {
            dst: slot_with(*dst, 0),
            a: slot_with(*a, 1),
            b: slot_with(*b, 2),
        },
        Neg { dst, src } => Neg {
            dst: slot_with(*dst, 0),
            src: slot_with(*src, 1),
        },
        Abs { dst, src } => Abs {
            dst: slot_with(*dst, 0),
            src: slot_with(*src, 1),
        },
        Sign { dst, src } => Sign {
            dst: slot_with(*dst, 0),
            src: slot_with(*src, 1),
        },
        Pow { dst, base, exp } => Pow {
            dst: slot_with(*dst, 0),
            base: slot_with(*base, 1),
            exp: slot_with(*exp, 2),
        },
        Clamp { dst, min, val, max } => Clamp {
            dst: slot_with(*dst, 0),
            min: slot_with(*min, 1),
            val: slot_with(*val, 2),
            max: slot_with(*max, 3),
        },
        Round { dst, strategy, val, interval } => Round {
            dst: slot_with(*dst, 0),
            strategy: *strategy,
            val: slot_with(*val, 1),
            interval: slot_with(*interval, 2),
        },
        Floor { dst, src } => Floor {
            dst: slot_with(*dst, 0),
            src: slot_with(*src, 1),
        },
        And { dst, a, b } => And {
            dst: slot_with(*dst, 0),
            a: slot_with(*a, 1),
            b: slot_with(*b, 2),
        },
        AndLit { dst, a, val } => AndLit {
            dst: slot_with(*dst, 0),
            a: slot_with(*a, 1),
            val: i32_with(*val, 2),
        },
        Shr { dst, a, b } => Shr {
            dst: slot_with(*dst, 0),
            a: slot_with(*a, 1),
            b: slot_with(*b, 2),
        },
        ShrLit { dst, a, val } => ShrLit {
            dst: slot_with(*dst, 0),
            a: slot_with(*a, 1),
            val: i32_with(*val, 2),
        },
        Shl { dst, a, b } => Shl {
            dst: slot_with(*dst, 0),
            a: slot_with(*a, 1),
            b: slot_with(*b, 2),
        },
        ShlLit { dst, a, val } => ShlLit {
            dst: slot_with(*dst, 0),
            a: slot_with(*a, 1),
            val: i32_with(*val, 2),
        },
        ModLit { dst, a, val } => ModLit {
            dst: slot_with(*dst, 0),
            a: slot_with(*a, 1),
            val: i32_with(*val, 2),
        },
        Bit { dst, val, idx } => Bit {
            dst: slot_with(*dst, 0),
            val: slot_with(*val, 1),
            idx: slot_with(*idx, 2),
        },
        BitAnd16 { dst, a, b } => BitAnd16 {
            dst: slot_with(*dst, 0),
            a: slot_with(*a, 1),
            b: slot_with(*b, 2),
        },
        BitOr16 { dst, a, b } => BitOr16 {
            dst: slot_with(*dst, 0),
            a: slot_with(*a, 1),
            b: slot_with(*b, 2),
        },
        BitXor16 { dst, a, b } => BitXor16 {
            dst: slot_with(*dst, 0),
            a: slot_with(*a, 1),
            b: slot_with(*b, 2),
        },
        BitNot16 { dst, a } => BitNot16 {
            dst: slot_with(*dst, 0),
            a: slot_with(*a, 1),
        },
        CmpEq { dst, a, b } => CmpEq {
            dst: slot_with(*dst, 0),
            a: slot_with(*a, 1),
            b: slot_with(*b, 2),
        },
        DispatchFlatArray { dst, key, array_id, base_key, default } => DispatchFlatArray {
            dst: slot_with(*dst, 0),
            key: slot_with(*key, 1),
            array_id: *array_id,
            base_key: i32_with(*base_key, 2),
            default: i32_with(*default, 3),
        },
        StoreState { addr, src } => StoreState {
            addr: i32_with(*addr, 0),
            src: slot_with(*src, 1),
        },
        StoreMem { addr_slot, src } => StoreMem {
            addr_slot: slot_with(*addr_slot, 0),
            src: slot_with(*src, 1),
        },

        // Branch/dispatch/bulk-mem variants and Vec-bearing variants
        // (Min/Max/Call) are rejected by the recogniser before reaching
        // here. Panic in debug to catch a recogniser bug; in release, fall
        // back to a clone (preserves correctness for unexpected inputs).
        Min { .. } | Max { .. } | Call { .. } => {
            debug_assert!(false, "apply_strides: Vec-bearing variant in body");
            template.clone()
        }
        BranchIfZero { .. }
        | BranchIfNotEqLit { .. }
        | LoadStateAndBranchIfNotEqLit { .. }
        | BranchIfNotEqLit2 { .. }
        | Jump { .. }
        | DispatchChain { .. }
        | Dispatch { .. }
        | MemoryFill { .. }
        | MemoryCopy { .. }
        | ReplicatedBody { .. } => {
            debug_assert!(false, "apply_strides: branch/bulk/replicated in body");
            template.clone()
        }
    }
}

/// Classify K observed values of an operand. Returns:
///   - `Some(Constant(v))` if all K values are equal to v
///   - `Some(Linear { base, stride })` if values[k] == base + k*stride for all k
///   - `None` if neither holds (NonAffine)
///
/// Detection: stride := values[1] - values[0]. Then verify values[k] == values[0] + k*stride.
/// Constant is the special case stride == 0 and we report it as Constant rather
/// than Linear { stride: 0 } so callers can specialise the codegen.
fn classify_operand(values: &[i64]) -> Option<OperandClass> {
    debug_assert!(values.len() >= 2);
    let base = values[0];
    let stride = values[1].checked_sub(base)?;
    for (k, &v) in values.iter().enumerate().skip(1) {
        // Use checked arithmetic — Slot indices can be near u32::MAX in
        // adversarial inputs and i64 multiplication of stride*k could in
        // principle overflow on a degenerate-large slice.
        let k_i64 = k as i64;
        let delta = stride.checked_mul(k_i64)?;
        let expected = base.checked_add(delta)?;
        if v != expected {
            return None;
        }
    }
    if stride == 0 {
        Some(OperandClass::Constant(base))
    } else {
        Some(OperandClass::Linear { base, stride })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::Op;

    // Helpers — synthesise op sequences with controlled periodicity.

    fn lit(dst: u32, val: i32) -> Op {
        Op::LoadLit { dst, val }
    }
    fn add_lit(dst: u32, a: u32, val: i32) -> Op {
        Op::AddLit { dst, a, val }
    }
    fn load_state(dst: u32, addr: i32) -> Op {
        Op::LoadState { dst, addr }
    }

    /// Build a body of given op-kind shape, repeated `reps` times.
    /// Operand values are deliberately varied across reps to confirm the
    /// detector ignores them.
    fn make_periodic(period_shape: &[fn(u32) -> Op], reps: usize) -> Vec<Op> {
        let mut out = Vec::with_capacity(period_shape.len() * reps);
        for k in 0..reps {
            for (j, mk) in period_shape.iter().enumerate() {
                out.push(mk((k * period_shape.len() + j) as u32));
            }
        }
        out
    }

    #[test]
    fn detects_period_2_with_16_reps() {
        // Body: [LoadState, AddLit] x 16
        let body: &[fn(u32) -> Op] = &[
            |i| load_state(i, i as i32 * 4),
            |i| add_lit(i, i, i as i32),
        ];
        let ops = make_periodic(body, 16);
        let p = detect_period(&ops).expect("should detect");
        assert_eq!(p.period, 2);
        assert_eq!(p.reps, 16);
    }

    #[test]
    fn detects_period_6_with_16_reps() {
        // The Doom XLAT-chain shape: 6-op body x 16. Body is heterogeneous.
        let body: &[fn(u32) -> Op] = &[
            |i| lit(i, i as i32),
            |i| load_state(i, i as i32),
            |i| load_state(i, i as i32),
            |i| add_lit(i, i, i as i32),
            |i| add_lit(i, i, i as i32),
            |i| lit(i, i as i32),
        ];
        let ops = make_periodic(body, 16);
        let p = detect_period(&ops).expect("should detect");
        assert_eq!(p.period, 6);
        assert_eq!(p.reps, 16);
    }

    #[test]
    fn rejects_too_few_reps() {
        // 7 reps of period-2 — below MIN_REPS (8).
        let body: &[fn(u32) -> Op] =
            &[|i| load_state(i, 0), |i| add_lit(i, 0, 0)];
        let ops = make_periodic(body, 7);
        assert!(detect_period(&ops).is_none());
    }

    #[test]
    fn rejects_period_1() {
        // 16 copies of the same op kind — period 1 is below MIN_PERIOD.
        let ops: Vec<Op> = (0..16).map(|i| lit(i, i as i32)).collect();
        // Could match at period=2 (LoadLit, LoadLit), but period-1 would match
        // first if we accepted it. We don't — detector starts at MIN_PERIOD=2.
        let p = detect_period(&ops).expect("period-2 of LoadLits should match");
        assert_eq!(p.period, 2);
        assert_eq!(p.reps, 8);
    }

    #[test]
    fn returns_smallest_period() {
        // [A,B] x 16. Candidate periods 2, 4, 8, 16, 32 (32 = len, reps=1, fails).
        // Smallest valid is 2.
        let body: &[fn(u32) -> Op] = &[|i| lit(i, 0), |i| load_state(i, 0)];
        let ops = make_periodic(body, 16);
        let p = detect_period(&ops).expect("should detect");
        assert_eq!(p.period, 2, "should pick smallest valid period");
    }

    #[test]
    fn rejects_aperiodic() {
        // Mix of three different kinds, no clean repetition.
        let ops = vec![
            lit(0, 0),
            load_state(1, 0),
            add_lit(2, 0, 0),
            lit(3, 0),
            load_state(4, 0),
            lit(5, 0),
            add_lit(6, 0, 0),
            load_state(7, 0),
            lit(8, 0),
            add_lit(9, 0, 0),
            lit(10, 0),
            load_state(11, 0),
            add_lit(12, 0, 0),
            load_state(13, 0),
            lit(14, 0),
            load_state(15, 0),
        ];
        assert!(detect_period(&ops).is_none());
    }

    #[test]
    fn rejects_one_corruption_in_periodic() {
        // 16-rep periodic, but one op in the middle differs.
        let body: &[fn(u32) -> Op] =
            &[|i| load_state(i, 0), |i| add_lit(i, 0, 0)];
        let mut ops = make_periodic(body, 16);
        // Corrupt rep 7's first op: AddLit instead of LoadState.
        ops[14] = add_lit(99, 0, 0);
        assert!(detect_period(&ops).is_none());
    }

    #[test]
    fn rejects_short_input() {
        let ops = vec![lit(0, 0), lit(1, 1)];
        assert!(detect_period(&ops).is_none());
    }

    #[test]
    fn empty_input() {
        assert!(detect_period(&[]).is_none());
    }

    #[test]
    fn longest_prefix_finds_15_of_16() {
        let body: &[fn(u32) -> Op] =
            &[|i| load_state(i, 0), |i| add_lit(i, 0, 0)];
        let mut ops = make_periodic(body, 16);
        // Corrupt the 16th rep so only 15 clean reps exist.
        ops[30] = lit(0, 0); // would have been LoadState
        let reps = longest_periodic_prefix(&ops, 0, 2);
        assert_eq!(reps, 15);
    }

    #[test]
    fn longest_prefix_one_when_second_rep_differs() {
        // First body [LoadLit, LoadState] is claimed; second body
        // [AddLit, LoadLit] doesn't match — only one rep present.
        let ops = vec![lit(0, 0), load_state(1, 0), add_lit(2, 0, 0), lit(3, 0)];
        let reps = longest_periodic_prefix(&ops, 0, 2);
        assert_eq!(reps, 1, "no replication means 1 body, not 0");
    }

    #[test]
    fn longest_prefix_zero_when_too_short_for_two_bodies() {
        let ops = vec![lit(0, 0), load_state(1, 0), add_lit(2, 0, 0)];
        let reps = longest_periodic_prefix(&ops, 0, 2);
        assert_eq!(reps, 0, "can't fit 2 bodies of period 2 in 3 ops");
    }

    #[test]
    fn ignores_operand_values_completely() {
        // All LoadLit but with wildly different operand values.
        // Period=2 body: [LoadLit, LoadLit].
        let ops: Vec<Op> = (0..32)
            .map(|i| lit(i, i as i32 * 12345))
            .collect();
        let p = detect_period(&ops).expect("should detect period-2");
        assert_eq!(p.period, 2);
        assert_eq!(p.reps, 16);
    }

    // -----------------------------------------------------------------------
    // Classifier tests
    // -----------------------------------------------------------------------

    fn store_state(addr: i32, src: u32) -> Op {
        Op::StoreState { addr, src }
    }

    #[test]
    fn classifies_constant_and_linear_operands() {
        // 16 reps of [LoadState { dst: 100, addr: 0x4000 + k*2 },
        //             StoreState { addr: 0x5000 + k*2, src: 100 }]
        // dst & src are constant; addr is linear stride 2.
        let mut ops = Vec::new();
        for k in 0..16 {
            ops.push(load_state(100, 0x4000 + (k * 2) as i32));
            ops.push(store_state(0x5000 + (k * 2) as i32, 100));
        }
        let bt = classify_body(&ops, 2, 16).expect("should classify");
        assert_eq!(bt.period, 2);
        assert_eq!(bt.reps, 16);
        // op0 = LoadState { dst, addr }
        assert_eq!(bt.ops[0].operands[0], OperandClass::Constant(100));
        assert_eq!(
            bt.ops[0].operands[1],
            OperandClass::Linear { base: 0x4000, stride: 2 }
        );
        // op1 = StoreState { addr, src }
        assert_eq!(
            bt.ops[1].operands[0],
            OperandClass::Linear { base: 0x5000, stride: 2 }
        );
        assert_eq!(bt.ops[1].operands[1], OperandClass::Constant(100));
    }

    #[test]
    fn slot_strides_detected() {
        // 16 reps of LoadLit { dst: k, val: 0 } — dst strides by 1.
        let ops: Vec<Op> = (0..16).map(|k| lit(k, 0)).collect();
        let bt = classify_body(&ops, 1, 16);
        // period=1 is below MIN_PERIOD; we'd never get here from detect_period,
        // but classify_body itself doesn't enforce that. Use period=2 instead.
        drop(bt);

        let ops: Vec<Op> = (0..8).flat_map(|k| {
            [lit(2 * k, 0), lit(2 * k + 1, 0)]
        }).collect();
        let bt = classify_body(&ops, 2, 8).expect("classifies");
        assert_eq!(bt.ops[0].operands[0], OperandClass::Linear { base: 0, stride: 2 });
        assert_eq!(bt.ops[1].operands[0], OperandClass::Linear { base: 1, stride: 2 });
    }

    #[test]
    fn bails_on_branch_in_body() {
        // 8 reps of [LoadState, Jump]. Jump has a target → bail.
        let mut ops = Vec::new();
        for k in 0..8 {
            ops.push(load_state(0, k as i32));
            ops.push(Op::Jump { target: 999 });
        }
        match classify_body(&ops, 2, 8) {
            Err(BailReason::BranchInBody { op_index }) => assert_eq!(op_index, 1),
            other => panic!("expected BranchInBody, got {:?}", other),
        }
    }

    #[test]
    fn bails_on_structural_mismatch_round_strategy() {
        // 8 reps of Round, but rep 0 is Down and rep 1+ is Up.
        let mut ops = Vec::new();
        for k in 0..8 {
            let strat = if k == 0 { RoundStrategy::Down } else { RoundStrategy::Up };
            ops.push(Op::Round { dst: 0, strategy: strat, val: 1, interval: 2 });
        }
        match classify_body(&ops, 1, 8) {
            // period=1 isn't really a body, but the classifier doesn't enforce
            // MIN_PERIOD; what we want to confirm is the structural-mismatch
            // path fires. Use period=2.
            _ => {}
        }

        let mut ops = Vec::new();
        for k in 0..8 {
            let strat0 = if k == 0 { RoundStrategy::Down } else { RoundStrategy::Up };
            ops.push(Op::Round { dst: 0, strategy: strat0, val: 1, interval: 2 });
            ops.push(Op::Round { dst: 1, strategy: RoundStrategy::Nearest, val: 1, interval: 2 });
        }
        match classify_body(&ops, 2, 8) {
            Err(BailReason::StructuralMismatch { op_index, .. }) => assert_eq!(op_index, 0),
            other => panic!("expected StructuralMismatch, got {:?}", other),
        }
    }

    #[test]
    fn bails_on_arity_mismatch_min() {
        // 8 reps of [Min { args: 2 }, Min { args: 2 }] — but rep 3's first Min
        // has 3 args.
        let mut ops = Vec::new();
        for k in 0..8 {
            let args = if k == 3 { vec![1, 2, 3] } else { vec![1, 2] };
            ops.push(Op::Min { dst: 0, args });
            ops.push(Op::Min { dst: 1, args: vec![3, 4] });
        }
        match classify_body(&ops, 2, 8) {
            Err(BailReason::ArityMismatch { op_index, what: "Min" }) => {
                assert_eq!(op_index, 0)
            }
            other => panic!("expected ArityMismatch on Min, got {:?}", other),
        }
    }

    #[test]
    fn bails_on_non_affine_operand() {
        // 8 reps of LoadState; addr values: 0, 4, 8, 12, 99, 20, 24, 28.
        // Position 4 breaks linearity.
        let addrs = [0i32, 4, 8, 12, 99, 20, 24, 28];
        let mut ops = Vec::new();
        for &a in &addrs {
            ops.push(load_state(0, a));
            ops.push(load_state(1, 0));
        }
        match classify_body(&ops, 2, 8) {
            Err(BailReason::NonAffineOperand { op_index, operand_index }) => {
                assert_eq!(op_index, 0);
                assert_eq!(operand_index, 1); // addr is operand index 1
            }
            other => panic!("expected NonAffineOperand, got {:?}", other),
        }
    }

    #[test]
    fn classifies_load_packed_byte_with_matching_table() {
        // Structural: table_id=7, pack=2 in every rep.
        // Operand: dst const, key_slot strides by 1.
        let mut ops = Vec::new();
        for k in 0..8 {
            ops.push(Op::LoadPackedByte { dst: 100, key_slot: k as u32, table_id: 7, pack: 2 });
            ops.push(store_state(0x6000 + k as i32, 100));
        }
        let bt = classify_body(&ops, 2, 8).expect("classifies");
        // op0 LoadPackedByte: structural [table_id, pack] = [7, 2]
        assert_eq!(bt.ops[0].structural, vec![7u64, 2u64]);
        assert_eq!(bt.ops[0].operands[0], OperandClass::Constant(100));
        assert_eq!(bt.ops[0].operands[1], OperandClass::Linear { base: 0, stride: 1 });
    }

    #[test]
    fn bails_on_packed_byte_table_id_mismatch() {
        let mut ops = Vec::new();
        for k in 0..8 {
            let tid = if k == 0 { 7 } else { 9 };
            ops.push(Op::LoadPackedByte { dst: 100, key_slot: 0, table_id: tid, pack: 2 });
            ops.push(store_state(0, 100));
        }
        match classify_body(&ops, 2, 8) {
            Err(BailReason::StructuralMismatch { op_index, .. }) => assert_eq!(op_index, 0),
            other => panic!("expected StructuralMismatch, got {:?}", other),
        }
    }

    #[test]
    fn classifies_call_with_matching_fn_id_and_arity() {
        // Call { fn_id: 5, arg_slots: [k, k+1] } over 8 reps.
        // dst constant; arg_slots both linear stride 1.
        let mut ops = Vec::new();
        for k in 0..8 {
            ops.push(Op::Call { dst: 200, fn_id: 5, arg_slots: vec![k as u32, k as u32 + 1] });
            ops.push(store_state(k as i32, 200));
        }
        let bt = classify_body(&ops, 2, 8).expect("classifies");
        assert_eq!(bt.ops[0].structural, vec![5u64]);
        assert_eq!(bt.ops[0].arity, Some(2));
        // operands: [dst, arg0, arg1]
        assert_eq!(bt.ops[0].operands[0], OperandClass::Constant(200));
        assert_eq!(bt.ops[0].operands[1], OperandClass::Linear { base: 0, stride: 1 });
        assert_eq!(bt.ops[0].operands[2], OperandClass::Linear { base: 1, stride: 1 });
    }

    #[test]
    fn bails_on_call_fn_id_mismatch() {
        let mut ops = Vec::new();
        for k in 0..8 {
            let fid = if k < 4 { 5 } else { 6 };
            ops.push(Op::Call { dst: 200, fn_id: fid, arg_slots: vec![0] });
            ops.push(store_state(0, 200));
        }
        match classify_body(&ops, 2, 8) {
            Err(BailReason::StructuralMismatch { op_index, .. }) => assert_eq!(op_index, 0),
            other => panic!("expected StructuralMismatch on Call fn_id, got {:?}", other),
        }
    }

    #[test]
    fn bails_on_call_arity_mismatch() {
        let mut ops = Vec::new();
        for k in 0..8 {
            let args = if k == 4 { vec![0u32, 1, 2] } else { vec![0u32, 1] };
            ops.push(Op::Call { dst: 200, fn_id: 5, arg_slots: args });
            ops.push(store_state(0, 200));
        }
        match classify_body(&ops, 2, 8) {
            Err(BailReason::ArityMismatch { op_index, .. }) => assert_eq!(op_index, 0),
            other => panic!("expected ArityMismatch on Call, got {:?}", other),
        }
    }

    #[test]
    fn bails_on_dispatch_in_body() {
        // Dispatch has a fallback_target.
        let mut ops = Vec::new();
        for _ in 0..8 {
            ops.push(load_state(0, 0));
            ops.push(Op::Dispatch { dst: 1, key: 0, table_id: 0, fallback_target: 999 });
        }
        match classify_body(&ops, 2, 8) {
            Err(BailReason::BranchInBody { op_index }) => assert_eq!(op_index, 1),
            other => panic!("expected BranchInBody for Dispatch, got {:?}", other),
        }
    }

    #[test]
    fn dispatch_flat_array_is_straight_line() {
        // DispatchFlatArray has no branch — pure lookup.
        let mut ops = Vec::new();
        for k in 0..8 {
            ops.push(Op::DispatchFlatArray {
                dst: 100, key: k as u32, array_id: 3, base_key: 0, default: 0,
            });
            ops.push(store_state(k as i32, 100));
        }
        let bt = classify_body(&ops, 2, 8).expect("classifies");
        assert_eq!(bt.ops[0].structural, vec![3u64]);
        // operands: [dst, key, base_key, default]
        assert_eq!(bt.ops[0].operands[0], OperandClass::Constant(100));
        assert_eq!(bt.ops[0].operands[1], OperandClass::Linear { base: 0, stride: 1 });
        assert_eq!(bt.ops[0].operands[2], OperandClass::Constant(0));
        assert_eq!(bt.ops[0].operands[3], OperandClass::Constant(0));
    }

    #[test]
    fn bails_on_memory_fill_in_body() {
        // MemoryFill has exit_target.
        let mut ops = Vec::new();
        for _ in 0..8 {
            ops.push(load_state(0, 0));
            ops.push(Op::MemoryFill {
                dst_slot: 0, val_slot: 0, count_slot: 0, exit_target: 999,
            });
        }
        match classify_body(&ops, 2, 8) {
            Err(BailReason::BranchInBody { op_index }) => assert_eq!(op_index, 1),
            other => panic!("expected BranchInBody for MemoryFill, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // apply_strides round-trip tests
    // -----------------------------------------------------------------------

    #[test]
    fn apply_strides_constant_only_returns_template() {
        // No strides = template returned unchanged for any k.
        let template = lit(42, 100);
        for k in 0..5 {
            let out = apply_strides(&template, k, &[]);
            match out {
                Op::LoadLit { dst, val } => {
                    assert_eq!(dst, 42);
                    assert_eq!(val, 100);
                }
                _ => panic!("unexpected op"),
            }
        }
    }

    #[test]
    fn apply_strides_linear_addr_load_state() {
        // LoadState { dst:100, addr:0x4000 } with stride 2 on addr (operand 1)
        let template = load_state(100, 0x4000);
        let strides = [(1u8, 2i32)];
        for k in 0..8 {
            let out = apply_strides(&template, k, &strides);
            match out {
                Op::LoadState { dst, addr } => {
                    assert_eq!(dst, 100, "dst constant");
                    assert_eq!(addr, 0x4000 + 2 * k as i32, "addr strides");
                }
                _ => panic!("unexpected op"),
            }
        }
    }

    #[test]
    fn apply_strides_slot_stride_loadlit() {
        // LoadLit { dst, val } where dst (operand 0) strides by 6.
        let template = lit(0, 99);
        let strides = [(0u8, 6i32)];
        for k in 0..4 {
            let out = apply_strides(&template, k, &strides);
            match out {
                Op::LoadLit { dst, val } => {
                    assert_eq!(dst, 6 * k);
                    assert_eq!(val, 99);
                }
                _ => panic!("unexpected op"),
            }
        }
    }

    #[test]
    fn apply_strides_round_trip_through_classifier() {
        // Build a body where every operand has a known stride; classify it;
        // apply strides; check we recover the original operand values per rep.
        let mut ops = Vec::new();
        for k in 0..8 {
            // LoadState { dst: 100 + k*2, addr: 0x4000 + k*4 }
            // AddLit    { dst: 200,        a: 100 + k*2, val: k }
            ops.push(load_state(100 + k as u32 * 2, 0x4000 + k as i32 * 4));
            ops.push(add_lit(200, 100 + k as u32 * 2, k as i32));
        }
        let p = detect_period(&ops).expect("periodic");
        assert_eq!(p.period, 2);
        assert_eq!(p.reps, 8);
        let bt = classify_body(&ops, p.period, p.reps).expect("classifies");

        // Translate BodyTemplate operand classifications back into a strides
        // list per op (drop Constant entries since they need no stride).
        let strides_per_op: Vec<Vec<(u8, i32)>> = bt.ops.iter().map(|op_t| {
            op_t.operands.iter().enumerate().filter_map(|(i, c)| {
                match c {
                    OperandClass::Constant(_) => None,
                    OperandClass::Linear { stride, .. } => Some((i as u8, *stride as i32)),
                }
            }).collect()
        }).collect();

        // For each rep, apply strides to the rep-0 templates and check we
        // recover the original op.
        let body_templates: Vec<Op> = ops[..p.period].to_vec();
        for k in 0..p.reps as u32 {
            for (j, template) in body_templates.iter().enumerate() {
                let materialised = apply_strides(template, k, &strides_per_op[j]);
                let original = &ops[k as usize * p.period + j];
                match (&materialised, original) {
                    (
                        Op::LoadState { dst: d1, addr: a1 },
                        Op::LoadState { dst: d2, addr: a2 },
                    ) => {
                        assert_eq!(d1, d2, "rep {} op {} dst", k, j);
                        assert_eq!(a1, a2, "rep {} op {} addr", k, j);
                    }
                    (
                        Op::AddLit { dst: d1, a: a1, val: v1 },
                        Op::AddLit { dst: d2, a: a2, val: v2 },
                    ) => {
                        assert_eq!(d1, d2, "rep {} op {} dst", k, j);
                        assert_eq!(a1, a2, "rep {} op {} a", k, j);
                        assert_eq!(v1, v2, "rep {} op {} val", k, j);
                    }
                    _ => panic!("variant mismatch at rep {} op {}", k, j),
                }
            }
        }
    }

    #[test]
    fn classifies_doom_xlat_kernel_shape() {
        // Synthetic body modelling the Doom8088 column-drawer inner loop.
        // 16 reps of:
        //   LoadState  dst, ax_addr           ; ah = al (from prev xlat)
        //   LoadMem    dst', dx               ; first xlat: ptab[frac]
        //   LoadMem    dst'', cx              ; second xlat: colormap[al]
        //   StoreMem   di_slot, src           ; stosw
        //   AddLit     di_slot, di_slot, 4    ; advance di
        //   AddLit     dx_slot, dx_slot, bp_lit ; frac += step
        // dst slots stride by some amount per rep; addresses linear.
        let mut ops = Vec::new();
        for k in 0..16 {
            let s = (k * 6) as u32;
            ops.push(load_state(s, 0x100));                       // const addr
            ops.push(Op::LoadMem { dst: s + 1, addr_slot: 50 });  // const slot
            ops.push(Op::LoadMem { dst: s + 2, addr_slot: 51 });  // const slot
            ops.push(Op::StoreMem { addr_slot: 60, src: s + 2 }); // const dst slot
            ops.push(Op::AddLit { dst: 60, a: 60, val: 4 });      // all const
            ops.push(Op::AddLit { dst: 50, a: 50, val: 8 });      // all const
        }
        let p = detect_period(&ops).expect("period detected");
        assert_eq!(p.period, 6);
        assert_eq!(p.reps, 16);
        let bt = classify_body(&ops, p.period, p.reps).expect("classifies");
        // op0: dst slot strides by 6 (k*6 + 0); addr is constant.
        assert_eq!(bt.ops[0].operands[0], OperandClass::Linear { base: 0, stride: 6 });
        assert_eq!(bt.ops[0].operands[1], OperandClass::Constant(0x100));
        // op4: AddLit { dst:60, a:60, val:4 } — all constant.
        for o in &bt.ops[4].operands {
            assert!(matches!(o, OperandClass::Constant(_)),
                "expected all constant operands on AddLit, got {:?}", o);
        }
    }
}
