//! calcite-debugger — MCP debug server for CSS execution.
//!
//! Parses a CSS file once, then exposes step / inspect / compare operations
//! as MCP tools over stdio. Designed for LLM-driven debugging.
//!
//! ## Transport
//!
//! stdio only. **stdout is reserved for MCP protocol frames** — every byte of
//! logging goes to stderr (via `eprintln!` and `env_logger` configured to
//! `Target::Stderr`).
//!
//! ## Tool surface
//!
//! See `DebugSession` impl below; each `#[tool]`-annotated method is exposed.
//! Run with `--list-tools` for a runtime dump (handy when wiring an MCP client).

use calcite_core::{Evaluator, State};
use clap::Parser;
use rmcp::{
    handler::server::{
        router::tool::ToolRouter,
        wrapper::{Json, Parameters},
    },
    model::{ErrorData, ServerCapabilities, ServerInfo},
    schemars,
    tool, tool_handler, tool_router,
    transport::stdio,
    ServerHandler, ServiceExt,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(name = "calcite-debugger", version, about = "MCP debug server for calcite")]
struct Cli {
    /// Path to the CSS file to debug. Optional — if omitted, the server starts
    /// with no program loaded and you call the `open` tool to choose one.
    #[arg(short, long)]
    input: Option<PathBuf>,

    /// Ticks between full-state base checkpoints. Deltas are recorded every
    /// tick regardless. Default 50,000 matches the PR 2 brief.
    #[arg(long, default_value = "50000")]
    base_interval: u32,
}

// ---------------------------------------------------------------------------
// Tool parameter & result types
//
// All types derive serde + schemars::JsonSchema so rmcp can build the
// JSON-RPC tool/list schema automatically.
// ---------------------------------------------------------------------------

#[derive(Deserialize, Serialize, schemars::JsonSchema, Default)]
struct EmptyParams {}

#[derive(Deserialize, schemars::JsonSchema)]
struct TickParams {
    /// Number of ticks to advance. Defaults to 1.
    #[serde(default = "default_one")]
    count: u32,
}
fn default_one() -> u32 {
    1
}

#[derive(Deserialize, schemars::JsonSchema)]
struct SeekParams {
    /// Target tick number. Forward or backward — uses nearest snapshot then replays.
    tick: u32,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct MemoryParams {
    /// Linear address (e.g. 0x400 for BDA, 0xB8000 for text VGA).
    addr: i32,
    /// Number of bytes to read. Defaults to 256.
    #[serde(default = "default_mem_len")]
    len: usize,
}
fn default_mem_len() -> usize {
    256
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ScreenParams {
    /// Linear address of video memory. Defaults to detected video region or 0xB8000.
    #[serde(default)]
    addr: Option<i32>,
    /// Columns. Defaults to 80.
    #[serde(default)]
    width: Option<usize>,
    /// Rows. Defaults to 25.
    #[serde(default)]
    height: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct CompareReferenceParams {
    /// Reference trace: list of {tick, registers...} entries to diff against.
    reference: Vec<RefTickJson>,
    /// Stop at the first divergence. Defaults to true.
    #[serde(default = "default_true")]
    stop_at_first: bool,
}
fn default_true() -> bool {
    true
}

#[derive(Deserialize, Clone, schemars::JsonSchema)]
struct RefTickJson {
    tick: u32,
    /// Register values keyed by name (e.g. {"AX": 1234, "IP": 256}).
    /// Use a separate `registers` object — flatten doesn't play well with schemars.
    registers: HashMap<String, i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct CompareStateParams {
    /// Register values to diff against, keyed by name ("AX", "IP", ...).
    #[serde(default)]
    registers: Option<HashMap<String, i64>>,
    /// Memory ranges to diff: list of {addr, len, bytes} entries.
    #[serde(default)]
    memory: Option<Vec<CompareMemEntry>>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct CompareMemEntry {
    addr: i32,
    len: usize,
    /// Expected bytes (from the reference side) as a u8 array.
    bytes: Vec<u8>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct SendKeyParams {
    /// Packed key value: (scancode << 8) | ascii. Use 0 to clear / release.
    value: i32,
    /// Where to write the key:
    /// `bda` (default) pushes into the BIOS Data Area ring buffer at 0x41E,
    /// `keyboard` sets the `--keyboard` CSS state var (used by the v3 microcode
    /// path that converts edges into IRQ 1).
    #[serde(default = "default_key_target")]
    target: String,
}
fn default_key_target() -> String {
    "bda".into()
}

#[derive(Deserialize, schemars::JsonSchema)]
struct TracePropertyParams {
    /// CSS property name to trace (e.g. "--memAddr").
    property: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct WatchpointParams {
    /// Linear memory address to watch.
    addr: i32,
    /// Maximum ticks to run before giving up. Defaults to 100,000.
    #[serde(default = "default_watch_max")]
    max_ticks: u32,
    /// If set, seek to this tick first.
    #[serde(default)]
    from_tick: Option<u32>,
    /// Stop when the byte equals this value (instead of "any change").
    #[serde(default)]
    expected: Option<i32>,
}
fn default_watch_max() -> u32 {
    100_000
}

/// Discriminated union — pass exactly one variant.
/// Matches the original HTTP `RunUntilCondition` shape.
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum RunUntilCondition {
    /// Stop when CS:IP matches.
    CsIp { cs: i32, ip: i32 },
    /// Stop when CS matches.
    Cs(i32),
    /// Stop when CS matches and min ≤ IP ≤ max.
    IpRange { cs: i32, min: i32, max: i32 },
    /// Stop when next opcode at CS:IP is 0xCD (any INT).
    Int(bool),
    /// Stop when CS:IP points to `CD <N>`.
    IntNum(i32),
    /// Stop when a CSS property equals a value.
    PropertyEquals { name: String, value: i64 },
    /// Stop when a CSS property changes from its starting value.
    PropertyChanges(String),
    /// Stop when memory byte equals a value.
    MemByteEquals { addr: i32, value: i32 },
}

#[derive(Deserialize, schemars::JsonSchema)]
struct RunUntilParams {
    condition: RunUntilCondition,
    /// Maximum ticks before giving up. Defaults to 1,000,000.
    #[serde(default = "default_run_until_max")]
    max_ticks: u32,
    /// If set, seek to this tick first.
    #[serde(default)]
    from_tick: Option<u32>,
}
fn default_run_until_max() -> u32 {
    1_000_000
}

#[derive(Deserialize, schemars::JsonSchema)]
struct DumpOpsParams {
    /// Op range start (inclusive).
    #[serde(default)]
    start: Option<usize>,
    /// Op range end (exclusive).
    #[serde(default)]
    end: Option<usize>,
    /// Property name — dump every op that contributes to this property.
    /// Mutually exclusive with start/end.
    #[serde(default)]
    property: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct SnapshotParams {
    /// `create` to checkpoint at the current tick, `list` to enumerate existing snapshots.
    /// Defaults to `list`.
    #[serde(default = "default_snapshot_action")]
    action: String,
}
fn default_snapshot_action() -> String {
    "list".into()
}

// --- Result types -----------------------------------------------------------

#[derive(Serialize, schemars::JsonSchema)]
struct InfoResult {
    /// False when no CSS file has been loaded yet. In that case the other
    /// fields are empty/zero and the caller should invoke `open` to load one.
    loaded: bool,
    css_file: Option<String>,
    current_tick: u32,
    properties_count: usize,
    functions_count: usize,
    assignments_count: usize,
    snapshots: Vec<u32>,
    /// The current base_interval setting (shared across all loaded programs).
    base_interval: u32,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct OpenParams {
    /// Path to the CSS file to load. Replaces any currently-loaded program;
    /// previous state / snapshots / deltas are discarded.
    path: String,
}

#[derive(Serialize, schemars::JsonSchema)]
struct OpenResult {
    css_file: String,
    properties_count: usize,
    functions_count: usize,
    assignments_count: usize,
}

#[derive(Serialize, schemars::JsonSchema)]
struct StateResult {
    tick: u32,
    registers: BTreeMap<String, i32>,
    properties: HashMap<String, i64>,
}

#[derive(Serialize, schemars::JsonSchema)]
struct TickResult {
    tick: u32,
    ticks_executed: u32,
    /// Per-state-var changes accumulated across all ticks executed this call.
    changes: Vec<(String, String)>,
}

#[derive(Serialize, schemars::JsonSchema)]
struct MemoryResult {
    addr: i32,
    len: usize,
    /// Hex dump (space-separated bytes).
    hex: String,
    bytes: Vec<u8>,
    /// Little-endian u16 view, for convenience.
    words: Vec<u16>,
}

#[derive(Serialize, schemars::JsonSchema)]
struct ScreenResult {
    addr: i32,
    width: usize,
    height: usize,
    text: String,
}

#[derive(Serialize, schemars::JsonSchema)]
struct DivergenceInfo {
    tick: u32,
    register: String,
    expected: i64,
    actual: i64,
}

#[derive(Serialize, schemars::JsonSchema)]
struct CompareReferenceResult {
    divergences: Vec<DivergenceInfo>,
    ticks_compared: u32,
}

#[derive(Serialize, schemars::JsonSchema)]
struct DiffEntry {
    property: String,
    compiled: i64,
    interpreted: i64,
}

#[derive(Serialize, schemars::JsonSchema)]
struct ComparePathsResult {
    tick: u32,
    register_diffs: Vec<DiffEntry>,
    property_diffs: Vec<DiffEntry>,
    memory_diffs: Vec<DiffEntry>,
    total_diffs: usize,
}

#[derive(Serialize, schemars::JsonSchema)]
struct MemDiffEntry {
    addr: i32,
    expected: u8,
    actual: u8,
}

#[derive(Serialize, schemars::JsonSchema)]
struct CompareStateResult {
    tick: u32,
    register_diffs: Vec<DiffEntry>,
    memory_diffs: Vec<MemDiffEntry>,
    total_diffs: usize,
}

#[derive(Serialize, schemars::JsonSchema)]
struct TracePropertyResult {
    tick: u32,
    property: String,
    compiled_value: i32,
    interpreted_value: Option<i32>,
    trace_entries: usize,
    /// Per-op trace entries as JSON (fields: pc, op, dst_slot, dst_value, inputs, branch_taken, depth).
    trace: serde_json::Value,
}

#[derive(Serialize, schemars::JsonSchema)]
struct SendKeyResult {
    tick: u32,
    value: i32,
    target: String,
    /// True if the keyboard state var existed (only meaningful for target=keyboard).
    ok: bool,
}

#[derive(Serialize, schemars::JsonSchema)]
struct WatchpointResult {
    hit: bool,
    tick: u32,
    addr: i32,
    /// Present if hit.
    old_value: Option<i32>,
    /// Present if hit.
    new_value: Option<i32>,
    /// Present if not hit (the unchanged value).
    value: Option<i32>,
    /// Snapshot of `--memAddr` register at hit time.
    mem_addr: Option<i64>,
    /// Snapshot of `--memVal` register at hit time.
    mem_val: Option<i64>,
    /// 24 bytes of context centered on `addr`. Empty if not hit.
    context_hex: String,
    /// Registers at hit time. Empty if not hit.
    registers: BTreeMap<String, i32>,
}

#[derive(Serialize, schemars::JsonSchema)]
struct RunUntilResult {
    hit: bool,
    tick: u32,
    ticks_run: u32,
    cs: Option<i32>,
    ip: Option<i32>,
    opcode: Option<i32>,
    next_byte: Option<i32>,
    ah: Option<i32>,
    /// Free-form description of which condition matched. Present only if hit.
    matched: Option<serde_json::Value>,
    registers: BTreeMap<String, i32>,
}

#[derive(Serialize, schemars::JsonSchema)]
struct DumpOpsResult {
    /// Each entry is one decoded op as a string.
    ops: Vec<String>,
    /// Echoed back so the caller knows which mode was used.
    property: Option<String>,
    start: Option<usize>,
    end: Option<usize>,
}

#[derive(Serialize, schemars::JsonSchema)]
struct SlotMapResult {
    slots: Vec<SlotEntry>,
}

#[derive(Serialize, schemars::JsonSchema)]
struct SlotEntry {
    slot: u32,
    name: String,
    /// None if the slot is out of range.
    value: Option<i32>,
}

#[derive(Serialize, schemars::JsonSchema)]
struct SnapshotResult {
    action: String,
    /// All snapshot ticks currently held.
    snapshots: Vec<u32>,
    /// If action=create, the tick that was just snapshotted (or already existed).
    created: Option<u32>,
}

// ---------------------------------------------------------------------------
// DebugSession — the actual debugger state, behind a Mutex inside the handler.
// Identical mechanics to the HTTP version; only the transport changed.
// ---------------------------------------------------------------------------

/// Delta-snapshot model:
///
/// - `bases`: full State clones at tick 0 and every `base_interval` ticks.
///   Memory cost: O(state_size) per base. With the default 50,000-tick
///   interval, a multi-MB State means ~MB per 50K ticks — acceptable for
///   long-running sessions and much less than the HTTP version's 1K-tick
///   clones.
/// - `deltas`: one per tick that has actually been executed, indexed by
///   frame_counter_before. Each delta stores old+new for every changed
///   channel — so we can apply forward (seek into the future) OR backward
///   (reverse seek).
///
/// Seeking from tick A to target T:
///  1. Find the nearest base b with b.tick ≤ T. If the current tick already
///     lies in [b.tick, T] and we're going forward, no restore needed.
///  2. Otherwise restore from the base (O(state_clone)).
///  3. Apply forward deltas from current tick up to T (O(T - current)).
///  4. Reverse seek (current > T) with no better base: revert deltas backward
///     from current down to T without touching the base.
struct DebugSession {
    evaluator: Evaluator,
    state: State,
    /// Full-state checkpoints. Always contains (0, initial_state).
    bases: Vec<(u32, State)>,
    /// One entry per executed tick, in order. `deltas[i].frame_counter_before`
    /// is the tick number when that delta was produced.
    deltas: Vec<calcite_core::StateDelta>,
    /// Ticks between base checkpoints. 0 = only keep the initial base.
    base_interval: u32,
    properties_count: usize,
    functions_count: usize,
    assignments_count: usize,
    css_file: String,
    video_config: Option<(usize, usize)>,
    property_names: Vec<String>,
}

impl DebugSession {
    fn current_tick(&self) -> u32 {
        self.state.frame_counter
    }

    fn registers_map(&self) -> BTreeMap<String, i32> {
        let mut map = BTreeMap::new();
        for (i, name) in self.state.state_var_names.iter().enumerate() {
            if i < self.state.state_vars.len() {
                map.insert(name.clone(), self.state.state_vars[i]);
            }
        }
        map
    }

    fn properties_map(&self) -> HashMap<String, i64> {
        let mut props = HashMap::new();
        for name in &self.property_names {
            if let Some(val) = self.evaluator.get_slot_value(name) {
                props.insert(name.clone(), val as i64);
            }
        }
        props
    }

    /// Execute a single tick: run the evaluator, record the delta, and drop a
    /// base checkpoint if we've crossed a base_interval boundary.
    ///
    /// This is the ONLY write path for `deltas` — every other tick-advancing
    /// operation (tick_n, seek forward, watchpoint, run_until) goes through
    /// here. Keeps the invariant "deltas covers every executed tick" trivially
    /// true.
    fn step_one(&mut self) -> calcite_core::eval::TickResult {
        let tick_before = self.current_tick();
        let (result, delta) = self.evaluator.tick_with_delta(&mut self.state);
        // deltas is indexed by frame_counter_before = position in the vec
        // relative to the first-ever executed tick. We simply append.
        // Re-execution after a seek can overwrite at [tick_before as usize]
        // if we already hold a delta there (they must be equivalent, but keep
        // the freshly-produced one defensively).
        if (tick_before as usize) < self.deltas.len() {
            self.deltas[tick_before as usize] = delta;
        } else {
            self.deltas.push(delta);
        }
        // Base checkpoint on crossing boundaries. Tick 0 is always in `bases`.
        if self.base_interval > 0 {
            let t = self.current_tick();
            if t > 0 && t % self.base_interval == 0
                && !self.bases.iter().any(|(bt, _)| *bt == t)
            {
                self.bases.push((t, self.state.clone()));
            }
        }
        result
    }

    fn tick_n(&mut self, count: u32) -> TickResult {
        let start = self.current_tick();
        let mut all_changes = Vec::new();
        for _ in 0..count {
            let r = self.step_one();
            all_changes.extend(r.changes);
        }
        TickResult {
            tick: self.current_tick(),
            ticks_executed: self.current_tick() - start,
            changes: all_changes,
        }
    }

    /// Seek to `target_tick`. Forward: replay deltas or re-execute. Backward:
    /// revert deltas if we have them, else restore from nearest base and
    /// replay forward.
    fn seek(&mut self, target_tick: u32) {
        let current = self.current_tick();
        if current == target_tick {
            return;
        }

        // --- Backward path ---
        if target_tick < current {
            // If we have every delta between target and current, walk back.
            // deltas[i] captures the transition from tick i -> tick i+1, so
            // reverting deltas[target..current] takes us from current to target.
            if (target_tick as usize) < self.deltas.len()
                && (current as usize) <= self.deltas.len()
            {
                // Revert in reverse order (current-1, current-2, ..., target).
                for i in (target_tick as usize..current as usize).rev() {
                    self.state.revert_delta(&self.deltas[i]);
                }
                self.refresh_slots();
                return;
            }
            // Otherwise restore from nearest base ≤ target and replay forward.
            let (base_tick, base_state) = self.nearest_base_le(target_tick);
            self.state = base_state;
            assert_eq!(self.state.frame_counter, base_tick);
            // Forward replay via re-execution. These ticks already have deltas
            // recorded; step_one will overwrite them at the same indices.
            while self.current_tick() < target_tick {
                self.step_one();
            }
            return;
        }

        // --- Forward path ---
        // Prefer delta replay if we have ALL the deltas we'd need, because
        // that avoids re-running the evaluator.
        let have_all_forward = (current as usize..target_tick as usize)
            .all(|i| i < self.deltas.len());
        if have_all_forward {
            for i in current as usize..target_tick as usize {
                // Safety: cloning the delta would work but is wasteful. Pull
                // the reference, apply, move on — state is mutated in place.
                // Re-borrow needed because self.state and self.deltas are both
                // fields of self.
                let delta_ptr = &self.deltas[i] as *const calcite_core::StateDelta;
                // SAFETY: we hold &mut self; deltas is not mutated in this
                // loop (apply_delta only touches self.state).
                unsafe {
                    self.state.apply_delta(&*delta_ptr);
                }
            }
            self.refresh_slots();
            return;
        }

        // Otherwise re-execute. If we'd need to start from a closer base
        // than where we are (e.g., we're way past target and there's no
        // path forward), jump to nearest base.
        if current > target_tick {
            // Unreachable given the backward-path early return above, but
            // kept defensively.
            let (_, base_state) = self.nearest_base_le(target_tick);
            self.state = base_state;
        }
        while self.current_tick() < target_tick {
            self.step_one();
        }
    }

    /// Re-populate the Evaluator's internal slot cache so that
    /// `get_slot_value` / `properties_map` reflect the current state.
    ///
    /// Needed after a delta-only seek (apply_delta / revert_delta) because
    /// those paths don't call `execute()`, which is what normally leaves
    /// `self.slots` populated. Without this, `properties` returned to the
    /// MCP client would be stale from the last executed tick.
    ///
    /// Runs the compiled path on a scratch clone. State-var deltas recorded
    /// by this scratch run are discarded (we only keep the updated slot
    /// cache on the Evaluator); frame_counter doesn't advance on the real
    /// state because scratch is thrown away.
    fn refresh_slots(&mut self) {
        let mut scratch = self.state.clone();
        // `tick` calls `compile::execute` which populates `self.slots`.
        // Pre-tick hooks on `scratch` are fine — they mutate scratch.memory
        // which is discarded.
        self.evaluator.tick(&mut scratch);
    }

    /// Nearest base with tick ≤ target. Always succeeds because (0, initial)
    /// is always in `bases`. Returns an owned State clone.
    fn nearest_base_le(&self, target: u32) -> (u32, State) {
        let mut best: Option<(u32, &State)> = None;
        for (t, s) in &self.bases {
            if *t <= target && (best.is_none() || *t > best.unwrap().0) {
                best = Some((*t, s));
            }
        }
        let (t, s) = best.expect("bases always contains (0, initial_state)");
        (t, s.clone())
    }

    fn read_memory(&self, addr: i32, len: usize) -> MemoryResult {
        let mut bytes = Vec::with_capacity(len);
        for i in 0..len {
            bytes.push(self.state.read_mem(addr + i as i32) as u8);
        }
        let hex = bytes
            .iter()
            .map(|b| format!("{:02X}", b))
            .collect::<Vec<_>>()
            .join(" ");
        let words: Vec<u16> = bytes
            .chunks(2)
            .map(|c| {
                let lo = c[0] as u16;
                let hi = if c.len() > 1 { c[1] as u16 } else { 0 };
                lo | (hi << 8)
            })
            .collect();
        MemoryResult { addr, len, hex, bytes, words }
    }

    fn render_screen(&self, addr: i32, width: usize, height: usize) -> ScreenResult {
        let text = self.state.render_screen(addr as usize, width, height);
        ScreenResult { addr, width, height, text }
    }

    fn watchpoint(&mut self, addr: i32, max_ticks: u32, expected: Option<i32>) -> WatchpointResult {
        let initial = self.state.read_mem(addr);
        let start = self.current_tick();
        let t0 = std::time::Instant::now();

        for _ in 0..max_ticks {
            self.step_one();
            let current = self.state.read_mem(addr);
            let matched = match expected {
                Some(exp) => current == exp,
                None => current != initial,
            };
            if matched {
                let elapsed = t0.elapsed();
                let mem_addr_val = self.state.get_var("memAddr").map(|v| v as i64);
                let mem_val_val = self.state.get_var("memVal").map(|v| v as i64);
                let ctx = self.read_memory(addr - 8, 24);
                eprintln!(
                    "[watchpoint] addr=0x{:x} changed at tick {} ({:.3}s, {} ticks)",
                    addr,
                    self.current_tick(),
                    elapsed.as_secs_f64(),
                    self.current_tick() - start
                );
                return WatchpointResult {
                    hit: true,
                    tick: self.current_tick(),
                    addr,
                    old_value: Some(initial),
                    new_value: Some(current),
                    value: None,
                    mem_addr: mem_addr_val,
                    mem_val: mem_val_val,
                    context_hex: ctx.hex,
                    registers: self.registers_map(),
                };
            }
        }

        let elapsed = t0.elapsed();
        eprintln!(
            "[watchpoint] addr=0x{:x} no change after {} ticks ({:.3}s)",
            addr,
            max_ticks,
            elapsed.as_secs_f64()
        );
        WatchpointResult {
            hit: false,
            tick: self.current_tick(),
            addr,
            old_value: None,
            new_value: None,
            value: Some(self.state.read_mem(addr)),
            mem_addr: None,
            mem_val: None,
            context_hex: String::new(),
            registers: BTreeMap::new(),
        }
    }

    fn run_until(&mut self, cond: RunUntilCondition, max_ticks: u32) -> RunUntilResult {
        let start = self.current_tick();
        let t0 = std::time::Instant::now();

        let initial_prop_value: Option<i64> = match &cond {
            RunUntilCondition::PropertyChanges(name) => {
                self.evaluator.get_slot_value(name).map(|v| v as i64)
            }
            _ => None,
        };

        for _ in 0..max_ticks {
            self.step_one();

            let cs = self.state.get_var("CS").unwrap_or(0) & 0xFFFF;
            let ip = self.state.get_var("IP").unwrap_or(0) & 0xFFFF;
            let fetch_addr = cs * 16 + ip;
            let op_byte = self.state.read_mem(fetch_addr) & 0xFF;
            let next_byte = self.state.read_mem(fetch_addr + 1) & 0xFF;

            let matched: Option<serde_json::Value> = match &cond {
                RunUntilCondition::CsIp { cs: c, ip: i } => {
                    (cs == *c && ip == *i).then(|| serde_json::json!("cs_ip"))
                }
                RunUntilCondition::Cs(c) => (cs == *c).then(|| serde_json::json!("cs")),
                RunUntilCondition::IpRange { cs: c, min, max } => {
                    (cs == *c && ip >= *min && ip <= *max).then(|| serde_json::json!("ip_range"))
                }
                RunUntilCondition::Int(_) => {
                    (op_byte == 0xCD).then(|| serde_json::json!({"int": next_byte}))
                }
                RunUntilCondition::IntNum(n) => (op_byte == 0xCD && next_byte == *n)
                    .then(|| serde_json::json!({"int": next_byte})),
                RunUntilCondition::PropertyEquals { name, value } => {
                    match self.evaluator.get_slot_value(name) {
                        Some(v) if v as i64 == *value => {
                            Some(serde_json::json!({"prop": name, "value": v as i64}))
                        }
                        _ => None,
                    }
                }
                RunUntilCondition::PropertyChanges(name) => {
                    let cur = self.evaluator.get_slot_value(name).map(|v| v as i64);
                    if cur.is_some() && cur != initial_prop_value {
                        Some(serde_json::json!({"prop": name, "from": initial_prop_value, "to": cur}))
                    } else {
                        None
                    }
                }
                RunUntilCondition::MemByteEquals { addr, value } => {
                    ((self.state.read_mem(*addr) & 0xFF) == (*value & 0xFF))
                        .then(|| serde_json::json!({"addr": addr, "value": value}))
                }
            };

            if let Some(desc) = matched {
                let ax = self.state.get_var("AX").unwrap_or(0) & 0xFFFF;
                let ah = (ax >> 8) & 0xFF;
                let ticks_run = self.current_tick() - start;
                eprintln!(
                    "[run-until] hit at tick {} (+{}) CS:IP={:04x}:{:04x} op={:02x} next={:02x} AH={:02x} ({:.3}s)",
                    self.current_tick(),
                    ticks_run,
                    cs,
                    ip,
                    op_byte,
                    next_byte,
                    ah,
                    t0.elapsed().as_secs_f64()
                );
                return RunUntilResult {
                    hit: true,
                    tick: self.current_tick(),
                    ticks_run,
                    cs: Some(cs),
                    ip: Some(ip),
                    opcode: Some(op_byte),
                    next_byte: Some(next_byte),
                    ah: Some(ah),
                    matched: Some(desc),
                    registers: self.registers_map(),
                };
            }
        }

        eprintln!(
            "[run-until] no match after {} ticks ({:.3}s)",
            max_ticks,
            t0.elapsed().as_secs_f64()
        );
        RunUntilResult {
            hit: false,
            tick: self.current_tick(),
            ticks_run: self.current_tick() - start,
            cs: None,
            ip: None,
            opcode: None,
            next_byte: None,
            ah: None,
            matched: None,
            registers: BTreeMap::new(),
        }
    }

    fn compare_paths(&mut self) -> ComparePathsResult {
        let tick = self.current_tick();
        let saved_state = self.state.clone();

        let mut compiled_state = saved_state.clone();
        self.evaluator.tick(&mut compiled_state);
        let compiled_slots = self.evaluator.get_all_slot_values();

        let mut interpreted_state = saved_state.clone();
        self.evaluator.tick_interpreted(&mut interpreted_state);
        let interpreted_props = self.evaluator.get_all_interpreted_values();

        // Don't advance the real state.
        self.state = saved_state;

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

        let mut property_diffs = Vec::new();
        let mut all_prop_names: std::collections::BTreeSet<String> =
            compiled_slots.keys().cloned().collect();
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
        ComparePathsResult {
            tick,
            register_diffs,
            property_diffs,
            memory_diffs,
            total_diffs,
        }
    }

    fn trace_property(&mut self, property: &str) -> TracePropertyResult {
        let tick = self.current_tick();

        let mut interp_state = self.state.clone();
        self.evaluator.tick_interpreted(&mut interp_state);
        let interp_props = self.evaluator.get_all_interpreted_values();
        let interpreted_value = interp_props.get(property).copied();

        let (compiled_value, trace) = self
            .evaluator
            .trace_property(&self.state, property)
            .unwrap_or((0, Vec::new()));

        TracePropertyResult {
            tick,
            property: property.to_string(),
            compiled_value,
            interpreted_value,
            trace_entries: trace.len(),
            trace: serde_json::to_value(&trace).unwrap_or(serde_json::Value::Null),
        }
    }
}

// ---------------------------------------------------------------------------
// MCP handler — wraps DebugSession in a Mutex so all tools take `&self`.
//
// Mutating tool methods lock the mutex internally. All evaluator work is
// CPU-bound and synchronous; we don't yield across awaits while holding it.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct DebuggerHandler {
    /// None when no CSS file is loaded yet (the server was started with no
    /// `-i` flag). Every tool except `info` and `open` errors with
    /// NO_PROGRAM_LOADED until `open` is called.
    session: Arc<Mutex<Option<DebugSession>>>,
    /// CLI-provided base_interval, used when a new program is loaded via `open`.
    base_interval: u32,
    tool_router: ToolRouter<DebuggerHandler>,
}

fn invalid_params(msg: impl Into<String>) -> ErrorData {
    ErrorData::invalid_params(msg.into(), None)
}

fn no_program() -> ErrorData {
    ErrorData::invalid_params(
        std::borrow::Cow::Borrowed(
            "no program loaded — call the `open` tool with a CSS file path first",
        ),
        None,
    )
}

/// Parse + compile a CSS file into a fresh DebugSession. Extracted so `main`
/// and the `open` tool share the same loader path.
fn load_css(path: &std::path::Path, base_interval: u32) -> Result<DebugSession, String> {
    eprintln!("Loading {}...", path.display());
    let css = std::fs::read_to_string(path)
        .map_err(|e| format!("reading {}: {e}", path.display()))?;

    let t0 = std::time::Instant::now();
    let parsed =
        calcite_core::parser::parse_css(&css).map_err(|e| format!("parse error: {e}"))?;
    eprintln!(
        "Parsed: {} @property, {} @function, {} assignments ({:.2}s)",
        parsed.properties.len(),
        parsed.functions.len(),
        parsed.assignments.len(),
        t0.elapsed().as_secs_f64(),
    );

    let mut state = State::default();
    state.load_properties(&parsed.properties);

    let t1 = std::time::Instant::now();
    let evaluator = Evaluator::from_parsed(&parsed);
    eprintln!("Compiled in {:.2}s", t1.elapsed().as_secs_f64());

    let video_config = calcite_core::detect_video_memory();
    if let Some((addr, size)) = video_config {
        eprintln!("Video memory detected at 0x{:X} ({} bytes)", addr, size);
    }

    let property_names: Vec<String> = evaluator
        .assignments
        .iter()
        .map(|a| a.property.clone())
        .collect();

    Ok(DebugSession {
        evaluator,
        state: state.clone(),
        bases: vec![(0u32, state)],
        deltas: Vec::new(),
        base_interval,
        properties_count: parsed.properties.len(),
        functions_count: parsed.functions.len(),
        assignments_count: parsed.assignments.len(),
        css_file: path.display().to_string(),
        video_config,
        property_names,
    })
}

#[tool_router]
impl DebuggerHandler {
    fn new(session: Option<DebugSession>, base_interval: u32) -> Self {
        Self {
            session: Arc::new(Mutex::new(session)),
            base_interval,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Session metadata. Also works when no program is loaded — returns {loaded: false} so the client knows to call `open` first."
    )]
    fn info(&self, _params: Parameters<EmptyParams>) -> Result<Json<InfoResult>, ErrorData> {
        let guard = self.session.lock().unwrap();
        let base_interval = self.base_interval;
        match guard.as_ref() {
            Some(s) => Ok(Json(InfoResult {
                loaded: true,
                css_file: Some(s.css_file.clone()),
                current_tick: s.current_tick(),
                properties_count: s.properties_count,
                functions_count: s.functions_count,
                assignments_count: s.assignments_count,
                snapshots: s.bases.iter().map(|(t, _)| *t).collect(),
                base_interval,
            })),
            None => Ok(Json(InfoResult {
                loaded: false,
                css_file: None,
                current_tick: 0,
                properties_count: 0,
                functions_count: 0,
                assignments_count: 0,
                snapshots: vec![],
                base_interval,
            })),
        }
    }

    #[tool(
        description = "Load a CSS file into the debugger. Replaces any currently-loaded program. Required before any other tool (except `info`) can run if the server was started without -i."
    )]
    fn open(
        &self,
        Parameters(p): Parameters<OpenParams>,
    ) -> Result<Json<OpenResult>, ErrorData> {
        let path = std::path::PathBuf::from(&p.path);
        let new_session = load_css(&path, self.base_interval).map_err(invalid_params)?;
        let result = OpenResult {
            css_file: new_session.css_file.clone(),
            properties_count: new_session.properties_count,
            functions_count: new_session.functions_count,
            assignments_count: new_session.assignments_count,
        };
        // Swap in the new session, dropping the old one (and all its snapshots/deltas).
        *self.session.lock().unwrap() = Some(new_session);
        Ok(Json(result))
    }

    #[tool(
        description = "Current registers (state vars) and computed property values at the current tick."
    )]
    fn get_state(&self, _params: Parameters<EmptyParams>) -> Result<Json<StateResult>, ErrorData> {
        let guard = self.session.lock().unwrap();
        let s = guard.as_ref().ok_or_else(no_program)?;
        Ok(Json(StateResult {
            tick: s.current_tick(),
            registers: s.registers_map(),
            properties: s.properties_map(),
        }))
    }

    #[tool(
        description = "Advance the simulation by N ticks. Returns the new tick, count executed, and accumulated state-var changes."
    )]
    fn tick(&self, Parameters(p): Parameters<TickParams>) -> Result<Json<TickResult>, ErrorData> {
        let mut guard = self.session.lock().unwrap();
        let s = guard.as_mut().ok_or_else(no_program)?;
        let t0 = std::time::Instant::now();
        let resp = s.tick_n(p.count);
        let elapsed = t0.elapsed();
        eprintln!(
            "[tick] {} -> {} ({} ticks, {:.3}s)",
            resp.tick - resp.ticks_executed,
            resp.tick,
            resp.ticks_executed,
            elapsed.as_secs_f64()
        );
        Ok(Json(resp))
    }

    #[tool(
        description = "Seek to a specific tick. Restores the nearest snapshot at or before the target, then replays forward. Going backward also works (seeks via snapshot then replays)."
    )]
    fn seek(&self, Parameters(p): Parameters<SeekParams>) -> Result<Json<StateResult>, ErrorData> {
        let mut guard = self.session.lock().unwrap();
        let s = guard.as_mut().ok_or_else(no_program)?;
        let from = s.current_tick();
        let t0 = std::time::Instant::now();
        s.seek(p.tick);
        let elapsed = t0.elapsed();
        eprintln!(
            "[seek] {} -> {} ({:.3}s)",
            from,
            s.current_tick(),
            elapsed.as_secs_f64()
        );
        Ok(Json(StateResult {
            tick: s.current_tick(),
            registers: s.registers_map(),
            properties: s.properties_map(),
        }))
    }

    #[tool(description = "Read a range of bytes from 8086 memory. Returns hex, raw bytes, and LE-u16 view.")]
    fn read_memory(
        &self,
        Parameters(p): Parameters<MemoryParams>,
    ) -> Result<Json<MemoryResult>, ErrorData> {
        let guard = self.session.lock().unwrap();
        let s = guard.as_ref().ok_or_else(no_program)?;
        Ok(Json(s.read_memory(p.addr, p.len)))
    }

    #[tool(
        description = "Render a region of video memory as text (each cell is one character byte; attribute bytes ignored). Defaults to detected video region or 0xB8000 80x25."
    )]
    fn render_screen(
        &self,
        Parameters(p): Parameters<ScreenParams>,
    ) -> Result<Json<ScreenResult>, ErrorData> {
        let guard = self.session.lock().unwrap();
        let s = guard.as_ref().ok_or_else(no_program)?;
        let (addr, w, h) = (
            p.addr.unwrap_or_else(|| {
                s.video_config.map(|(a, _)| a as i32).unwrap_or(0xB8000)
            }),
            p.width.unwrap_or(80),
            p.height.unwrap_or(25),
        );
        Ok(Json(s.render_screen(addr, w, h)))
    }

    #[tool(
        description = "Compare execution against a reference trace. Each ref entry is {tick, registers: {NAME: value, ...}}. Stops at first divergence by default."
    )]
    fn compare_reference(
        &self,
        Parameters(p): Parameters<CompareReferenceParams>,
    ) -> Result<Json<CompareReferenceResult>, ErrorData> {
        let mut guard = self.session.lock().unwrap();
        let s = guard.as_mut().ok_or_else(no_program)?;
        s.seek(0);
        let mut divergences = Vec::new();
        let ticks_to_compare = p.reference.len() as u32;
        for ref_tick in &p.reference {
            while s.current_tick() < ref_tick.tick {
                s.tick_n(1);
            }
            if s.current_tick() == ref_tick.tick {
                s.tick_n(1);
            }
            for (name, expected) in &ref_tick.registers {
                let actual = s.state.get_var(name).unwrap_or(0) as i64;
                if actual != *expected {
                    divergences.push(DivergenceInfo {
                        tick: ref_tick.tick,
                        register: name.clone(),
                        expected: *expected,
                        actual,
                    });
                }
            }
            if p.stop_at_first && !divergences.is_empty() {
                break;
            }
        }
        Ok(Json(CompareReferenceResult {
            divergences,
            ticks_compared: ticks_to_compare,
        }))
    }

    #[tool(
        description = "Run one tick via both the compiled and interpreted paths and diff the results. Does not advance the session. Use to find compiler bugs."
    )]
    fn compare_paths(
        &self,
        _params: Parameters<EmptyParams>,
    ) -> Result<Json<ComparePathsResult>, ErrorData> {
        let mut guard = self.session.lock().unwrap();
        let s = guard.as_mut().ok_or_else(no_program)?;
        Ok(Json(s.compare_paths()))
    }

    #[tool(description = "Diff the current state against a supplied register/memory snapshot.")]
    fn compare_state(
        &self,
        Parameters(p): Parameters<CompareStateParams>,
    ) -> Result<Json<CompareStateResult>, ErrorData> {
        let guard = self.session.lock().unwrap();
        let s = guard.as_ref().ok_or_else(no_program)?;
        let mut register_diffs = Vec::new();
        let mut memory_diffs = Vec::new();

        if let Some(regs) = p.registers {
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

        if let Some(ranges) = p.memory {
            for range in ranges {
                for i in 0..range.len {
                    if i >= range.bytes.len() {
                        break;
                    }
                    let addr = range.addr + i as i32;
                    let actual = s.state.read_mem(addr) as u8;
                    let expected = range.bytes[i];
                    if actual != expected {
                        memory_diffs.push(MemDiffEntry { addr, expected, actual });
                    }
                }
            }
        }

        let total_diffs = register_diffs.len() + memory_diffs.len();
        Ok(Json(CompareStateResult {
            tick: s.current_tick(),
            register_diffs,
            memory_diffs,
            total_diffs,
        }))
    }

    #[tool(
        description = "Send a key into the machine. target=bda (default) pushes into the BIOS Data Area ring buffer; target=keyboard sets the --keyboard CSS state variable."
    )]
    fn send_key(
        &self,
        Parameters(p): Parameters<SendKeyParams>,
    ) -> Result<Json<SendKeyResult>, ErrorData> {
        let mut guard = self.session.lock().unwrap();
        let s = guard.as_mut().ok_or_else(no_program)?;
        let target = p.target.to_lowercase();
        let ok = match target.as_str() {
            "bda" => {
                s.state.bda_push_key(p.value);
                true
            }
            "keyboard" => s.state.set_var("keyboard", p.value),
            other => {
                return Err(invalid_params(format!(
                    "unknown target '{other}'; expected 'bda' or 'keyboard'"
                )))
            }
        };
        Ok(Json(SendKeyResult {
            tick: s.current_tick(),
            value: p.value,
            target,
            ok,
        }))
    }

    #[tool(
        description = "Trace every compiled op that contributes to a property at the current tick. Returns the compiled value, interpreted value (for cross-check), and op-by-op trace."
    )]
    fn trace_property(
        &self,
        Parameters(p): Parameters<TracePropertyParams>,
    ) -> Result<Json<TracePropertyResult>, ErrorData> {
        let mut guard = self.session.lock().unwrap();
        let s = guard.as_mut().ok_or_else(no_program)?;
        Ok(Json(s.trace_property(&p.property)))
    }

    #[tool(
        description = "Run forward until a memory byte changes (or matches an expected value). Optionally seek to from_tick first. Stops at max_ticks."
    )]
    fn watchpoint(
        &self,
        Parameters(p): Parameters<WatchpointParams>,
    ) -> Result<Json<WatchpointResult>, ErrorData> {
        let mut guard = self.session.lock().unwrap();
        let s = guard.as_mut().ok_or_else(no_program)?;
        if let Some(t) = p.from_tick {
            s.seek(t);
        }
        Ok(Json(s.watchpoint(p.addr, p.max_ticks, p.expected)))
    }

    #[tool(
        description = "Run forward until a condition matches. Conditions: cs_ip, cs, ip_range, int (any INT), int_num (specific INT), property_equals, property_changes, mem_byte_equals."
    )]
    fn run_until(
        &self,
        Parameters(p): Parameters<RunUntilParams>,
    ) -> Result<Json<RunUntilResult>, ErrorData> {
        let mut guard = self.session.lock().unwrap();
        let s = guard.as_mut().ok_or_else(no_program)?;
        if let Some(t) = p.from_tick {
            s.seek(t);
        }
        Ok(Json(s.run_until(p.condition, p.max_ticks)))
    }

    #[tool(
        description = "Dump compiled ops. Either by index range (start, end) or by property name. Mutually exclusive."
    )]
    fn dump_ops(
        &self,
        Parameters(p): Parameters<DumpOpsParams>,
    ) -> Result<Json<DumpOpsResult>, ErrorData> {
        let guard = self.session.lock().unwrap();
        let s = guard.as_ref().ok_or_else(no_program)?;
        match (p.property.as_deref(), p.start, p.end) {
            (Some(prop), None, None) => match s.evaluator.get_ops_for_property(prop) {
                Some(lines) => Ok(Json(DumpOpsResult {
                    ops: lines,
                    property: Some(prop.to_string()),
                    start: None,
                    end: None,
                })),
                None => Err(invalid_params(format!("property '{prop}' not found"))),
            },
            (None, Some(start), Some(end)) => Ok(Json(DumpOpsResult {
                ops: s.evaluator.dump_ops_range(start, end),
                property: None,
                start: Some(start),
                end: Some(end),
            })),
            (Some(_), Some(_), _) | (Some(_), _, Some(_)) => Err(invalid_params(
                "pass either property OR (start, end), not both",
            )),
            _ => Err(invalid_params("pass property, or both start and end")),
        }
    }

    #[tool(description = "List every compiled slot with its name and current value.")]
    fn slot_map(
        &self,
        _params: Parameters<EmptyParams>,
    ) -> Result<Json<SlotMapResult>, ErrorData> {
        let guard = self.session.lock().unwrap();
        let s = guard.as_ref().ok_or_else(no_program)?;
        let entries: Vec<SlotEntry> = s
            .evaluator
            .get_slot_map()
            .iter()
            .map(|(slot, name)| SlotEntry {
                slot: *slot,
                name: name.clone(),
                value: s.evaluator.get_slot_by_index(*slot),
            })
            .collect();
        Ok(Json(SlotMapResult { slots: entries }))
    }

    #[tool(
        description = "Snapshot (base) management. action='create' forces a full-state checkpoint at the current tick (in addition to the automatic ones at base_interval boundaries); action='list' enumerates existing bases. Deltas are tracked automatically every tick."
    )]
    fn snapshots(
        &self,
        Parameters(p): Parameters<SnapshotParams>,
    ) -> Result<Json<SnapshotResult>, ErrorData> {
        let mut guard = self.session.lock().unwrap();
        let s = guard.as_mut().ok_or_else(no_program)?;
        let action = p.action.to_lowercase();
        let created = match action.as_str() {
            "create" => {
                let tick = s.current_tick();
                if !s.bases.iter().any(|(t, _)| *t == tick) {
                    let cloned = s.state.clone();
                    s.bases.push((tick, cloned));
                }
                Some(tick)
            }
            "list" => None,
            other => {
                return Err(invalid_params(format!(
                    "unknown action '{other}'; expected 'create' or 'list'"
                )))
            }
        };
        Ok(Json(SnapshotResult {
            action,
            snapshots: s.bases.iter().map(|(t, _)| *t).collect(),
            created,
        }))
    }
}

#[tool_handler]
impl ServerHandler for DebuggerHandler {
    fn get_info(&self) -> ServerInfo {
        // ServerInfo / ServerCapabilities are #[non_exhaustive] — build via Default+assign.
        let mut info = ServerInfo::default();
        info.instructions = Some(
            "calcite-debugger MCP server. Drives one CSS program through tick-by-tick \
             execution. Use `info` for session state, `tick`/`seek` to navigate, \
             `get_state`/`read_memory`/`render_screen` to inspect, and `compare_paths` / \
             `trace_property` to find compiler/interpreter divergences."
                .into(),
        );
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // CRITICAL: stdout is reserved for MCP frames. All logging → stderr.
    env_logger::Builder::from_default_env()
        .target(env_logger::Target::Stderr)
        .init();

    let cli = Cli::parse();

    // If -i was given, load that file now. If not, start with no program —
    // the client will call the `open` tool to pick one.
    let initial_session = match &cli.input {
        Some(path) => Some(load_css(path, cli.base_interval).map_err(|e| -> Box<dyn std::error::Error> { e.into() })?),
        None => {
            eprintln!("No -i flag; starting without a loaded program. Use the `open` tool to load a CSS file.");
            None
        }
    };

    eprintln!("MCP server ready on stdio. Awaiting initialize...");

    let handler = DebuggerHandler::new(initial_session, cli.base_interval);
    let service = handler.serve(stdio()).await?;
    let quit_reason = service.waiting().await?;
    eprintln!("MCP server stopped: {:?}", quit_reason);
    Ok(())
}
