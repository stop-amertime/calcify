use calcite_core::State;
use calcite_core::Evaluator;
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Mutex;
use tiny_http::{Header, Method, Response, Server};

/// calcite-debugger — HTTP debug server for CSS execution.
///
/// Parses a CSS file once, then serves an HTTP API for stepping through
/// execution tick by tick, inspecting state, comparing traces, etc.
#[derive(Parser, Debug)]
#[command(name = "calcite-debugger", version, about)]
struct Cli {
    /// Path to the CSS file to debug.
    #[arg(short, long)]
    input: PathBuf,

    /// Port to listen on.
    #[arg(short, long, default_value = "3333")]
    port: u16,

    /// Snapshot interval (ticks between automatic checkpoints).
    #[arg(long, default_value = "1000")]
    snapshot_interval: u32,
}

// ---------------------------------------------------------------------------
// JSON request/response types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TickRequest {
    count: Option<u32>,
}

#[derive(Deserialize)]
struct SeekRequest {
    tick: u32,
}

#[derive(Deserialize)]
struct MemoryRequest {
    addr: i32,
    len: Option<usize>,
}

#[derive(Deserialize)]
struct ScreenRequest {
    addr: Option<i32>,
    width: Option<usize>,
    height: Option<usize>,
}

#[derive(Deserialize)]
struct CompareRequest {
    reference: Vec<RefTick>,
    stop_at_first: Option<bool>,
}

#[derive(Deserialize)]
struct KeyRequest {
    /// Packed DOS key: (scancode << 8) | ascii. 0 clears the buffer.
    value: i32,
}

#[derive(Deserialize)]
struct CompareStateRequest {
    /// Register values to diff against, keyed by name ("AX", "IP", ...).
    registers: Option<HashMap<String, i64>>,
    /// Memory ranges to diff. Each entry is {addr, len, bytes}.
    memory: Option<Vec<CompareMemEntry>>,
}

#[derive(Deserialize)]
struct CompareMemEntry {
    addr: i32,
    len: usize,
    /// Expected bytes (from the reference side) as an array of u8 values.
    bytes: Vec<u8>,
}

#[derive(Serialize)]
struct CompareStateResponse {
    tick: u32,
    register_diffs: Vec<DiffEntry>,
    memory_diffs: Vec<MemDiffEntry>,
    total_diffs: usize,
}

#[derive(Serialize)]
struct MemDiffEntry {
    addr: i32,
    expected: u8,
    actual: u8,
}

#[derive(Deserialize, Clone)]
struct RefTick {
    tick: u32,
    #[serde(flatten)]
    registers: HashMap<String, serde_json::Value>,
}

#[derive(Serialize)]
struct StateResponse {
    tick: u32,
    registers: RegisterState,
    properties: HashMap<String, i64>,
}

#[derive(Serialize)]
struct RegisterState(BTreeMap<String, i32>);

#[derive(Serialize)]
struct MemoryResponse {
    addr: i32,
    len: usize,
    /// Hex dump of memory bytes.
    hex: String,
    /// Raw byte values.
    bytes: Vec<u8>,
    /// 16-bit words (little-endian) for convenience.
    words: Vec<u16>,
}

#[derive(Serialize)]
struct ScreenResponse {
    addr: i32,
    width: usize,
    height: usize,
    text: String,
}

#[derive(Serialize)]
struct DivergenceInfo {
    tick: u32,
    register: String,
    expected: i64,
    actual: i64,
}

#[derive(Serialize)]
struct CompareResponse {
    divergences: Vec<DivergenceInfo>,
    ticks_compared: u32,
}

#[derive(Serialize)]
struct DiffEntry {
    property: String,
    compiled: i64,
    interpreted: i64,
}

#[derive(Serialize)]
struct ComparePathsResponse {
    tick: u32,
    register_diffs: Vec<DiffEntry>,
    property_diffs: Vec<DiffEntry>,
    memory_diffs: Vec<DiffEntry>,
    total_diffs: usize,
}

#[derive(Serialize)]
struct TickResponse {
    tick: u32,
    ticks_executed: u32,
    changes: Vec<(String, String)>,
}

#[derive(Deserialize)]
struct TracePropertyRequest {
    /// CSS property name to trace (e.g. "--memAddr")
    property: String,
}

#[derive(Serialize)]
struct TracePropertyResponse {
    tick: u32,
    property: String,
    compiled_value: i32,
    interpreted_value: Option<i32>,
    trace_entries: usize,
    trace: Vec<calcite_core::compile::TraceEntry>,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

#[derive(Serialize)]
struct InfoResponse {
    css_file: String,
    current_tick: u32,
    properties_count: usize,
    functions_count: usize,
    assignments_count: usize,
    snapshots: Vec<u32>,
    endpoints: Vec<&'static str>,
}

// ---------------------------------------------------------------------------
// Debug session — holds all mutable state behind a Mutex
// ---------------------------------------------------------------------------

struct DebugSession {
    evaluator: Evaluator,
    state: State,
    /// Evaluator is immutable between ticks — only State needs snapshots.
    snapshots: Vec<(u32, State)>,
    snapshot_interval: u32,
    /// Property counts from parsed program (for info).
    properties_count: usize,
    functions_count: usize,
    assignments_count: usize,
    css_file: String,
    /// Video memory config if detected.
    video_config: Option<(usize, usize)>,
    /// All property names known to the compiled program.
    property_names: Vec<String>,
    /// The parsed program, kept for creating interpreted-path evaluators.
    parsed_program: calcite_core::types::ParsedProgram,
}

impl DebugSession {
    fn current_tick(&self) -> u32 {
        self.state.frame_counter
    }

    fn registers(&self) -> RegisterState {
        let mut map = BTreeMap::new();
        for (i, name) in self.state.state_var_names.iter().enumerate() {
            if i < self.state.state_vars.len() {
                map.insert(name.clone(), self.state.state_vars[i]);
            }
        }
        RegisterState(map)
    }

    fn get_properties(&self) -> HashMap<String, i64> {
        let mut props = HashMap::new();
        for name in &self.property_names {
            if let Some(val) = self.evaluator.get_slot_value(name) {
                props.insert(name.clone(), val as i64);
            }
        }
        props
    }

    fn tick(&mut self, count: u32) -> TickResponse {
        let start_tick = self.current_tick();
        let mut all_changes = Vec::new();

        for _ in 0..count {
            // Auto-snapshot at interval boundaries
            if self.snapshot_interval > 0
                && self.current_tick() > 0
                && self.current_tick() % self.snapshot_interval == 0
            {
                let tick = self.current_tick();
                if !self.snapshots.iter().any(|(t, _)| *t == tick) {
                    self.snapshots.push((tick, self.state.clone()));
                }
            }
            let result = self.evaluator.tick(&mut self.state);
            all_changes.extend(result.changes);
        }

        TickResponse {
            tick: self.current_tick(),
            ticks_executed: self.current_tick() - start_tick,
            changes: all_changes,
        }
    }

    fn seek(&mut self, target_tick: u32) {
        // Find nearest snapshot at or before target
        let mut best: Option<(u32, &State)> = None;
        for (tick, snap) in &self.snapshots {
            if *tick <= target_tick {
                if best.is_none() || *tick > best.unwrap().0 {
                    best = Some((*tick, snap));
                }
            }
        }

        if let Some((snap_tick, snap_state)) = best {
            if snap_tick > self.current_tick() || self.current_tick() > target_tick {
                // Restore from snapshot
                self.state = snap_state.clone();
            }
        } else if self.current_tick() > target_tick {
            // No useful snapshot — reset to tick 0
            self.state = State::default();
            self.state.load_properties(
                &self.parsed_program.properties,
            );
        }

        // Run forward to target
        while self.current_tick() < target_tick {
            // Auto-snapshot at interval boundaries
            if self.snapshot_interval > 0
                && self.current_tick() > 0
                && self.current_tick() % self.snapshot_interval == 0
            {
                let tick = self.current_tick();
                if !self.snapshots.iter().any(|(t, _)| *t == tick) {
                    self.snapshots.push((tick, self.state.clone()));
                }
            }
            self.evaluator.tick(&mut self.state);
        }
    }

    fn watchpoint(&mut self, addr: i32, max_ticks: u32, expected: Option<i32>) -> serde_json::Value {
        let initial = self.state.read_mem(addr);
        let start_tick = self.current_tick();
        let t0 = std::time::Instant::now();

        for _ in 0..max_ticks {
            // Auto-snapshot at interval boundaries
            if self.snapshot_interval > 0
                && self.current_tick() > 0
                && self.current_tick() % self.snapshot_interval == 0
            {
                let tick = self.current_tick();
                if !self.snapshots.iter().any(|(t, _)| *t == tick) {
                    self.snapshots.push((tick, self.state.clone()));
                }
            }
            self.evaluator.tick(&mut self.state);
            let current = self.state.read_mem(addr);
            let matched = if let Some(exp) = expected {
                current == exp
            } else {
                current != initial
            };
            if matched {
                let elapsed = t0.elapsed();
                let regs = self.registers();
                let mem_addr_val = self.state.get_var("memAddr").unwrap_or(-1) as i64;
                let mem_val_val = self.state.get_var("memVal").unwrap_or(0) as i64;
                let ctx = self.read_memory(addr - 8, 24);
                eprintln!(
                    "[watchpoint] addr=0x{:x} changed at tick {} ({:.3}s, {} ticks)",
                    addr, self.current_tick(), elapsed.as_secs_f64(),
                    self.current_tick() - start_tick
                );
                return serde_json::json!({
                    "hit": true,
                    "tick": self.current_tick(),
                    "addr": addr,
                    "old_value": initial,
                    "new_value": current,
                    "memAddr": mem_addr_val,
                    "memVal": mem_val_val,
                    "registers": regs,
                    "context": ctx,
                });
            }
        }

        let elapsed = t0.elapsed();
        eprintln!(
            "[watchpoint] addr=0x{:x} no change after {} ticks ({:.3}s)",
            addr, max_ticks, elapsed.as_secs_f64()
        );
        serde_json::json!({
            "hit": false,
            "tick": self.current_tick(),
            "addr": addr,
            "value": self.state.read_mem(addr),
            "ticks_checked": max_ticks,
        })
    }

    fn read_memory(&self, addr: i32, len: usize) -> MemoryResponse {
        let mut bytes = Vec::with_capacity(len);
        for i in 0..len {
            bytes.push(self.state.read_mem(addr + i as i32) as u8);
        }
        let hex = bytes.iter().map(|b| format!("{:02X}", b)).collect::<Vec<_>>().join(" ");
        let words: Vec<u16> = bytes
            .chunks(2)
            .map(|c| {
                let lo = c[0] as u16;
                let hi = if c.len() > 1 { c[1] as u16 } else { 0 };
                lo | (hi << 8)
            })
            .collect();
        MemoryResponse { addr, len, hex, bytes, words }
    }

    fn render_screen(&self, addr: i32, width: usize, height: usize) -> ScreenResponse {
        let text = self.state.render_screen(addr as usize, width, height);
        ScreenResponse { addr, width, height, text }
    }

    fn compare_paths(&mut self) -> ComparePathsResponse {
        let tick = self.current_tick();

        // Save current state
        let saved_state = self.state.clone();

        // Run one tick via compiled path, capture slot values before interpreted overwrites them
        let mut compiled_state = saved_state.clone();
        self.evaluator.tick(&mut compiled_state);
        let compiled_slots = self.evaluator.get_all_slot_values();

        // Run one tick via interpreted path, capture interpreted property values
        let mut interpreted_state = saved_state.clone();
        self.evaluator.tick_interpreted(&mut interpreted_state);
        let interpreted_props = self.evaluator.get_all_interpreted_values();

        // Restore original state (don't advance)
        self.state = saved_state;

        // Compare registers
        let mut register_diffs = Vec::new();
        let reg_names: Vec<String> = compiled_state.state_var_names.clone();
        for (i, name) in reg_names.iter().enumerate() {
            let c = compiled_state.state_vars.get(i).copied().unwrap_or(0);
            let interp = interpreted_state.state_vars.get(i).copied().unwrap_or(0);
            if c != interp {
                register_diffs.push(DiffEntry {
                    property: name.clone(),
                    compiled: c as i64,
                    interpreted: interp as i64,
                });
            }
        }

        // Compare computed intermediate properties: compiled slots vs interpreted properties
        let mut property_diffs = Vec::new();
        let mut all_prop_names: std::collections::BTreeSet<String> = compiled_slots.keys().cloned().collect();
        all_prop_names.extend(interpreted_props.keys().cloned());
        for name in &all_prop_names {
            let c = compiled_slots.get(name).copied().unwrap_or(0);
            let interp = interpreted_props.get(name).copied().unwrap_or(0);
            if c != interp {
                property_diffs.push(DiffEntry {
                    property: name.clone(),
                    compiled: c as i64,
                    interpreted: interp as i64,
                });
            }
        }

        // Compare memory (only check where they differ)
        let mut memory_diffs = Vec::new();
        let mem_len = compiled_state.memory.len().min(interpreted_state.memory.len());
        for i in 0..mem_len {
            let c = compiled_state.memory[i];
            let interp = interpreted_state.memory[i];
            if c != interp {
                memory_diffs.push(DiffEntry {
                    property: format!("m{}", i),
                    compiled: c as i64,
                    interpreted: interp as i64,
                });
            }
        }

        let total_diffs = register_diffs.len() + property_diffs.len() + memory_diffs.len();
        ComparePathsResponse {
            tick,
            register_diffs,
            property_diffs,
            memory_diffs,
            total_diffs,
        }
    }

    fn trace_property(&mut self, property: &str) -> TracePropertyResponse {
        let tick = self.current_tick();

        // Get interpreted value for comparison
        let mut interp_state = self.state.clone();
        self.evaluator.tick_interpreted(&mut interp_state);
        let interp_props = self.evaluator.get_all_interpreted_values();
        let interpreted_value = interp_props.get(property).copied();

        // Run traced compiled execution
        let (compiled_value, trace) = self.evaluator.trace_property(&self.state, property)
            .unwrap_or((0, Vec::new()));

        TracePropertyResponse {
            tick,
            property: property.to_string(),
            compiled_value,
            interpreted_value,
            trace_entries: trace.len(),
            trace,
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

fn json_response<T: Serialize>(data: &T) -> Response<std::io::Cursor<Vec<u8>>> {
    let body = serde_json::to_string_pretty(data).unwrap();
    let len = body.len();
    Response::new(
        tiny_http::StatusCode(200),
        vec![Header::from_bytes("Content-Type", "application/json").unwrap()],
        std::io::Cursor::new(body.into_bytes()),
        Some(len),
        None,
    )
}

fn error_response(status: u16, msg: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let body = serde_json::to_string(&ErrorResponse {
        error: msg.to_string(),
    })
    .unwrap();
    let len = body.len();
    Response::new(
        tiny_http::StatusCode(status),
        vec![Header::from_bytes("Content-Type", "application/json").unwrap()],
        std::io::Cursor::new(body.into_bytes()),
        Some(len),
        None,
    )
}

fn read_body(request: &mut tiny_http::Request) -> String {
    let mut body = String::new();
    request.as_reader().read_to_string(&mut body).unwrap_or(0);
    body
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    env_logger::init();
    let cli = Cli::parse();

    // Parse CSS
    eprintln!("Loading {}...", cli.input.display());
    let css = match std::fs::read_to_string(&cli.input) {
        Ok(content) => content,
        Err(e) => {
            eprintln!("Error reading {}: {e}", cli.input.display());
            std::process::exit(1);
        }
    };

    let t0 = std::time::Instant::now();
    let parsed = match calcite_core::parser::parse_css(&css) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Parse error: {e}");
            std::process::exit(1);
        }
    };
    let parse_time = t0.elapsed();
    eprintln!(
        "Parsed: {} @property, {} @function, {} assignments ({:.2}s)",
        parsed.properties.len(),
        parsed.functions.len(),
        parsed.assignments.len(),
        parse_time.as_secs_f64(),
    );

    let mut state = State::default();
    state.load_properties(&parsed.properties);

    let t1 = std::time::Instant::now();
    let evaluator = Evaluator::from_parsed(&parsed);
    let compile_time = t1.elapsed();
    eprintln!("Compiled in {:.2}s", compile_time.as_secs_f64());

    let video_config = calcite_core::detect_video_memory();
    if let Some((addr, size)) = video_config {
        eprintln!("Video memory detected at 0x{:X} ({} bytes)", addr, size);
    }

    // Collect all property names from the compiled program
    let property_names: Vec<String> = evaluator
        .assignments
        .iter()
        .map(|a| a.property.clone())
        .collect();

    let properties_count = parsed.properties.len();
    let functions_count = parsed.functions.len();
    let assignments_count = parsed.assignments.len();

    // Save initial state as snapshot 0
    let initial_snapshot = (0, state.clone());

    let session = Mutex::new(DebugSession {
        evaluator,
        state,
        snapshots: vec![initial_snapshot],
        snapshot_interval: cli.snapshot_interval,
        properties_count,
        functions_count,
        assignments_count,
        css_file: cli.input.display().to_string(),
        video_config,
        property_names,
        parsed_program: parsed,
    });

    // Kill any existing debugger on this port before binding
    let addr = format!("0.0.0.0:{}", cli.port);
    if let Ok(mut stream) = std::net::TcpStream::connect(format!("127.0.0.1:{}", cli.port)) {
        use std::io::Write;
        let _ = stream.write_all(b"POST /shutdown HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n");
        let _ = stream.flush();
        drop(stream);
        eprintln!("WARNING: Killed existing debugger on port {}. Use 'curl -X POST localhost:{}/shutdown' next time.", cli.port, cli.port);
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    // Start HTTP server
    let server = Server::http(&addr).unwrap_or_else(|e| {
        eprintln!("Failed to start server on {}: {e}", addr);
        std::process::exit(1);
    });
    eprintln!("Debug server listening on http://localhost:{}", cli.port);
    eprintln!();
    eprintln!("Endpoints:");
    eprintln!("  GET  /info              — session info and available endpoints");
    eprintln!("  GET  /state             — current registers + computed properties");
    eprintln!("  POST /tick              — advance ticks: {{\"count\": N}}");
    eprintln!("  POST /seek              — seek to tick: {{\"tick\": N}}");
    eprintln!("  POST /memory            — read memory: {{\"addr\": N, \"len\": N}}");
    eprintln!("  POST /screen            — render screen: {{\"addr\": 0xB8000, \"width\": 80, \"height\": 25}}");
    eprintln!("  POST /compare           — compare vs reference: {{\"reference\": [...]}}");
    eprintln!("  POST /compare-state     — diff current state vs supplied registers/memory");
    eprintln!("  POST /key               — set keyboard buffer (BDA): {{\"value\": (scancode<<8)|ascii}}");
    eprintln!("  POST /keyboard          — set --keyboard CSS property: {{\"value\": (scancode<<8)|ascii}}");
    eprintln!("  GET  /compare-paths     — diff compiled vs interpreted for current tick");
    eprintln!("  POST /trace-property    — trace compiled ops for a property: {{\"property\": \"--memAddr\"}}");
    eprintln!("  POST /snapshot          — create snapshot at current tick");
    eprintln!("  GET  /snapshots         — list all snapshots");

    for mut request in server.incoming_requests() {
        let path = request.url().split('?').next().unwrap_or("").to_string();
        let method = request.method().clone();

        let response = match (method, path.as_str()) {
            (Method::Get, "/info") => {
                let s = session.lock().unwrap();
                json_response(&InfoResponse {
                    css_file: s.css_file.clone(),
                    current_tick: s.current_tick(),
                    properties_count: s.properties_count,
                    functions_count: s.functions_count,
                    assignments_count: s.assignments_count,
                    snapshots: s.snapshots.iter().map(|(t, _)| *t).collect(),
                    endpoints: vec![
                        "GET /info",
                        "GET /state",
                        "POST /tick {count}",
                        "POST /seek {tick}",
                        "POST /memory {addr, len}",
                        "POST /screen {addr, width, height}",
                        "POST /compare {reference}",
                        "POST /compare-state {registers, memory}",
                        "POST /key {value}",
                        "POST /keyboard {value}",
                        "GET /compare-paths",
                        "POST /trace-property {property}",
                        "POST /snapshot",
                        "GET /snapshots",
                    ],
                })
            }

            (Method::Get, "/state") => {
                let s = session.lock().unwrap();
                json_response(&StateResponse {
                    tick: s.current_tick(),
                    registers: s.registers(),
                    properties: s.get_properties(),
                })
            }

            (Method::Post, "/tick") => {
                let body = read_body(&mut request);
                let count = if body.is_empty() {
                    1
                } else {
                    match serde_json::from_str::<TickRequest>(&body) {
                        Ok(r) => r.count.unwrap_or(1),
                        Err(e) => {
                            let _ = request.respond(error_response(400, &format!("Bad JSON: {e}")));
                            continue;
                        }
                    }
                };
                let mut s = session.lock().unwrap();
                let t0 = std::time::Instant::now();
                let resp = s.tick(count);
                let elapsed = t0.elapsed();
                eprintln!(
                    "[tick] {} -> {} ({} ticks, {:.3}s)",
                    resp.tick - resp.ticks_executed,
                    resp.tick,
                    resp.ticks_executed,
                    elapsed.as_secs_f64()
                );
                json_response(&resp)
            }

            (Method::Post, "/seek") => {
                let body = read_body(&mut request);
                match serde_json::from_str::<SeekRequest>(&body) {
                    Ok(r) => {
                        let mut s = session.lock().unwrap();
                        let from = s.current_tick();
                        let t0 = std::time::Instant::now();
                        s.seek(r.tick);
                        let elapsed = t0.elapsed();
                        eprintln!(
                            "[seek] {} -> {} ({:.3}s)",
                            from,
                            s.current_tick(),
                            elapsed.as_secs_f64()
                        );
                        json_response(&StateResponse {
                            tick: s.current_tick(),
                            registers: s.registers(),
                            properties: s.get_properties(),
                        })
                    }
                    Err(e) => error_response(400, &format!("Bad JSON: {e}")),
                }
            }

            (Method::Post, "/memory") => {
                let body = read_body(&mut request);
                match serde_json::from_str::<MemoryRequest>(&body) {
                    Ok(r) => {
                        let s = session.lock().unwrap();
                        let len = r.len.unwrap_or(256);
                        json_response(&s.read_memory(r.addr, len))
                    }
                    Err(e) => error_response(400, &format!("Bad JSON: {e}")),
                }
            }

            (Method::Post, "/screen") => {
                let body = read_body(&mut request);
                let (addr, width, height) = if body.is_empty() {
                    // Use detected video config or defaults
                    let s = session.lock().unwrap();
                    if let Some((vaddr, _)) = s.video_config {
                        (vaddr as i32, 80, 25)
                    } else {
                        (0xB8000, 80, 25)
                    }
                } else {
                    match serde_json::from_str::<ScreenRequest>(&body) {
                        Ok(r) => (
                            r.addr.unwrap_or(0xB8000),
                            r.width.unwrap_or(80),
                            r.height.unwrap_or(25),
                        ),
                        Err(e) => {
                            let _ = request.respond(error_response(400, &format!("Bad JSON: {e}")));
                            continue;
                        }
                    }
                };
                let s = session.lock().unwrap();
                json_response(&s.render_screen(addr, width, height))
            }

            (Method::Post, "/compare") => {
                let body = read_body(&mut request);
                match serde_json::from_str::<CompareRequest>(&body) {
                    Ok(r) => {
                        let mut s = session.lock().unwrap();
                        let stop_at_first = r.stop_at_first.unwrap_or(true);

                        // Reset to tick 0
                        s.seek(0);

                        let mut divergences = Vec::new();

                        let ticks_to_compare = r.reference.len() as u32;
                        for ref_tick in &r.reference {
                            // Advance to the right tick
                            while s.current_tick() < ref_tick.tick {
                                s.tick(1);
                            }
                            // Also run the tick AT ref_tick.tick if we're behind
                            if s.current_tick() == ref_tick.tick {
                                s.tick(1);
                            }

                            // Compare registers by name
                            for (name, expected) in &ref_tick.registers {
                                if let Some(expected_val) = expected.as_i64() {
                                    let actual = s.state.get_var(name).unwrap_or(0) as i64;
                                    if actual != expected_val {
                                        divergences.push(DivergenceInfo {
                                            tick: ref_tick.tick,
                                            register: name.clone(),
                                            expected: expected_val,
                                            actual,
                                        });
                                    }
                                }
                            }

                            if stop_at_first && !divergences.is_empty() {
                                break;
                            }
                        }

                        json_response(&CompareResponse {
                            divergences,
                            ticks_compared: ticks_to_compare,
                        })
                    }
                    Err(e) => error_response(400, &format!("Bad JSON: {e}")),
                }
            }

            (Method::Get, "/compare-paths") => {
                let mut s = session.lock().unwrap();
                let resp = s.compare_paths();
                json_response(&resp)
            }

            (Method::Post, "/dump-ops") => {
                let body = read_body(&mut request);
                #[derive(Deserialize)]
                struct DumpOpsReq { start: usize, end: usize }
                match serde_json::from_str::<DumpOpsReq>(&body) {
                    Ok(r) => {
                        let s = session.lock().unwrap();
                        let lines = s.evaluator.dump_ops_range(r.start, r.end);
                        json_response(&serde_json::json!({"ops": lines}))
                    }
                    Err(e) => error_response(400, &format!("Bad JSON: {e}")),
                }
            }

            (Method::Post, "/trace-property") => {
                let body = read_body(&mut request);
                match serde_json::from_str::<TracePropertyRequest>(&body) {
                    Ok(r) => {
                        let mut s = session.lock().unwrap();
                        let resp = s.trace_property(&r.property);
                        json_response(&resp)
                    }
                    Err(e) => error_response(400, &format!("Bad JSON: {e}")),
                }
            }

            (Method::Post, "/key") => {
                let body = read_body(&mut request);
                match serde_json::from_str::<KeyRequest>(&body) {
                    Ok(r) => {
                        let mut s = session.lock().unwrap();
                        s.state.bda_push_key(r.value);
                        json_response(&serde_json::json!({
                            "keyboard": r.value,
                            "tick": s.current_tick(),
                        }))
                    }
                    Err(e) => error_response(400, &format!("Bad JSON: {e}")),
                }
            }

            // Set the --keyboard CSS state variable.
            // This is for the v3 microcode path where keyboard input goes through
            // CSS edge detection → IRQ 1 → INT 09h microcode → BDA buffer.
            // Use value=(scancode<<8)|ascii for key press, value=0 for release.
            (Method::Post, "/keyboard") => {
                let body = read_body(&mut request);
                match serde_json::from_str::<KeyRequest>(&body) {
                    Ok(r) => {
                        let mut s = session.lock().unwrap();
                        let set = s.state.set_var("keyboard", r.value);
                        json_response(&serde_json::json!({
                            "ok": set,
                            "keyboard": r.value,
                            "tick": s.current_tick(),
                        }))
                    }
                    Err(e) => error_response(400, &format!("Bad JSON: {e}")),
                }
            }

            (Method::Post, "/compare-state") => {
                let body = read_body(&mut request);
                match serde_json::from_str::<CompareStateRequest>(&body) {
                    Ok(r) => {
                        let s = session.lock().unwrap();
                        let mut register_diffs = Vec::new();
                        let mut memory_diffs = Vec::new();

                        if let Some(regs) = r.registers {
                            for (name, expected) in regs {
                                let actual = s.state.get_var(&name).unwrap_or(0) as i64;
                                if actual != expected {
                                    register_diffs.push(DiffEntry {
                                        property: name.clone(),
                                        compiled: actual,
                                        interpreted: expected,
                                    });
                                }
                            }
                        }

                        if let Some(ranges) = r.memory {
                            for range in ranges {
                                for i in 0..range.len {
                                    if i >= range.bytes.len() {
                                        break;
                                    }
                                    let addr = range.addr + i as i32;
                                    let actual = s.state.read_mem(addr) as u8;
                                    let expected = range.bytes[i];
                                    if actual != expected {
                                        memory_diffs.push(MemDiffEntry {
                                            addr,
                                            expected,
                                            actual,
                                        });
                                    }
                                }
                            }
                        }

                        let total_diffs = register_diffs.len() + memory_diffs.len();
                        json_response(&CompareStateResponse {
                            tick: s.current_tick(),
                            register_diffs,
                            memory_diffs,
                            total_diffs,
                        })
                    }
                    Err(e) => error_response(400, &format!("Bad JSON: {e}")),
                }
            }

            (Method::Post, "/snapshot") => {
                let mut s = session.lock().unwrap();
                let tick = s.current_tick();
                if !s.snapshots.iter().any(|(t, _)| *t == tick) {
                    let cloned = s.state.clone();
                    s.snapshots.push((tick, cloned));
                }
                json_response(&serde_json::json!({
                    "snapshot_created": tick,
                    "total_snapshots": s.snapshots.len(),
                }))
            }

            (Method::Get, "/snapshots") => {
                let s = session.lock().unwrap();
                let ticks: Vec<u32> = s.snapshots.iter().map(|(t, _)| *t).collect();
                json_response(&serde_json::json!({
                    "snapshots": ticks,
                    "count": ticks.len(),
                }))
            }

            (Method::Get, "/slot-map") => {
                let s = session.lock().unwrap();
                let map = s.evaluator.get_slot_map();
                let entries: Vec<serde_json::Value> = map.iter()
                    .map(|(slot, name)| serde_json::json!({"slot": slot, "name": name, "value": s.evaluator.get_slot_by_index(*slot)}))
                    .collect();
                json_response(&serde_json::json!({ "slots": entries }))
            }

            (Method::Post, "/ops") => {
                // Dump compiled ops for a property: {"property": "--CS"}
                let body = read_body(&mut request);
                let req: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                let prop = req["property"].as_str().unwrap_or("--CS").to_string();
                let s = session.lock().unwrap();
                match s.evaluator.get_ops_for_property(&prop) {
                    Some(lines) => json_response(&serde_json::json!({ "property": prop, "ops": lines })),
                    None => error_response(404, &format!("Property {} not found", prop)),
                }
            }

            // Memory watchpoint: run forward until a byte changes.
            // POST /watchpoint {"addr": N, "max_ticks": M}
            // Optionally: "from_tick": T to seek first, "expected": V to watch for a specific value.
            (Method::Post, "/watchpoint") => {
                let body = read_body(&mut request);
                #[derive(Deserialize)]
                struct WatchReq {
                    addr: i32,
                    max_ticks: Option<u32>,
                    from_tick: Option<u32>,
                    expected: Option<i32>,
                }
                match serde_json::from_str::<WatchReq>(&body) {
                    Ok(r) => {
                        let mut s = session.lock().unwrap();
                        if let Some(from) = r.from_tick {
                            s.seek(from);
                        }
                        let resp = s.watchpoint(r.addr, r.max_ticks.unwrap_or(100_000), r.expected);
                        json_response(&resp)
                    }
                    Err(e) => error_response(400, &format!("Bad JSON: {e}")),
                }
            }

            (Method::Post, "/shutdown") => {
                let _ = request.respond(json_response(&serde_json::json!({"status": "shutting down"})));
                eprintln!("Shutdown requested");
                std::process::exit(0);
            }

            _ => error_response(404, &format!("Unknown endpoint: {}", path)),
        };

        if let Err(e) = request.respond(response) {
            eprintln!("Failed to send response: {e}");
        }
    }
}
