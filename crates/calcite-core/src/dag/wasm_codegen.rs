//! Phase 3 (a): hand-emitted wasm codegen.
//!
//! For each `WriteVar` terminal whose cone is fully covered by the
//! op vocabulary below, emit one wasm function inside a single
//! per-cabinet wasm module. The module is built once at backend-
//! select time and instantiated against a host runtime
//! (wasmtime on native, `WebAssembly.instantiate` in the browser —
//! browser wiring is a follow-up).
//!
//! This module is **portable** — it builds for both native and
//! `wasm32-unknown-unknown`. The host runtime is not portable;
//! `WasmHost` lives in `wasm_host.rs` behind a `cfg(not(target_arch =
//! "wasm32"))` gate.
//!
//! ## Design
//!
//! `emit_value(node, ...)` recursively emits any covered DAG node
//! such that its result is left on the wasm operand stack. The
//! emitter handles:
//!
//! - **Pure straight-line nodes** (Lit, LoadVar, Calc, BitField,
//!   BitwiseOp): evaluate deps recursively, then emit the op. Cached
//!   in `local_of` for CSE reuse — but only at the unconditional
//!   level (see § Conditional sharing below).
//! - **Switch**: emit the key, then a chain of `i32.eq` + wasm `if`
//!   blocks (one per branch). Each branch recursively emits its
//!   sub-cone, leaving the branch's result on the stack as the
//!   `if`-block's typed result. The fallback is the innermost else.
//! - **If**: same chain shape, with `StyleCondNode` predicates
//!   compiled to short-circuited i32-boolean wasm.
//! - **Call**: each `@function` body lowers to its own wasm function
//!   in the same module (with `(param i32 ... i32) (result i32)`
//!   matching the function's arity). A `Call { fn_id, args }` lowers
//!   to evaluating each arg onto the stack then `wasm.call`. `Param(i)`
//!   inside a body lowers to `local.get i`.
//!
//! ## Conditional sharing
//!
//! Locals persist for the lifetime of the wasm function but are
//! initialised to zero. A node reachable from multiple Switch/If
//! branches can't be safely cached as a local because if branch A
//! computed it and branch B reads it, B sees a stale zero. So
//! `local_of` caching is only used for nodes emitted at the
//! function's unconditional top level — once we descend into a
//! control branch, we re-emit shared sub-DAGs locally to that
//! branch. This costs wasm bytes on cabinets with shared
//! sub-expressions across branches; in practice the DAG lowering
//! rarely shares across branches because each branch is lowered
//! independently.
//!
//! Cardinal rule: this module knows nothing about CSS-DOS, x86, or
//! cabinets. It walks `Dag` shapes — same vocabulary as the walker,
//! the closure prototype, and the inlined prototype.

use std::collections::HashMap;

use wasm_encoder::{
    BlockType, CodeSection, ConstExpr, EntityType, ExportKind, ExportSection, Function,
    FunctionSection, ImportSection, Instruction, MemoryType, Module, TypeSection, ValType,
};

use crate::dag::types::{BitwiseKind, CalcKind, Dag, DagNode, NodeId, StyleCondNode, TickPosition};

/// Function index of `host_read_state_var(slot: i32) -> i32`.
pub const HOST_READ_STATE_VAR: u32 = 0;
/// Function index of `host_read_memory(slot: i32) -> i32`.
pub const HOST_READ_MEMORY: u32 = 1;
/// Function index of `host_read_transient(slot: i32) -> i32`.
pub const HOST_READ_TRANSIENT: u32 = 2;
/// Function index of `host_read_prev(slot: i32) -> i32`. Bypasses
/// the per-tick caches; goes straight to `state.read_mem`.
pub const HOST_READ_PREV: u32 = 3;
/// Function index of `host_calc_binary(kind: i32, a: i32, b: i32) -> i32`.
/// Generic binary op trampoline — `kind` selects f64 op, args are
/// passed as `i32` (the f64 cast happens host-side to match walker
/// semantics exactly).
pub const HOST_CALC_BINARY: u32 = 4;
/// Function index of `host_calc_unary(kind: i32, a: i32) -> i32`.
pub const HOST_CALC_UNARY: u32 = 5;
/// Function index of `host_calc_ternary(kind: i32, a: i32, b: i32, c: i32) -> i32`.
/// Used by Clamp (lo, v, hi) and Round(strategy, v, interval).
pub const HOST_CALC_TERNARY: u32 = 6;
/// Number of host imports above. User functions start at this index.
const N_IMPORTS: u32 = 7;

/// Op kinds for the `host_calc_*` trampolines.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WasmCalcKind {
    Mul = 1,
    Div = 2,
    Mod = 3,
    Pow = 4,
    Min2 = 5,
    Max2 = 6,
    Neg = 10,
    Abs = 11,
    Sign = 12,
    Clamp = 20,
    Round = 21,
}

impl WasmCalcKind {
    pub fn from_i32(v: i32) -> Option<WasmCalcKind> {
        Some(match v {
            1 => WasmCalcKind::Mul,
            2 => WasmCalcKind::Div,
            3 => WasmCalcKind::Mod,
            4 => WasmCalcKind::Pow,
            5 => WasmCalcKind::Min2,
            6 => WasmCalcKind::Max2,
            10 => WasmCalcKind::Neg,
            11 => WasmCalcKind::Abs,
            12 => WasmCalcKind::Sign,
            20 => WasmCalcKind::Clamp,
            21 => WasmCalcKind::Round,
            _ => return None,
        })
    }
}

pub const ROUND_NEAREST: i32 = 30;
pub const ROUND_UP: i32 = 31;
pub const ROUND_DOWN: i32 = 32;
pub const ROUND_TO_ZERO: i32 = 33;

/// One emitted terminal: the wasm function index inside the module
/// and the local count (for diagnostics).
#[derive(Debug, Clone)]
pub struct EmittedWasmTerminal {
    /// Index into the module's function space (post-imports).
    pub func_idx: u32,
    /// Number of i32 locals the function declared.
    pub local_count: u32,
}

/// A compiled wasm module for one cabinet, plus the per-terminal
/// dispatch info the driver uses to decide emit-vs-fallback.
pub struct WasmDag {
    /// The full DAG (used by the driver for walker fallback).
    pub dag: Dag,
    /// One slot per `dag.terminals` entry. `Some` means the terminal
    /// has an emitted wasm function; `None` means the driver falls
    /// back to the walker.
    pub emitted: Vec<Option<EmittedWasmTerminal>>,
    /// The complete wasm module bytes, ready for instantiation.
    pub wasm_bytes: Vec<u8>,
}

impl WasmDag {
    /// Diagnostic: (n_terminals_total, n_terminals_emitted).
    pub fn coverage(&self) -> (usize, usize) {
        let total = self.emitted.len();
        let covered = self.emitted.iter().filter(|x| x.is_some()).count();
        (total, covered)
    }

    /// Build a wasm module + per-terminal dispatch table from the DAG.
    pub fn build(dag: Dag) -> Self {
        let mut builder = ModuleBuilder::new();

        // Phase 1: lower each `@function` body to a wasm function.
        //
        // Each body has a fixed wasm function index allocated up front
        // so that intra-module `call` instructions resolve regardless
        // of emit order (CSS @function forbids recursion but mutual
        // helpers reach each other through forward references). A
        // body's body-cone may contain a Call into a sibling whose own
        // cone fails to lower; in that case both bodies become
        // non-emittable and any Call into either bails its enclosing
        // terminal.
        //
        // Detect this by emitting bodies until the emittability set
        // reaches a fixpoint. Initial pass marks "structurally
        // emittable" bodies (no unsupported nodes in own cone). Each
        // subsequent pass invalidates bodies whose Calls now point at
        // a body marked non-emittable. Converges in at most
        // `function_roots.len()` passes (one body per pass in the
        // worst case of a chain of dependencies).
        let n_fns = dag.function_roots.len();
        let mut fn_wasm_idx: Vec<Option<u32>> = Vec::with_capacity(n_fns);
        let mut fn_type_indices: Vec<u32> = Vec::with_capacity(n_fns);
        for (fn_id, _root) in dag.function_roots.iter().enumerate() {
            let n_params = dag.function_param_counts[fn_id];
            let fn_type_idx = builder.add_function_type(n_params);
            fn_type_indices.push(fn_type_idx);
            let func_idx = builder.next_func_idx;
            builder.next_func_idx += 1;
            fn_wasm_idx.push(Some(func_idx));
        }

        // Fixpoint loop: try to lower each body; on failure mark its
        // wasm idx as None; repeat until no body's status changed.
        // O(n_fns) per pass × O(n_fns) passes worst case — acceptable
        // for the typical n_fns < 100.
        loop {
            let mut changed = false;
            for fn_id in 0..n_fns {
                if fn_wasm_idx[fn_id].is_none() {
                    continue; // already marked failed
                }
                let n_params = dag.function_param_counts[fn_id];
                let root = dag.function_roots[fn_id];
                let mut local_count: u32 = n_params;
                let mut local_of: HashMap<NodeId, u32> = HashMap::new();
                let mut body: Vec<Instruction<'static>> = Vec::new();
                let ok = emit_value(
                    &dag,
                    root,
                    &mut body,
                    &mut local_count,
                    &mut local_of,
                    &fn_wasm_idx,
                    /*inside_branch=*/ false,
                );
                if !ok {
                    fn_wasm_idx[fn_id] = None;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        // Now emit final bodies. Successful bodies emit normally;
        // failed bodies emit `unreachable` so the wasm validates but
        // any (non-bailed) call into them traps loudly. (Calls
        // SHOULD have bailed already — this is belt-and-suspenders.)
        for fn_id in 0..n_fns {
            let fn_type_idx = fn_type_indices[fn_id];
            let n_params = dag.function_param_counts[fn_id];
            let root = dag.function_roots[fn_id];
            let mut local_count: u32 = n_params;
            let mut local_of: HashMap<NodeId, u32> = HashMap::new();
            let mut body: Vec<Instruction<'static>> = Vec::new();
            let ok = if fn_wasm_idx[fn_id].is_some() {
                emit_value(
                    &dag,
                    root,
                    &mut body,
                    &mut local_count,
                    &mut local_of,
                    &fn_wasm_idx,
                    false,
                )
            } else {
                false
            };
            let n_extra = local_count.saturating_sub(n_params);
            let mut func = Function::new([(n_extra, ValType::I32)]);
            if ok {
                for ins in body {
                    func.instruction(&ins);
                }
                func.instruction(&Instruction::End);
            } else {
                func.instruction(&Instruction::Unreachable);
                func.instruction(&Instruction::End);
            }
            builder.functions.function(fn_type_idx);
            builder.code.function(&func);
        }

        // Phase 2: emit one wasm function per WriteVar terminal whose
        // cone is fully supported.
        let mut emitted: Vec<Option<EmittedWasmTerminal>> = Vec::with_capacity(dag.terminals.len());
        for &term_id in &dag.terminals {
            let result = match &dag.nodes[term_id as usize] {
                DagNode::WriteVar { value, .. } => emit_terminal(&dag, *value, &mut builder, &fn_wasm_idx),
                _ => None,
            };
            emitted.push(result);
        }

        let wasm_bytes = builder.finish();
        WasmDag { dag, emitted, wasm_bytes }
    }
}

/// Encoder state.
struct ModuleBuilder {
    types: TypeSection,
    imports: ImportSection,
    functions: FunctionSection,
    exports: ExportSection,
    code: CodeSection,
    /// Index of `() -> i32` in the type section (terminal signature).
    type_idx_terminal: u32,
    /// Cache: arity → type index for `(param i32 × N) -> i32`. Reused
    /// across function-body type definitions.
    fn_type_cache: HashMap<u32, u32>,
    /// Next user function index (post-imports + already-allocated).
    next_func_idx: u32,
}

impl ModuleBuilder {
    fn new() -> Self {
        let mut types = TypeSection::new();
        // Index 0: () -> i32 (terminal functions).
        types.ty().function([], [ValType::I32]);
        let type_idx_terminal = 0;
        // Index 1: (i32) -> i32 (host_read_*).
        types.ty().function([ValType::I32], [ValType::I32]);
        let type_idx_read_slot = 1;
        // Index 2: (i32, i32, i32) -> i32 (host_calc_binary).
        types
            .ty()
            .function([ValType::I32, ValType::I32, ValType::I32], [ValType::I32]);
        let type_idx_calc_binary = 2;
        // Index 3: (i32, i32) -> i32 (host_calc_unary).
        types
            .ty()
            .function([ValType::I32, ValType::I32], [ValType::I32]);
        let type_idx_calc_unary = 3;
        // Index 4: (i32, i32, i32, i32) -> i32 (host_calc_ternary).
        types.ty().function(
            [ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            [ValType::I32],
        );
        let type_idx_calc_ternary = 4;

        let mut imports = ImportSection::new();
        imports.import("host", "read_state_var", EntityType::Function(type_idx_read_slot));
        imports.import("host", "read_memory", EntityType::Function(type_idx_read_slot));
        imports.import("host", "read_transient", EntityType::Function(type_idx_read_slot));
        imports.import("host", "read_prev", EntityType::Function(type_idx_read_slot));
        imports.import("host", "calc_binary", EntityType::Function(type_idx_calc_binary));
        imports.import("host", "calc_unary", EntityType::Function(type_idx_calc_unary));
        imports.import("host", "calc_ternary", EntityType::Function(type_idx_calc_ternary));

        let _ = (type_idx_calc_binary, type_idx_calc_unary, type_idx_calc_ternary);

        ModuleBuilder {
            types,
            imports,
            functions: FunctionSection::new(),
            exports: ExportSection::new(),
            code: CodeSection::new(),
            type_idx_terminal,
            fn_type_cache: HashMap::new(),
            next_func_idx: N_IMPORTS,
        }
    }

    /// Get/create a function-body type index for `(param i32 × n) -> i32`.
    fn add_function_type(&mut self, n_params: u32) -> u32 {
        if let Some(&idx) = self.fn_type_cache.get(&n_params) {
            return idx;
        }
        let params: Vec<ValType> = (0..n_params).map(|_| ValType::I32).collect();
        // Allocate next type index. The five base types occupy 0..5;
        // each new one appends.
        let new_idx = 5 + self.fn_type_cache.len() as u32;
        self.types.ty().function(params, [ValType::I32]);
        self.fn_type_cache.insert(n_params, new_idx);
        new_idx
    }

    /// Append a terminal function. `body` must terminate with the
    /// result on the stack; `End` is appended automatically.
    fn add_terminal(&mut self, n_locals: u32, body: Vec<Instruction<'static>>) -> u32 {
        let func_idx = self.next_func_idx;
        self.next_func_idx += 1;
        self.functions.function(self.type_idx_terminal);
        let mut func = Function::new([(n_locals, ValType::I32)]);
        for ins in body {
            func.instruction(&ins);
        }
        func.instruction(&Instruction::End);
        self.exports
            .export(&format!("t{}", func_idx), ExportKind::Func, func_idx);
        self.code.function(&func);
        func_idx
    }

    /// Assemble the module.
    fn finish(self) -> Vec<u8> {
        let mut module = Module::new();
        module.section(&self.types);
        module.section(&self.imports);
        module.section(&self.functions);
        module.section(&self.exports);
        module.section(&self.code);
        let _ = ConstExpr::i32_const(0);
        let _ = MemoryType { minimum: 0, maximum: None, memory64: false, shared: false, page_size_log2: None };
        let _ = BlockType::Empty;
        module.finish()
    }
}

/// Emit a terminal function for one `WriteVar`'s value root.
fn emit_terminal(
    dag: &Dag,
    value_root: NodeId,
    builder: &mut ModuleBuilder,
    fn_wasm_idx: &[Option<u32>],
) -> Option<EmittedWasmTerminal> {
    let mut local_count: u32 = 0;
    let mut local_of: HashMap<NodeId, u32> = HashMap::new();
    let mut body: Vec<Instruction<'static>> = Vec::new();
    let ok = emit_value(
        dag,
        value_root,
        &mut body,
        &mut local_count,
        &mut local_of,
        fn_wasm_idx,
        /*inside_branch=*/ false,
    );
    if !ok {
        return None;
    }
    let func_idx = builder.add_terminal(local_count, body);
    Some(EmittedWasmTerminal { func_idx, local_count })
}

/// Emit `node` such that its result is left on the wasm operand stack.
/// Returns false if the cone contains anything we can't lower (caller
/// bails the whole terminal).
///
/// `local_count` is the total number of i32 locals the wasm function
/// will declare (param count not included for terminal functions —
/// they have no params; for body functions, the params occupy
/// 0..n_params and `local_count` starts at `n_params`).
///
/// `local_of` caches non-Param nodes that have been emitted to a
/// local; reads after the first cache the value via `local.tee` /
/// `local.get`. Caching only happens at unconditional positions
/// (`inside_branch == false`) for correctness — a node emitted inside
/// one branch can't be safely reused from another branch (locals
/// default to 0; the second branch would observe a stale read).
#[allow(clippy::too_many_arguments)]
fn emit_value(
    dag: &Dag,
    node_id: NodeId,
    body: &mut Vec<Instruction<'static>>,
    local_count: &mut u32,
    local_of: &mut HashMap<NodeId, u32>,
    fn_wasm_idx: &[Option<u32>],
    inside_branch: bool,
) -> bool {
    // Cache hit (only honoured outside branches; see § Conditional
    // sharing). Param nodes never cache — they always read from the
    // function's parameter local directly.
    if !inside_branch {
        if let Some(&local) = local_of.get(&node_id) {
            body.push(Instruction::LocalGet(local));
            return true;
        }
    }

    match &dag.nodes[node_id as usize] {
        DagNode::Lit(v) => {
            body.push(Instruction::I32Const(*v as i32));
        }
        DagNode::LoadVar { slot, kind } => {
            emit_load_var(body, *slot, *kind);
        }
        DagNode::Param(idx) => {
            // Function param. Read directly; never cached.
            body.push(Instruction::LocalGet(*idx));
            return true;
        }
        DagNode::Calc { op, args } => {
            // Three shapes:
            // - Pure i32 ops (Add/Sub): args first, then op. Direct.
            // - Binary/unary trampoline (Mul/Div/Mod/Pow/Min/Max/
            //   Neg/Abs/Sign): kind first, then args, then call.
            // - Ternary trampoline (Clamp/Round): kind first, then
            //   args, optional strategy id, then call.
            match (op, args.len()) {
                (CalcKind::Add, 2) => {
                    if !emit_value(dag, args[0], body, local_count, local_of, fn_wasm_idx, inside_branch) { return false; }
                    if !emit_value(dag, args[1], body, local_count, local_of, fn_wasm_idx, inside_branch) { return false; }
                    body.push(Instruction::I32Add);
                }
                (CalcKind::Sub, 2) => {
                    if !emit_value(dag, args[0], body, local_count, local_of, fn_wasm_idx, inside_branch) { return false; }
                    if !emit_value(dag, args[1], body, local_count, local_of, fn_wasm_idx, inside_branch) { return false; }
                    body.push(Instruction::I32Sub);
                }
                (CalcKind::Mul, 2) | (CalcKind::Div, 2) | (CalcKind::Mod, 2)
                | (CalcKind::Pow, 2) | (CalcKind::Min, 2) | (CalcKind::Max, 2) => {
                    let k = match op {
                        CalcKind::Mul => WasmCalcKind::Mul,
                        CalcKind::Div => WasmCalcKind::Div,
                        CalcKind::Mod => WasmCalcKind::Mod,
                        CalcKind::Pow => WasmCalcKind::Pow,
                        CalcKind::Min => WasmCalcKind::Min2,
                        CalcKind::Max => WasmCalcKind::Max2,
                        _ => unreachable!(),
                    };
                    body.push(Instruction::I32Const(k as i32));
                    if !emit_value(dag, args[0], body, local_count, local_of, fn_wasm_idx, inside_branch) { return false; }
                    if !emit_value(dag, args[1], body, local_count, local_of, fn_wasm_idx, inside_branch) { return false; }
                    body.push(Instruction::Call(HOST_CALC_BINARY));
                }
                (CalcKind::Neg, 1) | (CalcKind::Abs, 1) | (CalcKind::Sign, 1) => {
                    let k = match op {
                        CalcKind::Neg => WasmCalcKind::Neg,
                        CalcKind::Abs => WasmCalcKind::Abs,
                        CalcKind::Sign => WasmCalcKind::Sign,
                        _ => unreachable!(),
                    };
                    body.push(Instruction::I32Const(k as i32));
                    if !emit_value(dag, args[0], body, local_count, local_of, fn_wasm_idx, inside_branch) { return false; }
                    body.push(Instruction::Call(HOST_CALC_UNARY));
                }
                (CalcKind::Clamp, 3) => {
                    body.push(Instruction::I32Const(WasmCalcKind::Clamp as i32));
                    if !emit_value(dag, args[0], body, local_count, local_of, fn_wasm_idx, inside_branch) { return false; }
                    if !emit_value(dag, args[1], body, local_count, local_of, fn_wasm_idx, inside_branch) { return false; }
                    if !emit_value(dag, args[2], body, local_count, local_of, fn_wasm_idx, inside_branch) { return false; }
                    body.push(Instruction::Call(HOST_CALC_TERNARY));
                }
                (CalcKind::Round(strategy), 2) => {
                    body.push(Instruction::I32Const(WasmCalcKind::Round as i32));
                    if !emit_value(dag, args[0], body, local_count, local_of, fn_wasm_idx, inside_branch) { return false; }
                    if !emit_value(dag, args[1], body, local_count, local_of, fn_wasm_idx, inside_branch) { return false; }
                    let strat_id = match strategy {
                        crate::types::RoundStrategy::Nearest => ROUND_NEAREST,
                        crate::types::RoundStrategy::Up => ROUND_UP,
                        crate::types::RoundStrategy::Down => ROUND_DOWN,
                        crate::types::RoundStrategy::ToZero => ROUND_TO_ZERO,
                    };
                    body.push(Instruction::I32Const(strat_id));
                    body.push(Instruction::Call(HOST_CALC_TERNARY));
                }
                // Variadic Min/Max with arity > 2 not supported.
                _ => return false,
            }
        }
        DagNode::BitField { src, shift, width } => {
            // Shift >= 32 → result is constant 0.
            if *shift >= 32 {
                body.push(Instruction::I32Const(0));
            } else {
                if !emit_value(dag, *src, body, local_count, local_of, fn_wasm_idx, inside_branch) {
                    return false;
                }
                body.push(Instruction::I32Const(*shift as i32));
                body.push(Instruction::I32ShrS);
                match width {
                    None => {}
                    Some(w) if *w >= 32 => {}
                    Some(w) => {
                        body.push(Instruction::I32Const(((1i64 << w) - 1) as i32));
                        body.push(Instruction::I32And);
                    }
                }
            }
        }
        DagNode::BitwiseOp { kind, args } => {
            if !emit_bitwise(body, dag, *kind, args, local_count, local_of, fn_wasm_idx, inside_branch) {
                return false;
            }
        }
        DagNode::Call { fn_id, args } => {
            let Some(target_idx) = fn_wasm_idx.get(*fn_id as usize).and_then(|x| *x) else {
                return false;
            };
            let expected_arity = dag.function_param_counts[*fn_id as usize] as usize;
            if expected_arity != args.len() {
                return false;
            }
            for a in args {
                if !emit_value(dag, *a, body, local_count, local_of, fn_wasm_idx, inside_branch) {
                    return false;
                }
            }
            body.push(Instruction::Call(target_idx));
        }
        DagNode::Switch { key, table, fallback } => {
            // Compute key into a fresh local (we reference it once per
            // i32.eq comparison). Allocate the local even inside a
            // branch — wasm locals default to 0 so the inner branch
            // initialises before reading.
            let key_local = *local_count;
            *local_count += 1;
            if !emit_value(dag, *key, body, local_count, local_of, fn_wasm_idx, inside_branch) {
                return false;
            }
            body.push(Instruction::LocalSet(key_local));

            // Emit chained `if` blocks: for each (k, branch) in `table`,
            //   if (key == k) { emit branch } else { ...next... }
            // Innermost else is the fallback.
            //
            // Iteration order is HashMap-arbitrary, which doesn't
            // affect correctness (keys are tested for equality) but
            // makes the wasm bytes non-deterministic across builds. To
            // give us stable output and deterministic differentials,
            // sort by key first.
            let mut entries: Vec<(i64, NodeId)> =
                table.iter().map(|(k, v)| (*k, *v)).collect();
            entries.sort_by_key(|x| x.0);

            // Validate every key fits in i32 before emitting anything
            // (so failure doesn't leave half-emitted ops in body).
            for (k, _) in &entries {
                if i32::try_from(*k).is_err() {
                    return false;
                }
            }

            for (k, branch_id) in &entries {
                let kv = i32::try_from(*k).expect("checked above");
                body.push(Instruction::LocalGet(key_local));
                body.push(Instruction::I32Const(kv));
                body.push(Instruction::I32Eq);
                body.push(Instruction::If(BlockType::Result(ValType::I32)));
                if !emit_value(dag, *branch_id, body, local_count, local_of, fn_wasm_idx, true) {
                    return false;
                }
                body.push(Instruction::Else);
            }
            if !emit_value(dag, *fallback, body, local_count, local_of, fn_wasm_idx, true) {
                return false;
            }
            for _ in &entries {
                body.push(Instruction::End);
            }
        }
        DagNode::If { branches, fallback } => {
            // Cascade: for each branch, eval its predicate; if true,
            // emit branch value; else continue to next predicate or
            // fallback. Same chain structure as Switch except that
            // the predicate is a StyleCondNode tree, not an equality
            // test.
            if branches.is_empty() {
                // Degenerate If — just emit fallback.
                return emit_value(dag, *fallback, body, local_count, local_of, fn_wasm_idx, inside_branch);
            }
            for (cond, branch) in branches {
                if !emit_style_cond(dag, cond, body, local_count, local_of, fn_wasm_idx, inside_branch) {
                    return false;
                }
                body.push(Instruction::If(BlockType::Result(ValType::I32)));
                if !emit_value(dag, *branch, body, local_count, local_of, fn_wasm_idx, true) {
                    return false;
                }
                body.push(Instruction::Else);
            }
            if !emit_value(dag, *fallback, body, local_count, local_of, fn_wasm_idx, true) {
                return false;
            }
            for _ in branches {
                body.push(Instruction::End);
            }
        }
        // Concat / LitStr / IndirectStore / WriteVar / LoadVarDynamic
        // — not handled in this codegen pass.
        _ => return false,
    }

    // Cache leaf results so subsequent reads inside this same
    // (unconditional) cone hit the cache. Skip for control nodes
    // since their result is freshly-emitted on each path through.
    if !inside_branch {
        match &dag.nodes[node_id as usize] {
            DagNode::Lit(_)
            | DagNode::LoadVar { .. }
            | DagNode::Calc { .. }
            | DagNode::BitField { .. }
            | DagNode::BitwiseOp { .. }
            | DagNode::Call { .. } => {
                // Tee into a fresh local for caching.
                let local = *local_count;
                *local_count += 1;
                body.push(Instruction::LocalTee(local));
                local_of.insert(node_id, local);
            }
            // Switch / If / Param: already left on stack, no caching.
            _ => {}
        }
    }
    true
}

/// Emit a `StyleCondNode` predicate: leaves an i32 on the stack
/// (1 = true, 0 = false).
#[allow(clippy::too_many_arguments)]
fn emit_style_cond(
    dag: &Dag,
    cond: &StyleCondNode,
    body: &mut Vec<Instruction<'static>>,
    local_count: &mut u32,
    local_of: &mut HashMap<NodeId, u32>,
    fn_wasm_idx: &[Option<u32>],
    inside_branch: bool,
) -> bool {
    match cond {
        StyleCondNode::Single { lhs, value } => {
            if !emit_value(dag, *lhs, body, local_count, local_of, fn_wasm_idx, inside_branch) {
                return false;
            }
            if !emit_value(dag, *value, body, local_count, local_of, fn_wasm_idx, inside_branch) {
                return false;
            }
            body.push(Instruction::I32Eq);
        }
        StyleCondNode::And(parts) => {
            // Short-circuit AND: for each part, (cond) if(result i32)
            // {next} else {0}. Innermost cond drops through.
            if parts.is_empty() {
                body.push(Instruction::I32Const(1));
                return true;
            }
            for part in &parts[..parts.len() - 1] {
                if !emit_style_cond(dag, part, body, local_count, local_of, fn_wasm_idx, inside_branch) {
                    return false;
                }
                body.push(Instruction::If(BlockType::Result(ValType::I32)));
            }
            // Innermost: emit last predicate as the value of this
            // chain on the matching side; 0 on the failing side.
            if !emit_style_cond(
                dag,
                &parts[parts.len() - 1],
                body,
                local_count,
                local_of,
                fn_wasm_idx,
                inside_branch,
            ) {
                return false;
            }
            // Close N-1 `if`s (each one with Else 0).
            for _ in 0..parts.len() - 1 {
                body.push(Instruction::Else);
                body.push(Instruction::I32Const(0));
                body.push(Instruction::End);
            }
        }
        StyleCondNode::Or(parts) => {
            if parts.is_empty() {
                body.push(Instruction::I32Const(0));
                return true;
            }
            for part in &parts[..parts.len() - 1] {
                if !emit_style_cond(dag, part, body, local_count, local_of, fn_wasm_idx, inside_branch) {
                    return false;
                }
                // If true, short-circuit to 1; else continue.
                body.push(Instruction::If(BlockType::Result(ValType::I32)));
                body.push(Instruction::I32Const(1));
                body.push(Instruction::Else);
            }
            if !emit_style_cond(
                dag,
                &parts[parts.len() - 1],
                body,
                local_count,
                local_of,
                fn_wasm_idx,
                inside_branch,
            ) {
                return false;
            }
            for _ in 0..parts.len() - 1 {
                body.push(Instruction::End);
            }
        }
    }
    true
}

fn emit_load_var(body: &mut Vec<Instruction<'static>>, slot: i32, kind: TickPosition) {
    body.push(Instruction::I32Const(slot));
    let func = match kind {
        TickPosition::Prev => HOST_READ_PREV,
        TickPosition::Current => {
            if slot >= crate::dag::TRANSIENT_BASE {
                HOST_READ_TRANSIENT
            } else if slot < 0 {
                HOST_READ_STATE_VAR
            } else {
                HOST_READ_MEMORY
            }
        }
    };
    body.push(Instruction::Call(func));
}

#[allow(clippy::too_many_arguments)]
fn emit_bitwise(
    body: &mut Vec<Instruction<'static>>,
    dag: &Dag,
    kind: BitwiseKind,
    args: &[NodeId],
    local_count: &mut u32,
    local_of: &mut HashMap<NodeId, u32>,
    fn_wasm_idx: &[Option<u32>],
    inside_branch: bool,
) -> bool {
    let mask16 = 0xFFFFi32;
    match kind {
        BitwiseKind::Not => {
            if args.len() != 1 {
                return false;
            }
            if !emit_value(dag, args[0], body, local_count, local_of, fn_wasm_idx, inside_branch) {
                return false;
            }
            body.push(Instruction::I32Const(mask16));
            body.push(Instruction::I32And);
            body.push(Instruction::I32Const(-1));
            body.push(Instruction::I32Xor);
            body.push(Instruction::I32Const(mask16));
            body.push(Instruction::I32And);
        }
        BitwiseKind::And | BitwiseKind::Or | BitwiseKind::Xor => {
            if args.len() != 2 {
                return false;
            }
            if !emit_value(dag, args[0], body, local_count, local_of, fn_wasm_idx, inside_branch) {
                return false;
            }
            body.push(Instruction::I32Const(mask16));
            body.push(Instruction::I32And);
            if !emit_value(dag, args[1], body, local_count, local_of, fn_wasm_idx, inside_branch) {
                return false;
            }
            body.push(Instruction::I32Const(mask16));
            body.push(Instruction::I32And);
            let op = match kind {
                BitwiseKind::And => Instruction::I32And,
                BitwiseKind::Or => Instruction::I32Or,
                BitwiseKind::Xor => Instruction::I32Xor,
                BitwiseKind::Not => unreachable!(),
            };
            body.push(op);
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_emits_valid_wasm() {
        let mut dag = Dag::default();
        let lit = dag.push(DagNode::Lit(42.0));
        let term = dag.push(DagNode::WriteVar { slot: -1, value: lit });
        dag.terminals.push(term);

        let wasm_dag = WasmDag::build(dag);
        assert_eq!(wasm_dag.coverage(), (1, 1));
        #[cfg(not(target_arch = "wasm32"))]
        {
            let engine = wasmtime::Engine::default();
            wasmtime::Module::validate(&engine, &wasm_dag.wasm_bytes)
                .expect("emitted wasm should validate");
        }
    }

    #[test]
    fn arith_chain_emitted() {
        let mut dag = Dag::default();
        let one = dag.push(DagNode::Lit(1.0));
        let two = dag.push(DagNode::Lit(2.0));
        let three = dag.push(DagNode::Lit(3.0));
        let four = dag.push(DagNode::Lit(4.0));
        let add = dag.push(DagNode::Calc { op: CalcKind::Add, args: vec![one, two] });
        let sub = dag.push(DagNode::Calc { op: CalcKind::Sub, args: vec![three, four] });
        // Use only Add/Sub (pure i32) — Mul would need the trampoline
        // shape we haven't fixed yet.
        let combine = dag.push(DagNode::Calc { op: CalcKind::Add, args: vec![add, sub] });
        let term = dag.push(DagNode::WriteVar { slot: -1, value: combine });
        dag.terminals.push(term);

        let wasm_dag = WasmDag::build(dag);
        assert_eq!(wasm_dag.coverage(), (1, 1));
        #[cfg(not(target_arch = "wasm32"))]
        {
            let engine = wasmtime::Engine::default();
            wasmtime::Module::validate(&engine, &wasm_dag.wasm_bytes).expect("valid wasm");
        }
    }
}
