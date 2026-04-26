//! probe_doom_jump — narrow in on the CS:0055→CS:0000 transition for Doom8088.
//!
//! Runs ticks fast until just before the suspected transition, then steps
//! 1 tick at a time, recording (tick, cycles, CS, IP, SS, SP, FLAGS).
//! When CS first becomes 0 (or another anomaly), dumps:
//!   - IVT for INTs 0..=0x67 (so we see both BIOS handlers and any 0:0 entry)
//!   - 16 bytes of memory at the previous CS:IP
//!   - 32 bytes of stack at the previous SS:SP
//! Plus snapshots two ticks before the transition for context.

use std::io::Write;
use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
struct Args {
    #[arg(short, long)]
    input: PathBuf,

    /// Run this many ticks fast before single-stepping.
    #[arg(long, default_value = "838500")]
    fast_to: u32,

    /// Single-step at most this many ticks past fast_to.
    #[arg(long, default_value = "200")]
    step_for: u32,

    /// Where to write the report.
    #[arg(long, default_value = "/tmp/diag-report.md")]
    report: PathBuf,
}

fn read_bytes(state: &calcite_core::State, addr: u32, n: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        out.push(state.read_mem((addr + i as u32) as i32) as u8);
    }
    out
}

fn read_u16(state: &calcite_core::State, addr: u32) -> u16 {
    let lo = state.read_mem(addr as i32) as u8 as u16;
    let hi = state.read_mem((addr + 1) as i32) as u8 as u16;
    lo | (hi << 8)
}

fn hex_dump(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02X}", b)).collect::<Vec<_>>().join(" ")
}

fn main() {
    env_logger::init();
    let args = Args::parse();

    eprintln!("Loading {}", args.input.display());
    let css = std::fs::read_to_string(&args.input).expect("read css");
    let parsed = calcite_core::parser::parse_css(&css).expect("parse");
    let mut state = calcite_core::State::default();
    state.load_properties(&parsed.properties);
    let mut evaluator = calcite_core::Evaluator::from_parsed(&parsed);
    evaluator.wire_state_for_packed_memory(&mut state);

    // Phase 1: run fast in big chunks
    let chunk = 50_000u32;
    let mut tick = 0u32;
    while tick + chunk <= args.fast_to {
        evaluator.run_batch(&mut state, chunk);
        tick += chunk;
    }
    let remaining = args.fast_to - tick;
    if remaining > 0 {
        evaluator.run_batch(&mut state, remaining);
        tick = args.fast_to;
    }
    eprintln!("Reached tick {} (fast)", tick);

    // Phase 1.5: Single-step the LAST 200 ticks before fast_to to capture pre-context
    // Actually instead: keep running 1-tick at a time but recording into history
    // for the window leading up to the transition. We already do this in phase 2;
    // we just need to extend it to cover before-and-after.

    // History buffer of last few states
    #[derive(Clone, Debug)]
    struct Snap {
        tick: u32,
        cycles: i32,
        cs: u16,
        ip: u16,
        ss: u16,
        sp: u16,
        flags: u16,
        ax: u16,
        bx: u16,
        cx: u16,
        dx: u16,
        bp: u16,
        si: u16,
        di: u16,
        ds: u16,
        es: u16,
        // up to 16 bytes at CS:IP (linear)
        code: Vec<u8>,
        // up to 32 bytes at SS:SP
        stack: Vec<u8>,
        // 8 bytes at fixed linear address 0x3E8A (=0055:393A) — to detect SMC
        code_3E8A: Vec<u8>,
        // 32 bytes at fixed linear address 0x3E70 (=0055:3920) — full procedure tail
        code_3E70: Vec<u8>,
    }

    let snap = |state: &calcite_core::State, tick: u32| -> Snap {
        let cs = state.get_var("CS").unwrap_or(0) as u16;
        let ip = state.get_var("IP").unwrap_or(0) as u16;
        let ss = state.get_var("SS").unwrap_or(0) as u16;
        let sp = state.get_var("SP").unwrap_or(0) as u16;
        let flags = state.get_var("FLAGS").unwrap_or(0) as u16;
        let cycles = state.get_var("cycleCount").unwrap_or(0);
        let ax = state.get_var("AX").unwrap_or(0) as u16;
        let bx = state.get_var("BX").unwrap_or(0) as u16;
        let cx = state.get_var("CX").unwrap_or(0) as u16;
        let dx = state.get_var("DX").unwrap_or(0) as u16;
        let bp = state.get_var("BP").unwrap_or(0) as u16;
        let si = state.get_var("SI").unwrap_or(0) as u16;
        let di = state.get_var("DI").unwrap_or(0) as u16;
        let ds = state.get_var("DS").unwrap_or(0) as u16;
        let es = state.get_var("ES").unwrap_or(0) as u16;
        let code_lin = ((cs as u32) << 4).wrapping_add(ip as u32);
        let stack_lin = ((ss as u32) << 4).wrapping_add(sp as u32);
        Snap {
            tick, cycles, cs, ip, ss, sp, flags, ax, bx, cx, dx, bp, si, di, ds, es,
            code: read_bytes(state, code_lin & 0xFFFFF, 16),
            stack: read_bytes(state, stack_lin & 0xFFFFF, 32),
            code_3E8A: read_bytes(state, 0x3E8A, 8),
            code_3E70: read_bytes(state, 0x3E70, 0x40),
        }
    };

    let mut history: Vec<Snap> = Vec::new();
    history.push(snap(&state, tick));

    // Phase 2: single-step
    let mut transition_idx: Option<usize> = None;
    for _ in 0..args.step_for {
        evaluator.run_batch(&mut state, 1);
        tick += 1;
        let s = snap(&state, tick);
        let cs_now = s.cs;
        let cs_prev = history.last().unwrap().cs;
        history.push(s);
        if cs_now == 0 && cs_prev != 0 {
            transition_idx = Some(history.len() - 1);
            // Step a few more to capture post-transition behavior
            for _ in 0..6 {
                evaluator.run_batch(&mut state, 1);
                tick += 1;
                history.push(snap(&state, tick));
            }
            break;
        }
    }

    // Build report
    let mut out = String::new();
    out.push_str("# Doom8088 CS:IP=0:0 transition diagnosis\n\n");
    out.push_str(&format!("Cabinet: `{}`\n\n", args.input.display()));

    // Also capture code at 0x3E91 / etc at every history snapshot
    out.push_str("## Code at 0055:393A linear=0x3E8A across the trace\n\n");
    out.push_str("(captured at the END of probe — represents memory state then)\n\n");
    if let Some(idx) = transition_idx {
        out.push_str(&format!("## Transition detected\n\n"));
        let pre = &history[idx - 1];
        let cur = &history[idx];
        out.push_str(&format!(
            "- **Tick {} → {}**: CS:IP {:04X}:{:04X} → {:04X}:{:04X}\n",
            pre.tick, cur.tick, pre.cs, pre.ip, cur.cs, cur.ip
        ));
        out.push_str(&format!(
            "- cycles {} → {} (delta={})\n",
            pre.cycles, cur.cycles, cur.cycles - pre.cycles
        ));
        out.push_str("\n## Trace around transition\n\n");
        out.push_str("| tick | cycles | CS:IP | SS:SP | FLAGS | AX | BX | CX | DX | BP | SI | DI | DS | ES | bytes@0x3E8A |\n");
        out.push_str("|------|--------|-------|-------|-------|----|----|----|----|----|----|----|----|----|--------------|\n");
        let lo = idx.saturating_sub(80);
        let hi = (idx + 7).min(history.len() - 1);
        for s in &history[lo..=hi] {
            out.push_str(&format!(
                "| {} | {} | {:04X}:{:04X} | {:04X}:{:04X} | {:04X} | {:04X} | {:04X} | {:04X} | {:04X} | {:04X} | {:04X} | {:04X} | {:04X} | {:04X} | {} |\n",
                s.tick, s.cycles, s.cs, s.ip, s.ss, s.sp, s.flags,
                s.ax, s.bx, s.cx, s.dx, s.bp, s.si, s.di, s.ds, s.es,
                hex_dump(&s.code_3E8A)
            ));
        }

        out.push_str("\n## Pre-transition context\n\n");
        out.push_str(&format!("Previous tick {}: CS:IP={:04X}:{:04X}\n\n", pre.tick, pre.cs, pre.ip));
        out.push_str(&format!("Code bytes at CS:IP: `{}`\n\n", hex_dump(&pre.code)));
        out.push_str(&format!("Stack bytes at SS:SP={:04X}:{:04X} (linear=0x{:05X}): `{}`\n\n",
            pre.ss, pre.sp, ((pre.ss as u32) << 4) + pre.sp as u32, hex_dump(&pre.stack)));
        out.push_str(&format!("\n## Code at fixed linear 0x3E70 (= 0055:3920) at pre-transition\n\n"));
        for c in (0..pre.code_3E70.len()).step_by(16) {
            let e = (c+16).min(pre.code_3E70.len());
            out.push_str(&format!("`{:04X}: {}`\n", 0x3920 + c, hex_dump(&pre.code_3E70[c..e])));
        }

        out.push_str("\n## Post-transition\n\n");
        out.push_str(&format!("Current tick {}: CS:IP={:04X}:{:04X}\n\n", cur.tick, cur.cs, cur.ip));
        out.push_str(&format!("Code bytes at CS:IP=0:{:04X}: `{}`\n\n", cur.ip, hex_dump(&cur.code)));
        out.push_str(&format!("Stack bytes at SS:SP={:04X}:{:04X}: `{}`\n\n",
            cur.ss, cur.sp, hex_dump(&cur.stack)));
    } else {
        out.push_str("## NO transition to CS=0 found in this window!\n\n");
        out.push_str(&format!(
            "Last state: tick={} cycles={} CS:IP={:04X}:{:04X} SS:SP={:04X}:{:04X}\n\n",
            history.last().unwrap().tick,
            history.last().unwrap().cycles,
            history.last().unwrap().cs,
            history.last().unwrap().ip,
            history.last().unwrap().ss,
            history.last().unwrap().sp
        ));
    }

    // Dump IVT for INT 0..=0x67
    out.push_str("\n## IVT dump (INT 00h .. INT 67h)\n\n");
    out.push_str("| INT | seg:off | linear | notes |\n");
    out.push_str("|-----|---------|--------|-------|\n");
    for i in 0..0x68u32 {
        let off = read_u16(&state, i * 4);
        let seg = read_u16(&state, i * 4 + 2);
        let lin = ((seg as u32) << 4) + off as u32;
        let note = if seg == 0 && off == 0 {
            "**ZERO!**".to_string()
        } else if seg == 0xF000 {
            "BIOS".to_string()
        } else {
            "".to_string()
        };
        out.push_str(&format!("| {:02X}h | {:04X}:{:04X} | {:05X} | {} |\n", i, seg, off, lin, note));
    }

    // Dump bytes at 0:0..0:0x80 (covering the early IVT raw)
    out.push_str("\n## Raw memory at 0:0000 .. 0:0080\n\n");
    let raw = read_bytes(&state, 0, 0x80);
    for chunk_start in (0..raw.len()).step_by(16) {
        let end = (chunk_start + 16).min(raw.len());
        out.push_str(&format!("`{:04X}: {}`\n", chunk_start, hex_dump(&raw[chunk_start..end])));
    }

    // Dump bytes at 0:0..0:0x80 ALSO showing what's at 0000:0050, the post-transition target
    if let Some(idx) = transition_idx {
        let cur = &history[idx];
        out.push_str(&format!("\n## Memory at post-transition target 0000:{:04X} (linear=0x{:05X})\n\n", cur.ip, cur.ip as u32));
        let bytes = read_bytes(&state, cur.ip as u32, 64);
        for c in (0..bytes.len()).step_by(16) {
            let e = (c+16).min(bytes.len());
            out.push_str(&format!("`{:04X}: {}`\n", cur.ip as u32 + c as u32, hex_dump(&bytes[c..e])));
        }

        // The IRET appears to be at 0055:3941. The instructions BEFORE it (393A..3940)
        // looked like single-byte instructions because IP increments by 1 each tick.
        // Dump 0055:3920..0055:3950 so we can see the procedure tail.
        out.push_str("\n## Code window 0055:3920..0055:3960 (procedure tail before transition)\n\n");
        let base = 0x550u32 * 0x10 + 0x3920;
        let bytes = read_bytes(&state, base, 0x40);
        for c in (0..bytes.len()).step_by(16) {
            let e = (c+16).min(bytes.len());
            out.push_str(&format!("`{:04X}: {}`\n", 0x3920 + c, hex_dump(&bytes[c..e])));
        }

        // Stack region: dump 64 bytes BELOW pre-transition SP and 64 ABOVE
        // (the popping direction).
        let pre = &history[idx - 1];
        let stack_lin = ((pre.ss as u32) << 4) + pre.sp as u32;
        out.push_str(&format!(
            "\n## Stack region around pre-transition SP={:04X}:{:04X} (linear=0x{:05X})\n\n",
            pre.ss, pre.sp, stack_lin));
        // From SS:SP-32 to SS:SP+96 (the bytes that will be popped)
        let dump_start = stack_lin.saturating_sub(32);
        let dump_len = 128usize;
        let bytes = read_bytes(&state, dump_start, dump_len);
        for c in (0..bytes.len()).step_by(16) {
            let e = (c+16).min(bytes.len());
            out.push_str(&format!("`+{:+04X}: {}`\n",
                (dump_start as i64 - stack_lin as i64) + c as i64,
                hex_dump(&bytes[c..e])));
        }

        // Walk back further on the stack — sometimes 1-2 KB below contains data
        out.push_str(&format!(
            "\n## Stack region 0x100 bytes BELOW SS:0 (between 31BD:0 and 31CD:0)\n\n"));
        let bytes = read_bytes(&state, ((pre.ss as u32) << 4).saturating_sub(0x100), 0x200);
        for c in (0..bytes.len()).step_by(16) {
            let e = (c+16).min(bytes.len());
            out.push_str(&format!("`{:05X}: {}`\n",
                ((pre.ss as u32) << 4).saturating_sub(0x100) + c as u32,
                hex_dump(&bytes[c..e])));
        }
    }

    std::fs::write(&args.report, out.as_bytes()).expect("write report");
    eprintln!("Report written to {}", args.report.display());

    // Also print the report to stdout
    let txt = std::fs::read_to_string(&args.report).unwrap();
    print!("{}", txt);
}
