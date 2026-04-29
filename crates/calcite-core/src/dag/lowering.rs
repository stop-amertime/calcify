//! ParsedProgram → DAG lowering.
//!
//! See `docs/v2-rewrite-design.md` § Lowering rules for the full spec.
//! Phase 1 is direct mechanical lowering with no idiom recognition;
//! every `Expr` variant maps to one or two `DagNode` variants.

use std::collections::HashMap;

use super::types::{CalcKind, Dag, DagNode, NodeId, SlotId, StyleCondNode, TickPosition};
use crate::types::{
    Assignment, CalcOp, Expr, ParsedProgram, StyleBranch, StyleTest,
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
struct LowerCtx {
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
}

impl LowerCtx {
    fn new() -> Self {
        Self {
            dag: Dag::default(),
            transient_slots: HashMap::new(),
            assigned_names: std::collections::HashSet::new(),
        }
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
                // CSS spec: `var(--x, fb)` evaluates fb only when --x
                // is unresolved. "Unresolved" means: --x has neither
                // a registered @property declaration nor any
                // assignment that produces it. v2 distinguishes:
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
                let fn_id = self.intern_function(name);
                let lowered: Vec<NodeId> = args.iter().map(|e| self.lower_expr(e)).collect();
                self.dag.push(DagNode::FuncCall { fn_id, args: lowered })
            }

            Expr::StyleCondition { branches, fallback } => {
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

    fn lower_style_test(&mut self, test: &StyleTest) -> StyleCondNode {
        match test {
            StyleTest::Single { property, value } => {
                let (bare, _kind) = strip_var_name(property);
                // The slot of a `style(--P: V)` test is P's current-
                // tick slot. May be transient if --P is computed but
                // not @property-declared (e.g. --opcode in CSS-DOS
                // cabinets). The walker handles transient slots in
                // both reads (LoadVar) and StyleCondNode lookups.
                let slot = self.slot_for(bare);
                let value_id = self.lower_expr(value);
                StyleCondNode::Single { slot, value: value_id }
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

    // Intern function names so call sites can reference them by id.
    // Phase 1 does NOT lower function bodies — see lower_function
    // above for the rationale. The walker delegates entire calls to
    // v1's eval_function_call, which handles parameter binding.
    for f in &program.functions {
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
        StyleCondNode::Single { slot, value } => {
            if let Some(&idx) = slot_to_term.get(slot) {
                out.push(idx);
            }
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
    fn lowers_function_call() {
        let css = r#"
            @property --AX { syntax: "<integer>"; inherits: true; initial-value: 0; }
            @function --double(--x <integer>) returns <integer> {
                result: calc(var(--x) * 2);
            }
            .cpu { --AX: --double(21); }
        "#;
        let dag = build(css);
        // The function should be interned and have a root.
        assert_eq!(dag.function_names.len(), 1);
        let id = dag.function_names["--double"];
        assert!(dag.function_roots[id as usize] != NodeId::MAX);
        // The terminal's value should be a FuncCall referencing it.
        let terminal_id = dag.terminals[0];
        match &dag.nodes[terminal_id as usize] {
            DagNode::WriteVar { value, .. } => match &dag.nodes[*value as usize] {
                DagNode::FuncCall { fn_id, .. } => assert_eq!(*fn_id, id),
                other => panic!("expected FuncCall, got {other:?}"),
            },
            other => panic!("expected WriteVar, got {other:?}"),
        }
    }
}
