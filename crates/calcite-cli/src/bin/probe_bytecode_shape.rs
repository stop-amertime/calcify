//! Diagnostic probe: scan compiled bytecode for near-miss memory-fill
//! patterns. Prints windows where StoreMem is followed by an AddLit on
//! the same slot (the "start" of a fill loop) to help understand why
//! the exact 5-op window matcher isn't firing on splash-fill.

use std::path::PathBuf;

use calcite_core::Evaluator;
use calcite_core::compile::Op;

fn hunt(name: &str, ops: &[Op], limit: usize) -> usize {
    let mut hits = 0;
    for i in 0..ops.len().saturating_sub(1) {
        if let Op::StoreMem { addr_slot: d1, src: v } = &ops[i] {
            if let Op::AddLit { dst, a, val } = &ops[i + 1] {
                if *dst == *d1 && *a == *d1 {
                    if hits >= limit { return hits; }
                    hits += 1;
                    eprintln!(
                        "  {}[pc {}]: StoreMem(dst={}, src={}) then AddLit(+{}) on same slot",
                        name, i, d1, v, val
                    );
                    // Print next 4 ops for context.
                    for j in 2..6.min(ops.len() - i) {
                        eprintln!("    +{}: {:?}", j, ops[i + j]);
                    }
                    // Print previous 2 ops for context.
                    let back_start = i.saturating_sub(2);
                    for j in back_start..i {
                        eprintln!("    -{}: {:?}", i - j, ops[j]);
                    }
                }
            }
        }
    }
    hits
}

fn main() {
    let css_path = PathBuf::from(
        "C:/Users/AdmT9N0CX01V65438A/Documents/src/calcite/output/bootle-ctest.css",
    );
    eprintln!("Parsing {}...", css_path.display());
    let css = std::fs::read_to_string(&css_path).expect("read");
    let parsed = calcite_core::parser::parse_css(&css).expect("parse");
    eprintln!(
        "  {} assignments, {} properties, {} functions",
        parsed.assignments.len(),
        parsed.properties.len(),
        parsed.functions.len()
    );
    eprintln!("Building evaluator (runs compile)...");
    let evaluator = Evaluator::from_parsed(&parsed);
    let program = evaluator.compiled();
    eprintln!(
        "  main ops: {}, dispatch tables: {}, broadcast writes: {}, chain tables: {}",
        program.ops.len(),
        program.dispatch_tables.len(),
        program.broadcast_writes.len(),
        program.chain_tables.len(),
    );

    eprintln!();
    eprintln!("== main ops ==");
    let hits = hunt("main", &program.ops, 5);
    eprintln!("  main: {} StoreMem+AddLit pairs found", hits);

    eprintln!();
    eprintln!("== dispatch table entries ==");
    let mut total_de_hits = 0;
    for (ti, table) in program.dispatch_tables.iter().enumerate() {
        let mut table_hits = 0;
        for (k, (entry_ops, _s)) in &table.entries {
            let n = hunt(
                &format!("table{}_key{}", ti, k),
                entry_ops,
                3,
            );
            table_hits += n;
            if total_de_hits + table_hits > 20 {
                break;
            }
        }
        total_de_hits += table_hits;
        if total_de_hits > 20 {
            eprintln!("  ... (stopping after 20 hits across tables)");
            break;
        }
    }
    eprintln!("  total dispatch-entry hits printed: {}", total_de_hits);

    eprintln!();
    eprintln!("== broadcast write value_ops ==");
    let mut bw_hits = 0;
    for (bi, bw) in program.broadcast_writes.iter().enumerate() {
        let n = hunt(&format!("bw{}_value", bi), &bw.value_ops, 2);
        bw_hits += n;
        if bw_hits > 10 {
            break;
        }
    }
    eprintln!("  total broadcast-write hits printed: {}", bw_hits);

    eprintln!();
    eprintln!("== op-kind histogram (main only) ==");
    let mut counts: std::collections::HashMap<&'static str, usize> = Default::default();
    for op in &program.ops {
        let name = op_name(op);
        *counts.entry(name).or_insert(0) += 1;
    }
    let mut sorted: Vec<_> = counts.iter().collect();
    sorted.sort_by_key(|(_, v)| std::cmp::Reverse(**v));
    for (k, v) in sorted.iter().take(15) {
        eprintln!("  {:32} {}", k, v);
    }

    eprintln!();
    eprintln!("done.");
}

fn op_name(op: &Op) -> &'static str {
    match op {
        Op::LoadLit { .. } => "LoadLit",
        Op::LoadSlot { .. } => "LoadSlot",
        Op::LoadState { .. } => "LoadState",
        Op::LoadMem { .. } => "LoadMem",
        Op::LoadMem16 { .. } => "LoadMem16",
        Op::LoadPackedByte { .. } => "LoadPackedByte",
        Op::Add { .. } => "Add",
        Op::AddLit { .. } => "AddLit",
        Op::Sub { .. } => "Sub",
        Op::SubLit { .. } => "SubLit",
        Op::Mul { .. } => "Mul",
        Op::MulLit { .. } => "MulLit",
        Op::Div { .. } => "Div",
        Op::Mod { .. } => "Mod",
        Op::ModLit { .. } => "ModLit",
        Op::Neg { .. } => "Neg",
        Op::Abs { .. } => "Abs",
        Op::Sign { .. } => "Sign",
        Op::Pow { .. } => "Pow",
        Op::Min { .. } => "Min",
        Op::Max { .. } => "Max",
        Op::Clamp { .. } => "Clamp",
        Op::Round { .. } => "Round",
        Op::Floor { .. } => "Floor",
        Op::And { .. } => "And",
        Op::AndLit { .. } => "AndLit",
        Op::Shr { .. } => "Shr",
        Op::ShrLit { .. } => "ShrLit",
        Op::Shl { .. } => "Shl",
        Op::ShlLit { .. } => "ShlLit",
        Op::Bit { .. } => "Bit",
        Op::BitAnd16 { .. } => "BitAnd16",
        Op::BitOr16 { .. } => "BitOr16",
        Op::BitXor16 { .. } => "BitXor16",
        Op::BitNot16 { .. } => "BitNot16",
        Op::CmpEq { .. } => "CmpEq",
        Op::BranchIfZero { .. } => "BranchIfZero",
        Op::BranchIfNotEqLit { .. } => "BranchIfNotEqLit",
        Op::LoadStateAndBranchIfNotEqLit { .. } => "LoadStateAndBranchIfNotEqLit",
        Op::Jump { .. } => "Jump",
        Op::DispatchChain { .. } => "DispatchChain",
        Op::Dispatch { .. } => "Dispatch",
        Op::DispatchFlatArray { .. } => "DispatchFlatArray",
        Op::StoreState { .. } => "StoreState",
        Op::StoreMem { .. } => "StoreMem",
        Op::MemoryFill { .. } => "MemoryFill",
        Op::MemoryCopy { .. } => "MemoryCopy",
        Op::Call { .. } => "Call",
        Op::ReplicatedBody { .. } => "ReplicatedBody",
    }
}
