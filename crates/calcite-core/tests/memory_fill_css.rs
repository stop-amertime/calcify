//! End-to-end integration test for CSS-level bulk-fill recognition + execution.
//!
//! Mirrors the shape CSS-DOS's bulk-op transpiler path emits:
//! every memory cell gains a top-of-chain range-predicate branch
//! (`style(--bulkOpKind: 1) and calc(addr >= var(--bulkDst))
//!   and calc(addr < var(--bulkDst) + var(--bulkCount))`) writing
//! `var(--bulkValue)`. The detector collapses the whole range to a single
//! binding; the evaluator executes it pre-tick via native `memory.fill`.
//!
//! This test hand-crafts a `ParsedProgram` with that shape (no parser
//! round-trip — when CSS-DOS begins emitting the shape its syntax is a
//! separate concern), builds an Evaluator, ticks once with the bulk
//! properties active, and verifies the fill range matches bit-exactly.

use calcite_core::eval::Evaluator;
use calcite_core::state::State;
use calcite_core::types::*;

/// Build a memory-cell assignment whose first branch is the bulk-fill range
/// predicate, followed by a simple keep fallback.
fn mk_bulk_fill_cell(addr: i64) -> Assignment {
    let cell_name = format!("--memByte_{addr}");
    let addr_lit = Expr::Literal(addr as f64);
    let bulk_dst_var = Expr::Var {
        name: "--bulkDst".to_string(),
        fallback: None,
    };
    let bulk_count_var = Expr::Var {
        name: "--bulkCount".to_string(),
        fallback: None,
    };

    let bulk_branch = StyleBranch {
        condition: StyleTest::And(vec![
            StyleTest::Single {
                property: "--bulkOpKind".to_string(),
                value: Expr::Literal(1.0),
            },
            StyleTest::Compare {
                left: addr_lit.clone(),
                op: CompareOp::Ge,
                right: bulk_dst_var.clone(),
            },
            StyleTest::Compare {
                left: addr_lit,
                op: CompareOp::Lt,
                right: Expr::Calc(CalcOp::Add(
                    Box::new(bulk_dst_var),
                    Box::new(bulk_count_var),
                )),
            },
        ]),
        then: Expr::Var {
            name: "--bulkValue".to_string(),
            fallback: None,
        },
    };

    Assignment {
        property: cell_name,
        value: Expr::StyleCondition {
            branches: vec![bulk_branch],
            fallback: Box::new(Expr::Literal(0.0)),
        },
    }
}

fn mk_property(name: &str) -> PropertyDef {
    PropertyDef {
        name: name.to_string(),
        syntax: PropertySyntax::Integer,
        inherits: false,
        initial_value: Some(CssValue::Integer(0)),
    }
}

#[test]
fn bulk_fill_fires_on_active_tick_and_fills_range() {
    // 256-cell memory region. The bulk-op control properties occupy the
    // first few addresses; the memory cells start at 256 and run for 256
    // bytes. This mirrors the BIOS-reserved-address convention from the
    // design spec without coupling the test to exact addresses.
    const CELL_START: i64 = 256;
    const CELL_COUNT: i64 = 256;

    let mut assignments: Vec<Assignment> = Vec::with_capacity(CELL_COUNT as usize);
    for i in 0..CELL_COUNT {
        assignments.push(mk_bulk_fill_cell(CELL_START + i));
    }

    // @property declarations for the state variables. The exact addresses
    // are assigned in declaration order by State::load_properties — each
    // named property gets the next slot in the state-vars region.
    let properties = vec![
        mk_property("--bulkOpKind"),
        mk_property("--bulkDst"),
        mk_property("--bulkCount"),
        mk_property("--bulkValue"),
    ];

    let program = ParsedProgram {
        properties,
        functions: Vec::new(),
        assignments,
    };

    let mut state = State::new(4096);
    state.load_properties(&program.properties);

    let mut eval = Evaluator::from_parsed(&program);

    // The detector must have fired — one binding covering all 256 cells.
    assert_eq!(
        eval.compiled().bulk_fill_bindings.len(),
        1,
        "expected a single bulk-fill binding covering the whole range"
    );
    let b = &eval.compiled().bulk_fill_bindings[0];
    assert_eq!(b.addr_range, CELL_START..CELL_START + CELL_COUNT);

    // Write the bulk-op control values into state as the BIOS handler
    // would. Use property_to_address to locate each one — same mechanism
    // the evaluator uses internally.
    let kind_addr = calcite_core::eval::property_to_address("--bulkOpKind").expect("kind addr");
    let dst_addr = calcite_core::eval::property_to_address("--bulkDst").expect("dst addr");
    let count_addr = calcite_core::eval::property_to_address("--bulkCount").expect("count addr");
    let val_addr = calcite_core::eval::property_to_address("--bulkValue").expect("val addr");

    // Fill cells [CELL_START + 16, CELL_START + 32) with 0x5A.
    let fill_dst = (CELL_START as i32) + 16;
    let fill_count = 16i32;
    let fill_val = 0x5Ai32;
    state.write_mem(kind_addr, 1);
    state.write_mem(dst_addr, fill_dst);
    state.write_mem(count_addr, fill_count);
    state.write_mem(val_addr, fill_val);

    eval.tick(&mut state);

    // Byte immediately before the fill range untouched.
    assert_eq!(
        state.read_mem(fill_dst - 1),
        0,
        "byte before fill range must be untouched"
    );
    // Fill range matches the fill value.
    for i in 0..fill_count {
        assert_eq!(
            state.read_mem(fill_dst + i) & 0xFF,
            fill_val & 0xFF,
            "byte at offset {i} in fill range",
        );
    }
    // Byte immediately after untouched.
    assert_eq!(
        state.read_mem(fill_dst + fill_count) & 0xFF,
        0,
        "byte after fill range must be untouched"
    );
}

#[test]
fn bulk_fill_inactive_when_kind_is_zero() {
    const CELL_START: i64 = 256;
    const CELL_COUNT: i64 = 64;

    let mut assignments: Vec<Assignment> = Vec::with_capacity(CELL_COUNT as usize);
    for i in 0..CELL_COUNT {
        assignments.push(mk_bulk_fill_cell(CELL_START + i));
    }
    let properties = vec![
        mk_property("--bulkOpKind"),
        mk_property("--bulkDst"),
        mk_property("--bulkCount"),
        mk_property("--bulkValue"),
    ];

    let program = ParsedProgram {
        properties,
        functions: Vec::new(),
        assignments,
    };

    let mut state = State::new(2048);
    state.load_properties(&program.properties);

    let mut eval = Evaluator::from_parsed(&program);
    assert_eq!(eval.compiled().bulk_fill_bindings.len(), 1);

    // Pre-populate the fill range with a sentinel value. The bulk op is
    // inactive (kind=0), so those bytes must survive the tick. The compiled
    // CSS will compute 0 for each cell (range predicate fails, fallback is
    // literal 0), but the native fill path is what we're testing here —
    // confirm we didn't fill when we shouldn't have.
    let dst_addr = calcite_core::eval::property_to_address("--bulkDst").expect("dst addr");
    let count_addr = calcite_core::eval::property_to_address("--bulkCount").expect("count addr");
    let val_addr = calcite_core::eval::property_to_address("--bulkValue").expect("val addr");

    // Seed sentinel bytes inside the would-be fill range. With kind=0 the
    // native bulk-fill path must not run, so these bytes must survive the
    // tick unchanged. (The compiled path has no writeback for these cells
    // — they're anonymous `--memByte_N` assignments without an @property
    // declaration — so what remains in `state.memory` is whatever the
    // pre-tick bulk fill did, which should be nothing.)
    for i in 0..16i32 {
        state.write_mem((CELL_START as i32) + i, 0xCD);
    }

    // Kind = 0 (inactive), dst/count set to plausible values that WOULD
    // match the detected binding if the gate fired.
    state.write_mem(dst_addr, CELL_START as i32);
    state.write_mem(count_addr, 16);
    state.write_mem(val_addr, 0xFF);

    eval.tick(&mut state);

    // Sentinel bytes must be unchanged — bulk fill must not have fired
    // with kind=0.
    for i in 0..16i32 {
        let b = state.read_mem((CELL_START as i32) + i) & 0xFF;
        assert_eq!(
            b,
            0xCD,
            "cell {} sentinel must survive when bulk op is inactive",
            CELL_START + i as i64
        );
    }
}
