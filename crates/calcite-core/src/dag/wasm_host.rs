//! Native host runtime for executing the wasm module produced by
//! `wasm_codegen`. Wraps wasmtime and provides:
//!
//! - One-shot module instantiation (build the wasmtime `Module` and
//!   `Instance` from the bytes produced by `WasmDag::build`).
//! - Per-tick state binding: the host pokes raw pointers to the
//!   per-tick caches into the wasmtime `Store` data, so import calls
//!   from wasm see the current tick's caches without going through
//!   wasmtime's structured-data interface (which would conflict with
//!   Rust's borrow rules during a call).
//! - Calling one terminal function and returning its `i32` result.
//!
//! ## Native-only
//!
//! This module is gated `cfg(not(target_arch = "wasm32"))` from
//! `dag::mod`. The browser runs the wasm bytes via the JS engine
//! (`WebAssembly.instantiate`) — that wiring lives in `calcite-wasm`
//! and is a follow-up. Without it, `Backend::DagV2Wasm` is a
//! native-only backend, used for differentials and benches.
//!
//! ## Lifetime trick
//!
//! Wasmtime's `Caller<'_, T>` gives mutable access to `T` during a
//! host call. We can't put `&mut State` in `T` because the outer
//! caller (the tick driver) holds it too. So we put **raw pointers**
//! in `T`, set them at the top of `tick`, clear them after. Host
//! callbacks deref the pointers under `unsafe`. The pointers are
//! valid for the whole tick; nothing reentrant runs that would
//! invalidate them. Same idiom every wasmtime-host hot-path codebase
//! uses — see e.g. wasmtime's own examples.

use std::collections::HashMap;
use std::sync::Mutex;

use wasmtime::{Caller, Engine, Instance, Linker, Module, Store, TypedFunc};

use crate::dag::wasm_codegen::{
    WasmCalcKind, ROUND_DOWN, ROUND_NEAREST, ROUND_TO_ZERO, ROUND_UP,
};
use crate::dag::TRANSIENT_BASE;
use crate::state::State;
use crate::types::RoundStrategy;

/// Per-tick host context. Lives inside the wasmtime `Store`'s data
/// slot; pointers are set by `WasmHost::with_tick_ctx` immediately
/// before calling into wasm and cleared on return.
///
/// All pointers are raw because the lifetime they have to span (one
/// wasm call from the host) is one we can guarantee by construction
/// and one Rust's borrow checker can't see through.
pub struct WasmHostCtx {
    pub state: *const State,
    pub state_var_cache: *const Vec<Option<i32>>,
    pub memory_cache: *const HashMap<i32, i32>,
    pub transient_cache: *const Vec<Option<i32>>,
}

impl Default for WasmHostCtx {
    fn default() -> Self {
        WasmHostCtx {
            state: std::ptr::null(),
            state_var_cache: std::ptr::null(),
            memory_cache: std::ptr::null(),
            transient_cache: std::ptr::null(),
        }
    }
}

// SAFETY: WasmHostCtx is only used inside an Evaluator owned by one
// thread; the raw pointers it carries are not shared across threads.
// wasmtime's Store requires Send for its data, hence the marker.
unsafe impl Send for WasmHostCtx {}

/// Compiled + instantiated wasm module ready to run terminal calls.
///
/// Holds a wasmtime `Engine`, `Store`, `Instance`, and a per-terminal
/// `TypedFunc<(), i32>` cache so we don't pay the export-lookup cost
/// per tick.
///
/// Wrapped in a `Mutex` because wasmtime's `Store` is `!Sync`; the
/// only contention is per-tick, so this is essentially free.
pub struct WasmHost {
    /// Engine shared across reinstantiations (cheap to clone).
    engine: Engine,
    /// Store + Instance + TypedFunc cache. The Mutex is here only to
    /// satisfy `Sync`-when-shared in case the `Evaluator` ever moves
    /// across threads; on the hot path we just `lock()` once per tick.
    inner: Mutex<WasmHostInner>,
}

struct WasmHostInner {
    store: Store<WasmHostCtx>,
    /// Per-terminal pre-resolved typed function. Indexed by the same
    /// position as `WasmDag::emitted` (so emitted[i] = Some(...) ⇒
    /// typed_fns[i] = Some(...), and vice versa).
    typed_fns: Vec<Option<TypedFunc<(), i32>>>,
}

impl WasmHost {
    /// Compile and instantiate the wasm bytes produced by
    /// `WasmDag::build`. The host sets up the seven imports
    /// (read_state_var, read_memory, read_transient, read_prev,
    /// calc_binary, calc_unary, calc_ternary).
    pub fn instantiate(
        wasm_bytes: &[u8],
        emitted: &[Option<crate::dag::wasm_codegen::EmittedWasmTerminal>],
    ) -> Result<Self, wasmtime::Error> {
        let engine = Engine::default();
        let module = Module::new(&engine, wasm_bytes)?;

        let mut linker: Linker<WasmHostCtx> = Linker::new(&engine);
        register_imports(&mut linker)?;

        let mut store: Store<WasmHostCtx> = Store::new(&engine, WasmHostCtx::default());
        let instance = linker.instantiate(&mut store, &module)?;

        // Pre-resolve each emitted terminal's exported function as a
        // TypedFunc. Terminals with no emit slot stay None.
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
            inner: Mutex::new(WasmHostInner { store, typed_fns }),
        })
    }

    /// Diagnostic: number of emitted terminal entries that resolved
    /// to a callable typed function.
    pub fn typed_fn_count(&self) -> usize {
        let g = self.inner.lock().expect("WasmHost poisoned");
        g.typed_fns.iter().filter(|x| x.is_some()).count()
    }

    /// Quick liveness check that delegates to `wasmtime::Engine::increment_epoch`.
    /// Currently only used from tests to confirm the engine is real.
    #[doc(hidden)]
    pub fn engine_alive(&self) -> bool {
        // Increment-epoch returns nothing useful, but invoking it
        // proves the engine handle is intact.
        self.engine.increment_epoch();
        true
    }

    /// Run the i-th terminal under a tick context bound to the
    /// supplied caches and state. Panics if the index is out of
    /// range or if the terminal wasn't emitted to wasm.
    ///
    /// SAFETY: the caller guarantees that `state`, `state_var_cache`,
    /// `memory_cache`, and `transient_cache` outlive the call.
    pub fn run_terminal(
        &self,
        i: usize,
        state: &State,
        state_var_cache: &Vec<Option<i32>>,
        memory_cache: &HashMap<i32, i32>,
        transient_cache: &Vec<Option<i32>>,
    ) -> i32 {
        let mut guard = self.inner.lock().expect("WasmHost poisoned");
        let WasmHostInner { store, typed_fns } = &mut *guard;
        // Bind tick-context pointers.
        {
            let ctx = store.data_mut();
            ctx.state = state as *const State;
            ctx.state_var_cache = state_var_cache as *const _;
            ctx.memory_cache = memory_cache as *const _;
            ctx.transient_cache = transient_cache as *const _;
        }
        let f = typed_fns[i]
            .as_ref()
            .expect("run_terminal called with non-emitted terminal");
        let result = f.call(&mut *store, ()).expect("wasm terminal call failed");
        // Clear pointers so a stray reentrant call would NPE
        // immediately rather than read stale data.
        {
            let ctx = store.data_mut();
            *ctx = WasmHostCtx::default();
        }
        result
    }
}

/// Register the seven host imports the codegen relies on.
fn register_imports(linker: &mut Linker<WasmHostCtx>) -> Result<(), wasmtime::Error> {
    linker.func_wrap("host", "read_state_var", host_read_state_var)?;
    linker.func_wrap("host", "read_memory", host_read_memory)?;
    linker.func_wrap("host", "read_transient", host_read_transient)?;
    linker.func_wrap("host", "read_prev", host_read_prev)?;
    linker.func_wrap("host", "calc_binary", host_calc_binary)?;
    linker.func_wrap("host", "calc_unary", host_calc_unary)?;
    linker.func_wrap("host", "calc_ternary", host_calc_ternary)?;
    Ok(())
}

// ----- imports --------------------------------------------------------

/// Read a state-var slot (slot < 0). Mirrors the
/// `state_var_cache`-then-`state.read_mem` dispatch in
/// `inlined_codegen::read_slot_dispatch`.
fn host_read_state_var(caller: Caller<'_, WasmHostCtx>, slot: i32) -> i32 {
    let ctx = caller.data();
    // SAFETY: the driver sets these pointers before invoking the
    // wasm function and clears them after. The lifetime of the call
    // is bounded by `WasmHost::run_terminal`, where the caches are
    // borrowed.
    unsafe {
        let cache = &*ctx.state_var_cache;
        let state = &*ctx.state;
        if slot < 0 {
            let idx = (-slot - 1) as usize;
            if let Some(Some(v)) = cache.get(idx) {
                return *v;
            }
        }
        state.read_mem(slot)
    }
}

fn host_read_memory(caller: Caller<'_, WasmHostCtx>, slot: i32) -> i32 {
    let ctx = caller.data();
    unsafe {
        let cache = &*ctx.memory_cache;
        if let Some(&v) = cache.get(&slot) {
            return v;
        }
        let state = &*ctx.state;
        state.read_mem(slot)
    }
}

fn host_read_transient(caller: Caller<'_, WasmHostCtx>, slot: i32) -> i32 {
    let ctx = caller.data();
    unsafe {
        let cache = &*ctx.transient_cache;
        let idx = (slot - TRANSIENT_BASE) as usize;
        cache.get(idx).and_then(|x| *x).unwrap_or(0)
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
//
// These mirror `inlined_codegen::run_terminal`'s arithmetic arms
// arm-for-arm — same f64 cast pattern, same div-by-zero handling,
// same rounding semantics. The wire kinds are the `WasmCalcKind`
// enum from `wasm_codegen.rs`.

fn host_calc_binary(_caller: Caller<'_, WasmHostCtx>, kind: i32, a: i32, b: i32) -> i32 {
    let Some(k) = WasmCalcKind::from_i32(kind) else { return 0; };
    match k {
        WasmCalcKind::Mul => {
            let av = a as f64;
            let bv = b as f64;
            (av * bv) as i32
        }
        WasmCalcKind::Div => {
            let bv = b as f64;
            if bv == 0.0 {
                0
            } else {
                let av = a as f64;
                (av / bv) as i32
            }
        }
        WasmCalcKind::Mod => {
            let bv = b as f64;
            if bv == 0.0 {
                0
            } else {
                let av = a as f64;
                (av - (av / bv).floor() * bv) as i32
            }
        }
        WasmCalcKind::Pow => {
            let av = a as f64;
            let bv = b as f64;
            av.powf(bv) as i32
        }
        WasmCalcKind::Min2 => (a as f64).min(b as f64) as i32,
        WasmCalcKind::Max2 => (a as f64).max(b as f64) as i32,
        // Unary kinds shouldn't be dispatched here.
        _ => 0,
    }
}

fn host_calc_unary(_caller: Caller<'_, WasmHostCtx>, kind: i32, a: i32) -> i32 {
    let Some(k) = WasmCalcKind::from_i32(kind) else { return 0; };
    match k {
        WasmCalcKind::Neg => (-(a as f64)) as i32,
        WasmCalcKind::Abs => (a as f64).abs() as i32,
        WasmCalcKind::Sign => {
            let av = a as f64;
            (if av > 0.0 { 1.0 } else if av < 0.0 { -1.0 } else { 0.0 }) as i32
        }
        _ => 0,
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
        WasmCalcKind::Clamp => {
            let lo = a as f64;
            let v = b as f64;
            let hi = c as f64;
            v.clamp(lo, hi) as i32
        }
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
        _ => 0,
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

        let wasm_dag = WasmDag::build(dag);
        let host = WasmHost::instantiate(&wasm_dag.wasm_bytes, &wasm_dag.emitted)
            .expect("instantiate");
        assert_eq!(host.typed_fn_count(), 1);

        let state = empty_state();
        let svc: Vec<Option<i32>> = vec![None; 8];
        let mc: HashMap<i32, i32> = HashMap::new();
        let tc: Vec<Option<i32>> = Vec::new();
        let v = host.run_terminal(0, &state, &svc, &mc, &tc);
        assert_eq!(v, 42);
    }

    #[test]
    fn arith_chain_wasm_runs() {
        // (3 + 4) - 1 = 6 (using only Add/Sub which are pure i32 ops,
        // no host trampoline).
        let mut dag = Dag::default();
        let three = dag.push(DagNode::Lit(3.0));
        let four = dag.push(DagNode::Lit(4.0));
        let one = dag.push(DagNode::Lit(1.0));
        let add = dag.push(DagNode::Calc { op: CalcKind::Add, args: vec![three, four] });
        let sub = dag.push(DagNode::Calc { op: CalcKind::Sub, args: vec![add, one] });
        let term = dag.push(DagNode::WriteVar { slot: -1, value: sub });
        dag.terminals.push(term);

        let wasm_dag = WasmDag::build(dag);
        let host = WasmHost::instantiate(&wasm_dag.wasm_bytes, &wasm_dag.emitted)
            .expect("instantiate");

        let state = empty_state();
        let svc: Vec<Option<i32>> = vec![None; 8];
        let mc: HashMap<i32, i32> = HashMap::new();
        let tc: Vec<Option<i32>> = Vec::new();
        let v = host.run_terminal(0, &state, &svc, &mc, &tc);
        assert_eq!(v, 6);
    }

    #[test]
    fn mul_via_host_trampoline() {
        // 6 * 7 = 42 — exercises host_calc_binary.
        let mut dag = Dag::default();
        let six = dag.push(DagNode::Lit(6.0));
        let seven = dag.push(DagNode::Lit(7.0));
        let mul = dag.push(DagNode::Calc { op: CalcKind::Mul, args: vec![six, seven] });
        let term = dag.push(DagNode::WriteVar { slot: -1, value: mul });
        dag.terminals.push(term);

        let wasm_dag = WasmDag::build(dag);
        let host = WasmHost::instantiate(&wasm_dag.wasm_bytes, &wasm_dag.emitted)
            .expect("instantiate");

        let state = empty_state();
        let svc: Vec<Option<i32>> = vec![None; 8];
        let mc: HashMap<i32, i32> = HashMap::new();
        let tc: Vec<Option<i32>> = Vec::new();
        let v = host.run_terminal(0, &state, &svc, &mc, &tc);
        assert_eq!(v, 42);
    }

    #[test]
    fn engine_handle_is_alive() {
        let mut dag = Dag::default();
        let lit = dag.push(DagNode::Lit(0.0));
        let term = dag.push(DagNode::WriteVar { slot: -1, value: lit });
        dag.terminals.push(term);
        let wasm_dag = WasmDag::build(dag);
        let host = WasmHost::instantiate(&wasm_dag.wasm_bytes, &wasm_dag.emitted)
            .expect("instantiate");
        assert!(host.engine_alive());
    }
}
