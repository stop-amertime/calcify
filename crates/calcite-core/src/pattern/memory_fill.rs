//! CSS-level detector: range-predicate bulk-fill → Op::MemoryFill binding.
//!
//! Recognises the shape emitted by CSS-DOS's kiln transpiler for INT 2F/AH=FE
//! bulk fills. Every memory cell gains a clause of the form:
//!
//! ```css
//! --m<N>: if(
//!   style(--bulkOpKind: 1):
//!     calc(
//!       sign(max(0, <N> - var(--bulkDst) + 1))
//!       * sign(max(0, var(--bulkDst) + var(--bulkCount) - <N>))
//!       * var(--bulkValue)
//!       + (1 - sign(max(0, <N> - var(--bulkDst) + 1))
//!            * sign(max(0, var(--bulkDst) + var(--bulkCount) - <N>)))
//!       * var(--__1m<N>)
//!     );
//!   style(--memAddr0: <N>): var(--memVal0);
//!   ...
//!   else: var(--__1m<N>));
//! ```
//!
//! Two jobs:
//!
//! 1. **Recognise + collapse.** When many cells in a dense address range carry
//!    the same bulk-fill top-clause (differing only in the cell address
//!    literal), record one `CssMemoryFillBinding` naming the state addresses
//!    for bulkOpKind / bulkDst / bulkCount / bulkValue, plus the absorbed
//!    address range.
//!
//! 2. **Rewrite the assignments.** Return a parallel list of "bulk-branch-
//!    stripped" assignments: the bulk clause is removed, keeping the per-cell
//!    slot dispatch + fallback. Compilation then processes cheap, tiny
//!    expressions instead of the full calc tree. Correctness is preserved
//!    because the pre-tick hook performs the actual fill natively before
//!    the tick's bytecode runs, and the stripped assignments still handle
//!    normal (non-bulk) writes via the existing memAddr/memVal slots.
//!
//! Integration: `Evaluator::from_parsed` calls `recognise_bulk_fills`, takes
//! the returned bindings, swaps the absorbed assignments with their stripped
//! versions, and installs a pre-tick hook that executes the fill when
//! bulkOpKind (at linear 0x510) equals 1.

#![allow(dead_code)]

use std::collections::HashMap;

use crate::types::*;

/// One CSS-level bulk-fill binding recognised across a range of memory cells.
#[derive(Debug, Clone)]
pub struct CssMemoryFillBinding {
    /// Property name for the kind byte (e.g. `--bulkOpKind`).
    pub kind_property: String,
    /// Property name for the dst address (e.g. `--bulkDst`).
    pub dst_property: String,
    /// Property name for the count (e.g. `--bulkCount`).
    pub count_property: String,
    /// Property name for the fill value (e.g. `--bulkValue`).
    pub val_property: String,
    /// Memory address range absorbed by this binding (exclusive end).
    pub addr_range: std::ops::Range<i64>,
}

/// Result of bulk-fill pattern recognition.
pub struct BulkFillResult {
    /// One binding per recognised dense range.
    pub bindings: Vec<CssMemoryFillBinding>,
    /// For each absorbed cell: its rewritten assignment (bulk branch stripped).
    /// Keyed by property name so the caller can swap assignments in place.
    pub rewritten: HashMap<String, Assignment>,
}

/// Analyse assignments for the bulk-fill range-predicate pattern.
///
/// Returns all recognised bindings plus the rewritten per-cell assignments.
/// Cells whose top clause doesn't match the expected shape are left alone
/// (not included in `rewritten`).
pub fn recognise_bulk_fills(assignments: &[Assignment]) -> BulkFillResult {
    // Phase 1: match every assignment against the expected shape and collect
    // (cell_address, binding_signature, rewritten_assignment) tuples.
    //
    // binding_signature is a 4-tuple of property names (kind, dst, count, val);
    // cells that share a signature are grouped into one binding.
    let mut matches: Vec<(i64, BindingSig, Assignment)> = Vec::new();

    for a in assignments {
        if let Some((addr, sig, stripped)) = extract_bulk_fill_cell(a) {
            matches.push((addr, sig, stripped));
        }
    }

    if matches.is_empty() {
        return BulkFillResult {
            bindings: Vec::new(),
            rewritten: HashMap::new(),
        };
    }

    // Phase 2: group by signature, then find dense ranges within each group.
    // A group is absorbed only if it's a contiguous run of addresses — gaps
    // would mean the native fill would touch cells the CSS wouldn't. Rather
    // than split into multiple bindings, skip any signature whose addresses
    // aren't dense. (In practice, kiln emits the clause on every memory cell
    // in the emitted address set, so this is one big dense range.)
    let mut by_sig: HashMap<BindingSig, Vec<(i64, Assignment)>> = HashMap::new();
    for (addr, sig, stripped) in matches {
        by_sig.entry(sig).or_default().push((addr, stripped));
    }

    let mut bindings = Vec::new();
    let mut rewritten = HashMap::new();

    for (sig, mut cells) in by_sig {
        cells.sort_by_key(|(a, _)| *a);

        // Walk cells, emitting one binding per contiguous run.
        let mut i = 0;
        while i < cells.len() {
            let start = cells[i].0;
            let mut end = start;
            let mut j = i;
            while j < cells.len() && cells[j].0 == end {
                rewritten.insert(cells[j].1.property.clone(), cells[j].1.clone());
                end += 1;
                j += 1;
            }
            bindings.push(CssMemoryFillBinding {
                kind_property: sig.0.clone(),
                dst_property: sig.1.clone(),
                count_property: sig.2.clone(),
                val_property: sig.3.clone(),
                addr_range: start..end,
            });
            i = j;
        }
    }

    BulkFillResult { bindings, rewritten }
}

/// 4-tuple of property names identifying a binding.
type BindingSig = (String, String, String, String);

/// Try to match one assignment against the bulk-fill template.
///
/// On match, returns (cell_address, binding_signature, stripped_assignment).
/// The stripped assignment drops the bulk branch, keeping the rest.
fn extract_bulk_fill_cell(a: &Assignment) -> Option<(i64, BindingSig, Assignment)> {
    // Property must be `--m<N>` with a parseable integer suffix.
    let addr = parse_mem_cell_address(&a.property)?;

    // Value must be `if(style(--bulkOpKind: 1): <calc>; <other branches...>; else: <fallback>)`
    let (branches, fallback) = match &a.value {
        Expr::StyleCondition { branches, fallback } => (branches, fallback.clone()),
        _ => return None,
    };

    if branches.is_empty() {
        return None;
    }

    // First branch: style(--bulkOpKind: 1): <calc value>
    let first = &branches[0];
    let kind_prop = match &first.condition {
        StyleTest::Single { property, value: Expr::Literal(v) } if *v == 1.0 => property.clone(),
        _ => return None,
    };
    if kind_prop != "--bulkOpKind" {
        return None;
    }

    // The value expression: walk it to extract dst/count/val property names.
    let (dst_prop, count_prop, val_prop) = extract_fill_value_props(&first.then, addr)?;

    let sig: BindingSig = (kind_prop, dst_prop, count_prop, val_prop);

    // Build stripped assignment: same property, same fallback, drop first branch.
    let stripped_value = if branches.len() == 1 {
        // Only branch was the bulk one — value becomes the fallback.
        *fallback
    } else {
        Expr::StyleCondition {
            branches: branches[1..].to_vec(),
            fallback,
        }
    };

    Some((
        addr,
        sig,
        Assignment {
            property: a.property.clone(),
            value: stripped_value,
        },
    ))
}

/// Parse `--m<N>` → N. Rejects anything else.
fn parse_mem_cell_address(property: &str) -> Option<i64> {
    let rest = property.strip_prefix("--m")?;
    rest.parse::<i64>().ok()
}

/// Walk the bulk-fill value expression and extract the three property names.
///
/// The shape we expect:
/// ```text
/// calc(R * V + (1 - R) * P)       — the "in range ? V : P" arithmetic
/// ```
///
/// or for the kind-byte self-clear:
/// ```text
/// Literal(0)
/// ```
///
/// where:
/// - R is `sign(max(0, addr - var(--bulkDst) + 1)) * sign(max(0, var(--bulkDst) + var(--bulkCount) - addr))`
/// - V is `var(--bulkValue)`
/// - P is `var(--__1m<addr>)`
///
/// We don't need to verify the exact tree structure — a weaker check is fine:
/// collect all `var(--X)` references inside the expression and expect them to
/// form the set { bulkDst, bulkCount, bulkValue, __1m<addr> } (for the in-range
/// case). For the self-clear case, it's a literal 0 with no vars.
fn extract_fill_value_props(expr: &Expr, cell_addr: i64) -> Option<(String, String, String)> {
    // Self-clear case: the kind byte at the scratch base writes 0.
    if let Expr::Literal(0.0) = expr {
        // Return fixed names so this still groups with the main binding.
        return Some((
            "--bulkDst".to_string(),
            "--bulkCount".to_string(),
            "--bulkValue".to_string(),
        ));
    }

    let mut seen: Vec<String> = Vec::new();
    collect_var_names(expr, &mut seen);

    // Required names
    let has_dst = seen.iter().any(|s| s == "--bulkDst");
    let has_count = seen.iter().any(|s| s == "--bulkCount");
    let has_value = seen.iter().any(|s| s == "--bulkValue");
    let expected_prev = format!("--__1m{}", cell_addr);
    let has_prev = seen.iter().any(|s| *s == expected_prev);

    if !(has_dst && has_count && has_value && has_prev) {
        return None;
    }

    Some((
        "--bulkDst".to_string(),
        "--bulkCount".to_string(),
        "--bulkValue".to_string(),
    ))
}

/// Recursively collect all `var(--X)` names referenced in an expression.
fn collect_var_names(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::Var { name, fallback } => {
            out.push(name.clone());
            if let Some(fb) = fallback {
                collect_var_names(fb, out);
            }
        }
        Expr::Calc(op) => collect_var_names_calc(op, out),
        Expr::StyleCondition { branches, fallback } => {
            for b in branches {
                collect_var_names(&b.then, out);
            }
            collect_var_names(fallback, out);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                collect_var_names(a, out);
            }
        }
        Expr::Concat(parts) => {
            for p in parts {
                collect_var_names(p, out);
            }
        }
        Expr::Literal(_) | Expr::StringLiteral(_) => {}
    }
}

fn collect_var_names_calc(op: &CalcOp, out: &mut Vec<String>) {
    match op {
        CalcOp::Add(a, b)
        | CalcOp::Sub(a, b)
        | CalcOp::Mul(a, b)
        | CalcOp::Div(a, b)
        | CalcOp::Mod(a, b) => {
            collect_var_names(a, out);
            collect_var_names(b, out);
        }
        CalcOp::Min(xs) | CalcOp::Max(xs) => {
            for x in xs {
                collect_var_names(x, out);
            }
        }
        CalcOp::Clamp(a, b, c) => {
            collect_var_names(a, out);
            collect_var_names(b, out);
            collect_var_names(c, out);
        }
        CalcOp::Round(_, a, b) => {
            collect_var_names(a, out);
            collect_var_names(b, out);
        }
        CalcOp::Sign(a) | CalcOp::Abs(a) | CalcOp::Negate(a) => {
            collect_var_names(a, out);
        }
        CalcOp::Pow(a, b) => {
            collect_var_names(a, out);
            collect_var_names(b, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_assignment(property: &str, value: Expr) -> Assignment {
        Assignment {
            property: property.to_string(),
            value,
        }
    }

    /// Build a minimal bulk-fill cell assignment for testing.
    fn mk_bulk_cell(addr: i64) -> Assignment {
        // style(--bulkOpKind: 1): calc((sign(max(0, ...)) * sign(max(0, ...))) * var(--bulkValue) + (1 - ...) * var(--__1m<addr>))
        // The exact shape inside the calc doesn't matter — the detector only
        // checks for the required var() names.
        let in_range = Expr::Calc(CalcOp::Mul(
            Box::new(Expr::Calc(CalcOp::Sign(Box::new(Expr::Calc(CalcOp::Max(vec![
                Expr::Literal(0.0),
                Expr::Calc(CalcOp::Sub(
                    Box::new(Expr::Literal(addr as f64)),
                    Box::new(Expr::Var {
                        name: "--bulkDst".to_string(),
                        fallback: None,
                    }),
                )),
            ])))))),
            Box::new(Expr::Var {
                name: "--bulkCount".to_string(),
                fallback: None,
            }),
        ));
        let fill_value = Expr::Var {
            name: "--bulkValue".to_string(),
            fallback: None,
        };
        let prev = Expr::Var {
            name: format!("--__1m{}", addr),
            fallback: None,
        };
        let calc_value = Expr::Calc(CalcOp::Add(
            Box::new(Expr::Calc(CalcOp::Mul(Box::new(in_range.clone()), Box::new(fill_value)))),
            Box::new(Expr::Calc(CalcOp::Mul(
                Box::new(Expr::Literal(1.0)),
                Box::new(prev.clone()),
            ))),
        ));

        // Second branch: one normal slot dispatch
        let slot_branch = StyleBranch {
            condition: StyleTest::Single {
                property: "--memAddr0".to_string(),
                value: Expr::Literal(addr as f64),
            },
            then: Expr::Var {
                name: "--memVal0".to_string(),
                fallback: None,
            },
        };

        let bulk_branch = StyleBranch {
            condition: StyleTest::Single {
                property: "--bulkOpKind".to_string(),
                value: Expr::Literal(1.0),
            },
            then: calc_value,
        };

        mk_assignment(
            &format!("--m{}", addr),
            Expr::StyleCondition {
                branches: vec![bulk_branch, slot_branch],
                fallback: Box::new(prev),
            },
        )
    }

    #[test]
    fn empty_input_yields_no_bindings() {
        let r = recognise_bulk_fills(&[]);
        assert!(r.bindings.is_empty());
        assert!(r.rewritten.is_empty());
    }

    #[test]
    fn recognises_dense_range() {
        let assignments: Vec<Assignment> = (0x100..0x110).map(mk_bulk_cell).collect();
        let r = recognise_bulk_fills(&assignments);
        assert_eq!(r.bindings.len(), 1);
        assert_eq!(r.rewritten.len(), 16);
        assert_eq!(r.bindings[0].addr_range, 0x100..0x110);
        assert_eq!(r.bindings[0].kind_property, "--bulkOpKind");
        assert_eq!(r.bindings[0].dst_property, "--bulkDst");
        assert_eq!(r.bindings[0].count_property, "--bulkCount");
        assert_eq!(r.bindings[0].val_property, "--bulkValue");
    }

    #[test]
    fn non_bulk_cells_ignored() {
        // A memory cell with only slot dispatch, no bulk clause.
        let plain = mk_assignment(
            "--m200",
            Expr::StyleCondition {
                branches: vec![StyleBranch {
                    condition: StyleTest::Single {
                        property: "--memAddr0".to_string(),
                        value: Expr::Literal(200.0),
                    },
                    then: Expr::Var {
                        name: "--memVal0".to_string(),
                        fallback: None,
                    },
                }],
                fallback: Box::new(Expr::Var {
                    name: "--__1m200".to_string(),
                    fallback: None,
                }),
            },
        );
        let r = recognise_bulk_fills(&[plain]);
        assert!(r.bindings.is_empty());
        assert!(r.rewritten.is_empty());
    }

    #[test]
    fn stripped_assignment_drops_bulk_branch() {
        let a = mk_bulk_cell(0x200);
        let r = recognise_bulk_fills(&[a]);
        let stripped = r.rewritten.get("--m512").expect("stripped assignment");
        match &stripped.value {
            Expr::StyleCondition { branches, .. } => {
                // Original had 2 branches (bulk + slot); stripped should have 1.
                assert_eq!(branches.len(), 1);
                // The remaining branch is the slot one — condition on --memAddr0.
                match &branches[0].condition {
                    StyleTest::Single { property, .. } => assert_eq!(property, "--memAddr0"),
                    _ => panic!("expected single style test on --memAddr0"),
                }
            }
            _ => panic!("expected StyleCondition after stripping"),
        }
    }

    #[test]
    fn gap_in_range_splits_bindings() {
        // Cells at 0x100..0x108 and 0x110..0x118 (gap at 0x108..0x110)
        let mut assignments: Vec<Assignment> = (0x100..0x108).map(mk_bulk_cell).collect();
        assignments.extend((0x110..0x118).map(mk_bulk_cell));
        let r = recognise_bulk_fills(&assignments);
        assert_eq!(r.bindings.len(), 2);
        let mut ranges: Vec<_> = r.bindings.iter().map(|b| b.addr_range.clone()).collect();
        ranges.sort_by_key(|r| r.start);
        assert_eq!(ranges[0], 0x100..0x108);
        assert_eq!(ranges[1], 0x110..0x118);
    }
}
