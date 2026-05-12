use calcite_core::parser::parse_css;
use calcite_core::State;
use calcite_debug_summary::{render_summary, segment, EventLogger, SegmentConfig, SummaryConfig};

/// End-to-end: a fake CPU that walks three "phases" (splash, boot, loop),
/// fed through the EventLogger and block segmenter. Verifies the whole
/// chain — record, collapse, segment, render — behaves for a realistic
/// multi-phase run.
#[test]
fn summary_captures_three_phases() {
    let mut state = State::default();
    let props_css = r#"
        @property --CS { syntax: "<integer>"; inherits: true; initial-value: 0; }
        @property --IP { syntax: "<integer>"; inherits: true; initial-value: 0; }
        @property --opcode { syntax: "<integer>"; inherits: true; initial-value: 0; }
        @property --_irqActive { syntax: "<integer>"; inherits: true; initial-value: 0; }
    "#;
    let parsed = parse_css(props_css).unwrap();
    state.load_properties(&parsed.properties);

    let mut log = EventLogger::new(SummaryConfig {
        max_events: 10_000,
        record_writes: false,
    });

    state.set_var("CS", 0xF000);
    for i in 0..500 {
        state.set_var("IP", 0x100 + (i % 100));
        state.frame_counter = i as u32;
        log.record_tick(&state);
    }

    for i in 0..300 {
        state.set_var("IP", 0x300);
        state.frame_counter = 500 + i as u32;
        log.record_tick(&state);
    }

    state.set_var("CS", 0x0070);
    for i in 0..200 {
        state.set_var("IP", 0x400 + (i % 50));
        state.frame_counter = 800 + i as u32;
        log.record_tick(&state);
    }

    let blocks = segment(&log.events, &SegmentConfig::default());
    assert!(
        blocks.len() >= 3,
        "expected ≥3 blocks across the 3 phases, got {}: {:?}",
        blocks.len(),
        blocks.iter().map(|b| (b.cs, b.start_tick, b.end_tick)).collect::<Vec<_>>()
    );

    let stall = blocks.iter().find(|b| b.total_ticks == 300)
        .expect("should find the 300-tick stall block");
    assert!(stall.dominant_pc.is_some(), "stall should have dominant PC");
    let (cs, ip, _hits) = stall.dominant_pc.unwrap();
    assert_eq!(cs, 0xF000);
    assert_eq!(ip, 0x300);

    assert!(blocks.iter().any(|b| b.cs == 0x0070 && b.total_ticks == 200),
        "expected a CS=0x70 block with 200 ticks");

    let prose = render_summary(&blocks);
    assert!(prose.lines().count() == blocks.len());
}
