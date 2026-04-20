//! Evaluator unit tests for `Op::MemoryCopy`.
//!
//! Mirrors `memory_fill.rs` in shape: hand-crafted op streams executed
//! against a fresh State, asserting memory + slots. Exists to prove the
//! evaluator arm is correct independently of any detector.

use calcite_core::State;
use calcite_core::compile::{Op, exec_ops_for_test};

fn fresh_state_mib() -> State {
    // 1 MiB — covers the x86 real-mode address space.
    State::new(1 << 20)
}

#[test]
fn memory_copy_basic_contiguous() {
    // slots: 0=src (0x2000), 1=dst (0x3000), 2=count (64), 3=marker
    // pc 0: MemoryCopy → exit_target=2
    // pc 1: LoadLit dst=3, val=999 — must NOT run
    // pc 2: LoadLit dst=3, val=7 — runs once
    let ops = vec![
        Op::MemoryCopy { src_slot: 0, dst_slot: 1, count_slot: 2, exit_target: 2 },
        Op::LoadLit { dst: 3, val: 999 },
        Op::LoadLit { dst: 3, val: 7 },
    ];
    let mut state = fresh_state_mib();

    // Seed source with a recognisable pattern.
    for i in 0..64 {
        state.memory[0x2000 + i] = (i as u8).wrapping_add(0x40);
    }
    let mut slots: Vec<i32> = vec![0x2000, 0x3000, 64, 0];

    exec_ops_for_test(&ops, &mut state, &mut slots);

    // Destination matches source.
    for i in 0..64 {
        assert_eq!(
            state.memory[0x3000 + i],
            (i as u8).wrapping_add(0x40),
            "copied byte {}",
            i
        );
    }
    // Source is untouched (non-overlapping case).
    for i in 0..64 {
        assert_eq!(
            state.memory[0x2000 + i],
            (i as u8).wrapping_add(0x40),
            "source byte {} preserved",
            i
        );
    }
    // Boundary bytes untouched.
    assert_eq!(state.memory[0x3000 - 1], 0);
    assert_eq!(state.memory[0x3000 + 64], 0);

    // src/dst advanced, count zeroed.
    assert_eq!(slots[0], 0x2000 + 64);
    assert_eq!(slots[1], 0x3000 + 64);
    assert_eq!(slots[2], 0);
    // exit_target=2 reached; pc=1 skipped.
    assert_eq!(slots[3], 7);
}

#[test]
fn memory_copy_zero_count_no_op() {
    let ops = vec![
        Op::MemoryCopy { src_slot: 0, dst_slot: 1, count_slot: 2, exit_target: 1 },
        Op::LoadLit { dst: 3, val: 1 },
    ];
    let mut state = fresh_state_mib();
    state.memory[0x2000] = 0xAB;
    let baseline = state.memory.clone();
    let mut slots: Vec<i32> = vec![0x2000, 0x3000, 0, 0];

    exec_ops_for_test(&ops, &mut state, &mut slots);

    assert_eq!(state.memory, baseline, "zero count must not write any byte");
    assert_eq!(slots[0], 0x2000, "src unchanged when count == 0");
    assert_eq!(slots[1], 0x3000, "dst unchanged when count == 0");
    assert_eq!(slots[2], 0);
    assert_eq!(slots[3], 1, "exit_target reached");
}

#[test]
fn memory_copy_out_of_bounds_clips() {
    let ops = vec![
        Op::MemoryCopy { src_slot: 0, dst_slot: 1, count_slot: 2, exit_target: 1 },
        Op::LoadLit { dst: 3, val: 1 },
    ];
    let mut state = fresh_state_mib();
    let mem_len = state.memory.len() as i32;

    // Dst starts 50 bytes before end; try to copy 200 bytes.
    let dst_start = mem_len - 50;
    let src_start = 0x1000;
    // Seed source.
    for i in 0..200 {
        state.memory[(src_start + i) as usize] = 0x55;
    }
    let mut slots: Vec<i32> = vec![src_start, dst_start, 200, 0];

    exec_ops_for_test(&ops, &mut state, &mut slots);

    // Last 50 bytes should be 0x55 (copied); no panic.
    for i in 0..50 {
        assert_eq!(
            state.memory[(dst_start + i) as usize],
            0x55,
            "dst byte {} after clip",
            i
        );
    }
    // src/dst advance by full count logically.
    assert_eq!(slots[0], src_start.wrapping_add(200));
    assert_eq!(slots[1], dst_start.wrapping_add(200));
    assert_eq!(slots[2], 0);
}

#[test]
fn memory_copy_negative_dst_noop_memory() {
    let ops = vec![Op::MemoryCopy {
        src_slot: 0,
        dst_slot: 1,
        count_slot: 2,
        exit_target: 1,
    }];
    let mut state = fresh_state_mib();
    state.memory[0x1000] = 0xEE;
    let baseline = state.memory.clone();
    let mut slots: Vec<i32> = vec![0x1000, -4, 16, 0];

    exec_ops_for_test(&ops, &mut state, &mut slots);

    assert_eq!(state.memory, baseline, "negative dst must not write");
    assert_eq!(slots[0], 0x1000 + 16);
    assert_eq!(slots[1], -4 + 16);
    assert_eq!(slots[2], 0);
}

#[test]
fn memory_copy_negative_src_noop_memory() {
    let ops = vec![Op::MemoryCopy {
        src_slot: 0,
        dst_slot: 1,
        count_slot: 2,
        exit_target: 1,
    }];
    let mut state = fresh_state_mib();
    let baseline = state.memory.clone();
    let mut slots: Vec<i32> = vec![-8, 0x2000, 16, 0];

    exec_ops_for_test(&ops, &mut state, &mut slots);

    assert_eq!(state.memory, baseline, "negative src must not write");
    assert_eq!(slots[0], -8 + 16);
    assert_eq!(slots[1], 0x2000 + 16);
    assert_eq!(slots[2], 0);
}

#[test]
fn memory_copy_forward_overlap_dst_after_src() {
    // src = 0x1000, dst = 0x1004, count = 8
    // Bytes at 0x1000..0x1008 are [0,1,2,3,4,5,6,7].
    // copy_within copies [0..8] to [4..12], yielding at 0x1000..0x100C:
    //   [0,1,2,3, 0,1,2,3,4,5,6,7]
    // (i.e. the source view at the time of the copy is snapshotted byte-by-byte
    //  from index 0 upward, which `copy_within` does correctly even when dst > src).
    let ops = vec![Op::MemoryCopy {
        src_slot: 0,
        dst_slot: 1,
        count_slot: 2,
        exit_target: 1,
    }];
    let mut state = fresh_state_mib();
    for i in 0..8u8 {
        state.memory[0x1000 + i as usize] = i;
    }
    let mut slots: Vec<i32> = vec![0x1000, 0x1004, 8, 0];

    exec_ops_for_test(&ops, &mut state, &mut slots);

    // First 4 bytes: original source prefix untouched by the copy (dst starts at +4).
    let expected = [0u8, 1, 2, 3, 0, 1, 2, 3, 4, 5, 6, 7];
    for (i, want) in expected.iter().enumerate() {
        assert_eq!(
            state.memory[0x1000 + i],
            *want,
            "forward-overlap byte {} (dst>src)",
            i
        );
    }
    assert_eq!(slots[0], 0x1000 + 8);
    assert_eq!(slots[1], 0x1004 + 8);
    assert_eq!(slots[2], 0);
}

#[test]
fn memory_copy_backward_overlap_dst_before_src() {
    // src = 0x1004, dst = 0x1000, count = 8
    // Bytes at 0x1000..0x100C are [_,_,_,_, A,B,C,D,E,F,G,H] (A=0x40..H=0x47).
    // We leave first 4 bytes at 0. copy_within copies [4..12] to [0..8]:
    // Result: [A,B,C,D, E,F,G,H, E,F,G,H]
    // i.e. first 8 bytes become the source, last 4 (bytes 8..12) are a tail overlap that
    // (in the non-overlapping theoretical sense) would be unchanged. With copy_within,
    // since dst < src and no overlap in the copy direction past index 8, the tail
    // bytes from the original source at 0x1008..0x100C remain E,F,G,H.
    let ops = vec![Op::MemoryCopy {
        src_slot: 0,
        dst_slot: 1,
        count_slot: 2,
        exit_target: 1,
    }];
    let mut state = fresh_state_mib();
    for i in 0..8u8 {
        state.memory[0x1004 + i as usize] = 0x40 + i;
    }
    let mut slots: Vec<i32> = vec![0x1004, 0x1000, 8, 0];

    exec_ops_for_test(&ops, &mut state, &mut slots);

    // Dest region [0x1000..0x1008] = original source [0x40..0x48).
    for i in 0..8u8 {
        assert_eq!(
            state.memory[0x1000 + i as usize],
            0x40 + i,
            "backward-overlap dst byte {}",
            i
        );
    }
    // The original source region [0x1004..0x1008] got overwritten too by the
    // copy (because it overlaps the dst end). That's expected and identical
    // to what a forward byte-by-byte REP MOVSB loop would do.
    assert_eq!(slots[0], 0x1004 + 8);
    assert_eq!(slots[1], 0x1000 + 8);
    assert_eq!(slots[2], 0);
}
