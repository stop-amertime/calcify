//! Integration tests exercising the full pipeline: parse → analyse → evaluate.
//!
//! CSS snippets are modelled after x86CSS patterns.

use calcify_core::eval::Evaluator;
use calcify_core::parser::parse_css;
use calcify_core::state::{self, State};

/// Parse CSS and build an evaluator.
fn setup(css: &str) -> (Evaluator, State) {
    let parsed = parse_css(css).expect("CSS should parse");
    let evaluator = Evaluator::from_parsed(&parsed);
    let state = State::default();
    (evaluator, state)
}

#[test]
fn simple_assignment() {
    let css = r#"
        .cpu {
            --AX: 42;
            --CX: 7;
        }
    "#;

    let (evaluator, mut state) = setup(css);
    evaluator.tick(&mut state);

    assert_eq!(state.registers[state::reg::AX], 42);
    assert_eq!(state.registers[state::reg::CX], 7);
}

#[test]
fn calc_expression() {
    let css = r#"
        .cpu {
            --AX: calc(10 + 32);
            --CX: calc(var(--AX) * 2);
        }
    "#;

    let (evaluator, mut state) = setup(css);
    evaluator.tick(&mut state);

    assert_eq!(state.registers[state::reg::AX], 42);
    // --CX depends on --AX being computed first (declaration order)
    assert_eq!(state.registers[state::reg::CX], 84);
}

#[test]
fn var_references_between_properties() {
    let css = r#"
        .cpu {
            --AX: 100;
            --BX: calc(var(--AX) + 50);
            --CX: calc(var(--BX) - var(--AX));
        }
    "#;

    let (evaluator, mut state) = setup(css);
    evaluator.tick(&mut state);

    assert_eq!(state.registers[state::reg::AX], 100);
    assert_eq!(state.registers[state::reg::BX], 150);
    assert_eq!(state.registers[state::reg::CX], 50);
}

#[test]
fn if_style_condition() {
    let css = r#"
        .cpu {
            --AX: 2;
            --BX: if(style(--AX: 1): 10; style(--AX: 2): 20; style(--AX: 3): 30; else: 0);
        }
    "#;

    let (evaluator, mut state) = setup(css);
    evaluator.tick(&mut state);

    assert_eq!(state.registers[state::reg::BX], 20);
}

#[test]
fn mod_and_round() {
    // 4660 = 0x1234 (CSS doesn't have hex number literals)
    let css = r#"
        .cpu {
            --AX: 4660;
            --BX: mod(var(--AX), 256);
            --CX: round(down, calc(var(--AX) / 256), 1);
        }
    "#;

    let (evaluator, mut state) = setup(css);
    evaluator.tick(&mut state);

    assert_eq!(state.registers[state::reg::AX], 4660);
    // AL = AX mod 256 = 0x34 = 52
    assert_eq!(state.registers[state::reg::BX], 52);
    // AH = floor(AX / 256) = 0x12 = 18
    assert_eq!(state.registers[state::reg::CX], 18);
}

#[test]
fn memory_writes() {
    let css = r#"
        .cpu {
            --m0: 255;
            --m1: 66;
            --m256: 99;
        }
    "#;

    let (evaluator, mut state) = setup(css);
    evaluator.tick(&mut state);

    assert_eq!(state.memory[0], 255);
    assert_eq!(state.memory[1], 66);
    assert_eq!(state.memory[256], 99);
}

#[test]
fn function_call() {
    // A simplified readMem-like function
    let css = r#"
        @function --double(--x <integer>) returns <integer> {
            result: calc(var(--x) * 2);
        }

        .cpu {
            --AX: --double(21);
        }
    "#;

    let (evaluator, mut state) = setup(css);
    evaluator.tick(&mut state);

    assert_eq!(state.registers[state::reg::AX], 42);
}

#[test]
fn multiple_ticks_accumulate() {
    // Each tick adds 1 to AX (by reading its previous value)
    let css = r#"
        .cpu {
            --AX: calc(var(--AX) + 1);
        }
    "#;

    let (evaluator, mut state) = setup(css);

    for _ in 0..10 {
        evaluator.tick(&mut state);
    }

    assert_eq!(state.registers[state::reg::AX], 10);
}

#[test]
fn dispatch_table_in_function() {
    // A function with enough branches to trigger dispatch table recognition
    let css = r#"
        @function --lookup(--key <integer>) returns <integer> {
            result: if(
                style(--key: 0): 100;
                style(--key: 1): 200;
                style(--key: 2): 300;
                style(--key: 3): 400;
                style(--key: 4): 500;
                else: 0
            );
        }

        .cpu {
            --AX: --lookup(3);
        }
    "#;

    let (evaluator, mut state) = setup(css);
    evaluator.tick(&mut state);

    assert_eq!(state.registers[state::reg::AX], 400);
}

#[test]
fn property_declarations() {
    let css = r#"
        @property --counter {
            syntax: "<integer>";
            inherits: false;
            initial-value: 0;
        }

        .cpu {
            --AX: 1;
        }
    "#;

    let parsed = parse_css(css).expect("should parse");
    assert_eq!(parsed.properties.len(), 1);
    assert_eq!(parsed.properties[0].name, "--counter");
    assert_eq!(parsed.functions.len(), 0);
    assert_eq!(parsed.assignments.len(), 1);
}

#[test]
fn complex_nested_expression() {
    // Mimics x86CSS's byte extraction pattern: split and reconstruct
    // 4660 = 0x1234, lo=52 (0x34), hi=18 (0x12)
    let css = r#"
        .cpu {
            --AX: 4660;
            --BX: calc(mod(var(--AX), 256) + round(down, calc(var(--AX) / 256), 1) * 256);
        }
    "#;

    let (evaluator, mut state) = setup(css);
    evaluator.tick(&mut state);

    // BX should reconstruct AX from its bytes: AL + AH*256 = AX
    assert_eq!(state.registers[state::reg::AX], 4660);
    assert_eq!(state.registers[state::reg::BX], 4660);
}

#[test]
fn min_max_clamp() {
    let css = r#"
        .cpu {
            --AX: min(100, 200, 50);
            --BX: max(10, 30, 20);
            --CX: clamp(0, 500, 255);
        }
    "#;

    let (evaluator, mut state) = setup(css);
    evaluator.tick(&mut state);

    assert_eq!(state.registers[state::reg::AX], 50);
    assert_eq!(state.registers[state::reg::BX], 30);
    assert_eq!(state.registers[state::reg::CX], 255);
}

#[test]
fn empty_css() {
    let css = "";
    let parsed = parse_css(css).expect("empty CSS should parse");
    assert!(parsed.properties.is_empty());
    assert!(parsed.functions.is_empty());
    assert!(parsed.assignments.is_empty());
}
