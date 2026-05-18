//! Per-dispatch-key specialisation — discovery + Expr-level partial
//! evaluator.
//!
//! ## What this is
//!
//! Phases 1-2 of the per-dispatch-key specialisation plan (CSS-DOS
//! `docs/plans/2026-05-12-per-dispatch-key-specialisation.md`). No
//! runtime integration yet — pure compile-time structural transforms.
//!
//! Phase-1 primitives (discovery):
//!
//! - `discover_hot_key(assignments)` — return the custom-property name
//!   that the most `StyleCondition`-`Single` tests across all
//!   assignments branch on, plus the count.
//! - `discover_key_value_set(assignments, key)` — return the set of
//!   literal `i64` values that `Single` tests on that key compare
//!   against.
//!
//! Phase-2 primitives (specialisation):
//!
//! - `specialise_assignments(&mut [Assignment], key, value)` —
//!   partial-evaluate every assignment's `Expr` tree against the
//!   binding `(key = value)`. Drops branches whose test rejects the
//!   binding, folds chains decided by a guaranteed-true test, leaves
//!   undecidable branches in place.
//! - `specialise_expr(&mut Expr, key, value)` — same, on a single tree.
//!
//! ## Cardinal-rule defence
//!
//! Everything here is purely structural:
//!
//! - We read property *names* but never read characters out of them
//!   (no prefix matching, no convention-based filtering).
//! - We read literal *values* but never assign meaning to them
//!   (0x40 is just `64`).
//! - The hot-key choice is fully determined by `(count, name)`. The
//!   secondary lexicographic ordering on name is for determinism
//!   only — same cabinet, same answer, on any machine.
//! - Specialisation evaluates `StyleTest`s against the binding using
//!   only `Single { property == key, value == Literal }` resolution
//!   + And/Or shape. It never tries to evaluate `Var(...)` against
//!   current cabinet state.
//!
//! A 6502 cabinet's hot dispatch key would be discovered identically.
//! A brainfuck cabinet's `--currentCommand` key would be picked if it
//! were the most-tested key. The rule is "whichever property the most
//! `StyleCondition`-`Single` tests reference, with at least two distinct
//! literal values to specialise against."

use std::collections::BTreeSet;
use std::collections::HashMap;

use crate::types::{Assignment, CalcOp, Expr, StyleBranch, StyleTest};

/// Result of `discover_hot_key`: the chosen key and how many
/// `StyleCondition`-`Single` tests reference it across all walked
/// assignments. `None` means the cabinet has no `StyleCondition`-`Single`
/// tests at all (no specialisable shape).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotKey {
    pub name: String,
    pub count: usize,
}

/// Walk every assignment's `Expr` tree and tally, per property name,
/// the number of `StyleTest::Single` references with that name as the
/// branch's tested property. Returns the top-ranked key by count,
/// breaking ties by lexicographic order on the name (so the answer
/// is deterministic across runs).
pub fn discover_hot_key(assignments: &[Assignment]) -> Option<HotKey> {
    let counts = count_keys(assignments);
    let mut ranked: Vec<(&String, &usize)> = counts.iter().collect();
    if ranked.is_empty() {
        return None;
    }
    // Highest count first, then lexicographic on name for ties.
    ranked.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let (name, count) = ranked[0];
    Some(HotKey {
        name: name.clone(),
        count: *count,
    })
}

/// Return the top-K dispatch keys by count, lexicographic name break
/// for ties. Useful for the diagnostic line and for surveying cabinets
/// where the top key has a near-tie with the runner-up.
pub fn rank_dispatch_keys(assignments: &[Assignment], top_k: usize) -> Vec<HotKey> {
    let counts = count_keys(assignments);
    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked
        .into_iter()
        .take(top_k)
        .map(|(name, count)| HotKey { name, count })
        .collect()
}

/// Collect the set of literal `i64` values that `StyleTest::Single`
/// tests on `key` compare against. Non-literal RHS values are skipped
/// (they're not specialisable as compile-time constants).
///
/// Returned as a `BTreeSet` for deterministic iteration order.
pub fn discover_key_value_set(
    assignments: &[Assignment],
    key: &str,
) -> BTreeSet<i64> {
    let mut values = BTreeSet::new();
    for a in assignments {
        collect_values_in_expr(&a.value, key, &mut values);
    }
    values
}

fn count_keys(assignments: &[Assignment]) -> HashMap<String, usize> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for a in assignments {
        walk_collect_keys(&a.value, &mut counts);
    }
    counts
}

fn walk_collect_keys(e: &Expr, counts: &mut HashMap<String, usize>) {
    match e {
        Expr::StyleCondition { branches, fallback } => {
            for b in branches {
                collect_keys_in_test(&b.condition, counts);
                walk_collect_keys(&b.then, counts);
            }
            walk_collect_keys(fallback, counts);
        }
        Expr::Calc(op) => walk_calc_keys(op, counts),
        Expr::Concat(parts) => {
            for p in parts {
                walk_collect_keys(p, counts);
            }
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                walk_collect_keys(a, counts);
            }
        }
        Expr::Var { fallback, .. } => {
            if let Some(fb) = fallback {
                walk_collect_keys(fb, counts);
            }
        }
        Expr::Literal(_) | Expr::StringLiteral(_) => {}
    }
}

fn walk_calc_keys(op: &CalcOp, counts: &mut HashMap<String, usize>) {
    match op {
        CalcOp::Add(a, b)
        | CalcOp::Sub(a, b)
        | CalcOp::Mul(a, b)
        | CalcOp::Div(a, b)
        | CalcOp::Mod(a, b)
        | CalcOp::Pow(a, b) => {
            walk_collect_keys(a, counts);
            walk_collect_keys(b, counts);
        }
        CalcOp::Min(args) | CalcOp::Max(args) => {
            for x in args {
                walk_collect_keys(x, counts);
            }
        }
        CalcOp::Clamp(a, b, c) => {
            walk_collect_keys(a, counts);
            walk_collect_keys(b, counts);
            walk_collect_keys(c, counts);
        }
        CalcOp::Round(_, a, b) => {
            walk_collect_keys(a, counts);
            walk_collect_keys(b, counts);
        }
        CalcOp::Sign(a) | CalcOp::Abs(a) | CalcOp::Negate(a) => {
            walk_collect_keys(a, counts);
        }
    }
}

fn collect_keys_in_test(t: &StyleTest, counts: &mut HashMap<String, usize>) {
    match t {
        StyleTest::Single { property, .. } => {
            *counts.entry(property.clone()).or_insert(0) += 1;
        }
        StyleTest::And(parts) | StyleTest::Or(parts) => {
            for p in parts {
                collect_keys_in_test(p, counts);
            }
        }
    }
}

fn collect_values_in_expr(e: &Expr, key: &str, values: &mut BTreeSet<i64>) {
    match e {
        Expr::StyleCondition { branches, fallback } => {
            for b in branches {
                collect_values_in_test(&b.condition, key, values);
                collect_values_in_expr(&b.then, key, values);
            }
            collect_values_in_expr(fallback, key, values);
        }
        Expr::Calc(op) => collect_values_in_calc(op, key, values),
        Expr::Concat(parts) => {
            for p in parts {
                collect_values_in_expr(p, key, values);
            }
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                collect_values_in_expr(a, key, values);
            }
        }
        Expr::Var { fallback, .. } => {
            if let Some(fb) = fallback {
                collect_values_in_expr(fb, key, values);
            }
        }
        Expr::Literal(_) | Expr::StringLiteral(_) => {}
    }
}

fn collect_values_in_calc(op: &CalcOp, key: &str, values: &mut BTreeSet<i64>) {
    match op {
        CalcOp::Add(a, b)
        | CalcOp::Sub(a, b)
        | CalcOp::Mul(a, b)
        | CalcOp::Div(a, b)
        | CalcOp::Mod(a, b)
        | CalcOp::Pow(a, b) => {
            collect_values_in_expr(a, key, values);
            collect_values_in_expr(b, key, values);
        }
        CalcOp::Min(args) | CalcOp::Max(args) => {
            for x in args {
                collect_values_in_expr(x, key, values);
            }
        }
        CalcOp::Clamp(a, b, c) => {
            collect_values_in_expr(a, key, values);
            collect_values_in_expr(b, key, values);
            collect_values_in_expr(c, key, values);
        }
        CalcOp::Round(_, a, b) => {
            collect_values_in_expr(a, key, values);
            collect_values_in_expr(b, key, values);
        }
        CalcOp::Sign(a) | CalcOp::Abs(a) | CalcOp::Negate(a) => {
            collect_values_in_expr(a, key, values);
        }
    }
}

fn collect_values_in_test(t: &StyleTest, key: &str, values: &mut BTreeSet<i64>) {
    match t {
        StyleTest::Single { property, value } => {
            if property == key {
                if let Expr::Literal(lit) = value {
                    values.insert(*lit as i64);
                }
            }
            // Recurse into the test's value expression in case it contains
            // a nested StyleCondition (rare but possible).
            collect_values_in_expr(value, key, values);
        }
        StyleTest::And(parts) | StyleTest::Or(parts) => {
            for p in parts {
                collect_values_in_test(p, key, values);
            }
        }
    }
}

// ----------------------------------------------------------------------
// Phase 3 prologue/tail split: given a target property name (typically
// the hot dispatch key, e.g. `--opcode`), find the transitive closure
// of assignments whose computation that property depends on. The
// returned set is the "prologue" — assignments that must run before
// the dispatch key's value is known, so they CAN'T be specialised on
// the key's value. The complement is the "tail" — what we specialise.
//
// Genericity: works for any target property name. No knowledge of
// what `--opcode` means. The set is purely "what feeds into the
// computation of this property name."
// ----------------------------------------------------------------------

use std::collections::HashSet;

/// Find the transitive closure of property names whose computation the
/// `target` property depends on. The returned set includes `target`
/// itself (the prologue must compute `target`).
///
/// Walks each assignment's Expr tree backwards from `target`. For
/// every `Var { name }` reference encountered, if there's an assignment
/// computing that name, recurse into its Expr. Stops at properties not
/// computed by any assignment in the slice (e.g. state-var reads —
/// those are valid prologue inputs, not transitively-walked deps).
///
/// Returns a `HashSet<String>` of property names (including the leading
/// `--`). Empty set means `target` has no assignment in the slice.
pub fn find_prologue_properties(
    assignments: &[Assignment],
    target: &str,
) -> HashSet<String> {
    // Index by property name → Expr for O(1) lookup.
    let by_name: HashMap<&str, &Expr> = assignments
        .iter()
        .map(|a| (a.property.as_str(), &a.value))
        .collect();

    let mut closure: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = Vec::new();

    if by_name.contains_key(target) {
        stack.push(target.to_string());
    }

    while let Some(name) = stack.pop() {
        if !closure.insert(name.clone()) {
            continue; // already visited
        }
        if let Some(expr) = by_name.get(name.as_str()) {
            let mut refs: HashSet<String> = HashSet::new();
            collect_var_refs(expr, &mut refs);
            for r in refs {
                if !closure.contains(&r) && by_name.contains_key(r.as_str()) {
                    stack.push(r);
                }
            }
        }
    }

    closure
}

/// Walk an Expr collecting every `Var { name }` reference (including
/// those inside `style(--prop: literal)` tests, since those property
/// names ARE dependencies — the test reads that property's value).
/// Used internally by `find_prologue_properties`.
fn collect_var_refs(e: &Expr, out: &mut HashSet<String>) {
    match e {
        Expr::Var { name, fallback } => {
            out.insert(name.clone());
            if let Some(fb) = fallback {
                collect_var_refs(fb, out);
            }
        }
        Expr::StyleCondition { branches, fallback } => {
            for b in branches {
                collect_var_refs_in_test(&b.condition, out);
                collect_var_refs(&b.then, out);
            }
            collect_var_refs(fallback, out);
        }
        Expr::Calc(op) => collect_var_refs_in_calc(op, out),
        Expr::Concat(parts) => {
            for p in parts {
                collect_var_refs(p, out);
            }
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                collect_var_refs(a, out);
            }
        }
        Expr::Literal(_) | Expr::StringLiteral(_) => {}
    }
}

fn collect_var_refs_in_calc(op: &CalcOp, out: &mut HashSet<String>) {
    match op {
        CalcOp::Add(a, b)
        | CalcOp::Sub(a, b)
        | CalcOp::Mul(a, b)
        | CalcOp::Div(a, b)
        | CalcOp::Mod(a, b)
        | CalcOp::Pow(a, b) => {
            collect_var_refs(a, out);
            collect_var_refs(b, out);
        }
        CalcOp::Min(args) | CalcOp::Max(args) => {
            for x in args {
                collect_var_refs(x, out);
            }
        }
        CalcOp::Clamp(a, b, c) => {
            collect_var_refs(a, out);
            collect_var_refs(b, out);
            collect_var_refs(c, out);
        }
        CalcOp::Round(_, a, b) => {
            collect_var_refs(a, out);
            collect_var_refs(b, out);
        }
        CalcOp::Sign(a) | CalcOp::Abs(a) | CalcOp::Negate(a) => {
            collect_var_refs(a, out);
        }
    }
}

fn collect_var_refs_in_test(t: &StyleTest, out: &mut HashSet<String>) {
    match t {
        StyleTest::Single { property, value } => {
            // The `property` IS a dependency — the test reads its value.
            out.insert(property.clone());
            collect_var_refs(value, out);
        }
        StyleTest::And(parts) | StyleTest::Or(parts) => {
            for p in parts {
                collect_var_refs_in_test(p, out);
            }
        }
    }
}

/// Split a topologically-sorted assignment slice into (prologue, tail)
/// where `prologue` contains every assignment in `prologue_props` (in
/// the same relative order) and `tail` contains the rest. The relative
/// ordering within each half is preserved.
///
/// Assumes the input is already topologically sorted — the split
/// preserves dependency ordering inside each half. Crucially, no tail
/// assignment depends on another tail assignment in a way that crosses
/// the split (if it did, that other assignment would have to be in
/// the prologue, contradicting the closure definition).
pub fn split_prologue_tail(
    sorted: Vec<Assignment>,
    prologue_props: &HashSet<String>,
) -> (Vec<Assignment>, Vec<Assignment>) {
    let mut prologue = Vec::new();
    let mut tail = Vec::new();
    for a in sorted {
        if prologue_props.contains(&a.property) {
            prologue.push(a);
        } else {
            tail.push(a);
        }
    }
    (prologue, tail)
}

// ----------------------------------------------------------------------
// Phase 2: Expr-level specialisation (partial evaluation against a
// known (key, value) binding).
// ----------------------------------------------------------------------

/// Aggregate counters reported by `specialise_assignments`.
///
/// All counts are over the assignment slice *as a whole* — i.e. summed
/// across every tree walked. Useful for the phase-2 gate
/// ("StyleCondition count ≤ 10 % of unspecialised").
#[derive(Debug, Default, Clone, Copy)]
pub struct SpecialiseStats {
    /// Number of `StyleBranch`es that were dropped because their test
    /// `NeverTakes` under the binding.
    pub branches_dropped: usize,
    /// Number of `StyleCondition`s that collapsed because some branch
    /// `AlwaysTakes` under the binding (entire condition replaced with
    /// the decided branch's `then`).
    pub conditions_decided: usize,
    /// Number of `StyleCondition`s that collapsed because all their
    /// branches were dropped (only the fallback remained, so the
    /// condition was replaced with the specialised fallback).
    pub conditions_collapsed_to_fallback: usize,
    /// Number of `StyleCondition` nodes visited.
    pub conditions_visited: usize,
    /// Number of `Var { name = key }` references folded to `Literal(value)`.
    /// A direct read of the dispatch key returns the binding's value, so
    /// every such read is collapsed at compile time.
    pub var_refs_folded: usize,
}

impl SpecialiseStats {
    fn add(&mut self, o: &SpecialiseStats) {
        self.branches_dropped += o.branches_dropped;
        self.conditions_decided += o.conditions_decided;
        self.conditions_collapsed_to_fallback += o.conditions_collapsed_to_fallback;
        self.conditions_visited += o.conditions_visited;
        self.var_refs_folded += o.var_refs_folded;
    }
}

/// Outcome of testing a single `StyleTest` against a known
/// `(key, value)` binding. Public so phase-3 code paths that want to
/// reason about partial decidability can share the same shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchOutcome {
    /// The test is guaranteed to fire (every reachable `Single` on
    /// `key` matches `value`, and the compound shape resolves to true).
    AlwaysTakes,
    /// The test can never fire (some required `Single` on `key`
    /// rejects `value`, or every `Or` arm rejects).
    NeverTakes,
    /// The test reads something other than the known binding (or has
    /// a non-literal `Single` RHS); outcome can't be decided at
    /// compile time. Keep the branch.
    Unknown,
}

/// Evaluate a `StyleTest` under the binding `(key = value)`.
///
/// Only `StyleTest::Single { property == key, value == Literal(L) }`
/// is resolved. Any other shape (different property, non-literal RHS,
/// nested condition) contributes `Unknown`. Compound `And`/`Or` are
/// composed: `And` is `AlwaysTakes` iff every arm is `AlwaysTakes`,
/// `NeverTakes` if any arm rejects; `Or` is `AlwaysTakes` if any arm
/// matches, `NeverTakes` iff every arm rejects.
pub fn test_outcome(t: &StyleTest, key: &str, value: i64) -> BranchOutcome {
    match t {
        StyleTest::Single { property, value: v } => {
            if property != key {
                return BranchOutcome::Unknown;
            }
            match v {
                Expr::Literal(lit) => {
                    if (*lit as i64) == value {
                        BranchOutcome::AlwaysTakes
                    } else {
                        BranchOutcome::NeverTakes
                    }
                }
                _ => BranchOutcome::Unknown,
            }
        }
        StyleTest::And(parts) => {
            let mut all_take = true;
            for p in parts {
                match test_outcome(p, key, value) {
                    BranchOutcome::NeverTakes => return BranchOutcome::NeverTakes,
                    BranchOutcome::Unknown => all_take = false,
                    BranchOutcome::AlwaysTakes => {}
                }
            }
            if all_take {
                BranchOutcome::AlwaysTakes
            } else {
                BranchOutcome::Unknown
            }
        }
        StyleTest::Or(parts) => {
            let mut any_take = false;
            let mut all_reject = true;
            for p in parts {
                match test_outcome(p, key, value) {
                    BranchOutcome::AlwaysTakes => any_take = true,
                    BranchOutcome::NeverTakes => {}
                    BranchOutcome::Unknown => all_reject = false,
                }
            }
            if any_take {
                BranchOutcome::AlwaysTakes
            } else if all_reject {
                BranchOutcome::NeverTakes
            } else {
                BranchOutcome::Unknown
            }
        }
    }
}

/// Partial-evaluate every assignment's `Expr` tree against the binding
/// `(key = value)`. In-place. Returns aggregate stats.
///
/// See `specialise_expr` for the per-tree contract.
pub fn specialise_assignments(
    assignments: &mut [Assignment],
    key: &str,
    value: i64,
) -> SpecialiseStats {
    let mut total = SpecialiseStats::default();
    for a in assignments.iter_mut() {
        let mut s = SpecialiseStats::default();
        specialise_expr(&mut a.value, key, value, &mut s);
        total.add(&s);
    }
    total
}

/// Partial-evaluate a single `Expr` against the binding `(key = value)`.
/// In-place. The `stats` argument accumulates per-tree counters.
///
/// Rules (generic, structural):
///
/// - `StyleCondition { branches, fallback }`:
///   * Walk branches in order. Drop any branch whose test
///     `NeverTakes`.
///   * If a branch's test `AlwaysTakes`, the chain is decided at
///     compile time: replace the entire condition with that branch's
///     specialised `then`, dropping subsequent branches and the
///     fallback.
///   * Otherwise keep the branch; specialise its `then`.
///   * If no branches remain, collapse the condition to the
///     specialised fallback.
/// - `Calc` / `Concat` / `FunctionCall` / `Var.fallback`: recurse into
///   children.
/// - `Var` / `Literal` / `StringLiteral`: leaf; no change.
pub fn specialise_expr(expr: &mut Expr, key: &str, value: i64, stats: &mut SpecialiseStats) {
    match expr {
        Expr::StyleCondition { branches, fallback } => {
            stats.conditions_visited += 1;
            let mut new_branches: Vec<StyleBranch> = Vec::with_capacity(branches.len());
            let mut decided: Option<Expr> = None;
            for b in branches.drain(..) {
                match test_outcome(&b.condition, key, value) {
                    BranchOutcome::NeverTakes => {
                        stats.branches_dropped += 1;
                    }
                    BranchOutcome::AlwaysTakes => {
                        // Chain is decided. Specialise this branch's `then`
                        // and use it as the whole result.
                        let mut t = b.then;
                        specialise_expr(&mut t, key, value, stats);
                        decided = Some(t);
                        break;
                    }
                    BranchOutcome::Unknown => {
                        let StyleBranch { condition, mut then } = b;
                        specialise_expr(&mut then, key, value, stats);
                        new_branches.push(StyleBranch { condition, then });
                    }
                }
            }

            if let Some(t) = decided {
                stats.conditions_decided += 1;
                *expr = t;
                return;
            }

            // No branch was guaranteed; specialise fallback too.
            specialise_expr(fallback, key, value, stats);

            if new_branches.is_empty() {
                stats.conditions_collapsed_to_fallback += 1;
                let fb = std::mem::replace(fallback.as_mut(), Expr::Literal(0.0));
                *expr = fb;
            } else {
                *branches = new_branches;
            }
        }
        Expr::Calc(op) => specialise_calc(op, key, value, stats),
        Expr::Concat(parts) => {
            for p in parts {
                specialise_expr(p, key, value, stats);
            }
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                specialise_expr(a, key, value, stats);
            }
        }
        Expr::Var { name, fallback } => {
            // If this Var references the binding's key, fold it to a literal.
            // The binding `(key = value)` means every read of `key` returns
            // `value`. Tracked separately from style-condition folding so the
            // stats stay informative.
            if name == key {
                stats.var_refs_folded += 1;
                *expr = Expr::Literal(value as f64);
                return;
            }
            if let Some(fb) = fallback {
                specialise_expr(fb, key, value, stats);
            }
        }
        Expr::Literal(_) | Expr::StringLiteral(_) => {}
    }
}

fn specialise_calc(op: &mut CalcOp, key: &str, value: i64, stats: &mut SpecialiseStats) {
    match op {
        CalcOp::Add(a, b)
        | CalcOp::Sub(a, b)
        | CalcOp::Mul(a, b)
        | CalcOp::Div(a, b)
        | CalcOp::Mod(a, b)
        | CalcOp::Pow(a, b) => {
            specialise_expr(a, key, value, stats);
            specialise_expr(b, key, value, stats);
        }
        CalcOp::Min(args) | CalcOp::Max(args) => {
            for x in args {
                specialise_expr(x, key, value, stats);
            }
        }
        CalcOp::Clamp(a, b, c) => {
            specialise_expr(a, key, value, stats);
            specialise_expr(b, key, value, stats);
            specialise_expr(c, key, value, stats);
        }
        CalcOp::Round(_, a, b) => {
            specialise_expr(a, key, value, stats);
            specialise_expr(b, key, value, stats);
        }
        CalcOp::Sign(a) | CalcOp::Abs(a) | CalcOp::Negate(a) => {
            specialise_expr(a, key, value, stats);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{StyleBranch, StyleTest};

    fn lit(v: f64) -> Expr {
        Expr::Literal(v)
    }

    fn var(name: &str) -> Expr {
        Expr::Var {
            name: name.to_string(),
            fallback: None,
        }
    }

    fn single(key: &str, value: i64) -> StyleTest {
        StyleTest::Single {
            property: key.to_string(),
            value: Expr::Literal(value as f64),
        }
    }

    fn branch(cond: StyleTest, then: Expr) -> StyleBranch {
        StyleBranch { condition: cond, then }
    }

    fn cond(branches: Vec<StyleBranch>, fallback: Expr) -> Expr {
        Expr::StyleCondition {
            branches,
            fallback: Box::new(fallback),
        }
    }

    fn assign(prop: &str, value: Expr) -> Assignment {
        Assignment {
            property: prop.to_string(),
            value,
        }
    }

    #[test]
    fn returns_none_on_empty_assignments() {
        assert!(discover_hot_key(&[]).is_none());
    }

    #[test]
    fn returns_none_when_no_style_conditions() {
        let a = assign("--x", lit(42.0));
        let b = assign("--y", var("--z"));
        assert!(discover_hot_key(&[a, b]).is_none());
    }

    #[test]
    fn picks_the_only_dispatch_key() {
        let a = assign(
            "--out",
            cond(
                vec![
                    branch(single("--op", 1), lit(10.0)),
                    branch(single("--op", 2), lit(20.0)),
                    branch(single("--op", 3), lit(30.0)),
                ],
                lit(0.0),
            ),
        );
        let hot = discover_hot_key(&[a]).expect("should find key");
        assert_eq!(hot.name, "--op");
        assert_eq!(hot.count, 3);
    }

    #[test]
    fn picks_the_most_referenced_key() {
        // Two assignments. --op tested 3 times, --rm tested 2 times.
        let a = assign(
            "--A",
            cond(
                vec![
                    branch(single("--op", 1), lit(1.0)),
                    branch(single("--op", 2), lit(2.0)),
                ],
                lit(0.0),
            ),
        );
        let b = assign(
            "--B",
            cond(
                vec![
                    branch(single("--op", 5), lit(5.0)),
                    branch(single("--rm", 0), lit(50.0)),
                    branch(single("--rm", 1), lit(51.0)),
                ],
                lit(0.0),
            ),
        );
        let hot = discover_hot_key(&[a, b]).expect("should find key");
        assert_eq!(hot.name, "--op");
        assert_eq!(hot.count, 3);
    }

    #[test]
    fn breaks_ties_by_lexicographic_name() {
        // --bbb and --aaa both tested twice. Lexicographic break → --aaa.
        let a = assign(
            "--out",
            cond(
                vec![
                    branch(single("--bbb", 1), lit(1.0)),
                    branch(single("--bbb", 2), lit(2.0)),
                    branch(single("--aaa", 1), lit(10.0)),
                    branch(single("--aaa", 2), lit(20.0)),
                ],
                lit(0.0),
            ),
        );
        let hot = discover_hot_key(&[a]).expect("should find key");
        assert_eq!(hot.name, "--aaa");
        assert_eq!(hot.count, 2);
    }

    #[test]
    fn descends_into_calc_and_function_call() {
        // The key reference is buried inside calc(... + funcCall(cond(...)))
        let inner = cond(
            vec![branch(single("--deep", 7), lit(700.0))],
            lit(0.0),
        );
        let fc = Expr::FunctionCall {
            name: "--readMem".to_string(),
            args: vec![inner],
        };
        let outer_calc = Expr::Calc(CalcOp::Add(Box::new(fc), Box::new(lit(1.0))));
        let a = assign("--AX", outer_calc);
        let hot = discover_hot_key(&[a]).expect("should find key");
        assert_eq!(hot.name, "--deep");
        assert_eq!(hot.count, 1);
    }

    #[test]
    fn handles_and_or_compound_tests() {
        // A compound test that references --op twice and --rm once.
        let compound = StyleTest::And(vec![
            single("--op", 0x88),
            StyleTest::Or(vec![single("--op", 0x89), single("--rm", 3)]),
        ]);
        let a = assign(
            "--mem",
            cond(vec![branch(compound, lit(1.0))], lit(0.0)),
        );
        let hot = discover_hot_key(&[a]).expect("should find key");
        assert_eq!(hot.name, "--op");
        assert_eq!(hot.count, 2);
    }

    #[test]
    fn collects_value_set_correctly() {
        let a = assign(
            "--out",
            cond(
                vec![
                    branch(single("--op", 0x40), lit(1.0)),
                    branch(single("--op", 0x41), lit(2.0)),
                    branch(single("--op", 0x40), lit(3.0)), // duplicate
                    branch(single("--rm", 7), lit(50.0)),
                ],
                lit(0.0),
            ),
        );
        let vs = discover_key_value_set(&[a], "--op");
        let expected: BTreeSet<i64> = [0x40, 0x41].iter().copied().collect();
        assert_eq!(vs, expected);
    }

    #[test]
    fn value_set_ignores_non_literal_rhs() {
        // A test with `value: var(--y)` is not specialisable as a compile-
        // time constant; skip it. (The Single test's RHS is rarely non-
        // literal in practice, but the API should be defensive.)
        let a = assign(
            "--out",
            cond(
                vec![
                    branch(single("--op", 5), lit(50.0)),
                    branch(
                        StyleTest::Single {
                            property: "--op".to_string(),
                            value: var("--y"),
                        },
                        lit(60.0),
                    ),
                ],
                lit(0.0),
            ),
        );
        let vs = discover_key_value_set(&[a], "--op");
        let expected: BTreeSet<i64> = [5].iter().copied().collect();
        assert_eq!(vs, expected);
    }

    #[test]
    fn rank_returns_topk_sorted() {
        let a = assign(
            "--out",
            cond(
                vec![
                    branch(single("--op", 1), lit(1.0)),
                    branch(single("--op", 2), lit(2.0)),
                    branch(single("--op", 3), lit(3.0)),
                    branch(single("--rm", 0), lit(10.0)),
                    branch(single("--rm", 1), lit(11.0)),
                    branch(single("--reg", 0), lit(100.0)),
                ],
                lit(0.0),
            ),
        );
        let ranked = rank_dispatch_keys(&[a], 2);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].name, "--op");
        assert_eq!(ranked[0].count, 3);
        assert_eq!(ranked[1].name, "--rm");
        assert_eq!(ranked[1].count, 2);
    }

    // ------------------------------------------------------------------
    // Phase 2 — specialise_assignments / specialise_expr / test_outcome
    // ------------------------------------------------------------------

    #[test]
    fn specialise_folds_matching_single_branch() {
        // if(--op: 64: lit(10); --op: 65: lit(20); else: lit(0))
        // with binding --op=64 → folds to lit(10).
        let mut e = cond(
            vec![
                branch(single("--op", 64), lit(10.0)),
                branch(single("--op", 65), lit(20.0)),
            ],
            lit(0.0),
        );
        let mut s = SpecialiseStats::default();
        specialise_expr(&mut e, "--op", 64, &mut s);
        assert_eq!(e, lit(10.0));
        assert_eq!(s.conditions_decided, 1);
    }

    #[test]
    fn specialise_drops_rejecting_single_branch() {
        // if(--op: 65: lit(20); else: lit(0))  with --op=64
        // → fold to lit(0). Branch dropped (NeverTakes), fallback used.
        let mut e = cond(
            vec![branch(single("--op", 65), lit(20.0))],
            lit(0.0),
        );
        let mut s = SpecialiseStats::default();
        specialise_expr(&mut e, "--op", 64, &mut s);
        assert_eq!(e, lit(0.0));
        assert_eq!(s.branches_dropped, 1);
        assert_eq!(s.conditions_collapsed_to_fallback, 1);
    }

    #[test]
    fn specialise_keeps_unknown_single_branch() {
        // if(--rm: 3: lit(7); else: lit(0))  with --op=64
        // → unchanged (different property). Branch kept; no drops/decides.
        let original = cond(
            vec![branch(single("--rm", 3), lit(7.0))],
            lit(0.0),
        );
        let mut e = original.clone();
        let mut s = SpecialiseStats::default();
        specialise_expr(&mut e, "--op", 64, &mut s);
        assert_eq!(e, original);
        assert_eq!(s.branches_dropped, 0);
        assert_eq!(s.conditions_decided, 0);
        assert_eq!(s.conditions_collapsed_to_fallback, 0);
        assert_eq!(s.conditions_visited, 1);
    }

    #[test]
    fn specialise_and_of_two_matching_folds() {
        // if(--op: 64 AND --reg: 0: lit(99); else: lit(0))
        // binding --op=64 alone leaves --reg unresolved → branch is
        // Unknown, NOT AlwaysTakes (And of {matching, unknown} = Unknown).
        let mut e = cond(
            vec![branch(
                StyleTest::And(vec![single("--op", 64), single("--reg", 0)]),
                lit(99.0),
            )],
            lit(0.0),
        );
        let mut s = SpecialiseStats::default();
        specialise_expr(&mut e, "--op", 64, &mut s);
        // Branch kept (Unknown), but condition still visited.
        match &e {
            Expr::StyleCondition { branches, .. } => assert_eq!(branches.len(), 1),
            _ => panic!("expected StyleCondition, got {:?}", e),
        }

        // Now repeat the test where BOTH operands of the And resolve to
        // AlwaysTakes (both on --op).
        let mut e2 = cond(
            vec![branch(
                StyleTest::And(vec![single("--op", 64), single("--op", 64)]),
                lit(99.0),
            )],
            lit(0.0),
        );
        let mut s2 = SpecialiseStats::default();
        specialise_expr(&mut e2, "--op", 64, &mut s2);
        assert_eq!(e2, lit(99.0));
        assert_eq!(s2.conditions_decided, 1);
    }

    #[test]
    fn specialise_or_of_matching_and_rejecting_folds() {
        // if((--op: 64 OR --op: 65): lit(77); else: lit(0))  with --op=64
        // → Or of {AlwaysTakes, NeverTakes} = AlwaysTakes; folds to lit(77).
        let mut e = cond(
            vec![branch(
                StyleTest::Or(vec![single("--op", 64), single("--op", 65)]),
                lit(77.0),
            )],
            lit(0.0),
        );
        let mut s = SpecialiseStats::default();
        specialise_expr(&mut e, "--op", 64, &mut s);
        assert_eq!(e, lit(77.0));
        assert_eq!(s.conditions_decided, 1);

        // And the reverse: --op=99 with the same Or → both arms reject,
        // Or is NeverTakes → branch dropped → fold to fallback.
        let mut e2 = cond(
            vec![branch(
                StyleTest::Or(vec![single("--op", 64), single("--op", 65)]),
                lit(77.0),
            )],
            lit(0.0),
        );
        let mut s2 = SpecialiseStats::default();
        specialise_expr(&mut e2, "--op", 99, &mut s2);
        assert_eq!(e2, lit(0.0));
        assert_eq!(s2.branches_dropped, 1);
        assert_eq!(s2.conditions_collapsed_to_fallback, 1);
    }

    #[test]
    fn specialise_nested_in_calc_recurses() {
        // calc(if(--op:64: lit(10); else: lit(0)) + lit(5))  with --op=64
        // → calc(lit(10) + lit(5)). The Calc node is preserved; the
        // inner condition collapses.
        let inner = cond(vec![branch(single("--op", 64), lit(10.0))], lit(0.0));
        let mut e = Expr::Calc(CalcOp::Add(Box::new(inner), Box::new(lit(5.0))));
        let mut s = SpecialiseStats::default();
        specialise_expr(&mut e, "--op", 64, &mut s);
        match &e {
            Expr::Calc(CalcOp::Add(a, b)) => {
                assert_eq!(**a, lit(10.0));
                assert_eq!(**b, lit(5.0));
            }
            _ => panic!("expected Calc::Add, got {:?}", e),
        }
        assert_eq!(s.conditions_decided, 1);
    }

    #[test]
    fn specialise_assignments_aggregates_stats() {
        // Two assignments, three SCs total, two of them decided by --op=64.
        let a1 = assign(
            "--AX",
            cond(
                vec![
                    branch(single("--op", 64), lit(1.0)),     // AlwaysTakes
                    branch(single("--op", 65), lit(2.0)),     // dropped
                ],
                lit(0.0),
            ),
        );
        let a2 = assign(
            "--BX",
            cond(
                vec![
                    branch(single("--rm", 3), lit(10.0)),     // Unknown, kept
                    branch(single("--op", 99), lit(20.0)),    // dropped
                ],
                lit(0.0),
            ),
        );
        let mut input = vec![a1, a2];
        let stats = specialise_assignments(&mut input, "--op", 64);

        // a1 collapses to lit(1.0).
        assert_eq!(input[0].value, lit(1.0));
        // a2 keeps the --rm branch but drops the --op:99 branch.
        match &input[1].value {
            Expr::StyleCondition { branches, fallback } => {
                assert_eq!(branches.len(), 1);
                assert_eq!(branches[0].then, lit(10.0));
                assert_eq!(**fallback, lit(0.0));
            }
            _ => panic!("expected StyleCondition, got {:?}", input[1].value),
        }

        // Aggregate stats:
        //   conditions_visited = 2 (a1, a2)
        //   branches_dropped   = 1 (a2's --op:99 only; a1 short-circuits
        //                          on the AlwaysTakes branch and never
        //                          looks at --op:65)
        //   conditions_decided = 1 (a1)
        //   conditions_collapsed_to_fallback = 0
        assert_eq!(stats.conditions_visited, 2);
        assert_eq!(stats.branches_dropped, 1);
        assert_eq!(stats.conditions_decided, 1);
        assert_eq!(stats.conditions_collapsed_to_fallback, 0);
    }

    #[test]
    fn test_outcome_handles_non_literal_rhs_as_unknown() {
        // Single test with non-literal RHS → Unknown, even if the property
        // matches. Defensive: parser is unlikely to emit this, but the API
        // should not panic or guess.
        let t = StyleTest::Single {
            property: "--op".to_string(),
            value: var("--y"),
        };
        assert_eq!(test_outcome(&t, "--op", 64), BranchOutcome::Unknown);
    }

    #[test]
    fn var_ref_to_key_folds_to_literal() {
        // var(--op) under binding --op=64 → Literal(64).
        // Direct read of the dispatch key returns the binding's value.
        let mut e = var("--op");
        let mut stats = SpecialiseStats::default();
        specialise_expr(&mut e, "--op", 64, &mut stats);
        assert_eq!(e, Expr::Literal(64.0));
        assert_eq!(stats.var_refs_folded, 1);
    }

    #[test]
    fn var_ref_to_other_key_unchanged() {
        // var(--rm) under binding --op=64 → unchanged.
        let mut e = var("--rm");
        let mut stats = SpecialiseStats::default();
        specialise_expr(&mut e, "--op", 64, &mut stats);
        assert_eq!(e, var("--rm"));
        assert_eq!(stats.var_refs_folded, 0);
    }

    #[test]
    fn var_ref_inside_calc_folds() {
        // calc(var(--op) + 1) under --op=64 → calc(Literal(64) + 1).
        // The Var inside the Calc gets folded; the Calc structure preserved.
        let mut e = Expr::Calc(CalcOp::Add(
            Box::new(var("--op")),
            Box::new(lit(1.0)),
        ));
        let mut stats = SpecialiseStats::default();
        specialise_expr(&mut e, "--op", 64, &mut stats);
        assert_eq!(
            e,
            Expr::Calc(CalcOp::Add(Box::new(lit(64.0)), Box::new(lit(1.0))))
        );
        assert_eq!(stats.var_refs_folded, 1);
    }

    #[test]
    fn find_prologue_returns_closure_with_target() {
        // --target depends on --a, which depends on --b. --c is unrelated.
        // find_prologue(target=--target) should return {--target, --a, --b}.
        let a = assign("--target", var("--a"));
        let b = assign("--a", var("--b"));
        let c = assign("--b", lit(0.0));
        let d = assign("--c", lit(99.0));
        let props = find_prologue_properties(&[a, b, c, d], "--target");
        let mut got: Vec<&str> = props.iter().map(|s| s.as_str()).collect();
        got.sort();
        assert_eq!(got, vec!["--a", "--b", "--target"]);
    }

    #[test]
    fn find_prologue_handles_style_condition_deps() {
        // --target reads --a inside a style test → --a is a dependency.
        // --target also depends on --b for the then-value.
        let target = assign(
            "--target",
            cond(
                vec![branch(single("--a", 1), var("--b"))],
                lit(0.0),
            ),
        );
        let a = assign("--a", lit(1.0));
        let b = assign("--b", lit(2.0));
        let c = assign("--c", lit(3.0));
        let props = find_prologue_properties(&[target, a, b, c], "--target");
        let mut got: Vec<&str> = props.iter().map(|s| s.as_str()).collect();
        got.sort();
        assert_eq!(got, vec!["--a", "--b", "--target"]);
    }

    #[test]
    fn split_prologue_tail_preserves_order() {
        let assignments = vec![
            assign("--p1", lit(1.0)),
            assign("--t1", lit(2.0)),
            assign("--p2", lit(3.0)),
            assign("--t2", lit(4.0)),
        ];
        let mut prologue_props = HashSet::new();
        prologue_props.insert("--p1".to_string());
        prologue_props.insert("--p2".to_string());
        let (prologue, tail) = split_prologue_tail(assignments, &prologue_props);
        let p_names: Vec<&str> = prologue.iter().map(|a| a.property.as_str()).collect();
        let t_names: Vec<&str> = tail.iter().map(|a| a.property.as_str()).collect();
        assert_eq!(p_names, vec!["--p1", "--p2"]);
        assert_eq!(t_names, vec!["--t1", "--t2"]);
    }
}
