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
    simulate_ops_full_ext, Assumptions, BailReason, SlotEnv, SymExpr,
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

/// Per-body-byte decoded slot pins. For each byte index in the body,
/// describes the decoder state at the moment that byte's dispatch entry
/// fires (i.e. what --opcode/--prefixLen/--mod/--reg/--rm/--q0/--q1/
/// --raw0..3 are set to before the entry runs). Only the index corresponding
/// to an *opcode-byte* gets a pin — immediate bytes (e.g. 0xF0 of MOV
/// 0x88 0xF0, or the 0xEC 0x00 of ADD imm16) are consumed by their owning
/// instruction's modrm/imm fetch logic and don't fire opcode dispatch.
///
/// `prefix_active` carries any segment-override prefix in flight (e.g.
/// after byte 9 = 0x36, byte 10 dispatches with --es-prefix-active=0,
/// --ss-prefix-active=1, etc. — set via Assumptions::with()).
#[derive(Default, Clone, Debug)]
struct BytePin {
    /// True if a dispatch entry actually fires on this body index.
    /// (Most bytes are opcode bytes; a few are immediates, no fire.)
    fires: bool,
    /// Decoded values at fire time.
    prefix_len: Option<i64>,
    opcode: Option<i64>,
    q0: Option<i64>,
    q1: Option<i64>,
    raw0: Option<i64>,
    raw1: Option<i64>,
    raw2: Option<i64>,
    raw3: Option<i64>,
    mod_: Option<i64>,
    reg: Option<i64>,
    rm: Option<i64>,
}

impl BytePin {
    fn opcode_only(opcode: i64) -> Self {
        BytePin {
            fires: true,
            prefix_len: Some(0),
            opcode: Some(opcode),
            q0: Some(opcode),
            ..Default::default()
        }
    }
    fn modrm(opcode: i64, modrm: i64) -> Self {
        let mod_ = (modrm >> 6) & 0x3;
        let reg = (modrm >> 3) & 0x7;
        let rm = modrm & 0x7;
        BytePin {
            fires: true,
            prefix_len: Some(0),
            opcode: Some(opcode),
            q0: Some(opcode),
            q1: Some(modrm),
            mod_: Some(mod_),
            reg: Some(reg),
            rm: Some(rm),
            ..Default::default()
        }
    }
    fn no_fire() -> Self { BytePin { fires: false, ..Default::default() } }
}

/// Build the per-byte pin table for the doom column-drawer body.
/// Body: 88 F0 D0 E8 89 F3 D7 89 CB 36 D7 88 C4 AB AB 81 C7 EC 00 01 EA
fn doom_body_pins() -> Vec<BytePin> {
    vec![
        // [0] 0x88 MOV r/m8,r8 ; modrm=0xF0 (mod=3 reg=6 rm=0) — DH := AL? Actually: mov al,dh
        BytePin::modrm(0x88, 0xF0),
        // [1] 0xF0 — modrm immediate, consumed by [0]
        BytePin::no_fire(),
        // [2] 0xD0 SHR/SHL r/m8,1 ; modrm=0xE8 (mod=3 reg=5 rm=0) — shr al,1
        BytePin::modrm(0xD0, 0xE8),
        // [3] 0xE8 — modrm
        BytePin::no_fire(),
        // [4] 0x89 MOV r/m16,r16 ; modrm=0xF3 (mod=3 reg=6 rm=3) — mov bx,si
        BytePin::modrm(0x89, 0xF3),
        // [5] 0xF3 — modrm
        BytePin::no_fire(),
        // [6] 0xD7 XLAT (no modrm)
        BytePin::opcode_only(0xD7),
        // [7] 0x89 MOV r/m16,r16 ; modrm=0xCB (mod=3 reg=1 rm=3) — mov bx,cx
        BytePin::modrm(0x89, 0xCB),
        // [8] 0xCB — modrm
        BytePin::no_fire(),
        // [9] 0x36 SS: prefix — after this, prefix_len becomes 1 for next opcode
        {
            let mut p = BytePin::opcode_only(0x36);
            p.prefix_len = Some(0); // 0x36 itself dispatches with prefixLen=0
            p
        },
        // [10] 0xD7 XLAT (with SS:) — prefixLen=1 (post-prefix)
        {
            let mut p = BytePin::opcode_only(0xD7);
            p.prefix_len = Some(1);
            // raw1 holds the post-prefix opcode byte; raw0 holds the prefix
            p.raw0 = Some(0x36);
            p.raw1 = Some(0xD7);
            p.q0 = Some(0xD7);
            p
        },
        // [11] 0x88 MOV r/m8,r8 ; modrm=0xC4 (mod=3 reg=0 rm=4) — mov ah,al
        BytePin::modrm(0x88, 0xC4),
        // [12] 0xC4 — modrm
        BytePin::no_fire(),
        // [13] 0xAB STOSW (no modrm)
        BytePin::opcode_only(0xAB),
        // [14] 0xAB STOSW
        BytePin::opcode_only(0xAB),
        // [15] 0x81 ADD/etc r/m16,imm16 ; modrm=0xC7 (mod=3 reg=0 rm=7) — add di,imm16
        BytePin::modrm(0x81, 0xC7),
        // [16] 0xC7 — modrm
        BytePin::no_fire(),
        // [17] 0xEC — imm16 lo
        BytePin::no_fire(),
        // [18] 0x00 — imm16 hi (so imm16 = 0x00EC = 236)
        BytePin::no_fire(),
        // [19] 0x01 — ??? Actually 81 C7 EC 00 means add di,0x00EC — imm16 is bytes 17..18.
        // Bytes 19-20 are then 0x01 0xEA = 0xEA01... but 0xEA is JMP FAR (5-byte instr).
        // Looking again: 81 C7 EC 00 = ADD DI,0xEC (sign-extended imm8? no, 81 is 16-bit imm).
        // Actually 81 C7 imm16: imm16 = 0xEC + 0x00<<8 = 0x00EC. Then byte 19 = 0x01 starts
        // the next instr. But 0x01 isn't followed by enough bytes... The decoded body is
        // probably 81 C7 EC 00 (4 bytes) + 01 EA (2 bytes? 01 is ADD, modrm=EA = mod=3 reg=5 rm=2: add dx,bp).
        // Let me redo: bytes [15..19] = 81 C7 EC 00 01 — ADD DI,0x01EC = 492 (the column-stride).
        // Bytes 17 and 18 are imm16 lo+hi; byte 19 is ??? — wait, 81 has imm16 = 2 bytes.
        // 81 (1) + C7 modrm (1) + imm16 (2) = 4 bytes. So bytes 15+16+17+18 consumed,
        // byte 19 starts next instr. Then bytes 19-20 = 01 EA: ADD DX,BP. But there
        // are only 21 bytes total — that leaves no JMP. Reread the body.
        //
        // 88 F0 / D0 E8 / 89 F3 / D7 / 89 CB / 36 D7 / 88 C4 / AB / AB / 81 C7 EC 00 / 01 / EA
        //   ^0    ^2    ^4    ^6   ^7    ^9   ^11    ^13  ^14  ^15      ^19  ^20
        // Byte 19 (0x01) starts an instr but only 1 byte follows (0xEA). 01 needs modrm,
        // so 01 EA = ADD DX,BP. That's 2 bytes. Total 19+2 = 21. ✓
        // So the trailing 0xEA in our body is a *modrm byte* of the 01 instr, not JMP FAR.
        // Body has NO JMP FAR — it's a straight-line block.
        BytePin::modrm(0x01, 0xEA), // ADD r/m16,r16 ; modrm=0xEA (mod=3 reg=5 rm=2) — add dx,bp
        // [20] 0xEA — modrm
        BytePin::no_fire(),
    ]
}

/// Apply a BytePin to the env: sets the relevant slots to Const values
/// matching the decoder state at body-byte fire time. Slot numbers are
/// looked up from `program.property_slots`.
fn apply_pin(env: &mut SlotEnv, pin: &BytePin, slot_of: &dyn Fn(&str) -> Option<Slot>) {
    let set = |env: &mut SlotEnv, name: &str, val: Option<i64>| {
        if let (Some(v), Some(s)) = (val, slot_of(name)) {
            env.insert(s, SymExpr::Const(v));
        }
    };
    set(env, "--prefixLen", pin.prefix_len);
    set(env, "--opcode", pin.opcode);
    set(env, "--q0", pin.q0);
    set(env, "--q1", pin.q1);
    set(env, "--raw0", pin.raw0);
    set(env, "--raw1", pin.raw1);
    set(env, "--raw2", pin.raw2);
    set(env, "--raw3", pin.raw3);
    set(env, "--mod", pin.mod_);
    set(env, "--reg", pin.reg);
    set(env, "--rm", pin.rm);
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

    // Diagnostic: dump the bailing tables' byte-0 op streams so we know
    // which slots they're reading before composition has set them. This
    // tells us which slots to pre-pin in the body simulation.
    if std::env::var("CALCITE_DUMP_BAIL_OPS").is_ok() {
        // Suspects: tables that bail under per-byte pinning. Dump their
        // op streams at the offending body byte.
        let suspects: &[(usize, usize)] = &[
            (22, 19), (23, 19), (24, 19), (26, 19),
            (30, 2),
        ];
        for &(tid, byte_idx) in suspects {
            if let Some(table) = program.dispatch_tables.get(tid) {
                let b = body[byte_idx];
                if let Some((ops, rs)) = table.entries.get(&(b as i64)) {
                    eprintln!("  table {} byte-{} (0x{:02x}) ops, result_slot={}:", tid, byte_idx, b, rs);
                    for (i, op) in ops.iter().take(20).enumerate() {
                        eprintln!("    [{}] {:?}", i, op);
                    }
                }
            }
        }
        // Print property_slots snapshot so we know which slot is which.
        eprintln!("  --opcode slot: {:?}", program.property_slots.get("--opcode"));
        eprintln!("  --q0 slot:     {:?}", program.property_slots.get("--q0"));
        eprintln!("  --q1 slot:     {:?}", program.property_slots.get("--q1"));
        eprintln!("  --prefixLen:   {:?}", program.property_slots.get("--prefixLen"));
        eprintln!("  --raw0:        {:?}", program.property_slots.get("--raw0"));
        eprintln!("  --raw1:        {:?}", program.property_slots.get("--raw1"));
        eprintln!("  --raw2:        {:?}", program.property_slots.get("--raw2"));
        eprintln!("  --raw3:        {:?}", program.property_slots.get("--raw3"));
        eprintln!("  --linearIP:    {:?}", program.property_slots.get("--linearIP"));
        eprintln!("  --IP slot:     {:?}", program.property_slots.get("--IP"));
        eprintln!("  --ipAddr slot: {:?}", program.property_slots.get("--ipAddr"));
        // Reverse-lookup: which property names map to slots 34, 36, 50, 51?
        let interesting: std::collections::HashSet<i32> = [21, 25, 34, 35, 36, 46, 47, 48, 49, 50, 51, 53, 69, 70, 71, 72, 73, 74, 75, 76, 77, 78, 79, 80, 81].iter().copied().collect();
        let mut hits: Vec<(&String, &Slot)> = program
            .property_slots
            .iter()
            .filter(|(_, s)| interesting.contains(&(**s as i32)))
            .collect();
        hits.sort_by_key(|(_, s)| **s);
        for (name, slot) in hits {
            eprintln!("  slot {:>4} -> {}", slot, name);
        }
    }

    // Heuristic assumptions: --df=0 (CLD active in doom inner loop), and any
    // segment-override flag = 0 (no override active).
    let assumptions = Assumptions::new()
        .with(0x500, 0)  // discovered from probe-fusion-sim
        ;

    // Build the per-byte pin table for the column-drawer body. If the user
    // passed --body, we don't have a decode, so leave pins empty (preserves
    // legacy behaviour for non-doom bodies).
    let pins: Vec<BytePin> = if args.body.is_none() {
        doom_body_pins()
    } else {
        (0..body.len()).map(|_| BytePin::no_fire()).collect()
    };
    let use_pins = std::env::var("CALCITE_PIN_DECODE").map(|v| v != "0").unwrap_or(true);

    // Build a property_slots lookup closure.
    let slot_of = |name: &str| -> Option<Slot> {
        program.property_slots.get(name).copied()
    };

    // Body-invariant slot pins: state bits that are constant for the whole
    // column-drawer body. --hasREP / --_repActive are 0 because the body
    // contains no REP prefix; segment-override flags are 0 except after
    // byte 9 (0x36 SS:) where --ss-prefix-active becomes 1 for byte 10.
    // Pinning these eliminates BranchIfNotEqLit bails inside ops that
    // gate on them (e.g. STOSW's REP path).
    let body_invariant: Vec<(&str, i64)> = vec![
        ("--hasREP", 0),
        ("--_repActive", 0),
        ("--_repContinue", 0),
        ("--_repZF", 0),
        ("--es-prefix-active", 0),
        ("--cs-prefix-active", 0),
        ("--ss-prefix-active", 0),
        ("--ds-prefix-active", 0),
        ("--fs-prefix-active", 0),
        ("--gs-prefix-active", 0),
    ];

    if use_pins {
        eprintln!("  per-byte decoder pinning: ON ({} body bytes, {} fire)",
            pins.len(), pins.iter().filter(|p| p.fires).count());
    } else {
        eprintln!("  per-byte decoder pinning: OFF");
    }

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
    let mut bail_reasons: HashMap<String, usize> = HashMap::new();

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
            // Pre-pin the decoder slots for this body byte. Only fires for
            // *opcode* bytes (not modrm/imm). Pins go into env BEFORE the
            // entry runs so internal `BranchIfNotEqLit { a: --mod, val: ... }`
            // resolves statically.
            //
            // Critically: skip simulation entirely for non-fire bytes
            // (modrm / immediates). In real execution the dispatch path
            // only fires for opcode bytes — modrm/imm bytes are consumed
            // by their owning instruction's decode logic and never key
            // dispatch. Iterating them here would simulate phantom
            // dispatches that never run.
            let fires_here = if use_pins {
                pins.get(i).map(|p| p.fires).unwrap_or(true)
            } else {
                true
            };
            if !fires_here {
                bytes_processed += 1;
                continue;
            }
            if use_pins {
                // Re-apply body-invariant pins each fire (fusion entries
                // may overwrite these slots transiently).
                for (name, val) in &body_invariant {
                    if let Some(s) = slot_of(name) {
                        env.insert(s, SymExpr::Const(*val));
                    }
                }
                if let Some(pin) = pins.get(i) {
                    apply_pin(&mut env, pin, &slot_of);
                }
            }
            let key = b as i64;
            if let Some((ops, result_slot)) = table.entries.get(&key) {
                result_slot_seen = Some(*result_slot);
                match simulate_ops_full_ext(&mut env, ops, &assumptions, &program.flat_dispatch_arrays, &program.chain_tables, &program.dispatch_tables) {
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
        if let Some((_, _, ref e)) = bail {
            *bail_reasons.entry(format!("{:?}", e)).or_insert(0) += 1;
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

    // === Lower FULL-compose tables to fused Op sequences ===
    // For each table that fully composed, emit the fused Op sequence and
    // compare its op count against the sum of the original per-byte entry ops.
    println!();
    println!("== Fused-emit (per FULL table) ==");
    println!("    {:>4}  {:>10}  {:>10}  {:>8}  {}",
        "tbl", "orig_ops", "fused_ops", "shrink", "result_expr_kind");

    let mut total_orig_ops = 0usize;
    let mut total_fused_ops = 0usize;
    let mut emit_failures = 0usize;
    // Use a high slot range for scratch to avoid colliding with real slots.
    let mut next_scratch: Slot = 1_000_000;

    for (tbl_id, table) in program.dispatch_tables.iter().enumerate() {
        let has_any = body.iter().any(|&b| table.entries.contains_key(&(b as i64)));
        if !has_any { continue; }

        // Re-simulate the body on this table to get the final SymExpr for
        // the result slot. (Quick second pass — symbolic only.)
        let mut env: SlotEnv = HashMap::new();
        let mut result_slot_seen: Option<Slot> = None;
        let mut bailed = false;
        for (i, &b) in body.iter().enumerate() {
            if let Some(pin) = pins.get(i) {
                if !pin.fires { continue; }
            }
            // Re-pin invariants and decoder state.
            for (name, val) in &body_invariant {
                if let Some(s) = slot_of(name) {
                    env.insert(s, SymExpr::Const(*val));
                }
            }
            if let Some(pin) = pins.get(i) {
                apply_pin(&mut env, pin, &slot_of);
            }
            if let Some((ops, result_slot)) = table.entries.get(&(b as i64)) {
                result_slot_seen = Some(*result_slot);
                if simulate_ops_full_ext(&mut env, ops, &assumptions, &program.flat_dispatch_arrays, &program.chain_tables, &program.dispatch_tables).is_err() {
                    bailed = true;
                    break;
                }
            }
        }
        if bailed { continue; }
        let Some(rs) = result_slot_seen else { continue; };
        let Some(expr) = env.get(&rs).cloned() else { continue; };

        // Original op count: sum of ops across firing entries for this body.
        let orig_op_count: usize = body.iter().enumerate()
            .filter_map(|(i, &b)| {
                if pins.get(i).map(|p| !p.fires).unwrap_or(false) { return None; }
                table.entries.get(&(b as i64)).map(|(ops, _)| ops.len())
            })
            .sum();

        // Lower the SymExpr to a fused Op sequence writing into the
        // result_slot directly.
        let mut fused_ops = Vec::new();
        let mut alloc = || { let s = next_scratch; next_scratch += 1; s };
        match expr.lower_to_ops(rs, &mut alloc, &mut fused_ops) {
            Ok(()) => {
                let kind = match &expr {
                    SymExpr::Const(_) => "Const",
                    SymExpr::Slot(_) => "Slot (pure passthrough)",
                    SymExpr::Add(_, _) => "Add",
                    SymExpr::Sub(_, _) => "Sub",
                    SymExpr::Mul(_, _) => "Mul",
                    SymExpr::Shl(_, _) => "Shl",
                    SymExpr::Shr(_, _) => "Shr",
                    SymExpr::LowerBytes(_, _) => "LowerBytes",
                    SymExpr::Mod(_, _) => "Mod",
                    SymExpr::Div(_, _) => "Div",
                    SymExpr::BitAnd16(_, _) => "BitAnd16",
                    SymExpr::BitOr16(_, _) => "BitOr16",
                    SymExpr::BitXor16(_, _) => "BitXor16",
                    SymExpr::BitNot16(_) => "BitNot16",
                    SymExpr::Neg(_) | SymExpr::Floor(_) | SymExpr::Abs(_) | SymExpr::Sign(_) => "Unary",
                    SymExpr::BitExtract(_, _) => "BitExtract",
                    SymExpr::Min(_) => "Min",
                    SymExpr::Max(_) => "Max",
                    _ => "Other",
                };
                let shrink = if orig_op_count > 0 {
                    100.0 * (1.0 - (fused_ops.len() as f64) / (orig_op_count as f64))
                } else { 0.0 };
                println!("    {:>4}  {:>10}  {:>10}  {:>7.1}%  {}",
                    tbl_id, orig_op_count, fused_ops.len(), shrink, kind);
                total_orig_ops += orig_op_count;
                total_fused_ops += fused_ops.len();
            }
            Err(why) => {
                emit_failures += 1;
                println!("    {:>4}  {:>10}  {:>10}  --       lower-fail: {}",
                    tbl_id, orig_op_count, "?", why);
            }
        }
    }

    println!();
    println!("== Fused-emit summary ==");
    println!("  total original ops: {}", total_orig_ops);
    println!("  total fused ops:    {}", total_fused_ops);
    if total_orig_ops > 0 {
        println!("  shrink:             {:.1}%",
            100.0 * (1.0 - (total_fused_ops as f64) / (total_orig_ops as f64)));
    }
    if emit_failures > 0 {
        println!("  emit failures:      {} (non-pure SymExpr)", emit_failures);
    }
    if !bail_reasons.is_empty() {
        println!();
        println!("== Bail-reason histogram ==");
        let mut rows: Vec<(&String, &usize)> = bail_reasons.iter().collect();
        rows.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
        for (r, n) in rows {
            println!("  {:>3}  {}", n, r);
        }
    }
    println!();
    println!("Headline question: if we wanted to fuse this body across all");
    println!("dispatch tables that fire for it, {}/{} of those tables would",
        tables_full_compose, total_tables);
    println!("yield a clean composed expression. The {} partial tables would",
        tables_partial);
    println!("need extension: which Op variants did they bail on?");
}
