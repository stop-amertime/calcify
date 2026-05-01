//! Native host runtime for executing the wasm module produced by
//! `wasm_codegen`. Wraps wasmtime and provides:
//!
//! - One-shot module instantiation (build wasmtime `Module` and
//!   `Instance` from the bytes produced by `WasmDag::build`).
//! - Per-tick state binding: the host writes the per-tick state-var
//!   and transient caches into the wasm module's exported linear
//!   memory before each tick, then calls each terminal's exported
//!   function.
//! - Calling one terminal function and returning its `i32` result.
//!
//! ## Native-only
//!
//! Gated `cfg(not(target_arch = "wasm32"))` from `dag::mod`. The
//! browser runs the wasm bytes via the JS engine — that wiring lives
//! in `calcite-wasm` and is a follow-up. Without it,
//! `Backend::DagV2Wasm` is a native-only backend used for
//! differentials and benches.
//!
//! ## Lifetime trick (memory-cache import only)
//!
//! State-var and transient reads now go through wasm linear memory,
//! so the only host import that needs raw-pointer access to the host
//! state is `host_read_memory` (sparse HashMap cache → state.read_mem
//! fallback) and `host_read_prev`. The `WasmHostCtx` carries those
//! two pointers; they're set at the top of each tick and cleared
//! after.

use std::collections::HashMap;
use std::sync::Mutex;

use wasmtime::{Caller, Engine, Instance, Linker, Memory, Module, Store, TypedFunc};

use crate::dag::wasm_codegen::{
    WasmCalcKind, WasmMemoryLayout, ROUND_DOWN, ROUND_NEAREST, ROUND_TO_ZERO, ROUND_UP,
};
use crate::state::State;
use crate::types::RoundStrategy;

/// Per-tick host context. Set immediately before calling into wasm
/// and cleared on return. Pointers are raw because the lifetime they
/// have to span (one wasm call) is one we guarantee by construction.
pub struct WasmHostCtx {
    pub state: *const State,
    pub memory_cache: *const HashMap<i32, i32>,
}

impl Default for WasmHostCtx {
    fn default() -> Self {
        WasmHostCtx { state: std::ptr::null(), memory_cache: std::ptr::null() }
    }
}

unsafe impl Send for WasmHostCtx {}

/// Compiled + instantiated wasm module ready to run terminal calls.
pub struct WasmHost {
    #[allow(dead_code)]
    engine: Engine,
    inner: Mutex<WasmHostInner>,
    layout: WasmMemoryLayout,
}

struct WasmHostInner {
    store: Store<WasmHostCtx>,
    typed_fns: Vec<Option<TypedFunc<(), i32>>>,
    /// Exported wasm linear memory, used to materialise state-var
    /// and transient caches before each tick.
    memory: Memory,
}

impl WasmHost {
    pub fn instantiate(
        wasm_bytes: &[u8],
        emitted: &[Option<crate::dag::wasm_codegen::EmittedWasmTerminal>],
        layout: WasmMemoryLayout,
    ) -> Result<Self, wasmtime::Error> {
        let engine = Engine::default();
        let module = Module::new(&engine, wasm_bytes)?;

        let mut linker: Linker<WasmHostCtx> = Linker::new(&engine);
        register_imports(&mut linker)?;

        let mut store: Store<WasmHostCtx> = Store::new(&engine, WasmHostCtx::default());
        let instance = linker.instantiate(&mut store, &module)?;

        let memory = instance
            .get_memory(&mut store, "mem")
            .ok_or_else(|| wasmtime::Error::msg("emitted module is missing 'mem' export"))?;

        let mut typed_fns: Vec<Option<TypedFunc<(), i32>>> = Vec::with_capacity(emitted.len());
        for slot in emitted {
            match slot {
                Some(t) => {
                    let name = format!("t{}", t.func_idx);
                    let f = instance.get_typed_func::<(), i32>(&mut store, &name)?;
                    typed_fns.push(Some(f));
                }
                None => typed_fns.push(None),
            }
        }

        Ok(WasmHost {
            engine,
            inner: Mutex::new(WasmHostInner { store, typed_fns, memory }),
            layout,
        })
    }

    pub fn typed_fn_count(&self) -> usize {
        let g = self.inner.lock().expect("WasmHost poisoned");
        g.typed_fns.iter().filter(|x| x.is_some()).count()
    }

    /// Reset the per-tick presence regions to all-zero. Call once at
    /// the top of each tick; thereafter use `poke_state_var` and
    /// `poke_transient` to record cache hits incrementally as the
    /// driver computes WriteVar values.
    ///
    /// Bulk-zeroing on every terminal call (the obvious "synchronise
    /// the wasm memory with the cache" approach) costs O(n_sv) per
    /// terminal × n_terminals per tick — a significant fraction of
    /// the wasm savings on cabinets with hundreds of slots and
    /// hundreds of terminals. Doing one bulk reset + per-update
    /// pokes makes it O(n_sv) once + O(n_terminals) updates total.
    pub fn reset_tick(&self) {
        let mut guard = self.inner.lock().expect("WasmHost poisoned");
        let WasmHostInner { store, memory, .. } = &mut *guard;
        let mem_data: &mut [u8] = memory.data_mut(&mut *store);
        let layout = self.layout;
        let sv_pres = layout.sv_present_base as usize;
        let tr_pres = layout.tr_present_base as usize;
        let sv_pres_len = (layout.n_state_vars as usize) * 4;
        let tr_pres_len = (layout.n_transients as usize) * 4;
        for b in &mut mem_data[sv_pres..sv_pres + sv_pres_len] {
            *b = 0;
        }
        for b in &mut mem_data[tr_pres..tr_pres + tr_pres_len] {
            *b = 0;
        }
    }

    /// Record a state-var cache hit at index `i` with value `v`.
    /// Cheap: two i32 writes.
    pub fn poke_state_var(&self, i: usize, v: i32) {
        let mut guard = self.inner.lock().expect("WasmHost poisoned");
        let WasmHostInner { store, memory, .. } = &mut *guard;
        let mem_data: &mut [u8] = memory.data_mut(&mut *store);
        let layout = self.layout;
        if i >= layout.n_state_vars as usize {
            return;
        }
        let voff = layout.sv_value_base as usize + i * 4;
        let poff = layout.sv_present_base as usize + i * 4;
        mem_data[voff..voff + 4].copy_from_slice(&v.to_le_bytes());
        mem_data[poff..poff + 4].copy_from_slice(&1i32.to_le_bytes());
    }

    /// Record a transient cache hit at index `i` with value `v`.
    pub fn poke_transient(&self, i: usize, v: i32) {
        let mut guard = self.inner.lock().expect("WasmHost poisoned");
        let WasmHostInner { store, memory, .. } = &mut *guard;
        let mem_data: &mut [u8] = memory.data_mut(&mut *store);
        let layout = self.layout;
        if i >= layout.n_transients as usize {
            return;
        }
        let voff = layout.tr_value_base as usize + i * 4;
        let poff = layout.tr_present_base as usize + i * 4;
        mem_data[voff..voff + 4].copy_from_slice(&v.to_le_bytes());
        mem_data[poff..poff + 4].copy_from_slice(&1i32.to_le_bytes());
    }

    /// Run terminal `i` against the host state. Memory-cache reads
    /// route through `host_read_memory`; the binding is set here and
    /// cleared on return.
    ///
    /// SAFETY: caller guarantees `state` and `memory_cache` outlive
    /// the call.
    pub fn run_terminal(
        &self,
        i: usize,
        state: &State,
        memory_cache: &HashMap<i32, i32>,
    ) -> i32 {
        let mut guard = self.inner.lock().expect("WasmHost poisoned");
        let WasmHostInner { store, typed_fns, .. } = &mut *guard;
        {
            let ctx = store.data_mut();
            ctx.state = state as *const State;
            ctx.memory_cache = memory_cache as *const _;
        }
        let f = typed_fns[i]
            .as_ref()
            .expect("run_terminal called with non-emitted terminal");
        let result = f.call(&mut *store, ()).expect("wasm terminal call failed");
        {
            let ctx = store.data_mut();
            *ctx = WasmHostCtx::default();
        }
        result
    }

    /// After a tick, write back any state-var values that may have
    /// changed via `host_read_memory` (none, in current design — the
    /// wasm path doesn't write state directly). Reserved for future
    /// in-wasm writeback.
    #[allow(dead_code)]
    pub fn finalize_tick(&self) {}
}

fn register_imports(linker: &mut Linker<WasmHostCtx>) -> Result<(), wasmtime::Error> {
    linker.func_wrap("host", "read_memory", host_read_memory)?;
    linker.func_wrap("host", "read_prev", host_read_prev)?;
    linker.func_wrap("host", "calc_binary", host_calc_binary)?;
    linker.func_wrap("host", "calc_ternary", host_calc_ternary)?;
    Ok(())
}

// ----- imports --------------------------------------------------------

fn host_read_memory(caller: Caller<'_, WasmHostCtx>, slot: i32) -> i32 {
    let ctx = caller.data();
    // SAFETY: pointers set by run_terminal, cleared after.
    unsafe {
        let cache = &*ctx.memory_cache;
        if let Some(&v) = cache.get(&slot) {
            return v;
        }
        let state = &*ctx.state;
        state.read_mem(slot)
    }
}

fn host_read_prev(caller: Caller<'_, WasmHostCtx>, slot: i32) -> i32 {
    let ctx = caller.data();
    unsafe {
        let state = &*ctx.state;
        state.read_mem(slot)
    }
}

// ----- arithmetic trampolines ----------------------------------------

fn host_calc_binary(_caller: Caller<'_, WasmHostCtx>, kind: i32, a: i32, b: i32) -> i32 {
    let Some(k) = WasmCalcKind::from_i32(kind) else { return 0; };
    match k {
        WasmCalcKind::Pow => (a as f64).powf(b as f64) as i32,
        WasmCalcKind::Round => 0, // unused via this entry point
    }
}

fn host_calc_ternary(
    _caller: Caller<'_, WasmHostCtx>,
    kind: i32,
    a: i32,
    b: i32,
    c: i32,
) -> i32 {
    let Some(k) = WasmCalcKind::from_i32(kind) else { return 0; };
    match k {
        WasmCalcKind::Round => {
            let v = a as f64;
            let interval = b as f64;
            if interval == 0.0 {
                return v as i32;
            }
            let strategy = match c {
                ROUND_NEAREST => RoundStrategy::Nearest,
                ROUND_UP => RoundStrategy::Up,
                ROUND_DOWN => RoundStrategy::Down,
                ROUND_TO_ZERO => RoundStrategy::ToZero,
                _ => RoundStrategy::Nearest,
            };
            let q = v / interval;
            let rounded = match strategy {
                RoundStrategy::Nearest => {
                    let f = q.floor();
                    let frac = q - f;
                    if frac < 0.5 {
                        f
                    } else if frac > 0.5 {
                        f + 1.0
                    } else if (f as i64) % 2 == 0 {
                        f
                    } else {
                        f + 1.0
                    }
                }
                RoundStrategy::Up => q.ceil(),
                RoundStrategy::Down => q.floor(),
                RoundStrategy::ToZero => q.trunc(),
            };
            (rounded * interval) as i32
        }
        WasmCalcKind::Pow => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::types::{CalcKind, Dag, DagNode};
    use crate::dag::WasmDag;

    fn empty_state() -> State {
        State::default()
    }

    #[test]
    fn instantiate_lit_terminal() {
        let mut dag = Dag::default();
        let lit = dag.push(DagNode::Lit(42.0));
        let term = dag.push(DagNode::WriteVar { slot: -1, value: lit });
        dag.terminals.push(term);

        let wasm_dag = WasmDag::build(dag, 4);
        let host = WasmHost::instantiate(&wasm_dag.wasm_bytes, &wasm_dag.emitted, wasm_dag.layout)
            .expect("instantiate");
        assert_eq!(host.typed_fn_count(), 1);

        let state = empty_state();
        let svc: Vec<Option<i32>> = vec![None; 4];
        let mc: HashMap<i32, i32> = HashMap::new();
        let tc: Vec<Option<i32>> = Vec::new();
        host.reset_tick(); for (i, opt) in svc.iter().enumerate() { if let Some(v) = *opt { host.poke_state_var(i, v); } } for (i, opt) in tc.iter().enumerate() { if let Some(v) = *opt { host.poke_transient(i, v); } }
        let v = host.run_terminal(0, &state, &mc);
        assert_eq!(v, 42);
    }

    #[test]
    fn arith_chain_wasm_runs() {
        let mut dag = Dag::default();
        let three = dag.push(DagNode::Lit(3.0));
        let four = dag.push(DagNode::Lit(4.0));
        let one = dag.push(DagNode::Lit(1.0));
        let add = dag.push(DagNode::Calc { op: CalcKind::Add, args: vec![three, four] });
        let sub = dag.push(DagNode::Calc { op: CalcKind::Sub, args: vec![add, one] });
        let term = dag.push(DagNode::WriteVar { slot: -1, value: sub });
        dag.terminals.push(term);

        let wasm_dag = WasmDag::build(dag, 4);
        let host = WasmHost::instantiate(&wasm_dag.wasm_bytes, &wasm_dag.emitted, wasm_dag.layout)
            .expect("instantiate");

        let state = empty_state();
        let svc: Vec<Option<i32>> = vec![None; 4];
        let mc: HashMap<i32, i32> = HashMap::new();
        let tc: Vec<Option<i32>> = Vec::new();
        host.reset_tick(); for (i, opt) in svc.iter().enumerate() { if let Some(v) = *opt { host.poke_state_var(i, v); } } for (i, opt) in tc.iter().enumerate() { if let Some(v) = *opt { host.poke_transient(i, v); } }
        let v = host.run_terminal(0, &state, &mc);
        assert_eq!(v, 6);
    }

    #[test]
    fn mul_via_pure_wasm() {
        let mut dag = Dag::default();
        let six = dag.push(DagNode::Lit(6.0));
        let seven = dag.push(DagNode::Lit(7.0));
        let mul = dag.push(DagNode::Calc { op: CalcKind::Mul, args: vec![six, seven] });
        let term = dag.push(DagNode::WriteVar { slot: -1, value: mul });
        dag.terminals.push(term);

        let wasm_dag = WasmDag::build(dag, 4);
        let host = WasmHost::instantiate(&wasm_dag.wasm_bytes, &wasm_dag.emitted, wasm_dag.layout)
            .expect("instantiate");

        let state = empty_state();
        let svc: Vec<Option<i32>> = vec![None; 4];
        let mc: HashMap<i32, i32> = HashMap::new();
        let tc: Vec<Option<i32>> = Vec::new();
        host.reset_tick(); for (i, opt) in svc.iter().enumerate() { if let Some(v) = *opt { host.poke_state_var(i, v); } } for (i, opt) in tc.iter().enumerate() { if let Some(v) = *opt { host.poke_transient(i, v); } }
        let v = host.run_terminal(0, &state, &mc);
        assert_eq!(v, 42);
    }

    #[test]
    fn state_var_cache_hit_returns_cached_value() {
        // LoadVar from a state-var slot whose cache has a hit;
        // expect the cached value, not state.read_mem.
        let mut dag = Dag::default();
        let load = dag.push(DagNode::LoadVar {
            slot: -1,
            kind: crate::dag::types::TickPosition::Current,
        });
        let term = dag.push(DagNode::WriteVar { slot: -2, value: load });
        dag.terminals.push(term);

        let wasm_dag = WasmDag::build(dag, 4);
        let host = WasmHost::instantiate(&wasm_dag.wasm_bytes, &wasm_dag.emitted, wasm_dag.layout)
            .expect("instantiate");

        let state = empty_state();
        let mut svc: Vec<Option<i32>> = vec![None; 4];
        svc[0] = Some(123); // slot -1 → idx 0
        let mc: HashMap<i32, i32> = HashMap::new();
        let tc: Vec<Option<i32>> = Vec::new();
        host.reset_tick(); for (i, opt) in svc.iter().enumerate() { if let Some(v) = *opt { host.poke_state_var(i, v); } } for (i, opt) in tc.iter().enumerate() { if let Some(v) = *opt { host.poke_transient(i, v); } }
        let v = host.run_terminal(0, &state, &mc);
        assert_eq!(v, 123);
    }
}
