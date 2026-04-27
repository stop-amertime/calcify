use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use std::path::PathBuf;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod calcite_logo;
mod cssdos_logo;
mod menu;

/// calc(ite) — JIT compiler for computational CSS.
///
/// Parses CSS files, recognises computational patterns, and evaluates
/// them efficiently. Primary target: running x86CSS faster than Chrome.
#[derive(Parser, Debug)]
#[command(name = "calcite", version, about)]
struct Cli {
    /// Path to the CSS file to evaluate.
    ///
    /// If omitted, launches the interactive CSS-DOS program picker.
    #[arg(short, long)]
    input: Option<PathBuf>,

    /// Number of ticks to run. Omit for unlimited (interactive).
    #[arg(short = 'n', long)]
    ticks: Option<u32>,

    /// Print register state after each tick.
    #[arg(short, long)]
    verbose: bool,

    /// Only parse and analyse patterns (don't evaluate).
    #[arg(long)]
    parse_only: bool,

    /// Render the screen every N ticks.
    ///
    /// Reads BDA 0x0449 to determine the current video mode and renders
    /// accordingly: text modes show an in-terminal ANSI screen; Mode 13h
    /// shows a status line. Default: 500. Set to 0 for render-on-exit only.
    #[arg(long, value_name = "N", default_value = "500")]
    screen_interval: u32,

    /// Ticks per batch inside the interactive loop.
    ///
    /// The interactive loop polls keyboard, checks halt, and re-renders between
    /// batches. At 4.77 MHz 8086 timing, 50000 ticks ≈ 10.5 ms ≈ 95 fps, so
    /// keyboard latency at this batch size is imperceptible. Smaller values
    /// give finer-grained input at the cost of throughput.
    #[arg(long, value_name = "N", default_value = "50000")]
    interactive_batch: u32,

    /// Throttle execution to this multiple of real 8086 speed (4.77 MHz).
    ///
    /// 1.0 = real 8086 timing (default). 2.0 = double speed. 0 = unlimited
    /// (run as fast as the machine can). Throttling uses the 8086 cycleCount,
    /// not wall-ticks, so programs with variable cycles-per-instruction feel
    /// correct.
    #[arg(long, value_name = "MULT", default_value = "1.0")]
    speed: f64,

    /// Dump all computed slot values at a specific tick for debugging.
    ///
    /// Runs to the specified tick, then prints every property name and its
    /// computed value. Useful for diagnosing divergences found by compare.mjs.
    #[arg(long, value_name = "TICK")]
    dump_tick: Option<u32>,

    /// Dump state at multiple ticks in a single run.
    ///
    /// Comma-separated list of ticks (e.g. `--dump-ticks=100000,200000,300000`).
    /// Compiles once, runs forward, dumps at each tick. Much faster than
    /// invoking the CLI multiple times for sampling a property over time.
    /// Combine with --sample-cells for compact per-tick output.
    #[arg(long, value_name = "LIST", value_delimiter = ',')]
    dump_ticks: Vec<u32>,

    /// Only print these memory cells in dumps (comma-separated packed-cell indices).
    ///
    /// Example: `--sample-cells=2344,632` prints just those cells. Combine with
    /// --dump-tick or --dump-ticks to track specific memory cells across time.
    /// Without this flag, dumps include all state-var and cell values.
    #[arg(long, value_name = "LIST", value_delimiter = ',')]
    sample_cells: Vec<u32>,

    /// Log every tick when a specified cell changes value.
    ///
    /// Comma-separated list of packed-cell indices to watch. Runs the program
    /// tick-by-tick (no batching) and prints a line whenever any watched cell
    /// changes: `tick=N mcIDX: OLD -> NEW (CS:IP)`. Use with --ticks N to bound
    /// the run. Slower than batched execution but finer-grained than sampling.
    #[arg(long, value_name = "LIST", value_delimiter = ',')]
    watch_cell: Vec<u32>,

    /// Output a JSON register trace for conformance comparison.
    ///
    /// Each tick produces a JSON object with register values.
    /// Combine with compare.mjs for automated divergence detection.
    #[arg(long)]
    trace_json: bool,

    /// Stop execution when memory at ADDR becomes non-zero.
    ///
    /// The address is checked after each tick. Useful for halt flags
    /// written by BIOS/DOS exit handlers.
    #[arg(long, value_name = "ADDR")]
    halt: Option<String>,

    /// After execution, write the Mode 13h framebuffer to a PPM file.
    ///
    /// Only written when the final video mode is 0x13 (320x200x256).
    /// Useful for capturing the last frame of a graphics program.
    #[arg(long, value_name = "PATH")]
    framebuffer_out: Option<String>,

    /// Inject keyboard events at specific ticks.
    ///
    /// Format: TICK:VALUE,TICK:VALUE,...
    /// VALUE is (scancode<<8)|ascii in decimal or 0x hex.
    /// Use VALUE=0 for key release.
    ///
    /// Example: --key-events=50:0x1E61,100:0
    /// Injects key 'a' (scan=0x1E, ascii=0x61) at tick 50, release at tick 100.
    #[arg(long, value_name = "EVENTS")]
    key_events: Option<String>,

    /// Dump a flat byte range of guest memory to a binary file at end of run.
    ///
    /// Format: `ADDR:LEN:PATH`. ADDR and LEN may be decimal or 0xHEX. Writes
    /// `LEN` bytes starting at linear address `ADDR` to `PATH`. Repeat the
    /// flag to dump multiple regions in one run.
    ///
    /// Designed as the cheap data-source for an external screenshot tool:
    /// run `calcite-cli` to a target tick, dump VRAM (and the BDA video-mode
    /// byte) to disk, then let a Node-side rasteriser turn the bytes into
    /// pixels. Avoids the per-tick chunked-IPC cost of going through
    /// `calcite-debugger` and avoids re-implementing the rasterisers in Rust.
    ///
    /// Example: `--dump-mem-range=0xB8000:4000:vram.bin --dump-mem-range=0x449:1:mode.bin`
    #[arg(long = "dump-mem-range", value_name = "ADDR:LEN:PATH")]
    dump_mem_ranges: Vec<String>,

    /// Trace the moment haltCode goes 0->nonzero.
    ///
    /// Runs tick-by-tick, keeps a small ring buffer of (CS, IP, AX, BX, CX, DX,
    /// SS, SP, opcode) values, and prints the last N entries when haltCode
    /// transitions to non-zero. Useful for finding the exact instruction that
    /// branched the CPU into garbage. Stops at the first halt; combine with
    /// --ticks N as an upper bound.
    #[arg(long)]
    trace_halt: bool,

    /// Fast-forward (run_batch) to this tick before starting tick-by-tick tracing.
    /// Useful with --trace-halt when you already know the halt is far in.
    #[arg(long, value_name = "N")]
    trace_halt_skip: Option<u32>,

    /// Scripted action to fire at a specific tick. Repeatable.
    ///
    /// Format: `TICK:KIND[:ARGS...]`. TICK may be decimal or 0xHEX. KIND is
    /// one of:
    ///
    ///   `kbd:VALUE`               — set --keyboard to VALUE at TICK.
    ///                               VALUE = (scan << 8) | ascii. Decimal or 0xHEX.
    ///   `tap:VALUE[:HOLD]`        — press VALUE at TICK, release at TICK+HOLD.
    ///                               HOLD defaults to 5000 ticks. Use this for
    ///                               menu/key input — a single `kbd` doesn't
    ///                               produce a release edge, so subsequent
    ///                               keys are sometimes ignored.
    ///   `dump:ADDR:LEN:PATH`      — write LEN bytes from guest linear ADDR to PATH.
    ///   `regs[:TAG]`              — print one line of regs to stdout, prefixed `tag=TAG`.
    ///   `halt`                    — stop the run at TICK.
    ///
    /// Examples:
    ///   `--script-event=2000000:tap:0x1c0d` (Enter at 2M ticks)
    ///   `--script-event=10000000:dump:0xa0000:64000:vga@10m.bin`
    ///   `--script-event=15000000:regs:title-check`
    ///   `--script-event=30000000:halt`
    ///
    /// Events are sorted by TICK; between events calcite uses run_batch
    /// for full speed. This is the way to script multi-step interactions
    /// (press a key, wait, dump, press another key) without paying the
    /// per-tick cost of `--key-events`.
    #[arg(long = "script-event", value_name = "TICK:KIND[:ARGS]")]
    script_events: Vec<String>,

    /// Read additional --script-event entries from a file, one per line.
    /// Lines starting with `#` and blank lines are ignored.
    #[arg(long = "script-file", value_name = "PATH")]
    script_file: Option<PathBuf>,
}


/// Map a crossterm key event to DOS keyboard value: (scancode << 8) | ascii.
/// Returns 0 for unmapped keys.
fn key_to_dos(key: &KeyEvent) -> i32 {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    let (scan, ascii): (u8, u8) = match key.code {
        KeyCode::Esc => (0x01, 0x1B),
        KeyCode::Char('1') | KeyCode::Char('!') => (0x02, key.code.into_ascii()),
        KeyCode::Char('2') | KeyCode::Char('@') => (0x03, key.code.into_ascii()),
        KeyCode::Char('3') | KeyCode::Char('#') => (0x04, key.code.into_ascii()),
        KeyCode::Char('4') | KeyCode::Char('$') => (0x05, key.code.into_ascii()),
        KeyCode::Char('5') | KeyCode::Char('%') => (0x06, key.code.into_ascii()),
        KeyCode::Char('6') | KeyCode::Char('^') => (0x07, key.code.into_ascii()),
        KeyCode::Char('7') | KeyCode::Char('&') => (0x08, key.code.into_ascii()),
        KeyCode::Char('8') | KeyCode::Char('*') => (0x09, key.code.into_ascii()),
        KeyCode::Char('9') | KeyCode::Char('(') => (0x0A, key.code.into_ascii()),
        KeyCode::Char('0') | KeyCode::Char(')') => (0x0B, key.code.into_ascii()),
        KeyCode::Char('-') | KeyCode::Char('_') => (0x0C, key.code.into_ascii()),
        KeyCode::Char('=') | KeyCode::Char('+') => (0x0D, key.code.into_ascii()),
        KeyCode::Backspace => (0x0E, 0x08),
        KeyCode::Tab => (0x0F, 0x09),
        KeyCode::Char('q') | KeyCode::Char('Q') => (0x10, key.code.into_ascii()),
        KeyCode::Char('w') | KeyCode::Char('W') => (0x11, key.code.into_ascii()),
        KeyCode::Char('e') | KeyCode::Char('E') => (0x12, key.code.into_ascii()),
        KeyCode::Char('r') | KeyCode::Char('R') => (0x13, key.code.into_ascii()),
        KeyCode::Char('t') | KeyCode::Char('T') => (0x14, key.code.into_ascii()),
        KeyCode::Char('y') | KeyCode::Char('Y') => (0x15, key.code.into_ascii()),
        KeyCode::Char('u') | KeyCode::Char('U') => (0x16, key.code.into_ascii()),
        KeyCode::Char('i') | KeyCode::Char('I') => (0x17, key.code.into_ascii()),
        KeyCode::Char('o') | KeyCode::Char('O') => (0x18, key.code.into_ascii()),
        KeyCode::Char('p') | KeyCode::Char('P') => (0x19, key.code.into_ascii()),
        KeyCode::Char('[') | KeyCode::Char('{') => (0x1A, key.code.into_ascii()),
        KeyCode::Char(']') | KeyCode::Char('}') => (0x1B, key.code.into_ascii()),
        KeyCode::Enter => (0x1C, 0x0D),
        KeyCode::Char('a') | KeyCode::Char('A') => (0x1E, key.code.into_ascii()),
        KeyCode::Char('s') | KeyCode::Char('S') => (0x1F, key.code.into_ascii()),
        KeyCode::Char('d') | KeyCode::Char('D') => (0x20, key.code.into_ascii()),
        KeyCode::Char('f') | KeyCode::Char('F') => (0x21, key.code.into_ascii()),
        KeyCode::Char('g') | KeyCode::Char('G') => (0x22, key.code.into_ascii()),
        KeyCode::Char('h') | KeyCode::Char('H') => (0x23, key.code.into_ascii()),
        KeyCode::Char('j') | KeyCode::Char('J') => (0x24, key.code.into_ascii()),
        KeyCode::Char('k') | KeyCode::Char('K') => (0x25, key.code.into_ascii()),
        KeyCode::Char('l') | KeyCode::Char('L') => (0x26, key.code.into_ascii()),
        KeyCode::Char(';') | KeyCode::Char(':') => (0x27, key.code.into_ascii()),
        KeyCode::Char('\'') | KeyCode::Char('"') => (0x28, key.code.into_ascii()),
        KeyCode::Char('`') | KeyCode::Char('~') => (0x29, key.code.into_ascii()),
        KeyCode::Char('\\') | KeyCode::Char('|') => (0x2B, key.code.into_ascii()),
        KeyCode::Char('z') | KeyCode::Char('Z') => (0x2C, key.code.into_ascii()),
        KeyCode::Char('x') | KeyCode::Char('X') => (0x2D, key.code.into_ascii()),
        KeyCode::Char('c') | KeyCode::Char('C') => (0x2E, key.code.into_ascii()),
        KeyCode::Char('v') | KeyCode::Char('V') => (0x2F, key.code.into_ascii()),
        KeyCode::Char('b') | KeyCode::Char('B') => (0x30, key.code.into_ascii()),
        KeyCode::Char('n') | KeyCode::Char('N') => (0x31, key.code.into_ascii()),
        KeyCode::Char('m') | KeyCode::Char('M') => (0x32, key.code.into_ascii()),
        KeyCode::Char(',') | KeyCode::Char('<') => (0x33, key.code.into_ascii()),
        KeyCode::Char('.') | KeyCode::Char('>') => (0x34, key.code.into_ascii()),
        KeyCode::Char('/') | KeyCode::Char('?') => (0x35, key.code.into_ascii()),
        KeyCode::Char(' ') => (0x39, 0x20),
        // Extended keys: ascii=0
        KeyCode::Up => (0x48, 0),
        KeyCode::Down => (0x50, 0),
        KeyCode::Left => (0x4B, 0),
        KeyCode::Right => (0x4D, 0),
        KeyCode::Home => (0x47, 0),
        KeyCode::End => (0x4F, 0),
        KeyCode::PageUp => (0x49, 0),
        KeyCode::PageDown => (0x51, 0),
        KeyCode::Insert => (0x52, 0),
        KeyCode::Delete => (0x53, 0),
        KeyCode::F(1) => (0x3B, 0),
        KeyCode::F(2) => (0x3C, 0),
        KeyCode::F(3) => (0x3D, 0),
        KeyCode::F(4) => (0x3E, 0),
        KeyCode::F(5) => (0x3F, 0),
        KeyCode::F(6) => (0x40, 0),
        KeyCode::F(7) => (0x41, 0),
        KeyCode::F(8) => (0x42, 0),
        KeyCode::F(9) => (0x43, 0),
        KeyCode::F(10) => (0x44, 0),
        _ => return 0,
    };

    // Ctrl+letter: ascii becomes 1-26
    let ascii = if ctrl {
        match key.code {
            KeyCode::Char(c) if c.is_ascii_alphabetic() => (c.to_ascii_lowercase() as u8) - b'a' + 1,
            _ => ascii,
        }
    } else {
        ascii
    };

    ((scan as i32) << 8) | (ascii as i32)
}

/// Helper trait to extract ASCII value from KeyCode.
trait KeyCodeAscii {
    fn into_ascii(self) -> u8;
}

impl KeyCodeAscii for KeyCode {
    fn into_ascii(self) -> u8 {
        match self {
            KeyCode::Char(c) => c as u8,
            _ => 0,
        }
    }
}

/// Parse and execute a `--dump-mem-range=ADDR:LEN:PATH` spec: write `LEN`
/// bytes from guest linear address `ADDR` to `PATH`. Returns a stringified
/// error on parse or write failure. Numbers may be decimal or `0x`-prefixed.
///
/// Out-of-range reads return zeros (matching `state.read_mem` semantics) so a
/// dump request that overshoots the cabinet's allocated memory still produces
/// a file of the requested length — the caller can decide whether all-zeros
/// means "VRAM not yet written" or "wrong address".
fn dump_mem_range(spec: &str, state: &calcite_core::State) -> Result<(), String> {
    let parts: Vec<&str> = spec.splitn(3, ':').collect();
    if parts.len() != 3 {
        return Err(format!("expected ADDR:LEN:PATH, got {spec:?}"));
    }
    let parse_num = |s: &str, what: &str| -> Result<i64, String> {
        let s = s.trim();
        let v = if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            i64::from_str_radix(hex, 16)
        } else {
            s.parse::<i64>()
        };
        v.map_err(|e| format!("invalid {what} {s:?}: {e}"))
    };
    let addr = parse_num(parts[0], "ADDR")?;
    let len = parse_num(parts[1], "LEN")?;
    if len < 0 {
        return Err(format!("LEN must be non-negative, got {len}"));
    }
    let path = parts[2];
    let mut out = vec![0u8; len as usize];
    for i in 0..(len as usize) {
        out[i] = (state.read_mem(addr as i32 + i as i32) & 0xFF) as u8;
    }
    std::fs::write(path, &out).map_err(|e| format!("write {path:?}: {e}"))?;
    eprintln!("dump-mem-range: {} bytes from 0x{:X} -> {}", len, addr, path);
    Ok(())
}

/// One scheduled action during a run. Parsed from `--script-event` strings
/// of the form `TICK:KIND[:ARGS...]`. See the flag's clap help for syntax.
#[derive(Debug, Clone)]
enum ScriptAction {
    Kbd { value: i32 },
    DumpMem { addr: i64, len: i64, path: String },
    Regs { tag: String },
    Halt,
}

#[derive(Debug, Clone)]
struct ScriptEvent {
    tick: u32,
    action: ScriptAction,
}

fn parse_int_flexible(s: &str, what: &str) -> Result<i64, String> {
    let s = s.trim();
    let v = if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        i64::from_str_radix(hex, 16)
    } else {
        s.parse::<i64>()
    };
    v.map_err(|e| format!("invalid {what} {s:?}: {e}"))
}

/// Returns one or more events (a `tap` becomes a press + release pair).
fn parse_script_event(spec: &str) -> Result<Vec<ScriptEvent>, String> {
    let mut iter = spec.splitn(2, ':');
    let tick_s = iter.next().ok_or_else(|| format!("empty script-event {spec:?}"))?;
    let rest = iter.next().ok_or_else(|| format!("script-event missing KIND: {spec:?}"))?;
    let tick = parse_int_flexible(tick_s, "TICK")?;
    if tick < 0 || tick > u32::MAX as i64 {
        return Err(format!("TICK out of range in {spec:?}"));
    }
    let mut kind_iter = rest.splitn(2, ':');
    let kind = kind_iter.next().unwrap_or("").trim();
    let args = kind_iter.next().unwrap_or("");
    match kind {
        "kbd" => {
            let v = parse_int_flexible(args, "VALUE")?;
            Ok(vec![ScriptEvent { tick: tick as u32, action: ScriptAction::Kbd { value: v as i32 } }])
        }
        "tap" => {
            // tap:VALUE[:HOLD_TICKS] — synthesize a press + release pair so
            // the cabinet sees an _kbdPress edge then an _kbdRelease edge.
            // Without the release, --prevKeyboard stays non-zero and the
            // next "kbd <- VAL" only triggers a press if VAL differs from
            // prev. Default hold = 5000 ticks, plenty for the 18.2 Hz PIT
            // to fire IRQ 1 several times.
            let parts: Vec<&str> = args.splitn(2, ':').collect();
            let v = parse_int_flexible(parts[0], "VALUE")?;
            let hold = if parts.len() == 2 {
                parse_int_flexible(parts[1], "HOLD_TICKS")? as u32
            } else { 5000 };
            let release_tick = (tick as u32).saturating_add(hold);
            Ok(vec![
                ScriptEvent { tick: tick as u32, action: ScriptAction::Kbd { value: v as i32 } },
                ScriptEvent { tick: release_tick, action: ScriptAction::Kbd { value: 0 } },
            ])
        }
        "dump" => {
            let parts: Vec<&str> = args.splitn(3, ':').collect();
            if parts.len() != 3 {
                return Err(format!("dump expects ADDR:LEN:PATH, got {args:?}"));
            }
            let addr = parse_int_flexible(parts[0], "ADDR")?;
            let len = parse_int_flexible(parts[1], "LEN")?;
            if len < 0 { return Err(format!("LEN must be non-negative, got {len}")); }
            Ok(vec![ScriptEvent { tick: tick as u32, action: ScriptAction::DumpMem { addr, len, path: parts[2].to_string() } }])
        }
        "regs" => {
            let tag = if args.is_empty() { String::new() } else { args.to_string() };
            Ok(vec![ScriptEvent { tick: tick as u32, action: ScriptAction::Regs { tag } }])
        }
        "halt" => Ok(vec![ScriptEvent { tick: tick as u32, action: ScriptAction::Halt }]),
        _ => Err(format!("unknown script-event KIND {kind:?} in {spec:?}")),
    }
}

fn load_script_events(specs: &[String], file: Option<&std::path::Path>) -> Result<Vec<ScriptEvent>, String> {
    let mut out: Vec<ScriptEvent> = Vec::new();
    for s in specs {
        out.extend(parse_script_event(s)?);
    }
    if let Some(p) = file {
        let text = std::fs::read_to_string(p).map_err(|e| format!("read {p:?}: {e}"))?;
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            out.extend(parse_script_event(line).map_err(|e| format!("{p:?} line {}: {e}", i + 1))?);
        }
    }
    out.sort_by_key(|e| e.tick);
    Ok(out)
}

fn execute_script_action(action: &ScriptAction, state: &mut calcite_core::State, current_tick: u32) -> bool {
    // Returns true if execution should halt after this action.
    match action {
        ScriptAction::Kbd { value } => {
            state.set_var("keyboard", *value);
            eprintln!("[script {}] kbd <- 0x{:04x}", current_tick, *value as u32 & 0xFFFF);
            false
        }
        ScriptAction::DumpMem { addr, len, path } => {
            let mut buf = vec![0u8; *len as usize];
            for i in 0..(*len as usize) {
                buf[i] = (state.read_mem(*addr as i32 + i as i32) & 0xFF) as u8;
            }
            if let Err(e) = std::fs::write(path, &buf) {
                eprintln!("[script {}] dump 0x{:X}+{} -> {}: ERROR {}", current_tick, addr, len, path, e);
            } else {
                eprintln!("[script {}] dump 0x{:X}+{} -> {}", current_tick, addr, len, path);
            }
            false
        }
        ScriptAction::Regs { tag } => {
            // Print a compact one-line regs dump prefixed with the tag.
            let names = ["AX","BX","CX","DX","SP","BP","SI","DI","IP","CS","DS","ES","SS","flags","cycleCount"];
            let mut line = format!("[script {} regs", current_tick);
            if !tag.is_empty() { line.push_str(&format!(" tag={}", tag)); }
            line.push(']');
            for name in names {
                let v = state.state_var_names.iter().position(|n| n == name)
                    .map(|i| state.state_vars[i]).unwrap_or(0);
                line.push_str(&format!(" {}={}", name, v));
            }
            println!("{}", line);
            false
        }
        ScriptAction::Halt => {
            eprintln!("[script {}] halt", current_tick);
            true
        }
    }
}

fn main() {
    env_logger::init();
    let cli = Cli::parse();

    // Resolve input path: direct --input, or interactive menu.
    let input_path: PathBuf = match cli.input.clone() {
        Some(p) => p,
        None => {
            // Interactive menu mode. Root is the current working directory
            // (run.bat already cd's to calcite/; direct users should invoke
            // from the calcite repo root).
            let root = PathBuf::from(r"C:\Users\AdmT9N0CX01V65438A\Documents\src\calcite");
            let entries = menu::discover(&root);
            let selected = match menu::run(&entries) {
                Ok(Some(i)) => i,
                Ok(None) => {
                    eprintln!();
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("Menu error: {e}");
                    std::process::exit(1);
                }
            };
            // Clear after the menu exits so the generator output starts fresh.
            print!("\x1b[2J\x1b[H");
            cssdos_logo::print();
            match menu::resolve_to_css(&entries[selected], &root) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("Failed to generate CSS: {e}");
                    std::process::exit(1);
                }
            }
        }
    };

    // Transition into the calcite half of the experience: fresh screen,
    // calcite banner, parse/compile bars, video.
    print!("\x1b[2J\x1b[H");
    calcite_logo::print();

    let css = match std::fs::read_to_string(&input_path) {
        Ok(content) => content,
        Err(e) => {
            eprintln!("Error reading {}: {e}", input_path.display());
            std::process::exit(1);
        }
    };

    log::info!(
        "Read {} bytes of CSS from {}",
        css.len(),
        input_path.display()
    );

    let t0 = std::time::Instant::now();
    match calcite_core::parser::parse_css(&css) {
        Ok(parsed) => {
            let parse_time = t0.elapsed();
            eprintln!(
                "Parsed: {} @property, {} @function, {} assignments ({:.2}s)",
                parsed.properties.len(),
                parsed.functions.len(),
                parsed.assignments.len(),
                parse_time.as_secs_f64(),
            );

            if cli.parse_only {
                return;
            }

            // load_properties MUST be called before from_parsed so that state
            // variable addresses are in the address map before compilation.
            let mut state = calcite_core::State::default();
            state.load_properties(&parsed.properties);

            // Load the cabinet's disk sidecar so REP MOVSW from the rom-disk
            // window can fast-forward in bulk. Without this, every BIOS
            // sector load (Doom8088 hits hundreds during init) costs 256
            // ticks for the per-byte CSS dispatch instead of 1 in bulk —
            // calcite never reaches the demo loop within feasible tick budgets.
            // Sidecar is `<cabinet>.disk.bin` next to the .css.
            {
                let mut disk_path = input_path.clone();
                let stem = disk_path.file_stem().map(|s| s.to_string_lossy().to_string());
                if let Some(stem) = stem {
                    disk_path.set_file_name(format!("{}.disk.bin", stem));
                    if let Ok(bytes) = std::fs::read(&disk_path) {
                        eprintln!("loaded disk sidecar: {} ({} bytes)", disk_path.display(), bytes.len());
                        state.disk_image = Some(bytes);
                    }
                }
            }

            let t1 = std::time::Instant::now();
            let mut evaluator = calcite_core::Evaluator::from_parsed(&parsed);
            let compile_time = t1.elapsed();

            // Packed-cell cabinets (PACK_SIZE > 1) keep guest memory in
            // `mcN` state vars, not in `state.memory[]`. Without this call
            // every positive-address read returns 0 even though the CPU
            // wrote the value — see Evaluator::wire_state_for_packed_memory.
            evaluator.wire_state_for_packed_memory(&mut state);

            // Interactive iff --ticks is omitted; in that case run unlimited.
            let interactive = cli.ticks.is_none();
            let ticks_limit: u32 = cli.ticks.unwrap_or(u32::MAX);
            eprintln!(
                "Compiled: {:.2}s (parse {:.2}s + compile {:.2}s), memory: {} KB",
                (parse_time + compile_time).as_secs_f64(),
                parse_time.as_secs_f64(),
                compile_time.as_secs_f64(),
                state.memory.len() / 1024,
            );
            if interactive {
                eprintln!("Starting... (Ctrl+C to quit)");
            }

            let halt_addr: Option<i32> = cli.halt.as_ref().map(|s| {
                if s.starts_with("0x") || s.starts_with("0X") {
                    i32::from_str_radix(&s[2..], 16).expect("Invalid halt address")
                } else if s.starts_with('-') || s.chars().next().map_or(false, |c| c.is_ascii_digit()) {
                    s.parse().expect("Invalid halt address")
                } else {
                    // Treat as a state variable name — look up its slot address
                    if let Some(slot) = state.var_slot(s) {
                        -(slot as i32) - 1
                    } else {
                        panic!("Unknown state variable for --halt: {}", s);
                    }
                }
            });

            // Parse --key-events=TICK:VALUE,TICK:VALUE,...
            let key_events: Vec<(u32, i32)> = cli.key_events.as_ref().map(|s| {
                s.split(',')
                    .filter(|part| !part.is_empty())
                    .map(|part| {
                        let (tick_str, val_str) = part.split_once(':')
                            .unwrap_or_else(|| panic!("Invalid key-event format: {}", part));
                        let tick: u32 = tick_str.parse()
                            .unwrap_or_else(|_| panic!("Invalid tick in key-event: {}", tick_str));
                        let val: i32 = if val_str.starts_with("0x") || val_str.starts_with("0X") {
                            i32::from_str_radix(&val_str[2..], 16)
                                .unwrap_or_else(|_| panic!("Invalid hex value in key-event: {}", val_str))
                        } else {
                            val_str.parse()
                                .unwrap_or_else(|_| panic!("Invalid value in key-event: {}", val_str))
                        };
                        (tick, val)
                    })
                    .collect()
            }).unwrap_or_default();

            // Parse --script-event / --script-file. Sorted by tick.
            let script_events: Vec<ScriptEvent> = match load_script_events(
                &cli.script_events,
                cli.script_file.as_deref(),
            ) {
                Ok(v) => v,
                Err(e) => { eprintln!("script: {e}"); std::process::exit(1); }
            };
            if !script_events.is_empty() {
                eprintln!("loaded {} script event(s)", script_events.len());
            }

            let t2 = std::time::Instant::now();

            // Real 8086 clock: 4.77 MHz. We derive CPU speed from cycleCount
            // (accumulated 8086 cycle costs per instruction) divided by wall time.
            const REAL_8086_HZ: f64 = 4_772_727.0;

            // Format a frequency at a fixed width so the status line doesn't
            // glitch back and forth. Always MHz with 2 decimals for the rolling
            // status display (1.00 KHz == 0.00 MHz, still readable).
            fn format_hz(hz: f64) -> String {
                if hz >= 1_000_000.0 {
                    format!("{:.2} MHz", hz / 1_000_000.0)
                } else if hz >= 1_000.0 {
                    format!("{:.1} KHz", hz / 1_000.0)
                } else {
                    format!("{:.0} Hz", hz)
                }
            }
            fn format_hz_fixed(hz: f64) -> String {
                // Always "X.XX MHz" — 8 chars wide. At low speeds this reads
                // "0.04 MHz" etc., which stays aligned column-wise across
                // repaints.
                format!("{:5.2} MHz", hz / 1_000_000.0)
            }

            // Render the screen based on BDA video mode (0x0449).
            // Text modes: render in-terminal using ANSI. Mode 13h: status line.
            //
            // ticks = CSS ticks (= instructions in V4 single-cycle arch)
            // cycles = accumulated 8086 clock cycles from cycleCount
            //
            // Status line throttling: the speed readout only refreshes every
            // ~500ms of wall time so the digits don't thrash between repaints.
            // We also EMA-smooth ticks/s to make it calmer.
            let mut smoothed_tps: f64 = 0.0;
            let mut last_status_update = std::time::Instant::now();
            let mut cached_status = String::new();
            let mut render_screen = |state: &calcite_core::State, ticks: u32,
                                  delta_ticks: u32, delta_secs: f64,
                                  first: bool| {
                let cycles = state.get_var("cycleCount").unwrap_or(0) as u64;
                let instant_tps = if delta_secs > 0.0 { delta_ticks as f64 / delta_secs } else { 0.0 };
                // Exponential moving average — alpha 0.3 feels smooth but still
                // responsive.
                if smoothed_tps == 0.0 {
                    smoothed_tps = instant_tps;
                } else {
                    smoothed_tps = 0.3 * instant_tps + 0.7 * smoothed_tps;
                }
                let now = std::time::Instant::now();
                let since_update = now.duration_since(last_status_update).as_secs_f64();
                if first || since_update >= 0.5 {
                    let avg_cpt = if ticks > 0 { cycles as f64 / ticks as f64 } else { 10.0 };
                    let cycles_per_sec = smoothed_tps * avg_cpt;
                    let pct = cycles_per_sec / REAL_8086_HZ * 100.0;
                    // Fixed-width fields so the line doesn't jitter.
                    cached_status = format!(
                        " {:>10} ticks | {} | {:>5.1}% of 8086 | {:>9.0} ticks/s",
                        ticks, format_hz_fixed(cycles_per_sec), pct, smoothed_tps
                    );
                    last_status_update = now;
                }
                let status = &cached_status;
                let video_mode = state.read_mem(0x0449) as u8;
                if video_mode == 0x13 {
                    // Render 320x200 mode 13h via ANSI half-block chars.
                    // Each '▀' cell shows two vertical pixels: fg=upper, bg=lower.
                    // Downsample horizontally 2:1 so 320-wide fits typical terminals.
                    if first {
                        eprint!("\x1b[2J\x1b[H\x1b[?25l");
                    } else {
                        eprint!("\x1b[H");
                    }
                    let mut out = String::with_capacity(320 * 100 * 24);
                    for y in (0..200).step_by(2) {
                        for x in 0..320 {
                            let top = state.read_mem(0xA0000 + y * 320 + x) as usize & 0x0F;
                            let bot = state.read_mem(0xA0000 + (y + 1) * 320 + x) as usize & 0x0F;
                            let (tr, tg, tb) = calcite_core::state::CGA_PALETTE[top];
                            let (br, bg_, bb) = calcite_core::state::CGA_PALETTE[bot];
                            out.push_str(&format!(
                                "\x1b[38;2;{};{};{}m\x1b[48;2;{};{};{}m▀",
                                tr, tg, tb, br, bg_, bb
                            ));
                        }
                        out.push_str("\x1b[0m\r\n");
                    }
                    eprint!("{out}{status}\r\n");
                } else {
                    let cols = state.read_mem(0x044A) as usize;
                    let (width, height) = if cols == 40 { (40, 25) } else { (80, 25) };
                    if first {
                        eprint!("\x1b[2J\x1b[H\x1b[?25l");
                    } else {
                        eprint!("\x1b[H");
                    }
                    let screen = state.render_screen_ansi(0xB8000, width, height);
                    eprint!("┌{}┐\r\n", "─".repeat(width));
                    for line in screen.lines() {
                        eprint!("│{line}\x1b[0m│\r\n");
                    }
                    eprint!("└{}┘\r\n", "─".repeat(width));
                    eprint!("{status}\r\n");
                }
            };

            // --dump-tick / --dump-ticks: run to specific tick(s) and dump state
            // Build a sorted list of targets: either the single --dump-tick value,
            // or all values from --dump-ticks (combined if both given).
            let mut dump_targets: Vec<u32> = cli.dump_ticks.clone();
            if let Some(t) = cli.dump_tick {
                dump_targets.push(t);
            }
            if !dump_targets.is_empty() {
                dump_targets.sort_unstable();
                dump_targets.dedup();
                let compact = !cli.sample_cells.is_empty();

                // Print a header so callers can tell this is the dump output.
                if compact {
                    // Header: tick + register names + sampled cell indices.
                    print!("# tick");
                    for name in ["AX","BX","CX","DX","SP","BP","SI","DI","IP","CS","DS","ES","SS","flags","cycleCount"] {
                        print!(" {}", name);
                    }
                    for &idx in &cli.sample_cells {
                        print!(" mc{}", idx);
                    }
                    println!();
                }

                // Advance to each target in order, dumping along the way.
                let mut cursor: u32 = 0;
                for &target in &dump_targets {
                    // Advance from cursor to target. Run tick-by-tick when key events
                    // need injection; otherwise use run_batch + a final tick.
                    if !key_events.is_empty() {
                        for tick in cursor..target {
                            for &(ev_tick, ev_val) in &key_events {
                                if ev_tick == tick {
                                    state.set_var("keyboard", ev_val);
                                }
                            }
                            evaluator.tick(&mut state);
                        }
                    } else if target > cursor {
                        let batch = target - cursor;
                        if batch > 1 {
                            evaluator.run_batch(&mut state, batch - 1);
                        }
                        evaluator.tick(&mut state);
                    }
                    cursor = target;

                    if compact {
                        // One line per tick: tick + core regs + sampled cells.
                        print!("{}", target);
                        for name in ["AX","BX","CX","DX","SP","BP","SI","DI","IP","CS","DS","ES","SS","flags","cycleCount"] {
                            let v = state.state_var_names.iter().position(|n| n == name)
                                .map(|i| state.state_vars[i]).unwrap_or(0);
                            print!(" {}", v);
                        }
                        // Sampled cells — look up by "mcN" state-var name (how kiln
                        // emits packed cells) and fall back to 0 for unknown cells.
                        for &idx in &cli.sample_cells {
                            let name = format!("mc{}", idx);
                            let v = state.state_var_names.iter().position(|n| n == &name)
                                .map(|i| state.state_vars[i]).unwrap_or(0);
                            print!(" {}", v);
                        }
                        println!();
                    } else {
                        // Full slot dump (existing behavior, now usable for each target)
                        println!("=== Slot dump at tick {} ===", target);
                        print!("Registers:");
                        for (i, name) in state.state_var_names.iter().enumerate() {
                            print!(" {}={}", name, state.state_vars[i]);
                        }
                        println!();
                        let props = [
                            "--csBase", "--ipAddr", "--q0", "--q1", "--q2", "--q3",
                            "--opcode", "--mod", "--reg", "--rm",
                            "--modrmExtra", "--ea", "--eaSeg", "--eaOff",
                            "--immOff", "--immByte", "--immWord", "--imm8", "--imm16",
                            "--rmVal8", "--rmVal16", "--regVal8", "--regVal16",
                            "--memAddr", "--memVal",
                            "--memAddr0", "--memVal0", "--memAddr1", "--memVal1",
                            "--memAddr2", "--memVal2",
                            "--addrDestA", "--addrDestB", "--addrDestC",
                            "--addrValA", "--addrValA1", "--addrValA2", "--addrValB", "--addrValC",
                            "--isWordWrite", "--instId", "--instLen",
                            "--modRm", "--modRm_mod", "--modRm_reg", "--modRm_rm",
                            "--moveStack", "--moveSI", "--moveDI", "--jumpCS", "--addrJump",
                            "--AX", "--CX", "--DX", "--BX",
                            "--AL", "--CL", "--DL", "--BL",
                            "--AH", "--CH", "--DH", "--BH",
                            "--SP", "--BP", "--SI", "--DI", "--IP",
                            "--ES", "--CS", "--SS", "--DS", "--flags",
                            "--halt", "--uOp",
                        ];
                        println!("\nComputed properties:");
                        for name in &props {
                            if let Some(val) = evaluator.get_slot_value(name) {
                                println!("  {}: {} (0x{:X})", name, val, val as u32);
                            }
                        }
                    }
                }
                // --dump-mem-range pairs naturally with --dump-tick — same
                // "stop at this tick and report what's there" model. Run them
                // here too so callers can `--dump-tick=N --dump-mem-range=...`
                // without also passing --ticks (which the eval block below
                // requires).
                for spec in &cli.dump_mem_ranges {
                    if let Err(e) = dump_mem_range(spec, &state) {
                        eprintln!("--dump-mem-range={spec}: {e}");
                        std::process::exit(1);
                    }
                }
                return;
            }

            // --watch-cell: tick-by-tick monitor, print only transitions.
            if !cli.watch_cell.is_empty() {
                // Resolve each watched cell-idx to its state-var slot index up front.
                let watched: Vec<(u32, Option<usize>, i32)> = cli.watch_cell
                    .iter()
                    .map(|&idx| {
                        let name = format!("mc{}", idx);
                        let slot = state.state_var_names.iter().position(|n| n == &name);
                        let val = slot.map(|i| state.state_vars[i]).unwrap_or(0);
                        (idx, slot, val)
                    })
                    .collect();
                let mut last: Vec<i32> = watched.iter().map(|(_, _, v)| *v).collect();
                // Header: name the columns so the output is self-describing.
                print!("# watch-cell: tick");
                for (idx, _, _) in &watched { print!(" mc{}", idx); }
                println!(" | CS IP");
                // Initial values at tick 0.
                print!("0");
                for v in &last { print!(" {}", v); }
                let cs0 = state.get_var("CS").unwrap_or(0);
                let ip0 = state.get_var("IP").unwrap_or(0);
                println!(" | {} {}", cs0, ip0);

                for tick in 1..=ticks_limit {
                    for &(ev_tick, ev_val) in &key_events {
                        if ev_tick == tick { state.set_var("keyboard", ev_val); }
                    }
                    evaluator.tick(&mut state);
                    let mut any_change = false;
                    let mut now: Vec<i32> = Vec::with_capacity(watched.len());
                    for (_, slot, _) in &watched {
                        let v = slot.map(|i| state.state_vars[i]).unwrap_or(0);
                        now.push(v);
                    }
                    for i in 0..watched.len() {
                        if now[i] != last[i] { any_change = true; break; }
                    }
                    if any_change {
                        print!("{}", tick);
                        for v in &now { print!(" {}", v); }
                        let cs = state.get_var("CS").unwrap_or(0);
                        let ip = state.get_var("IP").unwrap_or(0);
                        println!(" | {} {}", cs, ip);
                        last = now;
                    }
                    if let Some(addr) = halt_addr {
                        if state.read_mem(addr) != 0 {
                            eprintln!("Halt: memory 0x{:X} set at tick {}", addr, tick);
                            break;
                        }
                    }
                }
                return;
            }

            // --trace-halt: tick-by-tick monitor that prints a ring buffer of
            // recent CPU state when haltCode goes 0 -> nonzero. Designed for
            // finding the exact instruction that branches the CPU into garbage
            // (the symptom is "haltCode=110/OUTSB" suddenly latching after
            // millions of clean ticks). Slow — runs unbatched — so cap with
            // --ticks N when looking near a known halt window.
            if cli.trace_halt {
                let names = ["CS","IP","AX","BX","CX","DX","SI","DI","SS","SP","BP","DS","ES","flags","cycleCount","haltCode","_irqActive","_tf","picPending","picMask","picInService","_irqBit","_pitFired","_kbdEdge","picVector","prevKeyboard","keyboard","pitCounter"];
                let slots: Vec<Option<usize>> = names.iter()
                    .map(|n| state.state_var_names.iter().position(|x| x == n))
                    .collect();
                let halt_idx = names.iter().position(|n| *n == "haltCode").unwrap();
                let halt_slot = slots[halt_idx];
                let cs_slot   = slots[0];
                let ip_slot   = slots[1];
                const RING: usize = 2048;
                let mut ring: Vec<(u32, Vec<i32>, u8, u8, u8, u8)> = Vec::with_capacity(RING + 4);
                // Fast-forward via run_batch if --trace-halt-skip was given.
                let skip = cli.trace_halt_skip.unwrap_or(0);
                let mut start_tick: u32 = 1;
                if skip > 0 && skip <= ticks_limit {
                    eprintln!("# trace-halt: fast-forwarding to tick {}", skip);
                    evaluator.run_batch(&mut state, skip);
                    let halt_now = halt_slot.map(|i| state.state_vars[i]).unwrap_or(0);
                    eprintln!("# trace-halt: post-skip haltCode={}", halt_now);
                    start_tick = skip + 1;
                }
                let mut prev_halt: i32 = halt_slot.map(|i| state.state_vars[i]).unwrap_or(0);
                eprintln!("# trace-halt: running tick-by-tick from {} to {}", start_tick, ticks_limit);
                for tick in start_tick..=ticks_limit {
                    for &(ev_tick, ev_val) in &key_events {
                        if ev_tick == tick { state.set_var("keyboard", ev_val); }
                    }
                    evaluator.tick(&mut state);
                    // Read each watched state-var.
                    let vals: Vec<i32> = slots.iter()
                        .map(|s| s.map(|i| state.state_vars[i]).unwrap_or(0))
                        .collect();
                    // Read 4 bytes at CS:IP for instruction context.
                    let cs = cs_slot.map(|i| state.state_vars[i]).unwrap_or(0);
                    let ip = ip_slot.map(|i| state.state_vars[i]).unwrap_or(0);
                    let lin = (cs as i32) * 16 + (ip as i32);
                    let b0 = (state.read_mem(lin)     & 0xFF) as u8;
                    let b1 = (state.read_mem(lin + 1) & 0xFF) as u8;
                    let b2 = (state.read_mem(lin + 2) & 0xFF) as u8;
                    let b3 = (state.read_mem(lin + 3) & 0xFF) as u8;
                    if ring.len() == RING { ring.remove(0); }
                    ring.push((tick, vals.clone(), b0, b1, b2, b3));
                    let halt = vals[halt_idx];
                    if prev_halt == 0 && halt != 0 {
                        eprintln!("\n=== haltCode 0 -> {} (0x{:02X}) at tick {} ===", halt, halt & 0xFF, tick);
                        eprintln!("(last {} ticks; columns: {})", ring.len(), names.join(","));
                        for (t, v, a, b, c, d) in &ring {
                            print!("tick={} ", t);
                            for (i, name) in names.iter().enumerate() {
                                print!("{}={} ", name, v[i]);
                            }
                            println!("bytes_at_csip={:02x} {:02x} {:02x} {:02x}", a, b, c, d);
                        }
                        return;
                    }
                    prev_halt = halt;
                }
                eprintln!("# trace-halt: ticks_limit reached ({}), no halt detected", ticks_limit);
                return;
            }

            // --trace-json: output JSON register trace
            if cli.trace_json {
                let halt_check = halt_addr;
                print!("[");
                for tick in 0..ticks_limit {
                    // Inject keyboard events at the right tick
                    for &(ev_tick, ev_val) in &key_events {
                        if ev_tick == tick {
                            state.set_var("keyboard", ev_val);
                        }
                    }
                    evaluator.tick(&mut state);
                    if tick > 0 { print!(","); }
                    print!("{{\"tick\":{}", tick);
                    for (i, name) in state.state_var_names.iter().enumerate() {
                        print!(",\"{}\":{}", name, state.state_vars[i]);
                    }
                    print!("}}");
                    // Halt check for trace mode too
                    if let Some(addr) = halt_check {
                        if state.read_mem(addr) != 0 {
                            break;
                        }
                    }
                }
                println!("]");
                return;
            }



            let mut ticks_run: u32 = 0;
            let mut first_frame = true;
            let screen_interval = cli.screen_interval;
            let needs_per_tick = cli.verbose || halt_addr.is_some() || (interactive && screen_interval > 0) || !key_events.is_empty();

            // Rolling speed measurement: track ticks at the last render
            let mut last_render_time = t2;
            let mut last_render_ticks: u32 = 0;

            // Enable raw terminal mode for keyboard input
            if interactive {
                crossterm::terminal::enable_raw_mode().ok();
            }

            // Keyboard model: one BDA buffer push per physical key press event.
            // The OS already fires key-press events repeatedly while a key is
            // held (Windows autorepeat), so the terminal event stream handles
            // "hold to repeat" for us. We don't simulate any additional repeat
            // inside the loop — that caused games to receive multiple moves per
            // press. A future improvement is a proper simulated autorepeat
            // (500 ms initial delay, 100 ms interval) that watches for release
            // via a last-seen timer, but the naïve version is fine for now.

            if needs_per_tick {
                // Verbose mode prints every tick so it must stay per-tick; batch
                // early and show the tail.
                let verbose_skip = if cli.verbose && ticks_limit > 100 {
                    ticks_limit.saturating_sub(20)
                } else {
                    0
                };
                if verbose_skip > 0 {
                    evaluator.run_batch(&mut state, verbose_skip);
                    ticks_run = verbose_skip;
                    let ip = state.get_var("IP").unwrap_or(0);
                    eprintln!("(batch: {} ticks, IP={})", verbose_skip, ip);
                }

                // Batch size between keyboard polls / renders / halt checks.
                // At 4.77 MHz, 50K ticks ≈ 10.5 ms — well under human perception
                // for input latency. Clamp to screen_interval so renders still
                // happen when requested.
                let base_batch = if cli.interactive_batch == 0 { 50_000 } else { cli.interactive_batch };
                let batch_cap = if screen_interval > 0 {
                    base_batch.min(screen_interval)
                } else {
                    base_batch
                }.max(1);

                // Speed throttling: pace execution to cli.speed × 4.77 MHz using
                // the accumulated cycleCount as the "sim time" clock. After each
                // batch, sleep if we've outrun the schedule. speed=0 disables.
                let throttle = cli.speed > 0.0;
                let throttle_start_wall = std::time::Instant::now();
                let throttle_start_cycles = state.get_var("cycleCount").unwrap_or(0) as u64;
                let sim_hz = REAL_8086_HZ * cli.speed;

                let mut quit = false;
                let mut tick = verbose_skip;
                while tick < ticks_limit {
                    if interactive {
                        // Poll keyboard non-blocking.
                        while event::poll(std::time::Duration::ZERO).unwrap_or(false) {
                            if let Ok(Event::Key(key_event)) = event::read() {
                                if key_event.kind == crossterm::event::KeyEventKind::Press {
                                    if key_event.modifiers.contains(KeyModifiers::CONTROL)
                                        && key_event.code == KeyCode::Char('c')
                                    {
                                        quit = true;
                                        break;
                                    }
                                    let dos_key = key_to_dos(&key_event);
                                    if dos_key != 0 {
                                        state.bda_push_key(dos_key);
                                    }
                                }
                            }
                        }
                        if quit { break; }
                    }

                    // Determine this batch's size: capped by remaining ticks and
                    // by the next scripted key_event (if any).
                    let mut batch = batch_cap.min(ticks_limit - tick);
                    for &(ev_tick, _) in &key_events {
                        if ev_tick >= tick && ev_tick < tick + batch {
                            batch = ev_tick - tick;
                            if batch == 0 { batch = 1; break; } // fire this tick immediately
                        }
                    }
                    if batch == 0 { batch = 1; }

                    // Fire any scripted keyboard events scheduled for this tick.
                    for &(ev_tick, ev_val) in &key_events {
                        if ev_tick == tick {
                            state.set_var("keyboard", ev_val);
                        }
                    }

                    // We push keys exactly once on press (in the event loop
                    // above). Re-pushing here would look like a repeat —
                    // rogue's INT 16h consumes the key each poll, so any refill
                    // registers as another keypress. Games handle their own key
                    // repeat; the CLI just delivers one press per physical key
                    // event.

                    evaluator.run_batch(&mut state, batch);
                    tick += batch;
                    ticks_run = tick;

                    if throttle {
                        let cycles_now = state.get_var("cycleCount").unwrap_or(0) as u64;
                        let sim_cycles = cycles_now.saturating_sub(throttle_start_cycles);
                        let target_wall = sim_cycles as f64 / sim_hz;
                        let actual_wall = throttle_start_wall.elapsed().as_secs_f64();
                        if target_wall > actual_wall {
                            let sleep_secs = target_wall - actual_wall;
                            std::thread::sleep(std::time::Duration::from_secs_f64(sleep_secs));
                        }
                    }

                    if cli.verbose {
                        print!("Tick {}:", tick - 1);
                        for (i, name) in state.state_var_names.iter().enumerate() {
                            print!(" {}={}", name, state.state_vars[i]);
                        }
                        println!();
                    }

                    if interactive && screen_interval > 0 && ticks_run % screen_interval == 0 {
                        let now = std::time::Instant::now();
                        let delta_secs = now.duration_since(last_render_time).as_secs_f64();
                        let delta_ticks = ticks_run - last_render_ticks;
                        render_screen(&state, ticks_run,
                                      delta_ticks, delta_secs, first_frame);
                        last_render_time = now;
                        last_render_ticks = ticks_run;
                        first_frame = false;
                    }

                    if let Some(addr) = halt_addr {
                        if state.read_mem(addr) != 0 {
                            eprintln!("Halt: memory 0x{:X} set at tick {}", addr, ticks_run);
                            break;
                        }
                    }
                }
            } else if !script_events.is_empty() {
                // Run in chunks bounded by script-event ticks. Between events
                // use run_batch for full speed.
                let mut cursor: u32 = 0;
                let mut halted = false;
                for ev in &script_events {
                    if ev.tick > ticks_limit { break; }
                    if ev.tick > cursor {
                        let batch = ev.tick - cursor;
                        evaluator.run_batch(&mut state, batch);
                        cursor = ev.tick;
                    }
                    if execute_script_action(&ev.action, &mut state, cursor) {
                        halted = true;
                        break;
                    }
                }
                if !halted && cursor < ticks_limit {
                    evaluator.run_batch(&mut state, ticks_limit - cursor);
                    cursor = ticks_limit;
                }
                ticks_run = cursor;
            } else {
                evaluator.run_batch(&mut state, ticks_limit);
                ticks_run = ticks_limit;
            }

            // Restore terminal
            if interactive {
                crossterm::terminal::disable_raw_mode().ok();
            }
            let tick_time = t2.elapsed();

            if !cli.verbose && cli.dump_tick.is_none() && !cli.trace_json {
                let ip = state.get_var("IP").unwrap_or(0);
                let cycles = state.get_var("cycleCount").unwrap_or(0) as u64;
                let elapsed = tick_time.as_secs_f64();
                let tps = if elapsed > 0.0 { ticks_run as f64 / elapsed } else { 0.0 };
                let cps = if elapsed > 0.0 { cycles as f64 / elapsed } else { 0.0 };
                let pct = cps / REAL_8086_HZ * 100.0;
                println!(
                    "{} ticks | {} cycles | {} ({:.1}% of 4.77 MHz) | {:.0} ticks/s | IP={}",
                    ticks_run,
                    cycles,
                    format_hz(cps),
                    pct,
                    tps,
                    ip,
                );
            }
            // Diagnostic: dump REP fast-forward fire/bail counts when
            // CALCITE_REP_DIAG is set. Cheap (~free) when not enabled.
            if std::env::var("CALCITE_REP_DIAG").is_ok() {
                eprint!("{}", calcite_core::compile::rep_diag_report());
            }
            eprintln!(
                "Elapsed: {:.3}s ({} ticks, {:.0} ticks/sec)",
                tick_time.as_secs_f64(),
                ticks_run,
                ticks_run as f64 / tick_time.as_secs_f64(),
            );

            // Display string property output (e.g., --textBuffer)
            for (name, value) in &state.string_properties {
                if !value.is_empty() {
                    println!("\n--{name}:\n{value}");
                }
            }

            // Final screen render (always, unless we just rendered this exact tick)
            if interactive {
                let just_rendered = screen_interval > 0 && ticks_run % screen_interval == 0;
                if !just_rendered {
                    let now = std::time::Instant::now();
                    let delta_secs = now.duration_since(last_render_time).as_secs_f64();
                    let delta_ticks = ticks_run - last_render_ticks;
                    render_screen(&state, ticks_run,
                                  delta_ticks, delta_secs, first_frame);
                }
                // Restore cursor visibility
                eprint!("\x1b[?25h");
            }

            // Write Mode 13h framebuffer to PPM if requested.
            if let Some(path) = cli.framebuffer_out.as_ref() {
                let video_mode = state.read_mem(0x0449) as u8;
                if video_mode == 0x13 {
                    let ppm = state.render_framebuffer(0xA0000, 320, 200);
                    match std::fs::write(path, &ppm) {
                        Ok(()) => eprintln!("Framebuffer: wrote {} bytes to {}", ppm.len(), path),
                        Err(e) => {
                            eprintln!("Failed to write framebuffer to {}: {}", path, e);
                            std::process::exit(1);
                        }
                    }
                } else {
                    eprintln!("--framebuffer-out: current video mode is 0x{video_mode:02X}, not Mode 13h — nothing written");
                }
            }

            // --dump-mem-range: write raw byte regions from guest memory to
            // disk so an external tool (e.g. tests/harness/fast-shoot.mjs)
            // can render screenshots without re-implementing video decode in
            // Rust or paying per-tick IPC cost through calcite-debugger.
            for spec in &cli.dump_mem_ranges {
                if let Err(e) = dump_mem_range(spec, &state) {
                    eprintln!("--dump-mem-range={spec}: {e}");
                    std::process::exit(1);
                }
            }
        }
        Err(e) => {
            eprintln!("Parse error: {e}");
            std::process::exit(1);
        }
    }
}
