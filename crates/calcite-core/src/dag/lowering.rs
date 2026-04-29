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

/// Resolve a bare property name to its slot.
///
/// Defers to `crate::eval::property_to_address`, which is v1's global
/// resolver (state vars + memory + dispatch-table-derived addresses).
fn slot_of(name: &str) -> Option<SlotId> {
    crate::eval::property_to_address(&format!("--{name}"))
}

/// Lowering context: a fresh DAG plus mutable function id allocator.
struct LowerCtx {
    dag: Dag,
}

impl LowerCtx {
    fn new() -> Self {
        Self { dag: Dag::default() }
    }

    fn intern_function(&mut self, name: &str) -> u32 {
        if let Some(&id) = self.dag.function_names.get(name) {
            return id;
        }
        let id = self.dag.function_names.len() as u32;
        self.dag.function_names.insert(name.to_string(), id);
        id
    }

    /// Lower an `Expr` to a `NodeId`. Pure (no side effects beyond
    /// allocating new nodes).
    fn lower_expr(&mut self, expr: &Expr) -> NodeId {
        match expr {
            Expr::Literal(v) => self.dag.push(DagNode::Lit(*v)),

            Expr::StringLiteral(s) => self.dag.push(DagNode::LitStr(s.clone())),

            Expr::Var { name, fallback } => {
                let (bare, kind) = strip_var_name(name);
                if let Some(slot) = slot_of(bare) {
                    self.dag.push(DagNode::LoadVar { slot, kind })
                } else if let Some(fb) = fallback.as_deref() {
                    // Unknown property with fallback: lower the fallback
                    // expression and use it directly. Matches v1's
                    // compile_var (compile.rs:1700).
                    self.lower_expr(fb)
                } else {
                    // Unknown property, no fallback: literal 0. Matches
                    // v1's compile.rs:1704.
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
                // The slot of a `style(--P: V)` test is always P's
                // *current-tick* slot. v1's evaluator reads style()
                // queries from this-tick computed values (or state on
                // miss). We don't carry a TickPosition for style tests
                // — Current is implicit.
                let slot = slot_of(bare).unwrap_or(0);
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
        let slot = slot_of(bare)?;
        let value = self.lower_expr(&a.value);
        let id = self.dag.push(DagNode::WriteVar { slot, value });
        Some(id)
    }

    fn lower_function(&mut self, f: &FunctionDef) -> NodeId {
        // Each function's body is its own sub-DAG; we lower the result
        // expression and record the root. Parameters and locals are
        // not lowered yet — Phase 1 walker uses v1's interpreter to
        // evaluate function calls. Phase 2 inlines.
        self.lower_expr(&f.result)
    }
}

/// Lower a `ParsedProgram` to a `Dag`.
pub fn lower_parsed_program(program: &ParsedProgram) -> Dag {
    let mut ctx = LowerCtx::new();

    // Lower each `@function`: intern the name, lower the body, record
    // the root. Sub-DAG is shared with the main DAG (one Vec<DagNode>).
    let mut function_roots: Vec<(u32, NodeId)> = Vec::new();
    for f in &program.functions {
        let fn_id = ctx.intern_function(&f.name);
        let root = ctx.lower_function(f);
        function_roots.push((fn_id, root));
    }
    // Pack into Dag::function_roots indexed by fn_id. Resize first.
    let max_id = ctx.dag.function_names.len();
    ctx.dag.function_roots = vec![NodeId::MAX; max_id];
    for (id, root) in function_roots {
        ctx.dag.function_roots[id as usize] = root;
    }

    // Lower each top-level assignment. Skip buffer copies and any
    // assignment whose slot doesn't resolve (the latter shouldn't
    // happen in practice — every property in a parsed program has a
    // declaration — but matches v1's filter behaviour).
    for a in &program.assignments {
        if let Some(terminal) = ctx.lower_assignment(a) {
            ctx.dag.terminals.push(terminal);
        }
    }

    // Phase 1: prebuilt broadcast writes from the parser fast-path get
    // an `IndirectStore` placeholder per port. The walker delegates
    // their evaluation to v1's executor for now (Phase 2 promotes them
    // to fully self-contained DAG nodes).
    for (i, _bw) in program.prebuilt_broadcast_writes.iter().enumerate() {
        let id = ctx.dag.push(DagNode::IndirectStore {
            port_id: i as u32,
            packed: false,
        });
        ctx.dag.terminals.push(id);
    }
    for (i, _port) in program.prebuilt_packed_broadcast_ports.iter().enumerate() {
        let id = ctx.dag.push(DagNode::IndirectStore {
            port_id: i as u32,
            packed: true,
        });
        ctx.dag.terminals.push(id);
    }

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
