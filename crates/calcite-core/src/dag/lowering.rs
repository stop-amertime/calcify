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
use crate::State;

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

/// Build a slot map from `State` after `load_properties` has run.
///
/// State vars get their negative slot indices; memory bytes get their
/// non-negative addresses. The map is keyed by *bare* name (no `--`,
/// no triple-buffer prefix).
fn build_slot_map(state: &State) -> HashMap<String, SlotId> {
    let mut map: HashMap<String, SlotId> = HashMap::new();
    // State vars: index i → slot -(i+1).
    for (i, name) in state_var_names(state).iter().enumerate() {
        map.insert(name.clone(), -(i as SlotId + 1));
    }
    // Memory bytes: every property `mN` → slot N. We can't enumerate
    // them from `State` directly (memory is just a flat Vec), but
    // `crate::eval::property_to_address` is the canonical resolver
    // and covers state vars + mN + dispatch-table-derived addresses.
    // We delegate to it lazily during `slot_of` rather than pre-
    // populating the map, which would miss late-bound entries.
    map
}

/// Get state var names from State.
///
/// Phase 1 stub: empty. `slot_of` uses `eval::property_to_address` as
/// the authoritative resolver (the same global address map v1 uses),
/// so we don't need to pre-build a slot map by name. Phase 2 may
/// switch to a per-DAG slot map for cache locality, at which point
/// this becomes the right place to enumerate.
fn state_var_names(_state: &State) -> Vec<String> {
    Vec::new()
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
pub fn lower_parsed_program(program: &ParsedProgram, state: &State) -> Dag {
    let mut ctx = LowerCtx::new();
    ctx.dag.slot_map = build_slot_map(state);

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

    ctx.dag
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_css;

    fn build(css: &str) -> Dag {
        let parsed = parse_css(css).expect("parse");
        let mut state = State::default();
        state.load_properties(&parsed.properties);
        lower_parsed_program(&parsed, &state)
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
