//! CSS-level detector: range-predicate bulk-fill → single `Op::MemoryFill` binding.
//!
//! Recognises the shape emitted by CSS-DOS's bulk-op transpiler path:
//!
//! ```text
//! --memByte_<N>: if(
//!   style(--bulkOpKind: 1)
//!     and calc(<N> >= var(--bulkDst))
//!     and calc(<N> < var(--bulkDst) + var(--bulkCount)):
//!     var(--bulkValue);
//!   style(--memAddr0: <N>): var(--memVal0);
//!   ...
//!   else: keep);
//! ```
//!
//! When every memory cell in a dense range has this identical top-clause —
//! same `--bulkOpKind` kind value, same `--bulkDst` / `--bulkCount` / `--bulkValue`
//! property references, and cell addresses that are contiguous — we collapse
//! ALL such cells into a single `CssMemoryFillBinding` so the evaluator can
//! execute the fill natively with one `memory.fill`.
//!
//! The detector is purely structural. It knows nothing about x86 or REP STOSB.
//! It recognises "every cell has the same range-predicate clause at the top"
//! and extracts the four binding property names plus the dense cell range.

use std::collections::HashMap;

use crate::types::*;

/// A single recognised bulk-fill binding.
///
/// Represents a group of memory cells whose top-of-chain branch is an identical
/// range-predicate write. All absorbed cells share the same three binding
/// property names (dst/count/value) and the kind sentinel the predicate gates
/// on. Cell addresses form a dense contiguous range.
#[derive(Debug, Clone, PartialEq)]
pub struct CssMemoryFillBinding {
    /// The `style(--bulkOpKind: K)` gate value that activates this fill.
    /// Conventionally 1 for bulk fill; kept parametric so later variants
    /// (kind 2 = memcpy, 3 = masked, ...) use a different binding type.
    pub kind_property: String,
    /// The property holding the destination base address (e.g. `--bulkDst`).
    pub dst_property: String,
    /// The property holding the byte count (e.g. `--bulkCount`).
    pub count_property: String,
    /// The property holding the fill byte value (e.g. `--bulkValue`).
    pub val_property: String,
    /// The literal kind value the gate tests for (e.g. 1 for fill).
    pub kind_value: i64,
    /// The inclusive-start, exclusive-end range of cell addresses absorbed.
    pub addr_range: std::ops::Range<i64>,
}

/// Result of bulk-fill recognition.
#[derive(Debug, Default, Clone)]
pub struct BulkFillResult {
    /// Detected bindings. One per contiguous run of matching cells.
    pub bindings: Vec<CssMemoryFillBinding>,
}

/// Run the detector across a slice of assignments.
///
/// The detector:
/// 1. For each assignment, checks whether its top-level branch matches the
///    bulk-fill template (`PerCellMatch`).
/// 2. Groups cells by `(kind_property, kind_value, dst_property, count_property, val_property)`.
/// 3. Emits one binding per group whose cell addresses form a dense contiguous
///    range. Sparse groups are rejected — emitting a fill for sparse cells
///    would clobber the gaps.
pub fn recognise_bulk_fills(assignments: &[Assignment]) -> BulkFillResult {
    // (kind_prop, kind_val, dst_prop, count_prop, val_prop) → Vec<cell_addr>
    let mut groups: HashMap<BindingKey, Vec<i64>> = HashMap::new();

    for assignment in assignments {
        let Some(m) = match_per_cell(assignment) else {
            continue;
        };
        let key = BindingKey {
            kind_property: m.kind_property,
            kind_value: m.kind_value,
            dst_property: m.dst_property,
            count_property: m.count_property,
            val_property: m.val_property,
        };
        groups.entry(key).or_default().push(m.cell_addr);
    }

    let mut bindings = Vec::new();
    for (key, mut addrs) in groups {
        addrs.sort_unstable();
        addrs.dedup();
        if addrs.len() < 2 {
            // Not worth collapsing a single cell into a "bulk" fill.
            continue;
        }
        let start = addrs[0];
        let end = *addrs.last().unwrap() + 1;
        let expected_len = (end - start) as usize;
        if addrs.len() != expected_len {
            // Sparse range — reject. We must not emit a fill that would
            // overwrite the gaps; they have different semantics.
            continue;
        }
        bindings.push(CssMemoryFillBinding {
            kind_property: key.kind_property,
            kind_value: key.kind_value,
            dst_property: key.dst_property,
            count_property: key.count_property,
            val_property: key.val_property,
            addr_range: start..end,
        });
    }

    // Deterministic ordering for tests.
    bindings.sort_by(|a, b| a.addr_range.start.cmp(&b.addr_range.start));

    BulkFillResult { bindings }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
struct BindingKey {
    kind_property: String,
    kind_value: i64,
    dst_property: String,
    count_property: String,
    val_property: String,
}

/// Intermediate record from matching a single memory-cell assignment.
#[derive(Debug)]
struct PerCellMatch {
    cell_addr: i64,
    kind_property: String,
    kind_value: i64,
    dst_property: String,
    count_property: String,
    val_property: String,
}

/// Check whether an assignment's first branch is the bulk-fill range-predicate
/// write. Returns `Some` if the shape matches, `None` otherwise.
///
/// Shape required (in the first branch of an `if()` chain):
///
/// ```text
/// condition:
///   style(--<kind_prop>: <K>)
///     and calc(<cell_addr> >= var(--<dst_prop>))
///     and calc(<cell_addr> < var(--<dst_prop>) + var(--<count_prop>))
/// then:
///   var(--<val_prop>)
/// ```
///
/// The `and` is commutative in principle, but CSS-DOS emits them in the
/// documented order, so that's what we accept. The design spec is explicit
/// about shape and ordering.
fn match_per_cell(assignment: &Assignment) -> Option<PerCellMatch> {
    let cell_addr = parse_cell_addr(&assignment.property)?;

    let Expr::StyleCondition { branches, .. } = &assignment.value else {
        return None;
    };
    let first = branches.first()?;

    // Expect `and` of 3 tests: kind gate, ge bound, lt bound.
    let StyleTest::And(tests) = &first.condition else {
        return None;
    };
    if tests.len() != 3 {
        return None;
    }

    let (kind_property, kind_value) = match &tests[0] {
        StyleTest::Single {
            property,
            value: Expr::Literal(v),
        } if v.fract() == 0.0 => (property.clone(), *v as i64),
        _ => return None,
    };

    // `calc(cell_addr >= var(--dst_prop))`
    let dst_property = match_ge_bound(&tests[1], cell_addr)?;

    // `calc(cell_addr < var(--dst_prop) + var(--count_prop))`
    let (dst2, count_property) = match_lt_bound(&tests[2], cell_addr)?;
    if dst2 != dst_property {
        return None;
    }

    // Then: var(--val_prop)
    let val_property = match &first.then {
        Expr::Var { name, .. } => name.clone(),
        _ => return None,
    };

    Some(PerCellMatch {
        cell_addr,
        kind_property,
        kind_value,
        dst_property,
        count_property,
        val_property,
    })
}

/// Parse the cell address from a memory-cell property name.
///
/// Matches CSS-DOS naming conventions:
/// - `--memByte_<N>` (v4 DOS output) → N
/// - `--m<N>`        (legacy hack-path output) → N
fn parse_cell_addr(name: &str) -> Option<i64> {
    let bare = name.strip_prefix("--")?;
    if let Some(n) = bare.strip_prefix("memByte_") {
        return n.parse::<i64>().ok();
    }
    if let Some(n) = bare.strip_prefix('m') {
        return n.parse::<i64>().ok();
    }
    None
}

/// Match `calc(cell_addr >= var(--<dst_prop>))`.
/// Returns the dst property name on success.
fn match_ge_bound(test: &StyleTest, cell_addr: i64) -> Option<String> {
    let StyleTest::Compare {
        left,
        op: CompareOp::Ge,
        right,
    } = test
    else {
        return None;
    };
    let Expr::Literal(v) = left else {
        return None;
    };
    if (*v as i64) != cell_addr || v.fract() != 0.0 {
        return None;
    }
    let Expr::Var { name, .. } = right else {
        return None;
    };
    Some(name.clone())
}

/// Match `calc(cell_addr < var(--<dst_prop>) + var(--<count_prop>))`.
/// Returns `(dst_prop, count_prop)` on success.
fn match_lt_bound(test: &StyleTest, cell_addr: i64) -> Option<(String, String)> {
    let StyleTest::Compare {
        left,
        op: CompareOp::Lt,
        right,
    } = test
    else {
        return None;
    };
    let Expr::Literal(v) = left else {
        return None;
    };
    if (*v as i64) != cell_addr || v.fract() != 0.0 {
        return None;
    }
    let Expr::Calc(CalcOp::Add(a, b)) = right else {
        return None;
    };
    let Expr::Var { name: dst, .. } = a.as_ref() else {
        return None;
    };
    let Expr::Var { name: count, .. } = b.as_ref() else {
        return None;
    };
    Some((dst.clone(), count.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construct a memory-cell Assignment whose first branch is the bulk-fill
    /// range predicate, followed by a simple keep-fallback. Mirrors the CSS
    /// the transpiler is specified to emit.
    fn mk_memcell_with_fill_clause(addr: i64) -> Assignment {
        let cell_name = format!("--memByte_{addr}");
        let addr_lit = Expr::Literal(addr as f64);
        let bulk_dst_var = Expr::Var {
            name: "--bulkDst".to_string(),
            fallback: None,
        };
        let bulk_count_var = Expr::Var {
            name: "--bulkCount".to_string(),
            fallback: None,
        };

        let bulk_branch = StyleBranch {
            condition: StyleTest::And(vec![
                StyleTest::Single {
                    property: "--bulkOpKind".to_string(),
                    value: Expr::Literal(1.0),
                },
                StyleTest::Compare {
                    left: addr_lit.clone(),
                    op: CompareOp::Ge,
                    right: bulk_dst_var.clone(),
                },
                StyleTest::Compare {
                    left: addr_lit,
                    op: CompareOp::Lt,
                    right: Expr::Calc(CalcOp::Add(
                        Box::new(bulk_dst_var),
                        Box::new(bulk_count_var),
                    )),
                },
            ]),
            then: Expr::Var {
                name: "--bulkValue".to_string(),
                fallback: None,
            },
        };

        Assignment {
            property: cell_name,
            value: Expr::StyleCondition {
                branches: vec![bulk_branch],
                fallback: Box::new(Expr::Var {
                    name: format!("--__1memByte_{addr}"),
                    fallback: None,
                }),
            },
        }
    }

    #[test]
    fn recognises_bulk_fill_across_cells() {
        let assignments: Vec<Assignment> = (0..256)
            .map(|i| mk_memcell_with_fill_clause(0xA0000 + i))
            .collect();
        let result = recognise_bulk_fills(&assignments);
        assert_eq!(result.bindings.len(), 1);
        let b = &result.bindings[0];
        assert_eq!(b.kind_property, "--bulkOpKind");
        assert_eq!(b.kind_value, 1);
        assert_eq!(b.dst_property, "--bulkDst");
        assert_eq!(b.count_property, "--bulkCount");
        assert_eq!(b.val_property, "--bulkValue");
        assert_eq!(b.addr_range, 0xA0000..0xA0100);
    }

    #[test]
    fn sparse_range_rejected() {
        // 256 cells at 0xA0000..0xA0100 but missing 0xA0050.
        let assignments: Vec<Assignment> = (0..256)
            .filter(|i| *i != 0x50)
            .map(|i| mk_memcell_with_fill_clause(0xA0000 + i))
            .collect();
        let result = recognise_bulk_fills(&assignments);
        assert!(
            result.bindings.is_empty(),
            "sparse run must be rejected; got {:?}",
            result.bindings
        );
    }

    #[test]
    fn distinct_bindings_emitted_separately() {
        // 100 cells share --bulkDst, another 100 share --otherDst.
        // Each forms its own contiguous run, so we emit two bindings.
        fn mk_with_dst_count(addr: i64, dst: &str, count: &str, val: &str) -> Assignment {
            let cell_name = format!("--memByte_{addr}");
            let addr_lit = Expr::Literal(addr as f64);
            let dst_var = Expr::Var {
                name: dst.to_string(),
                fallback: None,
            };
            let count_var = Expr::Var {
                name: count.to_string(),
                fallback: None,
            };
            Assignment {
                property: cell_name,
                value: Expr::StyleCondition {
                    branches: vec![StyleBranch {
                        condition: StyleTest::And(vec![
                            StyleTest::Single {
                                property: "--bulkOpKind".to_string(),
                                value: Expr::Literal(1.0),
                            },
                            StyleTest::Compare {
                                left: addr_lit.clone(),
                                op: CompareOp::Ge,
                                right: dst_var.clone(),
                            },
                            StyleTest::Compare {
                                left: addr_lit,
                                op: CompareOp::Lt,
                                right: Expr::Calc(CalcOp::Add(
                                    Box::new(dst_var),
                                    Box::new(count_var),
                                )),
                            },
                        ]),
                        then: Expr::Var {
                            name: val.to_string(),
                            fallback: None,
                        },
                    }],
                    fallback: Box::new(Expr::Literal(0.0)),
                },
            }
        }
        let mut assignments: Vec<Assignment> = (0..100)
            .map(|i| mk_with_dst_count(0x1000 + i, "--bulkDst", "--bulkCount", "--bulkValue"))
            .collect();
        assignments.extend(
            (0..100).map(|i| {
                mk_with_dst_count(0x2000 + i, "--otherDst", "--otherCount", "--otherValue")
            }),
        );
        let result = recognise_bulk_fills(&assignments);
        assert_eq!(result.bindings.len(), 2);
        // Order by addr_range.start (deterministic).
        assert_eq!(result.bindings[0].dst_property, "--bulkDst");
        assert_eq!(result.bindings[0].addr_range, 0x1000..0x1064);
        assert_eq!(result.bindings[1].dst_property, "--otherDst");
        assert_eq!(result.bindings[1].addr_range, 0x2000..0x2064);
    }

    #[test]
    fn non_bulk_assignment_ignored() {
        // A broadcast-write-style cell (no range predicate) contributes nothing.
        let a = Assignment {
            property: "--memByte_100".to_string(),
            value: Expr::StyleCondition {
                branches: vec![StyleBranch {
                    condition: StyleTest::Single {
                        property: "--memAddr0".to_string(),
                        value: Expr::Literal(100.0),
                    },
                    then: Expr::Var {
                        name: "--memVal0".to_string(),
                        fallback: None,
                    },
                }],
                fallback: Box::new(Expr::Literal(0.0)),
            },
        };
        let result = recognise_bulk_fills(&[a]);
        assert!(result.bindings.is_empty());
    }

    #[test]
    fn wrong_compare_op_rejected() {
        // If the transpiler writes `>` instead of `>=`, the cell must NOT
        // match — Chrome would evaluate different bytes, so calcite must not
        // claim this is a bulk fill.
        let addr = 42i64;
        let cell_name = format!("--memByte_{addr}");
        let addr_lit = Expr::Literal(addr as f64);
        let bulk_dst_var = Expr::Var {
            name: "--bulkDst".to_string(),
            fallback: None,
        };
        let bulk_count_var = Expr::Var {
            name: "--bulkCount".to_string(),
            fallback: None,
        };
        let a = Assignment {
            property: cell_name,
            value: Expr::StyleCondition {
                branches: vec![StyleBranch {
                    condition: StyleTest::And(vec![
                        StyleTest::Single {
                            property: "--bulkOpKind".to_string(),
                            value: Expr::Literal(1.0),
                        },
                        StyleTest::Compare {
                            left: addr_lit.clone(),
                            op: CompareOp::Gt, // wrong — should be Ge
                            right: bulk_dst_var.clone(),
                        },
                        StyleTest::Compare {
                            left: addr_lit,
                            op: CompareOp::Lt,
                            right: Expr::Calc(CalcOp::Add(
                                Box::new(bulk_dst_var),
                                Box::new(bulk_count_var),
                            )),
                        },
                    ]),
                    then: Expr::Var {
                        name: "--bulkValue".to_string(),
                        fallback: None,
                    },
                }],
                fallback: Box::new(Expr::Literal(0.0)),
            },
        };
        let result = recognise_bulk_fills(&[a]);
        assert!(result.bindings.is_empty());
    }
}
