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
    WasmCalcKind, WasmMemoryLayout, WasmSlab, ROUND_DOWN, ROUND_NEAREST, ROUND_TO_ZERO, ROUND_UP,
    TICK_EPOCH_OFFSET,
};
use crate::state::State;
use crate::types::RoundStrategy;

/// Per-tick host context. Set immediately before each slab call and
/// cleared on return. Pointers are raw because the lifetime they have
/// to span (one wasm call) is one we guarantee by construction.
///
/// `memory_cache` is a `*mut` because `host_store_memory` writes to
/// it from inside wasm (memory-cell `WriteVar` results that don't yet
/// live in the wasm module's linear memory — Step 2 will lift them
/// into linear memory and remove this back-channel).
pub struct WasmHostCtx {
    pub state: *const State,
    pub memory_cache: *mut HashMap<i32, i32>,
}

impl Default for WasmHostCtx {
    fn default() -> Self {
        WasmHostCtx { state: std::ptr::null(), memory_cache: std::ptr::null_mut() }
    }
}

unsafe impl Send for WasmHostCtx {}

/// Compiled + instantiated wasm module ready to run slab calls.
pub struct WasmHost {
    #[allow(dead_code)]
    engine: Engine,
    inner: Mutex<WasmHostInner>,
    layout: WasmMemoryLayout,
}

struct WasmHostInner {
    store: Store<WasmHostCtx>,
    /// One typed function handle per slab, indexed by slab index. Slab
    /// functions are `() -> ()` — they read/write linear memory rather
    /// than passing scalar args/returns across the wasmtime boundary.
    slab_fns: Vec<TypedFunc<(), ()>>,
    /// Exported wasm linear memory, used to materialise state-var
    /// and transient caches before each tick and to record slab
    /// results in-place.
    memory: Memory,
    /// Current tick's epoch, mirroring the value at `TICK_EPOCH_OFFSET`
    /// in linear memory. The host bumps this in `reset_tick`. Cells
    /// store the epoch at write time; "present" is `cell_epoch ==
    /// tick_epoch`. Avoids the O(memory_size) per-tick reset of a
    /// boolean presence array.
    tick_epoch: i32,
}

impl WasmHost {
    pub fn instantiate(
        wasm_bytes: &[u8],
        slabs: &[WasmSlab],
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

        let mut slab_fns: Vec<TypedFunc<(), ()>> = Vec::with_capacity(slabs.len());
        for slab in slabs {
            let name = format!("slab{}", slab.func_idx);
            let f = instance.get_typed_func::<(), ()>(&mut store, &name)?;
            slab_fns.push(f);
        }

        Ok(WasmHost {
            engine,
            inner: Mutex::new(WasmHostInner {
                store,
                slab_fns,
                memory,
                tick_epoch: 0,
            }),
            layout,
        })
    }

    pub fn slab_count(&self) -> usize {
        let g = self.inner.lock().expect("WasmHost poisoned");
        g.slab_fns.len()
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
    /// Bump the tick epoch. Old presence values (storing the prior
    /// epoch) become non-equal to the new epoch and so read as
    /// "absent" — effectively clearing all cache regions in O(1).
    /// On overflow back to 0 we still need to physically zero every
    /// presence array (otherwise stale `0` values would alias as
    /// "present at epoch 0"); this happens once every ~2 B ticks.
    pub fn reset_tick(&self) {
        let mut guard = self.inner.lock().expect("WasmHost poisoned");
        let WasmHostInner { store, memory, tick_epoch, .. } = &mut *guard;
        let layout = self.layout;
        let next = tick_epoch.wrapping_add(1);
        if next == 0 {
            // Wraparound — zero all presence arrays so the new epoch
            // (1) doesn't alias with the prior epoch's stale stores.
            let mem_data: &mut [u8] = memory.data_mut(&mut *store);
            let sv_pres = layout.sv_present_base as usize;
            let tr_pres = layout.tr_present_base as usize;
            let mc_pres = layout.mc_present_base as usize;
            let sv_pres_len = (layout.n_state_vars as usize) * 4;
            let tr_pres_len = (layout.n_transients as usize) * 4;
            let mc_pres_len = (layout.mc_capacity as usize) * 4;
            for b in &mut mem_data[sv_pres..sv_pres + sv_pres_len] {
                *b = 0;
            }
            for b in &mut mem_data[tr_pres..tr_pres + tr_pres_len] {
                *b = 0;
            }
            for b in &mut mem_data[mc_pres..mc_pres + mc_pres_len] {
                *b = 0;
            }
            *tick_epoch = 1;
        } else {
            *tick_epoch = next;
        }
        // Mirror the bumped epoch into linear memory so wasm reads
        // see the same value via `i32.load TICK_EPOCH_OFFSET`.
        let mem_data: &mut [u8] = memory.data_mut(&mut *store);
        let off = TICK_EPOCH_OFFSET as usize;
        mem_data[off..off + 4].copy_from_slice(&tick_epoch.to_le_bytes());
    }

    /// Record a state-var cache hit at index `i` with value `v`.
    /// Cheap: two i32 writes.
    pub fn poke_state_var(&self, i: usize, v: i32) {
        let mut guard = self.inner.lock().expect("WasmHost poisoned");
        let WasmHostInner { store, memory, tick_epoch, .. } = &mut *guard;
        let mem_data: &mut [u8] = memory.data_mut(&mut *store);
        let layout = self.layout;
        if i >= layout.n_state_vars as usize {
            return;
        }
        let voff = layout.sv_value_base as usize + i * 4;
        let poff = layout.sv_present_base as usize + i * 4;
        mem_data[voff..voff + 4].copy_from_slice(&v.to_le_bytes());
        mem_data[poff..poff + 4].copy_from_slice(&tick_epoch.to_le_bytes());
    }

    /// Record a transient cache hit at index `i` with value `v`.
    pub fn poke_transient(&self, i: usize, v: i32) {
        let mut guard = self.inner.lock().expect("WasmHost poisoned");
        let WasmHostInner { store, memory, tick_epoch, .. } = &mut *guard;
        let mem_data: &mut [u8] = memory.data_mut(&mut *store);
        let layout = self.layout;
        if i >= layout.n_transients as usize {
            return;
        }
        let voff = layout.tr_value_base as usize + i * 4;
        let poff = layout.tr_present_base as usize + i * 4;
        mem_data[voff..voff + 4].copy_from_slice(&v.to_le_bytes());
        mem_data[poff..poff + 4].copy_from_slice(&tick_epoch.to_le_bytes());
    }

    /// Record a memory-cell cache hit at slot `addr` with value `v`.
    /// Used by walker fallback when it writes a memory cell mid-tick,
    /// so subsequent slab loads of that cell see the cascade write.
    pub fn poke_mem_cell(&self, addr: i32, v: i32) {
        let mut guard = self.inner.lock().expect("WasmHost poisoned");
        let WasmHostInner { store, memory, tick_epoch, .. } = &mut *guard;
        let mem_data: &mut [u8] = memory.data_mut(&mut *store);
        let layout = self.layout;
        if addr < 0 || (addr as u32) >= layout.mc_capacity {
            return;
        }
        let voff = layout.mc_value_base as usize + (addr as usize) * 4;
        let poff = layout.mc_present_base as usize + (addr as usize) * 4;
        mem_data[voff..voff + 4].copy_from_slice(&v.to_le_bytes());
        mem_data[poff..poff + 4].copy_from_slice(&tick_epoch.to_le_bytes());
    }

    /// Read back the memory-cell cache value/present pair after a
    /// slab has run. `present == true` iff the slab wrote this cell
    /// during the current tick.
    pub fn read_mem_cell(&self, addr: i32) -> (i32, bool) {
        let mut guard = self.inner.lock().expect("WasmHost poisoned");
        let WasmHostInner { store, memory, tick_epoch, .. } = &mut *guard;
        let mem_data: &[u8] = memory.data(&mut *store);
        let layout = self.layout;
        if addr < 0 || (addr as u32) >= layout.mc_capacity {
            return (0, false);
        }
        let voff = layout.mc_value_base as usize + (addr as usize) * 4;
        let poff = layout.mc_present_base as usize + (addr as usize) * 4;
        let v = i32::from_le_bytes(mem_data[voff..voff + 4].try_into().expect("4 bytes"));
        let p = i32::from_le_bytes(mem_data[poff..poff + 4].try_into().expect("4 bytes"));
        (v, p == *tick_epoch)
    }

    /// Bulk version of `read_mem_cell`: scan a list of addresses and
    /// invoke `f(addr, value)` for each whose `present == tick_epoch`.
    /// Holds the host Mutex for one acquisition instead of N — avoids
    /// per-cell lock overhead when a slab covers many memory writes.
    pub fn drain_mem_cells_into<F: FnMut(i32, i32)>(&self, addrs: &[i32], mut f: F) {
        let mut guard = self.inner.lock().expect("WasmHost poisoned");
        let WasmHostInner { store, memory, tick_epoch, .. } = &mut *guard;
        let mem_data: &[u8] = memory.data(&mut *store);
        let layout = self.layout;
        let epoch = *tick_epoch;
        for &addr in addrs {
            if addr < 0 || (addr as u32) >= layout.mc_capacity {
                continue;
            }
            let voff = layout.mc_value_base as usize + (addr as usize) * 4;
            let poff = layout.mc_present_base as usize + (addr as usize) * 4;
            let v = i32::from_le_bytes(mem_data[voff..voff + 4].try_into().expect("4 bytes"));
            let p = i32::from_le_bytes(mem_data[poff..poff + 4].try_into().expect("4 bytes"));
            if p == epoch {
                f(addr, v);
            }
        }
    }

    /// Bulk version of `read_state_var`: walk a list of state-var
    /// indices and invoke `f(idx, value)` for each whose
    /// `present == tick_epoch`.
    pub fn drain_state_vars_into<F: FnMut(usize, i32)>(&self, indices: &[usize], mut f: F) {
        let mut guard = self.inner.lock().expect("WasmHost poisoned");
        let WasmHostInner { store, memory, tick_epoch, .. } = &mut *guard;
        let mem_data: &[u8] = memory.data(&mut *store);
        let layout = self.layout;
        let epoch = *tick_epoch;
        for &idx in indices {
            if idx >= layout.n_state_vars as usize {
                continue;
            }
            let voff = layout.sv_value_base as usize + idx * 4;
            let poff = layout.sv_present_base as usize + idx * 4;
            let v = i32::from_le_bytes(mem_data[voff..voff + 4].try_into().expect("4 bytes"));
            let p = i32::from_le_bytes(mem_data[poff..poff + 4].try_into().expect("4 bytes"));
            if p == epoch {
                f(idx, v);
            }
        }
    }

    /// Bulk version of `read_transient`.
    pub fn drain_transients_into<F: FnMut(usize, i32)>(&self, indices: &[usize], mut f: F) {
        let mut guard = self.inner.lock().expect("WasmHost poisoned");
        let WasmHostInner { store, memory, tick_epoch, .. } = &mut *guard;
        let mem_data: &[u8] = memory.data(&mut *store);
        let layout = self.layout;
        let epoch = *tick_epoch;
        for &idx in indices {
            if idx >= layout.n_transients as usize {
                continue;
            }
            let voff = layout.tr_value_base as usize + idx * 4;
            let poff = layout.tr_present_base as usize + idx * 4;
            let v = i32::from_le_bytes(mem_data[voff..voff + 4].try_into().expect("4 bytes"));
            let p = i32::from_le_bytes(mem_data[poff..poff + 4].try_into().expect("4 bytes"));
            if p == epoch {
                f(idx, v);
            }
        }
    }

    /// Run slab `i` against the host state. Slabs read state-var /
    /// transient caches and write their results directly into linear
    /// memory; memory-cell reads/writes still trampoline through host
    /// imports until Step 2 lifts them into linear memory.
    ///
    /// SAFETY: caller guarantees `state` and `memory_cache` outlive
    /// the call.
    pub fn run_slab(&self, i: usize, state: &State, memory_cache: &mut HashMap<i32, i32>) {
        let mut guard = self.inner.lock().expect("WasmHost poisoned");
        let WasmHostInner { store, slab_fns, .. } = &mut *guard;
        {
            let ctx = store.data_mut();
            ctx.state = state as *const State;
            ctx.memory_cache = memory_cache as *mut _;
        }
        let f = &slab_fns[i];
        f.call(&mut *store, ()).expect("wasm slab call failed");
        {
            let ctx = store.data_mut();
            *ctx = WasmHostCtx::default();
        }
    }

    /// Read back a state-var cache value/present pair after a slab
    /// has run. `present == true` iff the slab wrote this slot during
    /// the current tick.
    pub fn read_state_var(&self, i: usize) -> (i32, bool) {
        let mut guard = self.inner.lock().expect("WasmHost poisoned");
        let WasmHostInner { store, memory, tick_epoch, .. } = &mut *guard;
        let mem_data: &[u8] = memory.data(&mut *store);
        let layout = self.layout;
        if i >= layout.n_state_vars as usize {
            return (0, false);
        }
        let voff = layout.sv_value_base as usize + i * 4;
        let poff = layout.sv_present_base as usize + i * 4;
        let v = i32::from_le_bytes(mem_data[voff..voff + 4].try_into().expect("4 bytes"));
        let p = i32::from_le_bytes(mem_data[poff..poff + 4].try_into().expect("4 bytes"));
        (v, p == *tick_epoch)
    }

    /// Read back a transient cache value/present pair after a slab
    /// has run.
    pub fn read_transient(&self, i: usize) -> (i32, bool) {
        let mut guard = self.inner.lock().expect("WasmHost poisoned");
        let WasmHostInner { store, memory, tick_epoch, .. } = &mut *guard;
        let mem_data: &[u8] = memory.data(&mut *store);
        let layout = self.layout;
        if i >= layout.n_transients as usize {
            return (0, false);
        }
        let voff = layout.tr_value_base as usize + i * 4;
        let poff = layout.tr_present_base as usize + i * 4;
        let v = i32::from_le_bytes(mem_data[voff..voff + 4].try_into().expect("4 bytes"));
        let p = i32::from_le_bytes(mem_data[poff..poff + 4].try_into().expect("4 bytes"));
        (v, p == *tick_epoch)
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
    linker.func_wrap("host", "store_memory", host_store_memory)?;
    Ok(())
}

fn host_store_memory(caller: Caller<'_, WasmHostCtx>, slot: i32, value: i32) {
    let ctx = caller.data();
    // SAFETY: pointer set by run_slab (`*mut HashMap`), cleared on
    // return. Memory-cell WriteVar results land here for now;
    // Step 2 moves the cache into linear memory and removes this
    // back-channel.
    unsafe {
        let cache = &mut *ctx.memory_cache;
        cache.insert(slot, value);
    }
}

// ----- imports --------------------------------------------------------

fn host_read_memory(caller: Caller<'_, WasmHostCtx>, slot: i32) -> i32 {
    let ctx = caller.data();
    // SAFETY: pointers set by run_slab, cleared after.
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

        let wasm_dag = WasmDag::build(dag, 4, 256);
        let host = WasmHost::instantiate(&wasm_dag.wasm_bytes, &wasm_dag.slabs, wasm_dag.layout)
            .expect("instantiate");
        assert_eq!(host.slab_count(), 1);

        let state = empty_state();
        let mut mc: HashMap<i32, i32> = HashMap::new();
        host.reset_tick();
        host.run_slab(0, &state, &mut mc);
        assert_eq!(host.read_state_var(0), (42, true));
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

        let wasm_dag = WasmDag::build(dag, 4, 256);
        let host = WasmHost::instantiate(&wasm_dag.wasm_bytes, &wasm_dag.slabs, wasm_dag.layout)
            .expect("instantiate");

        let state = empty_state();
        let mut mc: HashMap<i32, i32> = HashMap::new();
        host.reset_tick();
        host.run_slab(0, &state, &mut mc);
        assert_eq!(host.read_state_var(0), (6, true));
    }

    #[test]
    fn mul_via_pure_wasm() {
        let mut dag = Dag::default();
        let six = dag.push(DagNode::Lit(6.0));
        let seven = dag.push(DagNode::Lit(7.0));
        let mul = dag.push(DagNode::Calc { op: CalcKind::Mul, args: vec![six, seven] });
        let term = dag.push(DagNode::WriteVar { slot: -1, value: mul });
        dag.terminals.push(term);

        let wasm_dag = WasmDag::build(dag, 4, 256);
        let host = WasmHost::instantiate(&wasm_dag.wasm_bytes, &wasm_dag.slabs, wasm_dag.layout)
            .expect("instantiate");

        let state = empty_state();
        let mut mc: HashMap<i32, i32> = HashMap::new();
        host.reset_tick();
        host.run_slab(0, &state, &mut mc);
        assert_eq!(host.read_state_var(0), (42, true));
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

        let wasm_dag = WasmDag::build(dag, 4, 256);
        let host = WasmHost::instantiate(&wasm_dag.wasm_bytes, &wasm_dag.slabs, wasm_dag.layout)
            .expect("instantiate");

        let state = empty_state();
        host.reset_tick();
        host.poke_state_var(0, 123); // slot -1 → idx 0
        let mut mc: HashMap<i32, i32> = HashMap::new();
        host.run_slab(0, &state, &mut mc);
        // Slot -2 → idx 1.
        assert_eq!(host.read_state_var(1), (123, true));
    }
}
