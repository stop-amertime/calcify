//! Count v2 DAG node kinds for a real cabinet.
//!
//! Used to measure the impact of idiom recognisers (Phase 2): every
//! recogniser that fires should reduce raw-`If` count and increase
//! `Switch` (or other super-node) count. Per `compiler-mission.md`,
//! Phase 2's gate is node-count reduction on Doom-class cabinets.
//!
//! Usage: `cargo run --release --bin probe-dag-node-counts -- <cabinet.css>`

use std::collections::BTreeMap;
use std::env;
use std::fs;

use calcite_core::dag::types::{DagNode, NodeId, TickPosition, TRANSIENT_BASE};
use calcite_core::dag::build_dag;
use calcite_core::dag::Dag;
use calcite_core::parser::parse_css;
use calcite_core::State;

fn mark_reachable(dag: &Dag) -> Vec<bool> {
    let mut reach = vec![false; dag.nodes.len()];
    let mut stack: Vec<NodeId> = Vec::new();
    for &t in &dag.terminals {
        stack.push(t);
    }
    for &r in &dag.function_roots {
        if (r as usize) < dag.nodes.len() {
            stack.push(r);
        }
    }
    for bw in &dag.dag_broadcasts {
        stack.push(bw.value_node);
        for (_, hi) in bw.spillover_map.values() {
            stack.push(*hi);
        }
    }
    while let Some(id) = stack.pop() {
        let i = id as usize;
        if i >= reach.len() || reach[i] {
            continue;
        }
        reach[i] = true;
        match &dag.nodes[i] {
            DagNode::Lit(_) | DagNode::LitStr(_) | DagNode::LoadVar { .. }
            | DagNode::Param(_) | DagNode::IndirectStore { .. } => {}
            DagNode::Calc { args, .. } => stack.extend(args),
            DagNode::Call { args, .. } => stack.extend(args),
            DagNode::If { branches, fallback } => {
                stack.push(*fallback);
                for (cond, then_id) in branches {
                    stack.push(*then_id);
                    visit_cond(cond, &mut stack);
                }
            }
            DagNode::Switch { key, table, fallback } => {
                stack.push(*key);
                stack.push(*fallback);
                stack.extend(table.values().copied());
            }
            DagNode::Concat(parts) => stack.extend(parts),
            DagNode::WriteVar { value, .. } => stack.push(*value),
            DagNode::BitField { src, .. } => stack.push(*src),
            DagNode::LoadVarDynamic { slot_expr, fallback, .. } => {
                stack.push(*slot_expr);
                stack.push(*fallback);
            }
            DagNode::BitwiseOp { args, .. } => stack.extend(args),
        }
    }
    reach
}

fn visit_cond(cond: &calcite_core::dag::types::StyleCondNode, stack: &mut Vec<NodeId>) {
    use calcite_core::dag::types::StyleCondNode;
    match cond {
        StyleCondNode::Single { lhs, value } => {
            stack.push(*lhs);
            stack.push(*value);
        }
        StyleCondNode::And(parts) | StyleCondNode::Or(parts) => {
            for p in parts {
                visit_cond(p, stack);
            }
        }
    }
}

fn main() {
    let path = env::args().nth(1).expect("usage: probe-dag-node-counts <cabinet.css>");
    let css = fs::read_to_string(&path).expect("read cabinet");
    let program = parse_css(&css).expect("parse");
    // build_dag relies on the global address map populated by
    // State::load_properties — same ordering Evaluator::from_parsed uses.
    let mut state = State::new(1 << 20);
    state.load_properties(&program.properties);
    let dag = build_dag(&program);

    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut if_branch_total: usize = 0;
    let mut switch_table_total: usize = 0;
    let mut max_if_branches: usize = 0;
    let mut max_switch_entries: usize = 0;
    let mut lv_state_var_curr: usize = 0;
    let mut lv_state_var_prev: usize = 0;
    let mut lv_memory_curr: usize = 0;
    let mut lv_memory_prev: usize = 0;
    let mut lv_transient: usize = 0;

    for n in &dag.nodes {
        let key = match n {
            DagNode::Lit(_) => "Lit",
            DagNode::LitStr(_) => "LitStr",
            DagNode::LoadVar { slot, kind } => {
                let s = *slot;
                let prev = matches!(kind, TickPosition::Prev);
                if s >= TRANSIENT_BASE {
                    lv_transient += 1;
                } else if s < 0 {
                    if prev { lv_state_var_prev += 1; } else { lv_state_var_curr += 1; }
                } else if prev {
                    lv_memory_prev += 1;
                } else {
                    lv_memory_curr += 1;
                }
                "LoadVar"
            }
            DagNode::Calc { .. } => "Calc",
            DagNode::Call { .. } => "Call",
            DagNode::Param(_) => "Param",
            DagNode::If { branches, .. } => {
                if_branch_total += branches.len();
                if branches.len() > max_if_branches {
                    max_if_branches = branches.len();
                }
                "If"
            }
            DagNode::Switch { table, .. } => {
                switch_table_total += table.len();
                if table.len() > max_switch_entries {
                    max_switch_entries = table.len();
                }
                "Switch"
            }
            DagNode::Concat(_) => "Concat",
            DagNode::WriteVar { .. } => "WriteVar",
            DagNode::IndirectStore { .. } => "IndirectStore",
            DagNode::BitField { .. } => "BitField",
            DagNode::LoadVarDynamic { .. } => "LoadVarDynamic",
            DagNode::BitwiseOp { .. } => "BitwiseOp",
        };
        *counts.entry(key).or_insert(0) += 1;
    }

    // Reachability sweep from terminals + function roots + broadcast
    // value nodes. Anything not reached is dead code in the arena —
    // present in dag.nodes but never visited at evaluation time.
    let reachable = mark_reachable(&dag);

    println!("=== DAG node counts for {path} ===");
    println!("total nodes: {} (reachable: {})", dag.nodes.len(), reachable.iter().filter(|&&r| r).count());
    println!("terminals:   {}", dag.terminals.len());
    println!("functions:   {}", dag.function_roots.len());
    println!("dag_broadcasts: {}", dag.dag_broadcasts.len());
    println!("dag_packed_broadcasts: {}", dag.dag_packed_broadcasts.len());
    println!();
    println!("by kind:");
    for (k, v) in &counts {
        println!("  {k:<14} {v:>10}");
    }
    println!();
    println!("If branch total:     {if_branch_total} (max single-If branches: {max_if_branches})");
    println!("Switch entry total:  {switch_table_total} (max single-Switch entries: {max_switch_entries})");
    println!();
    // Inspect the largest Switch's first 4 entries.
    let mut largest_switch: Option<(usize, &std::collections::HashMap<i64, calcite_core::dag::NodeId>)> = None;
    for (i, n) in dag.nodes.iter().enumerate() {
        if let DagNode::Switch { table, .. } = n {
            if largest_switch.map(|(_, t)| t.len()).unwrap_or(0) < table.len() {
                largest_switch = Some((i, table));
            }
        }
    }
    if let Some((id, table)) = largest_switch {
        println!();
        println!("Largest Switch (node {id}, {} entries):", table.len());
        // Bucket bodies by kind.
        let mut n_loadvar = 0;
        let mut n_lit = 0;
        let mut n_other = 0;
        let mut other_examples: Vec<String> = Vec::new();
        for &body_id in table.values() {
            match &dag.nodes[body_id as usize] {
                DagNode::LoadVar { .. } => n_loadvar += 1,
                DagNode::Lit(_) => n_lit += 1,
                other => {
                    n_other += 1;
                    if other_examples.len() < 3 {
                        other_examples.push(format!("{other:?}"));
                    }
                }
            }
        }
        println!("  LoadVar bodies:  {n_loadvar:>10}");
        println!("  Lit bodies:      {n_lit:>10}");
        println!("  other bodies:    {n_other:>10}");
        for ex in &other_examples {
            println!("    e.g. {ex}");
        }
    }
    println!();
    println!("LoadVar breakdown:");
    println!("  state-var current  {lv_state_var_curr:>10}");
    println!("  state-var prev     {lv_state_var_prev:>10}");
    println!("  memory current     {lv_memory_curr:>10}");
    println!("  memory prev        {lv_memory_prev:>10}");
    println!("  transient          {lv_transient:>10}");
}
