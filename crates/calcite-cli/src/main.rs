use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use std::path::PathBuf;

/// calc(ite) — JIT compiler for computational CSS.
///
/// Parses CSS files, recognises computational patterns, and evaluates
/// them efficiently. Primary target: running x86CSS faster than Chrome.
#[derive(Parser, Debug)]
#[command(name = "calcite", version, about)]
struct Cli {
    /// Path to the CSS file to evaluate.
    #[arg(short, long)]
    input: PathBuf,

    /// Number of ticks to run.
    #[arg(short = 'n', long, default_value = "1")]
    ticks: u32,

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

    /// Dump all computed slot values at a specific tick for debugging.
    ///
    /// Runs to the specified tick, then prints every property name and its
    /// computed value. Useful for diagnosing divergences found by compare.mjs.
    #[arg(long, value_name = "TICK")]
    dump_tick: Option<u32>,

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

fn main() {
    env_logger::init();
    let cli = Cli::parse();

    let css = match std::fs::read_to_string(&cli.input) {
        Ok(content) => content,
        Err(e) => {
            eprintln!("Error reading {}: {e}", cli.input.display());
            std::process::exit(1);
        }
    };

    log::info!(
        "Read {} bytes of CSS from {}",
        css.len(),
        cli.input.display()
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

            let t1 = std::time::Instant::now();
            let mut evaluator = calcite_core::Evaluator::from_parsed(&parsed);
            let compile_time = t1.elapsed();

            let mut state = calcite_core::State::default();
            state.load_properties(&parsed.properties);

            let interactive = cli.ticks > 1;
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
                } else {
                    s.parse().expect("Invalid halt address")
                }
            });

            let t2 = std::time::Instant::now();

            // Render the screen based on BDA video mode (0x0449).
            // Text modes: render in-terminal using ANSI. Mode 13h: status line.
            let render_screen = |state: &calcite_core::State, tick: u32, first: bool| {
                let video_mode = state.read_mem(0x0449) as u8;
                if video_mode == 0x13 {
                    // Mode 13h — can't draw pixels in a terminal.
                    // Just show a status line so the user knows it's running.
                    if first {
                        eprint!("\x1b[2J\x1b[H\x1b[?25l");
                    } else {
                        eprint!("\x1b[H");
                    }
                    eprint!("[Mode 13h 320x200 graphics]\r\n tick {tick}\r\n");
                } else {
                    // Text mode: infer dimensions from BDA columns byte.
                    let cols = state.read_mem(0x044A) as usize;
                    let (width, height) = if cols == 40 { (40, 25) } else { (80, 25) };
                    if first {
                        eprint!("\x1b[2J\x1b[H\x1b[?25l");
                    } else {
                        eprint!("\x1b[H");
                    }
                    let screen = state.render_screen(0xB8000, width, height);
                    eprint!("┌{}┐\r\n", "─".repeat(width));
                    for line in screen.lines() {
                        eprint!("│{line}│\r\n");
                    }
                    eprint!("└{}┘\r\n", "─".repeat(width));
                    eprint!(" tick {tick} \r\n");
                }
            };

            // --dump-tick: run to specific tick and dump all computed properties
            if let Some(target_tick) = cli.dump_tick {
                if target_tick > 0 {
                    evaluator.run_batch(&mut state, target_tick);
                }
                evaluator.tick(&mut state);
                println!("=== Slot dump at tick {} ===", target_tick);
                println!(
                    "Registers: AX={} CX={} DX={} BX={} SP={} BP={} SI={} DI={} IP={} ES={} CS={} SS={} DS={} FLAGS={}",
                    state.registers[calcite_core::state::reg::AX],
                    state.registers[calcite_core::state::reg::CX],
                    state.registers[calcite_core::state::reg::DX],
                    state.registers[calcite_core::state::reg::BX],
                    state.registers[calcite_core::state::reg::SP],
                    state.registers[calcite_core::state::reg::BP],
                    state.registers[calcite_core::state::reg::SI],
                    state.registers[calcite_core::state::reg::DI],
                    state.registers[calcite_core::state::reg::IP],
                    state.registers[calcite_core::state::reg::ES],
                    state.registers[calcite_core::state::reg::CS],
                    state.registers[calcite_core::state::reg::SS],
                    state.registers[calcite_core::state::reg::DS],
                    state.registers[calcite_core::state::reg::FLAGS],
                );
                // Dump key computed properties
                let props = [
                    // v2 decode properties
                    "--csBase", "--ipAddr", "--q0", "--q1", "--q2", "--q3",
                    "--opcode", "--mod", "--reg", "--rm",
                    "--modrmExtra", "--ea", "--eaSeg", "--eaOff",
                    "--immOff", "--immByte", "--immWord", "--imm8", "--imm16",
                    "--rmVal8", "--rmVal16", "--regVal8", "--regVal16",
                    "--memAddr0", "--memVal0", "--memAddr1", "--memVal1",
                    "--memAddr2", "--memVal2",
                    // v1 legacy properties
                    "--addrDestA", "--addrDestB", "--addrDestC",
                    "--addrValA", "--addrValA1", "--addrValA2", "--addrValB", "--addrValC",
                    "--isWordWrite", "--instId", "--instLen",
                    "--modRm", "--modRm_mod", "--modRm_reg", "--modRm_rm",
                    "--moveStack", "--moveSI", "--moveDI", "--jumpCS", "--addrJump",
                    // registers
                    "--AX", "--CX", "--DX", "--BX",
                    "--AL", "--CL", "--DL", "--BL",
                    "--AH", "--CH", "--DH", "--BH",
                    "--SP", "--BP", "--SI", "--DI", "--IP",
                    "--ES", "--CS", "--SS", "--DS", "--flags", "--halt",
                ];
                println!("\nComputed properties:");
                for name in &props {
                    if let Some(val) = evaluator.get_slot_value(name) {
                        println!("  {}: {} (0x{:X})", name, val, val as u32);
                    }
                }
                return;
            }

            // --trace-json: output JSON register trace
            if cli.trace_json {
                print!("[");
                for tick in 0..cli.ticks {
                    evaluator.tick(&mut state);
                    if tick > 0 { print!(","); }
                    print!(
                        "{{\"tick\":{},\"AX\":{},\"CX\":{},\"DX\":{},\"BX\":{},\"SP\":{},\"BP\":{},\"SI\":{},\"DI\":{},\"IP\":{},\"ES\":{},\"CS\":{},\"SS\":{},\"DS\":{},\"FLAGS\":{}}}",
                        tick,
                        state.registers[calcite_core::state::reg::AX],
                        state.registers[calcite_core::state::reg::CX],
                        state.registers[calcite_core::state::reg::DX],
                        state.registers[calcite_core::state::reg::BX],
                        state.registers[calcite_core::state::reg::SP],
                        state.registers[calcite_core::state::reg::BP],
                        state.registers[calcite_core::state::reg::SI],
                        state.registers[calcite_core::state::reg::DI],
                        state.registers[calcite_core::state::reg::IP],
                        state.registers[calcite_core::state::reg::ES],
                        state.registers[calcite_core::state::reg::CS],
                        state.registers[calcite_core::state::reg::SS],
                        state.registers[calcite_core::state::reg::DS],
                        state.registers[calcite_core::state::reg::FLAGS],
                    );
                }
                println!("]");
                return;
            }



            let mut ticks_run: u32 = 0;
            let mut first_frame = true;
            let screen_interval = cli.screen_interval;
            let needs_per_tick = cli.verbose || halt_addr.is_some() || (interactive && screen_interval > 0);

            // Enable raw terminal mode for keyboard input
            if interactive {
                crossterm::terminal::enable_raw_mode().ok();
            }

            // Keyboard state: hold each key for at least KEY_HOLD_TICKS so the
            // CPU (which takes many ticks per instruction) has time to read it.
            const KEY_HOLD_TICKS: u32 = 50;
            let mut key_hold_remaining: u32 = 0;

            if needs_per_tick {
                // When verbose, batch early ticks then show tail
                let batch_skip = if cli.verbose && cli.ticks > 100 {
                    cli.ticks.saturating_sub(20)
                } else {
                    0
                };
                if batch_skip > 0 {
                    evaluator.run_batch(&mut state, batch_skip);
                    ticks_run = batch_skip;
                    if cli.verbose {
                        eprintln!(
                            "(batch: {} ticks, IP={})",
                            batch_skip,
                            state.registers[calcite_core::state::reg::IP]
                        );
                    }
                }
                let mut quit = false;
                for tick in batch_skip..cli.ticks {
                    // Poll keyboard (non-blocking) in interactive mode
                    if interactive {
                        // Hold timer: rate-limits key repeat by ignoring new presses
                        // for KEY_HOLD_TICKS after each keypress.
                        if key_hold_remaining > 0 {
                            key_hold_remaining -= 1;
                        }

                        while event::poll(std::time::Duration::ZERO).unwrap_or(false) {
                            match event::read() {
                                Ok(Event::Key(key_event)) => {
                                    if key_event.kind == crossterm::event::KeyEventKind::Press {
                                        // Ctrl+C to quit
                                        if key_event.modifiers.contains(KeyModifiers::CONTROL)
                                            && key_event.code == KeyCode::Char('c')
                                        {
                                            quit = true;
                                            break;
                                        }
                                        let dos_key = key_to_dos(&key_event);
                                        if dos_key != 0 && key_hold_remaining == 0 {
                                            // Write into the BDA keyboard ring buffer.
                                            // The BIOS INT 16h handler pops it — no host-side
                                            // clearing needed (matches real 8042 behaviour).
                                            let tail      = state.read_mem16(0x41C);
                                            let buf_end   = state.read_mem16(0x482);
                                            let buf_start = state.read_mem16(0x480);
                                            let linear    = 0x400 + tail;
                                            state.write_mem(linear,     dos_key & 0xFF);
                                            state.write_mem(linear + 1, (dos_key >> 8) & 0xFF);
                                            let mut new_tail = tail + 2;
                                            if new_tail >= buf_end { new_tail = buf_start; }
                                            let head = state.read_mem16(0x41A);
                                            if new_tail != head {
                                                state.write_mem(0x41C, new_tail & 0xFF);
                                                state.write_mem(0x41D, (new_tail >> 8) & 0xFF);
                                            }
                                            key_hold_remaining = KEY_HOLD_TICKS;
                                        }
                                    }
                                    // Release events are ignored — we use the hold timer instead
                                }
                                _ => {}
                            }
                        }
                        if quit { break; }
                    }

                    evaluator.tick(&mut state);
                    ticks_run = tick + 1;

                    if cli.verbose {
                        println!(
                            "Tick {tick}: AX={} CX={} DX={} BX={} SP={} BP={} SI={} DI={} IP={} ES={} CS={} SS={} DS={} flags={}",
                            state.registers[calcite_core::state::reg::AX],
                            state.registers[calcite_core::state::reg::CX],
                            state.registers[calcite_core::state::reg::DX],
                            state.registers[calcite_core::state::reg::BX],
                            state.registers[calcite_core::state::reg::SP],
                            state.registers[calcite_core::state::reg::BP],
                            state.registers[calcite_core::state::reg::SI],
                            state.registers[calcite_core::state::reg::DI],
                            state.registers[calcite_core::state::reg::IP],
                            state.registers[calcite_core::state::reg::ES],
                            state.registers[calcite_core::state::reg::CS],
                            state.registers[calcite_core::state::reg::SS],
                            state.registers[calcite_core::state::reg::DS],
                            state.registers[calcite_core::state::reg::FLAGS],
                        );
                    }

                    // Periodic screen rendering
                    if interactive && screen_interval > 0 && ticks_run % screen_interval == 0 {
                        render_screen(&state, ticks_run, first_frame);
                        first_frame = false;
                    }

                    if let Some(addr) = halt_addr {
                        if state.read_mem(addr) != 0 {
                            eprintln!("Halt: memory 0x{:X} set at tick {}", addr, ticks_run);
                            break;
                        }
                    }
                }
            } else {
                evaluator.run_batch(&mut state, cli.ticks);
                ticks_run = cli.ticks;
            }

            // Restore terminal
            if interactive {
                crossterm::terminal::disable_raw_mode().ok();
            }
            let tick_time = t2.elapsed();

            if !cli.verbose && cli.dump_tick.is_none() && !cli.trace_json {
                println!(
                    "Ran {} ticks | AX={} CX={} IP={}",
                    ticks_run,
                    state.registers[calcite_core::state::reg::AX],
                    state.registers[calcite_core::state::reg::CX],
                    state.registers[calcite_core::state::reg::IP],
                );
            }
            eprintln!(
                "Ticks: {:.3}s ({:.0} ticks/sec)",
                tick_time.as_secs_f64(),
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
                    render_screen(&state, ticks_run, first_frame);
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
        }
        Err(e) => {
            eprintln!("Parse error: {e}");
            std::process::exit(1);
        }
    }
}
