//! Dump a node by its NodeId. Used to look up specific node ids
//! reported by the runtime CALCITE_WASM_CMP diagnostic.
use std::env;
use std::fs;

use calcite_core::dag::{build_dag, Dag, DagNode, NodeId};
use calcite_core::parser::parse_css;

fn main() {
    let path = env::args().nth(1).expect("css path");
    let node_id: NodeId = env::args().nth(2).and_then(|s| s.parse().ok()).expect("node_id");
    let max_depth: usize = env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(8);
    let css = fs::read_to_string(&path).expect("read");
    let parsed = parse_css(&css).expect("parse");
    let dag = build_dag(&parsed);
    if (node_id as usize) >= dag.nodes.len() {
        println!("node {node_id} out of range ({} nodes)", dag.nodes.len());
        return;
    }
    print_node(&dag, node_id, 0, max_depth);
}

fn print_node(dag: &Dag, node_id: NodeId, depth: usize, max_depth: usize) {
    let pad = "  ".repeat(depth);
    if depth > max_depth {
        println!("{pad}node {node_id}: [truncated]");
        return;
    }
    let n = &dag.nodes[node_id as usize];
    match n {
        DagNode::Lit(v) => println!("{pad}node {node_id}: Lit({v})"),
        DagNode::LitStr(s) => println!("{pad}node {node_id}: LitStr({s:?})"),
        DagNode::LoadVar { slot, kind } => {
            println!("{pad}node {node_id}: LoadVar slot={slot} kind={kind:?}")
        }
        DagNode::Param(i) => println!("{pad}node {node_id}: Param({i})"),
        DagNode::Calc { op, args } => {
            println!("{pad}node {node_id}: Calc {op:?} ({} args)", args.len());
            for &a in args {
                print_node(dag, a, depth + 1, max_depth);
            }
        }
        DagNode::BitField { src, shift, width } => {
            println!("{pad}node {node_id}: BitField shift={shift} width={width:?}");
            print_node(dag, *src, depth + 1, max_depth);
        }
        DagNode::BitwiseOp { kind, args } => {
            println!("{pad}node {node_id}: BitwiseOp {kind:?} ({} args)", args.len());
            for &a in args {
                print_node(dag, a, depth + 1, max_depth);
            }
        }
        DagNode::Call { fn_id, args } => {
            println!("{pad}node {node_id}: Call fn_id={fn_id} ({} args)", args.len());
            for &a in args {
                print_node(dag, a, depth + 1, max_depth);
            }
        }
        DagNode::WriteVar { slot, value } => {
            println!("{pad}node {node_id}: WriteVar slot={slot}");
            print_node(dag, *value, depth + 1, max_depth);
        }
        DagNode::Switch { key, table, fallback } => {
            println!(
                "{pad}node {node_id}: Switch ({} entries, fallback node {fallback})",
                table.len()
            );
            println!("{pad}  key:");
            print_node(dag, *key, depth + 2, max_depth);
            let mut sorted: Vec<_> = table.iter().collect();
            sorted.sort_by_key(|x| x.0);
            for (k, v) in sorted.iter().take(8) {
                println!("{pad}  case {k} -> node {v}");
                print_node(dag, **v, depth + 2, max_depth);
            }
            if sorted.len() > 8 {
                println!("{pad}  ... {} more cases ...", sorted.len() - 8);
            }
            println!("{pad}  fallback (node {fallback}):");
            print_node(dag, *fallback, depth + 2, max_depth);
        }
        DagNode::If { branches, fallback } => {
            println!(
                "{pad}node {node_id}: If ({} branches, fallback node {fallback})",
                branches.len()
            );
            for (i, (_cond, branch)) in branches.iter().enumerate().take(8) {
                println!("{pad}  branch[{i}] -> node {branch}:");
                print_node(dag, *branch, depth + 2, max_depth);
            }
            if branches.len() > 8 {
                println!("{pad}  ... {} more branches ...", branches.len() - 8);
            }
            println!("{pad}  fallback (node {fallback}):");
            print_node(dag, *fallback, depth + 2, max_depth);
        }
        other => println!("{pad}node {node_id}: {other:?}"),
    }
}
