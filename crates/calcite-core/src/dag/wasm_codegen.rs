//! Phase 3 (a): hand-emitted wasm codegen.
//!
//! For each `WriteVar` terminal whose cone is fully covered by the
//! op vocabulary below, emit one wasm function inside a single
//! per-cabinet wasm module. The module is built once at backend-
//! select time and instantiated against a host runtime
//! (wasmtime on native, `WebAssembly.instantiate` in the browser —
//! browser wiring is a follow-up).
//!
//! Portable: builds for both native and `wasm32-unknown-unknown`.
//! The host runtime lives in `wasm_host.rs` behind a
//! `cfg(not(target_arch = "wasm32"))` gate.
//!
//! ## State in linear memory
//!
//! State-var and transient cache values + presence flags live in the
//! wasm module's linear memory. Layout (computed at codegen time):
//!
//! ```text
//!   [0,                  n_state_vars*4)        state-var values
//!   [n_state_vars*4,    2*n_state_vars*4)       state-var present flags
//!   [2*n_state_vars*4,  ... + n_transients*4)   transient values
//!   [..,                ... + n_transients*4)   transient present flags
//! ```
//!
//! `WasmDag::layout` carries the offsets so the host can populate
//! the regions before each tick. Cache misses (flag == 0) fall
//! through to a host import (state-var) or just return 0 (transient).
//!
//! Memory reads (slot >= 0, < TRANSIENT_BASE) still go via host
//! import (`host_read_memory`) because their per-tick cache is sparse
//! (HashMap) and not worth materialising. Cross-tick reads
//! (`TickPosition::Prev`) go via `host_read_prev`.
//!
//! ## In-wasm arithmetic
//!
//! Add/Sub/Mul/Div/Mod/Min/Max/Neg/Abs/Sign/Clamp lower to pure
//! wasm i32 ops, matching the walker's f64 semantics on integer
//! inputs (the typical cabinet workload — verified by differential).
//! Pow and Round-with-interval keep the host trampoline because they
//! need real f64.
//!
//! ## Conditional sharing
//!
//! Locals persist for the lifetime of the wasm function but default
//! to zero. A node reachable from multiple Switch/If branches can't
//! safely cache as a local because if branch A computed it and
//! branch B reads it, B sees a stale zero. So `local_of` caching
//! is only honoured outside control flow (`inside_branch == false`).
//!
//! Cardinal rule: this module knows nothing about CSS-DOS, x86, or
//! cabinets. It walks `Dag` shapes — same vocabulary as the walker,
//! the closure prototype, and the inlined prototype.

use std::collections::HashMap;

use wasm_encoder::{
    BlockType, CodeSection, ConstExpr, EntityType, ExportKind, ExportSection, Function,
    FunctionSection, ImportSection, Instruction, MemArg, MemoryType, Module, TypeSection, ValType,
};

use crate::dag::types::{BitwiseKind, CalcKind, Dag, DagNode, NodeId, StyleCondNode, TickPosition};

// ---------- host imports ---------------------------------------------

/// `host_read_memory(slot: i32) -> i32`. Memory reads (slot >= 0,
/// < TRANSIENT_BASE) consult the host's sparse per-tick HashMap
/// cache, then fall through to `state.read_mem`.
pub const HOST_READ_MEMORY: u32 = 0;
/// `host_read_prev(slot: i32) -> i32`. Cross-tick reads bypass all
/// per-tick caches; goes straight to `state.read_mem`.
pub const HOST_READ_PREV: u32 = 1;
/// `host_calc_binary(kind: i32, a: i32, b: i32) -> i32`. Used for
/// `Pow`, the only binary calc that wasm i32 can't reproduce.
pub const HOST_CALC_BINARY: u32 = 2;
/// `host_calc_ternary(kind: i32, a: i32, b: i32, c: i32) -> i32`.
/// Used for `Round(strategy)`; strategy id is the third arg.
pub const HOST_CALC_TERNARY: u32 = 3;
/// User function indices start after the imports.
const N_IMPORTS: u32 = 4;

/// Op kinds for the `host_calc_*` trampolines. Trimmed to just the
/// f64-rounded ops that don't lower cleanly to pure wasm.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WasmCalcKind {
    Pow = 4,
    Round = 21,
}

impl WasmCalcKind {
    pub fn from_i32(v: i32) -> Option<WasmCalcKind> {
        Some(match v {
            4 => WasmCalcKind::Pow,
            21 => WasmCalcKind::Round,
            _ => return None,
        })
    }
}

pub const ROUND_NEAREST: i32 = 30;
pub const ROUND_UP: i32 = 31;
pub const ROUND_DOWN: i32 = 32;
pub const ROUND_TO_ZERO: i32 = 33;

// ---------- public types ---------------------------------------------

/// One emitted terminal: the wasm function index inside the module
/// and the local count (for diagnostics).
#[derive(Debug, Clone)]
pub struct EmittedWasmTerminal {
    pub func_idx: u32,
    pub local_count: u32,
}

/// Linear-memory layout of the per-cabinet wasm module. Sized at
/// codegen time from the host's `state.state_var_count()` and
/// `dag.transient_slot_count`.
#[derive(Debug, Clone, Copy)]
pub struct WasmMemoryLayout {
    pub n_state_vars: u32,
    pub n_transients: u32,
    pub sv_value_base: u32,
    pub sv_present_base: u32,
    pub tr_value_base: u32,
    pub tr_present_base: u32,
    pub mem_bytes: u32,
    pub mem_pages: u32,
}

impl WasmMemoryLayout {
    pub fn new(n_state_vars: u32, n_transients: u32) -> Self {
        let sv_value_base = 0u32;
        let sv_present_base = sv_value_base + n_state_vars * 4;
        let tr_value_base = sv_present_base + n_state_vars * 4;
        let tr_present_base = tr_value_base + n_transients * 4;
        let used = tr_present_base + n_transients * 4;
        let mem_pages = ((used as usize + 0xFFFF) / 0x10000).max(1) as u32;
        let mem_bytes = mem_pages * 0x10000;
        WasmMemoryLayout {
            n_state_vars,
            n_transients,
            sv_value_base,
            sv_present_base,
            tr_value_base,
            tr_present_base,
            mem_bytes,
            mem_pages,
        }
    }
}

/// A compiled wasm module for one cabinet.
pub struct WasmDag {
    pub dag: Dag,
    pub emitted: Vec<Option<EmittedWasmTerminal>>,
    pub wasm_bytes: Vec<u8>,
    pub layout: WasmMemoryLayout,
}

impl WasmDag {
    pub fn coverage(&self) -> (usize, usize) {
        let total = self.emitted.len();
        let covered = self.emitted.iter().filter(|x| x.is_some()).count();
        (total, covered)
    }

    /// Build a wasm module from `dag`. `n_state_vars` is the host's
    /// state-var slot count (used to size the wasm linear memory).
    pub fn build(dag: Dag, n_state_vars: u32) -> Self {
        let n_transients = dag.transient_slot_count as u32;
        let layout = WasmMemoryLayout::new(n_state_vars, n_transients);
        let mut builder = ModuleBuilder::new(layout);

        // Phase 1: emit one wasm function per `@function` body.
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

        // Fixpoint: try to lower each body; on failure mark its idx
        // as None; repeat until stable. Bodies whose Calls target a
        // now-failed body fail too.
        loop {
            let mut changed = false;
            for fn_id in 0..n_fns {
                if fn_wasm_idx[fn_id].is_none() {
                    continue;
                }
                let n_params = dag.function_param_counts[fn_id];
                let root = dag.function_roots[fn_id];
                let mut local_count: u32 = n_params;
                let mut local_of: HashMap<NodeId, u32> = HashMap::new();
                let mut body: Vec<Instruction<'static>> = Vec::new();
                let mut ctx = EmitCtx {
                    local_count: &mut local_count,
                    local_of: &mut local_of,
                    fn_wasm_idx: &fn_wasm_idx,
                    layout: &layout,
                };
                let ok = emit_value(&dag, root, &mut body, &mut ctx, false);
                if !ok {
                    fn_wasm_idx[fn_id] = None;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        // Final body emit (committing to module).
        for fn_id in 0..n_fns {
            let fn_type_idx = fn_type_indices[fn_id];
            let n_params = dag.function_param_counts[fn_id];
            let root = dag.function_roots[fn_id];
            let mut local_count: u32 = n_params;
            let mut local_of: HashMap<NodeId, u32> = HashMap::new();
            let mut body: Vec<Instruction<'static>> = Vec::new();
            let ok = if fn_wasm_idx[fn_id].is_some() {
                let mut ctx = EmitCtx {
                    local_count: &mut local_count,
                    local_of: &mut local_of,
                    fn_wasm_idx: &fn_wasm_idx,
                    layout: &layout,
                };
                emit_value(&dag, root, &mut body, &mut ctx, false)
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

        // Phase 2: terminal functions.
        let mut emitted: Vec<Option<EmittedWasmTerminal>> = Vec::with_capacity(dag.terminals.len());
        for &term_id in &dag.terminals {
            let result = match &dag.nodes[term_id as usize] {
                DagNode::WriteVar { value, .. } => {
                    emit_terminal(&dag, *value, &mut builder, &fn_wasm_idx)
                }
                _ => None,
            };
            emitted.push(result);
        }

        let wasm_bytes = builder.finish();
        WasmDag { dag, emitted, wasm_bytes, layout }
    }
}

// ---------- module builder -------------------------------------------

struct ModuleBuilder {
    types: TypeSection,
    imports: ImportSection,
    functions: FunctionSection,
    exports: ExportSection,
    code: CodeSection,
    memory: wasm_encoder::MemorySection,
    layout: WasmMemoryLayout,
    type_idx_terminal: u32,
    fn_type_cache: HashMap<u32, u32>,
    next_func_idx: u32,
}

impl ModuleBuilder {
    fn new(layout: WasmMemoryLayout) -> Self {
        let mut types = TypeSection::new();
        types.ty().function([], [ValType::I32]);
        let type_idx_terminal = 0;
        types.ty().function([ValType::I32], [ValType::I32]);
        let type_idx_read_slot = 1;
        types
            .ty()
            .function([ValType::I32, ValType::I32, ValType::I32], [ValType::I32]);
        let type_idx_calc_binary = 2;
        types.ty().function(
            [ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            [ValType::I32],
        );
        let type_idx_calc_ternary = 3;

        let mut imports = ImportSection::new();
        imports.import("host", "read_memory", EntityType::Function(type_idx_read_slot));
        imports.import("host", "read_prev", EntityType::Function(type_idx_read_slot));
        imports.import("host", "calc_binary", EntityType::Function(type_idx_calc_binary));
        imports.import("host", "calc_ternary", EntityType::Function(type_idx_calc_ternary));

        let mut memory = wasm_encoder::MemorySection::new();
        memory.memory(MemoryType {
            minimum: layout.mem_pages as u64,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        });

        let _ = (type_idx_calc_binary, type_idx_calc_ternary);

        ModuleBuilder {
            types,
            imports,
            functions: FunctionSection::new(),
            exports: ExportSection::new(),
            code: CodeSection::new(),
            memory,
            layout,
            type_idx_terminal,
            fn_type_cache: HashMap::new(),
            next_func_idx: N_IMPORTS,
        }
    }

    fn add_function_type(&mut self, n_params: u32) -> u32 {
        if let Some(&idx) = self.fn_type_cache.get(&n_params) {
            return idx;
        }
        let params: Vec<ValType> = (0..n_params).map(|_| ValType::I32).collect();
        let new_idx = 4 + self.fn_type_cache.len() as u32;
        self.types.ty().function(params, [ValType::I32]);
        self.fn_type_cache.insert(n_params, new_idx);
        new_idx
    }

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

    fn finish(mut self) -> Vec<u8> {
        self.exports.export("mem", ExportKind::Memory, 0);
        let mut module = Module::new();
        module.section(&self.types);
        module.section(&self.imports);
        module.section(&self.functions);
        module.section(&self.memory);
        module.section(&self.exports);
        module.section(&self.code);
        let _ = ConstExpr::i32_const(0);
        let _ = BlockType::Empty;
        module.finish()
    }
}

// ---------- emit context ---------------------------------------------

struct EmitCtx<'a> {
    local_count: &'a mut u32,
    local_of: &'a mut HashMap<NodeId, u32>,
    fn_wasm_idx: &'a [Option<u32>],
    layout: &'a WasmMemoryLayout,
}

impl<'a> EmitCtx<'a> {
    fn alloc_local(&mut self) -> u32 {
        let id = *self.local_count;
        *self.local_count += 1;
        id
    }
}

fn emit_terminal(
    dag: &Dag,
    value_root: NodeId,
    builder: &mut ModuleBuilder,
    fn_wasm_idx: &[Option<u32>],
) -> Option<EmittedWasmTerminal> {
    let mut local_count: u32 = 0;
    let mut local_of: HashMap<NodeId, u32> = HashMap::new();
    let mut body: Vec<Instruction<'static>> = Vec::new();
    let mut ctx = EmitCtx {
        local_count: &mut local_count,
        local_of: &mut local_of,
        fn_wasm_idx,
        layout: &builder.layout,
    };
    let ok = emit_value(dag, value_root, &mut body, &mut ctx, false);
    if !ok {
        return None;
    }
    let func_idx = builder.add_terminal(local_count, body);
    Some(EmittedWasmTerminal { func_idx, local_count })
}

// ---------- emit_value -----------------------------------------------

fn emit_value(
    dag: &Dag,
    node_id: NodeId,
    body: &mut Vec<Instruction<'static>>,
    ctx: &mut EmitCtx<'_>,
    inside_branch: bool,
) -> bool {
    if !inside_branch {
        if let Some(&local) = ctx.local_of.get(&node_id) {
            body.push(Instruction::LocalGet(local));
            return true;
        }
    }

    match &dag.nodes[node_id as usize] {
        DagNode::Lit(v) => {
            body.push(Instruction::I32Const(*v as i32));
        }
        DagNode::LoadVar { slot, kind } => {
            emit_load_var(body, *slot, *kind, ctx);
        }
        DagNode::Param(idx) => {
            body.push(Instruction::LocalGet(*idx));
            return true;
        }
        DagNode::Calc { op, args } => {
            if !emit_calc(dag, op, args, body, ctx, inside_branch) {
                return false;
            }
        }
        DagNode::BitField { src, shift, width } => {
            if *shift >= 32 {
                body.push(Instruction::I32Const(0));
            } else {
                if !emit_value(dag, *src, body, ctx, inside_branch) {
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
            if !emit_bitwise(dag, *kind, args, body, ctx, inside_branch) {
                return false;
            }
        }
        DagNode::Call { fn_id, args } => {
            let Some(target_idx) = ctx.fn_wasm_idx.get(*fn_id as usize).and_then(|x| *x) else {
                return false;
            };
            let expected_arity = dag.function_param_counts[*fn_id as usize] as usize;
            if expected_arity != args.len() {
                return false;
            }
            for a in args {
                if !emit_value(dag, *a, body, ctx, inside_branch) {
                    return false;
                }
            }
            body.push(Instruction::Call(target_idx));
        }
        DagNode::Switch { key, table, fallback } => {
            let key_local = ctx.alloc_local();
            if !emit_value(dag, *key, body, ctx, inside_branch) {
                return false;
            }
            body.push(Instruction::LocalSet(key_local));

            let mut entries: Vec<(i64, NodeId)> =
                table.iter().map(|(k, v)| (*k, *v)).collect();
            entries.sort_by_key(|x| x.0);
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
                if !emit_value(dag, *branch_id, body, ctx, true) {
                    return false;
                }
                body.push(Instruction::Else);
            }
            if !emit_value(dag, *fallback, body, ctx, true) {
                return false;
            }
            for _ in &entries {
                body.push(Instruction::End);
            }
        }
        DagNode::If { branches, fallback } => {
            if branches.is_empty() {
                return emit_value(dag, *fallback, body, ctx, inside_branch);
            }
            for (cond, branch) in branches {
                if !emit_style_cond(dag, cond, body, ctx, inside_branch) {
                    return false;
                }
                body.push(Instruction::If(BlockType::Result(ValType::I32)));
                if !emit_value(dag, *branch, body, ctx, true) {
                    return false;
                }
                body.push(Instruction::Else);
            }
            if !emit_value(dag, *fallback, body, ctx, true) {
                return false;
            }
            for _ in branches {
                body.push(Instruction::End);
            }
        }
        _ => return false,
    }

    if !inside_branch {
        match &dag.nodes[node_id as usize] {
            DagNode::Lit(_)
            | DagNode::LoadVar { .. }
            | DagNode::Calc { .. }
            | DagNode::BitField { .. }
            | DagNode::BitwiseOp { .. }
            | DagNode::Call { .. } => {
                let local = ctx.alloc_local();
                body.push(Instruction::LocalTee(local));
                ctx.local_of.insert(node_id, local);
            }
            _ => {}
        }
    }
    true
}

// ---------- LoadVar via linear memory --------------------------------

fn emit_load_var(
    body: &mut Vec<Instruction<'static>>,
    slot: i32,
    kind: TickPosition,
    ctx: &EmitCtx<'_>,
) {
    if matches!(kind, TickPosition::Prev) {
        body.push(Instruction::I32Const(slot));
        body.push(Instruction::Call(HOST_READ_PREV));
        return;
    }

    let layout = ctx.layout;
    if slot >= crate::dag::TRANSIENT_BASE {
        let idx = (slot - crate::dag::TRANSIENT_BASE) as i64;
        if idx < 0 || idx as u32 >= layout.n_transients {
            body.push(Instruction::I32Const(0));
            return;
        }
        let value_addr = layout.tr_value_base + (idx as u32) * 4;
        let present_addr = layout.tr_present_base + (idx as u32) * 4;
        // present ? value : 0 (transients with no cache hit return 0
        // — matches inlined runner's `unwrap_or(0)`).
        body.push(Instruction::I32Const(value_addr as i32));
        body.push(Instruction::I32Load(memarg_at(0)));
        body.push(Instruction::I32Const(0));
        body.push(Instruction::I32Const(present_addr as i32));
        body.push(Instruction::I32Load(memarg_at(0)));
        body.push(Instruction::Select);
        return;
    }
    if slot < 0 {
        let idx = (-slot - 1) as i64;
        if idx < 0 || idx as u32 >= layout.n_state_vars {
            body.push(Instruction::I32Const(slot));
            body.push(Instruction::Call(HOST_READ_MEMORY));
            return;
        }
        let value_addr = layout.sv_value_base + (idx as u32) * 4;
        let present_addr = layout.sv_present_base + (idx as u32) * 4;
        // if present then value else host_read_memory(slot).
        body.push(Instruction::I32Const(present_addr as i32));
        body.push(Instruction::I32Load(memarg_at(0)));
        body.push(Instruction::If(BlockType::Result(ValType::I32)));
        body.push(Instruction::I32Const(value_addr as i32));
        body.push(Instruction::I32Load(memarg_at(0)));
        body.push(Instruction::Else);
        body.push(Instruction::I32Const(slot));
        body.push(Instruction::Call(HOST_READ_MEMORY));
        body.push(Instruction::End);
        return;
    }
    body.push(Instruction::I32Const(slot));
    body.push(Instruction::Call(HOST_READ_MEMORY));
}

#[inline]
fn memarg_at(_offset: u64) -> MemArg {
    MemArg { offset: 0, align: 2, memory_index: 0 }
}

// ---------- Calc -----------------------------------------------------

fn emit_calc(
    dag: &Dag,
    op: &CalcKind,
    args: &[NodeId],
    body: &mut Vec<Instruction<'static>>,
    ctx: &mut EmitCtx<'_>,
    inside_branch: bool,
) -> bool {
    match (op, args.len()) {
        (CalcKind::Add, 2) => {
            if !emit_value(dag, args[0], body, ctx, inside_branch) { return false; }
            if !emit_value(dag, args[1], body, ctx, inside_branch) { return false; }
            body.push(Instruction::I32Add);
        }
        (CalcKind::Sub, 2) => {
            if !emit_value(dag, args[0], body, ctx, inside_branch) { return false; }
            if !emit_value(dag, args[1], body, ctx, inside_branch) { return false; }
            body.push(Instruction::I32Sub);
        }
        (CalcKind::Mul, 2) => {
            if !emit_value(dag, args[0], body, ctx, inside_branch) { return false; }
            if !emit_value(dag, args[1], body, ctx, inside_branch) { return false; }
            body.push(Instruction::I32Mul);
        }
        (CalcKind::Div, 2) => return emit_div(dag, args, body, ctx, inside_branch),
        (CalcKind::Mod, 2) => return emit_mod(dag, args, body, ctx, inside_branch),
        (CalcKind::Min, 2) => return emit_min_or_max(dag, args, body, ctx, inside_branch, true),
        (CalcKind::Max, 2) => return emit_min_or_max(dag, args, body, ctx, inside_branch, false),
        (CalcKind::Neg, 1) => {
            body.push(Instruction::I32Const(0));
            if !emit_value(dag, args[0], body, ctx, inside_branch) { return false; }
            body.push(Instruction::I32Sub);
        }
        (CalcKind::Abs, 1) => return emit_abs(dag, args, body, ctx, inside_branch),
        (CalcKind::Sign, 1) => return emit_sign(dag, args, body, ctx, inside_branch),
        (CalcKind::Clamp, 3) => return emit_clamp(dag, args, body, ctx, inside_branch),
        (CalcKind::Pow, 2) => {
            body.push(Instruction::I32Const(WasmCalcKind::Pow as i32));
            if !emit_value(dag, args[0], body, ctx, inside_branch) { return false; }
            if !emit_value(dag, args[1], body, ctx, inside_branch) { return false; }
            body.push(Instruction::Call(HOST_CALC_BINARY));
        }
        (CalcKind::Round(strategy), 2) => {
            body.push(Instruction::I32Const(WasmCalcKind::Round as i32));
            if !emit_value(dag, args[0], body, ctx, inside_branch) { return false; }
            if !emit_value(dag, args[1], body, ctx, inside_branch) { return false; }
            let strat_id = match strategy {
                crate::types::RoundStrategy::Nearest => ROUND_NEAREST,
                crate::types::RoundStrategy::Up => ROUND_UP,
                crate::types::RoundStrategy::Down => ROUND_DOWN,
                crate::types::RoundStrategy::ToZero => ROUND_TO_ZERO,
            };
            body.push(Instruction::I32Const(strat_id));
            body.push(Instruction::Call(HOST_CALC_TERNARY));
        }
        _ => return false,
    }
    true
}

fn emit_div(
    dag: &Dag,
    args: &[NodeId],
    body: &mut Vec<Instruction<'static>>,
    ctx: &mut EmitCtx<'_>,
    inside_branch: bool,
) -> bool {
    let n_local = ctx.alloc_local();
    let d_local = ctx.alloc_local();
    if !emit_value(dag, args[0], body, ctx, inside_branch) { return false; }
    body.push(Instruction::LocalSet(n_local));
    if !emit_value(dag, args[1], body, ctx, inside_branch) { return false; }
    body.push(Instruction::LocalSet(d_local));
    body.push(Instruction::LocalGet(d_local));
    body.push(Instruction::I32Eqz);
    body.push(Instruction::If(BlockType::Result(ValType::I32)));
    body.push(Instruction::I32Const(0));
    body.push(Instruction::Else);
    // Guard INT_MIN / -1 (i32.div_s would trap). Walker behaviour:
    // (INT_MIN as f64 / -1.0) as i32 == INT_MIN.
    body.push(Instruction::LocalGet(n_local));
    body.push(Instruction::I32Const(i32::MIN));
    body.push(Instruction::I32Eq);
    body.push(Instruction::LocalGet(d_local));
    body.push(Instruction::I32Const(-1));
    body.push(Instruction::I32Eq);
    body.push(Instruction::I32And);
    body.push(Instruction::If(BlockType::Result(ValType::I32)));
    body.push(Instruction::I32Const(i32::MIN));
    body.push(Instruction::Else);
    body.push(Instruction::LocalGet(n_local));
    body.push(Instruction::LocalGet(d_local));
    body.push(Instruction::I32DivS);
    body.push(Instruction::End);
    body.push(Instruction::End);
    true
}

fn emit_mod(
    dag: &Dag,
    args: &[NodeId],
    body: &mut Vec<Instruction<'static>>,
    ctx: &mut EmitCtx<'_>,
    inside_branch: bool,
) -> bool {
    let n_local = ctx.alloc_local();
    let d_local = ctx.alloc_local();
    if !emit_value(dag, args[0], body, ctx, inside_branch) { return false; }
    body.push(Instruction::LocalSet(n_local));
    if !emit_value(dag, args[1], body, ctx, inside_branch) { return false; }
    body.push(Instruction::LocalSet(d_local));
    body.push(Instruction::LocalGet(d_local));
    body.push(Instruction::I32Eqz);
    body.push(Instruction::If(BlockType::Result(ValType::I32)));
    body.push(Instruction::I32Const(0));
    body.push(Instruction::Else);
    body.push(Instruction::LocalGet(n_local));
    body.push(Instruction::I32Const(i32::MIN));
    body.push(Instruction::I32Eq);
    body.push(Instruction::LocalGet(d_local));
    body.push(Instruction::I32Const(-1));
    body.push(Instruction::I32Eq);
    body.push(Instruction::I32And);
    body.push(Instruction::If(BlockType::Result(ValType::I32)));
    body.push(Instruction::I32Const(0));
    body.push(Instruction::Else);
    body.push(Instruction::LocalGet(n_local));
    body.push(Instruction::LocalGet(d_local));
    body.push(Instruction::I32RemS);
    body.push(Instruction::End);
    body.push(Instruction::End);
    true
}

fn emit_min_or_max(
    dag: &Dag,
    args: &[NodeId],
    body: &mut Vec<Instruction<'static>>,
    ctx: &mut EmitCtx<'_>,
    inside_branch: bool,
    is_min: bool,
) -> bool {
    let a_local = ctx.alloc_local();
    let b_local = ctx.alloc_local();
    if !emit_value(dag, args[0], body, ctx, inside_branch) { return false; }
    body.push(Instruction::LocalSet(a_local));
    if !emit_value(dag, args[1], body, ctx, inside_branch) { return false; }
    body.push(Instruction::LocalSet(b_local));
    body.push(Instruction::LocalGet(a_local));
    body.push(Instruction::LocalGet(b_local));
    body.push(Instruction::LocalGet(a_local));
    body.push(Instruction::LocalGet(b_local));
    body.push(if is_min { Instruction::I32LtS } else { Instruction::I32GtS });
    body.push(Instruction::Select);
    true
}

fn emit_abs(
    dag: &Dag,
    args: &[NodeId],
    body: &mut Vec<Instruction<'static>>,
    ctx: &mut EmitCtx<'_>,
    inside_branch: bool,
) -> bool {
    let a_local = ctx.alloc_local();
    if !emit_value(dag, args[0], body, ctx, inside_branch) { return false; }
    body.push(Instruction::LocalSet(a_local));
    body.push(Instruction::LocalGet(a_local));
    body.push(Instruction::I32Const(0));
    body.push(Instruction::LocalGet(a_local));
    body.push(Instruction::I32Sub);
    body.push(Instruction::LocalGet(a_local));
    body.push(Instruction::I32Const(0));
    body.push(Instruction::I32GeS);
    body.push(Instruction::Select);
    true
}

fn emit_sign(
    dag: &Dag,
    args: &[NodeId],
    body: &mut Vec<Instruction<'static>>,
    ctx: &mut EmitCtx<'_>,
    inside_branch: bool,
) -> bool {
    let a_local = ctx.alloc_local();
    if !emit_value(dag, args[0], body, ctx, inside_branch) { return false; }
    body.push(Instruction::LocalSet(a_local));
    // (x > 0 ? 1 : 0) - (x < 0 ? 1 : 0)
    body.push(Instruction::I32Const(1));
    body.push(Instruction::I32Const(0));
    body.push(Instruction::LocalGet(a_local));
    body.push(Instruction::I32Const(0));
    body.push(Instruction::I32GtS);
    body.push(Instruction::Select);
    body.push(Instruction::I32Const(1));
    body.push(Instruction::I32Const(0));
    body.push(Instruction::LocalGet(a_local));
    body.push(Instruction::I32Const(0));
    body.push(Instruction::I32LtS);
    body.push(Instruction::Select);
    body.push(Instruction::I32Sub);
    true
}

fn emit_clamp(
    dag: &Dag,
    args: &[NodeId],
    body: &mut Vec<Instruction<'static>>,
    ctx: &mut EmitCtx<'_>,
    inside_branch: bool,
) -> bool {
    let lo_local = ctx.alloc_local();
    let v_local = ctx.alloc_local();
    let hi_local = ctx.alloc_local();
    let max_local = ctx.alloc_local();
    if !emit_value(dag, args[0], body, ctx, inside_branch) { return false; }
    body.push(Instruction::LocalSet(lo_local));
    if !emit_value(dag, args[1], body, ctx, inside_branch) { return false; }
    body.push(Instruction::LocalSet(v_local));
    if !emit_value(dag, args[2], body, ctx, inside_branch) { return false; }
    body.push(Instruction::LocalSet(hi_local));
    // max(v, lo)
    body.push(Instruction::LocalGet(v_local));
    body.push(Instruction::LocalGet(lo_local));
    body.push(Instruction::LocalGet(v_local));
    body.push(Instruction::LocalGet(lo_local));
    body.push(Instruction::I32GtS);
    body.push(Instruction::Select);
    body.push(Instruction::LocalSet(max_local));
    // min(max_local, hi)
    body.push(Instruction::LocalGet(max_local));
    body.push(Instruction::LocalGet(hi_local));
    body.push(Instruction::LocalGet(max_local));
    body.push(Instruction::LocalGet(hi_local));
    body.push(Instruction::I32LtS);
    body.push(Instruction::Select);
    true
}

// ---------- StyleCondNode --------------------------------------------

fn emit_style_cond(
    dag: &Dag,
    cond: &StyleCondNode,
    body: &mut Vec<Instruction<'static>>,
    ctx: &mut EmitCtx<'_>,
    inside_branch: bool,
) -> bool {
    match cond {
        StyleCondNode::Single { lhs, value } => {
            if !emit_value(dag, *lhs, body, ctx, inside_branch) { return false; }
            if !emit_value(dag, *value, body, ctx, inside_branch) { return false; }
            body.push(Instruction::I32Eq);
        }
        StyleCondNode::And(parts) => {
            if parts.is_empty() {
                body.push(Instruction::I32Const(1));
                return true;
            }
            for part in &parts[..parts.len() - 1] {
                if !emit_style_cond(dag, part, body, ctx, inside_branch) { return false; }
                body.push(Instruction::If(BlockType::Result(ValType::I32)));
            }
            if !emit_style_cond(dag, &parts[parts.len() - 1], body, ctx, inside_branch) {
                return false;
            }
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
                if !emit_style_cond(dag, part, body, ctx, inside_branch) { return false; }
                body.push(Instruction::If(BlockType::Result(ValType::I32)));
                body.push(Instruction::I32Const(1));
                body.push(Instruction::Else);
            }
            if !emit_style_cond(dag, &parts[parts.len() - 1], body, ctx, inside_branch) {
                return false;
            }
            for _ in 0..parts.len() - 1 {
                body.push(Instruction::End);
            }
        }
    }
    true
}

// ---------- BitwiseOp ------------------------------------------------

fn emit_bitwise(
    dag: &Dag,
    kind: BitwiseKind,
    args: &[NodeId],
    body: &mut Vec<Instruction<'static>>,
    ctx: &mut EmitCtx<'_>,
    inside_branch: bool,
) -> bool {
    let mask16 = 0xFFFFi32;
    match kind {
        BitwiseKind::Not => {
            if args.len() != 1 { return false; }
            if !emit_value(dag, args[0], body, ctx, inside_branch) { return false; }
            body.push(Instruction::I32Const(mask16));
            body.push(Instruction::I32And);
            body.push(Instruction::I32Const(-1));
            body.push(Instruction::I32Xor);
            body.push(Instruction::I32Const(mask16));
            body.push(Instruction::I32And);
        }
        BitwiseKind::And | BitwiseKind::Or | BitwiseKind::Xor => {
            if args.len() != 2 { return false; }
            if !emit_value(dag, args[0], body, ctx, inside_branch) { return false; }
            body.push(Instruction::I32Const(mask16));
            body.push(Instruction::I32And);
            if !emit_value(dag, args[1], body, ctx, inside_branch) { return false; }
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

        let wasm_dag = WasmDag::build(dag, 4);
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
        let combine = dag.push(DagNode::Calc { op: CalcKind::Mul, args: vec![add, sub] });
        let term = dag.push(DagNode::WriteVar { slot: -1, value: combine });
        dag.terminals.push(term);

        let wasm_dag = WasmDag::build(dag, 4);
        assert_eq!(wasm_dag.coverage(), (1, 1));
        #[cfg(not(target_arch = "wasm32"))]
        {
            let engine = wasmtime::Engine::default();
            wasmtime::Module::validate(&engine, &wasm_dag.wasm_bytes).expect("valid wasm");
        }
    }

    #[test]
    fn memory_layout_sizes_correctly() {
        let layout = WasmMemoryLayout::new(100, 50);
        assert_eq!(layout.sv_value_base, 0);
        assert_eq!(layout.sv_present_base, 400);
        assert_eq!(layout.tr_value_base, 800);
        assert_eq!(layout.tr_present_base, 1000);
        assert_eq!(layout.mem_pages, 1);
        assert_eq!(layout.mem_bytes, 65536);
    }
}
