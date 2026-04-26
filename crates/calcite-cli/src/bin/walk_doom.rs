//! walk_doom — fast tick walker that screenshots periodically.
//!
//! Loads a cabinet, runs ticks in chunks via run_batch, dumps state
//! and framebuffer at each landmark. Stops on cycle stall or max-ticks.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;
use std::io::Write;

use clap::Parser;

// Encode RGBA pixels into a minimal PNG (truecolor + alpha, no compression filter beyond zlib stored).
fn encode_png(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    use std::io::Write;
    fn crc32(data: &[u8]) -> u32 {
        // Polynomial 0xEDB88320, table-less.
        let mut crc: u32 = 0xFFFFFFFF;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xEDB88320 & ((crc & 1).wrapping_neg()));
            }
        }
        !crc
    }
    fn adler32(data: &[u8]) -> u32 {
        let mut a: u32 = 1;
        let mut b: u32 = 0;
        for &x in data {
            a = (a + x as u32) % 65521;
            b = (b + a) % 65521;
        }
        (b << 16) | a
    }
    fn write_chunk(buf: &mut Vec<u8>, ty: &[u8; 4], data: &[u8]) {
        buf.extend(&(data.len() as u32).to_be_bytes());
        let start = buf.len();
        buf.extend(ty);
        buf.extend(data);
        let crc = crc32(&buf[start..]);
        buf.extend(&crc.to_be_bytes());
    }
    let mut png = vec![137,80,78,71,13,10,26,10];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend(&w.to_be_bytes());
    ihdr.extend(&h.to_be_bytes());
    ihdr.extend(&[8u8, 6u8, 0u8, 0u8, 0u8]); // bit depth=8, color type 6 (RGBA), no interlace
    write_chunk(&mut png, b"IHDR", &ihdr);

    // Build raw image data with filter byte 0 per row.
    let row_bytes = w as usize * 4;
    let mut raw = Vec::with_capacity((row_bytes + 1) * h as usize);
    for y in 0..h as usize {
        raw.push(0u8);
        raw.extend(&rgba[y * row_bytes .. (y + 1) * row_bytes]);
    }

    // Wrap raw into a "stored" zlib stream (no compression).
    let mut zlib = Vec::with_capacity(raw.len() + 16);
    zlib.push(0x78); zlib.push(0x01); // zlib header (no compression preset)
    let mut i = 0usize;
    while i < raw.len() {
        let n = (raw.len() - i).min(65535);
        let last = (i + n) == raw.len();
        zlib.push(if last { 1 } else { 0 });
        zlib.extend(&(n as u16).to_le_bytes());
        let nlen = !(n as u16);
        zlib.extend(&nlen.to_le_bytes());
        zlib.extend(&raw[i..i + n]);
        i += n;
    }
    zlib.extend(&adler32(&raw).to_be_bytes());
    write_chunk(&mut png, b"IDAT", &zlib);
    write_chunk(&mut png, b"IEND", &[]);
    let _ = std::io::sink().write_all(&[]); // silence unused import in some builds
    png
}

#[derive(Parser, Debug)]
#[command(name = "walk-doom", about = "Walk through doom boot, screenshot at landmarks")]
struct Args {
    #[arg(short, long)]
    input: PathBuf,

    #[arg(long, default_value = "10000000")]
    max_ticks: u32,

    #[arg(long, default_value = "100000")]
    chunk: u32,

    /// Switch to fine-grained chunking after this tick. Useful to fast-forward
    /// then trace 1-tick steps near a boundary.
    #[arg(long)]
    fine_after: Option<u32>,

    /// Fine chunk size to use after `fine_after` tick.
    #[arg(long, default_value = "1")]
    fine_chunk: u32,

    /// Code segment to dump bytes at (hex). Dumps 32 bytes around CS:IP each report.
    #[arg(long)]
    dump_code: bool,

    /// If set, write a PNG screenshot per chunk based on current video mode.
    #[arg(long)]
    screenshot: bool,

    #[arg(long, default_value = "/tmp/doom-walk")]
    out_dir: PathBuf,

    /// Inject keyboard events at specific ticks.
    /// Format: TICK:VALUE,TICK:VALUE,... VALUE is (scancode<<8)|ascii
    #[arg(long)]
    key_events: Option<String>,

    /// Inject keyboard event when a specific tick *condition* is detected.
    /// Format: VAR=VALUE:KEY (e.g. cycleCount=14650005:0x1C0D)
    #[arg(long)]
    key_on_stall: Option<String>,
}

fn parse_key_events(s: &str) -> Vec<(u32, i32)> {
    s.split(',')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (t, v) = p.split_once(':').expect("TICK:VALUE");
            let tick: u32 = t.parse().expect("tick");
            let val: i32 = if v.starts_with("0x") || v.starts_with("0X") {
                i32::from_str_radix(&v[2..], 16).expect("hex")
            } else {
                v.parse().expect("dec")
            };
            (tick, val)
        })
        .collect()
}

fn main() {
    env_logger::init();
    let args = Args::parse();

    std::fs::create_dir_all(&args.out_dir).expect("mkdir");

    eprintln!("Loading {}", args.input.display());
    let css = std::fs::read_to_string(&args.input).expect("read css");
    let t0 = Instant::now();
    let parsed = calcite_core::parser::parse_css(&css).expect("parse");
    let mut state = calcite_core::State::default();
    state.load_properties(&parsed.properties);
    let mut evaluator = calcite_core::Evaluator::from_parsed(&parsed);
    evaluator.wire_state_for_packed_memory(&mut state);
    eprintln!("Parsed/compiled in {:.2}s", t0.elapsed().as_secs_f64());

    let key_events = args
        .key_events
        .as_deref()
        .map(parse_key_events)
        .unwrap_or_default();

    let mut summary: Vec<HashMap<&str, String>> = Vec::new();

    let mut tick: u32 = 0;
    let mut last_cycles: i32 = 0;
    let mut stall_count: u32 = 0;
    let mut stall_fired_keys: bool = false;
    let parse_stall_key: Option<i32> = args.key_on_stall.as_ref().and_then(|s| {
        // Just take the part after :
        s.split(':').nth(1).and_then(|v| {
            if v.starts_with("0x") {
                i32::from_str_radix(&v[2..], 16).ok()
            } else {
                v.parse().ok()
            }
        })
    });

    let t_run = Instant::now();
    while tick < args.max_ticks {
        // Apply scheduled key events that fall in [tick, tick+1)
        let active_chunk = if let Some(fine) = args.fine_after {
            if tick >= fine {
                args.fine_chunk
            } else if tick + args.chunk > fine {
                // Clip to land exactly on fine_after.
                fine - tick
            } else {
                args.chunk
            }
        } else { args.chunk };
        let chunk = active_chunk.min(args.max_ticks - tick).max(1);
        // Fire any key events for this tick window
        for &(et, ev) in &key_events {
            if et >= tick && et < tick + chunk {
                eprintln!("  injecting key 0x{:04X} at tick {}", ev, et);
                state.bda_push_key(ev);
            }
        }
        evaluator.run_batch(&mut state, chunk);
        tick += chunk;

        let cs = state.get_var("CS").unwrap_or(0);
        let ip = state.get_var("IP").unwrap_or(0);
        let ds = state.get_var("DS").unwrap_or(0);
        let es = state.get_var("ES").unwrap_or(0);
        let ss = state.get_var("SS").unwrap_or(0);
        let sp = state.get_var("SP").unwrap_or(0);
        let bp = state.get_var("BP").unwrap_or(0);
        let ax = state.get_var("AX").unwrap_or(0);
        let bx = state.get_var("BX").unwrap_or(0);
        let cx = state.get_var("CX").unwrap_or(0);
        let dx = state.get_var("DX").unwrap_or(0);
        let si = state.get_var("SI").unwrap_or(0);
        let di = state.get_var("DI").unwrap_or(0);
        let cycles = state.get_var("cycleCount").unwrap_or(0);
        let halt = state.get_var("halt").unwrap_or(0);
        let video_mode = state.read_mem(0x0449) as u8;

        eprintln!(
            "tick={:>8} cy={:>10} CS:IP={:04X}:{:04X} SS:SP={:04X}:{:04X} DS={:04X} ES={:04X} BP={:04X} AX={:04X} BX={:04X} CX={:04X} DX={:04X} SI={:04X} DI={:04X} mode=0x{:02X} {}",
            tick, cycles, cs as u16, ip as u16, ss as u16, sp as u16, ds as u16, es as u16, bp as u16, ax as u16, bx as u16, cx as u16, dx as u16, si as u16, di as u16, video_mode,
            if cycles == last_cycles { "STALL" } else { "" }
        );

        if args.screenshot {
            // Dump appropriate framebuffer for current mode.
            if video_mode == 0x13 {
                // Mode 13h: 320x200x256 at 0xA0000, palette at DAC linear 0x100000.
                let mut rgba = vec![0u8; 320 * 200 * 4];
                let mut dac = vec![0u8; 768];
                for i in 0..768 {
                    dac[i] = state.read_mem(0x100000 + i as i32) as u8;
                }
                for i in 0..(320 * 200) {
                    let c = state.read_mem(0xA0000 + i as i32) as u8 as usize;
                    let r6 = dac[c * 3] & 0x3F;
                    let g6 = dac[c * 3 + 1] & 0x3F;
                    let b6 = dac[c * 3 + 2] & 0x3F;
                    rgba[i * 4] = (r6 << 2) | (r6 >> 4);
                    rgba[i * 4 + 1] = (g6 << 2) | (g6 >> 4);
                    rgba[i * 4 + 2] = (b6 << 2) | (b6 >> 4);
                    rgba[i * 4 + 3] = 0xFF;
                }
                let png = encode_png(320, 200, &rgba);
                let path = args.out_dir.join(format!("shot-tick-{:08}.png", tick));
                let _ = std::fs::write(path, png);
            } else if video_mode == 0x03 || video_mode == 0x02 {
                // Text mode: dump as a simple 80x25 ASCII string.
                let mut text = String::new();
                for y in 0..25 {
                    for x in 0..80 {
                        let off = (y * 80 + x) * 2;
                        let c = state.read_mem(0xB8000 + off as i32) as u8;
                        text.push(if c >= 0x20 && c < 0x7F { c as char } else if c == 0 { ' ' } else { '.' });
                    }
                    text.push('\n');
                }
                let path = args.out_dir.join(format!("shot-tick-{:08}.txt", tick));
                let _ = std::fs::write(path, text);
            }
        }

        if args.dump_code {
            let cs_base = (cs as i64) * 16;
            let ip_lin = cs_base + ip as i64;
            let mut codebytes = String::new();
            for off in -4i64..=20 {
                let addr = (ip_lin + off) as i32;
                if addr >= 0 {
                    codebytes += &format!("{:02X} ", state.read_mem(addr) & 0xFF);
                }
            }
            eprintln!("    code @ CS:IP-4 .. CS:IP+20: {}", codebytes);
            // Stack peek (regular)
            let ss_base = (ss as i64) * 16;
            let sp_lin = ss_base + sp as i64;
            let mut stbytes = String::new();
            for off in 0i64..16 {
                let addr = (sp_lin + off) as i32;
                if addr >= 0 {
                    stbytes += &format!("{:02X} ", state.read_mem(addr) & 0xFF);
                }
            }
            eprintln!("    stack @ SS:SP .. +16: {}", stbytes);
            // Wrap-aware stack peek: if SP is negative (kiln bug), also show
            // memory at SS*16 + (SP & 0xFFFF) so we can see if INT writes
            // landed at the wrap-aware address.
            let sp_u16 = (sp as i32) & 0xFFFF;
            if sp_u16 != sp as i32 {
                let sp_lin_u16 = ss_base + sp_u16 as i64;
                let mut wrapbytes = String::new();
                for off in -4i64..16 {
                    let addr = (sp_lin_u16 + off) as i32;
                    if addr >= 0 {
                        wrapbytes += &format!("{:02X} ", state.read_mem(addr) & 0xFF);
                    }
                }
                eprintln!("    stack @ SS:(SP&0xFFFF)-4 .. +16: {}", wrapbytes);
            }
            // Also dump memory at the suspected INT-wrap area: linear SS*16-32 .. SS*16+16
            let mut wraparound = String::new();
            for off in -32i64..16 {
                let addr = (ss_base + off) as i32;
                if addr >= 0 {
                    wraparound += &format!("{:02X} ", state.read_mem(addr) & 0xFF);
                }
            }
            eprintln!("    mem @ SS*16-32 .. +16 (linear {:X}..{:X}): {}",
                (ss_base - 32).max(0), ss_base + 16, wraparound);
        }

        // Dump state landmarks
        let mut entry = HashMap::new();
        entry.insert("tick", tick.to_string());
        entry.insert("cycles", cycles.to_string());
        entry.insert("cs", format!("{:04X}", cs));
        entry.insert("ip", format!("{:04X}", ip));
        entry.insert("mode", format!("0x{:02X}", video_mode));
        entry.insert("halt", halt.to_string());
        summary.push(entry);

        // Save framebuffer snapshot
        let snap_path = args.out_dir.join(format!("snap-tick-{:08}.bin", tick));
        let mut f = std::fs::File::create(&snap_path).expect("create snap");
        // Header: 16 bytes [magic 4][tick 4][cycles 4][cs 2 ip 2]
        f.write_all(b"WLKD").unwrap();
        f.write_all(&tick.to_le_bytes()).unwrap();
        f.write_all(&(cycles as u32).to_le_bytes()).unwrap();
        f.write_all(&(cs as u16).to_le_bytes()).unwrap();
        f.write_all(&(ip as u16).to_le_bytes()).unwrap();
        // Mode byte + 3 pad
        f.write_all(&[video_mode, halt as u8, 0, 0]).unwrap();
        // Text VRAM 8KB
        let mut tvram = vec![0u8; 8192];
        for i in 0..8192 {
            tvram[i] = state.read_mem(0xB8000 + i as i32) as u8;
        }
        f.write_all(&tvram).unwrap();
        // Mode13 VRAM 64KB
        let mut gvram = vec![0u8; 320 * 200];
        for i in 0..(320 * 200) {
            gvram[i] = state.read_mem(0xA0000 + i as i32) as u8;
        }
        f.write_all(&gvram).unwrap();
        // DAC palette 768 bytes
        let mut dac = vec![0u8; 768];
        for i in 0..768 {
            dac[i] = state.read_mem(0x100000 + i as i32) as u8;
        }
        f.write_all(&dac).unwrap();
        drop(f);

        // Stall detection
        if cycles == last_cycles {
            stall_count += 1;
            if stall_count >= 3 {
                eprintln!("--- CYCLES STALLED for 3 chunks ({} cycles unchanged) ---", cycles);
                if let Some(k) = parse_stall_key {
                    if !stall_fired_keys {
                        eprintln!("  injecting key 0x{:04X} on stall", k);
                        state.bda_push_key(k);
                        stall_fired_keys = true;
                        stall_count = 0;
                        // Continue running
                        continue;
                    }
                }
                break;
            }
        } else {
            stall_count = 0;
            stall_fired_keys = false;
        }
        last_cycles = cycles;
    }

    let elapsed = t_run.elapsed().as_secs_f64();
    eprintln!(
        "\nElapsed: {:.2}s, {:.0} ticks/sec",
        elapsed, tick as f64 / elapsed
    );

    // Write summary
    let sum_path = args.out_dir.join("summary.json");
    let mut sum_f = std::fs::File::create(&sum_path).expect("create summary");
    writeln!(sum_f, "[").unwrap();
    for (i, entry) in summary.iter().enumerate() {
        write!(sum_f, "  {{").unwrap();
        let mut first = true;
        for (k, v) in entry {
            if !first { write!(sum_f, ", ").unwrap(); }
            write!(sum_f, "\"{}\": \"{}\"", k, v).unwrap();
            first = false;
        }
        write!(sum_f, "}}").unwrap();
        if i + 1 < summary.len() { write!(sum_f, ",").unwrap(); }
        writeln!(sum_f).unwrap();
    }
    writeln!(sum_f, "]").unwrap();

    eprintln!("Summary written to {}", sum_path.display());
    eprintln!("Snapshots in {}", args.out_dir.display());
}
