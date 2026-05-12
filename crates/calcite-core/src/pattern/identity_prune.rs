//! Identity-branch pruning for `StyleCondition` expressions.
//!
//! ## What this does
//!
//! Many cabinets emit `if(style(--key: V1): <work>; style(--key: V2): <work>;
//! ...; else: <fallback>)` chains where most branches have `<work> =
//! <fallback>` — i.e. the branch produces the same value the fallback
//! would. Kiln (the CSS-DOS transpiler) emits these by necessity, because
//! CSS demands every property have a defined value: when key=V1 doesn't
//! affect a given property, kiln must still write a branch yielding the
//! property's current value.
//!
//! At compile time we can detect this structurally and drop the branch.
//! The semantics are preserved: an absent branch falls through to the
//! enclosing condition's fallback, which produces the same value.
//!
//! ## Why this is cardinal-rule-safe
//!
//! The recogniser compares two `Expr` trees for structural equality
//! (`PartialEq`, already derived on `Expr`). It does NOT:
//!
//! - Read characters of any slot or property name.
//! - Encode any cabinet-specific conventions (like CSS-DOS's `__1`
//!   snapshot-variable prefix).
//! - Make decisions based on specific addresses, opcodes, or bytes.
//!
//! A 6502 cabinet's assignments with the same "branch `then` equals
//! enclosing fallback" shape would prune identically. So would a
//! brainfuck cabinet. The rule is purely structural.
//!
//! ## What gets pruned
//!
//! For each `StyleCondition { branches, fallback }`:
//!
//! 1. Drop every branch whose `then` is structurally `Expr::eq` to
//!    `fallback`.
//! 2. Recurse into each remaining branch's `then` (which may itself
//!    be a `StyleCondition`) — propagate the enclosing assignment's
//!    fallback if the inner `StyleCondition`'s own fallback is also
//!    identity, OR just use the inner condition's own fallback as the
//!    identity reference for its branches.
//! 3. Recurse into `Calc`, `Concat`, `FunctionCall` arguments so
//!    nested conditions inside arithmetic get pruned too.
//!
//! ## Reporting
//!
//! `prune_assignments` returns a `PruneStats` with branch counts before
//! and after; the caller logs these for visibility.

use crate::types::{Assignment, CalcOp, Expr, StyleBranch};

#[derive(Debug, Default, Clone, Copy)]
pub struct PruneStats {
    pub branches_before: usize,
    pub branches_after: usize,
    pub conditions_visited: usize,
    pub conditions_eliminated: usize,
}

impl PruneStats {
    pub fn branches_dropped(&self) -> usize {
        self.branches_before.saturating_sub(self.branches_after)
    }
}

/// Walk all assignments and prune identity branches in-place.
pub fn prune_assignments(assignments: &mut [Assignment]) -> PruneStats {
    let mut stats = PruneStats::default();
    for a in assignments.iter_mut() {
        let before = stats.branches_before;
        let after_before = stats.branches_after;
        prune_expr(&mut a.value, &mut stats);
        let dropped_here = (stats.branches_before - before)
            .saturating_sub(stats.branches_after - after_before);
        if dropped_here > 0 {
            log::debug!(
                "[identity-prune] {} → dropped {} branches",
                a.property, dropped_here
            );
        }
    }
    stats
}

/// Prune identity branches inside one expression tree.
///
/// Rule: a branch B in a `StyleCondition { branches, fallback }` is
/// safe to drop iff `B.then` is structurally equal to `fallback`.
/// Removing B causes its condition to fall through to `fallback`, which
/// produces the same value B would have produced. Arithmetic / function
/// wrappers around the whole condition transform both paths equally, so
/// they don't affect safety. Verified empirically against doom8088: with
/// this rule, calcite-cli produces bit-identical execution (same
/// `ticksToInGame`, same `cyclesToInGame`) to the no-prune variant.
///
/// What this rule does NOT do: it does NOT recursively unwrap the
/// fallback to look for "deep identity." When the immediate fallback is
/// itself a `StyleCondition` (e.g. outer `if(_tf:1: var(X); _irq:1:
/// var(X); else: <inner-condition>)`), the inner condition's branches
/// may compute non-identity values, so pruning the outer's `var(X)`
/// branches would change semantics. The simple "match immediate
/// fallback only" rule sidesteps that.
fn prune_expr(expr: &mut Expr, stats: &mut PruneStats) {
    match expr {
        Expr::StyleCondition { branches, fallback } => {
            stats.conditions_visited += 1;
            // Recurse into the fallback first — nested conditions
            // inside the fallback should be pruned too.
            prune_expr(fallback, stats);
            stats.branches_before += branches.len();

            // Drop branches whose `then` matches the fallback.
            branches.retain(|b| !exprs_equal(&b.then, fallback));

            // Recurse into the remaining branches' `then`s.
            for b in branches.iter_mut() {
                prune_expr(&mut b.then, stats);
            }

            stats.branches_after += branches.len();

            // If every branch was pruned and only the fallback remains,
            // collapse the condition entirely.
            if branches.is_empty() {
                stats.conditions_eliminated += 1;
                let fb = std::mem::replace(fallback.as_mut(), Expr::Literal(0.0));
                *expr = fb;
            }
        }
        Expr::Calc(op) => prune_calc(op, stats),
        Expr::Concat(parts) => {
            for p in parts.iter_mut() {
                prune_expr(p, stats);
            }
        }
        Expr::FunctionCall { args, .. } => {
            for a in args.iter_mut() {
                prune_expr(a, stats);
            }
        }
        Expr::Literal(_) | Expr::Var { .. } | Expr::StringLiteral(_) => {}
    }
}

fn prune_calc(op: &mut CalcOp, stats: &mut PruneStats) {
    match op {
        CalcOp::Add(a, b)
        | CalcOp::Sub(a, b)
        | CalcOp::Mul(a, b)
        | CalcOp::Div(a, b)
        | CalcOp::Mod(a, b)
        | CalcOp::Pow(a, b) => {
            prune_expr(a, stats);
            prune_expr(b, stats);
        }
        CalcOp::Min(args) | CalcOp::Max(args) => {
            for x in args.iter_mut() {
                prune_expr(x, stats);
            }
        }
        CalcOp::Clamp(a, b, c) => {
            prune_expr(a, stats);
            prune_expr(b, stats);
            prune_expr(c, stats);
        }
        CalcOp::Round(_, a, b) => {
            prune_expr(a, stats);
            prune_expr(b, stats);
        }
        CalcOp::Sign(a) | CalcOp::Abs(a) | CalcOp::Negate(a) => {
            prune_expr(a, stats);
        }
    }
}

/// Structural Expr equality. Delegates to the derived `PartialEq`.
///
/// Kept as a separate function so future relaxations (e.g. "treat
/// `var(x)` and `calc(var(x) + 0)` as equal") can be slotted in here
/// without touching the rest of the pass.
fn exprs_equal(a: &Expr, b: &Expr) -> bool {
    a == b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;

    fn var(name: &str) -> Expr {
        Expr::Var {
            name: name.to_string(),
            fallback: None,
        }
    }

    fn lit(v: f64) -> Expr {
        Expr::Literal(v)
    }

    /// Helper: build a StyleCondition with a single key/value test.
    fn cond(key: &str, val: i64, then: Expr, fallback: Expr) -> Expr {
        Expr::StyleCondition {
            branches: vec![StyleBranch {
                condition: StyleTest::Single {
                    property: key.to_string(),
                    value: Expr::Literal(val as f64),
                },
                then,
            }],
            fallback: Box::new(fallback),
        }
    }

    /// Helper: build a multi-branch StyleCondition.
    fn multi_cond(
        branches: Vec<(&str, i64, Expr)>,
        fallback: Expr,
    ) -> Expr {
        Expr::StyleCondition {
            branches: branches
                .into_iter()
                .map(|(k, v, t)| StyleBranch {
                    condition: StyleTest::Single {
                        property: k.to_string(),
                        value: Expr::Literal(v as f64),
                    },
                    then: t,
                })
                .collect(),
            fallback: Box::new(fallback),
        }
    }

    #[test]
    fn prunes_identity_branch_matching_fallback() {
        // if(--key: 1: var(--x); --key: 2: var(--x); else: var(--x))
        // → collapses entirely to var(--x).
        let mut e = multi_cond(
            vec![
                ("--key", 1, var("--x")),
                ("--key", 2, var("--x")),
            ],
            var("--x"),
        );
        let mut stats = PruneStats::default();
        prune_expr(&mut e, &mut stats);
        assert_eq!(e, var("--x"));
        assert_eq!(stats.branches_dropped(), 2);
        assert_eq!(stats.conditions_eliminated, 1);
    }

    #[test]
    fn keeps_non_identity_branches() {
        // if(--key: 1: var(--y); --key: 2: var(--x); else: var(--x))
        // → first branch kept (different expr), second pruned.
        let mut e = multi_cond(
            vec![
                ("--key", 1, var("--y")),
                ("--key", 2, var("--x")),
            ],
            var("--x"),
        );
        let mut stats = PruneStats::default();
        prune_expr(&mut e, &mut stats);
        // Should still be a StyleCondition with one branch left.
        match &e {
            Expr::StyleCondition { branches, fallback } => {
                assert_eq!(branches.len(), 1);
                assert_eq!(branches[0].then, var("--y"));
                assert_eq!(**fallback, var("--x"));
            }
            _ => panic!("expected StyleCondition, got {:?}", e),
        }
        assert_eq!(stats.branches_dropped(), 1);
    }

    #[test]
    fn recurses_into_nested_conditions() {
        // Outer: if(--op: 0: <inner>; else: var(--x))
        // Inner: if(--rm: 3: var(--x); else: var(--y))  ← branch is identity vs OUTER fallback,
        //   but the recursive prune only sees the inner's own fallback (var(--y)),
        //   so the rm=3 branch stays.
        // This test pins down the semantics: pruning is per-condition,
        // using THAT condition's own fallback. We don't peek outward.
        let inner = cond("--rm", 3, var("--x"), var("--y"));
        let mut e = cond("--op", 0, inner, var("--x"));
        let mut stats = PruneStats::default();
        prune_expr(&mut e, &mut stats);
        // Outer: branch's `then` is the inner condition (not equal to outer fallback var(--x)),
        // so the outer branch is kept.
        match &e {
            Expr::StyleCondition { branches, .. } => {
                assert_eq!(branches.len(), 1);
                // Inner: rm=3 branch's then (var(--x)) ≠ inner's fallback (var(--y)),
                // so inner branch is also kept.
                match &branches[0].then {
                    Expr::StyleCondition { branches: inner_branches, .. } => {
                        assert_eq!(inner_branches.len(), 1);
                        assert_eq!(inner_branches[0].then, var("--x"));
                    }
                    other => panic!("expected inner StyleCondition, got {:?}", other),
                }
            }
            _ => panic!("expected outer StyleCondition"),
        }
    }

    #[test]
    fn collapses_to_fallback_when_all_branches_identity() {
        // if(--key: 1: lit(7); else: lit(7))
        // → collapses to lit(7).
        let mut e = cond("--key", 1, lit(7.0), lit(7.0));
        let mut stats = PruneStats::default();
        prune_expr(&mut e, &mut stats);
        assert_eq!(e, lit(7.0));
        assert_eq!(stats.conditions_eliminated, 1);
    }

    #[test]
    fn recurses_into_calc_args() {
        // calc(if(--key: 1: var(--x); else: var(--x)) + lit(1))
        // → calc(var(--x) + lit(1)).
        let mut e = Expr::Calc(CalcOp::Add(
            Box::new(cond("--key", 1, var("--x"), var("--x"))),
            Box::new(lit(1.0)),
        ));
        let mut stats = PruneStats::default();
        prune_expr(&mut e, &mut stats);
        match &e {
            Expr::Calc(CalcOp::Add(a, b)) => {
                assert_eq!(**a, var("--x"));
                assert_eq!(**b, lit(1.0));
            }
            _ => panic!("expected Add, got {:?}", e),
        }
        assert_eq!(stats.conditions_eliminated, 1);
    }

    #[test]
    fn does_not_prune_when_fallback_is_unique_expr() {
        // if(--key: 1: var(--x); else: var(--y))
        // → unchanged (no identity).
        let original = cond("--key", 1, var("--x"), var("--y"));
        let mut e = original.clone();
        let mut stats = PruneStats::default();
        prune_expr(&mut e, &mut stats);
        assert_eq!(e, original);
        assert_eq!(stats.branches_dropped(), 0);
    }
}
