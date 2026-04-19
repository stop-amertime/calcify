//! Evaluator unit tests for `Op::MemoryFill`.
//!
//! These bypass parse/compile: we hand-craft the op stream, execute it
//! against a fresh State, and assert on memory + slots. They exist to
//! prove the evaluator arm is correct independently of any detector.

use calcite_core::State;
use calcite_core::compile::{Op, exec_ops_for_test};

fn fresh_state_mib() -> State {
    // 1 MiB — covers the x86 real-mode address space so VGA-style fills
    // at 0xA0000 are in range.
    State::new(1 << 20)
}

#[test]
fn memory_fill_basic_fills_range_and_updates_slots() {
    // slots: 0=dst (0xA0000), 1=val (0x42), 2=count (128), 3=marker
    // pc 0: MemoryFill → jumps to exit_target=2
    // pc 1: LoadLit dst=3, val=999 — must NOT run
    // pc 2: LoadLit dst=3, val=7 — runs once, then falls off the end
    let ops = vec![
        Op::MemoryFill { dst_slot: 0, val_slot: 1, count_slot: 2, exit_target: 2 },
        Op::LoadLit { dst: 3, val: 999 },
        Op::LoadLit { dst: 3, val: 7 },
    ];
    let mut state = fresh_state_mib();
    let mut slots: Vec<i32> = vec![0xA0000, 0x42, 128, 0];

    exec_ops_for_test(&ops, &mut state, &mut slots);

    // Byte before fill untouched.
    assert_eq!(state.memory[0xA0000 - 1], 0, "byte before fill untouched");
    // Fill region matches.
    for i in 0..128 {
        assert_eq!(state.memory[0xA0000 + i], 0x42, "byte {} in fill region", i);
    }
    // Byte after fill untouched.
    assert_eq!(state.memory[0xA0000 + 128], 0, "byte immediately after fill untouched");

    // dst advanced, count zeroed.
    assert_eq!(slots[0], 0xA0000 + 128);
    assert_eq!(slots[2], 0);
    // exit_target=2 reached; pc=1 skipped.
    assert_eq!(slots[3], 7, "should have executed the op at exit_target=2, not pc=1");
}

#[test]
fn memory_fill_zero_count_no_writes() {
    let ops = vec![
        Op::MemoryFill { dst_slot: 0, val_slot: 1, count_slot: 2, exit_target: 1 },
        Op::LoadLit { dst: 3, val: 1 },
    ];
    let mut state = fresh_state_mib();
    let baseline = state.memory.clone();
    let mut slots: Vec<i32> = vec![0x1000, 0xFF, 0, 0];

    exec_ops_for_test(&ops, &mut state, &mut slots);

    assert_eq!(state.memory, baseline, "zero count must not write any byte");
    assert_eq!(slots[0], 0x1000, "dst unchanged when count == 0");
    assert_eq!(slots[2], 0);
    assert_eq!(slots[3], 1, "exit_target reached");
}

#[test]
fn memory_fill_out_of_bounds_clips() {
    let ops = vec![
        Op::MemoryFill { dst_slot: 0, val_slot: 1, count_slot: 2, exit_target: 1 },
        Op::LoadLit { dst: 3, val: 1 },
    ];
    let mut state = fresh_state_mib();
    let mem_len = state.memory.len() as i32;
    // Start 50 bytes before the end, try to write 100 bytes.
    let start = mem_len - 50;
    let mut slots: Vec<i32> = vec![start, 0x33, 100, 0];

    exec_ops_for_test(&ops, &mut state, &mut slots);

    // Last 50 bytes of memory should be 0x33; no panic.
    for i in 0..50 {
        assert_eq!(
            state.memory[(start + i) as usize],
            0x33,
            "byte {} after clip-start",
            i
        );
    }
    // dst still advances by the full count (matches what the loop would
    // have done — logical iteration count is N regardless of OOB).
    assert_eq!(slots[0], start.wrapping_add(100));
    assert_eq!(slots[2], 0);
}

#[test]
fn memory_fill_negative_dst_noop_memory() {
    let ops = vec![Op::MemoryFill {
        dst_slot: 0,
        val_slot: 1,
        count_slot: 2,
        exit_target: 1,
    }];
    let mut state = fresh_state_mib();
    let baseline = state.memory.clone();
    let mut slots: Vec<i32> = vec![-4, 0x77, 16, 0];

    exec_ops_for_test(&ops, &mut state, &mut slots);

    assert_eq!(state.memory, baseline, "negative dst must not write");
    // dst += count: -4 + 16 = 12.
    assert_eq!(slots[0], 12);
    assert_eq!(slots[2], 0);
}

#[test]
fn memory_fill_value_is_masked_to_byte() {
    let ops = vec![Op::MemoryFill {
        dst_slot: 0,
        val_slot: 1,
        count_slot: 2,
        exit_target: 1,
    }];
    let mut state = fresh_state_mib();
    // 0x12345678 masked to byte → 0x78.
    let mut slots: Vec<i32> = vec![0x2000, 0x12345678, 4, 0];

    exec_ops_for_test(&ops, &mut state, &mut slots);

    for i in 0..4 {
        assert_eq!(state.memory[0x2000 + i], 0x78, "low byte only");
    }
}
