//! Self-loop recogniser: extract structural descriptors of opcode-keyed
//! self-repeating instructions from the parsed dispatch table family.
//!
//! ## What this is
//!
//! Some opcodes that a cabinet emits are *self-loops*: when the dispatched
//! body executes, it leaves the program counter unchanged (or moved back to
//! itself) so the same opcode runs again on the next tick, gated by a
//! counter. Each iteration touches memory and steps a pointer. The classic
//! examples come from x86 `REP STOSB` / `REP MOVSB` / `REP CMPSB` etc., but
//! the structural shape is general — any cabinet that wants a CPU-level
//! self-iterating instruction will emit the same shape.
//!
//! Per the calcite cardinal rule (see `../CLAUDE.md`), this recogniser may
//! NOT look at characters in any property/slot/function name. Names are
//! opaque tokens. The recogniser determines structure from:
//!
//! - **Slot identity**: whether two `var(--name)` references point at the
//!   same slot (string equality on the whole name).
//! - **Expression shape**: the tree shape of `Expr` nodes (literals,
//!   arithmetic, conditionals, calls).
//! - **Repetition**: which slots appear in multiple per-opcode bodies, and
//!   in which roles.
//!
//! It does NOT do prefix sniffing, suffix sniffing, character searches, or
//! any other content-based decision on names. A 6502 cabinet, a brainfuck
//! cabinet, or an arbitrary non-emulator cabinet whose CSS happens to emit
//! the same shape MUST trigger the same recognition with no calcite-side
//! change.
//!
//! ## What it produces
//!
//! For each opcode value V where the per-V dispatch bodies, taken together,
//! match the self-loop signature, this module yields a [`LoopDescriptor`]
//! describing the structural facts: counter slot, pointer slots and their
//! step formulas, write descriptors, exit predicate, and IP-advance
//! formula.
//!
//! Phase 1 of the [rep_fast_forward genericity mission][plan] only emits
//! descriptors; the descriptor-driven runtime applier is phase 2.
//!
//! [plan]: ../../../../CSS-DOS/docs/plans/2026-05-06-rep-fast-forward-genericity.md
//!
//! ## Recognition strategy at a glance
//!
//! Given the parsed assignments on `.cpu`, we look for a family of
//! opcode-keyed dispatches: assignments whose RHS is
//! `Expr::StyleCondition` and whose branches all test the same single
//! property (the "dispatch key", typically the opcode latch slot). Among
//! the family, an *IP slot* is one whose per-V body has the
//! "stay-here-or-advance" shape: a `StyleCondition` whose two outcomes are
//! (a) the slot's own prior value minus a literal/slot offset and (b) the
//! slot's own prior value plus a literal. The predicate of that
//! `StyleCondition` is the *loop predicate* — the structural shape that
//! tells the rest of the per-V bodies whether iteration is continuing.
//!
//! From there, for each other family member we look for known shapes
//! against that predicate:
//! - "counter": `if(<predicate>: self; else: max(0, self - 1))`
//!   (self meaning the same opaque slot read on both sides).
//! - "pointer": `if(<rep-guard>: lowerBytes(self ± k - bit(flags, n) * 2k, 16); else: self)`.
//! - "memwrite": for assignments belonging to the memwrite family, the
//!   per-V body of the address half is gated to `-1` when the rep-guard
//!   says "no fire", and to a real address expression when it should
//!   write.
//!
//! The pieces are returned as a [`LoopDescriptor`].

use std::collections::{HashMap, HashSet};

use crate::types::*;

/// Structural description of a self-loop opcode discovered in a cabinet's
/// dispatch family.
///
/// This is purely a description of CSS shape — it carries no x86, 6502, or
/// any other ISA assumptions. The runtime applier (phase 2) reads these
/// descriptors and walks them; phase 1 only emits them.
#[derive(Debug, Clone, PartialEq)]
pub struct LoopDescriptor {
    /// The dispatch key property name (e.g. the opcode latch).
    /// Stored as an opaque token; the recogniser does not inspect it.
    pub key_property: String,
    /// The dispatch key value this descriptor describes (e.g. one specific
    /// opcode literal).
    pub key_value: i64,
    /// Property name whose dispatch body has the "stay-here-or-advance"
    /// shape. The runtime applier writes the advance value directly when
    /// fast-forwarding.
    pub ip_property: String,
    /// Slot read by both branches of the IP body — the "prior" mirror of
    /// the IP slot. Carried as an opaque name; the runtime resolves it to
    /// a slot index.
    pub ip_self_property: String,
    /// Literal offset added by the IP-advance branch (the per-iter IP step
    /// taken when the loop exits).
    pub ip_advance_literal: i32,
    /// Property names that participate in the loop's continuation predicate.
    /// Captured for diagnostic / verification purposes; the predicate
    /// itself is also stored verbatim in [`Self::predicate`].
    pub predicate_properties: Vec<String>,
    /// The full predicate expression as it appears in the IP body's
    /// `StyleCondition`. The runtime applier evaluates this against the
    /// post-tick slot view to decide whether to fast-forward at all.
    pub predicate: StyleTest,
    /// The counter slot, if one was found.
    pub counter: Option<CounterEntry>,
    /// Pointer slots and their step formulas. Order is recogniser-dependent
    /// (see implementation); descriptors compare equal regardless of order.
    pub pointers: Vec<PointerEntry>,
    /// Write descriptors gathered from memwrite-slot assignments belonging
    /// to the same dispatch family.
    pub writes: Vec<WriteEntry>,
    /// Whether the predicate is a flag-conditioned exit (i.e. the loop
    /// exits not just on counter zero but also on a flag-bit condition,
    /// CMPS/SCAS-style). Phase 1 only sets this when the predicate's
    /// shape itself has a flag-bit conjunction; phase 2 will use it to
    /// drive the right runtime walker.
    pub flag_conditioned: bool,
    /// Bulk-applier classification computed structurally at descriptor
    /// build time. Phase 3a populates this; phase 3b's runtime applier
    /// dispatches on it. The classifier is purely shape-based — it does
    /// not look at any name, only at whether write-value expressions
    /// transitively reference any pointer-slot mirror.
    pub bulk_class: BulkClass,
}

/// Coarse classification of how a recognised self-loop's per-iter
/// memory writes can be collapsed into a bulk operation.
///
/// The classifier is structural and does not encode opcode knowledge.
/// It looks at:
///
/// - Whether the descriptor has any write entries (`writes.len()`).
/// - Whether each write's value expression transitively references the
///   `self_property` of any pointer entry (i.e. the prior-tick mirror
///   the cabinet uses to read the source pointer's pre-iter value).
///
/// "Transitively references" means: there is some `Expr::Var` or
/// `StyleTest::Single { property, .. }` somewhere in the value
/// expression tree whose property name equals one of the pointer's
/// `self_property` strings (whole-name equality — no substring or
/// character inspection).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BulkClass {
    /// No memory writes (CMPS / SCAS / LODS). Bulk applier walks
    /// counter-many iterations doing reads and predicate checks; no
    /// memory mutation.
    ReadOnly,
    /// All write-value expressions are independent of pointer-slot
    /// state (typical STOS: every iteration writes the same constant
    /// from an accumulator). Collapses to a flat memset over the
    /// iterated address range.
    Fill,
    /// At least one write-value expression reads through a pointer
    /// slot's mirror (typical MOVS: writes byte fetched from the
    /// per-iter source pointer). Collapses to a memcpy along the
    /// stepped address range, modulo overlap rules.
    Copy,
    /// Write-value expression depends on something other than a
    /// pointer mirror, or has shape we don't recognise structurally.
    /// Bulk applier falls back to per-iter evaluation.
    PerIter,
}

/// A counter slot — one whose per-V body decrements itself when the
/// loop predicate says "fire" and saturates at zero.
#[derive(Debug, Clone, PartialEq)]
pub struct CounterEntry {
    /// The property the dispatch body is for.
    pub property: String,
    /// The "self" slot read on both sides of the body. In well-formed
    /// shapes this equals `property` (or its prior-tick mirror); the
    /// recogniser only requires that both branches read the same slot,
    /// not that it bears a particular name.
    pub self_property: String,
    /// The decrement amount. Almost always 1 in practice but recorded
    /// generically.
    pub step: i32,
}

/// A pointer slot — one whose per-V body advances by ±k under a
/// direction-flag bit, gated by the rep-guard.
#[derive(Debug, Clone, PartialEq)]
pub struct PointerEntry {
    /// The dispatch body's target property.
    pub property: String,
    /// The "self" slot read by the body's update branch.
    pub self_property: String,
    /// The base step magnitude (positive). The actual signed step is
    /// `base_step` when the direction-flag bit is 0, `-base_step` when 1.
    pub base_step: i32,
    /// The flag slot the direction bit is read from.
    pub flag_property: String,
    /// Bit position of the direction flag inside `flag_property`.
    pub flag_bit: u32,
}

/// A memwrite descriptor — captured as raw expression slices since phase
/// 1 doesn't yet drive a runtime; phase 2 compiles these into the applier.
#[derive(Debug, Clone, PartialEq)]
pub struct WriteEntry {
    /// The address-side property the kiln-style memwrite slot was on
    /// (e.g. one of the `--memAddrN`-shaped slots, but the recogniser
    /// treats the name as opaque).
    pub addr_property: String,
    /// The value-side property paired with the address.
    pub val_property: String,
    /// Address expression for this opcode. Already unwrapped from the
    /// per-V `StyleCondition` branch.
    pub addr_expr: Expr,
    /// Value expression for this opcode.
    pub val_expr: Expr,
}

/// Recognise self-loop opcodes in a dispatch family.
///
/// Inputs:
///
/// - `assignments`: the cabinet's top-level assignments (e.g. the body of
///   `.cpu`). These are scanned for the dispatch family.
///
/// The function returns one [`LoopDescriptor`] per recognised opcode.
///
/// **Cardinal-rule contract.** The implementation only inspects:
/// - Slot/property *identity* (string-equality on the entire name).
/// - Expression-tree *shape* (variant, arity, literals).
/// - Slot *repetition* (how many times a name appears, and in which
///   structural positions).
///
/// It does NOT inspect characters of any name. Renaming every slot in the
/// cabinet (preserving identity) MUST produce identical descriptors modulo
/// the new names.
pub fn recognise_loops(assignments: &[Assignment]) -> Vec<LoopDescriptor> {
    // 1. Find the dispatch family — assignments whose RHS is a top-level
    //    StyleCondition keyed on a single property, all sharing the same
    //    key.
    let family = collect_dispatch_family(assignments);
    let Some(family) = family else { return Vec::new() };

    let mut out = Vec::new();
    for &key_value in &family.keys {
        if let Some(desc) = recognise_one_opcode(&family, key_value) {
            out.push(desc);
        }
    }
    out.sort_by_key(|d| d.key_value);
    out
}

// ---------------------------------------------------------------------------
// Dispatch family — assignments keyed on a common single property.
// ---------------------------------------------------------------------------

/// A group of assignments whose RHS is a top-level `StyleCondition` whose
/// branches all test a single common property (the dispatch key). The set
/// of values across all members is the *key set*.
struct DispatchFamily<'a> {
    key_property: String,
    /// Set of all key values that appear in any member's branches.
    keys: Vec<i64>,
    /// Per-member: property name → list of (key_value, body Expr).
    /// A member is one assignment, indexed by its property name.
    members: HashMap<&'a str, FamilyMember<'a>>,
}

struct FamilyMember<'a> {
    /// Per-key body. A body is the `then` of the matching StyleBranch.
    bodies: HashMap<i64, &'a Expr>,
    /// The fallback Expr (applied when no key matches).
    #[allow(dead_code)]
    fallback: &'a Expr,
    /// Position of this assignment in the source's `assignments` list.
    /// Used purely structurally — the recogniser does not interpret the
    /// number, only compares it across members. Cabinets that emit
    /// related slots in matched order (kiln pairs an address slot with
    /// the value slot that immediately follows) get correct pairing for
    /// free; cabinets that don't degrade to first-by-position pairing,
    /// which is no worse than the old name-sort heuristic.
    assignment_index: usize,
}

fn collect_dispatch_family<'a>(assignments: &'a [Assignment]) -> Option<DispatchFamily<'a>> {
    // For each assignment, try to extract a single-key dispatch shape:
    //   if(style(P:K1): B1; style(P:K2): B2; ...; else: F)
    // (StyleCondition where every branch is StyleTest::Single on the
    // same P, with K_i a literal.)
    let mut by_key: HashMap<String, HashMap<&'a str, FamilyMember<'a>>> = HashMap::new();

    for (idx, asn) in assignments.iter().enumerate() {
        let Some((key_prop, mut member)) = extract_single_key_dispatch(asn) else {
            continue;
        };
        member.assignment_index = idx;
        by_key.entry(key_prop).or_default().insert(asn.property.as_str(), member);
    }

    // Pick the largest family (in member count). If two are tied, prefer
    // the one with more keys.
    let (best_key_prop, best_members) = by_key
        .into_iter()
        .max_by_key(|(_, m)| {
            let n_members = m.len();
            let n_keys: usize = m.values().map(|fm| fm.bodies.len()).sum();
            (n_members, n_keys)
        })?;

    if best_members.len() < 2 {
        // A loop needs at least an IP body and one of {counter, pointer,
        // memwrite}. Two members is the minimum.
        return None;
    }

    let mut all_keys: HashSet<i64> = HashSet::new();
    for m in best_members.values() {
        for &k in m.bodies.keys() {
            all_keys.insert(k);
        }
    }
    let mut keys: Vec<i64> = all_keys.into_iter().collect();
    keys.sort_unstable();

    Some(DispatchFamily {
        key_property: best_key_prop,
        keys,
        members: best_members,
    })
}

fn extract_single_key_dispatch<'a>(
    asn: &'a Assignment,
) -> Option<(String, FamilyMember<'a>)> {
    let dispatch_expr = find_inner_dispatch(&asn.value)?;

    // Try the strict shape first: every branch on the same property
    // with a Literal value. This matches register dispatches like --CX
    // where the wrapper is peeled off before reaching here.
    if let Some(strict) = extract_strict_single_key(dispatch_expr, asn.property.as_str()) {
        return Some(strict);
    }

    // Fall back to the dominant-key shape: a StyleCondition where most
    // branches are keyed on one property but a few wrapper branches
    // (TF/IRQ override) are keyed on others. Used by the memwrite slot
    // assignments (--memAddrN / --memValN) where kiln folds the
    // wrapper into the same chain.
    let (key_prop, bodies_vec, fallback) = extract_dominant_dispatch_key(dispatch_expr)?;
    if bodies_vec.is_empty() {
        return None;
    }
    let mut bodies: HashMap<i64, &Expr> = HashMap::new();
    for (k, body) in bodies_vec {
        bodies.insert(k, body);
    }
    Some((
        key_prop,
        FamilyMember {
            bodies,
            fallback,
            assignment_index: 0, // overwritten by caller
        },
    ))
}

fn extract_strict_single_key<'a>(
    dispatch_expr: &'a Expr,
    _member_prop: &'a str,
) -> Option<(String, FamilyMember<'a>)> {
    let Expr::StyleCondition { branches, fallback } = dispatch_expr else { return None };
    if branches.is_empty() {
        return None;
    }
    let key_prop = match &branches[0].condition {
        StyleTest::Single { property, .. } => property.clone(),
        _ => return None,
    };
    let mut bodies: HashMap<i64, &Expr> = HashMap::new();
    for branch in branches {
        let StyleTest::Single { property, value } = &branch.condition else {
            return None;
        };
        if property != &key_prop {
            return None;
        }
        let Expr::Literal(v) = value else {
            return None;
        };
        bodies.insert(*v as i64, &branch.then);
    }
    Some((
        key_prop,
        FamilyMember {
            bodies,
            fallback: fallback.as_ref(),
            assignment_index: 0, // overwritten by caller
        },
    ))
}

/// Find the inner "single-key dispatch" StyleCondition inside an
/// expression that may have outer wrappers. Recognises the structural
/// shape: a StyleCondition whose every branch tests the SAME single
/// property against an integer literal. The wrappers we descend through
/// are also purely structural:
///
/// - `Calc(<inner>, <anything>)` arithmetic — kiln wraps the IP
///   dispatch in `calc(<dispatch> + var(--prefixLen))`. We descend
///   into either side of any binary calc op, and into the singleton arg
///   of unary ones, looking for an inner dispatch.
/// - `StyleCondition { branches, fallback }` whose `branches` keys are
///   on a different property than what the inner dispatch keys on. The
///   TF / IRQ wrapper kiln emits for every register has shape
///   `if(style(--_tf: 1): X; style(--_irqActive: 1): Y; else: <real>)`
///   where the real dispatch (keyed on `--opcode`) lives in the else.
///   The wrapper's two override branches return non-dispatch values
///   (constants or var-reads). We descend into the fallback whenever it
///   itself contains a single-key dispatch on a property different from
///   the outer's branch keys.
///
/// Returns `Some(<the StyleCondition that IS the single-key dispatch>)`,
/// or `None` if no such inner dispatch exists.
fn find_inner_dispatch(expr: &Expr) -> Option<&Expr> {
    // Direct hit?
    if is_single_key_dispatch(expr) {
        return Some(expr);
    }
    // Descend into Calc binary/unary ops.
    if let Expr::Calc(op) = expr {
        if let Some(inner) = descend_calc_for_dispatch(op) {
            return Some(inner);
        }
    }
    if let Expr::StyleCondition { fallback, branches } = expr {
        if !branches.is_empty() {
            // Try the fallback (TF/IRQ wrapper case: real dispatch
            // lives in the else).
            if let Some(inner) = find_inner_dispatch(fallback) {
                return Some(inner);
            }
            // Mixed-key StyleCondition (memwrite-slot case: the
            // wrapper branches are folded into the same chain as the
            // dispatch branches, all sharing a single
            // `if(... ;... ;... ;else: ...)`). Accept this expr as the
            // dispatch point if some single property dominates the
            // branches' keys. The downstream
            // `extract_dominant_dispatch_key` then picks out only the
            // dispatch-keyed branches.
            if has_dominant_dispatch_key(expr) {
                return Some(expr);
            }
        }
    }
    None
}

fn has_dominant_dispatch_key(expr: &Expr) -> bool {
    let Expr::StyleCondition { branches, .. } = expr else { return false };
    if branches.is_empty() {
        return false;
    }
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for b in branches {
        if let StyleTest::Single { property, value } = &b.condition {
            if matches!(value, Expr::Literal(_)) {
                *counts.entry(property.as_str()).or_default() += 1;
            }
        }
    }
    let total: usize = counts.values().sum();
    if total < 2 {
        return false;
    }
    let max_count = counts.values().copied().max().unwrap_or(0);
    max_count * 2 >= total
}

fn descend_calc_for_dispatch(op: &CalcOp) -> Option<&Expr> {
    match op {
        CalcOp::Add(a, b)
        | CalcOp::Sub(a, b)
        | CalcOp::Mul(a, b)
        | CalcOp::Div(a, b)
        | CalcOp::Mod(a, b)
        | CalcOp::Pow(a, b) => find_inner_dispatch(a).or_else(|| find_inner_dispatch(b)),
        CalcOp::Min(args) | CalcOp::Max(args) => args.iter().find_map(find_inner_dispatch),
        CalcOp::Clamp(a, b, c) => find_inner_dispatch(a)
            .or_else(|| find_inner_dispatch(b))
            .or_else(|| find_inner_dispatch(c)),
        CalcOp::Round(_, a, b) => find_inner_dispatch(a).or_else(|| find_inner_dispatch(b)),
        CalcOp::Sign(a) | CalcOp::Abs(a) | CalcOp::Negate(a) => find_inner_dispatch(a),
    }
}

fn is_single_key_dispatch(expr: &Expr) -> bool {
    let Expr::StyleCondition { branches, .. } = expr else { return false };
    if branches.is_empty() {
        return false;
    }
    let key = match &branches[0].condition {
        StyleTest::Single { property, .. } => property,
        _ => return false,
    };
    branches.iter().all(|b| match &b.condition {
        StyleTest::Single { property, value } => {
            property == key && matches!(value, Expr::Literal(_))
        }
        _ => false,
    })
}

/// Like [`is_single_key_dispatch`], but tolerant of "wrapper" branches
/// keyed on a different property than the dispatch's main key. Used
/// when a memwrite-slot StyleCondition has TF/IRQ override branches
/// keyed on `_tf` / `_irqActive` interleaved with the opcode-keyed
/// dispatch branches, all in one `if(...)` chain.
///
/// Returns the dispatch property name and the matching branches if
/// at least one Single::Literal branch on a single common property
/// dominates the branch set (more than half of all
/// `StyleTest::Single { value: Literal }` branches share its key).
fn extract_dominant_dispatch_key<'a>(
    expr: &'a Expr,
) -> Option<(String, Vec<(i64, &'a Expr)>, &'a Expr)> {
    let Expr::StyleCondition { branches, fallback } = expr else { return None };
    if branches.is_empty() {
        return None;
    }
    // Count occurrences of each Single-property used with a Literal
    // value.
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for b in branches {
        if let StyleTest::Single { property, value } = &b.condition {
            if matches!(value, Expr::Literal(_)) {
                *counts.entry(property.as_str()).or_default() += 1;
            }
        }
    }
    let total: usize = counts.values().sum();
    if total == 0 {
        return None;
    }
    // Pick the property with the most occurrences. Must have a clear
    // majority (>= 50% of literal-keyed branches) — otherwise we don't
    // have a coherent dispatch.
    let (dom_key, dom_count) = counts.iter().max_by_key(|(_, c)| **c)?;
    if *dom_count * 2 < total {
        return None;
    }
    let dom_key = dom_key.to_string();
    let mut bodies: Vec<(i64, &Expr)> = Vec::new();
    for b in branches {
        if let StyleTest::Single { property, value } = &b.condition {
            if property == &dom_key {
                if let Expr::Literal(v) = value {
                    bodies.push((*v as i64, &b.then));
                }
            }
        }
    }
    Some((dom_key, bodies, fallback.as_ref()))
}

// ---------------------------------------------------------------------------
// Per-opcode recognition.
// ---------------------------------------------------------------------------

fn recognise_one_opcode<'a>(
    family: &DispatchFamily<'a>,
    key_value: i64,
) -> Option<LoopDescriptor> {
    // Step 1: find a member with the IP-stay-or-advance shape for this
    // key. This is the killer signature; without it there is no loop.
    let mut ip_member: Option<(&str, IpShape)> = None;
    for (&prop, member) in &family.members {
        let Some(body) = member.bodies.get(&key_value) else { continue };
        if let Some(shape) = match_ip_stay_or_advance(body) {
            ip_member = Some((prop, shape));
            break;
        }
    }
    let (ip_prop, ip_shape) = ip_member?;

    // Step 2: walk the rest of the family for this key, classifying each
    // body against the predicate from the IP shape.
    let predicate = ip_shape.predicate.clone();
    let predicate_properties = collect_property_names_from_predicate(&predicate);
    let flag_conditioned = predicate_mentions_flag_bit(&predicate);

    let mut counter: Option<CounterEntry> = None;
    let mut pointers: Vec<PointerEntry> = Vec::new();
    let mut writes_addr: HashMap<&str, (&str, &Expr)> = HashMap::new(); // prop → (val_paired_property_name_unknown_yet, addr_expr)
    let mut writes_val: HashMap<&str, &Expr> = HashMap::new();

    for (&prop, member) in &family.members {
        if prop == ip_prop {
            continue;
        }
        let Some(body) = member.bodies.get(&key_value) else { continue };

        if let Some(c) = match_counter(body, &predicate, prop) {
            // Keep the FIRST counter — there should be at most one in a
            // well-formed loop. Multiple counters mean the predicate fits
            // multiple decrement bodies; we conservatively keep the first
            // (sorted) to make output deterministic.
            if counter.is_none() {
                counter = Some(c);
            }
            continue;
        }

        if let Some(p) = match_pointer(body, prop) {
            pointers.push(p);
            continue;
        }

        // Otherwise: try to classify as memwrite address or value.
        // We do this *purely* by shape: an address expression is a
        // StyleCondition (or unconditional value) producing either
        // `-1` (gated off) or an arithmetic address; a value expression
        // produces a slot read or a literal. We tentatively bin the body
        // by whether it has a `-1` literal in either branch position.
        match classify_memwrite_side(body) {
            MemwriteSide::AddressLike => {
                writes_addr.insert(prop, (prop, body));
            }
            MemwriteSide::ValueLike => {
                writes_val.insert(prop, body);
            }
        }
    }

    // Pair address/value memwrite slots by assignment-index proximity.
    //
    // The cabinet's emitter (in CSS-DOS, kiln) pairs related slots by
    // emitting them adjacent in the cabinet source. We exploit that
    // structurally: for each address slot, the matching value slot is
    // the one whose assignment_index is closest to it among unpaired
    // value slots, with ties broken in favour of the immediately-after
    // position. This is purely positional — no character-level reads.
    //
    // Cabinets that don't co-locate addr/val slots degrade to "pick the
    // closest available", which is no worse than the old name-sort
    // heuristic and still deterministic.
    let mut writes: Vec<WriteEntry> = Vec::new();
    let mut addr_props: Vec<&str> = writes_addr.keys().copied().collect();
    // Sort addresses by assignment_index so pairing is left-to-right
    // through the cabinet source (deterministic regardless of HashMap
    // iteration order).
    addr_props.sort_by_key(|p| family.members[*p].assignment_index);
    for ap in addr_props {
        let (_, addr_expr) = writes_addr[ap];
        let addr_idx = family.members[ap].assignment_index;
        // Find the unpaired value property whose assignment_index is
        // closest to addr_idx, preferring "immediately after" over
        // "immediately before" on a tie.
        let mut best: Option<(&str, usize, isize)> = None; // (prop, |delta|, signed_delta)
        for vp in writes_val.keys().copied() {
            let v_idx = family.members[vp].assignment_index;
            let signed = v_idx as isize - addr_idx as isize;
            let abs = signed.unsigned_abs();
            let candidate = (vp, abs, signed);
            best = match best {
                None => Some(candidate),
                Some((_, prev_abs, prev_signed)) => {
                    if abs < prev_abs || (abs == prev_abs && signed > prev_signed) {
                        Some(candidate)
                    } else {
                        best
                    }
                }
            };
        }

        if let Some((vp, _, _)) = best {
            let ve = writes_val[vp];
            writes_val.remove(vp);
            writes.push(WriteEntry {
                addr_property: ap.to_string(),
                val_property: vp.to_string(),
                addr_expr: addr_expr.clone(),
                val_expr: ve.clone(),
            });
        } else {
            writes.push(WriteEntry {
                addr_property: ap.to_string(),
                val_property: String::new(),
                addr_expr: addr_expr.clone(),
                val_expr: Expr::Literal(0.0),
            });
        }
    }

    // A real loop has at least a counter or a pointer or a write. An IP
    // body alone is not enough — that would describe a bare unconditional
    // jump-back, with no termination. Refuse those (cardinal-rule:
    // unbounded loops aren't safe to fast-forward).
    if counter.is_none() && pointers.is_empty() && writes.is_empty() {
        return None;
    }

    // For determinism, sort pointer entries by property name.
    pointers.sort_by(|a, b| a.property.cmp(&b.property));

    let bulk_class = classify_bulk(&pointers, &writes);

    Some(LoopDescriptor {
        key_property: family.key_property.clone(),
        key_value,
        ip_property: ip_prop.to_string(),
        ip_self_property: ip_shape.self_property,
        ip_advance_literal: ip_shape.advance_literal,
        predicate_properties,
        predicate,
        counter,
        pointers,
        writes,
        flag_conditioned,
        bulk_class,
    })
}

/// Classify the bulk-applier shape of a recognised loop, structurally.
///
/// See [`BulkClass`]. This consults the pointer entries' `self_property`
/// strings (the prior-tick mirror slots) and asks, for each write's
/// value expression, whether any of those mirror names appear anywhere
/// in the expression tree. Pure name-equality on whole names — no
/// substring or character inspection.
fn classify_bulk(pointers: &[PointerEntry], writes: &[WriteEntry]) -> BulkClass {
    if writes.is_empty() {
        return BulkClass::ReadOnly;
    }
    let mirrors: HashSet<&str> = pointers
        .iter()
        .map(|p| p.self_property.as_str())
        .collect();
    let mut any_copy = false;
    for w in writes {
        if expr_references_any(&w.val_expr, &mirrors) {
            any_copy = true;
        }
    }
    if any_copy {
        BulkClass::Copy
    } else {
        BulkClass::Fill
    }
}

/// True iff `expr` transitively references any `Expr::Var { name, .. }`
/// or `StyleTest::Single { property, .. }` whose name is in `names`.
/// Whole-name equality only — no substring matching.
fn expr_references_any(expr: &Expr, names: &HashSet<&str>) -> bool {
    match expr {
        Expr::Var { name, fallback } => {
            if names.contains(name.as_str()) {
                return true;
            }
            if let Some(fb) = fallback {
                return expr_references_any(fb, names);
            }
            false
        }
        Expr::Literal(_) | Expr::StringLiteral(_) => false,
        Expr::Calc(op) => calc_references_any(op, names),
        Expr::StyleCondition { branches, fallback } => {
            for b in branches {
                if test_references_any(&b.condition, names) || expr_references_any(&b.then, names) {
                    return true;
                }
            }
            expr_references_any(fallback, names)
        }
        Expr::FunctionCall { args, .. } => {
            args.iter().any(|a| expr_references_any(a, names))
        }
        Expr::Concat(parts) => parts.iter().any(|p| expr_references_any(p, names)),
    }
}

fn calc_references_any(op: &CalcOp, names: &HashSet<&str>) -> bool {
    match op {
        CalcOp::Add(a, b)
        | CalcOp::Sub(a, b)
        | CalcOp::Mul(a, b)
        | CalcOp::Div(a, b)
        | CalcOp::Mod(a, b)
        | CalcOp::Pow(a, b) => {
            expr_references_any(a, names) || expr_references_any(b, names)
        }
        CalcOp::Min(args) | CalcOp::Max(args) => {
            args.iter().any(|a| expr_references_any(a, names))
        }
        CalcOp::Clamp(a, b, c) => {
            expr_references_any(a, names)
                || expr_references_any(b, names)
                || expr_references_any(c, names)
        }
        CalcOp::Round(_, a, b) => {
            expr_references_any(a, names) || expr_references_any(b, names)
        }
        CalcOp::Sign(a) | CalcOp::Abs(a) | CalcOp::Negate(a) => expr_references_any(a, names),
    }
}

fn test_references_any(test: &StyleTest, names: &HashSet<&str>) -> bool {
    match test {
        StyleTest::Single { property, value } => {
            names.contains(property.as_str()) || expr_references_any(value, names)
        }
        StyleTest::And(parts) | StyleTest::Or(parts) => {
            parts.iter().any(|p| test_references_any(p, names))
        }
    }
}

// ---------------------------------------------------------------------------
// Shape matchers.
// ---------------------------------------------------------------------------

/// The shape extracted from an IP-body that matches stay-or-advance.
#[derive(Debug, Clone)]
struct IpShape {
    /// The slot read by both branches.
    self_property: String,
    /// The "advance" literal — the integer offset added on the exit branch.
    advance_literal: i32,
    /// The predicate guarding "stay" vs "advance".
    predicate: StyleTest,
}

/// Match an IP-body whose shape is "stay-here-or-advance".
///
/// Two structural variants are accepted, both purely in terms of CSS
/// shape — the recogniser does not look at any property name:
///
/// 1. **Single-predicate (STOS/MOVS/LODS form).**
///    `if(<pred>: <X>; else: <Y>)` where one of `<X>` / `<Y>` is
///    `calc(self - <subtrahend>)` (the loop-stay branch) and the other
///    is `calc(self + <integer literal>)` (the loop-advance branch).
///    The two outcomes share the same `self` slot.
///
/// 2. **Disjunctive-predicate (CMPS/SCAS form).**
///    `if(<P1>: stay; <P2>: stay; ...; <Pn>: stay; else: advance)` —
///    multiple branches all yielding the same stay body, with the
///    fallback being the advance body. Or symmetrically, multiple
///    branches all advancing with the fallback staying. The synthesised
///    predicate is `Or(P1, P2, ..., Pn)`.
///
/// The predicate stored in the descriptor is the test as it appears (or
/// the synthesised disjunction), NOT normalised — phase 2 evaluates it
/// directly. If the stay branch was the `else` (i.e. kiln emitted the
/// inverted shape), phase 2 just reads the predicate and inverts its
/// outcome there; phase 1's job is only to extract the structural fact
/// that this is an IP body, not to canonicalise it.
fn match_ip_stay_or_advance(body: &Expr) -> Option<IpShape> {
    let Expr::StyleCondition { branches, fallback } = body else { return None };
    if branches.is_empty() {
        return None;
    }
    let else_ = fallback.as_ref();

    if branches.len() == 1 {
        // Single-branch form (STOS/MOVS/LODS). Try both orientations.
        let then = &branches[0].then;
        if let Some(s) = match_ip_orientation(then, else_, &branches[0].condition) {
            return Some(s);
        }
        return match_ip_orientation(else_, then, &branches[0].condition);
    }

    // Multi-branch form (CMPS/SCAS): all branch `then`s must be
    // structurally equal; fallback is the other side. Two orientations:
    //
    //   - All branches stay; fallback advances. Predicate = OR(branch
    //     conditions).
    //   - All branches advance; fallback stays. Predicate = OR(branch
    //     conditions), but inverted in meaning. We capture it as-is and
    //     let phase 2 choose the polarity.
    //
    // Equality is recursive structural equality on Expr (PartialEq).
    let first_then = &branches[0].then;
    if !branches.iter().all(|b| &b.then == first_then) {
        return None;
    }
    let conditions: Vec<StyleTest> = branches.iter().map(|b| b.condition.clone()).collect();
    let synth_predicate = StyleTest::Or(conditions);

    // Try (then=stay, else=advance) first, then the inverse.
    if let Some(s) = match_ip_orientation(first_then, else_, &synth_predicate) {
        return Some(s);
    }
    match_ip_orientation(else_, first_then, &synth_predicate)
}

fn match_ip_orientation(
    stay: &Expr,
    advance: &Expr,
    predicate: &StyleTest,
) -> Option<IpShape> {
    let (stay_self, _stay_offset) = match_calc_sub_var(stay)?;
    let (advance_self, advance_lit) = match_calc_add_var_lit(advance)?;
    if stay_self != advance_self {
        return None;
    }
    Some(IpShape {
        self_property: stay_self,
        advance_literal: advance_lit,
        predicate: predicate.clone(),
    })
}

/// Match `calc(var(name) - <anything>)`. Returns the var name and the
/// subtrahend.
fn match_calc_sub_var(expr: &Expr) -> Option<(String, &Expr)> {
    let Expr::Calc(CalcOp::Sub(a, b)) = expr else { return None };
    let Expr::Var { name, .. } = a.as_ref() else { return None };
    Some((name.clone(), b.as_ref()))
}

/// Match `calc(var(name) + <integer literal>)`. Returns var name and the
/// integer literal value as i32.
fn match_calc_add_var_lit(expr: &Expr) -> Option<(String, i32)> {
    let Expr::Calc(CalcOp::Add(a, b)) = expr else { return None };
    let Expr::Var { name, .. } = a.as_ref() else { return None };
    let Expr::Literal(v) = b.as_ref() else { return None };
    if v.fract() != 0.0 {
        return None;
    }
    Some((name.clone(), *v as i32))
}

/// Match the counter shape:
/// `if(<pred-equiv-or-rep-guard>: self; else: max(0, calc(self - 1)))`.
/// We accept any predicate equivalent to the loop predicate's *negation*
/// being the trigger for decrement — but at this level we don't try to
/// prove logical equivalence. Two acceptable shapes:
///
/// 1. `if(<P>: self; else: max(0, self - 1))` — decrement on else.
/// 2. The exact rep-guard kiln emits, which encodes "no rep prefix → keep
///    self; else decrement". Recognised structurally as
///    `if(<some-style-cond>: var(self); else: max(0, calc(var(self) - 1)))`.
///
/// Returns the structural Counter even if the inner predicate doesn't
/// textually match `predicate`; the runtime path in phase 2 evaluates
/// each predicate independently anyway. Phase 1 only cares about the
/// shape.
fn match_counter(body: &Expr, _predicate: &StyleTest, prop: &str) -> Option<CounterEntry> {
    let Expr::StyleCondition { branches, fallback } = body else { return None };
    if branches.len() != 1 {
        return None;
    }
    let then = &branches[0].then;
    let else_ = fallback.as_ref();

    // Either (then=self, else=decrement) or (then=decrement, else=self).
    // The recogniser doesn't care which orientation the cabinet picks.
    if let Some(c) = match_counter_orientation(then, else_, prop) {
        return Some(c);
    }
    match_counter_orientation(else_, then, prop)
}

fn match_counter_orientation(
    self_branch: &Expr,
    decrement_branch: &Expr,
    prop: &str,
) -> Option<CounterEntry> {
    // self_branch = var(self)
    let Expr::Var { name: self_name, .. } = self_branch else { return None };

    // decrement_branch = max(0, calc(self - step))
    let Expr::Calc(CalcOp::Max(args)) = decrement_branch else { return None };
    if args.len() != 2 {
        return None;
    }
    let zero_ok = matches!(&args[0], Expr::Literal(v) if *v == 0.0);
    if !zero_ok {
        return None;
    }
    let Expr::Calc(CalcOp::Sub(a, b)) = &args[1] else { return None };
    let Expr::Var { name: dec_name, .. } = a.as_ref() else { return None };
    let Expr::Literal(step) = b.as_ref() else { return None };
    if step.fract() != 0.0 || *step <= 0.0 {
        return None;
    }
    if dec_name != self_name {
        return None;
    }

    Some(CounterEntry {
        property: prop.to_string(),
        self_property: self_name.clone(),
        step: *step as i32,
    })
}

/// Match the pointer shape:
///
///   `if(<gate>: <A>; else: <B>)`
///
/// where exactly ONE of `<A>` / `<B>` is `var(self)` (the guard
/// branch) and the OTHER is the update expression
/// `funcCall(calc(var(self) + k - call(var(flag), n) * 2k), 16)` —
/// kiln's `--lowerBytes(... , 16)` shape for a 16-bit modular pointer
/// step under a direction flag.
///
/// The recogniser does NOT inspect the function names — it accepts ANY
/// outer 2-arg function call whose second arg is the literal 16, AND
/// any inner 2-arg function call whose second arg is a small integer
/// (the flag bit position). The shape is what matters; the names are
/// how the cabinet exposes that shape, and a 6502 cabinet calling its
/// equivalents `--lowBytes(_, 8)` would still get caught (modulo the
/// "16" being whatever modulus the cabinet uses; we accept any literal
/// modulus ≥ 2).
fn match_pointer(body: &Expr, prop: &str) -> Option<PointerEntry> {
    let Expr::StyleCondition { branches, fallback } = body else { return None };
    if branches.len() != 1 {
        return None;
    }
    let then = &branches[0].then;
    let else_ = fallback.as_ref();

    // Exactly one of {then, else} should be a bare var(self) (the guard
    // branch), and the other should be the update expression. Try both
    // orderings and return whichever matches.
    if let Some(p) = match_pointer_with_orientation(then, else_, prop) {
        return Some(p);
    }
    match_pointer_with_orientation(else_, then, prop)
}

/// Try matching with `guard_branch` = the `var(self)` side and
/// `update_branch` = the update-expression side.
fn match_pointer_with_orientation(
    guard_branch: &Expr,
    update_branch: &Expr,
    prop: &str,
) -> Option<PointerEntry> {
    let Expr::Var { name: self_guard, .. } = guard_branch else { return None };

    // update_branch = outerCall(inner, modulus_literal) where inner is
    //   calc(var(self) + k - innerCall(var(flag), bit) * (2k))
    let Expr::FunctionCall { name: _outer_name, args: outer_args } = update_branch else {
        return None;
    };
    if outer_args.len() != 2 {
        return None;
    }
    let Expr::Literal(modulus) = &outer_args[1] else { return None };
    if modulus.fract() != 0.0 || *modulus < 2.0 {
        return None;
    }

    // inner: calc(<calc(var(self) + k)> - <innerCall(var(flag), bit)> * (2k))
    //
    // Tree shape from kiln (after parsing `calc(a + b - c * d)`):
    //   Calc::Sub(Calc::Add(Var(self), Lit(k)), Calc::Mul(Call(flag, bit), Lit(2k)))
    let Expr::Calc(CalcOp::Sub(addexpr, mulexpr)) = &outer_args[0] else { return None };

    let Expr::Calc(CalcOp::Add(self_var, k_lit)) = addexpr.as_ref() else { return None };
    let Expr::Var { name: self_name, .. } = self_var.as_ref() else { return None };
    if self_name != self_guard {
        return None;
    }
    let Expr::Literal(k) = k_lit.as_ref() else { return None };
    if k.fract() != 0.0 || *k <= 0.0 {
        return None;
    }
    let base_step = *k as i32;

    let Expr::Calc(CalcOp::Mul(callexpr, twok_lit)) = mulexpr.as_ref() else { return None };
    let Expr::Literal(twok) = twok_lit.as_ref() else { return None };
    if (*twok - 2.0 * (*k)).abs() > f64::EPSILON {
        return None;
    }
    let Expr::FunctionCall { name: _inner_name, args: inner_args } = callexpr.as_ref() else {
        return None;
    };
    if inner_args.len() != 2 {
        return None;
    }
    let Expr::Var { name: flag_name, .. } = &inner_args[0] else { return None };
    let Expr::Literal(bit) = &inner_args[1] else { return None };
    if bit.fract() != 0.0 || *bit < 0.0 || *bit > 31.0 {
        return None;
    }

    Some(PointerEntry {
        property: prop.to_string(),
        self_property: self_name.clone(),
        base_step,
        flag_property: flag_name.clone(),
        flag_bit: *bit as u32,
    })
}

#[derive(Debug)]
enum MemwriteSide {
    AddressLike,
    ValueLike,
}

/// Tentative classifier for a per-V body that didn't match counter or
/// pointer. The kiln-emitted memwrite address slots collapse to `-1`
/// when no write should fire (gated through `repGuardAddr`); value
/// slots don't have that sentinel. So if we see a `-1` literal as the
/// "stay" branch of a `StyleCondition`, treat it as address-like.
/// Everything else is value-like.
///
/// Phase 1 only tags these tentatively; phase 2 will pair them by slot
/// index based on the cabinet's memwrite-slot assignment ordering. We
/// keep the address/value bodies separate here so the descriptor still
/// captures useful structural info even before pairing.
fn classify_memwrite_side(body: &Expr) -> MemwriteSide {
    if expression_contains_neg_one_literal(body) {
        MemwriteSide::AddressLike
    } else {
        MemwriteSide::ValueLike
    }
}

fn expression_contains_neg_one_literal(expr: &Expr) -> bool {
    match expr {
        Expr::Literal(v) => *v == -1.0,
        Expr::Calc(op) => calc_contains_neg_one(op),
        Expr::StyleCondition { branches, fallback } => {
            for b in branches {
                if expression_contains_neg_one_literal(&b.then) {
                    return true;
                }
            }
            expression_contains_neg_one_literal(fallback)
        }
        Expr::FunctionCall { args, .. } => args.iter().any(expression_contains_neg_one_literal),
        Expr::Concat(parts) => parts.iter().any(expression_contains_neg_one_literal),
        _ => false,
    }
}

fn calc_contains_neg_one(op: &CalcOp) -> bool {
    match op {
        CalcOp::Add(a, b)
        | CalcOp::Sub(a, b)
        | CalcOp::Mul(a, b)
        | CalcOp::Div(a, b)
        | CalcOp::Mod(a, b)
        | CalcOp::Pow(a, b) => {
            expression_contains_neg_one_literal(a) || expression_contains_neg_one_literal(b)
        }
        CalcOp::Min(args) | CalcOp::Max(args) => {
            args.iter().any(expression_contains_neg_one_literal)
        }
        CalcOp::Clamp(a, b, c) => {
            expression_contains_neg_one_literal(a)
                || expression_contains_neg_one_literal(b)
                || expression_contains_neg_one_literal(c)
        }
        CalcOp::Round(_, a, b) => {
            expression_contains_neg_one_literal(a) || expression_contains_neg_one_literal(b)
        }
        CalcOp::Sign(a) | CalcOp::Abs(a) | CalcOp::Negate(a) => {
            expression_contains_neg_one_literal(a)
        }
    }
}

// ---------------------------------------------------------------------------
// Predicate helpers.
// ---------------------------------------------------------------------------

fn collect_property_names_from_predicate(test: &StyleTest) -> Vec<String> {
    let mut out = Vec::new();
    walk_test(test, &mut |t| {
        if let StyleTest::Single { property, .. } = t {
            if !out.contains(property) {
                out.push(property.clone());
            }
        }
    });
    out
}

fn predicate_mentions_flag_bit(test: &StyleTest) -> bool {
    // Heuristic: if the predicate has more than one Single test on
    // distinct properties, AND-combined, treat it as flag-conditioned.
    // The exact "what flag, what bit" extraction lives in phase 2; phase
    // 1 only flags the descriptor as needing flag-aware semantics.
    let mut props: HashSet<String> = HashSet::new();
    walk_test(test, &mut |t| {
        if let StyleTest::Single { property, .. } = t {
            props.insert(property.clone());
        }
    });
    props.len() > 1
}

fn walk_test<F: FnMut(&StyleTest)>(test: &StyleTest, f: &mut F) {
    f(test);
    match test {
        StyleTest::Single { .. } => {}
        StyleTest::And(parts) | StyleTest::Or(parts) => {
            for p in parts {
                walk_test(p, f);
            }
        }
    }
}

#[cfg(test)]
mod tests;
