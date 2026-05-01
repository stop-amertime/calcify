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
//! `WasmHost` lives in `eval.rs` behind a `cfg(not(target_arch =
//! "wasm32"))` gate.
//!
//! ## Design starting point
//!
//! The (c+) `inlined_codegen.rs` prototype proved that per-terminal
//! straight-line emission is the correct shape (6/6 differentials,
//! bit-identical to the walker). Its op vocabulary translates 1:1
//! to wasm:
//!
//! | InlinedOp | wasm encoding |
//! |---|---|
//! | `LoadLit { v }` | `i32.const v` |
//! | `LoadVar { slot, kind }` | `i32.const slot` + `call $host_read_*` |
//! | `Add { a, b }` | `local.get a; local.get b; i32.add` |
//! | `Sub` / `Mul` | `i32.sub` / `i32.mul` |
//! | `Div` / `Mod` | f64 trampoline (wasm `i32.div_s` would diverge) |
//! | `Min2` / `Max2` / `Pow` / `Round` | f64 trampoline |
//! | `BitField` | `i32.shr_s` + `i32.and (1<<w)-1` |
//! | `Bitwise` | `i32.and` / `i32.or` / `i32.xor` (16-bit masked) |
//! | `Clamp` | f64 trampoline |
//! | `Neg` / `Abs` / `Sign` | f64 trampoline |
//!
//! Locals are wasm `i32` locals; `dst` indices map 1:1 to wasm local
//! indices. Topo order guarantees no use-before-write.
//!
//! For ops that have to match the walker's f64-rounding semantics
//! exactly (Mul, Div, Mod, Pow, Min2, Max2, Round, Clamp, Neg, Abs,
//! Sign), the emitted wasm calls a host import (`host_calc_*`) that
//! evaluates the op in Rust, matching `inlined_codegen::run_terminal`
//! arm-for-arm. This sacrifices some of the wasm performance win on
//! those ops in exchange for correctness day-one; pure-wasm i64
//! implementations can land in a follow-up if the profile motivates it.
//!
//! The walker fallback for terminals whose cones contain
//! Switch/Call/If/Concat/LoadVarDynamic/LitStr is handled by the
//! driver in `eval.rs` (same as the inlined backend), not by this
//! module.
//!
//! ## Cardinal rule
//!
//! This module knows nothing about CSS-DOS, x86, or cabinets. It
//! walks `Dag` shapes — same vocabulary as the walker, the closure
//! prototype, and the inlined prototype.

use std::collections::HashMap;

use wasm_encoder::{
    BlockType, CodeSection, ConstExpr, EntityType, ExportKind, ExportSection, Function,
    FunctionSection, ImportSection, Instruction, MemoryType, Module, TypeSection, ValType,
};

use crate::dag::types::{BitwiseKind, CalcKind, Dag, DagNode, NodeId, TickPosition};

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
/// Used by Clamp (lo, v, hi) and Round(strategy, v, interval) (the
/// strategy is encoded into `kind`).
pub const HOST_CALC_TERNARY: u32 = 6;

/// Op kinds for the `host_calc_*` trampolines. The wasm-side passes
/// these as `i32.const` immediates; the host side dispatches on them
/// to call into the matching arithmetic. Stable wire format — values
/// must match `WasmCalcKind::from_i32` in the host.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WasmCalcKind {
    // Binary
    Mul = 1,
    Div = 2,
    Mod = 3,
    Pow = 4,
    Min2 = 5,
    Max2 = 6,
    // Unary
    Neg = 10,
    Abs = 11,
    Sign = 12,
    // Ternary: Clamp(lo, v, hi)
    Clamp = 20,
    // Ternary: Round(v, interval, strategy_id) where strategy_id =
    // 30 (Nearest), 31 (Up), 32 (Down), 33 (ToZero).
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

/// Stable wire IDs for `RoundStrategy`, used as the third argument to
/// `host_calc_ternary` when `kind == Round`. Must match the host's
/// dispatch.
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
///
/// The `dag` is held by-value so the driver can route walker fallback
/// for uncovered terminals through the same `Dag` the wasm module was
/// built from — no risk of slot-id drift between the two sides.
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
    ///
    /// Returns a `WasmDag` whose `wasm_bytes` is a complete, valid
    /// wasm module. Always succeeds — terminals with unsupported
    /// cones get `None` in `emitted` and the driver falls back to the
    /// walker for them.
    pub fn build(dag: Dag) -> Self {
        let mut builder = ModuleBuilder::new();
        let mut emitted: Vec<Option<EmittedWasmTerminal>> = Vec::with_capacity(dag.terminals.len());

        for &term_id in &dag.terminals {
            let result = match &dag.nodes[term_id as usize] {
                DagNode::WriteVar { value, .. } => emit_terminal(&dag, *value, &mut builder),
                _ => None,
            };
            emitted.push(result);
        }

        let wasm_bytes = builder.finish();
        WasmDag { dag, emitted, wasm_bytes }
    }
}

/// Encoder state. Wraps `wasm-encoder` sections; we add to them as we
/// emit each terminal function, then assemble at `finish()` time.
struct ModuleBuilder {
    types: TypeSection,
    imports: ImportSection,
    functions: FunctionSection,
    exports: ExportSection,
    code: CodeSection,
    /// Index of the terminal-function signature `() -> i32`. Used
    /// once per terminal in `add_terminal`.
    type_idx_terminal: u32,
    /// Next function index to assign in the post-imports function
    /// space. Imports occupy [0, n_imports); user functions follow.
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

        // Imports must appear in the same order as the HOST_*
        // constants above so func indices line up.
        let mut imports = ImportSection::new();
        imports.import("host", "read_state_var", EntityType::Function(type_idx_read_slot));
        imports.import("host", "read_memory", EntityType::Function(type_idx_read_slot));
        imports.import("host", "read_transient", EntityType::Function(type_idx_read_slot));
        imports.import("host", "read_prev", EntityType::Function(type_idx_read_slot));
        imports.import(
            "host",
            "calc_binary",
            EntityType::Function(type_idx_calc_binary),
        );
        imports.import(
            "host",
            "calc_unary",
            EntityType::Function(type_idx_calc_unary),
        );
        imports.import(
            "host",
            "calc_ternary",
            EntityType::Function(type_idx_calc_ternary),
        );

        // The non-terminal type indices (read_slot, calc_*) are
        // referenced by the import section above and don't need to
        // be carried forward — terminal-function emission only uses
        // `type_idx_terminal`.
        let _ = (
            type_idx_read_slot,
            type_idx_calc_binary,
            type_idx_calc_unary,
            type_idx_calc_ternary,
        );

        ModuleBuilder {
            types,
            imports,
            functions: FunctionSection::new(),
            exports: ExportSection::new(),
            code: CodeSection::new(),
            type_idx_terminal,
            // 7 imports preceding any user function.
            next_func_idx: 7,
        }
    }

    /// Append one terminal function. `body` carries the encoded
    /// instruction stream (terminating with `End`). `local_count` is
    /// the number of i32 locals to declare.
    fn add_terminal(&mut self, local_count: u32, body: Vec<Instruction<'static>>) -> u32 {
        let func_idx = self.next_func_idx;
        self.next_func_idx += 1;
        self.functions.function(self.type_idx_terminal);
        let mut func = Function::new([(local_count, ValType::I32)]);
        for ins in body {
            func.instruction(&ins);
        }
        // Export the terminal function under a stable name keyed on
        // its index so the host can fetch it by name without a side
        // channel.
        self.exports
            .export(&format!("t{}", func_idx), ExportKind::Func, func_idx);
        self.code.function(&func);
        func_idx
    }

    /// Assemble the module. Sections must appear in the order wasm
    /// requires (types, imports, functions, exports, code).
    fn finish(self) -> Vec<u8> {
        let mut module = Module::new();
        module.section(&self.types);
        module.section(&self.imports);
        module.section(&self.functions);
        module.section(&self.exports);
        module.section(&self.code);
        // No memory section — Phase 3 (a) uses host imports for state
        // access, no linear memory needed yet. (Adding one is a
        // follow-up if/when we move state access to in-wasm memory.)
        let _ = ConstExpr::i32_const(0); // silence "unused import" if added back later
        let _ = MemoryType { minimum: 0, maximum: None, memory64: false, shared: false, page_size_log2: None };
        let _ = BlockType::Empty;
        module.finish()
    }
}

/// Walk the cone of `value_root`. If every node is supported, emit a
/// wasm function for it and return its `EmittedWasmTerminal`. Returns
/// `None` if the cone contains anything we can't yet emit.
fn emit_terminal(
    dag: &Dag,
    value_root: NodeId,
    builder: &mut ModuleBuilder,
) -> Option<EmittedWasmTerminal> {
    // Topo-order the cone via explicit-stack DFS (same shape as
    // inlined_codegen). Each node gets one wasm local.
    let mut order: Vec<NodeId> = Vec::new();
    let mut local_of: HashMap<NodeId, u32> = HashMap::new();
    let mut visited: HashMap<NodeId, bool> = HashMap::new();
    let mut stack: Vec<(NodeId, usize)> = Vec::new();
    stack.push((value_root, 0));
    while let Some(&(node_id, ci)) = stack.last() {
        if visited.get(&node_id).copied().unwrap_or(false) {
            stack.pop();
            continue;
        }
        let deps = node_deps(&dag.nodes[node_id as usize])?;
        if ci < deps.len() {
            stack.last_mut().unwrap().1 = ci + 1;
            let child = deps[ci];
            if visited.get(&child).copied().unwrap_or(false) {
                continue;
            }
            stack.push((child, 0));
        } else {
            stack.pop();
            visited.insert(node_id, true);
            let local = order.len() as u32;
            local_of.insert(node_id, local);
            order.push(node_id);
        }
    }

    // Emit wasm body. Each node's compute writes to its local; a
    // final `local.get result` leaves the value on the stack.
    let mut body: Vec<Instruction<'static>> = Vec::with_capacity(order.len() * 4);

    for &node_id in &order {
        let dst = local_of[&node_id];
        match &dag.nodes[node_id as usize] {
            DagNode::Lit(v) => {
                body.push(Instruction::I32Const(*v as i32));
                body.push(Instruction::LocalSet(dst));
            }
            DagNode::LoadVar { slot, kind } => {
                emit_load_var(&mut body, *slot, *kind);
                body.push(Instruction::LocalSet(dst));
            }
            DagNode::Calc { op, args } => {
                if !emit_calc(&mut body, op, args, &local_of) {
                    return None;
                }
                body.push(Instruction::LocalSet(dst));
            }
            DagNode::BitField { src, shift, width } => {
                emit_bitfield(&mut body, local_of[src], *shift, *width);
                body.push(Instruction::LocalSet(dst));
            }
            DagNode::BitwiseOp { kind, args } => {
                emit_bitwise(&mut body, *kind, args, &local_of);
                body.push(Instruction::LocalSet(dst));
            }
            _ => return None,
        }
    }

    // Result on the stack, then End.
    body.push(Instruction::LocalGet(local_of[&value_root]));
    body.push(Instruction::End);

    let local_count = order.len() as u32;
    let func_idx = builder.add_terminal(local_count, body);
    Some(EmittedWasmTerminal { func_idx, local_count })
}

/// Per-node dependency list. None means the node kind isn't supported
/// (terminal bails to walker fallback).
fn node_deps(node: &DagNode) -> Option<Vec<NodeId>> {
    match node {
        DagNode::Lit(_) => Some(Vec::new()),
        DagNode::LoadVar { .. } => Some(Vec::new()),
        DagNode::Calc { op, args } => match (op, args.len()) {
            (CalcKind::Sign | CalcKind::Abs | CalcKind::Neg, 1) => Some(args.clone()),
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
            (CalcKind::Clamp, 3) => Some(args.clone()),
            // Variadic Min/Max with >2 args not yet supported.
            _ => None,
        },
        DagNode::BitField { src, .. } => Some(vec![*src]),
        DagNode::BitwiseOp { args, .. } => Some(args.clone()),
        // Phase 3 (a) day-one coverage matches the inlined prototype:
        // Call, Param, If, Switch, Concat, LitStr, IndirectStore,
        // WriteVar (handled at terminal level), LoadVarDynamic — all
        // bail. Each gets its own follow-up.
        _ => None,
    }
}

fn emit_load_var(body: &mut Vec<Instruction<'static>>, slot: i32, kind: TickPosition) {
    // Same dispatch shape as inlined_codegen::read_slot_dispatch:
    //   - kind=Prev → host_read_prev
    //   - else slot >= TRANSIENT_BASE → host_read_transient
    //   - else slot < 0 → host_read_state_var
    //   - else → host_read_memory
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

/// Emit the wasm sequence for a `Calc` node. Returns false on
/// unsupported arity/op combinations (caller bails the terminal).
fn emit_calc(
    body: &mut Vec<Instruction<'static>>,
    op: &CalcKind,
    args: &[NodeId],
    local_of: &HashMap<NodeId, u32>,
) -> bool {
    match (op, args.len()) {
        // Pure i32 ops that match Rust's wrapping_{add,sub} exactly.
        (CalcKind::Add, 2) => {
            body.push(Instruction::LocalGet(local_of[&args[0]]));
            body.push(Instruction::LocalGet(local_of[&args[1]]));
            body.push(Instruction::I32Add);
        }
        (CalcKind::Sub, 2) => {
            body.push(Instruction::LocalGet(local_of[&args[0]]));
            body.push(Instruction::LocalGet(local_of[&args[1]]));
            body.push(Instruction::I32Sub);
        }
        // Mul/Div/Mod go through f64 in the walker; round-trip via
        // host import for bit-identity. (Pure-wasm i64 versions can
        // land later if the profile motivates it.)
        (CalcKind::Mul, 2) => emit_calc_binary(body, WasmCalcKind::Mul, args, local_of),
        (CalcKind::Div, 2) => emit_calc_binary(body, WasmCalcKind::Div, args, local_of),
        (CalcKind::Mod, 2) => emit_calc_binary(body, WasmCalcKind::Mod, args, local_of),
        (CalcKind::Pow, 2) => emit_calc_binary(body, WasmCalcKind::Pow, args, local_of),
        (CalcKind::Min, 2) => emit_calc_binary(body, WasmCalcKind::Min2, args, local_of),
        (CalcKind::Max, 2) => emit_calc_binary(body, WasmCalcKind::Max2, args, local_of),
        (CalcKind::Neg, 1) => emit_calc_unary(body, WasmCalcKind::Neg, args, local_of),
        (CalcKind::Abs, 1) => emit_calc_unary(body, WasmCalcKind::Abs, args, local_of),
        (CalcKind::Sign, 1) => emit_calc_unary(body, WasmCalcKind::Sign, args, local_of),
        (CalcKind::Clamp, 3) => {
            // Clamp(lo, v, hi).
            body.push(Instruction::I32Const(WasmCalcKind::Clamp as i32));
            body.push(Instruction::LocalGet(local_of[&args[0]]));
            body.push(Instruction::LocalGet(local_of[&args[1]]));
            body.push(Instruction::LocalGet(local_of[&args[2]]));
            body.push(Instruction::Call(HOST_CALC_TERNARY));
        }
        (CalcKind::Round(strategy), 2) => {
            // Round(v, interval, strategy_id) — strategy is the third
            // arg to the ternary trampoline.
            body.push(Instruction::I32Const(WasmCalcKind::Round as i32));
            body.push(Instruction::LocalGet(local_of[&args[0]]));
            body.push(Instruction::LocalGet(local_of[&args[1]]));
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

fn emit_calc_binary(
    body: &mut Vec<Instruction<'static>>,
    kind: WasmCalcKind,
    args: &[NodeId],
    local_of: &HashMap<NodeId, u32>,
) {
    body.push(Instruction::I32Const(kind as i32));
    body.push(Instruction::LocalGet(local_of[&args[0]]));
    body.push(Instruction::LocalGet(local_of[&args[1]]));
    body.push(Instruction::Call(HOST_CALC_BINARY));
}

fn emit_calc_unary(
    body: &mut Vec<Instruction<'static>>,
    kind: WasmCalcKind,
    args: &[NodeId],
    local_of: &HashMap<NodeId, u32>,
) {
    body.push(Instruction::I32Const(kind as i32));
    body.push(Instruction::LocalGet(local_of[&args[0]]));
    body.push(Instruction::Call(HOST_CALC_UNARY));
}

fn emit_bitfield(
    body: &mut Vec<Instruction<'static>>,
    src_local: u32,
    shift: u8,
    width: Option<u8>,
) {
    // shift >= 32 → 0 (matches inlined runner). Otherwise i32.shr_s
    // by `shift`, then optionally mask to `width` bits.
    if shift >= 32 {
        body.push(Instruction::I32Const(0));
    } else {
        body.push(Instruction::LocalGet(src_local));
        body.push(Instruction::I32Const(shift as i32));
        body.push(Instruction::I32ShrS);
        match width {
            None => {} // pure shift, sign-preserved
            Some(w) if w >= 32 => {} // full-range mask is a no-op
            Some(w) => {
                body.push(Instruction::I32Const(((1i64 << w) - 1) as i32));
                body.push(Instruction::I32And);
            }
        }
    }
}

fn emit_bitwise(
    body: &mut Vec<Instruction<'static>>,
    kind: BitwiseKind,
    args: &[NodeId],
    local_of: &HashMap<NodeId, u32>,
) {
    // 16-bit semantics: mask each operand to low 16 bits, do the op,
    // result is the low 16 bits as i32. Matches `inlined_codegen`'s
    // arm exactly.
    let mask16 = 0xFFFFi32;
    match kind {
        BitwiseKind::Not => {
            // ((!a) & 0xFFFF)
            body.push(Instruction::LocalGet(local_of[&args[0]]));
            body.push(Instruction::I32Const(mask16));
            body.push(Instruction::I32And);
            body.push(Instruction::I32Const(-1));
            body.push(Instruction::I32Xor); // bitwise NOT
            body.push(Instruction::I32Const(mask16));
            body.push(Instruction::I32And);
        }
        BitwiseKind::And | BitwiseKind::Or | BitwiseKind::Xor => {
            body.push(Instruction::LocalGet(local_of[&args[0]]));
            body.push(Instruction::I32Const(mask16));
            body.push(Instruction::I32And);
            body.push(Instruction::LocalGet(local_of[&args[1]]));
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: building a tiny DAG produces a valid wasm module.
    /// Validates the wasm bytes via wasmparser to catch encoder
    /// misuse early (wrong section order, type/index mismatches, etc.).
    #[test]
    fn build_emits_valid_wasm() {
        let mut dag = Dag::default();
        let lit = dag.push(DagNode::Lit(42.0));
        let term = dag.push(DagNode::WriteVar { slot: -1, value: lit });
        dag.terminals.push(term);

        let wasm_dag = WasmDag::build(dag);
        assert_eq!(wasm_dag.coverage(), (1, 1));
        // Validate the bytes parse — wasmparser is pulled in by
        // wasmtime so we can rely on it transitively in tests via
        // wasmtime::Module::validate.
        #[cfg(not(target_arch = "wasm32"))]
        {
            let engine = wasmtime::Engine::default();
            wasmtime::Module::validate(&engine, &wasm_dag.wasm_bytes)
                .expect("emitted wasm should validate");
        }
    }

    #[test]
    fn unsupported_terminal_marked_none() {
        // A terminal whose value is an If — currently unsupported.
        let mut dag = Dag::default();
        let zero = dag.push(DagNode::Lit(0.0));
        let one = dag.push(DagNode::Lit(1.0));
        let cond_load = dag.push(DagNode::LoadVar { slot: -1, kind: TickPosition::Current });
        let if_node = dag.push(DagNode::If {
            branches: vec![(
                crate::dag::types::StyleCondNode::Single { lhs: cond_load, value: one },
                one,
            )],
            fallback: zero,
        });
        let term = dag.push(DagNode::WriteVar { slot: -2, value: if_node });
        dag.terminals.push(term);

        let wasm_dag = WasmDag::build(dag);
        assert_eq!(wasm_dag.coverage(), (1, 0));
    }

    #[test]
    fn arith_chain_emitted() {
        // (1 + 2) * (3 - 4) — fully covered shape.
        let mut dag = Dag::default();
        let one = dag.push(DagNode::Lit(1.0));
        let two = dag.push(DagNode::Lit(2.0));
        let three = dag.push(DagNode::Lit(3.0));
        let four = dag.push(DagNode::Lit(4.0));
        let add = dag.push(DagNode::Calc { op: CalcKind::Add, args: vec![one, two] });
        let sub = dag.push(DagNode::Calc { op: CalcKind::Sub, args: vec![three, four] });
        let mul = dag.push(DagNode::Calc { op: CalcKind::Mul, args: vec![add, sub] });
        let term = dag.push(DagNode::WriteVar { slot: -1, value: mul });
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
