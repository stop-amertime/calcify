//! Post-tick REP fast-forward recognizer tests.
//!
//! These exercise `compile::rep_fast_forward` indirectly by invoking it
//! through `compile::execute` with a minimally-rigged CompiledProgram whose
//! main ops, writeback, and broadcast_writes are all empty. The rigging
//! captures only the "end of tick" portion of execute() — which is exactly
//! where the recognizer runs — and lets us assert on state mutations.
//!
//! Pre-conditions we set on State are the POST-ONE-ITERATION values that the
//! CSS would have produced: CX already decremented, DI already advanced,
//! the first byte/word already written. The recognizer's job is to collapse
//! the remaining CX iterations.

use calcite_core::State;
use calcite_core::compile::{execute, CompiledProgram};

/// Hand-roll a minimal CompiledProgram. Everything is empty — the only work
/// `execute()` does is call `rep_fast_forward(&mut state)` at the very end.
fn empty_program() -> CompiledProgram {
    CompiledProgram {
        ops: Vec::new(),
        slot_count: 0,
        writeback: Vec::new(),
        broadcast_writes: Vec::new(),
        dispatch_tables: Vec::new(),
        chain_tables: Vec::new(),
        flat_dispatch_arrays: Vec::new(),
        property_slots: std::collections::HashMap::new(),
        functions: Vec::new(),
    }
}

/// Load a fresh 1 MiB state with the state-var names the recognizer looks up.
/// The names are defined in the order the recognizer expects (bare, no --).
fn rigged_state() -> State {
    use calcite_core::types::{PropertyDef, PropertySyntax, CssValue};
    let mut state = State::new(1 << 20);
    // Names the recognizer reads/writes. Order doesn't matter for lookup.
    let names = [
        "opcode", "hasREP", "repType", "hasSegOverride",
        "CX", "DI", "SI", "IP",
        "AX", "ES", "DS", "flags", "cycleCount",
        "AL",
    ];
    let props: Vec<PropertyDef> = names
        .iter()
        .map(|n| PropertyDef {
            name: format!("--{}", n),
            syntax: PropertySyntax::Integer,
            inherits: false,
            initial_value: Some(CssValue::Integer(0)),
        })
        .collect();
    state.load_properties(&props);
    state
}

#[test]
fn rep_stosb_fast_forwards_remaining_iterations() {
    let mut state = rigged_state();
    let prog = empty_program();
    let mut slots = Vec::new();

    // Simulate a REP STOSB that did 1 iteration of a 256-byte fill at ES:DI=
    // 0xA000:0000, with AL=0x42. The CSS wrote byte 0 already and incremented
    // DI to 1 while decrementing CX to 255.
    state.memory[0xA0000] = 0x42; // already-written byte from the tick
    state.set_var("opcode", 0xAA);
    state.set_var("hasREP", 1);
    state.set_var("repType", 1);
    state.set_var("hasSegOverride", 0);
    state.set_var("CX", 255);
    state.set_var("DI", 1);
    state.set_var("ES", 0xA000);
    state.set_var("IP", 0x100); // IP held at REP prefix
    state.set_var("flags", 0);
    state.set_var("AL", 0x42);

    execute(&prog, &mut state, &mut slots);

    // All 256 bytes should be 0x42 now.
    for i in 0..256 {
        assert_eq!(state.memory[0xA0000 + i], 0x42, "byte {} post fast-forward", i);
    }
    // Byte after untouched.
    assert_eq!(state.memory[0xA0000 + 256], 0);
    // DI advanced, CX zeroed, IP past prefix+opcode.
    assert_eq!(state.get_var("DI"), Some(256));
    assert_eq!(state.get_var("CX"), Some(0));
    assert_eq!(state.get_var("IP"), Some(0x102));
    // cycleCount got 255 * 10.
    assert_eq!(state.get_var("cycleCount"), Some(255 * 10));
}

#[test]
fn rep_stosw_fast_forwards_word_fill() {
    let mut state = rigged_state();
    let prog = empty_program();
    let mut slots = Vec::new();

    // REP STOSW (0xAB), AX=0xBEEF, filled 1 word already. 3 iterations left.
    state.memory[0xA0000] = 0xEF;
    state.memory[0xA0001] = 0xBE;
    state.set_var("opcode", 0xAB);
    state.set_var("hasREP", 1);
    state.set_var("repType", 1);
    state.set_var("CX", 3);
    state.set_var("DI", 2);
    state.set_var("ES", 0xA000);
    state.set_var("IP", 0x100);
    state.set_var("AX", 0xBEEF);

    execute(&prog, &mut state, &mut slots);

    // 4 words total, each 0xBEEF little-endian → EF BE EF BE EF BE EF BE.
    let expected = [0xEFu8, 0xBE, 0xEF, 0xBE, 0xEF, 0xBE, 0xEF, 0xBE];
    for (i, want) in expected.iter().enumerate() {
        assert_eq!(state.memory[0xA0000 + i], *want, "word-fill byte {}", i);
    }
    assert_eq!(state.memory[0xA0000 + 8], 0); // untouched after
    assert_eq!(state.get_var("DI"), Some(8));
    assert_eq!(state.get_var("CX"), Some(0));
    assert_eq!(state.get_var("IP"), Some(0x102));
}

#[test]
fn rep_movsb_fast_forwards_copy() {
    let mut state = rigged_state();
    let prog = empty_program();
    let mut slots = Vec::new();

    // Seed source (DS=0x2000, SI=0) with a distinct pattern.
    let src_base = 0x20000;
    let dst_base = 0x30000;
    for i in 0..16u8 {
        state.memory[src_base + i as usize] = 0x40 + i;
    }
    // Simulate 1 byte already copied and SI/DI bumped to 1, CX=15.
    state.memory[dst_base] = state.memory[src_base];
    state.set_var("opcode", 0xA4);
    state.set_var("hasREP", 1);
    state.set_var("repType", 1);
    state.set_var("CX", 15);
    state.set_var("SI", 1);
    state.set_var("DI", 1);
    state.set_var("DS", 0x2000);
    state.set_var("ES", 0x3000);
    state.set_var("IP", 0x100);

    execute(&prog, &mut state, &mut slots);

    for i in 0..16u8 {
        assert_eq!(
            state.memory[dst_base + i as usize],
            0x40 + i,
            "copied byte {}",
            i
        );
    }
    assert_eq!(state.get_var("SI"), Some(16));
    assert_eq!(state.get_var("DI"), Some(16));
    assert_eq!(state.get_var("CX"), Some(0));
    assert_eq!(state.get_var("cycleCount"), Some(15 * 17));
}

#[test]
fn no_fast_forward_when_df_set() {
    // DF=1 means reverse direction — we conservatively skip. State must be
    // unchanged after the empty execute().
    let mut state = rigged_state();
    let prog = empty_program();
    let mut slots = Vec::new();

    state.memory[0x100] = 0xFF; // untouched sentinel
    state.set_var("opcode", 0xAA);
    state.set_var("hasREP", 1);
    state.set_var("repType", 1);
    state.set_var("CX", 100);
    state.set_var("DI", 0);
    state.set_var("ES", 0);
    state.set_var("flags", 1 << 10); // DF set
    state.set_var("AL", 0x55);
    state.set_var("IP", 0x200);

    execute(&prog, &mut state, &mut slots);

    // Memory untouched, CX unchanged.
    assert_eq!(state.memory[0x100], 0xFF);
    assert_eq!(state.get_var("CX"), Some(100));
    assert_eq!(state.get_var("IP"), Some(0x200));
}

#[test]
fn no_fast_forward_when_has_seg_override() {
    let mut state = rigged_state();
    let prog = empty_program();
    let mut slots = Vec::new();

    state.set_var("opcode", 0xA4);
    state.set_var("hasREP", 1);
    state.set_var("repType", 1);
    state.set_var("hasSegOverride", 1); // override active
    state.set_var("CX", 50);
    state.set_var("DI", 0);
    state.set_var("SI", 0);
    state.set_var("IP", 0x300);

    execute(&prog, &mut state, &mut slots);

    // CX unchanged — fast-forward bailed.
    assert_eq!(state.get_var("CX"), Some(50));
    assert_eq!(state.get_var("DI"), Some(0));
    assert_eq!(state.get_var("IP"), Some(0x300));
}

#[test]
fn no_fast_forward_when_no_rep_prefix() {
    // A bare STOSB (no REP) is a single-iteration instruction. Even if CX
    // happens to be large, we must not touch memory.
    let mut state = rigged_state();
    let prog = empty_program();
    let mut slots = Vec::new();

    state.set_var("opcode", 0xAA);
    state.set_var("hasREP", 0); // no prefix
    state.set_var("CX", 1000);
    state.set_var("DI", 0);
    state.set_var("ES", 0xB800);
    state.set_var("AL", 0x20);
    state.set_var("IP", 0x500);

    execute(&prog, &mut state, &mut slots);

    assert_eq!(state.memory[0xB8000], 0, "must not fill without REP");
    assert_eq!(state.get_var("CX"), Some(1000));
    assert_eq!(state.get_var("IP"), Some(0x500));
}

#[test]
fn no_fast_forward_when_cx_zero() {
    let mut state = rigged_state();
    let prog = empty_program();
    let mut slots = Vec::new();

    // REP STOSB that has just completed: CX=0.
    state.set_var("opcode", 0xAA);
    state.set_var("hasREP", 1);
    state.set_var("repType", 1);
    state.set_var("CX", 0);
    state.set_var("DI", 1000);
    state.set_var("IP", 0x400);

    execute(&prog, &mut state, &mut slots);

    // DI and IP untouched.
    assert_eq!(state.get_var("CX"), Some(0));
    assert_eq!(state.get_var("DI"), Some(1000));
    assert_eq!(state.get_var("IP"), Some(0x400));
}

#[test]
fn no_fast_forward_on_unrelated_opcode() {
    let mut state = rigged_state();
    let prog = empty_program();
    let mut slots = Vec::new();

    // 0x90 NOP — not a REP string op.
    state.set_var("opcode", 0x90);
    state.set_var("hasREP", 1);
    state.set_var("repType", 1);
    state.set_var("CX", 500);
    state.set_var("IP", 0x600);

    execute(&prog, &mut state, &mut slots);

    assert_eq!(state.get_var("CX"), Some(500));
    assert_eq!(state.get_var("IP"), Some(0x600));
}

#[test]
fn no_fast_forward_when_movs_source_overlaps_romdisk_window() {
    // The ROM-disk window at linear 0xD0000..0xD01FF is populated *per byte
    // read* by the CSS `--readDiskByte` function, not by state.memory. A bulk
    // copy_within from there would read zeros. This is exactly how CSS-DOS's
    // INT 13h handler triggers the bug: rep movsw with DS=0xD000, SI=0.
    let mut state = rigged_state();
    let prog = empty_program();
    let mut slots = Vec::new();

    // Seed the destination area with a sentinel so we can prove nothing moved.
    for i in 0..512 {
        state.memory[0x10000 + i] = 0xAB;
    }
    state.set_var("opcode", 0xA5); // REP MOVSW
    state.set_var("hasREP", 1);
    state.set_var("repType", 1);
    state.set_var("__1CX", 128); // 128 words remaining
    state.set_var("CX", 128);
    state.set_var("SI", 0);
    state.set_var("DI", 0);
    state.set_var("DS", 0xD000); // source: rom-disk window
    state.set_var("ES", 0x1000); // destination: normal RAM
    state.set_var("IP", 0x200);

    execute(&prog, &mut state, &mut slots);

    // Fast-forward must NOT have fired. CX/DI/SI unchanged, destination
    // sentinel bytes untouched.
    assert_eq!(state.get_var("CX"), Some(128), "CX should be unchanged");
    assert_eq!(state.get_var("DI"), Some(0), "DI should be unchanged");
    assert_eq!(state.get_var("SI"), Some(0), "SI should be unchanged");
    for i in 0..512 {
        assert_eq!(
            state.memory[0x10000 + i], 0xAB,
            "dst sentinel byte {} should not have been overwritten",
            i
        );
    }
}

#[test]
fn no_fast_forward_when_stos_dest_overlaps_bios_region() {
    // Destination at 0xF0000+ goes through state.extended, not state.memory,
    // so a bulk state.memory fill would silently drop the writes.
    let mut state = rigged_state();
    let prog = empty_program();
    let mut slots = Vec::new();

    state.set_var("opcode", 0xAA);
    state.set_var("hasREP", 1);
    state.set_var("repType", 1);
    state.set_var("CX", 100);
    state.set_var("DI", 0);
    state.set_var("ES", 0xF000); // ES*16 = 0xF0000 (BIOS ROM region)
    state.set_var("AL", 0x55);
    state.set_var("IP", 0x300);

    execute(&prog, &mut state, &mut slots);

    assert_eq!(state.get_var("CX"), Some(100), "CX should be unchanged");
    assert_eq!(state.get_var("DI"), Some(0), "DI should be unchanged");
}

#[test]
fn no_fast_forward_when_di_would_overflow_16_bits() {
    // DI + n * step > 0xFFFF → bail to avoid 16-bit wrap.
    let mut state = rigged_state();
    let prog = empty_program();
    let mut slots = Vec::new();

    state.set_var("opcode", 0xAA);
    state.set_var("hasREP", 1);
    state.set_var("repType", 1);
    state.set_var("CX", 100);
    state.set_var("DI", 0xFFF0); // only 16 bytes left before wrap
    state.set_var("ES", 0);
    state.set_var("AL", 0x77);
    state.set_var("IP", 0x700);

    execute(&prog, &mut state, &mut slots);

    assert_eq!(state.get_var("CX"), Some(100));
    assert_eq!(state.get_var("IP"), Some(0x700));
    assert_eq!(state.memory[0xFFF0], 0);
}
