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
/// `host_store_memory(slot: i32, value: i32)`. Records a memory-cell
/// WriteVar result into the host's per-tick `memory_cache` HashMap so
/// later in-tick `LoadVar` of the same slot sees the cascade write.
/// Memory cells live in the host's HashMap until Step 2 moves them
/// into linear memory, at which point this import becomes a single
/// `i32.store`.
pub const HOST_STORE_MEMORY: u32 = 4;
/// User function indices start after the imports.
const N_IMPORTS: u32 = 5;

/// Built-in wasm function-type indices used by imports and slab
/// functions. Per-cabinet `@function` body types start at this index.
const N_BUILTIN_TYPES: u32 = 5;

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

/// One emitted terminal: kept for diagnostics and back-compat with
/// the per-terminal coverage report. With slab-based emission a
/// terminal's `func_idx` is the slab's `func_idx`, and many terminals
/// share the same slab.
#[derive(Debug, Clone)]
pub struct EmittedWasmTerminal {
    pub func_idx: u32,
    pub local_count: u32,
}

/// One slab: a wasm function exporting `slab{N}` that runs a
/// contiguous run of emittable WriteVar terminals end-to-end (in
/// `dag.terminals` order) and writes their results directly into the
/// linear-memory cascade caches. The driver calls one slab per
/// emittable run and walker-fallback for any terminal in between.
#[derive(Debug, Clone)]
pub struct WasmSlab {
    pub func_idx: u32,
    pub local_count: u32,
    /// Indices into `dag.terminals` covered by this slab (in order).
    pub term_indices: Vec<usize>,
}

/// How a single terminal is dispatched at tick time.
#[derive(Debug, Clone, Copy)]
pub enum TermDispatch {
    /// Run by `slabs[i]` (in concert with the rest of that slab's
    /// terminal range; the slab's wasm function performs the value
    /// computation and stores into linear memory).
    Slab(u32),
    /// Walker fallback. The driver evaluates the value via
    /// `dag_eval_node` and stores the result into the host caches
    /// + pokes linear memory.
    Walker,
}

/// Linear-memory layout of the per-cabinet wasm module. Sized at
/// codegen time from the host's `state.state_var_count()`,
/// `dag.transient_slot_count`, and the cabinet's memory-cell address
/// range (Step 2: memory-cell cache lives in linear memory too).
#[derive(Debug, Clone, Copy)]
pub struct WasmMemoryLayout {
    pub n_state_vars: u32,
    pub n_transients: u32,
    /// Number of in-tick memory-cell cache entries reachable by direct
    /// linear-memory load/store. Memory addresses `>= mc_capacity`
    /// still trampoline through host imports — typically the
    /// >= 0x100000 (extended) and >= 0xF0000 (BIOS) regions.
    pub mc_capacity: u32,
    pub sv_value_base: u32,
    pub sv_present_base: u32,
    pub tr_value_base: u32,
    pub tr_present_base: u32,
    pub mc_value_base: u32,
    pub mc_present_base: u32,
    pub mem_bytes: u32,
    pub mem_pages: u32,
}

/// Offset of the i32 holding the current tick's epoch. Every present
/// flag stores the epoch at the time the cell was written; a slab's
/// "present?" check is `present_field == epoch`. Driver bumps the
/// epoch once per tick — no full presence array reset, which would
/// be O(state.memory.len()) per tick.
pub const TICK_EPOCH_OFFSET: u32 = 0;

impl WasmMemoryLayout {
    pub fn new(n_state_vars: u32, n_transients: u32, mc_capacity: u32) -> Self {
        // Reserve the first 16 bytes for tick-scoped scratch
        // (currently just `tick_epoch` at offset 0; padding leaves
        // room for future scratch slots without re-laying-out
        // everything).
        let sv_value_base = 16u32;
        let sv_present_base = sv_value_base + n_state_vars * 4;
        let tr_value_base = sv_present_base + n_state_vars * 4;
        let tr_present_base = tr_value_base + n_transients * 4;
        let mc_value_base = tr_present_base + n_transients * 4;
        let mc_present_base = mc_value_base + mc_capacity * 4;
        let used = mc_present_base + mc_capacity * 4;
        let mem_pages = ((used as usize + 0xFFFF) / 0x10000).max(1) as u32;
        let mem_bytes = mem_pages * 0x10000;
        WasmMemoryLayout {
            n_state_vars,
            n_transients,
            mc_capacity,
            sv_value_base,
            sv_present_base,
            tr_value_base,
            tr_present_base,
            mc_value_base,
            mc_present_base,
            mem_bytes,
            mem_pages,
        }
    }
}

/// A compiled wasm module for one cabinet.
pub struct WasmDag {
    pub dag: Dag,
    /// Per-terminal coverage record, kept for backwards-compat with
    /// `coverage()` and the `probe-v2-wasm` diagnostic. Slab-based
    /// emission means many terminals share the same `func_idx`.
    pub emitted: Vec<Option<EmittedWasmTerminal>>,
    /// Per-terminal dispatch decision used by the driver.
    pub term_dispatch: Vec<TermDispatch>,
    /// One entry per emitted slab (a contiguous run of emittable
    /// WriteVar terminals). Slabs are ordered by appearance in
    /// `dag.terminals`.
    pub slabs: Vec<WasmSlab>,
    pub wasm_bytes: Vec<u8>,
    pub layout: WasmMemoryLayout,
}

impl WasmDag {
    pub fn coverage(&self) -> (usize, usize) {
        let total = self.emitted.len();
        let covered = self.emitted.iter().filter(|x| x.is_some()).count();
        (total, covered)
    }

    /// Number of emitted slabs. The driver calls each slab once per
    /// tick (interleaved with walker fallback for non-emittable
    /// terminals); fewer slabs = fewer wasmtime boundary crossings.
    pub fn slab_count(&self) -> usize {
        self.slabs.len()
    }

    /// Build a wasm module from `dag`. `n_state_vars` is the host's
    /// state-var slot count and `mc_capacity` is the size of the
    /// in-tick memory-cell cache region (typically equal to
    /// `state.memory.len()` so it covers the conventional 1 MB
    /// address range; reads/writes outside this range fall back to
    /// host imports).
    pub fn build(dag: Dag, n_state_vars: u32, mc_capacity: u32) -> Self {
        let n_transients = dag.transient_slot_count as u32;
        let layout = WasmMemoryLayout::new(n_state_vars, n_transients, mc_capacity);
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

        // Phase 2: emit slabs covering contiguous runs of emittable
        // WriteVar terminals. The driver dispatches each terminal to
        // either a slab call or walker fallback, in `dag.terminals`
        // order — boundaries fall wherever a non-emittable terminal
        // (or an `IndirectStore`, or an over-budget cone) appears.
        //
        // Inside one slab, `local_of` caches let two terminals share a
        // sub-DAG (Lit/LoadVar/Calc) for free, and inter-terminal
        // cascade reads see the prior write through the linear-memory
        // cache regions populated by `emit_store_to_linmem`. Across
        // slab boundaries `local_of` is reset because each slab is its
        // own wasm function with its own local frame.
        let mut emitted: Vec<Option<EmittedWasmTerminal>> = vec![None; dag.terminals.len()];
        let mut term_dispatch: Vec<TermDispatch> = vec![TermDispatch::Walker; dag.terminals.len()];
        let mut slabs: Vec<WasmSlab> = Vec::new();

        let mut cur_body: Vec<Instruction<'static>> = Vec::new();
        let mut cur_locals: u32 = 0;
        let mut cur_local_of: HashMap<NodeId, u32> = HashMap::new();
        let mut cur_terms: Vec<usize> = Vec::new();

        for (term_idx, &term_id) in dag.terminals.iter().enumerate() {
            let DagNode::WriteVar { slot, value } = &dag.nodes[term_id as usize] else {
                // IndirectStore (broadcast). Always walker for now;
                // closes any pending slab so subsequent emittable
                // terminals see the broadcast's effects (broadcasts
                // mutate state mid-tick).
                close_slab(
                    &mut cur_body,
                    &mut cur_locals,
                    &mut cur_local_of,
                    &mut cur_terms,
                    &mut builder,
                    &mut slabs,
                    &mut term_dispatch,
                    &mut emitted,
                );
                continue;
            };
            let slot = *slot;
            let value_root = *value;

            // Snapshot so we can roll back a partial emit on failure.
            let body_save = cur_body.len();
            let locals_save = cur_locals;
            // Per-tick: we accept the cost of cloning local_of at
            // boundaries (small, ≤ a few hundred entries).
            let local_of_save = cur_local_of.clone();

            let mut ok;
            {
                let mut ctx = EmitCtx {
                    local_count: &mut cur_locals,
                    local_of: &mut cur_local_of,
                    fn_wasm_idx: &fn_wasm_idx,
                    layout: &layout,
                };
                ok = emit_value(&dag, value_root, &mut cur_body, &mut ctx, false);
                if ok {
                    ok = emit_store_to_linmem(slot, &mut cur_body, &mut ctx);
                }
            }

            if !ok {
                cur_body.truncate(body_save);
                cur_locals = locals_save;
                cur_local_of = local_of_save;
                // Walker for this terminal: close pending slab so the
                // walker's mutation happens before any later in-tick
                // reads.
                close_slab(
                    &mut cur_body,
                    &mut cur_locals,
                    &mut cur_local_of,
                    &mut cur_terms,
                    &mut builder,
                    &mut slabs,
                    &mut term_dispatch,
                    &mut emitted,
                );
                continue;
            }

            cur_terms.push(term_idx);

            // Close the slab if it has grown past the per-function
            // engine limit, so the next terminal starts a fresh
            // function. This puts a clean upper bound on per-slab
            // cost regardless of cabinet size.
            if cur_body.len() >= INSTRUCTION_LIMIT || cur_locals >= LOCAL_LIMIT {
                close_slab(
                    &mut cur_body,
                    &mut cur_locals,
                    &mut cur_local_of,
                    &mut cur_terms,
                    &mut builder,
                    &mut slabs,
                    &mut term_dispatch,
                    &mut emitted,
                );
            }
        }
        close_slab(
            &mut cur_body,
            &mut cur_locals,
            &mut cur_local_of,
            &mut cur_terms,
            &mut builder,
            &mut slabs,
            &mut term_dispatch,
            &mut emitted,
        );

        let wasm_bytes = builder.finish();
        WasmDag {
            dag,
            emitted,
            term_dispatch,
            slabs,
            wasm_bytes,
            layout,
        }
    }
}

/// Close the current open slab — emit the accumulated body as a wasm
/// function, record the slab metadata, mark covered terminals as
/// `TermDispatch::Slab`, and clear the per-slab scratch buffers ready
/// for the next slab. No-op when the open slab has no terminals.
fn close_slab(
    body: &mut Vec<Instruction<'static>>,
    locals: &mut u32,
    local_of: &mut HashMap<NodeId, u32>,
    terms: &mut Vec<usize>,
    builder: &mut ModuleBuilder,
    slabs: &mut Vec<WasmSlab>,
    term_dispatch: &mut [TermDispatch],
    emitted: &mut [Option<EmittedWasmTerminal>],
) {
    if terms.is_empty() {
        body.clear();
        *locals = 0;
        local_of.clear();
        return;
    }
    let body_taken = std::mem::take(body);
    let func_idx = builder.add_slab(*locals, body_taken);
    let slab = WasmSlab {
        func_idx,
        local_count: *locals,
        term_indices: std::mem::take(terms),
    };
    let slab_idx = slabs.len();
    for &ti in &slab.term_indices {
        term_dispatch[ti] = TermDispatch::Slab(slab_idx as u32);
        emitted[ti] = Some(EmittedWasmTerminal {
            func_idx,
            local_count: *locals,
        });
    }
    slabs.push(slab);
    *locals = 0;
    local_of.clear();
}

/// Append store-to-linear-memory ops for a `WriteVar`'s result.
///
/// On entry the current expression value is on the wasm stack. After
/// this function returns the stack is empty: state-var, transient,
/// and memory-cell writes go into the slab's linear-memory cache
/// regions. Memory-cell writes outside `mc_capacity` (rare —
/// >= 0xF0000 BIOS / extended) trampoline through `host_store_memory`.
fn emit_store_to_linmem(
    slot: i32,
    body: &mut Vec<Instruction<'static>>,
    ctx: &mut EmitCtx<'_>,
) -> bool {
    let layout = ctx.layout;
    if slot < 0 {
        let idx = (-slot - 1) as i64;
        if idx < 0 || idx as u32 >= layout.n_state_vars {
            // Bail to walker — slot index out of materialised range.
            return false;
        }
        let value_addr = layout.sv_value_base + (idx as u32) * 4;
        let present_addr = layout.sv_present_base + (idx as u32) * 4;
        emit_store_value_and_epoch(body, ctx, value_addr, present_addr)
    } else if slot >= crate::dag::TRANSIENT_BASE {
        let idx = (slot - crate::dag::TRANSIENT_BASE) as i64;
        if idx < 0 || idx as u32 >= layout.n_transients {
            return false;
        }
        let value_addr = layout.tr_value_base + (idx as u32) * 4;
        let present_addr = layout.tr_present_base + (idx as u32) * 4;
        emit_store_value_and_epoch(body, ctx, value_addr, present_addr)
    } else {
        // Memory-cell write. Trampolines through `host_store_memory`
        // which inserts into the host's `memory_cache` HashMap so
        // (a) subsequent same-tick `LoadVar` of this address (via
        // host_read_memory) sees the cascade write and (b) Phase 2
        // writeback flushes it to `state.memory`.
        //
        // Lifting this into a pure linear-memory store would require
        // also handling the read side via linear memory (reads
        // dominate writes ~5:1, so a presence-flag check on every
        // read costs more than the write-side host crossing saves —
        // measured: -19 % on doom8088). Step 2 keeps memory-cell
        // writes on the host import; the win is the state-var /
        // transient cache + epoch-based reset.
        let _ = layout;
        let Some(tmp) = ctx.alloc_local() else { return false; };
        body.push(Instruction::LocalSet(tmp));
        body.push(Instruction::I32Const(slot));
        body.push(Instruction::LocalGet(tmp));
        body.push(Instruction::Call(HOST_STORE_MEMORY));
        true
    }
}

/// Pop the value off the stack and emit two stores: value into
/// `value_addr`, current `tick_epoch` into `present_addr`. The
/// epoch-as-present trick avoids the need for a per-tick presence
/// reset (which on a 1 MB memory-cell region would be O(MB) of writes
/// per tick).
fn emit_store_value_and_epoch(
    body: &mut Vec<Instruction<'static>>,
    ctx: &mut EmitCtx<'_>,
    value_addr: u32,
    present_addr: u32,
) -> bool {
    let Some(tmp) = ctx.alloc_local() else { return false; };
    body.push(Instruction::LocalSet(tmp));
    body.push(Instruction::I32Const(value_addr as i32));
    body.push(Instruction::LocalGet(tmp));
    body.push(Instruction::I32Store(memarg_at(0)));
    body.push(Instruction::I32Const(present_addr as i32));
    emit_load_epoch(body);
    body.push(Instruction::I32Store(memarg_at(0)));
    true
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
    type_idx_slab: u32,
    fn_type_cache: HashMap<u32, u32>,
    next_func_idx: u32,
}

impl ModuleBuilder {
    fn new(layout: WasmMemoryLayout) -> Self {
        let mut types = TypeSection::new();
        // 0: slab function — `() -> ()`, no parameters and no result.
        // Each slab writes its terminals' results directly into linear
        // memory; the host side never receives a return value from a
        // slab call.
        types.ty().function([], []);
        let type_idx_slab = 0;
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
        // 4: `host_store_memory(slot, value) -> ()`. Two i32 args, no
        // result.
        types.ty().function([ValType::I32, ValType::I32], []);
        let type_idx_store_memory = 4;

        let mut imports = ImportSection::new();
        imports.import("host", "read_memory", EntityType::Function(type_idx_read_slot));
        imports.import("host", "read_prev", EntityType::Function(type_idx_read_slot));
        imports.import("host", "calc_binary", EntityType::Function(type_idx_calc_binary));
        imports.import("host", "calc_ternary", EntityType::Function(type_idx_calc_ternary));
        imports.import("host", "store_memory", EntityType::Function(type_idx_store_memory));

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
            type_idx_slab,
            fn_type_cache: HashMap::new(),
            next_func_idx: N_IMPORTS,
        }
    }

    fn add_function_type(&mut self, n_params: u32) -> u32 {
        if let Some(&idx) = self.fn_type_cache.get(&n_params) {
            return idx;
        }
        let params: Vec<ValType> = (0..n_params).map(|_| ValType::I32).collect();
        // Built-in types occupy 0..N_BUILTIN_TYPES; user `@function`
        // types come after.
        let new_idx = N_BUILTIN_TYPES + self.fn_type_cache.len() as u32;
        self.types.ty().function(params, [ValType::I32]);
        self.fn_type_cache.insert(n_params, new_idx);
        new_idx
    }

    fn add_slab(&mut self, n_locals: u32, body: Vec<Instruction<'static>>) -> u32 {
        let func_idx = self.next_func_idx;
        self.next_func_idx += 1;
        self.functions.function(self.type_idx_slab);
        let mut func = Function::new([(n_locals, ValType::I32)]);
        for ins in body {
            func.instruction(&ins);
        }
        func.instruction(&Instruction::End);
        self.exports
            .export(&format!("slab{}", func_idx), ExportKind::Func, func_idx);
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

/// Wasm engines (wasmtime, V8) enforce a per-function local limit
/// well below the spec's 2^32 — instantiation fails with
/// "too many locals" above ~50,000. A complex cabinet's `WriteVar`
/// cone can blow this cap on real DOS cabinets. Cap below the
/// engine limit and bail to walker fallback when allocation fails.
const LOCAL_LIMIT: u32 = 30_000;

/// Wasm engines also cap per-function code size. Wasmtime's default
/// "Code for function is too large" threshold lands around 0.5–1 M
/// instructions on most cabinets. Bail well below to leave headroom
/// for instruction encoding (Switch/If cascades inflate ~5–10× on
/// the byte side) and to keep emit time bounded on pathological
/// terminal cones (Doom8088 has cones with millions of nodes).
const INSTRUCTION_LIMIT: usize = 200_000;

struct EmitCtx<'a> {
    local_count: &'a mut u32,
    local_of: &'a mut HashMap<NodeId, u32>,
    fn_wasm_idx: &'a [Option<u32>],
    layout: &'a WasmMemoryLayout,
}

impl<'a> EmitCtx<'a> {
    /// Returns the new local index, or `None` once we'd cross the
    /// engine's per-function local limit. Callers propagate `None`
    /// as `false` (walker fallback) up the emit stack.
    fn alloc_local(&mut self) -> Option<u32> {
        if *self.local_count >= LOCAL_LIMIT {
            return None;
        }
        let id = *self.local_count;
        *self.local_count += 1;
        Some(id)
    }
}

// ---------- emit_value -----------------------------------------------

fn emit_value(
    dag: &Dag,
    node_id: NodeId,
    body: &mut Vec<Instruction<'static>>,
    ctx: &mut EmitCtx<'_>,
    inside_branch: bool,
) -> bool {
    // Bail early if this function's body has grown past what the wasm
    // engine will accept. Cones with millions of nodes (Doom8088) blow
    // wasmtime's "Code for function is too large" otherwise.
    if body.len() > INSTRUCTION_LIMIT {
        return false;
    }
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
            let Some(key_local) = ctx.alloc_local() else { return false; };
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
            // Subsequent conditions are emitted INSIDE the `else` block
            // of the previous branch, so they're structurally inside a
            // branch even though `inside_branch` (the parameter to this
            // call) might be false. Force `inside_branch=true` for every
            // cond — caching a local inside an else-only path means a
            // later read returns 0 if any earlier branch was taken
            // (locals default to 0 in wasm). The first cond is at the
            // structural top of the If; emitting it with `true` is
            // harmless (just suppresses caching for cond[0]'s LoadVar/
            // Calc nodes).
            for (cond, branch) in branches {
                if !emit_style_cond(dag, cond, body, ctx, true) {
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
                if let Some(local) = ctx.alloc_local() {
                    body.push(Instruction::LocalTee(local));
                    ctx.local_of.insert(node_id, local);
                } else {
                    return false;
                }
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
        // present == epoch ? value : 0 (transients with no cache hit
        // return 0 — matches inlined runner's `unwrap_or(0)`).
        body.push(Instruction::I32Const(value_addr as i32));
        body.push(Instruction::I32Load(memarg_at(0)));
        body.push(Instruction::I32Const(0));
        emit_present_eq_epoch(body, present_addr);
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
        // if present == epoch then value else host_read_memory(slot).
        emit_present_eq_epoch(body, present_addr);
        body.push(Instruction::If(BlockType::Result(ValType::I32)));
        body.push(Instruction::I32Const(value_addr as i32));
        body.push(Instruction::I32Load(memarg_at(0)));
        body.push(Instruction::Else);
        body.push(Instruction::I32Const(slot));
        body.push(Instruction::Call(HOST_READ_MEMORY));
        body.push(Instruction::End);
        return;
    }
    // Memory-cell read (slot >= 0, < TRANSIENT_BASE). Trampolines
    // through `host_read_memory`, which consults the host's
    // `memory_cache` HashMap (in-tick cascade writes — populated
    // both by walker fallback and by the slab-after sync pass for
    // in-range memory writes) and then `state.read_mem` (packed-cell
    // aware). Lifting the cold-read base into linear memory requires
    // packed-cell-aware codegen and is out of scope for this step.
    let _ = layout;
    body.push(Instruction::I32Const(slot));
    body.push(Instruction::Call(HOST_READ_MEMORY));
}

/// Push `(present[present_addr] == tick_epoch)` onto the stack.
fn emit_present_eq_epoch(body: &mut Vec<Instruction<'static>>, present_addr: u32) {
    body.push(Instruction::I32Const(present_addr as i32));
    body.push(Instruction::I32Load(memarg_at(0)));
    body.push(Instruction::I32Const(TICK_EPOCH_OFFSET as i32));
    body.push(Instruction::I32Load(memarg_at(0)));
    body.push(Instruction::I32Eq);
}

/// Push the current tick epoch onto the stack.
fn emit_load_epoch(body: &mut Vec<Instruction<'static>>) {
    body.push(Instruction::I32Const(TICK_EPOCH_OFFSET as i32));
    body.push(Instruction::I32Load(memarg_at(0)));
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
            // Pow keeps the host trampoline. The dominant cabinet
            // shape is `pow(2, n)` for small non-negative `n`, which
            // could lower to a runtime loop; an integer-multiply
            // loop wraps on overflow rather than saturating to
            // i32::MAX like the walker's `(a as f64).powf(b as f64)
            // as i32`, and an f64-multiply loop needs an f64 local
            // (the EmitCtx currently only allocates i32 locals).
            // Until either a saturating integer pow or a
            // multi-type-locals pass lands, the host call is
            // correct and the only Pow path.
            body.push(Instruction::I32Const(WasmCalcKind::Pow as i32));
            if !emit_value(dag, args[0], body, ctx, inside_branch) { return false; }
            if !emit_value(dag, args[1], body, ctx, inside_branch) { return false; }
            body.push(Instruction::Call(HOST_CALC_BINARY));
        }
        (CalcKind::Round(strategy), 2) => {
            return emit_round(dag, args, body, ctx, inside_branch, *strategy);
        }
        _ => return false,
    }
    true
}

/// Pure-wasm `Round(strategy)(value, interval)`. Walker semantics:
/// `(value/interval).round_via_strategy() * interval` cast back to
/// i32. `interval == 0` → return value verbatim.
///
/// `f64.nearest` implements IEEE round-half-to-even, matching the
/// walker's hand-rolled banker's rounding for the `Nearest` case.
fn emit_round(
    dag: &Dag,
    args: &[NodeId],
    body: &mut Vec<Instruction<'static>>,
    ctx: &mut EmitCtx<'_>,
    inside_branch: bool,
    strategy: crate::types::RoundStrategy,
) -> bool {
    let Some(v_local) = ctx.alloc_local() else { return false; };
    let Some(interval_local) = ctx.alloc_local() else { return false; };
    if !emit_value(dag, args[0], body, ctx, inside_branch) { return false; }
    body.push(Instruction::LocalSet(v_local));
    if !emit_value(dag, args[1], body, ctx, inside_branch) { return false; }
    body.push(Instruction::LocalSet(interval_local));
    body.push(Instruction::LocalGet(interval_local));
    body.push(Instruction::I32Eqz);
    body.push(Instruction::If(BlockType::Result(ValType::I32)));
    body.push(Instruction::LocalGet(v_local));
    body.push(Instruction::Else);
    // q = v / interval  (in f64)
    body.push(Instruction::LocalGet(v_local));
    body.push(Instruction::F64ConvertI32S);
    body.push(Instruction::LocalGet(interval_local));
    body.push(Instruction::F64ConvertI32S);
    body.push(Instruction::F64Div);
    // round q
    let round_op = match strategy {
        crate::types::RoundStrategy::Nearest => Instruction::F64Nearest,
        crate::types::RoundStrategy::Up => Instruction::F64Ceil,
        crate::types::RoundStrategy::Down => Instruction::F64Floor,
        crate::types::RoundStrategy::ToZero => Instruction::F64Trunc,
    };
    body.push(round_op);
    // * interval
    body.push(Instruction::LocalGet(interval_local));
    body.push(Instruction::F64ConvertI32S);
    body.push(Instruction::F64Mul);
    // back to i32 (saturating). Walker uses `as i32` which is
    // saturate-to-i32 in Rust release builds; `i32.trunc_sat_f64_s`
    // implements the same.
    body.push(Instruction::I32TruncSatF64S);
    body.push(Instruction::End);
    true
}


fn emit_div(
    dag: &Dag,
    args: &[NodeId],
    body: &mut Vec<Instruction<'static>>,
    ctx: &mut EmitCtx<'_>,
    inside_branch: bool,
) -> bool {
    let Some(n_local) = ctx.alloc_local() else { return false; };
    let Some(d_local) = ctx.alloc_local() else { return false; };
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
    // CSS `mod()` is floor-mod (sign of divisor), matching the walker's
    // `av - (av / bv).floor() * bv`. wasm `i32.rem_s` is trunc-mod
    // (sign of dividend); apply the standard floor correction:
    // if r != 0 and r and d have opposite signs, r += d.
    let Some(n_local) = ctx.alloc_local() else { return false; };
    let Some(d_local) = ctx.alloc_local() else { return false; };
    let Some(r_local) = ctx.alloc_local() else { return false; };
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
    // r = n %_s d  (trunc-mod), keep on stack and tee into r_local.
    body.push(Instruction::LocalGet(n_local));
    body.push(Instruction::LocalGet(d_local));
    body.push(Instruction::I32RemS);
    body.push(Instruction::LocalTee(r_local));
    // floor correction: r==0 → 0; else if (r XOR d) < 0 → r+d; else r.
    body.push(Instruction::I32Eqz);
    body.push(Instruction::If(BlockType::Result(ValType::I32)));
    body.push(Instruction::I32Const(0));
    body.push(Instruction::Else);
    body.push(Instruction::LocalGet(r_local));
    body.push(Instruction::LocalGet(d_local));
    body.push(Instruction::I32Xor);
    body.push(Instruction::I32Const(0));
    body.push(Instruction::I32LtS);
    body.push(Instruction::If(BlockType::Result(ValType::I32)));
    body.push(Instruction::LocalGet(r_local));
    body.push(Instruction::LocalGet(d_local));
    body.push(Instruction::I32Add);
    body.push(Instruction::Else);
    body.push(Instruction::LocalGet(r_local));
    body.push(Instruction::End);
    body.push(Instruction::End);
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
    let Some(a_local) = ctx.alloc_local() else { return false; };
    let Some(b_local) = ctx.alloc_local() else { return false; };
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
    let Some(a_local) = ctx.alloc_local() else { return false; };
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
    let Some(a_local) = ctx.alloc_local() else { return false; };
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
    let Some(lo_local) = ctx.alloc_local() else { return false; };
    let Some(v_local) = ctx.alloc_local() else { return false; };
    let Some(hi_local) = ctx.alloc_local() else { return false; };
    let Some(max_local) = ctx.alloc_local() else { return false; };
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

        let wasm_dag = WasmDag::build(dag, 4, 256);
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

        let wasm_dag = WasmDag::build(dag, 4, 256);
        assert_eq!(wasm_dag.coverage(), (1, 1));
        #[cfg(not(target_arch = "wasm32"))]
        {
            let engine = wasmtime::Engine::default();
            wasmtime::Module::validate(&engine, &wasm_dag.wasm_bytes).expect("valid wasm");
        }
    }

    #[test]
    fn memory_layout_sizes_correctly() {
        let layout = WasmMemoryLayout::new(100, 50, 0);
        assert_eq!(layout.sv_value_base, 16);
        assert_eq!(layout.sv_present_base, 16 + 400);
        assert_eq!(layout.tr_value_base, 16 + 800);
        assert_eq!(layout.tr_present_base, 16 + 1000);
        assert_eq!(layout.mc_value_base, 16 + 1200);
        assert_eq!(layout.mc_present_base, 16 + 1200);
        assert_eq!(layout.mem_pages, 1);
        assert_eq!(layout.mem_bytes, 65536);
    }

    #[test]
    fn memory_layout_includes_mem_cache() {
        let layout = WasmMemoryLayout::new(10, 5, 1024);
        assert_eq!(layout.mc_value_base, 16 + 4 * (10 + 10 + 5 + 5));
        assert_eq!(layout.mc_present_base, layout.mc_value_base + 1024 * 4);
    }
}
