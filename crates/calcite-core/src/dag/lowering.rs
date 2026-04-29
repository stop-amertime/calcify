//! ParsedProgram → DAG lowering.
//!
//! See `docs/v2-rewrite-design.md` § Lowering rules for the full spec.
//! Phase 1 is direct mechanical lowering with no idiom recognition;
//! every `Expr` variant maps to one or two `DagNode` variants.

use std::collections::HashMap;

use super::types::{CalcKind, Dag, DagNode, NodeId, SlotId, StyleCondNode, TickPosition};
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

/// Lowering context: a fresh DAG plus a transient-slot allocator for
/// property names that don't resolve to committed state.
struct LowerCtx<'a> {
    dag: Dag,
    /// Bare-name → SlotId for transient (per-tick scratch) properties.
    /// Allocated lazily as references and assignments reach unknown
    /// property names. Slot ids are encoded `TRANSIENT_BASE + idx`.
    transient_slots: HashMap<String, SlotId>,
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
    /// Currently-inlining function names. If a call site tries to
    /// inline a function already on this stack, we fall back to a
    /// FuncCall node (delegated to v1) — CSS @function doesn't support
    /// recursion, so this is defensive.
    inlining_stack: Vec<String>,
}

impl<'a> LowerCtx<'a> {
    fn new() -> Self {
        Self {
            dag: Dag::default(),
            transient_slots: HashMap::new(),
            assigned_names: std::collections::HashSet::new(),
            functions: HashMap::new(),
            inline_frames: Vec::new(),
            inlining_stack: Vec::new(),
        }
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
    /// Every name we see is recorded in `dag.name_to_slot` — that's
    /// v2's authoritative slot map. Consumers that need to resolve a
    /// property name to a slot (e.g. the broadcast executor reading
    /// `--_slot0Live`) should consult that map, not
    /// `property_to_address` (which doesn't know about transients).
    fn slot_for(&mut self, bare: &str) -> SlotId {
        if let Some(&s) = self.dag.name_to_slot.get(bare) {
            return s;
        }
        let slot = if let Some(s) = committed_slot_of(bare) {
            s
        } else {
            let id = crate::dag::TRANSIENT_BASE + self.transient_slots.len() as SlotId;
            self.transient_slots.insert(bare.to_string(), id);
            id
        };
        self.dag.name_to_slot.insert(bare.to_string(), slot);
        slot
    }

    /// Lower an `Expr` to a `NodeId`. Pure (no side effects beyond
    /// allocating new nodes).
    fn lower_expr(&mut self, expr: &Expr) -> NodeId {
        match expr {
            Expr::Literal(v) => self.dag.push(DagNode::Lit(*v)),

            Expr::StringLiteral(s) => self.dag.push(DagNode::LitStr(s.clone())),

            Expr::Var { name, fallback } => {
                let (bare, kind) = strip_var_name(name);
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
                    self.dag.push(DagNode::LoadVar { slot, kind })
                } else if let Some(fb) = fallback.as_deref() {
                    self.lower_expr(fb)
                } else {
                    // Unresolved, no fallback: CSS spec falls back to
                    // the property's registered initial value, which
                    // is 0 for unregistered numeric properties.
                    self.dag.push(DagNode::Lit(0.0))
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
                self.dag.push(DagNode::Calc { op: kind, args })
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
                // Idiom: dispatch table. If every branch tests
                // `style(--P: <integer-literal>)` against the same
                // property `--P`, lower to a Switch keyed on `--P`'s
                // current value. O(1) lookup vs O(N) scan; matches
                // the speedup v1 gets via `dispatch_tables`.
                if let Some(switch_id) = self.try_lower_dispatch(branches, fallback) {
                    return switch_id;
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

            Expr::Concat(parts) => {
                let lowered: Vec<NodeId> = parts.iter().map(|e| self.lower_expr(e)).collect();
                self.dag.push(DagNode::Concat(lowered))
            }
        }
    }

    /// Lower a function call: inline the body if possible, fall back
    /// to a `FuncCall` delegation node otherwise.
    ///
    /// Inlining model: push a frame mapping each param name to the
    /// corresponding caller-evaluated arg `NodeId`, then lower the
    /// callee's `result` expression. Locals are lowered sequentially,
    /// each adding to the same frame so subsequent locals (and the
    /// result) can reference them.
    ///
    /// Recursion: `inlining_stack` tracks function names currently
    /// being inlined. CSS `@function` doesn't allow recursion (Chrome
    /// rejects it), so encountering a recursive call falls back to a
    /// `FuncCall` delegation node — defensive, not expected.
    ///
    /// Unknown callees: if the program declares no function with this
    /// name, emit a `FuncCall` to mirror v1's behaviour
    /// (`eval_function_call` returns 0 for undefined functions); the
    /// walker will hit the same not-found path. The `fn_id` is interned
    /// so the call site has a stable handle.
    fn lower_call(&mut self, name: &str, arg_ids: Vec<NodeId>) -> NodeId {
        let func = match self.functions.get(name) {
            Some(f) => *f,
            None => {
                let fn_id = self.intern_function(name);
                return self.dag.push(DagNode::FuncCall { fn_id, args: arg_ids });
            }
        };
        if self.inlining_stack.iter().any(|n| n == name) {
            let fn_id = self.intern_function(name);
            return self.dag.push(DagNode::FuncCall { fn_id, args: arg_ids });
        }

        // Build the param/local frame. Missing args (caller passed
        // fewer than declared) bind to a literal 0, matching v1
        // (`unwrap_or(Value::Number(0.0))`).
        let mut frame: HashMap<String, NodeId> = HashMap::new();
        for (i, param) in func.parameters.iter().enumerate() {
            let bare = param.name.strip_prefix("--").unwrap_or(&param.name).to_string();
            let id = arg_ids.get(i).copied().unwrap_or_else(|| self.dag.push(DagNode::Lit(0.0)));
            frame.insert(bare, id);
        }

        self.inlining_stack.push(name.to_string());
        self.inline_frames.push(frame);

        // Lower locals sequentially. Each local's body sees previously-
        // bound params and earlier locals (frame is on the stack), and
        // its lowered NodeId becomes the binding for its own name.
        for local in &func.locals {
            let value_id = self.lower_expr(&local.value);
            let bare = local.name.strip_prefix("--").unwrap_or(&local.name).to_string();
            self.inline_frames
                .last_mut()
                .expect("inline frame just pushed")
                .insert(bare, value_id);
        }

        let result_id = self.lower_expr(&func.result);

        self.inline_frames.pop();
        self.inlining_stack.pop();

        result_id
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
        Some(self.dag.push(DagNode::Switch {
            key: key_id,
            table,
            fallback: fallback_id,
        }))
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
            self.dag.push(DagNode::LoadVar { slot, kind })
        } else {
            self.dag.push(DagNode::Lit(0.0))
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
                        self.dag.push(DagNode::LoadVar { slot, kind })
                    } else {
                        // Unresolved property in a style() test reads
                        // as 0 (the registered initial-value default
                        // for unregistered numeric properties).
                        self.dag.push(DagNode::Lit(0.0))
                    }
                } else {
                    let slot = self.slot_for(bare);
                    self.dag.push(DagNode::LoadVar { slot, kind })
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

    // Index function definitions by name so `lower_call` can resolve
    // callees and inline their bodies. Also intern each name so that
    // any call we can't inline (currently: recursive callees only —
    // CSS rejects them but the guard is defensive) still has a stable
    // `fn_id` for the FuncCall fallback delegated to v1.
    for f in &program.functions {
        ctx.functions.insert(f.name.clone(), f);
        ctx.intern_function(&f.name);
    }

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

    ctx.dag.broadcast_writes = broadcast_writes;
    ctx.dag.packed_broadcast_ports = packed_broadcast_ports;
    ctx.dag.absorbed_properties = absorbed;
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
        DagNode::FuncCall { args, .. } => {
            // Conservatively include arg-side reads. The function
            // body's own reads aren't traced here (the body evaluates
            // arguments, not the call site's other slots) — Phase 2
            // inlining will tighten this.
            for &a in args {
                collect_current_reads(dag, a, slot_to_term, out);
            }
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
    fn lowers_function_call_by_inlining() {
        // `--double(21)` should inline to `calc(21 * 2)` — i.e. a Calc
        // node whose first arg is the lit 21 (param substitution) and
        // second is the lit 2. No FuncCall node should remain because
        // there's no recursion and the callee is known.
        let css = r#"
            @property --AX { syntax: "<integer>"; inherits: true; initial-value: 0; }
            @function --double(--x <integer>) returns <integer> {
                result: calc(var(--x) * 2);
            }
            .cpu { --AX: --double(21); }
        "#;
        let dag = build(css);
        assert_eq!(dag.terminals.len(), 1);

        // No FuncCall in the DAG.
        for n in &dag.nodes {
            if matches!(n, DagNode::FuncCall { .. }) {
                panic!("expected no FuncCall after inlining; DAG: {:?}", dag.nodes);
            }
        }

        // Terminal's value is a Mul node.
        let terminal_id = dag.terminals[0];
        let value_id = match &dag.nodes[terminal_id as usize] {
            DagNode::WriteVar { value, .. } => *value,
            other => panic!("expected WriteVar, got {other:?}"),
        };
        match &dag.nodes[value_id as usize] {
            DagNode::Calc { op: CalcKind::Mul, args } => {
                assert_eq!(args.len(), 2);
                // Param substitution: arg[0] should resolve to Lit(21).
                match &dag.nodes[args[0] as usize] {
                    DagNode::Lit(v) => assert_eq!(*v, 21.0),
                    other => panic!("expected Lit(21) for substituted --x, got {other:?}"),
                }
                match &dag.nodes[args[1] as usize] {
                    DagNode::Lit(v) => assert_eq!(*v, 2.0),
                    other => panic!("expected Lit(2), got {other:?}"),
                }
            }
            other => panic!("expected Calc(Mul), got {other:?}"),
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
        // No FuncCall remains.
        for n in &dag.nodes {
            if matches!(n, DagNode::FuncCall { .. }) {
                panic!("expected no FuncCall after inlining");
            }
        }
        assert_eq!(dag.terminals.len(), 1);
    }
}
