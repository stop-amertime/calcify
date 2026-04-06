//! Broadcast write pattern: `if(style(--dest: {addr}): value; else: keep)` → direct store.
//!
//! Detects: a set of assignments where each has the form:
//!   `--varN: if(style(--addrDest: N): value; else: var(--__1varN))`
//! All checking the same destination property against their own address.
//!
//! Replaces: evaluate `--addrDest` once, write `value` to `state[dest]` directly.

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
    // Group by dest_property → (address, target_var_name, value_expr)
    let mut groups: HashMap<String, Vec<(i64, String, Expr)>> = HashMap::new();
    let mut pure_broadcast: HashSet<String> = HashSet::new();

    for assignment in assignments {
        if let Some(ports) = extract_broadcast_ports(assignment) {
            pure_broadcast.insert(assignment.property.clone());
            for (dest_prop, addr, val_expr) in ports {
                groups.entry(dest_prop).or_default().push((
                    addr,
                    assignment.property.clone(),
                    val_expr,
                ));
            }
        }
    }

    let mut absorbed_properties = HashSet::new();

    // Only keep groups that are large enough to be a real broadcast pattern
    let writes = groups
        .into_iter()
        .filter(|(_, entries)| entries.len() >= 10)
        .map(|(dest_property, entries)| {
            let value_expr = entries
                .first()
                .map(|(_, _, expr)| expr.clone())
                .unwrap_or(Expr::Literal(0.0));
            for (_, name, _) in &entries {
                absorbed_properties.insert(name.clone());
            }
            let address_map = entries
                .into_iter()
                .map(|(addr, name, _)| (addr, name))
                .collect();
            BroadcastWrite {
                dest_property,
                value_expr,
                address_map,
            }
        })
        .collect();

    // Only absorb properties that are pure broadcast targets
    absorbed_properties.retain(|p| pure_broadcast.contains(p));

    BroadcastResult {
        writes,
        absorbed_properties,
    }
}

/// Extract all broadcast write ports from an assignment.
///
/// Returns `None` if any branch tests a non-`--addrDest*` property (meaning
/// the assignment has execution logic mixed in and is not a pure broadcast target).
///
/// Returns `Some(vec of (dest_property, address, value_expr))` — one per branch.
fn extract_broadcast_ports(assignment: &Assignment) -> Option<Vec<(String, i64, Expr)>> {
    match &assignment.value {
        Expr::StyleCondition { branches, .. } => {
            let mut ports = Vec::with_capacity(branches.len());
            for branch in branches {
                match &branch.condition {
                    StyleTest::Single { property, value } => {
                        if !property.starts_with("--addrDest") {
                            return None;
                        }
                        match value {
                            Expr::Literal(v) => {
                                ports.push((property.clone(), *v as i64, branch.then.clone()));
                            }
                            _ => return None,
                        }
                    }
                    _ => return None,
                }
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

    #[test]
    fn recognises_broadcast_pattern() {
        let assignments: Vec<_> = (0..20)
            .map(|i| make_broadcast_assignment(&format!("m{i}"), i))
            .collect();

        let result = recognise_broadcast(&assignments);
        assert_eq!(result.writes.len(), 1);
        assert_eq!(result.writes[0].dest_property, "--addrDestA");
        assert_eq!(result.writes[0].address_map.len(), 20);
        assert_eq!(result.absorbed_properties.len(), 20);
        assert!(result.absorbed_properties.contains("--m0"));
        assert!(result.absorbed_properties.contains("--m19"));
    }

    #[test]
    fn ignores_small_groups() {
        let assignments: Vec<_> = (0..3)
            .map(|i| make_broadcast_assignment(&format!("m{i}"), i))
            .collect();

        let result = recognise_broadcast(&assignments);
        assert!(result.writes.is_empty());
        assert!(result.absorbed_properties.is_empty());
    }
}
