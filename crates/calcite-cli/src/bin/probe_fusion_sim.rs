//! Drive the fusion_sim symbolic interpreter against real dispatch
//! entries from a cabinet, and compare its composed-then-concretised
//! output against running the same entries through the existing
//! interpreter. The "is fusion viable on this cabinet?" sanity test.
//!
//! Algorithm:
//!   1. Parse cabinet, get CompiledProgram.
//!   2. Pick a dispatch table (the largest one is usually the per-register
//!      opcode dispatch we want).
//!   3. For each entry whose opcode-key falls within a candidate byte
//!      sequence (default: doom column-drawer 21-byte body), simulate
//!      symbolically and concretely.
//!   4. Report bail-rate: how many entries the simulator successfully
//!      composed vs how many bailed out.
//!
//! This isn't a fusion implementation — it's just signal: if the
//! simulator bails on most dispatch entries, fusion can't work; if it
//! gets through them, the next step (composing across entries, plumbing
//! into CSS) is worth pursuing.

use std::collections::HashMap;
use std::path::PathBuf;

use calcite_core::Evaluator;
use calcite_core::compile::{CompiledProgram, Op, Slot};
use calcite_core::pattern::fusion_sim::{
    simulate_ops, simulate_ops_with, Assumptions, BailReason, SlotEnv,
};
use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "probe-fusion-sim")]
struct Args {
    #[arg(short, long)]
    input: Option<PathBuf>,

    /// Comma-separated list of byte values to probe as dispatch keys.
    /// Default = doom column-drawer body bytes.
    #[arg(long)]
    keys: Option<String>,
}

fn parse_key_list(s: &str) -> Vec<i64> {
    s.split(',')
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(|t| {
            if let Some(hex) = t.strip_prefix("0x") {
                i64::from_str_radix(hex, 16).expect("hex key")
            } else {
                t.parse::<i64>().expect("decimal key")
            }
        })
        .collect()
}

/// The unique byte values (deduplicated) in the doom column-drawer body:
///   88 F0 D0 E8 89 F3 D7 89 CB 36 D7 88 C4 AB AB 81 C7 EC 00 01 EA
/// Many opcodes in there are repeated — we just probe each unique one.
fn default_keys() -> Vec<i64> {
    let body: [u8; 21] = [
        0x88, 0xF0, 0xD0, 0xE8, 0x89, 0xF3, 0xD7, 0x89, 0xCB, 0x36, 0xD7, 0x88, 0xC4, 0xAB, 0xAB,
        0x81, 0xC7, 0xEC, 0x00, 0x01, 0xEA,
    ];
    let mut seen: HashMap<u8, ()> = HashMap::new();
    let mut out: Vec<i64> = Vec::new();
    for &b in &body {
        if seen.insert(b, ()).is_none() {
            out.push(b as i64);
        }
    }
    out
}

#[derive(Debug, Default)]
struct TableStats {
    total_entries: usize,
    probed: usize,
    composed_ok: usize,
    bail_branch: usize,
    bail_side_effect: usize,
    bail_unsupported: usize,
    /// (key, ops.len(), reason)
    bail_examples: Vec<(i64, usize, String)>,
}

fn probe_table(
    table_id: usize,
    program: &CompiledProgram,
    keys: &[i64],
    assumptions: &Assumptions,
) -> TableStats {
    let table = &program.dispatch_tables[table_id];
    let mut stats = TableStats::default();
    stats.total_entries = table.entries.len();

    for &key in keys {
        if let Some((ops, _result_slot)) = table.entries.get(&key) {
            stats.probed += 1;
            let mut env: SlotEnv = HashMap::new();
            match simulate_ops_with(&mut env, ops, assumptions) {
                Ok(()) => {
                    stats.composed_ok += 1;
                }
                Err(BailReason::Branch(s)) => {
                    stats.bail_branch += 1;
                    if stats.bail_examples.len() < 5 {
                        stats
                            .bail_examples
                            .push((key, ops.len(), format!("Branch({})", s)));
                    }
                }
                Err(BailReason::SideEffect(s)) => {
                    stats.bail_side_effect += 1;
                    if stats.bail_examples.len() < 5 {
                        stats
                            .bail_examples
                            .push((key, ops.len(), format!("SideEffect({})", s)));
                    }
                }
                Err(BailReason::Unsupported(s)) => {
                    stats.bail_unsupported += 1;
                    if stats.bail_examples.len() < 5 {
                        stats
                            .bail_examples
                            .push((key, ops.len(), format!("Unsupported({})", s)));
                    }
                }
            }
        }
    }
    stats
}

fn op_distribution_in_entry(ops: &[Op]) -> HashMap<&'static str, usize> {
    let mut out: HashMap<&'static str, usize> = HashMap::new();
    for op in ops {
        let name = op_name(op);
        *out.entry(name).or_insert(0) += 1;
    }
    out
}

fn op_name(op: &Op) -> &'static str {
    use Op::*;
    match op {
        LoadLit { .. } => "LoadLit",
        LoadSlot { .. } => "LoadSlot",
        LoadState { .. } => "LoadState",
        LoadMem { .. } => "LoadMem",
        LoadMem16 { .. } => "LoadMem16",
        LoadPackedByte { .. } => "LoadPackedByte",
        Add { .. } => "Add",
        AddLit { .. } => "AddLit",
        Sub { .. } => "Sub",
        SubLit { .. } => "SubLit",
        Mul { .. } => "Mul",
        MulLit { .. } => "MulLit",
        Div { .. } => "Div",
        Mod { .. } => "Mod",
        ModLit { .. } => "ModLit",
        Neg { .. } => "Neg",
        Abs { .. } => "Abs",
        Sign { .. } => "Sign",
        Pow { .. } => "Pow",
        Min { .. } => "Min",
        Max { .. } => "Max",
        Clamp { .. } => "Clamp",
        Round { .. } => "Round",
        Floor { .. } => "Floor",
        And { .. } => "And",
        AndLit { .. } => "AndLit",
        Shr { .. } => "Shr",
        ShrLit { .. } => "ShrLit",
        Shl { .. } => "Shl",
        ShlLit { .. } => "ShlLit",
        Bit { .. } => "Bit",
        BitAnd16 { .. } => "BitAnd16",
        BitOr16 { .. } => "BitOr16",
        BitXor16 { .. } => "BitXor16",
        BitNot16 { .. } => "BitNot16",
        CmpEq { .. } => "CmpEq",
        BranchIfZero { .. } => "BranchIfZero",
        BranchIfNotEqLit { .. } => "BranchIfNotEqLit",
        LoadStateAndBranchIfNotEqLit { .. } => "LoadStateAndBranchIfNotEqLit",
        BranchIfNotEqLit2 { .. } => "BranchIfNotEqLit2",
        Jump { .. } => "Jump",
        DispatchChain { .. } => "DispatchChain",
        Dispatch { .. } => "Dispatch",
        DispatchFlatArray { .. } => "DispatchFlatArray",
        Call { .. } => "Call",
        StoreState { .. } => "StoreState",
        StoreMem { .. } => "StoreMem",
        MemoryFill { .. } => "MemoryFill",
        MemoryCopy { .. } => "MemoryCopy",
        ReplicatedBody { .. } => "ReplicatedBody",
    }
}

fn main() {
    let args = Args::parse();
    let css_path = args.input.unwrap_or_else(|| {
        PathBuf::from("C:/Users/AdmT9N0CX01V65438A/Documents/src/CSS-DOS/tmp/doom8088.css")
    });
    let keys = match args.keys.as_deref() {
        Some(s) => parse_key_list(s),
        None => default_keys(),
    };

    eprintln!("probe-fusion-sim");
    eprintln!("  input: {}", css_path.display());
    eprintln!("  keys to probe: {} ({})", keys.len(), keys
        .iter()
        .map(|k| format!("0x{:x}", k))
        .collect::<Vec<_>>()
        .join(","));

    let css = std::fs::read_to_string(&css_path).expect("read");
    let parsed = calcite_core::parser::parse_css(&css).expect("parse");
    let evaluator = Evaluator::from_parsed(&parsed);
    let program = evaluator.compiled();
    eprintln!(
        "  parsed: {} dispatch tables, {} chain tables, {} flat-array dispatches",
        program.dispatch_tables.len(),
        program.chain_tables.len(),
        program.flat_dispatch_arrays.len()
    );

    // First sweep without assumptions; find which state-var addresses appear
    // in LoadState / LoadStateAndBranchIfNotEqLit inside bailing entries.
    let mut state_addr_freq: HashMap<i32, usize> = HashMap::new();
    for table in &program.dispatch_tables {
        for &key in &keys {
            if let Some((ops, _)) = table.entries.get(&key) {
                let mut env: SlotEnv = HashMap::new();
                if simulate_ops(&mut env, ops).is_err() {
                    for op in ops {
                        match op {
                            Op::LoadState { addr, .. }
                            | Op::LoadStateAndBranchIfNotEqLit { addr, .. } => {
                                *state_addr_freq.entry(*addr).or_insert(0) += 1;
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }
    let mut sorted_state: Vec<(i32, usize)> =
        state_addr_freq.into_iter().collect();
    sorted_state.sort_by(|a, b| b.1.cmp(&a.1));
    println!();
    println!("== Top state-var addrs in bailing entries (LoadState / LoadStateAndBranch) ==");
    for (addr, count) in sorted_state.iter().take(15) {
        println!("    addr=0x{:x} ({}) — appears {} times", addr, addr, count);
    }

    // Build assumptions from the top state vars assuming "==0" — that's
    // the typical "flag clear / no override active" baseline. If a state
    // var is supposed to be non-zero in the fusion context, the caller
    // must override before the fused expression fires.
    let mut assumptions = Assumptions::new();
    for (addr, _) in sorted_state.iter().take(10) {
        assumptions = assumptions.with(*addr, 0);
    }
    println!();
    println!("Re-running with assumptions: top {} state vars assumed = 0", sorted_state.iter().take(10).count());

    // Probe every dispatch table and report.
    println!("== Per-dispatch-table probe ==");
    let mut overall_probed = 0;
    let mut overall_ok = 0;
    let mut overall_bailed = 0;
    let mut bail_op_counts: HashMap<&'static str, usize> = HashMap::new();

    for (i, table) in program.dispatch_tables.iter().enumerate() {
        let stats = probe_table(i, program, &keys, &assumptions);
        if stats.probed == 0 {
            continue;
        }
        println!(
            "  table {} ({} entries): probed {} / composed_ok {} / bail (branch {}, side-effect {}, unsupported {})",
            i,
            stats.total_entries,
            stats.probed,
            stats.composed_ok,
            stats.bail_branch,
            stats.bail_side_effect,
            stats.bail_unsupported,
        );
        for (key, len, reason) in &stats.bail_examples {
            println!(
                "      bail: key=0x{:x} ops.len={} reason={}",
                key, len, reason
            );
        }
        overall_probed += stats.probed;
        overall_ok += stats.composed_ok;
        overall_bailed += stats.bail_branch + stats.bail_side_effect + stats.bail_unsupported;

        // Op-distribution stats for entries that bailed: tells us which Op
        // variants we'd need to add support for to push the bail rate down.
        for &key in &keys {
            if let Some((ops, _)) = table.entries.get(&key) {
                let mut env: SlotEnv = HashMap::new();
                if simulate_ops_with(&mut env, ops, &assumptions).is_err() {
                    for op in ops {
                        *bail_op_counts.entry(op_name(op)).or_insert(0) += 1;
                    }
                }
            }
        }
    }

    println!();
    println!(
        "== Overall ==
  probed entries: {} (across {} tables)
  composed_ok:    {} ({:.1}%)
  bailed:         {} ({:.1}%)",
        overall_probed,
        program.dispatch_tables.len(),
        overall_ok,
        100.0 * (overall_ok as f64) / (overall_probed.max(1) as f64),
        overall_bailed,
        100.0 * (overall_bailed as f64) / (overall_probed.max(1) as f64),
    );

    if !bail_op_counts.is_empty() {
        let mut sorted: Vec<(&&'static str, &usize)> = bail_op_counts.iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(a.1));
        println!();
        println!("== Op variants in entries that bailed (top 15) ==");
        println!("    these are the variants we'd need to add to push bail-rate down:");
        for (op_name, count) in sorted.iter().take(15) {
            println!("    {:<32} {}", op_name, count);
        }
    }

    // Deep dive into top-N tables that have ≥100 entries (likely per-register
    // dispatches keyed on opcode). For each, dump composed expressions for
    // every doom-body byte.
    println!();
    println!("== Deep dive: tables with >=100 entries ==");
    for (i, table) in program.dispatch_tables.iter().enumerate() {
        if table.entries.len() < 100 {
            continue;
        }
        println!("--- table {} ({} entries) ---", i, table.entries.len());
        for &key in &keys {
            if let Some((ops, result_slot)) = table.entries.get(&key) {
                let mut env: SlotEnv = HashMap::new();
                match simulate_ops_with(&mut env, ops, &assumptions) {
                    Ok(()) => {
                        let composed = env
                            .get(result_slot)
                            .map(|e| format!("{:?}", e))
                            .unwrap_or_else(|| "<result_slot untouched>".to_string());
                        println!(
                            "  key=0x{:x} ({} ops, result_slot={}): {}",
                            key,
                            ops.len(),
                            result_slot,
                            if composed.len() > 240 {
                                format!("{}... ({} chars)", &composed[..240], composed.len())
                            } else {
                                composed
                            }
                        );
                    }
                    Err(e) => println!("  key=0x{:x} ({} ops): BAIL {:?}", key, ops.len(), e),
                }
            }
        }
    }
}
