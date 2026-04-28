//! End-to-end fusion-composition probe. For a candidate body of
//! opcode bytes, simulate every dispatch table's entry for each byte,
//! threading the slot environment forward so the composed expression
//! at the end of the body represents "what would running the 21-byte
//! body do, expressed as a function of the slot state at body entry?"
//!
//! This is one rep. The next layer (16-rep replication) is just
//! repeating the same composition with the entry-state being the exit-
//! state of the previous rep.
//!
//! What the probe answers:
//!   - For how many slots can we produce a fully-composed SymExpr
//!     after running all 21 entries?
//!   - For how many slots does the composition bail (and where)?
//!
//! That's the "is full body fusion viable?" measurement.

use std::collections::HashMap;
use std::path::PathBuf;

use calcite_core::Evaluator;
use calcite_core::compile::{Op, Slot};
use calcite_core::pattern::fusion_sim::{
    simulate_ops_full, Assumptions, BailReason, SlotEnv, SymExpr,
};
#[allow(unused_imports)]
use std::io::Write;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "probe-fusion-compose")]
struct Args {
    #[arg(short, long)]
    input: Option<PathBuf>,

    /// Body bytes to compose, comma-separated hex. Default: doom column-drawer.
    #[arg(long)]
    body: Option<String>,
}

fn parse_body(s: &str) -> Vec<u8> {
    s.split(',')
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(|t| u8::from_str_radix(t.trim_start_matches("0x"), 16).expect("hex"))
        .collect()
}

fn doom_body() -> Vec<u8> {
    vec![
        0x88, 0xF0, 0xD0, 0xE8, 0x89, 0xF3, 0xD7, 0x89, 0xCB, 0x36, 0xD7, 0x88, 0xC4, 0xAB, 0xAB,
        0x81, 0xC7, 0xEC, 0x00, 0x01, 0xEA,
    ]
}

#[derive(Default)]
struct PerSlotStats {
    /// How many entry-firings touched this slot.
    touched: usize,
    /// True if at any point the slot ended up with a SymExpr we can describe.
    has_composed_expr: bool,
    /// The final expression after the body ran (if composition succeeded).
    final_expr: Option<SymExpr>,
}

fn main() {
    let args = Args::parse();
    let css_path = args.input.unwrap_or_else(|| {
        PathBuf::from("C:/Users/AdmT9N0CX01V65438A/Documents/src/CSS-DOS/tmp/doom8088.css")
    });
    let body = match args.body.as_deref() {
        Some(s) => parse_body(s),
        None => doom_body(),
    };

    eprintln!("probe-fusion-compose");
    eprintln!("  input: {}", css_path.display());
    eprintln!("  body: {} bytes ({})", body.len(),
        body.iter().map(|b| format!("0x{:02x}", b)).collect::<Vec<_>>().join(","));

    let css = std::fs::read_to_string(&css_path).expect("read");
    let parsed = calcite_core::parser::parse_css(&css).expect("parse");
    let evaluator = Evaluator::from_parsed(&parsed);
    let program = evaluator.compiled();
    eprintln!("  {} dispatch tables, {} chain tables",
        program.dispatch_tables.len(), program.chain_tables.len());

    // Heuristic assumptions: --df=0 (CLD active in doom inner loop), and any
    // segment-override flag = 0 (no override active).
    let assumptions = Assumptions::new()
        .with(0x500, 0)  // discovered from probe-fusion-sim
        ;

    // For each dispatch table, simulate the 21-byte body's entries in order,
    // threading env forward. Track per-slot final state.
    println!();
    println!("== Per-table body composition (21-byte body) ==");
    println!("    {:>4}  {:>8}  {:>10}  {:>10}  {}",
        "tbl", "entries", "byte_ok", "slots", "result");

    let mut total_tables = 0;
    let mut tables_full_compose = 0;
    let mut tables_partial = 0;
    let mut tables_bail = 0;

    for (tbl_id, table) in program.dispatch_tables.iter().enumerate() {
        // Skip tables that don't have entries for any of the body bytes
        // (probably not a per-opcode dispatch).
        let has_any = body.iter().any(|&b| table.entries.contains_key(&(b as i64)));
        if !has_any {
            continue;
        }
        total_tables += 1;

        let mut env: SlotEnv = HashMap::new();
        let mut bytes_processed = 0;
        let mut bail: Option<(usize, u8, BailReason)> = None;
        let mut result_slot_seen: Option<Slot> = None;

        for (i, &b) in body.iter().enumerate() {
            let key = b as i64;
            if let Some((ops, result_slot)) = table.entries.get(&key) {
                result_slot_seen = Some(*result_slot);
                match simulate_ops_full(&mut env, ops, &assumptions, &program.flat_dispatch_arrays) {
                    Ok(()) => {
                        bytes_processed += 1;
                    }
                    Err(e) => {
                        bail = Some((i, b, e));
                        break;
                    }
                }
            } else {
                // No entry for this byte in this table. That's normal; the
                // table covers a subset of opcodes. Treat as a no-op for
                // composition purposes.
                bytes_processed += 1;
            }
        }

        let outcome = if bail.is_none() {
            tables_full_compose += 1;
            "FULL".to_string()
        } else if bytes_processed > 0 {
            tables_partial += 1;
            format!("partial @ byte {}/{} (0x{:02x}, {:?})",
                bail.as_ref().unwrap().0,
                body.len(),
                bail.as_ref().unwrap().1,
                bail.as_ref().unwrap().2)
        } else {
            tables_bail += 1;
            format!("BAIL @ byte 0 (0x{:02x}, {:?})",
                bail.as_ref().unwrap().1,
                bail.as_ref().unwrap().2)
        };

        // Brief: just show the final result_slot expression if FULL.
        let result_str = if bail.is_none() && result_slot_seen.is_some() {
            let slot = result_slot_seen.unwrap();
            let expr = env.get(&slot).cloned().unwrap_or(SymExpr::Slot(slot));
            let s = format!("{:?}", expr);
            if s.len() > 80 { format!("{}...", &s[..80]) } else { s }
        } else {
            String::new()
        };

        println!(
            "    {:>4}  {:>8}  {:>10}  {:>10}  {} {}",
            tbl_id, table.entries.len(), bytes_processed, env.len(), outcome, result_str
        );
    }

    println!();
    println!("== Summary ==");
    println!("  tables touched: {}", total_tables);
    println!("  full compose:   {} ({:.1}%)",
        tables_full_compose,
        100.0 * (tables_full_compose as f64) / (total_tables.max(1) as f64));
    println!("  partial:        {}", tables_partial);
    println!("  bail at start:  {}", tables_bail);
    println!();
    println!("Headline question: if we wanted to fuse this body across all");
    println!("dispatch tables that fire for it, {}/{} of those tables would",
        tables_full_compose, total_tables);
    println!("yield a clean composed expression. The {} partial tables would",
        tables_partial);
    println!("need extension: which Op variants did they bail on?");
}
