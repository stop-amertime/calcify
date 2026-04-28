//! Fusion-site simulator: symbolic execution of Op sequences.
//!
//! Given a chain of dispatch entries (each dispatch entry is itself a
//! `Vec<Op>` that updates one slot from input slots), produce a single
//! composed expression for each touched slot. The composed expression
//! is itself a tree of `SymExpr` nodes — when concretised against an
//! input state, it yields the same value as running the original Op
//! sequence linearly.
//!
//! The simulator knows nothing about x86, opcodes, dispatch tables as
//! such — it walks Op enum variants by structural shape. It treats slot
//! reads as symbolic inputs (initially `SymExpr::Slot(s)`), evaluates
//! Op semantics symbolically, and writes the resulting `SymExpr` back
//! into the slot environment. After processing the full sequence, each
//! touched slot's environment value IS the composed expression.
//!
//! Genericity probe (cardinal rule): would this simulator fire on a
//! 6502 cabinet's per-opcode dispatch the same way? Yes — it walks Ops,
//! not opcode bytes. On a brainfuck cabinet's dispatch? Yes. Pattern
//! recognition over Op shape, not over what those Ops mean upstream.
//!
//! Scope. This first cut handles the pure register-update slice:
//!   - LoadLit, LoadSlot
//!   - AddLit, SubLit, MulLit, ShlLit, ShrLit, AndLit (lower-bytes), ModLit
//!   - Add, Sub, Mul, Shl, Shr, And (lower-bytes) — slot+slot
//!   - BitAnd16, BitOr16, BitXor16, BitNot16 — true bitwise (16-bit)
//!   - Neg, Floor
//!
//! Note: calcite's `And` / `AndLit` mean `lowerBytes(a, n)` =
//! `a & ((1<<n)-1)` — parameterised truncation, not bitwise AND. The
//! true bitwise ops are `BitAnd16` / `BitOr16` / `BitXor16` / `BitNot16`,
//! all of which are 16-bit-truncated. The simulator preserves this
//! distinction in `SymExpr::LowerBytes` vs `SymExpr::Bit{And,Or,Xor,Not}`.
//!
//! Anything else (memory loads/stores, dispatch, branch, function call,
//! bulk ops) bails out with `BailReason::Unsupported(variant_name)` and
//! reports the offending Op. Memory writes during fusion are handled at
//! a higher layer — the simulator's job is just the slot-side
//! composition. Branches inside an entry mean we can't compose
//! statically; bail.
//!
//! This is intentionally a small first cut. We extend the supported-Op
//! set as we encounter more shapes in real cabinet dispatch entries.

use std::collections::HashMap;

use crate::compile::{Op, Slot};

/// A symbolic expression. Trees of these represent composed values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymExpr {
    /// A constant integer.
    Const(i64),
    /// The initial value of a slot at fusion entry. Free variable.
    Slot(Slot),
    /// The initial value of a state-var address at fusion entry. Free variable.
    /// (Currently bails — included for completeness; LoadState support is TBD.)
    State(i32),
    /// Arithmetic on two sub-expressions.
    Add(Box<SymExpr>, Box<SymExpr>),
    Sub(Box<SymExpr>, Box<SymExpr>),
    Mul(Box<SymExpr>, Box<SymExpr>),
    Shl(Box<SymExpr>, Box<SymExpr>),
    Shr(Box<SymExpr>, Box<SymExpr>),
    /// `lowerBytes(a, n) = a & ((1<<n)-1)`. Calcite's `And`/`AndLit` semantics.
    /// The second arg is the bit width to keep.
    LowerBytes(Box<SymExpr>, Box<SymExpr>),
    /// `a % n`. Calcite's `Mod`/`ModLit`.
    Mod(Box<SymExpr>, Box<SymExpr>),
    Neg(Box<SymExpr>),
    Floor(Box<SymExpr>),
    Div(Box<SymExpr>, Box<SymExpr>),
    /// `(val >> idx) & 1` — calcite's Bit op.
    BitExtract(Box<SymExpr>, Box<SymExpr>),
    Min(Vec<SymExpr>),
    Max(Vec<SymExpr>),
    Abs(Box<SymExpr>),
    Sign(Box<SymExpr>),
    /// True bitwise ops, all 16-bit-truncated as in calcite.
    BitAnd16(Box<SymExpr>, Box<SymExpr>),
    BitOr16(Box<SymExpr>, Box<SymExpr>),
    BitXor16(Box<SymExpr>, Box<SymExpr>),
    BitNot16(Box<SymExpr>),
}

impl SymExpr {
    pub fn c(v: i64) -> Self { SymExpr::Const(v) }
    pub fn slot(s: Slot) -> Self { SymExpr::Slot(s) }

    /// Const-fold the expression bottom-up. Pure structural simplification.
    pub fn simplify(self) -> Self {
        use SymExpr::*;
        match self {
            Add(a, b) => {
                let a = a.simplify();
                let b = b.simplify();
                match (&a, &b) {
                    (Const(x), Const(y)) => Const(x.wrapping_add(*y)),
                    (Const(0), _) => b,
                    (_, Const(0)) => a,
                    _ => Add(Box::new(a), Box::new(b)),
                }
            }
            Sub(a, b) => {
                let a = a.simplify();
                let b = b.simplify();
                match (&a, &b) {
                    (Const(x), Const(y)) => Const(x.wrapping_sub(*y)),
                    (_, Const(0)) => a,
                    _ => Sub(Box::new(a), Box::new(b)),
                }
            }
            Mul(a, b) => {
                let a = a.simplify();
                let b = b.simplify();
                match (&a, &b) {
                    (Const(x), Const(y)) => Const(x.wrapping_mul(*y)),
                    (Const(0), _) | (_, Const(0)) => Const(0),
                    (Const(1), _) => b,
                    (_, Const(1)) => a,
                    _ => Mul(Box::new(a), Box::new(b)),
                }
            }
            Shl(a, b) => {
                let a = a.simplify();
                let b = b.simplify();
                match (&a, &b) {
                    (Const(x), Const(y)) => Const(x.wrapping_shl(*y as u32)),
                    (_, Const(0)) => a,
                    _ => Shl(Box::new(a), Box::new(b)),
                }
            }
            Shr(a, b) => {
                let a = a.simplify();
                let b = b.simplify();
                match (&a, &b) {
                    (Const(x), Const(y)) => Const(x.wrapping_shr(*y as u32)),
                    (_, Const(0)) => a,
                    _ => Shr(Box::new(a), Box::new(b)),
                }
            }
            LowerBytes(a, b) => {
                let a = a.simplify();
                let b = b.simplify();
                match (&a, &b) {
                    (Const(x), Const(n)) => {
                        let mask = (1i64 << (*n as u32)).wrapping_sub(1);
                        Const(x & mask)
                    }
                    _ => LowerBytes(Box::new(a), Box::new(b)),
                }
            }
            Mod(a, b) => {
                let a = a.simplify();
                let b = b.simplify();
                match (&a, &b) {
                    (Const(x), Const(y)) if *y != 0 => Const(x.wrapping_rem(*y)),
                    _ => Mod(Box::new(a), Box::new(b)),
                }
            }
            Neg(a) => {
                let a = a.simplify();
                match &a {
                    Const(x) => Const(x.wrapping_neg()),
                    _ => Neg(Box::new(a)),
                }
            }
            Floor(a) => {
                let a = a.simplify();
                match &a {
                    Const(_) => a,
                    _ => Floor(Box::new(a)),
                }
            }
            BitAnd16(a, b) => {
                let a = a.simplify();
                let b = b.simplify();
                match (&a, &b) {
                    (Const(x), Const(y)) => Const((x & y) & 0xFFFF),
                    (Const(0), _) | (_, Const(0)) => Const(0),
                    _ => BitAnd16(Box::new(a), Box::new(b)),
                }
            }
            BitOr16(a, b) => {
                let a = a.simplify();
                let b = b.simplify();
                match (&a, &b) {
                    (Const(x), Const(y)) => Const((x | y) & 0xFFFF),
                    (Const(0), _) => b,
                    (_, Const(0)) => a,
                    _ => BitOr16(Box::new(a), Box::new(b)),
                }
            }
            BitXor16(a, b) => {
                let a = a.simplify();
                let b = b.simplify();
                match (&a, &b) {
                    (Const(x), Const(y)) => Const((x ^ y) & 0xFFFF),
                    (Const(0), _) => b,
                    (_, Const(0)) => a,
                    _ => BitXor16(Box::new(a), Box::new(b)),
                }
            }
            BitNot16(a) => {
                let a = a.simplify();
                match &a {
                    Const(x) => Const((!x) & 0xFFFF),
                    _ => BitNot16(Box::new(a)),
                }
            }
            Div(a, b) => {
                let a = a.simplify();
                let b = b.simplify();
                match (&a, &b) {
                    (Const(x), Const(y)) if *y != 0 => Const(x.wrapping_div(*y)),
                    (_, Const(1)) => a,
                    _ => Div(Box::new(a), Box::new(b)),
                }
            }
            BitExtract(v, i) => {
                let v = v.simplify();
                let i = i.simplify();
                match (&v, &i) {
                    (Const(x), Const(idx)) => Const((x >> (*idx as u32)) & 1),
                    _ => BitExtract(Box::new(v), Box::new(i)),
                }
            }
            Min(args) => {
                let args: Vec<SymExpr> = args.into_iter().map(|a| a.simplify()).collect();
                if args.iter().all(|a| matches!(a, Const(_))) {
                    let m = args.iter().filter_map(|a| match a { Const(v) => Some(*v), _ => None }).min().unwrap_or(0);
                    Const(m)
                } else {
                    Min(args)
                }
            }
            Max(args) => {
                let args: Vec<SymExpr> = args.into_iter().map(|a| a.simplify()).collect();
                if args.iter().all(|a| matches!(a, Const(_))) {
                    let m = args.iter().filter_map(|a| match a { Const(v) => Some(*v), _ => None }).max().unwrap_or(0);
                    Const(m)
                } else {
                    Max(args)
                }
            }
            Abs(a) => {
                let a = a.simplify();
                match &a {
                    Const(x) => Const(x.wrapping_abs()),
                    _ => Abs(Box::new(a)),
                }
            }
            Sign(a) => {
                let a = a.simplify();
                match &a {
                    Const(x) => Const(x.signum()),
                    _ => Sign(Box::new(a)),
                }
            }
            _ => self,
        }
    }

    /// Concretise against a slot environment and return the i64 value.
    /// Only succeeds if every leaf is `Const` or a `Slot` present in env.
    pub fn eval_concrete(&self, slot_env: &HashMap<Slot, i64>) -> Option<i64> {
        use SymExpr::*;
        match self {
            Const(v) => Some(*v),
            Slot(s) => slot_env.get(s).copied(),
            State(_) => None,
            Add(a, b) => Some(a.eval_concrete(slot_env)?.wrapping_add(b.eval_concrete(slot_env)?)),
            Sub(a, b) => Some(a.eval_concrete(slot_env)?.wrapping_sub(b.eval_concrete(slot_env)?)),
            Mul(a, b) => Some(a.eval_concrete(slot_env)?.wrapping_mul(b.eval_concrete(slot_env)?)),
            Shl(a, b) => Some(a.eval_concrete(slot_env)?.wrapping_shl(b.eval_concrete(slot_env)? as u32)),
            Shr(a, b) => Some(a.eval_concrete(slot_env)?.wrapping_shr(b.eval_concrete(slot_env)? as u32)),
            LowerBytes(a, b) => {
                let av = a.eval_concrete(slot_env)?;
                let n = b.eval_concrete(slot_env)?;
                let mask = (1i64 << (n as u32)).wrapping_sub(1);
                Some(av & mask)
            }
            Mod(a, b) => {
                let bv = b.eval_concrete(slot_env)?;
                if bv == 0 { return None; }
                Some(a.eval_concrete(slot_env)?.wrapping_rem(bv))
            }
            Neg(a) => Some(a.eval_concrete(slot_env)?.wrapping_neg()),
            Floor(a) => a.eval_concrete(slot_env), // i64 floor is identity
            BitAnd16(a, b) => Some((a.eval_concrete(slot_env)? & b.eval_concrete(slot_env)?) & 0xFFFF),
            BitOr16(a, b) => Some((a.eval_concrete(slot_env)? | b.eval_concrete(slot_env)?) & 0xFFFF),
            BitXor16(a, b) => Some((a.eval_concrete(slot_env)? ^ b.eval_concrete(slot_env)?) & 0xFFFF),
            BitNot16(a) => Some((!a.eval_concrete(slot_env)?) & 0xFFFF),
            Div(a, b) => {
                let bv = b.eval_concrete(slot_env)?;
                if bv == 0 { return None; }
                Some(a.eval_concrete(slot_env)?.wrapping_div(bv))
            }
            BitExtract(v, i) => {
                let vv = v.eval_concrete(slot_env)?;
                let iv = i.eval_concrete(slot_env)?;
                Some((vv >> (iv as u32)) & 1)
            }
            Min(args) => {
                let mut min_v: Option<i64> = None;
                for a in args {
                    let v = a.eval_concrete(slot_env)?;
                    min_v = Some(min_v.map_or(v, |m| m.min(v)));
                }
                min_v
            }
            Max(args) => {
                let mut max_v: Option<i64> = None;
                for a in args {
                    let v = a.eval_concrete(slot_env)?;
                    max_v = Some(max_v.map_or(v, |m| m.max(v)));
                }
                max_v
            }
            Abs(a) => Some(a.eval_concrete(slot_env)?.wrapping_abs()),
            Sign(a) => Some(a.eval_concrete(slot_env)?.signum()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BailReason {
    /// Op variant not supported by the first-cut simulator.
    Unsupported(&'static str),
    /// Memory or state read/write — out-of-scope for the slot-only simulator.
    SideEffect(&'static str),
    /// Branches make static composition impossible.
    Branch(&'static str),
}

/// Symbolic environment: slot index → current expression for that slot.
pub type SlotEnv = HashMap<Slot, SymExpr>;

/// Compile-time assumptions about state-var values at fusion entry.
/// Maps `state.read_mem(addr)` results to a known constant. Used to
/// resolve `LoadState` / `LoadStateAndBranchIfNotEqLit` and (transitively)
/// branches whose condition slot becomes `Const` after assumption-aware
/// loading.
///
/// Semantics: the caller asserts that for each `(addr, val)` in
/// `state_vars`, `state.read_mem(addr)` will return `val` at the moment
/// the fused entry fires. If the assertion is wrong, the fused
/// expression returns wrong results — caller's responsibility.
///
/// On the doom column drawer, common assumptions are:
///   --df = 0 (CLD has fired)
///   --es-prefix-active = 0 (no segment override)
#[derive(Debug, Clone, Default)]
pub struct Assumptions {
    pub state_vars: HashMap<i32, i32>,
}

impl Assumptions {
    pub fn new() -> Self { Self::default() }
    pub fn with(mut self, addr: i32, val: i32) -> Self {
        self.state_vars.insert(addr, val);
        self
    }
}

/// Simulate one straight-line Op sequence (no control flow) against `env`.
/// On bail, return the reason and leave `env` partially updated (caller
/// should not trust it).
pub fn simulate_ops(env: &mut SlotEnv, ops: &[Op]) -> Result<(), BailReason> {
    let assumptions = Assumptions::default();
    simulate_ops_with(env, ops, &assumptions)
}

/// Simulate one Op sequence with control-flow + assumptions support.
/// Branches whose condition slot has a `Const` value are resolved
/// statically; unresolvable branches bail.
pub fn simulate_ops_with(
    env: &mut SlotEnv,
    ops: &[Op],
    assumptions: &Assumptions,
) -> Result<(), BailReason> {
    let mut pc: usize = 0;
    let mut steps: usize = 0;
    let max_steps = ops.len() * 16; // bound runaway simulation
    while pc < ops.len() {
        if steps > max_steps {
            return Err(BailReason::Branch("loop-or-runaway"));
        }
        steps += 1;
        let op = &ops[pc];
        match simulate_op_with_cfg(env, op, assumptions)? {
            StepOutcome::Next => pc += 1,
            StepOutcome::Jump(t) => pc = t,
            StepOutcome::Halt => break,
        }
    }
    Ok(())
}

enum StepOutcome {
    /// Advance to pc+1.
    Next,
    /// Jump to the given absolute op index inside the current sequence.
    Jump(usize),
    /// Stop simulation (used by an unconditional Jump out of bounds, etc.).
    Halt,
}

fn read_slot(env: &SlotEnv, s: Slot) -> SymExpr {
    env.get(&s).cloned().unwrap_or_else(|| SymExpr::Slot(s))
}

fn simulate_op(env: &mut SlotEnv, op: &Op) -> Result<(), BailReason> {
    let assumptions = Assumptions::default();
    match simulate_op_with_cfg(env, op, &assumptions)? {
        StepOutcome::Next => Ok(()),
        StepOutcome::Jump(_) | StepOutcome::Halt => Err(BailReason::Branch("branch")),
    }
}

fn simulate_op_with_cfg(
    env: &mut SlotEnv,
    op: &Op,
    assumptions: &Assumptions,
) -> Result<StepOutcome, BailReason> {
    use Op::*;
    match op {
        LoadLit { dst, val } => {
            env.insert(*dst, SymExpr::Const(*val as i64));
        }
        LoadSlot { dst, src } => {
            let v = read_slot(env, *src);
            env.insert(*dst, v);
        }
        AddLit { dst, a, val } => {
            let a = read_slot(env, *a);
            env.insert(
                *dst,
                SymExpr::Add(Box::new(a), Box::new(SymExpr::Const(*val as i64))).simplify(),
            );
        }
        SubLit { dst, a, val } => {
            let a = read_slot(env, *a);
            env.insert(
                *dst,
                SymExpr::Sub(Box::new(a), Box::new(SymExpr::Const(*val as i64))).simplify(),
            );
        }
        MulLit { dst, a, val } => {
            let a = read_slot(env, *a);
            env.insert(
                *dst,
                SymExpr::Mul(Box::new(a), Box::new(SymExpr::Const(*val as i64))).simplify(),
            );
        }
        AndLit { dst, a, val } => {
            // calcite's AndLit: lowerBytes(a, val) = a & ((1<<val)-1)
            let a = read_slot(env, *a);
            env.insert(
                *dst,
                SymExpr::LowerBytes(Box::new(a), Box::new(SymExpr::Const(*val as i64))).simplify(),
            );
        }
        ShlLit { dst, a, val } => {
            let a = read_slot(env, *a);
            env.insert(
                *dst,
                SymExpr::Shl(Box::new(a), Box::new(SymExpr::Const(*val as i64))).simplify(),
            );
        }
        ShrLit { dst, a, val } => {
            let a = read_slot(env, *a);
            env.insert(
                *dst,
                SymExpr::Shr(Box::new(a), Box::new(SymExpr::Const(*val as i64))).simplify(),
            );
        }
        ModLit { dst, a, val } => {
            let a = read_slot(env, *a);
            env.insert(
                *dst,
                SymExpr::Mod(Box::new(a), Box::new(SymExpr::Const(*val as i64))).simplify(),
            );
        }
        Add { dst, a, b } => bin_op(env, *dst, *a, *b, |x, y| SymExpr::Add(Box::new(x), Box::new(y))),
        Sub { dst, a, b } => bin_op(env, *dst, *a, *b, |x, y| SymExpr::Sub(Box::new(x), Box::new(y))),
        Mul { dst, a, b } => bin_op(env, *dst, *a, *b, |x, y| SymExpr::Mul(Box::new(x), Box::new(y))),
        And { dst, a, b } => bin_op(env, *dst, *a, *b, |x, y| SymExpr::LowerBytes(Box::new(x), Box::new(y))),
        Shl { dst, a, b } => bin_op(env, *dst, *a, *b, |x, y| SymExpr::Shl(Box::new(x), Box::new(y))),
        Shr { dst, a, b } => bin_op(env, *dst, *a, *b, |x, y| SymExpr::Shr(Box::new(x), Box::new(y))),
        Mod { dst, a, b } => bin_op(env, *dst, *a, *b, |x, y| SymExpr::Mod(Box::new(x), Box::new(y))),
        BitAnd16 { dst, a, b } => bin_op(env, *dst, *a, *b, |x, y| SymExpr::BitAnd16(Box::new(x), Box::new(y))),
        BitOr16  { dst, a, b } => bin_op(env, *dst, *a, *b, |x, y| SymExpr::BitOr16(Box::new(x), Box::new(y))),
        BitXor16 { dst, a, b } => bin_op(env, *dst, *a, *b, |x, y| SymExpr::BitXor16(Box::new(x), Box::new(y))),
        BitNot16 { dst, a } => {
            let ax = read_slot(env, *a);
            env.insert(*dst, SymExpr::BitNot16(Box::new(ax)).simplify());
        }
        Neg { dst, src } => {
            let v = read_slot(env, *src);
            env.insert(*dst, SymExpr::Neg(Box::new(v)).simplify());
        }
        Floor { dst, src } => {
            let v = read_slot(env, *src);
            env.insert(*dst, SymExpr::Floor(Box::new(v)).simplify());
        }
        Div { dst, a, b } => bin_op(env, *dst, *a, *b, |x, y| SymExpr::Div(Box::new(x), Box::new(y))),
        Bit { dst, val, idx } => {
            let v = read_slot(env, *val);
            let i = read_slot(env, *idx);
            env.insert(*dst, SymExpr::BitExtract(Box::new(v), Box::new(i)).simplify());
        }
        Min { dst, args } => {
            let xs: Vec<SymExpr> = args.iter().map(|s| read_slot(env, *s)).collect();
            env.insert(*dst, SymExpr::Min(xs).simplify());
        }
        Max { dst, args } => {
            let xs: Vec<SymExpr> = args.iter().map(|s| read_slot(env, *s)).collect();
            env.insert(*dst, SymExpr::Max(xs).simplify());
        }
        Abs { dst, src } => {
            let v = read_slot(env, *src);
            env.insert(*dst, SymExpr::Abs(Box::new(v)).simplify());
        }
        Sign { dst, src } => {
            let v = read_slot(env, *src);
            env.insert(*dst, SymExpr::Sign(Box::new(v)).simplify());
        }
        // Round: dst = round(val, interval) per RoundStrategy. For symbolic
        // composition, we represent it by collapsing to the val expression
        // when interval is Const(1) (round-to-int is a no-op on integers
        // for our purposes); otherwise bail. This matches the typical
        // calcite emit pattern where Round-with-interval-1 is just Floor.
        Round { dst, strategy: _, val, interval } => {
            let v = read_slot(env, *val);
            let i = read_slot(env, *interval);
            if let SymExpr::Const(1) = i {
                env.insert(*dst, SymExpr::Floor(Box::new(v)).simplify());
            } else {
                return Err(BailReason::Unsupported("Round (non-1 interval)"));
            }
        }
        Clamp { dst, min, val, max } => {
            // clamp(val, min, max) = max(min, min(val, max))
            let mn = read_slot(env, *min);
            let v = read_slot(env, *val);
            let mx = read_slot(env, *max);
            let inner = SymExpr::Min(vec![v, mx]);
            let outer = SymExpr::Max(vec![mn, inner]);
            env.insert(*dst, outer.simplify());
        }
        CmpEq { dst, a, b } => {
            // Symbolic equality: only resolvable if both Const.
            let ax = read_slot(env, *a);
            let bx = read_slot(env, *b);
            if let (SymExpr::Const(x), SymExpr::Const(y)) = (&ax, &bx) {
                env.insert(*dst, SymExpr::Const(if x == y { 1 } else { 0 }));
            } else {
                return Err(BailReason::Unsupported("CmpEq (non-const operands)"));
            }
        }

        // ---- Assumption-aware loads + branches ----
        LoadState { dst, addr } => {
            // Resolve to Const if assumed; otherwise bail.
            if let Some(&v) = assumptions.state_vars.get(addr) {
                env.insert(*dst, SymExpr::Const(v as i64));
            } else {
                return Err(BailReason::SideEffect("LoadState"));
            }
        }
        LoadStateAndBranchIfNotEqLit { dst, addr, val, target } => {
            // Resolve LoadState under assumption, then statically resolve branch.
            if let Some(&v) = assumptions.state_vars.get(addr) {
                env.insert(*dst, SymExpr::Const(v as i64));
                if v != *val {
                    return Ok(StepOutcome::Jump(*target as usize));
                }
            } else {
                return Err(BailReason::SideEffect("LoadState"));
            }
        }
        BranchIfNotEqLit { a, val, target } => {
            // If slot a is Const, statically resolve.
            let cond = read_slot(env, *a);
            if let SymExpr::Const(v) = cond {
                if v != *val as i64 {
                    return Ok(StepOutcome::Jump(*target as usize));
                }
            } else {
                return Err(BailReason::Branch("BranchIfNotEqLit (non-const)"));
            }
        }
        BranchIfZero { cond, target } => {
            let c = read_slot(env, *cond);
            if let SymExpr::Const(v) = c {
                if v == 0 {
                    return Ok(StepOutcome::Jump(*target as usize));
                }
            } else {
                return Err(BailReason::Branch("BranchIfZero (non-const)"));
            }
        }
        Jump { target } => {
            return Ok(StepOutcome::Jump(*target as usize));
        }

        // ---- Out of scope ----
        StoreState { .. } | StoreMem { .. } => return Err(BailReason::SideEffect("StoreState/StoreMem")),
        LoadMem { .. } | LoadMem16 { .. } | LoadPackedByte { .. } => {
            return Err(BailReason::SideEffect("LoadMem*"));
        }
        Dispatch { .. } | DispatchFlatArray { .. } | DispatchChain { .. } => {
            return Err(BailReason::Unsupported("dispatch*"));
        }
        Call { .. } => return Err(BailReason::Unsupported("Call")),
        MemoryFill { .. } | MemoryCopy { .. } => {
            return Err(BailReason::SideEffect("Memory{Fill,Copy}"));
        }
        ReplicatedBody { .. } => return Err(BailReason::Unsupported("ReplicatedBody")),
        // Catch-all for variants not yet supported (Div, Pow, Min, Max, Clamp,
        // Round, Abs, Sign, CmpEq, Bit). Add support as we encounter them.
        other => return Err(BailReason::Unsupported(op_variant_name(other))),
    }
    Ok(StepOutcome::Next)
}

fn bin_op(
    env: &mut SlotEnv,
    dst: Slot,
    a: Slot,
    b: Slot,
    build: impl FnOnce(SymExpr, SymExpr) -> SymExpr,
) {
    let ax = read_slot(env, a);
    let bx = read_slot(env, b);
    env.insert(dst, build(ax, bx).simplify());
}

/// Best-effort variant name for the catch-all bail message.
fn op_variant_name(op: &Op) -> &'static str {
    use Op::*;
    match op {
        Div { .. } => "Div",
        Pow { .. } => "Pow",
        Min { .. } => "Min",
        Max { .. } => "Max",
        Clamp { .. } => "Clamp",
        Round { .. } => "Round",
        Abs { .. } => "Abs",
        Sign { .. } => "Sign",
        CmpEq { .. } => "CmpEq",
        Bit { .. } => "Bit",
        _ => "<unsupported>",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn const_propagation_through_addlit_chain() {
        let mut env: SlotEnv = HashMap::new();
        env.insert(0, SymExpr::Const(10));
        let ops = vec![
            Op::AddLit { dst: 0, a: 0, val: 5 },
            Op::AddLit { dst: 0, a: 0, val: 7 },
            Op::AddLit { dst: 0, a: 0, val: -3 },
        ];
        simulate_ops(&mut env, &ops).unwrap();
        assert_eq!(env[&0], SymExpr::Const(19));
    }

    #[test]
    fn slot_input_threads_through() {
        // env[1] = AddLit(env[0], 5) where env[0] is unset (free Slot(0)).
        let mut env: SlotEnv = HashMap::new();
        let ops = vec![Op::AddLit { dst: 1, a: 0, val: 5 }];
        simulate_ops(&mut env, &ops).unwrap();
        assert_eq!(
            env[&1],
            SymExpr::Add(Box::new(SymExpr::Slot(0)), Box::new(SymExpr::Const(5)))
        );
    }

    #[test]
    fn move_chain_collapses() {
        // r1 := r0 + 3; r2 := r1; r3 := r2 + 1 → r3 == r0 + 3 + 1
        let mut env: SlotEnv = HashMap::new();
        let ops = vec![
            Op::AddLit { dst: 1, a: 0, val: 3 },
            Op::LoadSlot { dst: 2, src: 1 },
            Op::AddLit { dst: 3, a: 2, val: 1 },
        ];
        simulate_ops(&mut env, &ops).unwrap();
        // We want r3 == Slot(0) + 3 + 1 ideally, but our simplifier doesn't
        // associate, so we expect ((Slot(0) + 3) + 1).
        let expected = SymExpr::Add(
            Box::new(SymExpr::Add(
                Box::new(SymExpr::Slot(0)),
                Box::new(SymExpr::Const(3)),
            )),
            Box::new(SymExpr::Const(1)),
        );
        assert_eq!(env[&3], expected);
    }

    #[test]
    fn concrete_evaluation_matches() {
        // Build a small composed expression and concretise it against a slot env.
        let mut env: SlotEnv = HashMap::new();
        let ops = vec![
            Op::LoadLit { dst: 0, val: 10 },
            Op::AddLit { dst: 1, a: 0, val: 5 },
            Op::Add { dst: 2, a: 0, b: 1 }, // 10 + 15 = 25
            Op::ShlLit { dst: 3, a: 2, val: 1 }, // 25 << 1 = 50
        ];
        simulate_ops(&mut env, &ops).unwrap();
        let concrete: HashMap<Slot, i64> = HashMap::new();
        let v = env[&3].eval_concrete(&concrete);
        assert_eq!(v, Some(50));
        // Also: works with a free slot if we provide it.
        let mut env2: SlotEnv = HashMap::new();
        let ops2 = vec![
            Op::AddLit { dst: 1, a: 0, val: 5 },  // r1 = r0 + 5
            Op::ShlLit { dst: 2, a: 1, val: 2 }, // r2 = (r0 + 5) << 2
        ];
        simulate_ops(&mut env2, &ops2).unwrap();
        let mut concrete2: HashMap<Slot, i64> = HashMap::new();
        concrete2.insert(0, 7);
        let v2 = env2[&2].eval_concrete(&concrete2);
        assert_eq!(v2, Some((7 + 5) << 2)); // 48
    }

    #[test]
    fn bails_on_branch() {
        let mut env: SlotEnv = HashMap::new();
        let ops = vec![Op::BranchIfNotEqLit { a: 0, val: 0, target: 5 }];
        let r = simulate_ops(&mut env, &ops);
        assert!(matches!(r, Err(BailReason::Branch(_))));
    }

    #[test]
    fn bails_on_memory() {
        let mut env: SlotEnv = HashMap::new();
        let ops = vec![Op::LoadMem { dst: 0, addr_slot: 1 }];
        let r = simulate_ops(&mut env, &ops);
        assert!(matches!(r, Err(BailReason::SideEffect(_))));
    }

    #[test]
    fn binary_op_composes_via_bitxor() {
        let mut env: SlotEnv = HashMap::new();
        // r2 := r0 + r1; r3 := r2 ^ r0 (true bitwise via BitXor16)
        let ops = vec![
            Op::Add { dst: 2, a: 0, b: 1 },
            Op::BitXor16 { dst: 3, a: 2, b: 0 },
        ];
        simulate_ops(&mut env, &ops).unwrap();
        let expected = SymExpr::BitXor16(
            Box::new(SymExpr::Add(
                Box::new(SymExpr::Slot(0)),
                Box::new(SymExpr::Slot(1)),
            )),
            Box::new(SymExpr::Slot(0)),
        );
        assert_eq!(env[&3], expected);
    }

    #[test]
    fn andlit_means_lower_bytes() {
        let mut env: SlotEnv = HashMap::new();
        // AndLit { val: 8 } should produce LowerBytes(slot, 8) — i.e., a & 0xFF.
        let ops = vec![Op::AndLit { dst: 1, a: 0, val: 8 }];
        simulate_ops(&mut env, &ops).unwrap();
        let expected = SymExpr::LowerBytes(
            Box::new(SymExpr::Slot(0)),
            Box::new(SymExpr::Const(8)),
        );
        assert_eq!(env[&1], expected);
        // Concrete check: 0x1234 with val=8 → 0x34.
        let mut concrete: HashMap<Slot, i64> = HashMap::new();
        concrete.insert(0, 0x1234);
        assert_eq!(env[&1].eval_concrete(&concrete), Some(0x34));
    }

    #[test]
    fn loadstate_resolves_under_assumption() {
        // LoadState normally bails. With an assumption, it produces Const.
        let mut env: SlotEnv = HashMap::new();
        let ops = vec![
            Op::LoadState { dst: 0, addr: 0xDEAD },
            Op::AddLit { dst: 1, a: 0, val: 7 },
        ];
        let asm = Assumptions::new().with(0xDEAD, 5);
        simulate_ops_with(&mut env, &ops, &asm).unwrap();
        // env[1] = 5 + 7 = 12 (folded).
        assert_eq!(env[&1], SymExpr::Const(12));
    }

    #[test]
    fn loadstate_bails_without_assumption() {
        let mut env: SlotEnv = HashMap::new();
        let ops = vec![Op::LoadState { dst: 0, addr: 0xDEAD }];
        let r = simulate_ops(&mut env, &ops);
        assert!(matches!(r, Err(BailReason::SideEffect(_))));
    }

    #[test]
    fn branch_resolves_when_condition_const() {
        // r0 = 0 (const). BranchIfZero target=2 should skip op[1].
        // The body sets r1 in op[1] and op[2]; if branch fires, only op[2] runs.
        let mut env: SlotEnv = HashMap::new();
        let ops = vec![
            Op::LoadLit { dst: 0, val: 0 },         // pc=0: r0 = 0
            Op::LoadLit { dst: 1, val: 100 },       // pc=1: r1 = 100  (skipped)
            Op::BranchIfZero { cond: 0, target: 4 }, // pc=2: r0==0, jump to pc=4
            Op::LoadLit { dst: 1, val: 200 },       // pc=3: skipped via Jump
            Op::LoadLit { dst: 1, val: 300 },       // pc=4: r1 = 300
        ];
        let asm = Assumptions::new();
        simulate_ops_with(&mut env, &ops, &asm).unwrap();
        // We took op[0], op[1] (r1=100), op[2] (jump to 4), op[4] (r1=300).
        // Final r1=300.
        assert_eq!(env[&1], SymExpr::Const(300));
    }

    #[test]
    fn loadstate_branch_combo_resolves() {
        // The shape we hit on doom8088: LoadState(--df) +
        // BranchIfNotEqLit(if df != 0, jump to target).
        // With assumption --df=0, branch falls through.
        let mut env: SlotEnv = HashMap::new();
        let ops = vec![
            Op::LoadStateAndBranchIfNotEqLit { dst: 0, addr: 0x500, val: 0, target: 3 },
            // primary path: r1 = 99 (df==0)
            Op::LoadLit { dst: 1, val: 99 },
            Op::Jump { target: 4 },
            // alt path: r1 = 11 (df != 0)
            Op::LoadLit { dst: 1, val: 11 },
            // end
            Op::LoadLit { dst: 2, val: 1 },
        ];
        let asm = Assumptions::new().with(0x500, 0);
        simulate_ops_with(&mut env, &ops, &asm).unwrap();
        assert_eq!(env[&1], SymExpr::Const(99));
        assert_eq!(env[&2], SymExpr::Const(1));
    }

    #[test]
    fn bitand16_truncates() {
        let mut env: SlotEnv = HashMap::new();
        env.insert(0, SymExpr::Const(0x12345));
        env.insert(1, SymExpr::Const(0xFFFF));
        let ops = vec![Op::BitAnd16 { dst: 2, a: 0, b: 1 }];
        simulate_ops(&mut env, &ops).unwrap();
        // Const-folded path: should equal Const(0x2345 & 0xFFFF) = 0x2345.
        assert_eq!(env[&2], SymExpr::Const(0x2345));
    }
}
