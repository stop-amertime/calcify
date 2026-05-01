//! Integration tests for the script-primitive layer. Each test uses
//! the smallest possible CSS cabinet that exercises the primitive
//! end-to-end through the real Evaluator + State; no x86 content.
//!
//! These tests double as living documentation for the API: they show
//! exactly how a host (CLI, wasm bridge, future bench harness)
//! composes a WatchRegistry with run_batch_silent / poll to drive
//! measurements.

use calcite_core::eval::Evaluator;
use calcite_core::parser::parse_css;
use calcite_core::script::*;
use calcite_core::script_eval::poll;
use calcite_core::State;
use std::cell::Cell;

/// A 1-property cabinet that increments a counter every tick.
///
/// Includes a stub `--opcode` property because calcite-core's
/// `rep_fast_forward` post-tick hook panics on cabinets without that
/// slot (pre-existing issue documented in CSS-DOS LOGBOOK 2026-05-01).
/// Setting it to a non-string-op value (here 0x90 NOP) makes the hook
/// short-circuit cleanly.
const COUNTER_CSS: &str = r#"
@property --n {
    syntax: "<integer>";
    inherits: true;
    initial-value: 0;
}
@property --opcode {
    syntax: "<integer>";
    inherits: true;
    initial-value: 144;
}
.cpu {
    --n: calc(var(--n) + 1);
}
"#;

fn build(css: &str) -> (Evaluator, State) {
    let parsed = parse_css(css).expect("parse");
    let mut state = State::default();
    state.load_properties(&parsed.properties);
    let evaluator = Evaluator::from_parsed(&parsed);
    (evaluator, state)
}

/// Drive the engine tick-by-tick for `n_ticks`, polling the registry
/// after each tick. Returns the events drained from the registry.
fn run_with_polling(
    evaluator: &mut Evaluator,
    state: &mut State,
    reg: &mut WatchRegistry,
    n_ticks: u32,
) -> Vec<MeasurementEvent> {
    let mut events = Vec::new();
    for tick in 1..=n_ticks {
        evaluator.run_batch_silent(state, 1);
        poll(reg, state, tick);
        events.extend(reg.drain_events());
        if reg.halt_requested() {
            break;
        }
    }
    events
}

#[test]
fn stride_fires_every_n_ticks_against_real_engine() {
    let (mut ev, mut state) = build(COUNTER_CSS);
    let mut reg = WatchRegistry::new();
    reg.register(WatchSpec {
        name: "every100".to_string(),
        kind: WatchKind::Stride { every: 100 },
        gate: None,
        actions: vec![Action::Emit],
        sample_vars: vec!["n".to_string()],
    });
    let events = run_with_polling(&mut ev, &mut state, &mut reg, 1000);
    // 1000 ticks / 100 = 10 fires (at 100, 200, ..., 1000).
    assert_eq!(events.len(), 10);
    // Sampled --n values follow the same cadence.
    assert_eq!(events[0].sampled_vars, vec![("n".to_string(), 100)]);
    assert_eq!(events[9].sampled_vars, vec![("n".to_string(), 1000)]);
    assert_eq!(events[0].tick, 100);
    assert_eq!(events[9].tick, 1000);
}

#[test]
fn cond_fires_once_when_byte_predicate_holds() {
    // We don't need a CSS cabinet that writes the byte itself  the
    // test injects the byte transition via state.write_mem at tick
    // 500. This isolates the primitive from any cabinet shape.
    let (mut ev, mut state) = build(COUNTER_CSS);
    let mut reg = WatchRegistry::new();

    // Cheap stride watch that gates the expensive cond.
    reg.register(WatchSpec {
        name: "poll".to_string(),
        kind: WatchKind::Stride { every: 50 },
        gate: None,
        actions: vec![],
        sample_vars: vec![],
    });
    // Expensive cond: byte at 0x200 == 1. Gated by `poll`. Halts when
    // it fires.
    reg.register(WatchSpec {
        name: "byte_set".to_string(),
        kind: WatchKind::Cond {
            tests: vec![Predicate::ByteEq { addr: 0x200, val: 1 }],
            repeat: false,
            fired: Cell::new(false),
            last_held: Cell::new(false),
        },
        gate: Some("poll".to_string()),
        actions: vec![Action::Emit, Action::Halt],
        sample_vars: vec!["n".to_string()],
    });

    let mut events = Vec::new();
    let mut halted = false;
    for tick in 1..=1000 {
        if tick == 500 {
            state.write_mem(0x200, 1);
        }
        ev.run_batch_silent(&mut state, 1);
        poll(&mut reg, &mut state, tick);
        events.extend(reg.drain_events());
        if reg.halt_requested() {
            halted = true;
            break;
        }
    }
    assert!(halted, "cond should have requested halt");
    assert_eq!(events.len(), 1);
    // Byte was written before the run_batch+poll at tick 500; stride
    // 50 fires there, the gated cond evaluates and matches.
    assert_eq!(events[0].tick, 500);
    assert_eq!(events[0].watch_name, "byte_set");
    assert!(events[0].halted);
    assert_eq!(events[0].sampled_vars, vec![("n".to_string(), 500)]);
}

#[test]
fn edge_fires_on_zero_to_one_transition() {
    let (mut ev, mut state) = build(COUNTER_CSS);
    let mut reg = WatchRegistry::new();
    reg.register(WatchSpec {
        name: "poll".to_string(),
        kind: WatchKind::Stride { every: 1 },
        gate: None,
        actions: vec![],
        sample_vars: vec![],
    });
    reg.register(WatchSpec {
        name: "edge".to_string(),
        kind: WatchKind::Edge {
            addr: 0x300,
            last: Cell::new(0),
            primed: Cell::new(false),
        },
        gate: Some("poll".to_string()),
        actions: vec![Action::Emit],
        sample_vars: vec![],
    });

    let mut events = Vec::new();
    for tick in 1..=1000 {
        if tick == 700 {
            state.write_mem(0x300, 0xAB);
        }
        if tick == 800 {
            state.write_mem(0x300, 0x00);
        }
        ev.run_batch_silent(&mut state, 1);
        poll(&mut reg, &mut state, tick);
        events.extend(reg.drain_events());
    }
    // First tick primes the edge watch (records last=0); the 0->0xAB
    // transition fires at tick 700; the 0xAB->0 transition fires at 800.
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].tick, 700);
    assert_eq!(events[1].tick, 800);
}

#[test]
fn halt_kind_stops_run_and_emits() {
    let (mut ev, mut state) = build(COUNTER_CSS);
    let mut reg = WatchRegistry::new();
    reg.register(WatchSpec {
        name: "halt_byte".to_string(),
        kind: WatchKind::Halt { addr: 0x400 },
        gate: None,
        actions: vec![Action::Emit],
        sample_vars: vec![],
    });

    let mut events = Vec::new();
    let mut last_tick = 0u32;
    for tick in 1..=2000 {
        if tick == 1234 {
            state.write_mem(0x400, 0xFF);
        }
        ev.run_batch_silent(&mut state, 1);
        poll(&mut reg, &mut state, tick);
        events.extend(reg.drain_events());
        last_tick = tick;
        if reg.halt_requested() {
            break;
        }
    }
    assert_eq!(last_tick, 1234);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].tick, 1234);
    assert!(events[0].halted);
}

#[test]
fn at_kind_fires_exactly_once_at_target_tick() {
    let (mut ev, mut state) = build(COUNTER_CSS);
    let mut reg = WatchRegistry::new();
    reg.register(WatchSpec {
        name: "scheduled".to_string(),
        kind: WatchKind::At { tick: 333 },
        gate: None,
        actions: vec![Action::Emit, Action::SetVar {
            name: "n".to_string(),
            value: 999_999,
        }],
        sample_vars: vec![],
    });
    let events = run_with_polling(&mut ev, &mut state, &mut reg, 500);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].tick, 333);
    // SetVar action ran at tick 333 (after the 333rd run_batch); the
    // remaining 167 ticks (334..=500) each increment --n. So the
    // final value is 999_999 + 167 = 1_000_166.
    assert_eq!(state.get_var("n"), Some(1_000_166));
}

#[test]
fn dump_mem_range_attaches_payload_to_event() {
    let (mut ev, mut state) = build(COUNTER_CSS);
    // Pre-populate a 16-byte region.
    for i in 0..16 {
        state.write_mem(0x500 + i, (i + 1) as i32);
    }
    let mut reg = WatchRegistry::new();
    reg.set_dump_sink(DumpSink::Memory);
    reg.register(WatchSpec {
        name: "dump_at_100".to_string(),
        kind: WatchKind::At { tick: 100 },
        gate: None,
        actions: vec![Action::DumpMemRange {
            addr: 0x500,
            len: 16,
            path_template: "ignored".to_string(),
        }],
        sample_vars: vec![],
    });
    let events = run_with_polling(&mut ev, &mut state, &mut reg, 200);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].dumps.len(), 1);
    let dump = &events[0].dumps[0];
    assert_eq!(dump.tag, "mem");
    assert_eq!(dump.addr, 0x500);
    assert_eq!(dump.bytes, (1u8..=16).collect::<Vec<u8>>());
    // Memory sink leaves path None.
    assert!(dump.path.is_none());
}

#[test]
fn cond_with_byte_pattern_at() {
    // Compose the primitive that replaces the old vram_text:NEEDLE
    // helper: walking memory at (base, stride) for an ASCII needle.
    let (mut ev, mut state) = build(COUNTER_CSS);
    let mut reg = WatchRegistry::new();
    reg.register(WatchSpec {
        name: "poll".to_string(),
        kind: WatchKind::Stride { every: 25 },
        gate: None,
        actions: vec![],
        sample_vars: vec![],
    });
    reg.register(WatchSpec {
        name: "needle_visible".to_string(),
        kind: WatchKind::Cond {
            tests: vec![Predicate::BytePatternAt {
                base: 0x100,
                stride: 2,
                max_window: 200,
                needle: b"HELLO".to_vec(),
            }],
            repeat: false,
            fired: Cell::new(false),
            last_held: Cell::new(false),
        },
        gate: Some("poll".to_string()),
        actions: vec![Action::Emit, Action::Halt],
        sample_vars: vec![],
    });

    // Inject "HELLO" at stride 2 starting at 0x100 + 10, at tick 350.
    let mut halted = false;
    let mut events = Vec::new();
    for tick in 1..=1000 {
        if tick == 350 {
            for (i, &b) in b"HELLO".iter().enumerate() {
                state.write_mem(0x100 + 10 + (i as i32) * 2, b as i32);
            }
        }
        ev.run_batch_silent(&mut state, 1);
        poll(&mut reg, &mut state, tick);
        events.extend(reg.drain_events());
        if reg.halt_requested() {
            halted = true;
            break;
        }
    }
    assert!(halted);
    assert_eq!(events.len(), 1);
    // Stride 25; first poll-tick >= 350 is 350.
    assert_eq!(events[0].tick, 350);
    assert_eq!(events[0].watch_name, "needle_visible");
}
