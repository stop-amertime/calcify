//! Descriptor-driven REP fast-forward applier.
//!
//! Phase 3b step 4 shipped [`apply_fill`] — the `BulkClass::Fill` arm of
//! the eventual replacement for `compile.rs::rep_fast_forward`'s hardcoded
//! per-opcode `match`. Step 5 adds [`apply_copy`] — the `BulkClass::Copy`
//! arm, MOVS-shaped: each iteration reads a byte (or a small block of
//! bytes) at a stepping source pointer and writes it at a stepping
//! destination pointer. Step 6 fills in `ReadOnly` next to them; step 7
//! then rips out the hardcoded path.
//!
//! ## What "descriptor-driven" means here
//!
//! The applier reads only structural metadata produced by the recogniser:
//!
//! - `descriptor.counter.property` — the slot whose decrement gates the
//!   loop; resolved to a slot index and read for the current iteration
//!   count.
//! - `descriptor.pointers[0]` — the destination pointer slot, its
//!   per-iteration `base_step`, and the direction-flag slot+bit. The
//!   applier never asks "is this STOSB or STOSW?"; the byte-vs-word width
//!   is whatever `base_step` says.
//! - `descriptor.writes[]` — one entry per byte the loop emits per
//!   iteration. The position of each entry in the vector is its offset
//!   *within* one iteration's destination slice. (Kiln emits paired
//!   addr/val slots in source order, sorted here by `assignment_index`;
//!   for a 2-byte word write the low byte is index 0, high byte is index
//!   1 — which is what the hardcoded path encodes by name as `lo` / `hi`.)
//! - `WriteEntry.addr_decomposition` — `Some((seg_property, ptr_property))`
//!   for the canonical `seg*16 + ptr` shape. The applier reads the seg
//!   slot once per call and the pointer slot once per call.
//! - `WriteEntry.val_expr` — the source of the byte. For `Fill` the
//!   classifier guarantees the value is independent of any pointer mirror;
//!   step 4 supports the bare `Var(name)` shape (resolves to a slot read)
//!   and panics on anything else, leaving the door open for future shapes
//!   without silently miscomputing.
//!
//! ## What it does NOT read
//!
//! - No property name as text. Strings only ever flow through
//!   `program.property_slots` and `state.state_var_index` lookups
//!   (whole-name equality, the same routing the hardcoded path uses).
//! - No opcode value. The dispatch in [`apply_fill`] is by structural
//!   role — counter slot, pointer slot, write entries — and the function
//!   would behave identically on a 6502 / brainfuck / non-emulator
//!   cabinet whose CSS happened to share this shape.
//! - No constant other than 16 (already absorbed by the recogniser into
//!   `addr_decomposition`) and the literal byte mask 0xFF (the structural
//!   "extract a byte from a wider integer slot" — same as the hardcoded
//!   path; not encoded as "this is x86").
//!
//! ## Where the byte mutations go
//!
//! Routes through the existing `compile::bulk_fill` /
//! `compile::bulk_store_byte` primitives. Those primitives own the
//! packed-cell + windowed-array + extended-map routing the hardcoded path
//! relies on; rewriting them here would be reinvention.

use crate::compile::{bulk_fill, bulk_store_byte, ranges_overlap_virtual, CompiledProgram};
use crate::pattern::loop_descriptor::{BulkClass, LoopDescriptor};
use crate::state::State;
use crate::types::Expr;

/// Outcome of applying a descriptor to state. The harness consumes this to
/// decide whether to compare against the hardcoded path; future callers
/// (step 7) will simply ignore non-`Applied` outcomes and let the
/// hardcoded path keep running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApplyOutcome {
    /// Applier ran to completion. Real cabinet state has been mutated as
    /// the hardcoded path would have. `iterations` is the number of
    /// iterations the applier collapsed; the harness uses it to charge
    /// cycles when running on a State clone.
    Applied { iterations: i32 },
    /// Applier refused to run because the descriptor's shape isn't yet
    /// supported by the variant landed in the current step. Caller should
    /// fall back to the hardcoded path. (Step 4 returns this for any
    /// `BulkClass` other than `Fill`, and for `Fill` shapes whose
    /// `val_expr` isn't a bare `Var`.)
    Unsupported(&'static str),
}

/// Look up a property name's current value. Mirrors the resolver inside
/// `compile.rs::rep_fast_forward`: try the compiled program's
/// `property_slots` first (which resolves slot-allocated CSS properties),
/// then fall back to `state.state_vars` by bare name (which resolves the
/// register-shaped state vars the cabinet wires through `--CX`/`--DI`/
/// etc.). Returns `None` if neither table has the name.
fn read_prop(
    program: &CompiledProgram,
    state: &State,
    slots: &[i32],
    name: &str,
) -> Option<i32> {
    if let Some(&s) = program.property_slots.get(name) {
        return Some(slots[s as usize]);
    }
    let bare = name.strip_prefix("--").unwrap_or(name);
    state.get_var(bare)
}

/// Resolve a `val_expr` to its current i32 value, structurally.
///
/// Step 4 supports the bare `Var(name)` shape only — the recogniser's
/// `BulkClass::Fill` classification guarantees the value is independent
/// of any pointer mirror, and on doom8088 the cabinet emits STOS as a
/// bare slot read of AL/AX. Future shapes (e.g. structural byte-extract
/// of a wider register) extend this matcher; bailing with
/// `Unsupported` keeps unmatched shapes routed through the hardcoded
/// path until they're handled.
fn resolve_fill_value(
    val_expr: &Expr,
    program: &CompiledProgram,
    state: &State,
    slots: &[i32],
) -> Option<i32> {
    match val_expr {
        Expr::Var { name, fallback: None } => read_prop(program, state, slots, name),
        Expr::Var { name, fallback: Some(_) } => {
            // A var with a fallback expression — kiln does emit these
            // for some slot reads. The fallback only triggers if the
            // primary slot is missing; in a fully-loaded cabinet the
            // primary always resolves, so reading the slot is correct.
            // If it doesn't resolve, returning None forces the caller
            // to bail.
            read_prop(program, state, slots, name)
        }
        // Step 4 deliberately handles only the bare-Var shape. STOSW on
        // doom8088 emits two writes whose val_exprs are bare `Var(--AL)`
        // and `Var(--AH)` (kiln has dedicated AL/AH slots that mirror
        // AX). Cabinets that emit `(AX & 0xFF)` and `((AX >> 8) & 0xFF)`
        // calc shapes will need their own arm here — bail with
        // `Unsupported` until that arm lands.
        _ => None,
    }
}

/// Apply a `BulkClass::Fill` descriptor to state. The structural contract:
///
/// 1. Resolve counter, destination pointer, direction flag.
/// 2. For each `WriteEntry`, resolve its address decomposition's seg+ptr
///    slots and its `val_expr`. Position-in-vec is the byte offset within
///    one iteration.
/// 3. For each iteration `i ∈ [0, n)`, for each write entry `k`, write
///    `byte_k` at `seg*16 + pointer + signed_step*i + k`.
///
/// Returns the iteration count so the caller can charge cycles. Mirrors
/// what `compile::rep_fast_forward` does, but driven by descriptor
/// structure instead of a `match opcode { 0xAA => ..., 0xAB => ... }`.
///
/// This function is reentrant and pure of side effects beyond the state
/// mutations it performs — no env-var reads, no global state.
pub(crate) fn apply_fill(
    descriptor: &LoopDescriptor,
    program: &CompiledProgram,
    state: &mut State,
    slots: &[i32],
) -> ApplyOutcome {
    if descriptor.bulk_class != BulkClass::Fill {
        return ApplyOutcome::Unsupported("not Fill class");
    }
    // Counter — must be present, must resolve, must be > 0. CX<=0 is the
    // "no work to do" case the hardcoded path already filters before
    // calling us; the dual harness only reaches here after that filter,
    // so we treat it as Unsupported (caller bails to hardcoded path).
    let Some(counter) = descriptor.counter.as_ref() else {
        return ApplyOutcome::Unsupported("no counter");
    };
    let Some(n) = read_prop(program, state, slots, &counter.property) else {
        return ApplyOutcome::Unsupported("counter unresolved");
    };
    if n <= 0 {
        return ApplyOutcome::Applied { iterations: 0 };
    }

    // A Fill needs exactly one destination pointer (the byte sink). Two
    // pointers would be a Copy-shape (one source, one dest) — the
    // classifier wouldn't emit `Fill` in that case, but we double-check
    // structurally.
    if descriptor.pointers.len() != 1 {
        return ApplyOutcome::Unsupported("Fill expects exactly one pointer");
    }
    let ptr_entry = &descriptor.pointers[0];
    let Some(ptr_value) = read_prop(program, state, slots, &ptr_entry.self_property) else {
        return ApplyOutcome::Unsupported("pointer self_property unresolved");
    };
    let Some(flags) = read_prop(program, state, slots, &ptr_entry.flag_property) else {
        return ApplyOutcome::Unsupported("flag_property unresolved");
    };
    let df_active = ((flags >> ptr_entry.flag_bit) & 1) != 0;
    let step: i32 = ptr_entry.base_step;
    let signed_step: i32 = if df_active { -step } else { step };

    // Pre-compute (seg, val) for each write entry. Bail early on any
    // missing structural piece.
    if descriptor.writes.is_empty() {
        return ApplyOutcome::Unsupported("Fill expects at least one write");
    }
    if descriptor.writes.len() != step as usize {
        // Structural sanity: the per-iteration byte-count emitted as
        // writes should match the pointer's stride. If they don't, the
        // recogniser emitted an unexpected shape and the position-as-
        // offset assumption is unsafe. Bail.
        return ApplyOutcome::Unsupported(
            "writes.len() != base_step (per-iter byte count mismatch)",
        );
    }

    let mut prepared: Vec<(i64, u8)> = Vec::with_capacity(descriptor.writes.len());
    for w in &descriptor.writes {
        let Some((seg_prop, _ptr_prop)) = w.addr_decomposition.as_ref() else {
            return ApplyOutcome::Unsupported("write has no addr_decomposition");
        };
        let Some(seg) = read_prop(program, state, slots, seg_prop) else {
            return ApplyOutcome::Unsupported("seg_property unresolved");
        };
        let Some(byte_value) = resolve_fill_value(&w.val_expr, program, state, slots) else {
            return ApplyOutcome::Unsupported("val_expr shape not yet supported");
        };
        let seg_base = (seg as i64) * 16;
        prepared.push((seg_base, (byte_value & 0xFF) as u8));
    }

    // Pointer wrap check (16-bit segment offset). Each iteration writes
    // `step` bytes starting at `pointer + i*signed_step`. Forward (DF=0):
    //   range = [pointer, pointer + n*step)
    // Reverse (DF=1):
    //   range = [pointer - (n-1)*step, pointer + step)
    let n64 = n as i64;
    let step64 = step as i64;
    let (off_lo, off_hi) = if df_active {
        (ptr_value as i64 - (n64 - 1) * step64, ptr_value as i64 + step64)
    } else {
        (ptr_value as i64, ptr_value as i64 + n64 * step64)
    };
    if off_lo < 0 || off_hi > 0x10000 {
        return ApplyOutcome::Unsupported("pointer would wrap past 16-bit segment");
    }

    // Virtual-region overlap check: bulk writes through state.memory
    // can't substitute for cabinet-emitted dispatch over BIOS / ROM-disk
    // / packed-broadcast windows. The hardcoded path bails on this and
    // panics; the descriptor-driven applier returns Unsupported and
    // lets the caller decide.
    let seg_for_range = prepared[0].0; // all writes share the same seg in Fill
    let dst_lo_linear = seg_for_range + off_lo;
    let dst_hi_linear = seg_for_range + off_hi;
    if ranges_overlap_virtual(state, dst_lo_linear, dst_hi_linear - dst_lo_linear) {
        return ApplyOutcome::Unsupported(
            "destination range overlaps virtual region",
        );
    }

    // Hot loop. Two structural shapes:
    //   - Single write per iter (step=1): collapse the entire loop into
    //     one bulk_fill call (the hardcoded STOSB path's optimisation).
    //   - Multiple writes per iter: walk per-iteration, per-write. The
    //     hardcoded STOSW path also does this; bulk_fill can't be used
    //     because the bytes within an iteration are *different*.
    let iterations = n;
    if descriptor.writes.len() == 1 {
        let (seg_base, byte) = prepared[0];
        let dst_start = seg_base + off_lo;
        let dst_count = (off_hi - off_lo) as usize;
        bulk_fill(state, dst_start, dst_count, byte);
    } else {
        // Multi-write per iter (e.g. STOSW: low byte then high byte).
        // For each iteration, write each prepared byte at the
        // structurally-implied offset (position-in-vec). Direction only
        // affects the iteration step — within a single iteration the
        // bytes are written in the order the recogniser sorted them
        // (low-to-high in source order, which matches the hardcoded
        // STOSW path's `lo` / `hi` ordering).
        for i in 0..n64 {
            let iter_off = i * (signed_step as i64);
            let iter_base = (ptr_value as i64) + iter_off;
            for (k, (seg_base, byte)) in prepared.iter().enumerate() {
                let addr = seg_base + iter_base + k as i64;
                bulk_store_byte(state, addr, *byte);
            }
        }
    }

    ApplyOutcome::Applied { iterations }
}

/// Apply a `BulkClass::Copy` descriptor to state. The structural contract:
///
/// 1. Resolve counter, identify the destination and source pointers.
///    The destination pointer is the one named in every write entry's
///    `addr_decomposition.pointer_property`; the source pointer is the
///    one named in every write entry's `val_indirect_read.pointer_property`.
///    Both names must resolve to entries in `descriptor.pointers` so we
///    can find each pointer's `base_step`, `flag_property`, and `flag_bit`.
/// 2. Resolve segment slots: destination from `addr_decomposition.seg_property`,
///    source from `val_indirect_read.seg_property`. Both must be present
///    (`None` from the recogniser means the address shape was too messy
///    to decompose — bail to hardcoded path).
/// 3. For each iteration `i ∈ [0, n)`, for each write entry `k`:
///       byte = state.read_mem(src_seg*16 + src_ptr + signed_step*i + k)
///       bulk_store_byte(dst_seg*16 + dst_ptr + signed_step*i + k, byte)
///    The `signed_step` is the same for both pointers — MOVS-shape loops
///    advance both pointers by the same direction-flag-multiplied step,
///    and the descriptor's two pointer entries structurally describe
///    that (same flag_property, same flag_bit, same base_step). We
///    verify those structurally rather than assume.
///
/// Per-iter, per-write `state.read_mem` calls match the hardcoded path's
/// MOVS shape exactly — the fetch routes through packed cells, windowed
/// byte arrays, and the extended map without any of that knowledge
/// leaking up here. Overlap semantics are preserved by the
/// read-then-write iteration order: forward direction with overlapping
/// ranges (e.g. dst = src + 1) sees the source bytes after they've been
/// rewritten by earlier iterations, exactly as the hardcoded path
/// (and real x86 MOVS) does.
///
/// Cardinal-rule shape: like [`apply_fill`], the applier reads zero
/// property names as text. The `descriptor.pointers` lookup is
/// whole-name string equality (a HashMap lookup, not a substring
/// inspection), and the position-as-byte-offset within an iteration is
/// a structural fact about the write vector's order — the same as in
/// `apply_fill`.
///
/// Returns the iteration count so the caller can charge cycles.
pub(crate) fn apply_copy(
    descriptor: &LoopDescriptor,
    program: &CompiledProgram,
    state: &mut State,
    slots: &[i32],
) -> ApplyOutcome {
    if descriptor.bulk_class != BulkClass::Copy {
        return ApplyOutcome::Unsupported("not Copy class");
    }
    let Some(counter) = descriptor.counter.as_ref() else {
        return ApplyOutcome::Unsupported("no counter");
    };
    let Some(n) = read_prop(program, state, slots, &counter.property) else {
        return ApplyOutcome::Unsupported("counter unresolved");
    };
    if n <= 0 {
        return ApplyOutcome::Applied { iterations: 0 };
    }

    // A Copy needs exactly two pointers: a destination and a source.
    // Anything else is shape we don't recognise — bail.
    if descriptor.pointers.len() != 2 {
        return ApplyOutcome::Unsupported("Copy expects exactly two pointers");
    }
    if descriptor.writes.is_empty() {
        return ApplyOutcome::Unsupported("Copy expects at least one write");
    }

    // Identify destination and source pointers structurally from the
    // write entries. Every write must agree on both pointer names — a
    // canonical MOVS-shape loop has all writes targeting the same dst
    // and reading from the same src. Disagreement signals a shape we
    // don't yet handle.
    let first_dst_ptr = descriptor.writes[0]
        .addr_decomposition
        .as_ref()
        .map(|(_, p)| p.as_str());
    let first_src = descriptor.writes[0]
        .val_indirect_read
        .as_ref()
        .map(|ir| ir.pointer_property.as_str());
    let (Some(dst_ptr_name), Some(src_ptr_name)) = (first_dst_ptr, first_src) else {
        return ApplyOutcome::Unsupported(
            "Copy needs addr_decomposition + val_indirect_read on first write",
        );
    };
    if dst_ptr_name == src_ptr_name {
        // The dst and src must be different stepping pointers.
        return ApplyOutcome::Unsupported("Copy dst and src pointer names match");
    }
    for w in &descriptor.writes {
        let Some((_, p)) = w.addr_decomposition.as_ref() else {
            return ApplyOutcome::Unsupported("Copy write missing addr_decomposition");
        };
        if p != dst_ptr_name {
            return ApplyOutcome::Unsupported("Copy writes disagree on dst pointer");
        }
        let Some(ir) = w.val_indirect_read.as_ref() else {
            return ApplyOutcome::Unsupported("Copy write missing val_indirect_read");
        };
        if ir.pointer_property != src_ptr_name {
            return ApplyOutcome::Unsupported("Copy writes disagree on src pointer");
        }
    }

    // Resolve the two pointer entries by name. Whole-name equality —
    // identical to how `read_prop` resolves a slot.
    let dst_entry = descriptor
        .pointers
        .iter()
        .find(|p| p.self_property == dst_ptr_name);
    let src_entry = descriptor
        .pointers
        .iter()
        .find(|p| p.self_property == src_ptr_name);
    let (Some(dst_entry), Some(src_entry)) = (dst_entry, src_entry) else {
        return ApplyOutcome::Unsupported(
            "Copy pointer names not found in descriptor.pointers",
        );
    };

    // MOVS-shape: both pointers advance by the same signed step
    // (same direction-flag bit, same base_step). Structurally enforce
    // that — if the descriptor says the two pointers move
    // independently, the cabinet is doing something we don't yet
    // model and the hardcoded path should keep the wheel.
    if dst_entry.base_step != src_entry.base_step {
        return ApplyOutcome::Unsupported("Copy pointers have different base_step");
    }
    if dst_entry.flag_property != src_entry.flag_property
        || dst_entry.flag_bit != src_entry.flag_bit
    {
        return ApplyOutcome::Unsupported(
            "Copy pointers gated by different direction flags",
        );
    }
    let step: i32 = dst_entry.base_step;
    if descriptor.writes.len() != step as usize {
        return ApplyOutcome::Unsupported(
            "writes.len() != base_step (per-iter byte count mismatch)",
        );
    }

    // Resolve segment slots and pointer values. For Copy we need both
    // segments to be cleanly decomposed — `seg_property: None` from
    // the recogniser means the source address expression had a shape
    // we couldn't simplify, and per-iter raw-Expr evaluation isn't
    // wired up yet.
    let dst_seg_name = descriptor.writes[0]
        .addr_decomposition
        .as_ref()
        .map(|(s, _)| s.as_str())
        .unwrap();
    let src_seg_name_opt = descriptor.writes[0]
        .val_indirect_read
        .as_ref()
        .and_then(|ir| ir.seg_property.as_deref());
    let Some(src_seg_name) = src_seg_name_opt else {
        return ApplyOutcome::Unsupported(
            "Copy val_indirect_read has no seg decomposition",
        );
    };
    // Every write must agree on its decomposed segment slots too.
    for w in &descriptor.writes {
        let dst_seg = w.addr_decomposition.as_ref().map(|(s, _)| s.as_str()).unwrap();
        if dst_seg != dst_seg_name {
            return ApplyOutcome::Unsupported("Copy writes disagree on dst seg");
        }
        let src_seg = w
            .val_indirect_read
            .as_ref()
            .and_then(|ir| ir.seg_property.as_deref());
        if src_seg != Some(src_seg_name) {
            return ApplyOutcome::Unsupported("Copy writes disagree on src seg");
        }
    }
    let Some(dst_ptr_value) = read_prop(program, state, slots, dst_ptr_name) else {
        return ApplyOutcome::Unsupported("dst pointer slot unresolved");
    };
    let Some(src_ptr_value) = read_prop(program, state, slots, src_ptr_name) else {
        return ApplyOutcome::Unsupported("src pointer slot unresolved");
    };
    let Some(dst_seg_value) = read_prop(program, state, slots, dst_seg_name) else {
        return ApplyOutcome::Unsupported("dst seg slot unresolved");
    };
    let Some(src_seg_value) = read_prop(program, state, slots, src_seg_name) else {
        return ApplyOutcome::Unsupported("src seg slot unresolved");
    };
    let Some(flags) = read_prop(program, state, slots, &dst_entry.flag_property) else {
        return ApplyOutcome::Unsupported("flag_property unresolved");
    };
    let df_active = ((flags >> dst_entry.flag_bit) & 1) != 0;
    let signed_step: i32 = if df_active { -step } else { step };

    // Pointer-wrap check, both ranges (16-bit segment offset). Each
    // iteration touches `step` bytes at `ptr + i*signed_step` for
    // both src and dst.
    let n64 = n as i64;
    let step64 = step as i64;
    let bounds = |ptr: i32| -> (i64, i64) {
        if df_active {
            (ptr as i64 - (n64 - 1) * step64, ptr as i64 + step64)
        } else {
            (ptr as i64, ptr as i64 + n64 * step64)
        }
    };
    let (dst_lo, dst_hi) = bounds(dst_ptr_value);
    let (src_lo, src_hi) = bounds(src_ptr_value);
    if dst_lo < 0 || dst_hi > 0x10000 {
        return ApplyOutcome::Unsupported("dst pointer would wrap past 16-bit segment");
    }
    if src_lo < 0 || src_hi > 0x10000 {
        return ApplyOutcome::Unsupported("src pointer would wrap past 16-bit segment");
    }

    // Virtual-region overlap on the destination only — the source side
    // is read through `state.read_mem`, which transparently handles
    // packed cells / extended map / windowed byte arrays. The
    // destination is written through `bulk_store_byte` which routes
    // similarly, but the hardcoded path bails on virtual-region writes
    // (it has stricter assumptions about whether the bulk path matches
    // CSS-side writeback). Match its behaviour.
    let dst_seg_base = (dst_seg_value as i64) * 16;
    let dst_lo_linear = dst_seg_base + dst_lo;
    let dst_hi_linear = dst_seg_base + dst_hi;
    if ranges_overlap_virtual(state, dst_lo_linear, dst_hi_linear - dst_lo_linear) {
        return ApplyOutcome::Unsupported("destination range overlaps virtual region");
    }

    // Hot loop. Walk per-iter, per-write — same as the hardcoded MOVSW
    // path. For step=1 (MOVSB-shape) this is one read+write per iter;
    // for step=2 (MOVSW-shape) this is two read+writes per iter, in
    // position order (low byte at offset 0, high byte at offset 1).
    //
    // The read-then-write order within an iteration matters for
    // overlap: if dst overlaps src downstream of the iteration's
    // current position, an earlier iteration's write may overwrite a
    // future iteration's source. The hardcoded path walks per-byte in
    // hardware iteration order to preserve those semantics; this
    // applier does the same by reading and writing each byte through
    // `state.read_mem` / `bulk_store_byte` in the same order.
    let src_seg_base = (src_seg_value as i64) * 16;
    let dst_ptr_i64 = dst_ptr_value as i64;
    let src_ptr_i64 = src_ptr_value as i64;
    for i in 0..n64 {
        let iter_off = i * (signed_step as i64);
        let dst_iter_base = dst_seg_base + dst_ptr_i64 + iter_off;
        let src_iter_base = src_seg_base + src_ptr_i64 + iter_off;
        for k in 0..(descriptor.writes.len() as i64) {
            let s = src_iter_base + k;
            let d = dst_iter_base + k;
            let b = (state.read_mem(s as i32) & 0xFF) as u8;
            bulk_store_byte(state, d, b);
        }
    }

    ApplyOutcome::Applied { iterations: n }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pattern::loop_descriptor::{
        BulkClass, CounterEntry, IndirectRead, LoopDescriptor, PointerEntry, WriteEntry,
    };
    use crate::types::{CssValue, Expr, PropertyDef, PropertySyntax, StyleTest};

    /// Construct a minimal `CompiledProgram` whose only filled-in field
    /// is `loop_descriptors`. The applier reads `property_slots` too,
    /// but on cabinets that route registers through state-vars (the
    /// case for AL/AX/CX/DI/etc.) `property_slots` stays empty and the
    /// fallback `state.get_var` path takes over. That's the case all
    /// of these tests exercise.
    fn empty_program_with_descriptor(desc: LoopDescriptor) -> CompiledProgram {
        CompiledProgram {
            ops: Vec::new(),
            slot_count: 0,
            writeback: Vec::new(),
            broadcast_writes: Vec::new(),
            packed_broadcast_writes: Vec::new(),
            packed_cell_tables: Vec::new(),
            packed_exception_tables: Vec::new(),
            dispatch_tables: Vec::new(),
            chain_tables: Vec::new(),
            flat_dispatch_arrays: Vec::new(),
            property_slots: std::collections::HashMap::new(),
            functions: Vec::new(),
            windowed_byte_array: None,
            loop_descriptors: vec![desc],
        }
    }

    /// Allocate a 1 MiB state and load the named property as a state var.
    fn rigged_state(names: &[&str]) -> State {
        let mut state = State::new(1 << 20);
        let props: Vec<PropertyDef> = names
            .iter()
            .map(|n| PropertyDef {
                name: format!("--{}", n),
                syntax: PropertySyntax::Integer,
                inherits: false,
                initial_value: Some(CssValue::Integer(0)),
            })
            .collect();
        state.load_properties(&props);
        state
    }

    /// Build a STOSB-shape descriptor: one byte-step pointer (`--DI`),
    /// one write entry whose val_expr reads `--AL`. Counter is `--CX`.
    /// Address decomposition is `(--ES, --DI)`.
    fn stosb_descriptor() -> LoopDescriptor {
        LoopDescriptor {
            key_property: "--opcode".to_string(),
            key_value: 0xAA,
            ip_property: "--IP".to_string(),
            ip_self_property: "--IP_prev".to_string(),
            ip_advance_literal: 1,
            predicate_properties: vec!["--repContinue".to_string()],
            predicate: StyleTest::Single {
                property: "--repContinue".to_string(),
                value: Expr::Literal(1.0),
            },
            counter: Some(CounterEntry {
                property: "--CX".to_string(),
                self_property: "--CX_prev".to_string(),
                step: 1,
            }),
            pointers: vec![PointerEntry {
                property: "--DI".to_string(),
                self_property: "--DI".to_string(),
                base_step: 1,
                flag_property: "--flags".to_string(),
                flag_bit: 10,
            }],
            writes: vec![WriteEntry {
                addr_property: "--mAddr0".to_string(),
                val_property: "--mVal0".to_string(),
                addr_expr: Expr::Literal(0.0),
                val_expr: Expr::Var {
                    name: "--AL".to_string(),
                    fallback: None,
                },
                addr_decomposition: Some(("--ES".to_string(), "--DI".to_string())),
                val_indirect_read: None,
            }],
            flag_conditioned: false,
            bulk_class: BulkClass::Fill,
        }
    }

    /// Build a STOSW-shape descriptor: one word-step pointer, two write
    /// entries, low byte then high byte.
    fn stosw_descriptor() -> LoopDescriptor {
        LoopDescriptor {
            key_property: "--opcode".to_string(),
            key_value: 0xAB,
            ip_property: "--IP".to_string(),
            ip_self_property: "--IP_prev".to_string(),
            ip_advance_literal: 1,
            predicate_properties: vec!["--repContinue".to_string()],
            predicate: StyleTest::Single {
                property: "--repContinue".to_string(),
                value: Expr::Literal(1.0),
            },
            counter: Some(CounterEntry {
                property: "--CX".to_string(),
                self_property: "--CX_prev".to_string(),
                step: 1,
            }),
            pointers: vec![PointerEntry {
                property: "--DI".to_string(),
                self_property: "--DI".to_string(),
                base_step: 2,
                flag_property: "--flags".to_string(),
                flag_bit: 10,
            }],
            writes: vec![
                WriteEntry {
                    addr_property: "--mAddr0".to_string(),
                    val_property: "--mVal0".to_string(),
                    addr_expr: Expr::Literal(0.0),
                    val_expr: Expr::Var {
                        name: "--AL".to_string(),
                        fallback: None,
                    },
                    addr_decomposition: Some(("--ES".to_string(), "--DI".to_string())),
                    val_indirect_read: None,
                },
                WriteEntry {
                    addr_property: "--mAddr1".to_string(),
                    val_property: "--mVal1".to_string(),
                    addr_expr: Expr::Literal(0.0),
                    val_expr: Expr::Var {
                        name: "--AH".to_string(),
                        fallback: None,
                    },
                    addr_decomposition: Some(("--ES".to_string(), "--DI".to_string())),
                    val_indirect_read: None,
                },
            ],
            flag_conditioned: false,
            bulk_class: BulkClass::Fill,
        }
    }

    /// Byte fill, forward direction: 64 bytes of 0x42 starting at
    /// ES:DI = 0xA000:0x10. Verifies the bulk-fill fast path (single
    /// write per iter collapses to one `bulk_fill` call).
    #[test]
    fn fill_byte_forward_writes_n_copies() {
        let desc = stosb_descriptor();
        let prog = empty_program_with_descriptor(desc.clone());
        let mut state = rigged_state(&["CX", "DI", "ES", "AL", "flags"]);
        state.set_var("CX", 64);
        state.set_var("DI", 0x10);
        state.set_var("ES", 0xA000);
        state.set_var("AL", 0x42);
        state.set_var("flags", 0); // DF=0

        let outcome = apply_fill(&desc, &prog, &mut state, &[]);
        assert_eq!(outcome, ApplyOutcome::Applied { iterations: 64 });

        for i in 0..64 {
            assert_eq!(
                state.memory[0xA0010 + i],
                0x42,
                "byte {} should be 0x42",
                i
            );
        }
        // Boundaries untouched.
        assert_eq!(state.memory[0xA000F], 0);
        assert_eq!(state.memory[0xA0050], 0);
    }

    /// Byte fill, reverse direction (DF=1): 16 bytes of 0xAA starting
    /// at DI=0x20 walking *backward*. End-state should fill
    /// [0x11..0x21) (DI inclusive, plus 15 lower addresses).
    #[test]
    fn fill_byte_reverse_writes_lower_addresses() {
        let desc = stosb_descriptor();
        let prog = empty_program_with_descriptor(desc.clone());
        let mut state = rigged_state(&["CX", "DI", "ES", "AL", "flags"]);
        state.set_var("CX", 16);
        state.set_var("DI", 0x20);
        state.set_var("ES", 0xA000);
        state.set_var("AL", 0xAA);
        state.set_var("flags", 1 << 10); // DF=1

        let outcome = apply_fill(&desc, &prog, &mut state, &[]);
        assert_eq!(outcome, ApplyOutcome::Applied { iterations: 16 });

        // 0x11..=0x20 inclusive (16 bytes ending at DI=0x20 inclusive).
        for i in 0x11..=0x20 {
            assert_eq!(
                state.memory[0xA0000 + i],
                0xAA,
                "byte {:#x} should be 0xAA",
                i
            );
        }
        // Boundaries untouched.
        assert_eq!(state.memory[0xA0010], 0);
        assert_eq!(state.memory[0xA0021], 0);
    }

    /// Word fill, forward: AX=0xBEEF, 4 iterations starting at DI=0.
    /// Byte order is low-then-high per iteration: EF BE EF BE EF BE EF BE.
    #[test]
    fn fill_word_forward_writes_low_then_high() {
        let desc = stosw_descriptor();
        let prog = empty_program_with_descriptor(desc.clone());
        let mut state = rigged_state(&["CX", "DI", "ES", "AL", "AH", "flags"]);
        state.set_var("CX", 4);
        state.set_var("DI", 0);
        state.set_var("ES", 0xA000);
        state.set_var("AL", 0xEF);
        state.set_var("AH", 0xBE);
        state.set_var("flags", 0);

        let outcome = apply_fill(&desc, &prog, &mut state, &[]);
        assert_eq!(outcome, ApplyOutcome::Applied { iterations: 4 });

        let want: [u8; 8] = [0xEF, 0xBE, 0xEF, 0xBE, 0xEF, 0xBE, 0xEF, 0xBE];
        for (i, b) in want.iter().enumerate() {
            assert_eq!(state.memory[0xA0000 + i], *b, "word-fill byte {}", i);
        }
        assert_eq!(state.memory[0xA0008], 0);
    }

    /// Word fill, reverse direction: 3 iterations starting at DI=0x10
    /// walking backward by 2.
    #[test]
    fn fill_word_reverse_walks_backward() {
        let desc = stosw_descriptor();
        let prog = empty_program_with_descriptor(desc.clone());
        let mut state = rigged_state(&["CX", "DI", "ES", "AL", "AH", "flags"]);
        state.set_var("CX", 3);
        state.set_var("DI", 0x10);
        state.set_var("ES", 0xA000);
        state.set_var("AL", 0x34);
        state.set_var("AH", 0x12);
        state.set_var("flags", 1 << 10); // DF=1

        let outcome = apply_fill(&desc, &prog, &mut state, &[]);
        assert_eq!(outcome, ApplyOutcome::Applied { iterations: 3 });

        // Iteration 0 writes (0x34, 0x12) at DI=0x10..0x11.
        // Iteration 1 writes at DI-2=0x0E..0x0F.
        // Iteration 2 writes at DI-4=0x0C..0x0D.
        for &(addr_off, want) in &[
            (0x10usize, 0x34u8), (0x11, 0x12),
            (0x0E, 0x34), (0x0F, 0x12),
            (0x0C, 0x34), (0x0D, 0x12),
        ] {
            assert_eq!(
                state.memory[0xA0000 + addr_off],
                want,
                "addr {:#x}",
                addr_off
            );
        }
        // Boundaries untouched.
        assert_eq!(state.memory[0xA000B], 0);
        assert_eq!(state.memory[0xA0012], 0);
    }

    /// Counter already zero: applier returns `Applied { iterations: 0 }`
    /// without touching memory. The hardcoded path bails before calling
    /// us (cx<=0), but the dual harness's contract is that an early-
    /// reached applier behaves identically to the hardcoded skip.
    #[test]
    fn fill_with_zero_counter_is_noop() {
        let desc = stosb_descriptor();
        let prog = empty_program_with_descriptor(desc.clone());
        let mut state = rigged_state(&["CX", "DI", "ES", "AL", "flags"]);
        state.set_var("CX", 0);
        state.set_var("DI", 0x100);
        state.set_var("ES", 0x1000);
        state.set_var("AL", 0x55);
        state.set_var("flags", 0);

        // Pre-fill an arbitrary byte so we can verify it stays.
        state.memory[0x10100] = 0x77;
        let outcome = apply_fill(&desc, &prog, &mut state, &[]);
        assert_eq!(outcome, ApplyOutcome::Applied { iterations: 0 });
        assert_eq!(state.memory[0x10100], 0x77, "memory must be untouched");
    }

    /// Negative counter: same noop semantics as zero. (CSS shouldn't
    /// produce this in practice; it's a defensive check.)
    #[test]
    fn fill_with_negative_counter_is_noop() {
        let desc = stosb_descriptor();
        let prog = empty_program_with_descriptor(desc.clone());
        let mut state = rigged_state(&["CX", "DI", "ES", "AL", "flags"]);
        state.set_var("CX", -1);
        state.set_var("DI", 0x100);
        state.set_var("ES", 0x1000);
        state.set_var("AL", 0x55);
        state.set_var("flags", 0);

        let outcome = apply_fill(&desc, &prog, &mut state, &[]);
        assert_eq!(outcome, ApplyOutcome::Applied { iterations: 0 });
    }

    /// The applier refuses non-`Fill` classes — Step 4 owns Fill only.
    #[test]
    fn refuses_non_fill_class() {
        let mut desc = stosb_descriptor();
        desc.bulk_class = BulkClass::Copy;
        let prog = empty_program_with_descriptor(desc.clone());
        let mut state = rigged_state(&["CX", "DI", "ES", "AL", "flags"]);
        state.set_var("CX", 1);

        let outcome = apply_fill(&desc, &prog, &mut state, &[]);
        assert!(matches!(outcome, ApplyOutcome::Unsupported(_)));
    }

    /// Missing `addr_decomposition` on a write: applier bails so the
    /// caller can fall back to the hardcoded path. (This covers
    /// pathological cabinets whose addr expressions don't match the
    /// canonical seg*16+ptr shape; doom8088 always decomposes cleanly.)
    #[test]
    fn refuses_when_addr_decomposition_missing() {
        let mut desc = stosb_descriptor();
        desc.writes[0].addr_decomposition = None;
        let prog = empty_program_with_descriptor(desc.clone());
        let mut state = rigged_state(&["CX", "DI", "ES", "AL", "flags"]);
        state.set_var("CX", 4);
        state.set_var("DI", 0);
        state.set_var("ES", 0x1000);
        state.set_var("AL", 0x55);

        let outcome = apply_fill(&desc, &prog, &mut state, &[]);
        assert!(matches!(outcome, ApplyOutcome::Unsupported(_)));
    }

    /// `val_expr` is something other than a bare Var: applier bails. A
    /// future step extends `resolve_fill_value`; until then the
    /// hardcoded path still handles these shapes.
    #[test]
    fn refuses_when_val_expr_is_not_bare_var() {
        let mut desc = stosb_descriptor();
        // Pretend the cabinet emitted `(--AL & 0xFF)` (a Calc shape) as
        // the value-side. We don't yet structurally decompose the
        // byte-extract idiom; bail.
        use crate::types::CalcOp;
        desc.writes[0].val_expr = Expr::Calc(CalcOp::Add(
            Box::new(Expr::Var { name: "--AL".to_string(), fallback: None }),
            Box::new(Expr::Literal(0.0)),
        ));
        let prog = empty_program_with_descriptor(desc.clone());
        let mut state = rigged_state(&["CX", "DI", "ES", "AL", "flags"]);
        state.set_var("CX", 4);
        state.set_var("DI", 0);
        state.set_var("ES", 0x1000);
        state.set_var("AL", 0x55);

        let outcome = apply_fill(&desc, &prog, &mut state, &[]);
        assert!(matches!(outcome, ApplyOutcome::Unsupported(_)));
    }

    /// Pointer wrap past 16-bit segment: applier bails. (Hardcoded path
    /// panics in this case; we choose Unsupported so the caller can
    /// keep its panic semantics if desired.)
    #[test]
    fn refuses_when_pointer_would_wrap() {
        let desc = stosb_descriptor();
        let prog = empty_program_with_descriptor(desc.clone());
        let mut state = rigged_state(&["CX", "DI", "ES", "AL", "flags"]);
        state.set_var("CX", 0x100);
        state.set_var("DI", 0xFF80); // n=256 from DI=0xFF80 wraps past 0xFFFF
        state.set_var("ES", 0xA000);
        state.set_var("AL", 0x42);

        let outcome = apply_fill(&desc, &prog, &mut state, &[]);
        assert!(matches!(outcome, ApplyOutcome::Unsupported(_)));
    }

    /// Position-as-byte-offset is the structural assumption that lets
    /// the multi-write path work without an explicit per-write byte
    /// offset in the descriptor. This test verifies it: if we ever
    /// reorder `writes` (low/high swap), the output flips byte-order.
    /// That's the structural contract — any cabinet whose recogniser
    /// emits writes in source order gets correct results; any whose
    /// recogniser shuffles them gets observable wrongness, surfaced
    /// here.
    #[test]
    fn write_order_determines_byte_order_in_iteration() {
        let mut desc = stosw_descriptor();
        // Swap the writes so high comes first.
        desc.writes.swap(0, 1);
        let prog = empty_program_with_descriptor(desc.clone());
        let mut state = rigged_state(&["CX", "DI", "ES", "AL", "AH", "flags"]);
        state.set_var("CX", 1);
        state.set_var("DI", 0);
        state.set_var("ES", 0xA000);
        state.set_var("AL", 0xEF);
        state.set_var("AH", 0xBE);
        state.set_var("flags", 0);

        let outcome = apply_fill(&desc, &prog, &mut state, &[]);
        assert_eq!(outcome, ApplyOutcome::Applied { iterations: 1 });
        // Swapped order → BE then EF.
        assert_eq!(state.memory[0xA0000], 0xBE);
        assert_eq!(state.memory[0xA0001], 0xEF);
    }

    /// Genericity probe: a descriptor with completely unrelated property
    /// names (no `--AL`, no `--DI`, no `--CX`) should produce
    /// identical mutations as long as the structural shape is the
    /// same. Verifies the applier reads no name as text.
    #[test]
    fn genericity_unrelated_names_same_shape_same_output() {
        let desc = LoopDescriptor {
            key_property: "--opaque_key".to_string(),
            key_value: 0x77, // arbitrary
            ip_property: "--ip_x".to_string(),
            ip_self_property: "--ip_x_prev".to_string(),
            ip_advance_literal: 1,
            predicate_properties: vec!["--cont_x".to_string()],
            predicate: StyleTest::Single {
                property: "--cont_x".to_string(),
                value: Expr::Literal(1.0),
            },
            counter: Some(CounterEntry {
                property: "--alpha".to_string(),
                self_property: "--alpha_prev".to_string(),
                step: 1,
            }),
            pointers: vec![PointerEntry {
                property: "--beta".to_string(),
                self_property: "--beta".to_string(),
                base_step: 1,
                flag_property: "--gamma".to_string(),
                flag_bit: 3, // arbitrary bit
            }],
            writes: vec![WriteEntry {
                addr_property: "--w_addr".to_string(),
                val_property: "--w_val".to_string(),
                addr_expr: Expr::Literal(0.0),
                val_expr: Expr::Var {
                    name: "--delta".to_string(),
                    fallback: None,
                },
                addr_decomposition: Some(("--epsilon".to_string(), "--beta".to_string())),
                val_indirect_read: None,
            }],
            flag_conditioned: false,
            bulk_class: BulkClass::Fill,
        };
        let prog = empty_program_with_descriptor(desc.clone());
        let mut state = rigged_state(&["alpha", "beta", "epsilon", "delta", "gamma"]);
        state.set_var("alpha", 8);
        state.set_var("beta", 0x40);
        state.set_var("epsilon", 0x1234);
        state.set_var("delta", 0x99);
        state.set_var("gamma", 0); // bit 3 clear → forward

        let outcome = apply_fill(&desc, &prog, &mut state, &[]);
        assert_eq!(outcome, ApplyOutcome::Applied { iterations: 8 });
        let base = (0x1234usize) * 16 + 0x40;
        for i in 0..8 {
            assert_eq!(state.memory[base + i], 0x99, "byte {}", i);
        }
        assert_eq!(state.memory[base + 8], 0);
    }

    /// The `IndirectRead` import is here to keep the test module's
    /// type-imports tidy; if the type were unused the compiler would
    /// warn. This bare-touch keeps it referenced without inflating any
    /// test's surface.
    #[test]
    fn indirect_read_type_referenced() {
        let _: Option<IndirectRead> = None;
    }

    /// Dual-mode equivalence: the applier's memory mutations must match
    /// what an equivalent direct call to the bulk primitives would
    /// produce. Drives the same setup through both paths on independent
    /// State clones and asserts memory equality. Smallest possible
    /// assertion of the harness's actual contract.
    ///
    /// STOSB shape: 200 bytes of 0x5A starting at ES:DI = 0x1234:0x80,
    /// forward direction.
    #[test]
    fn equivalence_byte_fill_matches_direct_bulk_fill() {
        let desc = stosb_descriptor();
        let prog = empty_program_with_descriptor(desc.clone());
        let names = &["CX", "DI", "ES", "AL", "flags"];
        let mut state_a = rigged_state(names);
        let mut state_b = rigged_state(names);
        for s in [&mut state_a, &mut state_b] {
            s.set_var("CX", 200);
            s.set_var("DI", 0x80);
            s.set_var("ES", 0x1234);
            s.set_var("AL", 0x5A);
            s.set_var("flags", 0);
        }

        // Path A: descriptor-driven applier.
        let outcome = apply_fill(&desc, &prog, &mut state_a, &[]);
        assert_eq!(outcome, ApplyOutcome::Applied { iterations: 200 });

        // Path B: direct bulk_fill, mirroring what compile.rs does for
        // STOSB. The inputs are the same; only the path through the
        // code differs.
        let es = 0x1234i64;
        let di = 0x80i64;
        let n = 200usize;
        bulk_fill(&mut state_b, es * 16 + di, n, 0x5A);

        // Memory must be byte-for-byte identical. (state_vars differ by
        // design — applier doesn't update them in step 4.)
        assert_eq!(
            state_a.memory, state_b.memory,
            "applier and direct-bulk_fill should produce identical memory"
        );
    }

    /// Same equivalence check, STOSW shape: 100 words of 0xCAFE
    /// starting at ES:DI = 0xB000:0x300, reverse direction (DF=1). DI
    /// is chosen large enough that 100 backward iterations stay in
    /// the segment.
    #[test]
    fn equivalence_word_fill_matches_direct_per_byte_writes() {
        let desc = stosw_descriptor();
        let prog = empty_program_with_descriptor(desc.clone());
        let names = &["CX", "DI", "ES", "AL", "AH", "flags"];
        let mut state_a = rigged_state(names);
        let mut state_b = rigged_state(names);
        for s in [&mut state_a, &mut state_b] {
            s.set_var("CX", 100);
            s.set_var("DI", 0x300);
            s.set_var("ES", 0xB000);
            s.set_var("AL", 0xFE);
            s.set_var("AH", 0xCA);
            s.set_var("flags", 1 << 10); // DF=1
        }

        // Path A: applier.
        let outcome = apply_fill(&desc, &prog, &mut state_a, &[]);
        assert_eq!(outcome, ApplyOutcome::Applied { iterations: 100 });

        // Path B: mirror compile.rs's STOSW reverse loop directly.
        let es = 0xB000i64;
        let di = 0x300i64;
        let lo = 0xFEu8;
        let hi = 0xCAu8;
        let n = 100i64;
        for k in 0..n {
            let off = -k * 2; // DF=1
            let base = es * 16 + di + off;
            bulk_store_byte(&mut state_b, base, lo);
            bulk_store_byte(&mut state_b, base + 1, hi);
        }

        assert_eq!(
            state_a.memory, state_b.memory,
            "applier word-fill must match direct per-byte writes byte-for-byte"
        );
    }

    // -----------------------------------------------------------------
    // Step 5 — apply_copy (BulkClass::Copy / MOVS-shape) tests.
    // -----------------------------------------------------------------

    /// Build a MOVSB-shape descriptor: two byte-step pointers, one write
    /// entry. The write's addr_decomposition resolves to (ES, DI); its
    /// val_indirect_read resolves to (DS, SI) via a derived intermediate
    /// slot named `--_strSrcByte`.
    fn movsb_descriptor() -> LoopDescriptor {
        LoopDescriptor {
            key_property: "--opcode".to_string(),
            key_value: 0xA4,
            ip_property: "--IP".to_string(),
            ip_self_property: "--IP_prev".to_string(),
            ip_advance_literal: 1,
            predicate_properties: vec!["--repContinue".to_string()],
            predicate: StyleTest::Single {
                property: "--repContinue".to_string(),
                value: Expr::Literal(1.0),
            },
            counter: Some(CounterEntry {
                property: "--CX".to_string(),
                self_property: "--CX_prev".to_string(),
                step: 1,
            }),
            pointers: vec![
                PointerEntry {
                    property: "--DI".to_string(),
                    self_property: "--DI".to_string(),
                    base_step: 1,
                    flag_property: "--flags".to_string(),
                    flag_bit: 10,
                },
                PointerEntry {
                    property: "--SI".to_string(),
                    self_property: "--SI".to_string(),
                    base_step: 1,
                    flag_property: "--flags".to_string(),
                    flag_bit: 10,
                },
            ],
            writes: vec![WriteEntry {
                addr_property: "--mAddr0".to_string(),
                val_property: "--mVal0".to_string(),
                addr_expr: Expr::Literal(0.0),
                val_expr: Expr::Var {
                    name: "--_strSrcByte".to_string(),
                    fallback: None,
                },
                addr_decomposition: Some(("--ES".to_string(), "--DI".to_string())),
                val_indirect_read: Some(IndirectRead {
                    seg_property: Some("--DS".to_string()),
                    pointer_property: "--SI".to_string(),
                    intermediate_property: "--_strSrcByte".to_string(),
                }),
            }],
            flag_conditioned: false,
            bulk_class: BulkClass::Copy,
        }
    }

    /// MOVSW-shape descriptor: two word-step pointers, two write entries
    /// (low byte at offset 0, high byte at offset 1).
    fn movsw_descriptor() -> LoopDescriptor {
        LoopDescriptor {
            key_property: "--opcode".to_string(),
            key_value: 0xA5,
            ip_property: "--IP".to_string(),
            ip_self_property: "--IP_prev".to_string(),
            ip_advance_literal: 1,
            predicate_properties: vec!["--repContinue".to_string()],
            predicate: StyleTest::Single {
                property: "--repContinue".to_string(),
                value: Expr::Literal(1.0),
            },
            counter: Some(CounterEntry {
                property: "--CX".to_string(),
                self_property: "--CX_prev".to_string(),
                step: 1,
            }),
            pointers: vec![
                PointerEntry {
                    property: "--DI".to_string(),
                    self_property: "--DI".to_string(),
                    base_step: 2,
                    flag_property: "--flags".to_string(),
                    flag_bit: 10,
                },
                PointerEntry {
                    property: "--SI".to_string(),
                    self_property: "--SI".to_string(),
                    base_step: 2,
                    flag_property: "--flags".to_string(),
                    flag_bit: 10,
                },
            ],
            writes: vec![
                WriteEntry {
                    addr_property: "--mAddr0".to_string(),
                    val_property: "--mVal0".to_string(),
                    addr_expr: Expr::Literal(0.0),
                    val_expr: Expr::Var {
                        name: "--_strSrcByteLo".to_string(),
                        fallback: None,
                    },
                    addr_decomposition: Some(("--ES".to_string(), "--DI".to_string())),
                    val_indirect_read: Some(IndirectRead {
                        seg_property: Some("--DS".to_string()),
                        pointer_property: "--SI".to_string(),
                        intermediate_property: "--_strSrcByteLo".to_string(),
                    }),
                },
                WriteEntry {
                    addr_property: "--mAddr1".to_string(),
                    val_property: "--mVal1".to_string(),
                    addr_expr: Expr::Literal(0.0),
                    val_expr: Expr::Var {
                        name: "--_strSrcByteHi".to_string(),
                        fallback: None,
                    },
                    addr_decomposition: Some(("--ES".to_string(), "--DI".to_string())),
                    val_indirect_read: Some(IndirectRead {
                        seg_property: Some("--DS".to_string()),
                        pointer_property: "--SI".to_string(),
                        intermediate_property: "--_strSrcByteHi".to_string(),
                    }),
                },
            ],
            flag_conditioned: false,
            bulk_class: BulkClass::Copy,
        }
    }

    /// Helper to seed source bytes at DS:SI for MOVS-shape tests.
    fn seed_bytes(state: &mut State, base_linear: usize, bytes: &[u8]) {
        for (i, &b) in bytes.iter().enumerate() {
            state.memory[base_linear + i] = b;
        }
    }

    /// Byte copy, forward direction: copy 8 bytes from DS:SI = 0x1000:0x40
    /// to ES:DI = 0x2000:0x80. Source range is non-overlapping with dest.
    #[test]
    fn copy_byte_forward() {
        let desc = movsb_descriptor();
        let prog = empty_program_with_descriptor(desc.clone());
        let mut state = rigged_state(&["CX", "DI", "SI", "ES", "DS", "flags"]);
        state.set_var("CX", 8);
        state.set_var("DI", 0x80);
        state.set_var("SI", 0x40);
        state.set_var("ES", 0x2000);
        state.set_var("DS", 0x1000);
        state.set_var("flags", 0); // DF=0
        let src_base = 0x1000 * 16 + 0x40;
        let dst_base = 0x2000 * 16 + 0x80;
        seed_bytes(&mut state, src_base, &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]);

        let outcome = apply_copy(&desc, &prog, &mut state, &[]);
        assert_eq!(outcome, ApplyOutcome::Applied { iterations: 8 });

        for (i, &b) in [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88].iter().enumerate() {
            assert_eq!(state.memory[dst_base + i], b, "dst byte {}", i);
        }
        // Source untouched (no overlap).
        assert_eq!(state.memory[src_base], 0x11);
        assert_eq!(state.memory[src_base + 7], 0x88);
        // Boundaries.
        assert_eq!(state.memory[dst_base + 8], 0);
    }

    /// Byte copy, reverse direction (DF=1): SI=0x47, DI=0x87, n=8. Both
    /// pointers walk backward by 1 each iteration.
    #[test]
    fn copy_byte_reverse() {
        let desc = movsb_descriptor();
        let prog = empty_program_with_descriptor(desc.clone());
        let mut state = rigged_state(&["CX", "DI", "SI", "ES", "DS", "flags"]);
        state.set_var("CX", 8);
        state.set_var("DI", 0x87);
        state.set_var("SI", 0x47);
        state.set_var("ES", 0x2000);
        state.set_var("DS", 0x1000);
        state.set_var("flags", 1 << 10); // DF=1
        let src_base = 0x1000 * 16 + 0x40;
        let dst_base = 0x2000 * 16 + 0x80;
        // Source bytes seeded across [0x40..0x48). Iter 0 reads from
        // 0x47, iter 1 from 0x46, ... iter 7 from 0x40. Same for dst.
        seed_bytes(&mut state, src_base, &[0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x10, 0x20]);

        let outcome = apply_copy(&desc, &prog, &mut state, &[]);
        assert_eq!(outcome, ApplyOutcome::Applied { iterations: 8 });

        // Destination bytes at [0x80..0x88) should be the same source
        // bytes — reverse iteration on parallel pointers preserves byte
        // ordering between equivalent positions.
        for (i, &b) in [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x10, 0x20].iter().enumerate() {
            assert_eq!(state.memory[dst_base + i], b, "dst byte {}", i);
        }
    }

    /// Word copy, forward: copy 4 words from DS:SI to ES:DI.
    #[test]
    fn copy_word_forward() {
        let desc = movsw_descriptor();
        let prog = empty_program_with_descriptor(desc.clone());
        let mut state = rigged_state(&["CX", "DI", "SI", "ES", "DS", "flags"]);
        state.set_var("CX", 4);
        state.set_var("DI", 0x100);
        state.set_var("SI", 0x80);
        state.set_var("ES", 0x3000);
        state.set_var("DS", 0x1000);
        state.set_var("flags", 0);
        let src_base = 0x1000 * 16 + 0x80;
        let dst_base = 0x3000 * 16 + 0x100;
        seed_bytes(
            &mut state,
            src_base,
            &[0xCE, 0xFA, 0xEF, 0xBE, 0x0D, 0xF0, 0xAD, 0xDE],
        );

        let outcome = apply_copy(&desc, &prog, &mut state, &[]);
        assert_eq!(outcome, ApplyOutcome::Applied { iterations: 4 });

        for (i, &b) in [0xCE, 0xFA, 0xEF, 0xBE, 0x0D, 0xF0, 0xAD, 0xDE].iter().enumerate() {
            assert_eq!(state.memory[dst_base + i], b, "dst byte {}", i);
        }
        assert_eq!(state.memory[dst_base + 8], 0);
    }

    /// Word copy, reverse: SI / DI start at the *end* of the data and
    /// walk backward by 2.
    #[test]
    fn copy_word_reverse() {
        let desc = movsw_descriptor();
        let prog = empty_program_with_descriptor(desc.clone());
        let mut state = rigged_state(&["CX", "DI", "SI", "ES", "DS", "flags"]);
        state.set_var("CX", 3);
        state.set_var("DI", 0x110);
        state.set_var("SI", 0x90);
        state.set_var("ES", 0x3000);
        state.set_var("DS", 0x1000);
        state.set_var("flags", 1 << 10); // DF=1
        let src_base = 0x1000 * 16;
        let dst_base = 0x3000 * 16;
        // Iter 0: SI=0x90 → reads two bytes at [0x90, 0x91].
        // Iter 1: SI=0x8E → reads at [0x8E, 0x8F].
        // Iter 2: SI=0x8C → reads at [0x8C, 0x8D].
        // Same offsets for dst around 0x110.
        seed_bytes(&mut state, src_base + 0x8C, &[0x10, 0x11, 0x12, 0x13, 0x14, 0x15]);

        let outcome = apply_copy(&desc, &prog, &mut state, &[]);
        assert_eq!(outcome, ApplyOutcome::Applied { iterations: 3 });

        // dst[0x110..0x112) = src[0x90..0x92) = [0x14, 0x15]
        // dst[0x10E..0x110) = src[0x8E..0x90) = [0x12, 0x13]
        // dst[0x10C..0x10E) = src[0x8C..0x8E) = [0x10, 0x11]
        let want: &[(usize, u8)] = &[
            (0x110, 0x14), (0x111, 0x15),
            (0x10E, 0x12), (0x10F, 0x13),
            (0x10C, 0x10), (0x10D, 0x11),
        ];
        for &(off, b) in want {
            assert_eq!(state.memory[dst_base + off], b, "addr {:#x}", off);
        }
        assert_eq!(state.memory[dst_base + 0x10B], 0);
        assert_eq!(state.memory[dst_base + 0x112], 0);
    }

    /// Counter already zero: applier returns `Applied { iterations: 0 }`
    /// without touching memory.
    #[test]
    fn copy_with_zero_counter_is_noop() {
        let desc = movsb_descriptor();
        let prog = empty_program_with_descriptor(desc.clone());
        let mut state = rigged_state(&["CX", "DI", "SI", "ES", "DS", "flags"]);
        state.set_var("CX", 0);
        state.set_var("DI", 0x80);
        state.set_var("SI", 0x40);
        state.set_var("ES", 0x2000);
        state.set_var("DS", 0x1000);
        state.set_var("flags", 0);
        state.memory[0x1000 * 16 + 0x40] = 0xAB;
        state.memory[0x2000 * 16 + 0x80] = 0xCD; // sentinel, must not be overwritten

        let outcome = apply_copy(&desc, &prog, &mut state, &[]);
        assert_eq!(outcome, ApplyOutcome::Applied { iterations: 0 });
        assert_eq!(state.memory[0x2000 * 16 + 0x80], 0xCD);
    }

    /// Overlapping ranges, forward: dst = src + 1, n=4. The hardcoded
    /// path's per-byte-in-iteration-order semantics produce a byte-fill
    /// effect (every dst byte ends up equal to the original src[0]).
    /// The applier must match this byte-for-byte.
    ///
    /// src = [0x11, 0x22, 0x33, 0x44] at offset 0x100..0x104
    /// dst starts at 0x101 (one byte forward of src)
    /// Iter 0: read[0x100]=0x11, write[0x101]=0x11 → src now [0x11, 0x11, 0x33, 0x44]
    /// Iter 1: read[0x101]=0x11, write[0x102]=0x11 → src now [0x11, 0x11, 0x11, 0x44]
    /// Iter 2: read[0x102]=0x11, write[0x103]=0x11 → src now [0x11, 0x11, 0x11, 0x11]
    /// Iter 3: read[0x103]=0x11, write[0x104]=0x11
    /// Final: [0x11, 0x11, 0x11, 0x11, 0x11] at 0x100..0x105
    #[test]
    fn copy_overlapping_forward_propagates_first_byte() {
        let desc = movsb_descriptor();
        let prog = empty_program_with_descriptor(desc.clone());
        // Use the same segment so the overlap is real.
        let mut state = rigged_state(&["CX", "DI", "SI", "ES", "DS", "flags"]);
        state.set_var("CX", 4);
        state.set_var("DI", 0x101);
        state.set_var("SI", 0x100);
        state.set_var("ES", 0x1000);
        state.set_var("DS", 0x1000);
        state.set_var("flags", 0); // DF=0 forward
        let base = 0x1000 * 16;
        seed_bytes(&mut state, base + 0x100, &[0x11, 0x22, 0x33, 0x44]);

        let outcome = apply_copy(&desc, &prog, &mut state, &[]);
        assert_eq!(outcome, ApplyOutcome::Applied { iterations: 4 });

        // Same pattern as the hardcoded path's per-byte forward walk:
        // first byte propagates through the destination.
        assert_eq!(state.memory[base + 0x100], 0x11); // src[0] unchanged
        assert_eq!(state.memory[base + 0x101], 0x11);
        assert_eq!(state.memory[base + 0x102], 0x11);
        assert_eq!(state.memory[base + 0x103], 0x11);
        assert_eq!(state.memory[base + 0x104], 0x11);
        assert_eq!(state.memory[base + 0x105], 0); // boundary
    }

    /// Refusal paths.
    #[test]
    fn copy_refuses_non_copy_class() {
        let mut desc = movsb_descriptor();
        desc.bulk_class = BulkClass::Fill;
        let prog = empty_program_with_descriptor(desc.clone());
        let mut state = rigged_state(&["CX", "DI", "SI", "ES", "DS", "flags"]);
        state.set_var("CX", 1);
        let outcome = apply_copy(&desc, &prog, &mut state, &[]);
        assert!(matches!(outcome, ApplyOutcome::Unsupported(_)));
    }

    #[test]
    fn copy_refuses_when_only_one_pointer() {
        let mut desc = movsb_descriptor();
        desc.pointers.pop();
        let prog = empty_program_with_descriptor(desc.clone());
        let mut state = rigged_state(&["CX", "DI", "SI", "ES", "DS", "flags"]);
        state.set_var("CX", 1);
        let outcome = apply_copy(&desc, &prog, &mut state, &[]);
        assert!(matches!(outcome, ApplyOutcome::Unsupported(_)));
    }

    #[test]
    fn copy_refuses_when_val_indirect_read_missing() {
        let mut desc = movsb_descriptor();
        desc.writes[0].val_indirect_read = None;
        let prog = empty_program_with_descriptor(desc.clone());
        let mut state = rigged_state(&["CX", "DI", "SI", "ES", "DS", "flags"]);
        state.set_var("CX", 1);
        let outcome = apply_copy(&desc, &prog, &mut state, &[]);
        assert!(matches!(outcome, ApplyOutcome::Unsupported(_)));
    }

    #[test]
    fn copy_refuses_when_indirect_read_seg_missing() {
        let mut desc = movsb_descriptor();
        if let Some(ir) = desc.writes[0].val_indirect_read.as_mut() {
            ir.seg_property = None;
        }
        let prog = empty_program_with_descriptor(desc.clone());
        let mut state = rigged_state(&["CX", "DI", "SI", "ES", "DS", "flags"]);
        state.set_var("CX", 1);
        let outcome = apply_copy(&desc, &prog, &mut state, &[]);
        assert!(matches!(outcome, ApplyOutcome::Unsupported(_)));
    }

    #[test]
    fn copy_refuses_when_pointers_have_different_step() {
        let mut desc = movsb_descriptor();
        desc.pointers[1].base_step = 2;
        let prog = empty_program_with_descriptor(desc.clone());
        let mut state = rigged_state(&["CX", "DI", "SI", "ES", "DS", "flags"]);
        state.set_var("CX", 1);
        let outcome = apply_copy(&desc, &prog, &mut state, &[]);
        assert!(matches!(outcome, ApplyOutcome::Unsupported(_)));
    }

    /// Genericity probe: a Copy descriptor with completely unrelated
    /// property names (no DI/SI/ES/DS/CX/flags) must produce identical
    /// behaviour. Verifies the applier reads no name as text — every
    /// resolution is whole-name slot lookup, every structural decision
    /// is by `Expr` shape and slot identity.
    #[test]
    fn copy_genericity_unrelated_names() {
        let desc = LoopDescriptor {
            key_property: "--opaque_key".to_string(),
            key_value: 0xBC, // arbitrary
            ip_property: "--ip_x".to_string(),
            ip_self_property: "--ip_x_prev".to_string(),
            ip_advance_literal: 1,
            predicate_properties: vec!["--cont_x".to_string()],
            predicate: StyleTest::Single {
                property: "--cont_x".to_string(),
                value: Expr::Literal(1.0),
            },
            counter: Some(CounterEntry {
                property: "--alpha".to_string(),
                self_property: "--alpha_prev".to_string(),
                step: 1,
            }),
            pointers: vec![
                PointerEntry {
                    property: "--beta".to_string(),
                    self_property: "--beta".to_string(),
                    base_step: 1,
                    flag_property: "--gamma".to_string(),
                    flag_bit: 5, // arbitrary
                },
                PointerEntry {
                    property: "--zeta".to_string(),
                    self_property: "--zeta".to_string(),
                    base_step: 1,
                    flag_property: "--gamma".to_string(),
                    flag_bit: 5,
                },
            ],
            writes: vec![WriteEntry {
                addr_property: "--w_addr".to_string(),
                val_property: "--w_val".to_string(),
                addr_expr: Expr::Literal(0.0),
                val_expr: Expr::Var {
                    name: "--theta".to_string(),
                    fallback: None,
                },
                addr_decomposition: Some(("--epsilon".to_string(), "--beta".to_string())),
                val_indirect_read: Some(IndirectRead {
                    seg_property: Some("--eta".to_string()),
                    pointer_property: "--zeta".to_string(),
                    intermediate_property: "--theta".to_string(),
                }),
            }],
            flag_conditioned: false,
            bulk_class: BulkClass::Copy,
        };
        let prog = empty_program_with_descriptor(desc.clone());
        let mut state =
            rigged_state(&["alpha", "beta", "zeta", "epsilon", "eta", "gamma", "theta"]);
        state.set_var("alpha", 5);
        state.set_var("beta", 0x10); // dst pointer
        state.set_var("zeta", 0x20); // src pointer
        state.set_var("epsilon", 0x4000); // dst seg
        state.set_var("eta", 0x2000); // src seg
        state.set_var("gamma", 0); // bit 5 clear → forward
        let src_base = 0x2000 * 16 + 0x20;
        let dst_base = 0x4000 * 16 + 0x10;
        seed_bytes(&mut state, src_base, &[0xA1, 0xA2, 0xA3, 0xA4, 0xA5]);

        let outcome = apply_copy(&desc, &prog, &mut state, &[]);
        assert_eq!(outcome, ApplyOutcome::Applied { iterations: 5 });
        for (i, &b) in [0xA1, 0xA2, 0xA3, 0xA4, 0xA5].iter().enumerate() {
            assert_eq!(state.memory[dst_base + i], b);
        }
    }

    /// Dual-mode equivalence (byte): drive the same setup through the
    /// applier and through a direct mirror of the hardcoded MOVSB loop
    /// on independent State clones. Memory must be byte-for-byte
    /// identical.
    #[test]
    fn copy_equivalence_byte_matches_direct_loop() {
        let desc = movsb_descriptor();
        let prog = empty_program_with_descriptor(desc.clone());
        let names = &["CX", "DI", "SI", "ES", "DS", "flags"];
        let mut state_a = rigged_state(names);
        let mut state_b = rigged_state(names);
        let n = 64usize;
        let src_base = 0x1000 * 16 + 0x100;
        for s in [&mut state_a, &mut state_b] {
            s.set_var("CX", n as i32);
            s.set_var("DI", 0x200);
            s.set_var("SI", 0x100);
            s.set_var("ES", 0x4000);
            s.set_var("DS", 0x1000);
            s.set_var("flags", 0);
            // Seed src.
            for k in 0..n {
                s.memory[src_base + k] = (0x10 + k as u8) ^ 0x5A;
            }
        }

        // Path A: applier.
        let outcome = apply_copy(&desc, &prog, &mut state_a, &[]);
        assert_eq!(outcome, ApplyOutcome::Applied { iterations: n as i32 });

        // Path B: mirror the hardcoded MOVSB loop from compile.rs.
        let es_base = 0x4000i64 * 16;
        let ds_base = 0x1000i64 * 16;
        let di = 0x200i64;
        let si = 0x100i64;
        for k in 0..(n as i64) {
            let s_addr = ds_base + si + k;
            let d_addr = es_base + di + k;
            let b = (state_b.read_mem(s_addr as i32) & 0xFF) as u8;
            bulk_store_byte(&mut state_b, d_addr, b);
        }

        assert_eq!(
            state_a.memory, state_b.memory,
            "applier byte-copy must match direct loop byte-for-byte"
        );
    }

    /// Dual-mode equivalence (word, reverse): MOVSW DF=1 against a
    /// direct mirror of the hardcoded MOVSW loop.
    #[test]
    fn copy_equivalence_word_reverse_matches_direct_loop() {
        let desc = movsw_descriptor();
        let prog = empty_program_with_descriptor(desc.clone());
        let names = &["CX", "DI", "SI", "ES", "DS", "flags"];
        let mut state_a = rigged_state(names);
        let mut state_b = rigged_state(names);
        let n = 50i64;
        let src_base = 0x2000 * 16 + 0x500;
        for s in [&mut state_a, &mut state_b] {
            s.set_var("CX", n as i32);
            s.set_var("DI", 0x600); // walks backward from 0x600
            s.set_var("SI", 0x500); // walks backward from 0x500
            s.set_var("ES", 0x5000);
            s.set_var("DS", 0x2000);
            s.set_var("flags", 1 << 10); // DF=1
            // Seed src across [0x500-2*49 .. 0x500+2) = [0x43E .. 0x502).
            for k in 0..(n * 2) {
                let off = 0x500 - (n - 1) * 2 + k;
                s.memory[(src_base as i64 - (n - 1) * 2 + k) as usize] =
                    ((off as u8) ^ 0xC3) & 0xFF;
            }
        }

        // Path A: applier.
        let outcome = apply_copy(&desc, &prog, &mut state_a, &[]);
        assert_eq!(outcome, ApplyOutcome::Applied { iterations: n as i32 });

        // Path B: mirror the hardcoded MOVSW reverse loop.
        let es_base = 0x5000i64 * 16;
        let ds_base = 0x2000i64 * 16;
        let di = 0x600i64;
        let si = 0x500i64;
        for k in 0..n {
            let off = -k * 2; // DF=1
            let s_addr = ds_base + si + off;
            let d_addr = es_base + di + off;
            let lo = (state_b.read_mem(s_addr as i32) & 0xFF) as u8;
            let hi = (state_b.read_mem((s_addr + 1) as i32) & 0xFF) as u8;
            bulk_store_byte(&mut state_b, d_addr, lo);
            bulk_store_byte(&mut state_b, d_addr + 1, hi);
        }

        assert_eq!(
            state_a.memory, state_b.memory,
            "applier word-copy reverse must match direct loop byte-for-byte"
        );
    }
}
