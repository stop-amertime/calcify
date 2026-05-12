//! Per-dispatch-key specialisation — phase 1: discovery diagnostics.
//!
//! ## What this is
//!
//! The first stage of the per-dispatch-key specialisation plan
//! (CSS-DOS `docs/plans/2026-05-12-per-dispatch-key-specialisation.md`).
//! No specialisation, no codegen — just two structural primitives:
//!
//! - `discover_hot_key(assignments)` — return the custom-property name
//!   that the most `StyleCondition`-`Single` tests across all
//!   assignments branch on, plus the count.
//! - `discover_key_value_set(assignments, key)` — return the set of
//!   literal `i64` values that `Single` tests on that key compare
//!   against.
//!
//! Together they answer "is this cabinet specialisable, and against
//! what?" without committing to any transformation.
//!
//! ## Cardinal-rule defence
//!
//! Both primitives are purely structural:
//!
//! - We read property *names* but never read characters out of them
//!   (no prefix matching, no convention-based filtering).
//! - We read literal *values* but never assign meaning to them
//!   (0x40 is just `64`).
//! - The hot-key choice is fully determined by `(count, name)`. The
//!   secondary lexicographic ordering on name is for determinism
//!   only — same cabinet, same answer, on any machine.
//!
//! A 6502 cabinet's hot dispatch key would be discovered identically.
//! A brainfuck cabinet's `--currentCommand` key would be picked if it
//! were the most-tested key. The rule is "whichever property the most
//! `StyleCondition`-`Single` tests reference, with at least two distinct
//! literal values to specialise against."

use std::collections::BTreeSet;
use std::collections::HashMap;

use crate::types::{Assignment, CalcOp, Expr, StyleTest};

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
}
