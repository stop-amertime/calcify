//! Dual-execute harness for the descriptor-driven REP applier.
//!
//! Phase 3b step 3: a safety net for landing the
//! `BulkClass::{Fill, Copy, ReadOnly, PerIter}` appliers one variant at
//! a time in steps 4-6. The actual appliers don't exist yet — every
//! `BulkClass` arm of the stub applier panics with `unimplemented!()`.
//!
//! ## How it fits into the existing path
//!
//! `compile.rs::rep_fast_forward` is the hardcoded REP fast-forward path
//! that ships today. When `CALCITE_REP_DUAL=1` is set, the per-tick
//! pipeline calls [`pre_hardcoded`] *before* the hardcoded path mutates
//! state. If a [`LoopDescriptor`] exists for the current opcode and the
//! descriptor's `BulkClass` is in the user-supplied
//! `CALCITE_REP_DUAL_VARIANTS` allow-list, we snapshot the entire `State`
//! (cheap relative to the rest of the per-tick cost; this mode is opt-in)
//! and return a [`DualSession`] carrying the clone.
//!
//! After the hardcoded path returns, the caller hands the post-state to
//! [`post_hardcoded`]. We run the descriptor-driven stub applier on the
//! pre-state clone and diff its output against the real post-state. Any
//! mismatch panics — that's the entire point of the harness, to surface
//! divergence as soon as a real applier ships.
//!
//! Steps 4-6 each fill in one `BulkClass` arm of the stub. While an arm
//! is unimplemented, **the harness must not invoke the stub for that
//! variant** — otherwise we'd get an `unimplemented!()` panic that's a
//! configuration error, not a real divergence. The `CALCITE_REP_DUAL_VARIANTS`
//! env var (comma-separated `BulkClass` names) controls which arms are
//! exercised. An empty allow-list (the default) means dual mode is
//! "wired but inert" — every snapshot is taken and discarded, and no
//! `unimplemented!()` ever fires.
//!
//! ## Cardinal-rule scope
//!
//! This module reads only structural metadata from `LoopDescriptor`:
//! slot indices, the `BulkClass` enum, `addr_decomposition`,
//! `val_indirect_read`. It does not read any property name as text. It
//! does not encode opcode-specific knowledge (no `match opcode` arms;
//! the dispatch is on `BulkClass`). The hook into `rep_fast_forward`
//! lives inside that function — the harness is a name-blind walker over
//! slot indices, per the scoping doc's Q1 Option A.
//!
//! ## Why we snapshot the full `State` rather than narrowing
//!
//! "Enough state" for a divergence check is whatever the new applier
//! could ever touch, plus anything the hardcoded path could ever touch.
//! The hardcoded path writes `memory`, `extended`, several `state_vars`
//! (DI, SI, CX, IP, cycleCount). The descriptor-driven applier in steps
//! 4-6 will reach for the same set, but the structural recogniser may
//! discover writes the hardcoded path didn't think to consider. A narrow
//! snapshot ("just the bytes the descriptor names") would silently miss
//! a future bug where the new applier touches an unexpected slot.
//!
//! `State::clone()` is `O(memory.len() + extended.len() + state_vars.len())` —
//! a few hundred KB of mostly-zero memory plus the sparse `extended`
//! map. With dual mode opt-in we eat that cost intentionally for the
//! safety net it buys. If divergence detection works, steps 4-6 land
//! confidently; if it doesn't, no real-world cabinet hits this code path
//! and the cost is only paid in tests.

use crate::compile::CompiledProgram;
use crate::pattern::loop_descriptor::{BulkClass, LoopDescriptor};
use crate::state::State;

/// Snapshot taken before the hardcoded REP fast-forward path runs. The
/// caller hands this back to [`post_hardcoded`] after the hardcoded path
/// has mutated the real state, so the harness can run the stub applier
/// on the snapshot and diff against the real post-state.
#[must_use]
pub struct DualSession {
    /// Index into `program.loop_descriptors` for the descriptor that
    /// matches the opcode being fast-forwarded. We carry the index, not
    /// a borrow, so this struct is `'static`-shaped and easy to thread.
    descriptor_index: usize,
    /// Pre-tick clone of the entire `State`. The stub applier mutates
    /// this clone, then we compare it to the real post-state.
    pre_state: State,
    /// Pre-tick snapshot of the property slot values. Slots are owned
    /// by the evaluator, not `State`, so they need their own snapshot
    /// for any applier that reads from them.
    pre_slots: Vec<i32>,
}

/// Possibly snapshot state and return a [`DualSession`] for diffing
/// after the hardcoded path runs.
///
/// Returns `None` (and does no work) when:
/// - `CALCITE_REP_DUAL` is not set / not truthy.
/// - No descriptor in `program.loop_descriptors` matches `opcode` —
///   the validator (`CALCITE_REP_GENERIC=1`) would already log this as
///   a MISS; the dual harness defers to the validator and bails.
/// - The descriptor's `bulk_class` is not in the allow-list set by
///   `CALCITE_REP_DUAL_VARIANTS`. This is the configuration knob that
///   keeps `unimplemented!()` arms from firing while their applier is
///   still being built (steps 4-6).
pub fn pre_hardcoded(
    program: &CompiledProgram,
    state: &State,
    slots: &[i32],
    opcode: i32,
) -> Option<DualSession> {
    if !dual_enabled() {
        return None;
    }
    let descriptor_index = program
        .loop_descriptors
        .iter()
        .position(|d| d.key_value == opcode as i64)?;
    let descriptor = &program.loop_descriptors[descriptor_index];
    if !variant_enabled(descriptor.bulk_class) {
        return None;
    }
    Some(DualSession {
        descriptor_index,
        pre_state: state.clone(),
        pre_slots: slots.to_vec(),
    })
}

/// Run the stub applier on the snapshot and diff against the real
/// post-state. Panics on divergence — that's the harness's whole job.
///
/// Step 3 ships the stub: every `BulkClass` arm is `unimplemented!()`.
/// Steps 4-6 will fill in one arm per commit. While a particular arm is
/// unimplemented, [`pre_hardcoded`] returns `None` for descriptors of
/// that class (gated by `CALCITE_REP_DUAL_VARIANTS`), so this function
/// never sees those — calls only land here when the user has explicitly
/// enabled the variant.
pub fn post_hardcoded(
    session: DualSession,
    program: &CompiledProgram,
    real_post_state: &State,
    real_post_slots: &[i32],
) {
    let DualSession {
        descriptor_index,
        mut pre_state,
        pre_slots,
    } = session;
    let descriptor = &program.loop_descriptors[descriptor_index];
    match apply_descriptor(descriptor, program, &mut pre_state, &pre_slots) {
        ApplierResult::Diff => {
            diff_or_panic(descriptor, &pre_state, real_post_state, real_post_slots);
        }
        ApplierResult::Skip(reason) => {
            // Applier refused this shape; nothing was mutated on the
            // clone, so a diff would always trip. Surface the skip on
            // stderr so a developer running with dual mode on can see
            // which shapes still fall through to the hardcoded path.
            eprintln!(
                "[rep-dual] skip opcode {:#04x} ({:?}): {}",
                descriptor.key_value, descriptor.bulk_class, reason,
            );
        }
    }
}

/// Descriptor-driven applier dispatch. Hands off to the per-`BulkClass`
/// applier in [`crate::pattern::rep_applier`] and returns whether the
/// caller should diff the resulting state against the hardcoded path.
///
/// Step 4 implements `BulkClass::Fill`; the other arms still
/// `unimplemented!()` — but [`pre_hardcoded`] only returns `Some` when
/// the variant is in the env-var allow-list, so unimplemented arms can
/// only fire if the user explicitly enables them.
fn apply_descriptor(
    descriptor: &LoopDescriptor,
    program: &CompiledProgram,
    state: &mut State,
    slots: &[i32],
) -> ApplierResult {
    use crate::pattern::rep_applier::{apply_copy, apply_fill, ApplyOutcome};
    match descriptor.bulk_class {
        BulkClass::Fill => match apply_fill(descriptor, program, state, slots) {
            ApplyOutcome::Applied { .. } => ApplierResult::Diff,
            ApplyOutcome::Unsupported(reason) => ApplierResult::Skip(reason),
        },
        BulkClass::Copy => match apply_copy(descriptor, program, state, slots) {
            ApplyOutcome::Applied { .. } => ApplierResult::Diff,
            ApplyOutcome::Unsupported(reason) => ApplierResult::Skip(reason),
        },
        BulkClass::ReadOnly => unimplemented!(
            "BulkClass::ReadOnly applier lands in phase 3b step 6 — CMPS/SCAS/LODS"
        ),
        BulkClass::PerIter => unimplemented!(
            "BulkClass::PerIter applier — fallback walker, lands alongside step 6"
        ),
    }
}

/// What the harness should do after the applier returns.
#[derive(Debug)]
enum ApplierResult {
    /// Applier ran. Diff the clone against the real post-state.
    Diff,
    /// Applier deliberately skipped this descriptor (shape it doesn't
    /// support yet). Don't diff — the clone wasn't mutated like the
    /// real path was, and a diff would just panic on every call.
    Skip(&'static str),
}

/// Diff the applier's output against the real post-state and panic on
/// divergence.
///
/// Step 4 of phase 3b ships an applier that only mutates *memory* —
/// state-var updates (DI/CX/IP/cycleCount) and the IP+prefix bookkeeping
/// are deferred to step 7's full flip, where the structural metadata
/// for prefix length and per-iter cycle cost lands too. Until then this
/// diff intentionally checks memory and the extended map only; the
/// state_var deltas belong to the hardcoded path that runs in parallel
/// during dual mode and don't represent applier divergence.
fn diff_or_panic(
    descriptor: &LoopDescriptor,
    stub_post_state: &State,
    real_post_state: &State,
    real_post_slots: &[i32],
) {
    let _ = real_post_slots;
    if stub_post_state.memory != real_post_state.memory {
        let first_diff = stub_post_state
            .memory
            .iter()
            .zip(real_post_state.memory.iter())
            .position(|(a, b)| a != b);
        panic!(
            "[rep-dual] memory divergence on opcode {:#04x} ({:?}): first byte differs at offset {:?}",
            descriptor.key_value,
            descriptor.bulk_class,
            first_diff,
        );
    }
    if stub_post_state.extended != real_post_state.extended {
        panic!(
            "[rep-dual] extended-map divergence on opcode {:#04x} ({:?}): {} entries vs {}",
            descriptor.key_value,
            descriptor.bulk_class,
            stub_post_state.extended.len(),
            real_post_state.extended.len(),
        );
    }
}

// ---------------------------------------------------------------------------
// Env-var gates.
// ---------------------------------------------------------------------------

#[cfg(not(target_arch = "wasm32"))]
fn dual_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        matches!(
            std::env::var("CALCITE_REP_DUAL").as_deref(),
            Ok("1") | Ok("true") | Ok("on")
        )
    })
}

#[cfg(target_arch = "wasm32")]
fn dual_enabled() -> bool {
    false
}

/// Per-variant allow-list. Comma-separated `BulkClass` names in
/// `CALCITE_REP_DUAL_VARIANTS`. Recognised tokens (case-insensitive):
/// `fill`, `copy`, `readonly`, `read_only`, `peritem`, `per_iter`.
/// Default is empty — dual mode without an allow-list is inert by
/// design (snapshots are taken and diffed against an empty applier
/// run, which never happens because every variant returns `None` from
/// [`pre_hardcoded`]).
#[cfg(not(target_arch = "wasm32"))]
fn variant_enabled(class: BulkClass) -> bool {
    use std::sync::OnceLock;
    static ALLOW: OnceLock<VariantAllowList> = OnceLock::new();
    let allow = ALLOW.get_or_init(|| {
        VariantAllowList::parse(std::env::var("CALCITE_REP_DUAL_VARIANTS").ok().as_deref())
    });
    allow.contains(class)
}

#[cfg(target_arch = "wasm32")]
fn variant_enabled(_class: BulkClass) -> bool {
    false
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct VariantAllowList {
    fill: bool,
    copy: bool,
    read_only: bool,
    per_iter: bool,
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
impl VariantAllowList {
    fn parse(raw: Option<&str>) -> Self {
        let mut out = Self::default();
        let Some(raw) = raw else { return out };
        for tok in raw.split(',') {
            let t = tok.trim().to_ascii_lowercase();
            match t.as_str() {
                "fill" => out.fill = true,
                "copy" => out.copy = true,
                "readonly" | "read_only" => out.read_only = true,
                "peritem" | "per_iter" | "periter" => out.per_iter = true,
                "" => {}
                _ => {
                    eprintln!(
                        "[rep-dual] CALCITE_REP_DUAL_VARIANTS: unknown variant `{}` (expected one of: fill, copy, readonly, per_iter)",
                        tok.trim()
                    );
                }
            }
        }
        out
    }

    fn contains(&self, class: BulkClass) -> bool {
        match class {
            BulkClass::Fill => self.fill,
            BulkClass::Copy => self.copy,
            BulkClass::ReadOnly => self.read_only,
            BulkClass::PerIter => self.per_iter,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pattern::loop_descriptor::{
        BulkClass, CounterEntry, LoopDescriptor, PointerEntry, WriteEntry,
    };
    use crate::types::{Expr, StyleTest};

    /// Build a minimal `LoopDescriptor` with a chosen `BulkClass`. The
    /// fields not directly tested are filled with defensible defaults;
    /// the harness wiring tests only inspect `key_value` and
    /// `bulk_class`.
    fn descriptor_with_class(key_value: i64, class: BulkClass) -> LoopDescriptor {
        LoopDescriptor {
            key_property: "--key".to_string(),
            key_value,
            ip_property: "--ip".to_string(),
            ip_self_property: "--ip_prev".to_string(),
            ip_advance_literal: 1,
            predicate_properties: vec![],
            predicate: StyleTest::Single {
                property: "--cont".to_string(),
                value: Expr::Literal(1.0),
            },
            counter: Some(CounterEntry {
                property: "--cx".to_string(),
                self_property: "--cx_prev".to_string(),
                step: 1,
            }),
            pointers: vec![PointerEntry {
                property: "--di".to_string(),
                self_property: "--di_prev".to_string(),
                base_step: 1,
                flag_property: "--flags".to_string(),
                flag_bit: 10,
            }],
            writes: vec![WriteEntry {
                addr_property: "--mAddr".to_string(),
                val_property: "--mVal".to_string(),
                addr_expr: Expr::Literal(0.0),
                val_expr: Expr::Literal(0.0),
                addr_decomposition: None,
                val_indirect_read: None,
            }],
            flag_conditioned: false,
            bulk_class: class,
        }
    }

    #[test]
    fn parse_empty_allow_list_admits_nothing() {
        let allow = VariantAllowList::parse(None);
        assert!(!allow.contains(BulkClass::Fill));
        assert!(!allow.contains(BulkClass::Copy));
        assert!(!allow.contains(BulkClass::ReadOnly));
        assert!(!allow.contains(BulkClass::PerIter));
    }

    #[test]
    fn parse_fill_only() {
        let allow = VariantAllowList::parse(Some("fill"));
        assert!(allow.contains(BulkClass::Fill));
        assert!(!allow.contains(BulkClass::Copy));
        assert!(!allow.contains(BulkClass::ReadOnly));
        assert!(!allow.contains(BulkClass::PerIter));
    }

    #[test]
    fn parse_multi_with_aliases() {
        let allow = VariantAllowList::parse(Some("Fill, copy, READ_ONLY, PerIter"));
        assert!(allow.contains(BulkClass::Fill));
        assert!(allow.contains(BulkClass::Copy));
        assert!(allow.contains(BulkClass::ReadOnly));
        assert!(allow.contains(BulkClass::PerIter));
    }

    #[test]
    fn parse_unknown_variant_does_not_panic() {
        // Unknown tokens log to stderr but shouldn't abort the program.
        let allow = VariantAllowList::parse(Some("fill,bogus,copy"));
        assert!(allow.contains(BulkClass::Fill));
        assert!(allow.contains(BulkClass::Copy));
        assert!(!allow.contains(BulkClass::ReadOnly));
    }

    /// Step 4 implements `BulkClass::Fill`. Dispatch should reach
    /// [`crate::pattern::rep_applier::apply_fill`] without panicking.
    /// The minimal `descriptor_with_class` helper produces a structurally
    /// invalid `Fill` descriptor (no real address decomposition), so
    /// the applier returns `Unsupported` and the dispatcher returns
    /// `ApplierResult::Skip` — which is the expected behaviour for any
    /// shape the applier doesn't recognise. The point of the test is
    /// that the `BulkClass::Fill` arm is no longer `unimplemented!()`.
    #[test]
    fn stub_applier_dispatches_on_fill_arm_without_panic() {
        let desc = descriptor_with_class(0xAA, BulkClass::Fill);
        let mut state = State::new(0x10);
        let prog = empty_program();
        let result = apply_descriptor(&desc, &prog, &mut state, &[]);
        // The fixture descriptor has no addr_decomposition, so the
        // applier surfaces Unsupported → Skip. What we're proving here
        // is that the Fill arm no longer matches `unimplemented!()`.
        assert!(matches!(result, ApplierResult::Skip(_)));
    }

    /// Step 5 implements `BulkClass::Copy`. Dispatch should reach
    /// [`crate::pattern::rep_applier::apply_copy`] without panicking.
    /// The minimal `descriptor_with_class` helper produces a structurally
    /// invalid `Copy` descriptor (only one pointer, no val_indirect_read),
    /// so the applier returns `Unsupported` and the dispatcher returns
    /// `ApplierResult::Skip` — which is the expected behaviour for any
    /// shape the applier doesn't recognise. The point of the test is
    /// that the `BulkClass::Copy` arm is no longer `unimplemented!()`.
    #[test]
    fn stub_applier_dispatches_on_copy_arm_without_panic() {
        let desc = descriptor_with_class(0xA4, BulkClass::Copy);
        let mut state = State::new(0x10);
        let prog = empty_program();
        let result = apply_descriptor(&desc, &prog, &mut state, &[]);
        assert!(matches!(result, ApplierResult::Skip(_)));
    }

    #[test]
    #[should_panic(expected = "BulkClass::ReadOnly applier lands in phase 3b step 6")]
    fn stub_applier_dispatches_on_readonly_arm() {
        let desc = descriptor_with_class(0xA6, BulkClass::ReadOnly);
        let mut state = State::new(0x10);
        let prog = empty_program();
        let _ = apply_descriptor(&desc, &prog, &mut state, &[]);
    }

    #[test]
    #[should_panic(expected = "BulkClass::PerIter applier")]
    fn stub_applier_dispatches_on_per_iter_arm() {
        let desc = descriptor_with_class(0xFF, BulkClass::PerIter);
        let mut state = State::new(0x10);
        let prog = empty_program();
        let _ = apply_descriptor(&desc, &prog, &mut state, &[]);
    }

    /// The harness's most important contract: when dual mode is off,
    /// `pre_hardcoded` returns `None` immediately and never touches the
    /// descriptor list. We verify by passing an empty `loop_descriptors`
    /// and a state we'd notice being cloned (it isn't — but the
    /// observable is the `None` return).
    #[test]
    fn pre_hardcoded_returns_none_when_dual_disabled() {
        // CALCITE_REP_DUAL is read once via OnceLock at first call.
        // In the test process it's almost certainly unset (the cargo
        // runner doesn't set it), so dual_enabled() should be false.
        // If this test is ever run with the env var set, that's the
        // user's choice and the assertion will fail — that's the
        // intended contract: dual mode requires explicit opt-in.
        let program = empty_program();
        let state = State::new(0x10);
        let session = pre_hardcoded(&program, &state, &[], 0xAA);
        assert!(session.is_none());
    }

    fn empty_program() -> CompiledProgram {
        // Construct a minimal CompiledProgram. The harness only reads
        // `loop_descriptors`, so all other fields default to empty.
        // Field shape mirrors the fixture in `tests/rep_fast_forward.rs`.
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
            loop_descriptors: Vec::new(),
        }
    }

    /// Allow-list parsing handles whitespace and case in the way that
    /// matters for the dispatch chain — case-insensitive, whitespace
    /// trimmed, empty tokens ignored. The harness needs this so users
    /// can write `CALCITE_REP_DUAL_VARIANTS="Fill, Copy"` without
    /// thinking about it.
    #[test]
    fn parse_handles_whitespace_and_empty_tokens() {
        let allow = VariantAllowList::parse(Some(" , fill , , copy , "));
        assert!(allow.contains(BulkClass::Fill));
        assert!(allow.contains(BulkClass::Copy));
        assert!(!allow.contains(BulkClass::ReadOnly));
        assert!(!allow.contains(BulkClass::PerIter));
    }
}
