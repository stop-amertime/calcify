//! Broadcast write pattern: `if(style(--dest: {addr}): value; else: keep)` → direct store.
//!
//! Detects: a set of assignments where each has the form:
//!   `--varN: if(style(--dest: N): value; else: var(--__1varN))`
//! All checking the same destination property against their own address.
//!
//! Works with both legacy (`--addrDestA/B/C`) and v2 (`--memAddr0/1/2`) patterns.
//!
//! Replaces: evaluate `--dest` once, write `value` to `state[dest]` directly.

use std::collections::{HashMap, HashSet};

use crate::types::*;

/// A recognised broadcast write pattern.
#[derive(Debug, Clone)]
pub struct BroadcastWrite {
    /// The destination address property (e.g., `--addrDestA`).
    pub dest_property: String,
    /// The value expression property (e.g., `--addrValA`).
    pub value_expr: Expr,
    /// The address → variable name mapping (O(1) lookup).
    /// Key: the integer address that each variable checks against.
    /// Value: the variable name that should be written to.
    pub address_map: HashMap<i64, String>,
    /// Word-write spillover: when `isWordWrite == 1` and dest is address N,
    /// also write the high byte to address N+1.
    /// Key: the address being tested (N-1 in the condition).
    /// Value: (target_var_name, value_expr for the high byte).
    pub spillover_map: HashMap<i64, (String, Expr)>,
    /// The property that gates spillover writes (e.g., `--isWordWrite`).
    pub spillover_guard: Option<String>,
    /// Optional "outer gate": if set, the broadcast write only fires when
    /// this property == 1. Used for broadcast assignments shaped like
    ///   `if(style(--memAddr0:N): valN;
    ///       style(--gate:1): if(style(--memAddr2:N): valN; ...);
    ///       else: keep)`
    /// where the inner broadcast lives inside a gating `style(--gate: 1)`
    /// branch. The recogniser peels the gate off and records it here, so
    /// the executor can skip the whole broadcast on ticks where the gate
    /// is 0 without evaluating any of its address entries.
    pub gate_property: Option<String>,
}

/// Result of broadcast pattern recognition.
pub struct BroadcastResult {
    /// Recognised broadcast write patterns.
    pub writes: Vec<BroadcastWrite>,
    /// Property names absorbed into broadcast writes (should be removed from the assignment loop).
    pub absorbed_properties: HashSet<String>,
}

/// Analyse a set of assignments to detect the broadcast write pattern.
///
/// Memory cells in x86CSS check multiple write ports:
///   `--m0: if(style(--addrDestA:0): valA; style(--addrDestB:0): valB; else: keep)`
///
/// We create one `BroadcastWrite` per port. Assignments where ALL branches are
/// pure `--addrDest*` checks are absorbed; register assignments that mix in
/// execution logic (e.g. `--addrJump`) are left in the normal assignment loop.
pub fn recognise_broadcast(assignments: &[Assignment]) -> BroadcastResult {
    let _p1 = web_time::Instant::now();
    // Phase 1: Collect all broadcast port entries, grouped by
    // (dest_property, gate_property). Keying by the gate as well as the dest
    // keeps ungated and gated ports from colliding into one group — each gate
    // forms its own broadcast-write entry so the gate can be checked once per
    // group at tick time.
    type DirectKey = (String, Option<String>);
    let mut direct_groups: HashMap<DirectKey, Vec<(i64, String, Expr)>> = HashMap::new();
    let mut spillover_groups: HashMap<String, Vec<(i64, String, String, Expr)>> = HashMap::new();
    let mut pure_broadcast: HashSet<String> = HashSet::new();

    // Track how many direct ports each assignment contributes (across all dest
    // properties). An assignment can only be absorbed if ALL its ports land in
    // broadcast groups — otherwise the non-absorbed ports lose their evaluation.
    let mut port_counts: HashMap<String, usize> = HashMap::new();

    for assignment in assignments {
        // Skip buffer copies — they're no-ops in mutable state and never broadcast targets.
        if assignment.property.starts_with("--__") {
            continue;
        }
        if let Some(ports) = extract_broadcast_ports(assignment) {
            pure_broadcast.insert(assignment.property.clone());
            for port in ports {
                match port {
                    BroadcastPort::Direct {
                        dest_property,
                        address,
                        value_expr,
                        gate_property,
                    } => {
                        *port_counts.entry(assignment.property.clone()).or_insert(0) += 1;
                        direct_groups
                            .entry((dest_property, gate_property))
                            .or_default()
                            .push((address, assignment.property.clone(), value_expr));
                    }
                    BroadcastPort::Spillover {
                        dest_property,
                        source_address,
                        guard_property,
                        value_expr,
                    } => {
                        spillover_groups.entry(dest_property).or_default().push((
                            source_address,
                            assignment.property.clone(),
                            guard_property,
                            value_expr,
                        ));
                    }
                }
            }
        }
    }

    log::info!("[bw phase1] collection: {:.2}s ({} assignments, {} direct groups)",
        _p1.elapsed().as_secs_f64(), assignments.len(),
        direct_groups.values().map(|v| v.len()).sum::<usize>());
    let _p2 = web_time::Instant::now();
    // Track how many of each assignment's ports were actually absorbed.
    let mut absorbed_port_counts: HashMap<String, usize> = HashMap::new();
    // Count of large groups that skip per-entry absorption tracking
    let mut bulk_absorbed_groups: usize = 0;

    // Phase 2: For each (dest_property, gate_property), find the majority
    // value expression and only absorb entries that use it.
    //
    // Spillovers attach only to the ungated group for a given dest_property —
    // gated broadcast writes and spillovers are never combined today.
    let writes: Vec<BroadcastWrite> = direct_groups
        .into_iter()
        .filter_map(|((dest_property, gate_property), entries)| {
            // Group entries by value expression using HashMap (O(n) via hashing)
            // instead of linear scan with deep equality (O(n * tree_depth)).
            let mut expr_map: HashMap<Expr, Vec<(i64, String)>> = HashMap::new();
            for (addr, name, value_expr) in entries {
                expr_map.entry(value_expr).or_default().push((addr, name));
            }
            // Convert to vec for max_by_key
            let expr_groups: Vec<(Expr, Vec<(i64, String)>)> = expr_map.into_iter().collect();

            // Pick the largest group (the true broadcast targets).
            let (value_expr, entries) = expr_groups
                .into_iter()
                .max_by_key(|(_, entries)| entries.len())?;

            if entries.len() < 10 {
                return None;
            }

            // A broadcast write maps different addresses to different target
            // properties. If most entries map to the same property (e.g. a single
            // register's opcode dispatch), this is not a broadcast pattern.
            // Skip the expensive uniqueness check for large groups (>10K entries)
            // — these are always memory broadcast patterns with unique targets.
            if entries.len() <= 10_000 {
                let unique_names: HashSet<&str> = entries.iter().map(|(_, n)| n.as_str()).collect();
                if unique_names.len() * 2 < entries.len() {
                    return None;
                }
            }

            let entries_len = entries.len();
            let address_map: HashMap<i64, String> = entries.into_iter().collect();
            // Track absorption counts. For large groups (>10K entries), use
            // a bulk flag instead of per-entry HashMap updates to avoid 6M+ String clones.
            if entries_len <= 10_000 {
                for name in address_map.values() {
                    *absorbed_port_counts.entry(name.clone()).or_insert(0) += 1;
                }
            } else {
                bulk_absorbed_groups += 1;
            }

            // Build spillover map for this dest_property. Only the ungated
            // group (gate_property == None) picks up spillovers — gated
            // broadcasts don't have spillover siblings in the recognised
            // CSS shape today.
            let (spillover_map, spillover_guard) = if gate_property.is_none() {
                let spillovers = spillover_groups.remove(&dest_property);
                if let Some(spills) = spillovers {
                    let guard = spills.first().map(|(_, _, g, _)| g.clone());
                    let map = spills
                        .into_iter()
                        .map(|(src_addr, var_name, _, val_expr)| (src_addr, (var_name, val_expr)))
                        .collect();
                    (map, guard)
                } else {
                    (HashMap::new(), None)
                }
            } else {
                (HashMap::new(), None)
            };

            Some(BroadcastWrite {
                dest_property,
                value_expr,
                address_map,
                spillover_map,
                spillover_guard,
                gate_property,
            })
        })
        .collect();

    // An assignment is only absorbed if:
    // 1. It's a pure broadcast target (all branches are addrDest checks)
    // 2. ALL of its direct ports were absorbed into broadcast groups
    //
    // Split registers like DX have some ports that match the majority expression
    // (e.g. var(--addrValA) for word writes) and others that don't (byte-merge
    // expressions). If ANY port is excluded, the assignment must stay in the
    // normal loop so those cases are still evaluated.
    let mut absorbed_properties = HashSet::new();
    for (name, total) in &port_counts {
        // For properties tracked individually, check absorbed count matches total
        let individually_absorbed = absorbed_port_counts.get(name).copied().unwrap_or(0);
        // For large groups, each bulk group contributes 1 absorbed port per property.
        // The property is fully absorbed if individual + bulk counts cover all ports.
        let total_absorbed = individually_absorbed + bulk_absorbed_groups;
        if total_absorbed == *total && pure_broadcast.contains(name) {
            absorbed_properties.insert(name.clone());
        }
    }

    log::info!("[bw phase2] grouping+absorption: {:.2}s ({} writes, {} absorbed)",
        _p2.elapsed().as_secs_f64(), writes.len(), absorbed_properties.len());
    BroadcastResult {
        writes,
        absorbed_properties,
    }
}

/// A port extracted from a broadcast write assignment.
#[derive(Debug)]
enum BroadcastPort {
    /// Direct write: `style(--addrDestX: ADDR) → value`
    ///
    /// `gate_property` is set when the port came from an inner branch of
    /// `style(--gate: 1): if(...)`. Gated and ungated ports for the same
    /// dest_property form separate BroadcastWrite entries so the gate can be
    /// checked once per group at tick time.
    Direct {
        dest_property: String,
        address: i64,
        value_expr: Expr,
        gate_property: Option<String>,
    },
    /// Spillover write: `style(--addrDestX: ADDR) and style(--isWordWrite: 1) → value`
    /// The address is the *source* address (N-1); the target cell is at N.
    Spillover {
        dest_property: String,
        source_address: i64,
        guard_property: String,
        value_expr: Expr,
    },
}

/// Extract all broadcast write ports from an assignment.
///
/// Returns `None` if:
/// - Any branch tests a non-address property (execution logic mixed in)
/// - The fallback (else branch) is not a simple keep of the previous value
///
/// Registers like SP, SI, DI have side-channel deltas in their else branches
/// (e.g., `else: calc(var(--__1SP) + var(--moveStack))`) and must NOT be absorbed.
///
/// The address property is recognised structurally (any `style(--X: <int>)` condition),
/// not by name prefix. This handles both legacy `--addrDestA/B/C` and v2 `--memAddr0/1/2`.
///
/// Returns `Some(vec of BroadcastPort)` — one per branch.
fn extract_broadcast_ports(assignment: &Assignment) -> Option<Vec<BroadcastPort>> {
    match &assignment.value {
        Expr::StyleCondition { branches, fallback } => {
            // Check the fallback: only absorb if it's a simple keep (var(--__1X) or var(--X)).
            // If the fallback has computation (calc, function call, etc.), this register
            // has side-channel logic that must be evaluated on every tick.
            if !is_simple_keep(fallback, &assignment.property) {
                return None;
            }

            let mut ports = Vec::with_capacity(branches.len());
            if !extract_branches_into(&assignment.property, branches, None, &mut ports) {
                return None;
            }
            if ports.is_empty() {
                None
            } else {
                Some(ports)
            }
        }
        _ => None,
    }
}

/// Try to interpret each branch as a broadcast port and append to `ports`.
/// Returns false if any branch isn't a valid port shape (bail whole assignment).
///
/// `outer_gate` is the gate carried down from an enclosing `style(--gate: 1): if(...)`
/// branch, or None at the top level. It's stamped onto every Direct port emitted
/// from this call, so inner broadcasts stay grouped separately by gate in phase 1.
fn extract_branches_into(
    property: &str,
    branches: &[StyleBranch],
    outer_gate: Option<&str>,
    ports: &mut Vec<BroadcastPort>,
) -> bool {
    for branch in branches {
        match &branch.condition {
            StyleTest::Single {
                property: cond_prop,
                value: Expr::Literal(v),
            } => {
                let val = *v as i64;
                // Gated-inner-broadcast pattern:
                //   style(--gate: 1): if(<inner StyleCondition with simple keep>)
                // Recurse into the inner branches, stamping the gate onto each
                // emitted Direct port. This lets the CSS nest broadcast writes
                // behind a rarely-true gate without defeating recognition.
                //
                // Only one level of gating is supported per port — an inner
                // branch carrying its own `style(--gate2: 1)` would form a
                // second gate, and we don't currently combine two gates into
                // one port. (Deeply-nested gates still recurse; the innermost
                // gate wins for grouping purposes. In practice the CSS only
                // uses two levels and we don't need a more general solution.)
                if val == 1 {
                    if let Expr::StyleCondition {
                        branches: inner_branches,
                        fallback: inner_fallback,
                    } = &branch.then
                    {
                        if is_simple_keep(inner_fallback, property) {
                            // Recurse; the gate property for inner ports is the
                            // current condition's property. If we already have
                            // an outer_gate, keep the innermost (same rationale
                            // as the comment above).
                            let gate = cond_prop.as_str();
                            if !extract_branches_into(property, inner_branches, Some(gate), ports) {
                                return false;
                            }
                            continue;
                        }
                    }
                }
                // Regular direct port (possibly under an outer gate).
                ports.push(BroadcastPort::Direct {
                    dest_property: cond_prop.clone(),
                    address: val,
                    value_expr: branch.then.clone(),
                    gate_property: outer_gate.map(|s| s.to_string()),
                });
            }
            StyleTest::Single { .. } => return false,
            StyleTest::And(tests) if tests.len() == 2 => {
                // Match: style(--addrX: N) and style(--guard: 1)
                // Identify the address test (the one whose property also appears
                // in single-condition branches) vs the guard test (value == 1).
                let (mut addr_test, mut guard_test) = (None, None);
                for t in tests {
                    if let StyleTest::Single {
                        property: p,
                        value: Expr::Literal(v),
                    } = t
                    {
                        if *v as i64 == 1 && guard_test.is_none() {
                            guard_test = Some(p.clone());
                        } else if addr_test.is_none() {
                            addr_test = Some((p.clone(), *v as i64));
                        }
                    }
                }
                match (addr_test, guard_test) {
                    (Some((dest_property, source_address)), Some(guard_property)) => {
                        // Spillover ports under an outer gate aren't supported;
                        // bail rather than silently drop the gate.
                        if outer_gate.is_some() {
                            return false;
                        }
                        ports.push(BroadcastPort::Spillover {
                            dest_property,
                            source_address,
                            guard_property,
                            value_expr: branch.then.clone(),
                        });
                    }
                    _ => return false,
                }
            }
            _ => return false,
        }
    }
    true
}

/// Check if a fallback expression is a simple "keep previous value" pattern.
///
/// Pure broadcast targets use `var(--__1X)` as their fallback — just keeping the
/// previous tick's value when no write port targets them. Registers with side channels
/// (SP, SI, DI, CS, flags) have computation in their else branches and must not be
/// absorbed into the broadcast write optimization.
fn is_simple_keep(fallback: &Expr, property_name: &str) -> bool {
    match fallback {
        Expr::Var { name, .. } => {
            // Accept var(--__1X) or var(--__0X) or var(--X) as simple keeps
            let bare = if let Some(rest) = name.strip_prefix("--__") {
                // --__0X, --__1X, --__2X → X
                &rest[1..]
            } else if let Some(rest) = name.strip_prefix("--") {
                rest
            } else {
                return false;
            };
            let prop_bare = property_name.strip_prefix("--").unwrap_or(property_name);
            bare == prop_bare
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_broadcast_assignment(name: &str, addr: i64) -> Assignment {
        Assignment {
            property: format!("--{name}"),
            value: Expr::StyleCondition {
                branches: vec![StyleBranch {
                    condition: StyleTest::Single {
                        property: "--addrDestA".to_string(),
                        value: Expr::Literal(addr as f64),
                    },
                    then: Expr::Var {
                        name: "--addrValA".to_string(),
                        fallback: None,
                    },
                }],
                fallback: Box::new(Expr::Var {
                    name: format!("--__1{name}"),
                    fallback: None,
                }),
            },
        }
    }

    /// Build a compound memory cell assignment matching the x86CSS pattern:
    /// `--mN: if(style(--addrDestA:N):val1; style(--addrDestA:N-1) and style(--isWordWrite:1):val2; style(--addrDestB:N):valB; else:keep)`
    fn make_compound_broadcast_assignment(name: &str, addr: i64) -> Assignment {
        let mut branches = vec![StyleBranch {
            condition: StyleTest::Single {
                property: "--addrDestA".to_string(),
                value: Expr::Literal(addr as f64),
            },
            then: Expr::Var {
                name: "--addrValA".to_string(),
                fallback: None,
            },
        }];
        if addr > 0 {
            // Add spillover branch: style(--addrDestA: addr-1) and style(--isWordWrite: 1) → high byte
            branches.push(StyleBranch {
                condition: StyleTest::And(vec![
                    StyleTest::Single {
                        property: "--addrDestA".to_string(),
                        value: Expr::Literal((addr - 1) as f64),
                    },
                    StyleTest::Single {
                        property: "--isWordWrite".to_string(),
                        value: Expr::Literal(1.0),
                    },
                ]),
                then: Expr::Var {
                    name: "--addrValA1".to_string(),
                    fallback: None,
                },
            });
        }
        // Add second write port
        branches.push(StyleBranch {
            condition: StyleTest::Single {
                property: "--addrDestB".to_string(),
                value: Expr::Literal(addr as f64),
            },
            then: Expr::Var {
                name: "--addrValB".to_string(),
                fallback: None,
            },
        });

        Assignment {
            property: format!("--{name}"),
            value: Expr::StyleCondition {
                branches,
                fallback: Box::new(Expr::Var {
                    name: format!("--__1{name}"),
                    fallback: None,
                }),
            },
        }
    }

    #[test]
    fn detects_simple_broadcast() {
        let assignments: Vec<Assignment> = (0..20)
            .map(|i| make_broadcast_assignment(&format!("m{i}"), i))
            .collect();
        let result = recognise_broadcast(&assignments);
        assert_eq!(result.writes.len(), 1);
        assert_eq!(result.writes[0].dest_property, "--addrDestA");
        assert_eq!(result.writes[0].address_map.len(), 20);
        assert_eq!(result.absorbed_properties.len(), 20);
    }

    #[test]
    fn compound_memory_cell_broadcast() {
        let assignments: Vec<Assignment> = (0..20)
            .map(|i| make_compound_broadcast_assignment(&format!("m{i}"), i))
            .collect();
        let result = recognise_broadcast(&assignments);
        assert_eq!(
            result.writes.len(),
            2,
            "Should have 2 write ports (A and B)"
        );
        let write_a = result
            .writes
            .iter()
            .find(|w| w.dest_property == "--addrDestA")
            .expect("Should have --addrDestA");
        assert_eq!(write_a.address_map.len(), 20);
        assert!(!write_a.spillover_map.is_empty());
        assert_eq!(
            write_a.spillover_guard.as_deref(),
            Some("--isWordWrite"),
            "Spillover should be gated by --isWordWrite"
        );
    }

    #[test]
    fn side_channel_not_absorbed() {
        // SP has a side channel: else: calc(var(--__1SP) + var(--moveStack))
        let sp_assignment = Assignment {
            property: "--SP".to_string(),
            value: Expr::StyleCondition {
                branches: vec![StyleBranch {
                    condition: StyleTest::Single {
                        property: "--addrDestA".to_string(),
                        value: Expr::Literal(-5.0),
                    },
                    then: Expr::Var {
                        name: "--addrValA".to_string(),
                        fallback: None,
                    },
                }],
                fallback: Box::new(Expr::Calc(CalcOp::Add(
                    Box::new(Expr::Var {
                        name: "--__1SP".to_string(),
                        fallback: None,
                    }),
                    Box::new(Expr::Var {
                        name: "--moveStack".to_string(),
                        fallback: None,
                    }),
                ))),
            },
        };
        let mut assignments: Vec<Assignment> = (0..20)
            .map(|i| make_broadcast_assignment(&format!("m{i}"), i))
            .collect();
        assignments.push(sp_assignment);
        let result = recognise_broadcast(&assignments);
        assert!(!result.absorbed_properties.contains("--SP"));
    }

    #[test]
    fn too_few_entries_not_recognised() {
        let assignments: Vec<Assignment> = (0..5)
            .map(|i| make_broadcast_assignment(&format!("m{i}"), i))
            .collect();
        let result = recognise_broadcast(&assignments);
        assert!(result.writes.is_empty());
    }

    /// Build a v2-style memory cell: 3 write ports with --memAddr0/1/2.
    /// `--mN: if(style(--memAddr0:N):var(--memVal0); style(--memAddr1:N):var(--memVal1); style(--memAddr2:N):var(--memVal2); else:var(--__1mN))`
    fn make_v2_broadcast_assignment(name: &str, addr: i64) -> Assignment {
        Assignment {
            property: format!("--{name}"),
            value: Expr::StyleCondition {
                branches: vec![
                    StyleBranch {
                        condition: StyleTest::Single {
                            property: "--memAddr0".to_string(),
                            value: Expr::Literal(addr as f64),
                        },
                        then: Expr::Var {
                            name: "--memVal0".to_string(),
                            fallback: None,
                        },
                    },
                    StyleBranch {
                        condition: StyleTest::Single {
                            property: "--memAddr1".to_string(),
                            value: Expr::Literal(addr as f64),
                        },
                        then: Expr::Var {
                            name: "--memVal1".to_string(),
                            fallback: None,
                        },
                    },
                    StyleBranch {
                        condition: StyleTest::Single {
                            property: "--memAddr2".to_string(),
                            value: Expr::Literal(addr as f64),
                        },
                        then: Expr::Var {
                            name: "--memVal2".to_string(),
                            fallback: None,
                        },
                    },
                ],
                fallback: Box::new(Expr::Var {
                    name: format!("--__1{name}"),
                    fallback: None,
                }),
            },
        }
    }

    #[test]
    fn detects_v2_broadcast() {
        let assignments: Vec<Assignment> = (0..20)
            .map(|i| make_v2_broadcast_assignment(&format!("m{i}"), i))
            .collect();
        let result = recognise_broadcast(&assignments);
        assert_eq!(result.writes.len(), 3, "Should have 3 write ports (memAddr0/1/2)");
        for port_name in &["--memAddr0", "--memAddr1", "--memAddr2"] {
            let write = result
                .writes
                .iter()
                .find(|w| w.dest_property == *port_name)
                .unwrap_or_else(|| panic!("Should have {port_name}"));
            assert_eq!(write.address_map.len(), 20);
        }
        assert_eq!(result.absorbed_properties.len(), 20);
    }

    #[test]
    fn opcode_dispatch_not_absorbed_as_broadcast() {
        // Simulate a register dispatch: --nextAX: if(style(--opcode: 0): calc(1); style(--opcode: 1): calc(2); ...)
        // All branches from the same assignment, different value exprs → should NOT be a broadcast.
        let assignments: Vec<Assignment> = vec![Assignment {
            property: "--nextAX".to_string(),
            value: Expr::StyleCondition {
                branches: (0..20)
                    .map(|i| StyleBranch {
                        condition: StyleTest::Single {
                            property: "--opcode".to_string(),
                            value: Expr::Literal(i as f64),
                        },
                        then: Expr::Literal(i as f64 + 100.0),
                    })
                    .collect(),
                fallback: Box::new(Expr::Var {
                    name: "--__1nextAX".to_string(),
                    fallback: None,
                }),
            },
        }];
        let result = recognise_broadcast(&assignments);
        assert!(
            result.writes.is_empty(),
            "Opcode dispatch should not be recognised as broadcast"
        );
        assert!(result.absorbed_properties.is_empty());
    }
}
