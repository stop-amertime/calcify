//! Execution summary — per-tick event log + block segmenter + prose renderer.
//!
//! A diagnostic view orthogonal to `cycle_tracker` (which looks for exact
//! repetition) and single-tick inspection (which gives you one keyhole at a
//! time). The goal: given a run of N thousand ticks, produce a short list of
//! "blocks" describing what the CPU was doing at each phase, so a human (or
//! an LLM) can spot anomalies at a glance.
//!
//! ## Event log
//!
//! For each tick we capture `(cs, ip, opcode, cycle_count, irq_active, writes)`.
//! If the next tick has the same `(cs, ip)` (e.g. a `REP MOVSW` stuck on one
//! string op), we merge it into the previous event by bumping a `repeat`
//! counter and extending the write list — this is essential for REP strings
//! and for stall diagnosis (the interesting thing is *how long* we sat at
//! that PC, not N identical entries).
//!
//! ## Block segmenter
//!
//! Walks the event log and starts a new block on:
//! - CS change (new code segment — often a real phase transition).
//! - IRQ-active edge (0→1 or 1→0).
//! - IP wander beyond a ±window from the block's running IP range.
//! - Long gap in cycle_count (not currently used; reserved).
//!
//! ## Renderer
//!
//! For each block, a one-liner: tick range, CS, IP range, interrupt entries
//! by vector, and the broad memory regions that were written. Not a
//! disassembly, not a tick-by-tick trace. A map, not a transcript.

use crate::state::State;

/// One tick's worth of observation, REP-collapsed: `repeat` counts how many
/// consecutive ticks shared this `(cs, ip)`.
#[derive(Debug, Clone)]
pub struct Event {
    pub tick: u32,
    pub cs: i32,
    pub ip: i32,
    pub opcode: i32,
    pub irq_active: bool,
    pub repeat: u32,
    /// Up to 8 memory writes per tick (the V4 slot count). `addr` can be
    /// negative (state var) — we filter those at render time.
    pub writes: Vec<(i32, i32)>,
}

/// The recorder. Call `record_tick` once per tick after the evaluator has
/// run. Collapses consecutive same-PC ticks into a single `Event`.
#[derive(Debug, Default)]
pub struct EventLogger {
    pub events: Vec<Event>,
    pub config: SummaryConfig,
    /// True if we stopped recording because `config.max_events` was hit.
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct SummaryConfig {
    /// Max number of events to keep. When exceeded, recording stops (we
    /// prefer "first N ticks of the run" over ring-buffer semantics — the
    /// interesting thing is usually the *start*, not the latest loop).
    pub max_events: usize,
    /// Record memory writes into the event. Off by default because the V4
    /// slot scan adds measurable cost.
    pub record_writes: bool,
}

impl Default for SummaryConfig {
    fn default() -> Self {
        Self {
            max_events: 500_000,
            record_writes: true,
        }
    }
}

impl EventLogger {
    pub fn new(config: SummaryConfig) -> Self {
        Self {
            events: Vec::new(),
            config,
            truncated: false,
        }
    }

    /// Record the current tick. Reads CS/IP/opcode/_irqActive from the state
    /// and (if configured) the memAddr0..7 / memVal0..7 slots for writes.
    ///
    /// Idempotent for callers: if the previous event shares `(cs, ip)`, this
    /// merges into it instead of appending. That keeps REP loops as a single
    /// entry with a `repeat` count — essential for readable output.
    pub fn record_tick(&mut self, state: &State) {
        if self.truncated {
            return;
        }
        if self.events.len() >= self.config.max_events {
            self.truncated = true;
            return;
        }
        let cs = state.get_var("CS").unwrap_or(0);
        let ip = state.get_var("IP").unwrap_or(0) & 0xFFFF;
        let opcode = state.get_var("opcode").unwrap_or(0) & 0xFF;
        let irq_active = state.get_var("_irqActive").unwrap_or(0) != 0;

        // Merge into previous event if same PC — classic REP / HLT / stuck-
        // loop collapse. Don't merge across IRQ edges: an IRQ firing at the
        // same CS:IP as the previous tick is a distinct event (the handler
        // will overwrite IP but the override tick itself still has the old
        // IP visible until the push/jump applies).
        if let Some(last) = self.events.last_mut() {
            if last.cs == cs && last.ip == ip && last.irq_active == irq_active {
                last.repeat += 1;
                // Don't pile up writes forever — cap per-event so REPs don't
                // explode the log. A REP MOVSW over 64 KB would otherwise
                // carry 65k writes in one event.
                if self.config.record_writes && last.writes.len() < 64 {
                    let room = 64 - last.writes.len();
                    append_writes(state, &mut last.writes, room);
                }
                return;
            }
        }

        let mut writes = Vec::new();
        if self.config.record_writes {
            append_writes(state, &mut writes, 8);
        }
        self.events.push(Event {
            tick: state.frame_counter,
            cs,
            ip,
            opcode,
            irq_active,
            repeat: 1,
            writes,
        });
    }
}

/// Pull --memAddrK/--memValK pairs (width-aware) out of state for the diagnostic
/// event log. Skips dead slots (addr < 0). Caps at `limit`.
///
/// CSS-DOS V4 currently exposes 3 slots. We probe 0..=7 to be tolerant of
/// future cabinets that change the slot count without needing to update this
/// path — `state.get_var` returns None for absent properties so the loop
/// naturally stops short.
///
/// Width handling: when --_slotKWidth is 2, the slot writes a 16-bit word
/// with lo at addr and hi at addr+1. We record both byte-writes so the event
/// log matches what the splice paths actually do to memory.
fn append_writes(state: &State, out: &mut Vec<(i32, i32)>, limit: usize) {
    if limit == 0 {
        return;
    }
    let mut added = 0;
    for i in 0..8 {
        if added >= limit {
            break;
        }
        let addr_name = format!("memAddr{i}");
        let val_name = format!("memVal{i}");
        let width_name = format!("_slot{i}Width");
        let Some(addr) = state.get_var(&addr_name) else {
            continue;
        };
        if addr < 0 {
            continue;
        }
        let val = state.get_var(&val_name).unwrap_or(0);
        let width = state.get_var(&width_name).unwrap_or(1);
        if width == 2 {
            out.push((addr, val & 0xFF));
            added += 1;
            if added >= limit {
                break;
            }
            out.push((addr + 1, (val >> 8) & 0xFF));
            added += 1;
        } else {
            out.push((addr, val & 0xFF));
            added += 1;
        }
    }
}

/// A coherent region of execution. Built by walking the event log.
#[derive(Debug, Clone)]
pub struct Block {
    pub start_tick: u32,
    pub end_tick: u32,
    pub cs: i32,
    pub ip_min: i32,
    pub ip_max: i32,
    /// `vector -> count`. An INT entry is detected by IRQ-active going 0→1.
    pub interrupts: std::collections::BTreeMap<i32, u32>,
    /// Coarse regions that were written: `(lo, hi, count)` in 256-byte
    /// buckets, collapsed to adjacent ranges. Keeps the summary short.
    pub write_regions: Vec<(i32, i32, u32)>,
    /// How many events the block covers. `repeat` counts are summed, so
    /// `total_ticks = sum(e.repeat)`.
    pub total_ticks: u32,
    pub event_count: u32,
    /// If the block spent most of its time at one `(cs, ip)`, that PC and
    /// the fraction of total_ticks it represents. Useful for spotting
    /// stalls ("93% of the block was CS:IP=0FC8:0BE0").
    pub dominant_pc: Option<(i32, i32, u32)>,
}

/// Reasons a new block starts. Exposed for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockBreak {
    CsChange,
    IrqEdge,
    IpWander,
}

/// Segmentation parameters.
#[derive(Debug, Clone)]
pub struct SegmentConfig {
    /// IP range permitted within a block before we break. Typical code runs
    /// stay within a few hundred bytes of entry; a big jump usually means a
    /// new routine.
    pub ip_window: i32,
}

impl Default for SegmentConfig {
    fn default() -> Self {
        Self { ip_window: 256 }
    }
}

pub fn segment(events: &[Event], config: &SegmentConfig) -> Vec<Block> {
    let mut blocks = Vec::new();
    if events.is_empty() {
        return blocks;
    }

    let mut current: Option<BlockBuilder> = None;
    for ev in events {
        let should_break = match &current {
            None => false,
            Some(b) => {
                if b.cs != ev.cs {
                    true
                } else if b.last_irq != ev.irq_active {
                    // Edge — break either direction so entry and exit both
                    // appear as distinct blocks.
                    true
                } else {
                    let lo = b.ip_min.min(ev.ip);
                    let hi = b.ip_max.max(ev.ip);
                    hi - lo > config.ip_window
                }
            }
        };
        if should_break {
            if let Some(b) = current.take() {
                blocks.push(b.finish());
            }
        }
        match &mut current {
            Some(b) => b.push(ev),
            None => current = Some(BlockBuilder::start(ev)),
        }
    }
    if let Some(b) = current {
        blocks.push(b.finish());
    }
    blocks
}

struct BlockBuilder {
    start_tick: u32,
    end_tick: u32,
    cs: i32,
    ip_min: i32,
    ip_max: i32,
    last_irq: bool,
    /// Detect IRQ entry (`irq_active` goes 0→1). The first event of the
    /// block sets this; subsequent 0→1 transitions increment `interrupts`.
    interrupts: std::collections::BTreeMap<i32, u32>,
    writes: Vec<(i32, i32)>,
    total_ticks: u32,
    event_count: u32,
    /// `(cs, ip) -> sum of repeat`. Kept small by merging REP runs at
    /// record time, so typical blocks have <100 entries.
    pc_hits: std::collections::HashMap<(i32, i32), u32>,
}

impl BlockBuilder {
    fn start(ev: &Event) -> Self {
        let mut pc_hits = std::collections::HashMap::new();
        pc_hits.insert((ev.cs, ev.ip), ev.repeat);
        let mut interrupts = std::collections::BTreeMap::new();
        if ev.irq_active {
            // Block entry mid-IRQ: we don't know the vector from state alone
            // (it's already been dispatched), so record under -1 as "unknown".
            *interrupts.entry(-1).or_insert(0) += 1;
        }
        Self {
            start_tick: ev.tick,
            end_tick: ev.tick + ev.repeat.saturating_sub(1),
            cs: ev.cs,
            ip_min: ev.ip,
            ip_max: ev.ip,
            last_irq: ev.irq_active,
            interrupts,
            writes: ev.writes.clone(),
            total_ticks: ev.repeat,
            event_count: 1,
            pc_hits,
        }
    }

    fn push(&mut self, ev: &Event) {
        if !self.last_irq && ev.irq_active {
            // Rising edge: the opcode at this tick is the vector number from
            // the INT instruction if software-initiated, but hardware IRQs
            // (PIT, keyboard) come in via the `--_irqActive` override path
            // — we don't have the vector directly. Record opcode as a proxy;
            // for software INTs the opcode byte is 0xCD and the next byte
            // (the vector) lives in memory. For now: use opcode as a rough
            // key, and let the renderer label 0xCD specially.
            *self.interrupts.entry(ev.opcode).or_insert(0) += 1;
        }
        self.last_irq = ev.irq_active;
        if ev.ip < self.ip_min {
            self.ip_min = ev.ip;
        }
        if ev.ip > self.ip_max {
            self.ip_max = ev.ip;
        }
        self.end_tick = ev.tick + ev.repeat.saturating_sub(1);
        self.total_ticks = self.total_ticks.saturating_add(ev.repeat);
        self.event_count += 1;
        *self.pc_hits.entry((ev.cs, ev.ip)).or_insert(0) += ev.repeat;
        // Keep writes cheap — cap per block.
        if self.writes.len() < 256 {
            for w in &ev.writes {
                if self.writes.len() >= 256 {
                    break;
                }
                self.writes.push(*w);
            }
        }
    }

    fn finish(self) -> Block {
        // Find dominant PC (if one cs:ip accounts for ≥60% of total ticks).
        let mut dominant_pc = None;
        if self.total_ticks > 0 {
            if let Some((&(cs, ip), &hits)) =
                self.pc_hits.iter().max_by_key(|(_, &v)| v)
            {
                if hits * 100 / self.total_ticks >= 60 {
                    dominant_pc = Some((cs, ip, hits));
                }
            }
        }
        Block {
            start_tick: self.start_tick,
            end_tick: self.end_tick,
            cs: self.cs,
            ip_min: self.ip_min,
            ip_max: self.ip_max,
            interrupts: self.interrupts,
            write_regions: bucket_writes(&self.writes),
            total_ticks: self.total_ticks,
            event_count: self.event_count,
            dominant_pc,
        }
    }
}

/// Bucket writes into coarse regions (256-byte granularity), then collapse
/// adjacent buckets into ranges. Returns `(lo, hi, count)`.
fn bucket_writes(writes: &[(i32, i32)]) -> Vec<(i32, i32, u32)> {
    use std::collections::BTreeMap;
    let mut buckets: BTreeMap<i32, u32> = BTreeMap::new();
    for &(addr, _) in writes {
        if addr < 0 {
            continue; // state-var slots, not interesting for region view
        }
        let bucket = addr & !0xFF;
        *buckets.entry(bucket).or_insert(0) += 1;
    }
    let mut out = Vec::new();
    let mut current: Option<(i32, i32, u32)> = None;
    for (&b, &n) in &buckets {
        match current {
            Some((lo, hi, count)) if b == hi + 0x100 => {
                current = Some((lo, b, count + n));
            }
            _ => {
                if let Some(c) = current {
                    out.push(c);
                }
                current = Some((b, b, n));
            }
        }
    }
    if let Some(c) = current {
        out.push(c);
    }
    out
}

/// Human-readable single-block line. Lives here so the debugger and CLI
/// render consistently.
pub fn render_block(b: &Block) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    write!(
        s,
        "ticks {:>8}-{:<8} CS={:04X} IP={:04X}-{:04X} ({} events, {} ticks)",
        b.start_tick,
        b.end_tick,
        b.cs & 0xFFFF,
        b.ip_min & 0xFFFF,
        b.ip_max & 0xFFFF,
        b.event_count,
        b.total_ticks,
    )
    .unwrap();
    if let Some((cs, ip, hits)) = b.dominant_pc {
        let pct = hits * 100 / b.total_ticks.max(1);
        write!(s, " [{}% @ {:04X}:{:04X}]", pct, cs & 0xFFFF, ip & 0xFFFF).unwrap();
    }
    if !b.interrupts.is_empty() {
        s.push_str(" irq:");
        for (k, v) in &b.interrupts {
            if *k == -1 {
                write!(s, " pre({})", v).unwrap();
            } else if *k == 0xCD {
                write!(s, " INT({})", v).unwrap();
            } else {
                write!(s, " op{:02X}({})", k & 0xFF, v).unwrap();
            }
        }
    }
    if !b.write_regions.is_empty() {
        s.push_str(" writes:");
        for (lo, hi, n) in &b.write_regions {
            if lo == hi {
                write!(s, " {:05X}({})", lo, n).unwrap();
            } else {
                write!(s, " {:05X}-{:05X}({})", lo, hi + 0xFF, n).unwrap();
            }
        }
    }
    s
}

/// Full-run summary string. One block per line.
pub fn render_summary(blocks: &[Block]) -> String {
    blocks
        .iter()
        .map(render_block)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::State;

    fn test_state() -> State {
        let mut s = State::new(0x1000);
        s.state_var_names = vec![
            "CS".to_string(),
            "IP".to_string(),
            "opcode".to_string(),
            "_irqActive".to_string(),
            "memAddr0".to_string(),
            "memVal0".to_string(),
        ];
        for (i, name) in s.state_var_names.iter().enumerate() {
            s.state_var_index.insert(name.clone(), i);
        }
        s.state_vars = vec![0; s.state_var_names.len()];
        s
    }

    fn set(s: &mut State, name: &str, v: i32) {
        let i = s.state_var_index[name];
        s.state_vars[i] = v;
    }

    #[test]
    fn collapses_consecutive_same_pc() {
        let mut s = test_state();
        let mut log = EventLogger::new(SummaryConfig::default());
        set(&mut s, "CS", 0xF000);
        set(&mut s, "IP", 0x0100);
        log.record_tick(&s);
        s.frame_counter += 1;
        log.record_tick(&s);
        s.frame_counter += 1;
        log.record_tick(&s);
        assert_eq!(log.events.len(), 1);
        assert_eq!(log.events[0].repeat, 3);
    }

    #[test]
    fn new_event_on_ip_change() {
        let mut s = test_state();
        let mut log = EventLogger::new(SummaryConfig::default());
        set(&mut s, "CS", 0xF000);
        set(&mut s, "IP", 0x0100);
        log.record_tick(&s);
        set(&mut s, "IP", 0x0103);
        s.frame_counter += 1;
        log.record_tick(&s);
        assert_eq!(log.events.len(), 2);
    }

    #[test]
    fn block_breaks_on_cs_change() {
        let events = vec![
            Event {
                tick: 0,
                cs: 0xF000,
                ip: 0,
                opcode: 0,
                irq_active: false,
                repeat: 10,
                writes: vec![],
            },
            Event {
                tick: 10,
                cs: 0x0070,
                ip: 0,
                opcode: 0,
                irq_active: false,
                repeat: 5,
                writes: vec![],
            },
        ];
        let blocks = segment(&events, &SegmentConfig::default());
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].cs, 0xF000);
        assert_eq!(blocks[1].cs, 0x0070);
    }

    #[test]
    fn block_breaks_on_ip_wander() {
        let events = vec![
            Event {
                tick: 0,
                cs: 0xF000,
                ip: 0x0100,
                opcode: 0,
                irq_active: false,
                repeat: 5,
                writes: vec![],
            },
            Event {
                tick: 5,
                cs: 0xF000,
                ip: 0x0110,
                opcode: 0,
                irq_active: false,
                repeat: 5,
                writes: vec![],
            },
            // 0x800 > 256 window — should break
            Event {
                tick: 10,
                cs: 0xF000,
                ip: 0x0900,
                opcode: 0,
                irq_active: false,
                repeat: 5,
                writes: vec![],
            },
        ];
        let blocks = segment(&events, &SegmentConfig::default());
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn dominant_pc_flagged() {
        let events: Vec<Event> = (0..2)
            .map(|i| Event {
                tick: i * 100,
                cs: 0xF000,
                ip: 0x0100 + (i as i32) * 3,
                opcode: 0,
                irq_active: false,
                repeat: if i == 0 { 950 } else { 50 },
                writes: vec![],
            })
            .collect();
        let blocks = segment(&events, &SegmentConfig::default());
        assert_eq!(blocks.len(), 1);
        let d = blocks[0].dominant_pc.unwrap();
        assert_eq!(d.0, 0xF000);
        assert_eq!(d.1, 0x0100);
    }
}
