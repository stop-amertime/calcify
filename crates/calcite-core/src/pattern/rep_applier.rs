//! Descriptor-driven REP fast-forward applier.
//!
//! Phase 3b step 4 ships [`apply_fill`] — the `BulkClass::Fill` arm of the
//! eventual replacement for `compile.rs::rep_fast_forward`'s hardcoded
//! per-opcode `match`. Steps 5-6 fill in `Copy` and `ReadOnly` next to it;
//! step 7 then rips out the hardcoded path.
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
}
