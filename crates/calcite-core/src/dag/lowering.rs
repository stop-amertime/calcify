//! ParsedProgram → DAG lowering.
//!
//! See `docs/v2-rewrite-design.md` § Lowering rules for the full spec.
//! Phase 1 is direct mechanical lowering with no idiom recognition;
//! every `Expr` variant maps to one or two `DagNode` variants.

use std::collections::HashMap;

use super::types::{
    CalcKind, Dag, DagBroadcast, DagNode, DagPackedBroadcast, NodeId, SlotId, StyleCondNode,
    TickPosition,
};
use crate::pattern::broadcast_write::BroadcastWrite;
use crate::pattern::packed_broadcast_write::PackedSlotPort;
use crate::types::{
    Assignment, CalcOp, Expr, FunctionDef, ParsedProgram, StyleBranch, StyleTest,
};

/// Strip `--`, then optional `__0`/`__1`/`__2` prefix. Returns the bare
/// property name plus a `TickPosition` flag indicating whether the read
/// is current or previous tick. Mirrors v1's `strip_prop_prefixes`
/// (`compile.rs:1064`) but also returns the tick position.
fn strip_var_name(name: &str) -> (&str, TickPosition) {
    let after_dashes = name.strip_prefix("--").unwrap_or(name);
    if let Some(rest) = after_dashes.strip_prefix("__0") {
        (rest, TickPosition::Prev)
    } else if let Some(rest) = after_dashes.strip_prefix("__1") {
        (rest, TickPosition::Prev)
    } else if let Some(rest) = after_dashes.strip_prefix("__2") {
        (rest, TickPosition::Prev)
    } else {
        (after_dashes, TickPosition::Current)
    }
}

/// True if the assignment property is a triple-buffer copy
/// (`--__0*`, `--__1*`, `--__2*`). Mirrors v1's `is_buffer_copy`
/// (`eval.rs:1351`). Buffer copies are skipped in lowering.
fn is_buffer_copy(name: &str) -> bool {
    name.starts_with("--__0") || name.starts_with("--__1") || name.starts_with("--__2")
}

/// Resolve a bare property name to its committed slot (state var or
/// memory address). Returns `None` for properties that don't have a
/// committed slot — the caller must allocate a transient slot for
/// these.
fn committed_slot_of(name: &str) -> Option<SlotId> {
    crate::eval::property_to_address(&format!("--{name}"))
}

/// True if `id` references a `Lit(v)` whose value bit-equals `expected`.
fn is_lit(dag: &Dag, id: NodeId, expected: f64) -> bool {
    matches!(dag.nodes[id as usize], DagNode::Lit(v) if v.to_bits() == expected.to_bits())
}

/// True if `id` references `Param(idx)`.
fn is_param(dag: &Dag, id: NodeId, idx: u32) -> bool {
    matches!(dag.nodes[id as usize], DagNode::Param(i) if i == idx)
}

/// Find the largest subset of `(key, slot)` pairs whose values lie on
/// a single line `slot = a + b * key` for integer `a`, `b` representable
/// as `i32`. Returns the subset (sorted by key) and the line params.
///
/// The returned subset has length 0 or 1 when no line fits at least
/// two points; line_params is `None` in those cases. For 2+ pairs,
/// line_params is `Some((a, b))`.
///
/// Strategy: for each ordered pair of distinct-key points, fit a
/// candidate line and count how many other pairs lie on it. Return
/// the line with the largest support set. O(n²) in pair count; fine
/// for the table sizes Phase 2 sees (millions of entries are still
/// quadratic but every entry is a constant-time check, and in practice
/// the first candidate line catches everything when the table really
/// is a clean progression). For large tables, an early-exit is the
/// natural extension if quadratic blows up.
fn largest_line_fit(pairs: &[(i64, SlotId)]) -> (Vec<(i64, SlotId)>, Option<(i32, i32)>) {
    if pairs.len() < 2 {
        return (pairs.to_vec(), None);
    }
    // Fast path: test the line through the first two points first. Real
    // cabinets typically have one giant clean progression and we want to
    // avoid the O(n²) outer loop in that common case.
    let candidate = fit_line_through(pairs[0], pairs[1]);
    if let Some((a, b)) = candidate {
        let mut on_line: Vec<(i64, SlotId)> = pairs
            .iter()
            .copied()
            .filter(|&(k, s)| (a as i64) + (b as i64) * k == s as i64)
            .collect();
        if on_line.len() == pairs.len() {
            on_line.sort_unstable_by_key(|p| p.0);
            return (on_line, Some((a, b)));
        }
    }
    // Slower fallback: exhaustively try lines through every pair.
    // Capped at a small number of candidate seeds to avoid quadratic
    // blowup on adversarial inputs; in practice the first-two-points
    // candidate already covers the common case.
    let cap = pairs.len().min(64);
    let mut best: (Vec<(i64, SlotId)>, Option<(i32, i32)>) = (Vec::new(), None);
    for i in 0..cap {
        for j in (i + 1)..cap {
            let Some((a, b)) = fit_line_through(pairs[i], pairs[j]) else {
                continue;
            };
            let on_line: Vec<(i64, SlotId)> = pairs
                .iter()
                .copied()
                .filter(|&(k, s)| (a as i64) + (b as i64) * k == s as i64)
                .collect();
            if on_line.len() > best.0.len() {
                best = (on_line, Some((a, b)));
            }
        }
    }
    best.0.sort_unstable_by_key(|p| p.0);
    best
}

/// Fit a line `slot = a + b * key` through two points. Returns `None`
/// if the keys are equal (vertical line is degenerate) or if `a` or
/// `b` doesn't fit in i32.
fn fit_line_through(p0: (i64, SlotId), p1: (i64, SlotId)) -> Option<(i32, i32)> {
    let (k0, s0) = p0;
    let (k1, s1) = p1;
    let dk = k1 - k0;
    if dk == 0 {
        return None;
    }
    let ds = (s1 as i64) - (s0 as i64);
    if ds % dk != 0 {
        return None;
    }
    let b = ds / dk;
    let a = (s0 as i64) - b * k0;
    let i32_range = (i32::MIN as i64)..=(i32::MAX as i64);
    if !i32_range.contains(&a) || !i32_range.contains(&b) {
        return None;
    }
    Some((a as i32, b as i32))
}

/// Lowering context: a fresh DAG plus a transient-slot allocator for
/// property names that don't resolve to committed state.
struct LowerCtx<'a> {
    dag: Dag,
    /// Bare-name → SlotId for transient (per-tick scratch) properties.
    /// Allocated lazily as references and assignments reach unknown
    /// property names. Slot ids are encoded `TRANSIENT_BASE + idx`.
    transient_slots: HashMap<String, SlotId>,
    /// Bare property name → SlotId, populated lazily as `slot_for`
    /// resolves names. Lowering-only cache: every value the walker
    /// needs is already baked into the resolved `SlotId`s on `LoadVar`,
    /// `WriteVar`, and the `DagBroadcast`/`DagPackedBroadcast` records,
    /// so this map doesn't need to survive into the runtime `Dag`.
    name_to_slot: HashMap<String, SlotId>,
    /// Set of bare property names that appear as the LHS of some
    /// assignment. Used by `Expr::Var` lowering to distinguish
    /// "name is computed by this program but not @property-declared"
    /// (→ allocate transient, ignore var() fallback) from "name is
    /// genuinely unresolved" (→ use var() fallback per CSS spec).
    assigned_names: std::collections::HashSet<String>,
    /// Function definitions, keyed by `--name`. Borrowed from the
    /// program for the duration of lowering so call sites can resolve
    /// callees and inline their bodies.
    functions: HashMap<String, &'a FunctionDef>,
    /// Active inlining stack: each entry is a frame mapping
    /// param/local name → NodeId. Innermost frame is last. A `var(--p)`
    /// reference scans frames innermost-first; if found, it resolves to
    /// the bound NodeId (the call-site arg or local's lowered body).
    /// Mirrors v1's `self.properties` shadow-and-restore in
    /// `eval_function_call`.
    inline_frames: Vec<HashMap<String, NodeId>>,
    /// When true, every `Expr::Var` lowering uses `TickPosition::Prev`
    /// regardless of the property name. Set while lowering broadcast
    /// value expressions, which run against committed state (after all
    /// `WriteVar` terminals have flushed their per-tick-cache entries
    /// to State). Default false for normal assignment lowering.
    force_prev_reads: bool,
    /// Hash-cons caches for the high-frequency pure node kinds. Without
    /// these, inlining N call sites of an identical function body emits
    /// N separate copies — on a 362K-assignment cabinet (zork) that
    /// blows past tens of GB of node memory and OOMs. With them,
    /// identical sub-expressions collapse to a single NodeId, so the
    /// DAG actually behaves like a DAG.
    ///
    /// We only intern the leaf-and-near-leaf kinds (Lit, LoadVar, Calc).
    /// If/Switch/Concat/Call are composed of NodeIds that themselves
    /// got interned, so the structural sharing already happens via their
    /// children — the parent nodes themselves are rare enough that
    /// hashing them is not worth the cost.
    lit_cache: HashMap<u64, NodeId>,
    load_var_cache: HashMap<(SlotId, TickPosition), NodeId>,
    calc_cache: HashMap<(CalcKind, Vec<NodeId>), NodeId>,
    /// Classification of each `@function` body as a bit-slicer shape
    /// (idiom 7). Indexed by `fn_id`. `None` for unclassified or non-
    /// slicer functions. Populated immediately after
    /// `lower_all_function_bodies` runs; consulted at top-level Call
    /// lowering so a `--lowerBytes(X, 8)` call site emits a `BitField`
    /// instead of a `Call`. Lowering-only — slicer rewrites happen at
    /// the call site, so the runtime `Dag` doesn't need to carry this.
    slicer_kinds: Vec<Option<SlicerKind>>,
    /// Classification of each `@function` body as a 16-bit bitwise op
    /// (idiom 8: AND/OR/XOR over two-input bit-decomposition; NOT over
    /// one-input). Indexed by `fn_id`. `None` for unclassified or
    /// non-bitwise functions. Populated alongside `slicer_kinds`;
    /// consulted at every Call lowering — any call to a classified
    /// bitwise function rewrites to a `BitwiseOp` super-node
    /// regardless of the argument shape (the op is fully general).
    bitwise_kinds: Vec<Option<crate::compile::BitwiseKind>>,
}

/// Bit-slicer body shapes (idiom 7). All three reduce to a `BitField`
/// at literal-arg call sites:
///
/// - `LowerBytes`: `mod(Param(0), pow(2, Param(1)))` → mask to N low
///   bits. At the call site `--lowerBytes(X, n)` with literal `n` ≥ 1,
///   emit `BitField { src: X, shift: 0, width: Some(n) }`.
/// - `RightShift`: `round(down, Param(0) / pow(2, Param(1)), 1)` →
///   floor-div by 2^N, equivalent to arithmetic right shift on i32 for
///   any N. At `--rightShift(X, n)` with literal `n`, emit
///   `BitField { src: X, shift: n, width: None }`.
/// - `Bit`: `mod(<RightShift body>(Param(0), Param(1)), 2)` → single-
///   bit extract. At `--bit(X, i)` with literal `i`, emit
///   `BitField { src: X, shift: i, width: Some(1) }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlicerKind {
    LowerBytes,
    RightShift,
    Bit,
}

impl<'a> LowerCtx<'a> {
    fn new() -> Self {
        Self {
            dag: Dag::default(),
            transient_slots: HashMap::new(),
            name_to_slot: HashMap::new(),
            assigned_names: std::collections::HashSet::new(),
            functions: HashMap::new(),
            inline_frames: Vec::new(),
            force_prev_reads: false,
            lit_cache: HashMap::new(),
            load_var_cache: HashMap::new(),
            calc_cache: HashMap::new(),
            slicer_kinds: Vec::new(),
            bitwise_kinds: Vec::new(),
        }
    }

    /// Hash-cons a `Lit(v)` node — return an existing NodeId if we've
    /// already produced one for the same bit pattern, otherwise push a
    /// fresh one. NaN handling: NaN compares unequal to itself, but
    /// `to_bits()` is reflexive, so two NaNs with identical bit
    /// patterns share. Different-bit NaNs don't share, which is fine.
    fn intern_lit(&mut self, v: f64) -> NodeId {
        let key = v.to_bits();
        if let Some(&id) = self.lit_cache.get(&key) {
            return id;
        }
        let id = self.dag.push(DagNode::Lit(v));
        self.lit_cache.insert(key, id);
        id
    }

    fn intern_load_var(&mut self, slot: SlotId, kind: TickPosition) -> NodeId {
        if let Some(&id) = self.load_var_cache.get(&(slot, kind)) {
            return id;
        }
        let id = self.dag.push(DagNode::LoadVar { slot, kind });
        self.load_var_cache.insert((slot, kind), id);
        id
    }

    fn intern_calc(&mut self, op: CalcKind, args: Vec<NodeId>) -> NodeId {
        let key = (op, args);
        if let Some(&id) = self.calc_cache.get(&key) {
            return id;
        }
        let (op, args) = key;
        let id = self.dag.push(DagNode::Calc {
            op: op.clone(),
            args: args.clone(),
        });
        self.calc_cache.insert((op, args), id);
        id
    }

    /// Look up a name in the active inline frames (innermost first).
    /// Used by `Expr::Var` lowering to short-circuit param/local
    /// references during function-body inlining.
    fn lookup_inline(&self, bare: &str) -> Option<NodeId> {
        for frame in self.inline_frames.iter().rev() {
            if let Some(&id) = frame.get(bare) {
                return Some(id);
            }
        }
        None
    }

    fn intern_function(&mut self, name: &str) -> u32 {
        if let Some(&id) = self.dag.function_names.get(name) {
            return id;
        }
        let id = self.dag.function_names.len() as u32;
        self.dag.function_names.insert(name.to_string(), id);
        id
    }

    /// Look up or allocate a slot for a bare property name. State-var
    /// and memory slots come from the global address map; transients
    /// are allocated on demand and don't commit to State.
    ///
    /// The `name_to_slot` map lives only on the lowering context — by
    /// the time the runtime `Dag` is built, every slot reference is
    /// already baked into a `LoadVar`/`WriteVar`/broadcast record, so
    /// the map has no runtime consumer.
    fn slot_for(&mut self, bare: &str) -> SlotId {
        if let Some(&s) = self.name_to_slot.get(bare) {
            return s;
        }
        let slot = if let Some(s) = committed_slot_of(bare) {
            s
        } else {
            let id = crate::dag::TRANSIENT_BASE + self.transient_slots.len() as SlotId;
            self.transient_slots.insert(bare.to_string(), id);
            id
        };
        self.name_to_slot.insert(bare.to_string(), slot);
        slot
    }

    /// Lower an `Expr` to a `NodeId`. Pure (no side effects beyond
    /// allocating new nodes). Hot-path leaves (Lit, LoadVar, Calc) go
    /// through hash-cons interns so identical sub-expressions share a
    /// NodeId — required for inlining at this scale to fit in memory.
    fn lower_expr(&mut self, expr: &Expr) -> NodeId {
        match expr {
            Expr::Literal(v) => self.intern_lit(*v),

            Expr::StringLiteral(s) => self.dag.push(DagNode::LitStr(s.clone())),

            Expr::Var { name, fallback } => {
                let (bare, kind) = strip_var_name(name);
                // Broadcast value expressions read committed state — the
                // executor runs after every WriteVar terminal has flushed
                // its per-tick cache entry to State. Force `Prev` reads
                // (which bypass the cache) so we don't accidentally pick
                // up scalar-walker per-tick scratch values. Inline-frame
                // lookups (param/local references inside an inlined
                // function body) take precedence — they refer to
                // call-bound NodeIds, not slot reads.
                let kind = if self.force_prev_reads && self.lookup_inline(bare).is_none() {
                    TickPosition::Prev
                } else {
                    kind
                };
                // 1. Inline frame check (param/local of the function
                //    we're currently inlining). This must come before
                //    the global-slot resolution: param names like
                //    `--at` aren't tick-global, they're call-bound.
                //    Triple-buffer prefixes (`--__1at`) on a param read
                //    don't make sense (params are always current-call),
                //    so we only consult the inline frame for current
                //    reads.
                if matches!(kind, TickPosition::Current) {
                    if let Some(id) = self.lookup_inline(bare) {
                        return id;
                    }
                }
                // 2. CSS spec: `var(--x, fb)` evaluates fb only when
                //    --x is unresolved. "Unresolved" means: --x has
                //    neither a registered @property declaration nor
                //    any assignment that produces it. v2 distinguishes:
                //   - committed_slot_of(bare).is_some() → state var
                //     or memory address → resolved.
                //   - assigned_names.contains(bare) → an assignment
                //     produces it (transient if not committed) →
                //     resolved, allocate slot.
                //   - neither → unresolved, use fallback or 0.
                if committed_slot_of(bare).is_some() || self.assigned_names.contains(bare) {
                    let slot = self.slot_for(bare);
                    self.intern_load_var(slot, kind)
                } else if let Some(fb) = fallback.as_deref() {
                    self.lower_expr(fb)
                } else {
                    // Unresolved, no fallback: CSS spec falls back to
                    // the property's registered initial value, which
                    // is 0 for unregistered numeric properties.
                    self.intern_lit(0.0)
                }
            }

            Expr::Calc(op) => {
                let (kind, args) = match op {
                    CalcOp::Add(a, b) => (CalcKind::Add, vec![self.lower_expr(a), self.lower_expr(b)]),
                    CalcOp::Sub(a, b) => (CalcKind::Sub, vec![self.lower_expr(a), self.lower_expr(b)]),
                    CalcOp::Mul(a, b) => (CalcKind::Mul, vec![self.lower_expr(a), self.lower_expr(b)]),
                    CalcOp::Div(a, b) => (CalcKind::Div, vec![self.lower_expr(a), self.lower_expr(b)]),
                    CalcOp::Mod(a, b) => (CalcKind::Mod, vec![self.lower_expr(a), self.lower_expr(b)]),
                    CalcOp::Pow(a, b) => (CalcKind::Pow, vec![self.lower_expr(a), self.lower_expr(b)]),
                    CalcOp::Min(args) => {
                        let lowered: Vec<NodeId> = args.iter().map(|e| self.lower_expr(e)).collect();
                        (CalcKind::Min, lowered)
                    }
                    CalcOp::Max(args) => {
                        let lowered: Vec<NodeId> = args.iter().map(|e| self.lower_expr(e)).collect();
                        (CalcKind::Max, lowered)
                    }
                    CalcOp::Clamp(a, b, c) => (
                        CalcKind::Clamp,
                        vec![self.lower_expr(a), self.lower_expr(b), self.lower_expr(c)],
                    ),
                    CalcOp::Round(strategy, val, interval) => (
                        CalcKind::Round(*strategy),
                        vec![self.lower_expr(val), self.lower_expr(interval)],
                    ),
                    CalcOp::Sign(a) => (CalcKind::Sign, vec![self.lower_expr(a)]),
                    CalcOp::Abs(a) => (CalcKind::Abs, vec![self.lower_expr(a)]),
                    CalcOp::Negate(a) => (CalcKind::Neg, vec![self.lower_expr(a)]),
                };
                self.intern_calc(kind, args)
            }

            Expr::FunctionCall { name, args } => {
                // Lower args in caller scope first. The same NodeId is
                // referenced by every param substitution site, so the
                // arg expression evaluates exactly once per call —
                // matching v1's `eval_function_call` (which evaluates
                // each arg once at bind time).
                let arg_ids: Vec<NodeId> = args.iter().map(|e| self.lower_expr(e)).collect();
                self.lower_call(name, arg_ids)
            }

            Expr::StyleCondition { branches, fallback } => {
                self.lower_style_condition(branches, fallback)
            }

            Expr::Concat(parts) => {
                let lowered: Vec<NodeId> = parts.iter().map(|e| self.lower_expr(e)).collect();
                self.dag.push(DagNode::Concat(lowered))
            }
        }
    }

    /// Emit a `Call { fn_id, args }` node for a `@function` call.
    ///
    /// Function bodies are lowered once into the global node arena
    /// (see `lower_function_body`) — call sites just reference the
    /// shared body via `fn_id`. Args are caller-context NodeIds that
    /// the walker resolves into the body's `Param(i)` placeholders.
    ///
    /// This replaces the inline-at-lowering model that was O(call
    /// sites × body size) — combinatorial expansion that OOMs on
    /// real cabinets like zork (362K assignments × deep call graphs).
    /// New model is O(unique functions × body size + call sites).
    ///
    /// Unknown callees: CSS spec for an unresolved `@function` call is
    /// the registered initial value of the surrounding property —
    /// numerically 0 for `<integer>`-syntax props (our sole numeric
    /// type). Lower to `Lit(0.0)` directly. Recursion (the call graph
    /// not being a DAG) is rejected by CSS at parse time and never
    /// reaches lowering; if a forward reference somehow appears here
    /// it's a lowering-order bug, not a runtime concern.
    fn lower_call(&mut self, name: &str, arg_ids: Vec<NodeId>) -> NodeId {
        let fn_id = match self.dag.function_names.get(name).copied() {
            Some(id) => id,
            None => return self.intern_lit(0.0),
        };
        // Pad missing args with Lit(0.0), matching v1 semantics
        // (`unwrap_or(Value::Number(0.0))` in eval_function_call).
        let expected = self.dag.function_param_counts.get(fn_id as usize).copied().unwrap_or(0) as usize;
        let mut padded = arg_ids;
        while padded.len() < expected {
            padded.push(self.intern_lit(0.0));
        }
        padded.truncate(expected);
        // Idiom 7: bit-slicer call site with a literal shift / width
        // collapses to a `BitField` super-node. Falls through to a
        // generic `Call` if the function isn't classified or the
        // shift arg isn't a literal in range.
        if let Some(node) = self.try_lower_slicer_call(fn_id, &padded) {
            return node;
        }
        // Idiom 8: native 16-bit bitwise op. Any call to a function
        // whose body matched the bit-decomposition shape rewrites
        // unconditionally — the op is fully general, no per-call shape
        // requirement.
        if let Some(node) = self.try_lower_bitwise_call(fn_id, &padded) {
            return node;
        }
        self.dag.push(DagNode::Call { fn_id, args: padded })
    }

    /// Try rewriting a call to a classified bitwise function as a
    /// `BitwiseOp` super-node. Returns `None` if the function isn't
    /// classified as a bitwise op.
    fn try_lower_bitwise_call(&mut self, fn_id: u32, args: &[NodeId]) -> Option<NodeId> {
        use crate::compile::BitwiseKind as V1Kind;
        use crate::dag::types::BitwiseKind as V2Kind;
        let v1_kind = (*self.bitwise_kinds.get(fn_id as usize)?)?;
        let kind = match v1_kind {
            V1Kind::And => V2Kind::And,
            V1Kind::Or => V2Kind::Or,
            V1Kind::Xor => V2Kind::Xor,
            V1Kind::Not => V2Kind::Not,
        };
        let expected_arity = if matches!(kind, V2Kind::Not) { 1 } else { 2 };
        if args.len() != expected_arity {
            return None;
        }
        Some(self.dag.push(DagNode::BitwiseOp {
            kind,
            args: args.to_vec(),
        }))
    }

    /// Try rewriting a call to a classified bit-slicer function as a
    /// `BitField` super-node. Returns `None` if the function isn't a
    /// slicer, or if the second arg isn't a literal integer in `0..=31`.
    fn try_lower_slicer_call(&mut self, fn_id: u32, args: &[NodeId]) -> Option<NodeId> {
        let kind = (*self.slicer_kinds.get(fn_id as usize)?)?;
        if args.len() != 2 {
            return None;
        }
        // The shift / width / bit-index arg must be a literal in range.
        let n = match self.dag.nodes[args[1] as usize] {
            DagNode::Lit(v) if v.is_finite() && v >= 0.0 && v <= 31.0 && v.fract() == 0.0 => {
                v as u8
            }
            _ => return None,
        };
        let src = args[0];
        Some(match kind {
            // mod(X, pow(2, n)) = X & ((1 << n) - 1) for non-negative X,
            // and equals the floored modulo for negative X — both
            // representable as `(X >> 0) & mask`. n=0 produces width=0
            // which is invalid (mask = 0 means always-zero result, which
            // does match `mod(X, 1) == 0` exactly), so we still emit it
            // but with width clamped to 1 to keep the invariant
            // 1<=width<=32. Width n=0 case: mod(X, pow(2, 0)) = mod(X, 1)
            // = 0; reject so the call falls back to the generic body
            // (which evaluates the same).
            SlicerKind::LowerBytes if n == 0 => return None,
            SlicerKind::LowerBytes => self.dag.push(DagNode::BitField {
                src,
                shift: 0,
                width: Some(n),
            }),
            // round(down, X / pow(2, n), 1): pure shift, no mask.
            SlicerKind::RightShift => self.dag.push(DagNode::BitField {
                src,
                shift: n,
                width: None,
            }),
            // mod(rightShift(X, i), 2): single-bit extract.
            SlicerKind::Bit => self.dag.push(DagNode::BitField {
                src,
                shift: n,
                width: Some(1),
            }),
        })
    }

    /// Lower one `@function` body into the global node arena, using
    /// `Param(i)` placeholders for parameter references and stashing
    /// local bindings in the inline-frame stack so locals can be
    /// referenced by subsequent locals and the result.
    ///
    /// Returns the result NodeId (root of the body sub-DAG) and the
    /// parameter count.
    fn lower_function_body(&mut self, func: &FunctionDef) -> (NodeId, u32) {
        let n_params = func.parameters.len() as u32;

        // Build the param frame: each param is a Param(i) node. Param
        // nodes are NOT hash-consed — each function gets fresh Param
        // nodes scoped to its own body, since `Param(0)` of function A
        // is a different value from `Param(0)` of function B.
        let mut frame: HashMap<String, NodeId> = HashMap::new();
        for (i, param) in func.parameters.iter().enumerate() {
            let bare = param.name.strip_prefix("--").unwrap_or(&param.name).to_string();
            let id = self.dag.push(DagNode::Param(i as u32));
            frame.insert(bare, id);
        }

        // Use the inline-frame stack as the symbol table: param + local
        // bindings go here; `Expr::Var` lowering scans innermost-first.
        // A function body has only its own frame on the stack.
        self.inline_frames.push(frame);

        // Locals lowered sequentially — each may reference earlier locals
        // and any param.
        for local in &func.locals {
            let value_id = self.lower_expr(&local.value);
            let bare = local.name.strip_prefix("--").unwrap_or(&local.name).to_string();
            self.inline_frames
                .last_mut()
                .expect("frame just pushed")
                .insert(bare, value_id);
        }

        let result_id = self.lower_expr(&func.result);

        self.inline_frames.pop();
        (result_id, n_params)
    }

    /// Lower every declared `@function` body once, in declaration order.
    /// Populates `dag.function_names`, `dag.function_roots`, and
    /// `dag.function_param_counts`. Must run before top-level
    /// assignments are lowered so that `Expr::FunctionCall` lowering
    /// can resolve callees.
    fn lower_all_function_bodies(&mut self, functions: &'a [FunctionDef]) {
        // Pre-pass: populate function_param_counts so that any forward
        // call site (function A's body lowering emits Call to function
        // B that hasn't been lowered yet) can correctly pad its arg
        // list. Without this, B's param_count reads as 0 and lower_call
        // truncates args to nothing — the symptom is calls returning
        // 0 instead of computed values.
        for func in functions {
            let fn_id = self.intern_function(&func.name);
            if (fn_id as usize) >= self.dag.function_param_counts.len() {
                self.dag
                    .function_param_counts
                    .resize(fn_id as usize + 1, 0);
                self.dag
                    .function_roots
                    .resize(fn_id as usize + 1, NodeId::MAX);
            }
            self.dag.function_param_counts[fn_id as usize] = func.parameters.len() as u32;
        }
        for func in functions {
            let fn_id = self.intern_function(&func.name);
            // Hash-cons caches must NOT be carried across function
            // boundaries: a `Param(0)` placeholder in function A is a
            // different node from a `Param(0)` placeholder in function
            // B, but their LoadVar/Lit children might collide on key.
            // In practice Lit and LoadVar interns ARE safe to share
            // (a literal 5 is a literal 5 anywhere), but Calc nodes
            // built over Param children must not be reused across
            // functions because Param NodeIds differ. To stay safe,
            // we just clear the calc cache per function.
            self.calc_cache.clear();
            let (root, n_params) = self.lower_function_body(func);
            // function_roots / function_param_counts are indexed by
            // fn_id — extend if needed.
            if (fn_id as usize) >= self.dag.function_roots.len() {
                self.dag
                    .function_roots
                    .resize(fn_id as usize + 1, NodeId::MAX);
                self.dag
                    .function_param_counts
                    .resize(fn_id as usize + 1, 0);
            }
            self.dag.function_roots[fn_id as usize] = root;
            self.dag.function_param_counts[fn_id as usize] = n_params;
        }
        // Top-level lowering is a fresh scope — clear the calc cache
        // again so call sites don't accidentally reuse a Calc node
        // built inside a function body (which references Params).
        self.calc_cache.clear();

        // Classify each lowered body as a bit-slicer (idiom 7) so that
        // top-level call-site lowering can rewrite to BitField. Walk in
        // function-id order; if `--bit` calls `--rightShift`, the
        // RightShift classification needs to be in place first. CSS
        // forbids recursion at parse, so this runs in a single pass —
        // any function whose dependencies didn't classify just stays
        // unclassified and falls through to a regular Call (correct,
        // just unoptimised).
        self.slicer_kinds = vec![None; self.dag.function_roots.len()];
        for fn_id in 0..self.dag.function_roots.len() {
            let root = self.dag.function_roots[fn_id];
            if root == NodeId::MAX {
                continue;
            }
            let n_params = self.dag.function_param_counts[fn_id];
            self.slicer_kinds[fn_id] = self.classify_slicer_body(root, n_params);
        }

        // Classify each function as a 16-bit bitwise op (idiom 8). v1's
        // `classify_bitwise_decomposition` is a body-shape matcher over
        // the parsed Expr — it inspects locals + result tree, never
        // looks at function names. v2 reuses it directly: same matcher,
        // both backends, no risk of the two recognisers drifting apart.
        self.bitwise_kinds = vec![None; self.dag.function_roots.len()];
        for func in functions {
            if let Some(&fn_id) = self.dag.function_names.get(&func.name) {
                self.bitwise_kinds[fn_id as usize] =
                    crate::compile::classify_bitwise_decomposition(func);
            }
        }
    }

    /// Classify a function body root as one of the bit-slicer shapes
    /// (idiom 7), or `None`. The body must reference `Param(0)` and
    /// optionally `Param(1)`; any other Param is a non-match.
    ///
    /// Shape matching only — no name lookup. A `@function --xyz(--a,
    /// --b) { result: mod(var(--a), pow(2, var(--b))); }` matches as
    /// `LowerBytes` regardless of the helper name, satisfying the
    /// catalogue's cardinal-rule note.
    fn classify_slicer_body(&self, root: NodeId, n_params: u32) -> Option<SlicerKind> {
        if n_params != 2 {
            return None;
        }
        if self.body_matches_lower_bytes(root) {
            return Some(SlicerKind::LowerBytes);
        }
        if self.body_matches_right_shift(root) {
            return Some(SlicerKind::RightShift);
        }
        if self.body_matches_bit(root) {
            return Some(SlicerKind::Bit);
        }
        None
    }

    /// Match `mod(Param(0), pow(2, Param(1)))`.
    fn body_matches_lower_bytes(&self, root: NodeId) -> bool {
        let DagNode::Calc { op: CalcKind::Mod, args } = &self.dag.nodes[root as usize] else {
            return false;
        };
        if args.len() != 2 {
            return false;
        }
        is_param(&self.dag, args[0], 0) && self.is_pow_two_to_param1(args[1])
    }

    /// Match `round(down, Param(0) / pow(2, Param(1)), 1)`.
    fn body_matches_right_shift(&self, root: NodeId) -> bool {
        use crate::types::RoundStrategy;
        let DagNode::Calc { op: CalcKind::Round(RoundStrategy::Down), args } =
            &self.dag.nodes[root as usize]
        else {
            return false;
        };
        if args.len() != 2 {
            return false;
        }
        // Interval arg must be Lit(1) for a pure floor-div by pow2 to
        // mean "shift". Anything else is not a slicer.
        if !is_lit(&self.dag, args[1], 1.0) {
            return false;
        }
        let DagNode::Calc { op: CalcKind::Div, args: div_args } =
            &self.dag.nodes[args[0] as usize]
        else {
            return false;
        };
        if div_args.len() != 2 {
            return false;
        }
        is_param(&self.dag, div_args[0], 0) && self.is_pow_two_to_param1(div_args[1])
    }

    /// Match `mod(Call{rightShift, [Param(0), Param(1)]}, 2)` — the
    /// inlined form `mod(round(down, Param(0)/pow(2,Param(1)), 1), 2)`
    /// also matches.
    fn body_matches_bit(&self, root: NodeId) -> bool {
        let DagNode::Calc { op: CalcKind::Mod, args } = &self.dag.nodes[root as usize] else {
            return false;
        };
        if args.len() != 2 {
            return false;
        }
        if !is_lit(&self.dag, args[1], 2.0) {
            return false;
        }
        // Inner can be either:
        //  (a) a Call to an already-classified RightShift function
        //      with args [Param(0), Param(1)], or
        //  (b) the raw RightShift body shape over Param(0), Param(1).
        match &self.dag.nodes[args[0] as usize] {
            DagNode::Call { fn_id, args: call_args } => {
                call_args.len() == 2
                    && is_param(&self.dag, call_args[0], 0)
                    && is_param(&self.dag, call_args[1], 1)
                    && self
                        .slicer_kinds
                        .get(*fn_id as usize)
                        .copied()
                        .flatten()
                        == Some(SlicerKind::RightShift)
            }
            _ => self.body_matches_right_shift(args[0]),
        }
    }

    /// Match `pow(2, Param(1))`.
    fn is_pow_two_to_param1(&self, id: NodeId) -> bool {
        let DagNode::Calc { op: CalcKind::Pow, args } = &self.dag.nodes[id as usize] else {
            return false;
        };
        args.len() == 2 && is_lit(&self.dag, args[0], 2.0) && is_param(&self.dag, args[1], 1)
    }

    /// Lower an `Expr::StyleCondition` cascade. Tries idiom recognisers
    /// in shape-specificity order, falling back to a generic `If`:
    ///
    /// 1. Whole-cascade dispatch table (idiom 1) — every branch tests the
    ///    same property against an integer literal. Lowers to one
    ///    `Switch`.
    /// 2. Priority head + Switch tail (idiom 9 wrapping idiom 1) — the
    ///    leading branches use heterogeneous keys (the priority head),
    ///    the suffix matches idiom 1. Lowers to `If { head, fallback:
    ///    Switch { … } }`. Catalogue note: kiln wraps every register
    ///    dispatch and every memory port in a TF/IRQ priority head, so
    ///    without this peel the inner cascade never gets recognised.
    /// 3. Generic — lower every branch as-is into an `If` node.
    ///
    /// Bit-identical to a single `If` over all branches: priority
    /// short-circuits left-to-right, so peeling the head and putting
    /// the tail in the fallback evaluates each branch under exactly
    /// the same conditions it would in the unpeeled form.
    fn lower_style_condition(
        &mut self,
        branches: &[StyleBranch],
        fallback: &Expr,
    ) -> NodeId {
        if let Some(switch_id) = self.try_lower_dispatch(branches, fallback) {
            return switch_id;
        }
        if let Some(id) = self.try_lower_priority_head_switch(branches, fallback) {
            return id;
        }
        let lowered_branches: Vec<(StyleCondNode, NodeId)> = branches
            .iter()
            .map(|StyleBranch { condition, then }| {
                let cond = self.lower_style_test(condition);
                let then_id = self.lower_expr(then);
                (cond, then_id)
            })
            .collect();
        let fallback_id = self.lower_expr(fallback);
        self.dag.push(DagNode::If {
            branches: lowered_branches,
            fallback: fallback_id,
        })
    }

    /// Recognise a priority-head + Switch-tail cascade (idiom 9 wrapping
    /// idiom 1). Returns `Some(node_id)` when:
    ///
    /// - The trailing run of branches all test the same property against
    ///   integer literals, AND that run has length ≥ 4 (same threshold
    ///   as `try_lower_dispatch`).
    /// - At least one branch precedes the run (otherwise
    ///   `try_lower_dispatch` would already have matched the whole
    ///   cascade).
    ///
    /// Lowers the head branches as a normal `If` whose fallback is a
    /// `Switch` over the tail. Walking semantics are identical to the
    /// original cascade because `If` short-circuits left-to-right and
    /// only evaluates the fallback when no head branch matches.
    fn try_lower_priority_head_switch(
        &mut self,
        branches: &[StyleBranch],
        fallback: &Expr,
    ) -> Option<NodeId> {
        // Walk back from the end, gathering as long a same-key
        // integer-literal run as possible. The first heterogeneous
        // branch (or compound condition) bounds the tail's left edge.
        let mut tail_start = branches.len();
        let mut tail_key: Option<String> = None;
        for (i, b) in branches.iter().enumerate().rev() {
            let StyleTest::Single { property, value } = &b.condition else {
                break;
            };
            if !matches!(value, Expr::Literal(_)) {
                break;
            }
            match &tail_key {
                None => tail_key = Some(property.clone()),
                Some(k) if k == property => {}
                _ => break,
            }
            tail_start = i;
        }

        let tail_len = branches.len() - tail_start;
        // Need at least 4 in the tail (matches the dispatch threshold)
        // and at least one branch in the head (otherwise the whole
        // cascade would already have lowered as a Switch).
        if tail_len < 4 || tail_start == 0 {
            return None;
        }

        let head = &branches[..tail_start];
        let tail = &branches[tail_start..];

        // Build the inner Switch first so the head's fallback NodeId is
        // ready before we lower the head bodies.
        let switch_id = self
            .try_lower_dispatch(tail, fallback)
            .expect("tail passes the dispatch shape by construction");

        let lowered_head: Vec<(StyleCondNode, NodeId)> = head
            .iter()
            .map(|StyleBranch { condition, then }| {
                let cond = self.lower_style_test(condition);
                let then_id = self.lower_expr(then);
                (cond, then_id)
            })
            .collect();

        Some(self.dag.push(DagNode::If {
            branches: lowered_head,
            fallback: switch_id,
        }))
    }

    /// Try lowering a `StyleCondition` to a `Switch` (dispatch table).
    /// Returns `Some(node_id)` when every branch is
    /// `style(--key: <integer-literal>): <expr>` against the same key
    /// and there are at least 4 branches (the same threshold v1 uses
    /// in `pattern::dispatch_table::recognise_dispatch`). Returns
    /// `None` otherwise; caller falls back to lowering as `If`.
    ///
    /// Why 4: smaller chains aren't worth the HashMap overhead. v1
    /// landed on this empirically.
    fn try_lower_dispatch(
        &mut self,
        branches: &[StyleBranch],
        fallback: &Expr,
    ) -> Option<NodeId> {
        if branches.len() < 4 {
            return None;
        }
        // First branch's property is the key. All subsequent branches
        // must use the same property and test it against an integer
        // literal. Compound conditions (And/Or) disqualify the chain.
        let key_property = match &branches[0].condition {
            StyleTest::Single { property, .. } => property.clone(),
            _ => return None,
        };
        let mut entries: Vec<(i64, &Expr)> = Vec::with_capacity(branches.len());
        for b in branches {
            match &b.condition {
                StyleTest::Single { property, value } => {
                    if property != &key_property {
                        return None;
                    }
                    match value {
                        Expr::Literal(v) => entries.push((*v as i64, &b.then)),
                        _ => return None,
                    }
                }
                _ => return None,
            }
        }
        // Dispatch matches; lower the key and the branch bodies.
        let key_id = self.lower_var_in_scope(&key_property);
        let mut table: std::collections::HashMap<i64, NodeId> =
            std::collections::HashMap::with_capacity(entries.len());
        for (k, expr) in entries {
            // First write wins on duplicate keys, mirroring v1's
            // HashMap::insert from `recognise_dispatch`. Duplicate
            // keys in v1 silently keep the last; we keep the first
            // because it matches the natural CSS semantics ("first
            // matching branch wins"). v1's "last wins" is a quirk of
            // its HashMap insertion order, not a Chrome contract.
            table.entry(k).or_insert_with(|| self.lower_expr(expr));
        }
        let fallback_id = self.lower_expr(fallback);
        // If a subset of entries' bodies resolves to same-kind LoadVars
        // whose (key, slot) pairs lie on a line, the per-key portion of
        // the table is redundant for those entries — peel them out into
        // a LoadVarDynamic and keep the remaining entries (literals,
        // calls, off-line LoadVars) in a smaller Switch above it.
        let final_node = self.try_peel_dispatch_to_dynamic_load(table, key_id, fallback_id);
        Some(final_node)
    }

    /// Build a Switch node from a key, table, and fallback. Skips the
    /// peel-to-LoadVarDynamic recogniser — used internally by the peel
    /// itself to emit the residual outer Switch without re-entering
    /// the recogniser.
    fn push_switch(&mut self, key: NodeId, table: std::collections::HashMap<i64, NodeId>, fallback: NodeId) -> NodeId {
        self.dag.push(DagNode::Switch { key, table, fallback })
    }

    /// Peel a `LoadVarDynamic` out of a `Switch` table.
    ///
    /// Partitions the table's entries into:
    ///   - `loadvar_subset`: entries whose body is `LoadVar { slot,
    ///     kind }` with a shared `kind` and whose `(key, slot)` pairs
    ///     fit a single arithmetic-progression line.
    ///   - `residual`: every other entry (literals, calls, off-line
    ///     LoadVars, mismatched-kind LoadVars).
    ///
    /// When `loadvar_subset.len() >= 4` (the dispatch threshold), the
    /// loadvar subset is replaced by one `LoadVarDynamic` super-node
    /// computing `slot = a + b*key` and bounds-checking against the
    /// covered slot range; the residual entries stay in an outer
    /// `Switch` whose fallback is the LoadVarDynamic. Out-of-table
    /// keys still hit the original fallback (LoadVarDynamic falls
    /// through to it on bounds-check miss).
    ///
    /// When no peel applies, returns the original Switch as-is.
    fn try_peel_dispatch_to_dynamic_load(
        &mut self,
        table: std::collections::HashMap<i64, NodeId>,
        key_id: NodeId,
        fallback_id: NodeId,
    ) -> NodeId {
        // Group LoadVar entries by tick-position; off-line entries and
        // non-LoadVar entries go straight into the residual. We pick
        // the largest line-fitting subset across all groups.
        let mut by_kind_curr: Vec<(i64, SlotId)> = Vec::new();
        let mut by_kind_prev: Vec<(i64, SlotId)> = Vec::new();
        let mut residual_keys: Vec<i64> = Vec::new();
        for (&k, &body) in &table {
            match self.dag.nodes[body as usize] {
                DagNode::LoadVar { slot, kind: TickPosition::Current } => {
                    by_kind_curr.push((k, slot));
                }
                DagNode::LoadVar { slot, kind: TickPosition::Prev } => {
                    by_kind_prev.push((k, slot));
                }
                _ => {
                    residual_keys.push(k);
                }
            }
        }

        // For each kind, find the largest subset whose (key, slot)
        // pairs fit one arithmetic-progression line. Take the larger
        // of the two; if it's below threshold, no peel.
        let (best_kind, best_pairs, mut off_line_keys) = {
            let curr = largest_line_fit(&by_kind_curr);
            let prev = largest_line_fit(&by_kind_prev);
            let prefer_prev = prev.0.len() >= curr.0.len();
            if prefer_prev {
                let off_keys: Vec<i64> = by_kind_curr.iter().map(|&(k, _)| k).collect();
                (TickPosition::Prev, prev, off_keys)
            } else {
                let off_keys: Vec<i64> = by_kind_prev.iter().map(|&(k, _)| k).collect();
                (TickPosition::Current, curr, off_keys)
            }
        };
        let (line_pairs, line_params) = best_pairs;
        if line_pairs.len() < 4 {
            return self.push_switch(key_id, table, fallback_id);
        }
        let (a, b) = line_params.expect("line_pairs.len() >= 4 implies fitted line");

        // Build the LoadVarDynamic. valid_lo/hi cover the slot range
        // of the line-fitting subset only.
        let (mut lo, mut hi) = (i32::MAX, i32::MIN);
        for &(_, s) in &line_pairs {
            if s < lo { lo = s; }
            if s > hi { hi = s; }
        }
        let valid_hi = hi.saturating_add(1);
        let slot_expr = self.build_slot_expr(a, b, key_id);
        let dynamic_load = self.dag.push(DagNode::LoadVarDynamic {
            slot_expr,
            valid_lo: lo,
            valid_hi,
            kind: best_kind,
            fallback: fallback_id,
        });

        // Residual = literal/call/etc. entries + off-line LoadVar
        // entries from the same kind + every entry from the other
        // kind. Build a smaller outer Switch over them; its fallback
        // is the LoadVarDynamic.
        let line_keys: std::collections::HashSet<i64> =
            line_pairs.iter().map(|&(k, _)| k).collect();
        let mut residual_table: std::collections::HashMap<i64, NodeId> =
            std::collections::HashMap::new();
        for k in &residual_keys {
            residual_table.insert(*k, table[k]);
        }
        // off-line LoadVars from the chosen kind (those not in the
        // line-fitting subset) also go in the residual.
        let chosen_kind_pairs = match best_kind {
            TickPosition::Prev => &by_kind_prev,
            TickPosition::Current => &by_kind_curr,
        };
        for &(k, _) in chosen_kind_pairs {
            if !line_keys.contains(&k) {
                residual_table.insert(k, table[&k]);
            }
        }
        // Other-kind entries are unconditionally residual.
        for k in off_line_keys.drain(..) {
            residual_table.insert(k, table[&k]);
        }

        if residual_table.is_empty() {
            return dynamic_load;
        }
        self.push_switch(key_id, residual_table, dynamic_load)
    }

    /// Build the slot expression `a + b * key`, specialising the
    /// trivial cases so the walker doesn't pay Mul/Add cost when one
    /// isn't needed.
    fn build_slot_expr(&mut self, a: i32, b: i32, key_id: NodeId) -> NodeId {
        if b == 1 && a == 0 {
            return key_id;
        }
        if b == 1 {
            let lit_a = self.intern_lit(a as f64);
            return self.intern_calc(CalcKind::Add, vec![lit_a, key_id]);
        }
        if a == 0 {
            let lit_b = self.intern_lit(b as f64);
            return self.intern_calc(CalcKind::Mul, vec![key_id, lit_b]);
        }
        let lit_b = self.intern_lit(b as f64);
        let mul = self.intern_calc(CalcKind::Mul, vec![key_id, lit_b]);
        let lit_a = self.intern_lit(a as f64);
        self.intern_calc(CalcKind::Add, vec![lit_a, mul])
    }

    /// Lower `var(--name)` in the current lowering scope. Same logic as
    /// `Expr::Var` arm of `lower_expr` but takes a bare property name
    /// (with or without leading `--`). Used by dispatch lowering for
    /// the key property.
    fn lower_var_in_scope(&mut self, name: &str) -> NodeId {
        let (bare, kind) = strip_var_name(name);
        if matches!(kind, TickPosition::Current) {
            if let Some(id) = self.lookup_inline(bare) {
                return id;
            }
        }
        if committed_slot_of(bare).is_some() || self.assigned_names.contains(bare) {
            let slot = self.slot_for(bare);
            self.intern_load_var(slot, kind)
        } else {
            self.intern_lit(0.0)
        }
    }

    fn lower_style_test(&mut self, test: &StyleTest) -> StyleCondNode {
        match test {
            StyleTest::Single { property, value } => {
                let (bare, kind) = strip_var_name(property);
                // The LHS of a `style(--P: V)` test is whatever a
                // `var(--P)` would evaluate to in the same scope.
                // During function-body inlining, that may be the
                // substituted call-site arg (param/local frame),
                // otherwise it's a tick-global slot read. Reuse the
                // same lookup logic as Expr::Var lowering — we just
                // construct a synthetic Var node and lower it.
                //
                // (We can't take a shortcut here because `--P` could
                // be a function parameter whose substituted value is
                // anything from a literal to a complex sub-expression.)
                let lhs = if matches!(kind, TickPosition::Current) {
                    if let Some(id) = self.lookup_inline(bare) {
                        id
                    } else if committed_slot_of(bare).is_some()
                        || self.assigned_names.contains(bare)
                    {
                        let slot = self.slot_for(bare);
                        self.intern_load_var(slot, kind)
                    } else {
                        // Unresolved property in a style() test reads
                        // as 0 (the registered initial-value default
                        // for unregistered numeric properties).
                        self.intern_lit(0.0)
                    }
                } else {
                    let slot = self.slot_for(bare);
                    self.intern_load_var(slot, kind)
                };
                let value_id = self.lower_expr(value);
                StyleCondNode::Single { lhs, value: value_id }
            }
            StyleTest::And(parts) => {
                StyleCondNode::And(parts.iter().map(|p| self.lower_style_test(p)).collect())
            }
            StyleTest::Or(parts) => {
                StyleCondNode::Or(parts.iter().map(|p| self.lower_style_test(p)).collect())
            }
        }
    }

    fn lower_assignment(&mut self, a: &Assignment) -> Option<NodeId> {
        if is_buffer_copy(&a.property) {
            return None;
        }
        let (bare, _kind) = strip_var_name(&a.property);
        let slot = self.slot_for(bare);
        let value = self.lower_expr(&a.value);
        let id = self.dag.push(DagNode::WriteVar { slot, value });
        Some(id)
    }

    // Function bodies are NOT lowered in Phase 1.
    //
    // Why: function parameters (`--at`, `--n`, etc.) and locals are
    // call-site-bound, not tick-global. If we lowered a function body
    // by walking its `result` expression, every `var(--paramName)`
    // would allocate a tick-global transient slot, conflating param
    // values across call sites and across the body's surrounding
    // scope. Lowering function bodies needs a parameter-frame
    // mechanism, which is Phase 2 work.
    //
    // Phase 1 delegates function-call evaluation entirely to
    // v1's `eval_function_call` (which binds parameters into
    // self.properties at call time, then evaluates the body via the
    // interpreter). The DAG records the function name so the walker
    // can route to the right callee, but the body itself stays in
    // `Expr` form on `Evaluator::functions`.

    /// Lower a v1-shaped `BroadcastWrite` into a v2-native record:
    /// resolve every property name to a `SlotId`, lower `value_expr`
    /// (and any spillover hi expressions) to `NodeId`s. Reads inside
    /// the value expression are forced to `TickPosition::Prev` because
    /// the broadcast executor runs against committed state — by the
    /// time it fires, every preceding `WriteVar` has already flushed
    /// its per-tick cache entry to State.
    fn lower_broadcast(&mut self, bw: &BroadcastWrite) -> DagBroadcast {
        // slot_for resolves committed slots and allocates transients
        // on demand — `--memAddr0` etc. aren't registered @properties
        // and need transient slots that match the slots the rest of
        // the program uses.
        let bare_of = |n: &str| {
            n.strip_prefix("--").unwrap_or(n).to_string()
        };
        let gate_slot = bw.gate_property.as_ref().map(|n| self.slot_for(&bare_of(n)));
        let dest_slot = self.slot_for(&bare_of(&bw.dest_property));

        // Address map: cell-var name → SlotId.
        let address_map: std::collections::HashMap<i64, SlotId> = bw
            .address_map
            .iter()
            .map(|(addr, name)| (*addr, self.slot_for(&bare_of(name))))
            .collect();

        // Lower value_expr with Prev reads forced.
        let prev_force = std::mem::replace(&mut self.force_prev_reads, true);
        let value_node = self.lower_expr(&bw.value_expr);

        // Spillover map: each entry has a target var name + a hi-byte
        // expression. Lower both alongside `value_node` (still under
        // force_prev_reads).
        let spillover_map: std::collections::HashMap<i64, (SlotId, NodeId)> = bw
            .spillover_map
            .iter()
            .map(|(addr, (target_name, hi_expr))| {
                let slot = self.slot_for(&bare_of(target_name));
                let hi_node = self.lower_expr(hi_expr);
                (*addr, (slot, hi_node))
            })
            .collect();
        self.force_prev_reads = prev_force;

        let spillover_guard = bw
            .spillover_guard
            .as_ref()
            .map(|n| self.slot_for(&bare_of(n)));

        DagBroadcast {
            gate_slot,
            dest_slot,
            value_node,
            address_map,
            spillover_map,
            spillover_guard,
        }
    }

    /// Lower a v1-shaped `PackedSlotPort` into a v2-native record:
    /// pre-resolve every property name. Packed broadcasts have no
    /// value expression (the byte/word value comes from a slot read
    /// at execution time), so this is pure slot resolution.
    fn lower_packed_broadcast(&mut self, p: &PackedSlotPort) -> DagPackedBroadcast {
        let bare_of = |n: &str| n.strip_prefix("--").unwrap_or(n).to_string();
        let gate_slot = self.slot_for(&bare_of(&p.gate_property));
        let addr_slot = self.slot_for(&bare_of(&p.addr_property));
        let val_slot = self.slot_for(&bare_of(&p.val_property));
        let width_slot = self.slot_for(&bare_of(&p.width_property));
        let address_map: std::collections::HashMap<i64, SlotId> = p
            .address_map
            .iter()
            .map(|(addr, name)| (*addr, self.slot_for(&bare_of(name))))
            .collect();
        DagPackedBroadcast {
            gate_slot,
            addr_slot,
            val_slot,
            width_slot,
            address_map,
            pack: p.pack,
        }
    }
}

/// Lower a `ParsedProgram` to a `Dag`.
pub fn lower_parsed_program(program: &ParsedProgram) -> Dag {
    let mut ctx = LowerCtx::new();

    // Pre-populate the set of LHS-assigned names so Expr::Var lowering
    // can distinguish "computed but not declared" (resolved → use slot)
    // from "never declared" (unresolved → use var() fallback per CSS spec).
    // Strip prefixes so --__1foo and --foo share an entry.
    for a in &program.assignments {
        let (bare, _) = strip_var_name(&a.property);
        ctx.assigned_names.insert(bare.to_string());
    }
    // The parser fast-path absorbs property declarations directly;
    // those names are also "computed" — their assignments live in the
    // prebuilt broadcast lists, not in program.assignments.
    for name in &program.fast_path_absorbed {
        let (bare, _) = strip_var_name(name);
        ctx.assigned_names.insert(bare.to_string());
    }

    // Run broadcast recognisers on the residual assignments. Their
    // results merge with the parser fast-path's prebuilt ports to form
    // the complete broadcast set; absorbed property names are filtered
    // out of the WriteVar lowering loop below.
    //
    // The recognisers are pure-CSS-shape matchers over Expr trees (see
    // pattern/broadcast_write.rs:61, packed_broadcast_write.rs:103) —
    // calling them is using shape-matching code, not inheriting v1's
    // bytecode decisions. See design doc § What v1 code stays callable.
    let bw_result = crate::pattern::broadcast_write::recognise_broadcast(
        &program.assignments,
    );
    let packed_result = crate::pattern::packed_broadcast_write::recognise_packed_broadcast(
        &program.assignments,
    );

    // Merge: recogniser output first, then parser-fast-path prebuilt.
    // Order is stable for given input. Indices into these vecs are
    // referenced by IndirectStore::port_id.
    let mut broadcast_writes = bw_result.writes;
    broadcast_writes.extend(program.prebuilt_broadcast_writes.iter().cloned());
    let mut packed_broadcast_ports = packed_result.ports;
    packed_broadcast_ports.extend(program.prebuilt_packed_broadcast_ports.iter().cloned());

    // Build the absorbed-properties union. v2 filters WriteVar lowering
    // against this; v1 does the same at eval.rs:418.
    let mut absorbed: std::collections::HashSet<String> = bw_result.absorbed_properties;
    absorbed.extend(packed_result.absorbed_properties);
    absorbed.extend(program.fast_path_absorbed.iter().cloned());

    // Index function definitions by name. Stable fn_ids assigned in
    // declaration order so call sites can reference any function
    // regardless of where it's declared.
    for f in &program.functions {
        ctx.functions.insert(f.name.clone(), f);
        ctx.intern_function(&f.name);
    }

    // Lower every @function body once into the global node arena.
    // Bodies use `Param(i)` placeholders for parameter references;
    // call sites later emit `Call { fn_id, args }` instead of
    // duplicating the body. This is the change from inline-at-lowering
    // (combinatorial blowup on real cabinets) to shared sub-DAGs.
    ctx.lower_all_function_bodies(&program.functions);

    // Lower top-level assignments. Skip:
    //   - Buffer-copy assignments (`--__0/__1/__2` prefix) — the slot
    //     model exposes prior-tick values as LoadVar Prev directly.
    //   - Assignments absorbed into broadcast lists — they're
    //     evaluated via IndirectStore terminals below.
    for a in &program.assignments {
        if absorbed.contains(&a.property) {
            continue;
        }
        if let Some(terminal) = ctx.lower_assignment(a) {
            ctx.dag.terminals.push(terminal);
        }
    }

    // Emit IndirectStore terminals for every broadcast port (recognised
    // + prebuilt). port_id indexes into Dag::broadcast_writes /
    // Dag::packed_broadcast_ports.
    for i in 0..broadcast_writes.len() {
        let id = ctx.dag.push(DagNode::IndirectStore {
            port_id: i as u32,
            packed: false,
        });
        ctx.dag.terminals.push(id);
    }
    for i in 0..packed_broadcast_ports.len() {
        let id = ctx.dag.push(DagNode::IndirectStore {
            port_id: i as u32,
            packed: true,
        });
        ctx.dag.terminals.push(id);
    }

    // Lower v1-shaped broadcast records into v2-native records:
    // every property name pre-resolved to a slot, every value
    // expression lowered to a NodeId. The walker uses these natively;
    // the v1-shaped vecs above are kept around only as scaffolding for
    // any path that hasn't migrated yet.
    let dag_broadcasts: Vec<DagBroadcast> = broadcast_writes
        .iter()
        .map(|bw| ctx.lower_broadcast(bw))
        .collect();
    let dag_packed_broadcasts: Vec<DagPackedBroadcast> = packed_broadcast_ports
        .iter()
        .map(|p| ctx.lower_packed_broadcast(p))
        .collect();

    ctx.dag.broadcast_writes = broadcast_writes;
    ctx.dag.packed_broadcast_ports = packed_broadcast_ports;
    ctx.dag.dag_broadcasts = dag_broadcasts;
    ctx.dag.dag_packed_broadcasts = dag_packed_broadcasts;
    ctx.dag.transient_slot_count = ctx.transient_slots.len();

    sort_terminals(&mut ctx.dag);
    ctx.dag
}

/// Topologically sort `dag.terminals` so that for each `WriteVar(slot=S)`,
/// every `WriteVar` whose value the current node reads via
/// `LoadVar { kind: Current, slot=S }` comes before it.
///
/// CSS spec: cascade resolves with substitution semantics. Topo sort is
/// the standard implementation. `LoadVar { kind: Prev }` reads don't
/// create edges (they read prior-cascade committed state, not this-pass
/// values).
///
/// On cycle, fall back to declaration order — CSS spec says cyclic
/// `var()` references make the declaration invalid at computed-value
/// time, but the registered initial value still applies. v1 produces
/// the same fallback (`eval.rs:1167`).
fn sort_terminals(dag: &mut Dag) {
    let n = dag.terminals.len();
    if n <= 1 {
        return;
    }

    // Map from slot → terminal index (only WriteVar terminals have
    // a defined slot; IndirectStore goes to many slots and is treated
    // as a sink — it can't be depended on).
    let mut slot_to_term: HashMap<SlotId, usize> = HashMap::new();
    for (i, &term_id) in dag.terminals.iter().enumerate() {
        if let DagNode::WriteVar { slot, .. } = dag.nodes[term_id as usize] {
            slot_to_term.insert(slot, i);
        }
    }

    // For each terminal, collect indices of terminals whose slots it
    // reads via LoadVar Current. Skip self-edges (CSS-spec cycle is
    // non-fatal; v1 retains them too).
    let mut deps: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, &term_id) in dag.terminals.iter().enumerate() {
        let mut reads: Vec<usize> = Vec::new();
        let value_root = match &dag.nodes[term_id as usize] {
            DagNode::WriteVar { value, .. } => Some(*value),
            _ => None,
        };
        if let Some(root) = value_root {
            collect_current_reads(dag, root, &slot_to_term, &mut reads);
        }
        reads.sort_unstable();
        reads.dedup();
        reads.retain(|&dep| dep != i);
        deps[i] = reads;
    }

    // Kahn's algorithm with min-heap on original index for stable order.
    let mut in_degree = vec![0usize; n];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, dep_list) in deps.iter().enumerate() {
        for &dep in dep_list {
            in_degree[i] += 1;
            dependents[dep].push(i);
        }
    }
    let mut ready: std::collections::BinaryHeap<std::cmp::Reverse<usize>> =
        std::collections::BinaryHeap::new();
    for (i, &deg) in in_degree.iter().enumerate() {
        if deg == 0 {
            ready.push(std::cmp::Reverse(i));
        }
    }
    let mut order: Vec<usize> = Vec::with_capacity(n);
    while let Some(std::cmp::Reverse(idx)) = ready.pop() {
        order.push(idx);
        for &d in &dependents[idx] {
            in_degree[d] -= 1;
            if in_degree[d] == 0 {
                ready.push(std::cmp::Reverse(d));
            }
        }
    }
    if order.len() != n {
        // Cycle. Keep declaration order; CSS spec says cyclic refs are
        // invalid-at-computed-value, but the registered initial value
        // still applies. The walker will compute *something* here; the
        // primitive_conformance var_self_cycle fixture pins what.
        log::debug!("v2 lowering: dependency cycle in terminals, keeping declaration order");
        return;
    }
    let new_terminals: Vec<NodeId> = order.iter().map(|&i| dag.terminals[i]).collect();
    dag.terminals = new_terminals;
}

/// Walk a node's sub-expression tree, pushing onto `out` the index of
/// any terminal whose slot is read via `LoadVar { kind: Current }`.
fn collect_current_reads(
    dag: &Dag,
    node_id: NodeId,
    slot_to_term: &HashMap<SlotId, usize>,
    out: &mut Vec<usize>,
) {
    match &dag.nodes[node_id as usize] {
        DagNode::Lit(_) | DagNode::LitStr(_) => {}
        DagNode::LoadVar { slot, kind } => {
            if matches!(kind, TickPosition::Current) {
                if let Some(&idx) = slot_to_term.get(slot) {
                    out.push(idx);
                }
            }
        }
        DagNode::Calc { args, .. } => {
            for &a in args {
                collect_current_reads(dag, a, slot_to_term, out);
            }
        }
        DagNode::Call { args, .. } => {
            // Arg-side reads count as dependencies. The function
            // body's *own* reads (LoadVar Current of tick-globals)
            // are conservatively NOT traversed here: bodies are
            // shared across call sites and may reference slots the
            // current call's caller doesn't depend on. A future
            // refinement would traverse the body once per fn_id and
            // cache the slot set.
            for &a in args {
                collect_current_reads(dag, a, slot_to_term, out);
            }
        }
        DagNode::Param(_) => {
            // Param placeholders only appear inside function bodies,
            // which the top-level traversal doesn't enter (see Call
            // arm above). If we ever do, Param resolves to a caller-
            // context arg — which has already been traversed.
        }
        DagNode::If { branches, fallback } => {
            for (cond, then_id) in branches {
                collect_style_cond_reads(dag, cond, slot_to_term, out);
                collect_current_reads(dag, *then_id, slot_to_term, out);
            }
            collect_current_reads(dag, *fallback, slot_to_term, out);
        }
        DagNode::Switch { key, table, fallback } => {
            collect_current_reads(dag, *key, slot_to_term, out);
            for branch_id in table.values() {
                collect_current_reads(dag, *branch_id, slot_to_term, out);
            }
            collect_current_reads(dag, *fallback, slot_to_term, out);
        }
        DagNode::Concat(parts) => {
            for &p in parts {
                collect_current_reads(dag, p, slot_to_term, out);
            }
        }
        DagNode::BitField { src, .. } => {
            collect_current_reads(dag, *src, slot_to_term, out);
        }
        DagNode::BitwiseOp { args, .. } => {
            for &a in args {
                collect_current_reads(dag, a, slot_to_term, out);
            }
        }
        DagNode::LoadVarDynamic { slot_expr, kind, fallback, valid_lo, valid_hi } => {
            // The dynamic slot read can hit any slot in [valid_lo,
            // valid_hi). For TickPosition::Current reads, that means a
            // dependency on every Current-WriteVar terminal whose slot
            // sits in that range — without it the topo sort can run a
            // dependent assignment before its source has flushed.
            if matches!(kind, TickPosition::Current) {
                let lo = *valid_lo;
                let hi = *valid_hi;
                for (slot, &idx) in slot_to_term.iter() {
                    if *slot >= lo && *slot < hi {
                        out.push(idx);
                    }
                }
            }
            // The slot-expression is itself a normal sub-DAG; its
            // Current reads are real dependencies regardless of kind.
            collect_current_reads(dag, *slot_expr, slot_to_term, out);
            collect_current_reads(dag, *fallback, slot_to_term, out);
        }
        DagNode::WriteVar { .. } | DagNode::IndirectStore { .. } => {
            // Not reached on well-formed sub-expressions.
        }
    }
}

fn collect_style_cond_reads(
    dag: &Dag,
    cond: &StyleCondNode,
    slot_to_term: &HashMap<SlotId, usize>,
    out: &mut Vec<usize>,
) {
    match cond {
        StyleCondNode::Single { lhs, value } => {
            // The LHS is now a NodeId (an arbitrary expression — a
            // LoadVar at top level, or a substituted param node when
            // inlined). Recurse into it so any LoadVar Current reads
            // contribute to the topo edges, just like the value side.
            collect_current_reads(dag, *lhs, slot_to_term, out);
            collect_current_reads(dag, *value, slot_to_term, out);
        }
        StyleCondNode::And(parts) | StyleCondNode::Or(parts) => {
            for p in parts {
                collect_style_cond_reads(dag, p, slot_to_term, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_css;

    fn build(css: &str) -> Dag {
        let parsed = parse_css(css).expect("parse");
        // load_properties populates the global address map that
        // lower_parsed_program reads via slot_of.
        let mut state = crate::State::default();
        state.load_properties(&parsed.properties);
        lower_parsed_program(&parsed)
    }

    #[test]
    fn lowers_simple_assignment() {
        let css = r#"
            @property --AX { syntax: "<integer>"; inherits: true; initial-value: 0; }
            .cpu { --AX: 42; }
        "#;
        let dag = build(css);
        // One terminal: the WriteVar for --AX.
        assert_eq!(dag.terminals.len(), 1);
        match &dag.nodes[dag.terminals[0] as usize] {
            DagNode::WriteVar { slot, .. } => assert_eq!(*slot, -1, "AX is the first state var → slot -1"),
            other => panic!("expected WriteVar, got {other:?}"),
        }
    }

    #[test]
    fn lowers_self_reference_to_load_var_current() {
        // --AX: calc(var(--AX) + 1) — the inner var(--AX) lowers to
        // LoadVar Current, even though semantically it reads prior tick
        // (because --AX hasn't been written yet at read time). The
        // walker is responsible for that semantics; lowering is purely
        // structural.
        let css = r#"
            @property --AX { syntax: "<integer>"; inherits: true; initial-value: 0; }
            .cpu { --AX: calc(var(--AX) + 1); }
        "#;
        let dag = build(css);
        let mut found_current_load = false;
        for n in &dag.nodes {
            if let DagNode::LoadVar { slot: -1, kind: TickPosition::Current } = n {
                found_current_load = true;
            }
        }
        assert!(found_current_load, "should emit LoadVar Current for --AX self-ref");
    }

    #[test]
    fn lowers_prev_tick_read_to_load_var_prev() {
        let css = r#"
            @property --AX { syntax: "<integer>"; inherits: true; initial-value: 0; }
            .cpu { --AX: calc(var(--__1AX) + 1); }
        "#;
        let dag = build(css);
        let mut found_prev_load = false;
        for n in &dag.nodes {
            if let DagNode::LoadVar { slot: -1, kind: TickPosition::Prev } = n {
                found_prev_load = true;
            }
        }
        assert!(found_prev_load, "should emit LoadVar Prev for --__1AX");
    }

    #[test]
    fn buffer_copy_assignment_skipped() {
        let css = r#"
            @property --AX { syntax: "<integer>"; inherits: true; initial-value: 0; }
            .cpu {
                --AX: 7;
                --__1AX: 999;
            }
        "#;
        let dag = build(css);
        // Exactly one terminal — the buffer-copy assignment is skipped.
        assert_eq!(dag.terminals.len(), 1);
    }

    #[test]
    fn lowers_function_call_to_shared_call_node() {
        // `--double(21)` should lower to a Call referencing the shared
        // body root, with `21` bound as the call's arg.
        let css = r#"
            @property --AX { syntax: "<integer>"; inherits: true; initial-value: 0; }
            @function --double(--x <integer>) returns <integer> {
                result: calc(var(--x) * 2);
            }
            .cpu { --AX: --double(21); }
        "#;
        let dag = build(css);
        assert_eq!(dag.terminals.len(), 1);

        // Terminal's value is a Call referencing the shared body root.
        let terminal_id = dag.terminals[0];
        let value_id = match &dag.nodes[terminal_id as usize] {
            DagNode::WriteVar { value, .. } => *value,
            other => panic!("expected WriteVar, got {other:?}"),
        };
        let (fn_id, args) = match &dag.nodes[value_id as usize] {
            DagNode::Call { fn_id, args } => (*fn_id, args.clone()),
            other => panic!("expected Call, got {other:?}"),
        };
        assert_eq!(args.len(), 1);
        // Arg is Lit(21) at the call site.
        match &dag.nodes[args[0] as usize] {
            DagNode::Lit(v) => assert_eq!(*v, 21.0),
            other => panic!("expected Lit(21), got {other:?}"),
        }

        // Function body root (lowered once) is a Mul node whose first
        // arg is Param(0) and second is Lit(2).
        assert_eq!(dag.function_param_counts[fn_id as usize], 1);
        let body_root = dag.function_roots[fn_id as usize];
        match &dag.nodes[body_root as usize] {
            DagNode::Calc { op: CalcKind::Mul, args: body_args } => {
                assert_eq!(body_args.len(), 2);
                match &dag.nodes[body_args[0] as usize] {
                    DagNode::Param(0) => {}
                    other => panic!("expected Param(0), got {other:?}"),
                }
                match &dag.nodes[body_args[1] as usize] {
                    DagNode::Lit(v) => assert_eq!(*v, 2.0),
                    other => panic!("expected Lit(2), got {other:?}"),
                }
            }
            other => panic!("expected Calc(Mul) body, got {other:?}"),
        }
    }

    #[test]
    fn lowers_dispatch_chain_to_switch() {
        // Six-branch dispatch on `--opcode` should lower to a Switch
        // node, not a chain of If branches.
        let css = r#"
            @property --opcode { syntax: "<integer>"; inherits: true; initial-value: 0; }
            @property --AX { syntax: "<integer>"; inherits: true; initial-value: 0; }
            :root {
                --AX: if(
                    style(--opcode: 1): 11;
                    style(--opcode: 2): 22;
                    style(--opcode: 3): 33;
                    style(--opcode: 4): 44;
                    style(--opcode: 5): 55;
                    style(--opcode: 6): 66;
                    else: 0
                );
            }
        "#;
        let dag = build(css);
        let mut found_switch = false;
        for n in &dag.nodes {
            if let DagNode::Switch { table, .. } = n {
                assert_eq!(table.len(), 6);
                found_switch = true;
            }
        }
        assert!(found_switch, "expected dispatch chain to lower to Switch");
    }

    #[test]
    fn small_chain_stays_as_if() {
        // Three-branch chain is below the dispatch threshold (4); it
        // should stay as an If, not become a Switch.
        let css = r#"
            @property --opcode { syntax: "<integer>"; inherits: true; initial-value: 0; }
            @property --AX { syntax: "<integer>"; inherits: true; initial-value: 0; }
            :root {
                --AX: if(
                    style(--opcode: 1): 11;
                    style(--opcode: 2): 22;
                    style(--opcode: 3): 33;
                    else: 0
                );
            }
        "#;
        let dag = build(css);
        for n in &dag.nodes {
            if matches!(n, DagNode::Switch { .. }) {
                panic!("3-branch chain should not lower to Switch");
            }
        }
    }

    #[test]
    fn inlined_function_body_can_become_switch() {
        // A function whose body is a dispatch chain on a parameter:
        // when inlined, the param substitution becomes the Switch key.
        let css = r#"
            @property --opcode { syntax: "<integer>"; inherits: true; initial-value: 3; }
            @property --AX { syntax: "<integer>"; inherits: true; initial-value: 0; }
            @function --pick(--k <integer>) returns <integer> {
                result: if(
                    style(--k: 1): 100;
                    style(--k: 2): 200;
                    style(--k: 3): 300;
                    style(--k: 4): 400;
                    else: -1
                );
            }
            :root {
                --AX: --pick(var(--opcode));
            }
        "#;
        let dag = build(css);
        let mut found = false;
        for n in &dag.nodes {
            if let DagNode::Switch { table, .. } = n {
                assert_eq!(table.len(), 4);
                found = true;
            }
        }
        assert!(found, "inlined dispatch on substituted param should become Switch");
    }

    #[test]
    fn broadcast_value_lowers_with_prev_reads() {
        // Build a broadcast write: 10+ branches dispatching on `--dest`
        // to write `var(--src)` into different cells. Verify a
        // DagBroadcast is produced and its value_node walks back to a
        // LoadVar with TickPosition::Prev (broadcasts read committed
        // state, not the per-tick scalar cache).
        let mut css = String::from(
            r#"
            @property --dest { syntax: "<integer>"; inherits: true; initial-value: 0; }
            @property --src { syntax: "<integer>"; inherits: true; initial-value: 0; }
            "#,
        );
        for i in 0..12 {
            css.push_str(&format!(
                "@property --cell{i} {{ syntax: \"<integer>\"; inherits: true; initial-value: 0; }}\n"
            ));
        }
        css.push_str(":root {\n");
        for i in 0..12 {
            css.push_str(&format!(
                "  --cell{i}: if(style(--dest: {i}): var(--src); else: var(--__1cell{i}));\n"
            ));
        }
        css.push_str("}\n");

        let dag = build(&css);

        // Recogniser fires (12 ≥ 10 entries) → one v2-native broadcast.
        assert_eq!(
            dag.dag_broadcasts.len(),
            1,
            "expected one v2-native broadcast"
        );
        let bw = &dag.dag_broadcasts[0];
        assert_eq!(bw.address_map.len(), 12);

        // Walk the value_node — should be a single LoadVar Prev for
        // --src (broadcast value reads bypass the per-tick cache).
        match &dag.nodes[bw.value_node as usize] {
            DagNode::LoadVar { kind: TickPosition::Prev, .. } => {}
            other => panic!("expected LoadVar Prev, got {other:?}"),
        }

        // dest_slot should resolve to --dest's committed state-var slot.
        let expected = crate::eval::property_to_address("--dest")
            .expect("--dest is a declared @property");
        assert_eq!(bw.dest_slot, expected);
    }

    #[test]
    fn inlining_preserves_locals() {
        // A function with a local that depends on a param. The local's
        // value-NodeId should be referenced by the result's body.
        let css = r#"
            @property --AX { syntax: "<integer>"; inherits: true; initial-value: 0; }
            @function --plus3(--x <integer>) returns <integer> {
                --tmp: calc(var(--x) + 1);
                result: calc(var(--tmp) + 2);
            }
            .cpu { --AX: --plus3(10); }
        "#;
        let dag = build(css);
        assert_eq!(dag.terminals.len(), 1);
    }

    #[test]
    fn unknown_callee_lowers_to_lit_zero() {
        // Calling an undeclared `@function` is unresolved per CSS spec;
        // the surrounding property gets its registered initial value
        // (0 for `<integer>`). Lowering must emit a `Lit(0.0)` directly
        // — no Call/FuncCall node, no v1-bridge fallback.
        let css = r#"
            @property --AX { syntax: "<integer>"; inherits: true; initial-value: 0; }
            .cpu { --AX: --doesNotExist(42); }
        "#;
        let dag = build(css);
        assert_eq!(dag.terminals.len(), 1);
        let value_id = match &dag.nodes[dag.terminals[0] as usize] {
            DagNode::WriteVar { value, .. } => *value,
            other => panic!("expected WriteVar, got {other:?}"),
        };
        match &dag.nodes[value_id as usize] {
            DagNode::Lit(v) => assert_eq!(*v, 0.0),
            other => panic!("expected Lit(0.0), got {other:?}"),
        }
        // Defensively: no Call node anywhere either.
        for n in &dag.nodes {
            if matches!(n, DagNode::Call { .. }) {
                panic!("unexpected Call for undeclared function");
            }
        }
    }

    #[test]
    fn priority_head_peels_from_dispatch_tail() {
        // A 2-branch priority head on different keys (--tf, --irq)
        // followed by a 6-branch dispatch on --opcode. Whole-cascade
        // dispatch fails the "same key everywhere" check; the
        // priority-head peel should produce an If whose fallback is
        // a Switch over the tail.
        let css = r#"
            @property --tf { syntax: "<integer>"; inherits: true; initial-value: 0; }
            @property --irq { syntax: "<integer>"; inherits: true; initial-value: 0; }
            @property --opcode { syntax: "<integer>"; inherits: true; initial-value: 0; }
            @property --AX { syntax: "<integer>"; inherits: true; initial-value: 0; }
            :root {
                --AX: if(
                    style(--tf: 1): 1000;
                    style(--irq: 1): 2000;
                    style(--opcode: 1): 11;
                    style(--opcode: 2): 22;
                    style(--opcode: 3): 33;
                    style(--opcode: 4): 44;
                    style(--opcode: 5): 55;
                    style(--opcode: 6): 66;
                    else: 0
                );
            }
        "#;
        let dag = build(css);

        // Find the WriteVar terminal and confirm its value is an If
        // with exactly 2 head branches and a Switch in the fallback.
        let value_id = match &dag.nodes[dag.terminals[0] as usize] {
            DagNode::WriteVar { value, .. } => *value,
            other => panic!("expected WriteVar, got {other:?}"),
        };
        let (head_len, fallback) = match &dag.nodes[value_id as usize] {
            DagNode::If { branches, fallback } => (branches.len(), *fallback),
            other => panic!("expected If at root, got {other:?}"),
        };
        assert_eq!(head_len, 2, "priority head should have 2 branches");
        let table_len = match &dag.nodes[fallback as usize] {
            DagNode::Switch { table, .. } => table.len(),
            other => panic!("expected Switch in fallback, got {other:?}"),
        };
        assert_eq!(table_len, 6, "tail Switch should hold the 6 opcode entries");
    }

    #[test]
    fn priority_head_short_tail_stays_as_if() {
        // Tail of only 3 same-key branches is below the dispatch
        // threshold, so the peel must NOT fire — the whole cascade
        // stays as one If.
        let css = r#"
            @property --tf { syntax: "<integer>"; inherits: true; initial-value: 0; }
            @property --opcode { syntax: "<integer>"; inherits: true; initial-value: 0; }
            @property --AX { syntax: "<integer>"; inherits: true; initial-value: 0; }
            :root {
                --AX: if(
                    style(--tf: 1): 1000;
                    style(--opcode: 1): 11;
                    style(--opcode: 2): 22;
                    style(--opcode: 3): 33;
                    else: 0
                );
            }
        "#;
        let dag = build(css);
        for n in &dag.nodes {
            if matches!(n, DagNode::Switch { .. }) {
                panic!("3-branch tail should not promote to Switch");
            }
        }
    }

    #[test]
    fn priority_head_pure_dispatch_still_uses_switch_directly() {
        // No priority head: the dispatch recogniser fires on the whole
        // cascade and produces one Switch with no wrapping If.
        let css = r#"
            @property --opcode { syntax: "<integer>"; inherits: true; initial-value: 0; }
            @property --AX { syntax: "<integer>"; inherits: true; initial-value: 0; }
            :root {
                --AX: if(
                    style(--opcode: 1): 11;
                    style(--opcode: 2): 22;
                    style(--opcode: 3): 33;
                    style(--opcode: 4): 44;
                    style(--opcode: 5): 55;
                    style(--opcode: 6): 66;
                    else: 0
                );
            }
        "#;
        let dag = build(css);
        let value_id = match &dag.nodes[dag.terminals[0] as usize] {
            DagNode::WriteVar { value, .. } => *value,
            other => panic!("expected WriteVar, got {other:?}"),
        };
        // Root of the value is a Switch, NOT an If wrapping a Switch.
        match &dag.nodes[value_id as usize] {
            DagNode::Switch { .. } => {}
            other => panic!("expected Switch at root, got {other:?}"),
        }
    }

    #[test]
    fn priority_head_compound_condition_breaks_run() {
        // A priority head followed by a Switch-shaped tail interrupted
        // by a compound-AND in the middle: the run scan walks back from
        // the end and stops at the compound branch, so the tail starts
        // AFTER the compound (only 4 same-key branches), which still
        // qualifies for the peel. The 3 branches before the compound
        // (1 priority + compound + 1 same-key) become the head.
        let css = r#"
            @property --tf { syntax: "<integer>"; inherits: true; initial-value: 0; }
            @property --gate { syntax: "<integer>"; inherits: true; initial-value: 0; }
            @property --opcode { syntax: "<integer>"; inherits: true; initial-value: 0; }
            @property --AX { syntax: "<integer>"; inherits: true; initial-value: 0; }
            :root {
                --AX: if(
                    style(--tf: 1): 1000;
                    style(--opcode: 1): 11;
                    style(--gate: 1) and style(--opcode: 99): 9900;
                    style(--opcode: 2): 22;
                    style(--opcode: 3): 33;
                    style(--opcode: 4): 44;
                    style(--opcode: 5): 55;
                    else: 0
                );
            }
        "#;
        let dag = build(css);
        let value_id = match &dag.nodes[dag.terminals[0] as usize] {
            DagNode::WriteVar { value, .. } => *value,
            other => panic!("expected WriteVar, got {other:?}"),
        };
        let (head_len, fallback) = match &dag.nodes[value_id as usize] {
            DagNode::If { branches, fallback } => (branches.len(), *fallback),
            other => panic!("expected If at root, got {other:?}"),
        };
        assert_eq!(head_len, 3, "head holds 1 priority + compound + 1 same-key");
        let table_len = match &dag.nodes[fallback as usize] {
            DagNode::Switch { table, .. } => table.len(),
            other => panic!("expected Switch in fallback, got {other:?}"),
        };
        assert_eq!(table_len, 4, "tail Switch holds opcode 2,3,4,5");
    }

    fn find_bitfield(dag: &Dag) -> (NodeId, u8, Option<u8>) {
        for (id, n) in dag.nodes.iter().enumerate() {
            if let DagNode::BitField { src, shift, width } = n {
                return (*src, *shift, *width);
            }
            let _ = id;
        }
        panic!("no BitField node in DAG");
    }

    #[test]
    fn lower_bytes_call_lowers_to_bitfield_with_mask() {
        let css = r#"
            @property --AX { syntax: "<integer>"; inherits: true; initial-value: 0; }
            @function --lowerBytes(--a <integer>, --b <integer>) returns <integer> {
                result: mod(var(--a), pow(2, var(--b)));
            }
            :root { --AX: --lowerBytes(4660, 8); }
        "#;
        let dag = build(css);
        let (_src, shift, width) = find_bitfield(&dag);
        assert_eq!(shift, 0);
        assert_eq!(width, Some(8));
    }

    #[test]
    fn right_shift_call_lowers_to_bitfield_no_mask() {
        let css = r#"
            @property --AX { syntax: "<integer>"; inherits: true; initial-value: 0; }
            @function --rightShift(--a <integer>, --b <integer>) returns <integer> {
                result: round(down, var(--a) / pow(2, var(--b)));
            }
            :root { --AX: --rightShift(4660, 4); }
        "#;
        let dag = build(css);
        let (_src, shift, width) = find_bitfield(&dag);
        assert_eq!(shift, 4);
        assert_eq!(width, None);
    }

    #[test]
    fn bit_call_lowers_to_bitfield_width_one() {
        // --bit references --rightShift; the body classifier must
        // recognise --bit as a Bit slicer because its body is
        // `mod(Call{rightShift, ...}, 2)` and rightShift was classified
        // earlier in the same pass.
        let css = r#"
            @property --AX { syntax: "<integer>"; inherits: true; initial-value: 0; }
            @function --rightShift(--a <integer>, --b <integer>) returns <integer> {
                result: round(down, var(--a) / pow(2, var(--b)));
            }
            @function --bit(--val <integer>, --idx <integer>) returns <integer> {
                result: mod(--rightShift(var(--val), var(--idx)), 2);
            }
            :root { --AX: --bit(64, 6); }
        "#;
        let dag = build(css);
        // There may be multiple BitFields (--bit's body inlining +
        // call-site rewrite); find the call-site one by checking it's
        // referenced from the WriteVar terminal.
        let value_id = match &dag.nodes[dag.terminals[0] as usize] {
            DagNode::WriteVar { value, .. } => *value,
            other => panic!("expected WriteVar, got {other:?}"),
        };
        let (_src, shift, width) = match &dag.nodes[value_id as usize] {
            DagNode::BitField { src, shift, width } => (*src, *shift, *width),
            other => panic!("expected BitField at root, got {other:?}"),
        };
        assert_eq!(shift, 6);
        assert_eq!(width, Some(1));
    }

    #[test]
    fn slicer_with_dynamic_shift_falls_back_to_call() {
        // Second arg is a property reference, not a literal — must NOT
        // collapse to BitField (the shift isn't compile-time known).
        let css = r#"
            @property --AX { syntax: "<integer>"; inherits: true; initial-value: 0; }
            @property --shift { syntax: "<integer>"; inherits: true; initial-value: 4; }
            @function --rightShift(--a <integer>, --b <integer>) returns <integer> {
                result: round(down, var(--a) / pow(2, var(--b)));
            }
            :root { --AX: --rightShift(4660, var(--shift)); }
        "#;
        let dag = build(css);
        // The terminal's value should be a Call, not a BitField.
        let value_id = match &dag.nodes[dag.terminals[0] as usize] {
            DagNode::WriteVar { value, .. } => *value,
            other => panic!("expected WriteVar, got {other:?}"),
        };
        match &dag.nodes[value_id as usize] {
            DagNode::Call { .. } => {}
            other => panic!("expected Call (dynamic shift), got {other:?}"),
        }
    }

    #[test]
    fn slicer_recogniser_is_name_agnostic() {
        // Same body shape as --lowerBytes but with an arbitrary helper
        // name. The recogniser matches on body shape, not name, so the
        // call site must still collapse to BitField. Cardinal-rule
        // operational test from the catalogue.
        let css = r#"
            @property --AX { syntax: "<integer>"; inherits: true; initial-value: 0; }
            @function --xyzMaskN(--p <integer>, --q <integer>) returns <integer> {
                result: mod(var(--p), pow(2, var(--q)));
            }
            :root { --AX: --xyzMaskN(4660, 4); }
        "#;
        let dag = build(css);
        let (_src, shift, width) = find_bitfield(&dag);
        assert_eq!(shift, 0);
        assert_eq!(width, Some(4));
    }

    #[test]
    fn dispatch_with_arithmetic_progression_slots_lowers_to_dynamic_load() {
        // A 4-branch dispatch where each branch reads `var(--__1mK)`
        // matching the branch's key. Slot for `--mK` is K (the address-
        // map fallback), so the (key, slot) line is slot = key. The
        // recogniser should collapse the Switch to a LoadVarDynamic.
        let css = r#"
            @property --idx { syntax: "<integer>"; inherits: true; initial-value: 0; }
            @property --AX  { syntax: "<integer>"; inherits: true; initial-value: 0; }
            :root {
                --AX: if(
                    style(--idx: 0): var(--__1m0);
                    style(--idx: 1): var(--__1m1);
                    style(--idx: 2): var(--__1m2);
                    style(--idx: 3): var(--__1m3);
                    else: 0
                );
            }
        "#;
        let dag = build(css);
        let value_id = match &dag.nodes[dag.terminals[0] as usize] {
            DagNode::WriteVar { value, .. } => *value,
            other => panic!("expected WriteVar, got {other:?}"),
        };
        let (slot_expr, lo, hi, kind) = match &dag.nodes[value_id as usize] {
            DagNode::LoadVarDynamic { slot_expr, valid_lo, valid_hi, kind, .. } => {
                (*slot_expr, *valid_lo, *valid_hi, *kind)
            }
            other => panic!("expected LoadVarDynamic, got {other:?}"),
        };
        assert_eq!(lo, 0);
        assert_eq!(hi, 4, "exclusive upper bound = max_slot + 1");
        assert_eq!(kind, TickPosition::Prev);
        // Line is slot = key, so slot_expr should be exactly the key
        // node (the trivial b=1, a=0 specialisation).
        match &dag.nodes[slot_expr as usize] {
            DagNode::LoadVar { kind: TickPosition::Current, .. } => {}
            other => panic!("slot_expr should be the key LoadVar, got {other:?}"),
        }
        // No leftover Switch in the DAG for this assignment.
        for n in &dag.nodes {
            if matches!(n, DagNode::Switch { .. }) {
                panic!("Switch should have been replaced");
            }
        }
    }

    #[test]
    fn dispatch_with_irregular_slots_stays_as_switch() {
        // Same shape but the bodies don't fit a line: keys are 0..4
        // but they read --__1m0, --__1m1, --__1m17, --__1m3 — the
        // third entry breaks the slope. Recogniser must NOT fire.
        let css = r#"
            @property --idx { syntax: "<integer>"; inherits: true; initial-value: 0; }
            @property --AX  { syntax: "<integer>"; inherits: true; initial-value: 0; }
            :root {
                --AX: if(
                    style(--idx: 0): var(--__1m0);
                    style(--idx: 1): var(--__1m1);
                    style(--idx: 2): var(--__1m17);
                    style(--idx: 3): var(--__1m3);
                    else: 0
                );
            }
        "#;
        let dag = build(css);
        let value_id = match &dag.nodes[dag.terminals[0] as usize] {
            DagNode::WriteVar { value, .. } => *value,
            other => panic!("expected WriteVar, got {other:?}"),
        };
        match &dag.nodes[value_id as usize] {
            DagNode::Switch { .. } => {}
            other => panic!("expected Switch (off-line), got {other:?}"),
        }
    }

    #[test]
    fn dispatch_with_mixed_kind_bodies_stays_as_switch() {
        // All bodies are LoadVars on slots that fit a line, but the
        // tick-position differs across entries (some Current, some
        // Prev). The recogniser requires a shared kind across the
        // table — must stay as a Switch.
        let css = r#"
            @property --idx { syntax: "<integer>"; inherits: true; initial-value: 0; }
            @property --AX  { syntax: "<integer>"; inherits: true; initial-value: 0; }
            :root {
                --AX: if(
                    style(--idx: 0): var(--__1m0);
                    style(--idx: 1): var(--__1m1);
                    style(--idx: 2): var(--__1m2);
                    style(--idx: 3): var(--m3);
                    else: 0
                );
            }
        "#;
        let dag = build(css);
        let value_id = match &dag.nodes[dag.terminals[0] as usize] {
            DagNode::WriteVar { value, .. } => *value,
            other => panic!("expected WriteVar, got {other:?}"),
        };
        match &dag.nodes[value_id as usize] {
            DagNode::Switch { .. } => {}
            other => panic!("expected Switch (mixed kind), got {other:?}"),
        }
    }

    #[test]
    fn dispatch_with_non_loadvar_body_stays_as_switch() {
        // One body isn't a static LoadVar — it's a calc. Recogniser
        // requires every entry to be LoadVar; falls through.
        let css = r#"
            @property --idx { syntax: "<integer>"; inherits: true; initial-value: 0; }
            @property --AX  { syntax: "<integer>"; inherits: true; initial-value: 0; }
            :root {
                --AX: if(
                    style(--idx: 0): var(--__1m0);
                    style(--idx: 1): var(--__1m1);
                    style(--idx: 2): calc(var(--__1m2) + 1);
                    style(--idx: 3): var(--__1m3);
                    else: 0
                );
            }
        "#;
        let dag = build(css);
        let value_id = match &dag.nodes[dag.terminals[0] as usize] {
            DagNode::WriteVar { value, .. } => *value,
            other => panic!("expected WriteVar, got {other:?}"),
        };
        match &dag.nodes[value_id as usize] {
            DagNode::Switch { .. } => {}
            other => panic!("expected Switch (non-loadvar body), got {other:?}"),
        }
    }

    #[test]
    fn dispatch_with_offset_arithmetic_slot_line_lowers() {
        // Keys 0..3, slots 100..103. Line is slot = 100 + 1*key.
        // The slot 100 here is just a regular memory cell index;
        // dispatch should still fire and produce slot_expr = a + key.
        let css = r#"
            @property --idx { syntax: "<integer>"; inherits: true; initial-value: 0; }
            @property --AX  { syntax: "<integer>"; inherits: true; initial-value: 0; }
            :root {
                --AX: if(
                    style(--idx: 0): var(--__1m100);
                    style(--idx: 1): var(--__1m101);
                    style(--idx: 2): var(--__1m102);
                    style(--idx: 3): var(--__1m103);
                    else: 0
                );
            }
        "#;
        let dag = build(css);
        let value_id = match &dag.nodes[dag.terminals[0] as usize] {
            DagNode::WriteVar { value, .. } => *value,
            other => panic!("expected WriteVar, got {other:?}"),
        };
        let (slot_expr, lo, hi) = match &dag.nodes[value_id as usize] {
            DagNode::LoadVarDynamic { slot_expr, valid_lo, valid_hi, .. } => {
                (*slot_expr, *valid_lo, *valid_hi)
            }
            other => panic!("expected LoadVarDynamic, got {other:?}"),
        };
        assert_eq!(lo, 100);
        assert_eq!(hi, 104);
        // slot_expr should be Add(Lit(100), key).
        match &dag.nodes[slot_expr as usize] {
            DagNode::Calc { op: CalcKind::Add, args } => {
                assert_eq!(args.len(), 2);
                match &dag.nodes[args[0] as usize] {
                    DagNode::Lit(v) => assert_eq!(*v, 100.0),
                    other => panic!("expected Lit(100), got {other:?}"),
                }
            }
            other => panic!("expected Add at slot_expr, got {other:?}"),
        }
    }

    #[test]
    fn bitwise_decomp_call_lowers_to_bitwise_op() {
        // A 2-input bit-decomposition body matching the AND shape:
        // 32 locals (16 per param), result = sum of `ai * bi * 2^i`.
        // The recogniser is shape-only (uses v1's classifier) — function
        // name is arbitrary.
        let css = r#"
            @property --x { syntax: "<integer>"; inherits: true; initial-value: 12; }
            @property --y { syntax: "<integer>"; inherits: true; initial-value: 10; }
            @property --result { syntax: "<integer>"; inherits: true; initial-value: 0; }
            @function --combine(--a <integer>, --b <integer>) returns <integer> {
              --a1: mod(var(--a), 2);
              --a2: mod(round(down, var(--a) / 2), 2);
              --a3: mod(round(down, var(--a) / 4), 2);
              --a4: mod(round(down, var(--a) / 8), 2);
              --a5: mod(round(down, var(--a) / 16), 2);
              --a6: mod(round(down, var(--a) / 32), 2);
              --a7: mod(round(down, var(--a) / 64), 2);
              --a8: mod(round(down, var(--a) / 128), 2);
              --a9: mod(round(down, var(--a) / 256), 2);
              --a10: mod(round(down, var(--a) / 512), 2);
              --a11: mod(round(down, var(--a) / 1024), 2);
              --a12: mod(round(down, var(--a) / 2048), 2);
              --a13: mod(round(down, var(--a) / 4096), 2);
              --a14: mod(round(down, var(--a) / 8192), 2);
              --a15: mod(round(down, var(--a) / 16384), 2);
              --a16: mod(round(down, var(--a) / 32768), 2);
              --b1: mod(var(--b), 2);
              --b2: mod(round(down, var(--b) / 2), 2);
              --b3: mod(round(down, var(--b) / 4), 2);
              --b4: mod(round(down, var(--b) / 8), 2);
              --b5: mod(round(down, var(--b) / 16), 2);
              --b6: mod(round(down, var(--b) / 32), 2);
              --b7: mod(round(down, var(--b) / 64), 2);
              --b8: mod(round(down, var(--b) / 128), 2);
              --b9: mod(round(down, var(--b) / 256), 2);
              --b10: mod(round(down, var(--b) / 512), 2);
              --b11: mod(round(down, var(--b) / 1024), 2);
              --b12: mod(round(down, var(--b) / 2048), 2);
              --b13: mod(round(down, var(--b) / 4096), 2);
              --b14: mod(round(down, var(--b) / 8192), 2);
              --b15: mod(round(down, var(--b) / 16384), 2);
              --b16: mod(round(down, var(--b) / 32768), 2);
              result: calc(
                calc(var(--a1) * var(--b1)) +
                calc(var(--a2) * var(--b2)) * 2 +
                calc(var(--a3) * var(--b3)) * 4 +
                calc(var(--a4) * var(--b4)) * 8 +
                calc(var(--a5) * var(--b5)) * 16 +
                calc(var(--a6) * var(--b6)) * 32 +
                calc(var(--a7) * var(--b7)) * 64 +
                calc(var(--a8) * var(--b8)) * 128 +
                calc(var(--a9) * var(--b9)) * 256 +
                calc(var(--a10) * var(--b10)) * 512 +
                calc(var(--a11) * var(--b11)) * 1024 +
                calc(var(--a12) * var(--b12)) * 2048 +
                calc(var(--a13) * var(--b13)) * 4096 +
                calc(var(--a14) * var(--b14)) * 8192 +
                calc(var(--a15) * var(--b15)) * 16384 +
                calc(var(--a16) * var(--b16)) * 32768
              );
            }
            :root { --result: --combine(var(--x), var(--y)); }
        "#;
        let dag = build(css);
        let value_id = match &dag.nodes[dag.terminals[0] as usize] {
            DagNode::WriteVar { value, .. } => *value,
            other => panic!("expected WriteVar, got {other:?}"),
        };
        match &dag.nodes[value_id as usize] {
            DagNode::BitwiseOp { kind, args } => {
                use crate::dag::types::BitwiseKind;
                assert_eq!(*kind, BitwiseKind::And);
                assert_eq!(args.len(), 2);
            }
            other => panic!("expected BitwiseOp, got {other:?}"),
        }
    }

    #[test]
    fn slicer_lowerbytes_zero_width_falls_through() {
        // --lowerBytes(X, 0) = mod(X, 1) = 0 for any X. The recogniser
        // refuses to emit a degenerate BitField with width=0; the
        // call falls through to a generic Call (which evaluates the
        // body and gets the same 0).
        let css = r#"
            @property --AX { syntax: "<integer>"; inherits: true; initial-value: 0; }
            @function --lowerBytes(--a <integer>, --b <integer>) returns <integer> {
                result: mod(var(--a), pow(2, var(--b)));
            }
            :root { --AX: --lowerBytes(123, 0); }
        "#;
        let dag = build(css);
        let value_id = match &dag.nodes[dag.terminals[0] as usize] {
            DagNode::WriteVar { value, .. } => *value,
            other => panic!("expected WriteVar, got {other:?}"),
        };
        match &dag.nodes[value_id as usize] {
            DagNode::Call { .. } => {}
            other => panic!("expected fallback Call for width=0, got {other:?}"),
        }
    }
}
