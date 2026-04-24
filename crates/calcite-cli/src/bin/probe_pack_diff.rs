//! probe-pack-diff
//!
//! Isolate where PACK_SIZE=2 cabinets diverge. Three modes:
//!
//!   --interp-vs-interp A.css B.css N
//!       Run both cabinets N ticks via the interpreter. Diff state.
//!       Use to compare pack=1 vs pack=2 *semantically* — if this
//!       diverges, the CSS itself disagrees, meaning kiln emission is
//!       wrong. If it agrees, the bug is in calcite's compiled path.
//!
//!   --interp-vs-compiled A.css N
//!       Run the same CSS N ticks through the interpreter AND the
//!       compiled path, diff state. If they disagree, there's a
//!       compiler bug.
//!
//!   --compiled-vs-compiled A.css B.css N
//!       Sanity check: run both cabinets N ticks via compiled path and
//!       diff. Equivalent to what the debugger's `diff_packed_memory`
//!       does but packaged as a one-shot.
//!
//! All modes diff a byte-level view of guest memory (0..0xA0000 by
//! default) derived appropriately for packed vs unpacked cabinets.

use std::path::PathBuf;
use calcite_core::{parser, Evaluator, State};

fn usage() -> ! {
    eprintln!("usage:");
    eprintln!("  probe-pack-diff --interp-vs-interp A.css B.css N [max_diffs]");
    eprintln!("  probe-pack-diff --interp-vs-compiled A.css N [max_diffs]");
    eprintln!("  probe-pack-diff --compiled-vs-compiled A.css B.css N [max_diffs]");
    eprintln!("  probe-pack-diff --first-divergence A.css B.css N [check_every]");
    eprintln!("     Run both (compiled) in lockstep; stop at first tick with any register/memory diff.");
    eprintln!("  probe-pack-diff --snapshot-at-tick A.css tick");
    eprintln!("     Run A.css for `tick` ticks, dump CS/IP/regs and bytes around CS:IP.");
    std::process::exit(2);
}

/// Read a byte from a State, resolving the packed-cell table if present.
fn byte_at(state: &State, addr: i32) -> u8 {
    if !state.packed_cell_table.is_empty() && state.packed_cell_size > 0 {
        let pack = state.packed_cell_size as i32;
        let cell_idx = (addr / pack) as usize;
        if cell_idx < state.packed_cell_table.len() {
            let sa = state.packed_cell_table[cell_idx];
            if sa < 0 {
                let sidx = (-sa - 1) as usize;
                if sidx < state.state_vars.len() {
                    let cell = state.state_vars[sidx];
                    let off = (addr % pack) as u32;
                    return ((cell >> (8 * off)) & 0xFF) as u8;
                }
            }
        }
    }
    // Fallback to flat shadow / state.read_mem semantics.
    (state.read_mem(addr) & 0xFF) as u8
}

/// Load a CSS cabinet, install the packed cell table if present, and
/// return an (Evaluator, State) ready to tick.
fn load(path: &std::path::Path) -> (Evaluator, State) {
    eprintln!("  parsing {}...", path.display());
    let t0 = std::time::Instant::now();
    let css = std::fs::read_to_string(path).expect("read CSS");
    let parsed = parser::parse_css(&css).expect("parse CSS");
    eprintln!(
        "    {} assignments, {} properties, {} functions in {:.2}s",
        parsed.assignments.len(),
        parsed.properties.len(),
        parsed.functions.len(),
        t0.elapsed().as_secs_f64()
    );

    eprintln!("  loading properties...");
    let t1 = std::time::Instant::now();
    let mut state = State::default();
    state.load_properties(&parsed.properties);
    eprintln!("    done in {:.2}s", t1.elapsed().as_secs_f64());

    eprintln!("  creating evaluator (compile)...");
    let t2 = std::time::Instant::now();
    let evaluator = Evaluator::from_parsed(&parsed);
    eprintln!("    done in {:.2}s", t2.elapsed().as_secs_f64());

    // Populate BIOS ROM from --readMem literals (same as wasm wrapper).
    if let Some(table) = evaluator.dispatch_tables.get("--readMem") {
        state.populate_memory_from_readmem(table);
    }
    // Install packed cell table.
    if let Some(port) = evaluator.compiled().packed_broadcast_writes.first() {
        state.packed_cell_table = port.cell_table.clone();
        state.packed_cell_size = port.pack;
        eprintln!(
            "    packed cabinet: pack={}, cell_table_len={}",
            state.packed_cell_size,
            state.packed_cell_table.len()
        );
    } else {
        eprintln!("    unpacked cabinet (no packed_broadcast_writes)");
    }
    (evaluator, state)
}

struct RunResult {
    state: State,
    ev: Evaluator,
}

fn run_interpreted(path: &std::path::Path, n: u32) -> RunResult {
    let (mut ev, mut state) = load(path);
    eprintln!("  running {} ticks via INTERPRETER...", n);
    let t0 = std::time::Instant::now();
    for i in 0..n {
        ev.tick_interpreted(&mut state);
        if i % 1000 == 0 && i > 0 {
            eprintln!("    tick {}  ({:.1}s)", i, t0.elapsed().as_secs_f64());
        }
    }
    eprintln!("    done {} ticks in {:.2}s", n, t0.elapsed().as_secs_f64());
    RunResult { state, ev }
}

fn run_compiled(path: &std::path::Path, n: u32) -> RunResult {
    let (mut ev, mut state) = load(path);
    eprintln!("  running {} ticks via COMPILED (run_batch, like browser)...", n);
    let t0 = std::time::Instant::now();
    ev.run_batch(&mut state, n);
    eprintln!("    done {} ticks in {:.2}s", n, t0.elapsed().as_secs_f64());
    RunResult { state, ev }
}

fn run_compiled_with_rep_diag(path: &std::path::Path, n: u32, label: &str) -> RunResult {
    calcite_core::compile::rep_diag_reset();
    let (mut ev, mut state) = load(path);
    eprintln!("  running {} ticks via COMPILED (run_batch, like browser)...", n);
    let t0 = std::time::Instant::now();
    ev.run_batch(&mut state, n);
    eprintln!("    done {} ticks in {:.2}s", n, t0.elapsed().as_secs_f64());
    println!("--- rep_fast_forward diagnostics ({}): ---", label);
    println!("{}", calcite_core::compile::rep_diag_report());
    RunResult { state, ev }
}

fn dump_cell(state: &State, cell: u32, label: &str) {
    let name = format!("mc{}", cell);
    let v = state.get_var(&name).unwrap_or(0);
    println!("  {} {}: {} (lo=0x{:02X} hi=0x{:02X})", label, name, v, v & 0xFF, (v >> 8) & 0xFF);
}

fn dump_slot_state(state: &State, label: &str) {
    println!("--- {} slot state ---", label);
    for i in 0..6 {
        let live = state.get_var(&format!("_slot{}Live", i)).unwrap_or(0);
        let addr = state.get_var(&format!("memAddr{}", i)).unwrap_or(0);
        let val = state.get_var(&format!("memVal{}", i)).unwrap_or(0);
        // Triple-buffer prefixes — what the NEXT tick's cell-write rule will see.
        let live1 = state.get_var(&format!("__1_slot{}Live", i)).unwrap_or(0);
        let addr1 = state.get_var(&format!("__1memAddr{}", i)).unwrap_or(0);
        let val1 = state.get_var(&format!("__1memVal{}", i)).unwrap_or(0);
        println!("  slot{}: live={} (_1={}) addr={} (_1={}) val={} (_1={})", i, live, live1, addr, addr1, val, val1);
    }
    for r in &["CS","IP","AX","BX","CX","DX","SP","BP","SI","DI","DS","ES","SS","flags","cycleCount","opcode"] {
        let v = state.get_var(r).unwrap_or(0);
        println!("  {}={}", r, v);
    }
}

fn report_register_diffs(a: &State, b: &State, label_a: &str, label_b: &str) {
    // Compare the common (non-mc) state vars by name — registers like AX,
    // IP, cycleCount etc. Skip the mc{N} cells; those are memory bytes
    // reported separately.
    let mut diffs: Vec<(String, i32, i32)> = Vec::new();
    for (i, name) in a.state_var_names.iter().enumerate() {
        if name.starts_with("mc") && name[2..].bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if name == "clock" { continue; }
        // Look up the same name in b.
        let va = a.state_vars.get(i).copied().unwrap_or(0);
        let vb = b.get_var(name).unwrap_or(0);
        if va != vb {
            diffs.push((name.clone(), va, vb));
        }
    }
    if diffs.is_empty() {
        println!("  REGISTERS: all match");
    } else {
        println!("  REGISTERS: {} differ", diffs.len());
        for (n, va, vb) in diffs.iter().take(30) {
            println!("    {}: {}={} vs {}={}", n, label_a, va, label_b, vb);
        }
        if diffs.len() > 30 {
            println!("    ... and {} more", diffs.len() - 30);
        }
    }
}

fn report_memory_diffs(a: &State, b: &State, start: i32, end: i32, max_report: usize) {
    let mut diffs: Vec<(i32, u8, u8)> = Vec::new();
    for addr in start..end {
        let ba = byte_at(a, addr);
        let bb = byte_at(b, addr);
        if ba != bb {
            diffs.push((addr, ba, bb));
            if diffs.len() > max_report + 50 { break; }
        }
    }
    if diffs.is_empty() {
        println!("  MEMORY [0x{:X},0x{:X}): all match", start, end);
    } else {
        println!("  MEMORY [0x{:X},0x{:X}): {}+ bytes differ", start, end, diffs.len());
        for (a_addr, ba, bb) in diffs.iter().take(max_report) {
            println!("    0x{:06X}: {:#04X} vs {:#04X}", a_addr, ba, bb);
        }
        if diffs.len() > max_report {
            println!("    ... reporting capped");
        }
    }
}

fn main() {
    env_logger::init();
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 { usage(); }
    match args[1].as_str() {
        "--interp-vs-interp" => {
            if args.len() < 5 { usage(); }
            let a_path = PathBuf::from(&args[2]);
            let b_path = PathBuf::from(&args[3]);
            let n: u32 = args[4].parse().expect("N must be integer");
            let max: usize = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(50);
            eprintln!("=== run A (interpreted) ===");
            let a = run_interpreted(&a_path, n);
            eprintln!("=== run B (interpreted) ===");
            let b = run_interpreted(&b_path, n);
            println!("=== DIFF: {} ticks interpreted ===", n);
            if std::env::var("DUMP").is_ok() {
                dump_slot_state(&a.state, "A");
                dump_slot_state(&b.state, "B");
                // Cells covering 1522..1527 (= addresses 0x5F2..0x5F7).
                println!("--- cells covering 0x5F2..0x5F7 ---");
                for cell in [761u32, 762, 763] {
                    dump_cell(&b.state, cell, "B");
                }
                // Any named property from the interpreter's properties map.
                if let Ok(name) = std::env::var("DUMP_PROP") {
                    let va = a.ev.get_all_interpreted_values();
                    let vb = b.ev.get_all_interpreted_values();
                    println!("--- property {} ---", name);
                    println!("  A: {:?}", va.get(&name));
                    println!("  B: {:?}", vb.get(&name));
                }
            }
            report_register_diffs(&a.state, &b.state, "A", "B");
            report_memory_diffs(&a.state, &b.state, 0, 0xA_0000, max);
        }
        "--interp-vs-compiled" => {
            if args.len() < 4 { usage(); }
            let a_path = PathBuf::from(&args[2]);
            let n: u32 = args[3].parse().expect("N must be integer");
            let max: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(50);
            eprintln!("=== run A (interpreted) ===");
            let a = run_interpreted(&a_path, n);
            eprintln!("=== run A (compiled) ===");
            let b = run_compiled(&a_path, n);
            println!("=== DIFF: same CSS, {} ticks interp vs compiled ===", n);
            report_register_diffs(&a.state, &b.state, "interp", "compiled");
            report_memory_diffs(&a.state, &b.state, 0, 0xA_0000, max);
        }
        "--compiled-vs-compiled" => {
            if args.len() < 5 { usage(); }
            let a_path = PathBuf::from(&args[2]);
            let b_path = PathBuf::from(&args[3]);
            let n: u32 = args[4].parse().expect("N must be integer");
            let max: usize = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(50);
            eprintln!("=== run A (compiled, run_batch) ===");
            let a = run_compiled_with_rep_diag(&a_path, n, "A");
            eprintln!("=== run B (compiled, run_batch) ===");
            let b = run_compiled_with_rep_diag(&b_path, n, "B");
            println!("=== DIFF: {} ticks compiled ===", n);
            println!("  A cycleCount={}, B cycleCount={}",
                a.state.get_var("cycleCount").unwrap_or(0),
                b.state.get_var("cycleCount").unwrap_or(0));
            report_register_diffs(&a.state, &b.state, "A", "B");
            report_memory_diffs(&a.state, &b.state, 0, 0xA_0000, max);
        }
        "--snapshot-region" => {
            // Usage: --snapshot-region A.css ticks addr len
            // Runs A.css for `ticks` via compiled path, then dumps
            // `len` bytes starting at linear `addr` to stdout as hex.
            if args.len() < 6 { usage(); }
            let a_path = PathBuf::from(&args[2]);
            let n: u32 = args[3].parse().expect("ticks");
            let addr: i32 = args[4].parse().expect("addr");
            let len: i32 = args[5].parse().expect("len");
            let r = run_compiled(&a_path, n);
            println!(
                "cycleCount={} IP={} CS={}",
                r.state.get_var("cycleCount").unwrap_or(0),
                r.state.get_var("IP").unwrap_or(0),
                r.state.get_var("CS").unwrap_or(0)
            );
            println!("addr=0x{:X} len={}", addr, len);
            for i in 0..len {
                let b = byte_at(&r.state, addr + i);
                print!("{:02X}", b);
                if (i + 1) % 16 == 0 { println!(); } else if (i + 1) % 8 == 0 { print!(" "); }
            }
            println!();
        }

        "--same-cycles" => {
            // Usage: --same-cycles A.css B.css target_cycles max_ticks
            // Run both until cycleCount >= target_cycles (or max_ticks), then diff.
            // Tests whether the two cabinets agree on the *same guest work done*
            // rather than on the same wall-clock tick count.
            if args.len() < 6 { usage(); }
            let a_path = PathBuf::from(&args[2]);
            let b_path = PathBuf::from(&args[3]);
            let target: i64 = args[4].parse().expect("target cycles");
            let max_ticks: u32 = args[5].parse().expect("max ticks");
            let max_report: usize = args.get(6).and_then(|s| s.parse().ok()).unwrap_or(30);

            eprintln!("Loading A...");
            let (mut ev_a, mut state_a) = load(&a_path);
            eprintln!("Loading B...");
            let (mut ev_b, mut state_b) = load(&b_path);

            eprintln!("Running A until cycles >= {}...", target);
            let mut a_ticks = 0u32;
            while (state_a.get_var("cycleCount").unwrap_or(0) as i64) < target && a_ticks < max_ticks {
                ev_a.tick(&mut state_a);
                a_ticks += 1;
            }
            let a_cycles = state_a.get_var("cycleCount").unwrap_or(0) as i64;
            eprintln!("  A: {} ticks → cycleCount={}", a_ticks, a_cycles);

            eprintln!("Running B until cycles >= {}...", target);
            let mut b_ticks = 0u32;
            while (state_b.get_var("cycleCount").unwrap_or(0) as i64) < target && b_ticks < max_ticks {
                ev_b.tick(&mut state_b);
                b_ticks += 1;
            }
            let b_cycles = state_b.get_var("cycleCount").unwrap_or(0) as i64;
            eprintln!("  B: {} ticks → cycleCount={}", b_ticks, b_cycles);

            println!("=== DIFF: both at cycleCount≈{} ===", target);
            println!("  A used {} ticks, B used {} ticks (B/A ratio={:.2})", a_ticks, b_ticks, (b_ticks as f64) / (a_ticks.max(1) as f64));
            println!("  A final cycleCount={}, B={} (delta={})", a_cycles, b_cycles, (b_cycles - a_cycles).abs());
            report_register_diffs(&state_a, &state_b, "A", "B");
            report_memory_diffs(&state_a, &state_b, 0, 0xA_0000, max_report);
        }

        "--snapshot-at-tick" => {
            // Usage: --snapshot-at-tick A.css tick
            // Runs A.css for `tick` ticks via compiled path, dumps CS/IP/regs and
            // ~32 bytes around CS:IP.
            if args.len() < 4 { usage(); }
            let a_path = PathBuf::from(&args[2]);
            let n: u32 = args[3].parse().expect("tick");
            let r = run_compiled(&a_path, n);
            let cs = r.state.get_var("CS").unwrap_or(0);
            let ip = r.state.get_var("IP").unwrap_or(0);
            let linear = ((cs as i64) * 16 + ip as i64) as i32;
            println!("tick={} cycleCount={}", n, r.state.get_var("cycleCount").unwrap_or(0));
            for reg in &["CS","IP","AX","BX","CX","DX","SP","BP","SI","DI","DS","ES","SS","flags","opcode","hasREP","repType"] {
                println!("  {}={}", reg, r.state.get_var(reg).unwrap_or(0));
            }
            println!("bytes at CS:IP (linear 0x{:X}):", linear);
            for i in -8..24 {
                let a = linear + i;
                if a >= 0 { print!("{:02X} ", byte_at(&r.state, a)); }
            }
            println!();
        }
        "--first-divergence" => {
            if args.len() < 5 { usage(); }
            let a_path = PathBuf::from(&args[2]);
            let b_path = PathBuf::from(&args[3]);
            let n: u32 = args[4].parse().expect("N must be integer");
            let check_every: u32 = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(100);

            eprintln!("Loading A...");
            let (mut ev_a, mut state_a) = load(&a_path);
            eprintln!("Loading B...");
            let (mut ev_b, mut state_b) = load(&b_path);

            // Register names to compare each checkpoint (cheap scalar diff).
            let regs: Vec<&str> = vec![
                "CS","IP","AX","BX","CX","DX","SP","BP","SI","DI",
                "DS","ES","SS","flags","cycleCount","opcode",
                "memAddr0","memAddr1","memAddr2","memAddr3","memAddr4","memAddr5",
                "memVal0","memVal1","memVal2","memVal3","memVal4","memVal5",
                "_slot0Live","_slot1Live","_slot2Live","_slot3Live","_slot4Live","_slot5Live",
            ];

            let t0 = std::time::Instant::now();
            let mut tick: u32 = 0;
            while tick < n {
                // Advance both by `check_every` ticks.
                let step = check_every.min(n - tick);
                for _ in 0..step {
                    ev_a.tick(&mut state_a);
                    ev_b.tick(&mut state_b);
                }
                tick += step;

                // Check registers.
                let mut reg_diff: Option<(String, i32, i32)> = None;
                for r in &regs {
                    let va = state_a.get_var(r).unwrap_or(0);
                    let vb = state_b.get_var(r).unwrap_or(0);
                    if va != vb { reg_diff = Some((r.to_string(), va, vb)); break; }
                }
                if reg_diff.is_some() || memory_differs_in_range(&state_a, &state_b, 0, 0xA_0000) {
                    println!("DIVERGED within (tick {}..{}]", tick - step, tick);
                    if let Some((r, va, vb)) = reg_diff {
                        println!("  first mismatched register at checkpoint: {} A={} B={}", r, va, vb);
                    }
                    // Report first memory diff if any.
                    for addr in 0..0xA_0000_i32 {
                        let ba = byte_at(&state_a, addr);
                        let bb = byte_at(&state_b, addr);
                        if ba != bb {
                            println!("  first mem diff at 0x{:X}: A=0x{:02X} B=0x{:02X}", addr, ba, bb);
                            break;
                        }
                    }
                    eprintln!("elapsed {:.1}s", t0.elapsed().as_secs_f64());
                    return;
                }
                eprintln!(
                    "  tick {} checkpoint OK  ({:.1}s elapsed)",
                    tick,
                    t0.elapsed().as_secs_f64(),
                );
            }
            println!("converged through {} ticks", n);
        }
        _ => usage(),
    }
}

fn memory_differs_in_range(a: &State, b: &State, start: i32, end: i32) -> bool {
    // Fast check: scan only reasonable byte ranges; caller narrows later.
    for addr in start..end {
        if byte_at(a, addr) != byte_at(b, addr) {
            return true;
        }
    }
    false
}
