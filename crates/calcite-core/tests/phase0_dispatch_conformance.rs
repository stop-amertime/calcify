//! Phase 0 conformance for `phase0_dispatch.rs` bench. **Throwaway after
//! Phase 3 lands.** The bench's speed ratio is meaningless without proof
//! that the handcoded path computes the same value as the interpreter
//! path for every input. This test asserts that.
//!
//! Same `PROGRAM` and `get_reg16` as `benches/phase0_dispatch.rs` —
//! kept in sync by the throwaway-code rule (changes to either should be
//! made to both, but neither is meant to outlive Phase 3 anyway).

use calcite_core::eval::Evaluator;
use calcite_core::parser::parse_css;
use calcite_core::State;

const PROGRAM: &str = r#"
@property --frame {
    syntax: "<integer>";
    initial-value: 0;
    inherits: false;
}
@property --result {
    syntax: "<integer>";
    initial-value: 0;
    inherits: false;
}
@property --ax { syntax: "<integer>"; initial-value: 1000; inherits: false; }
@property --cx { syntax: "<integer>"; initial-value: 2000; inherits: false; }
@property --dx { syntax: "<integer>"; initial-value: 3000; inherits: false; }
@property --bx { syntax: "<integer>"; initial-value: 4000; inherits: false; }
@property --sp { syntax: "<integer>"; initial-value: 5000; inherits: false; }
@property --bp { syntax: "<integer>"; initial-value: 6000; inherits: false; }
@property --si { syntax: "<integer>"; initial-value: 7000; inherits: false; }
@property --di { syntax: "<integer>"; initial-value: 8000; inherits: false; }

@function --getReg16(--r <integer>,
                     --ax <integer>, --cx <integer>, --dx <integer>, --bx <integer>,
                     --sp <integer>, --bp <integer>, --si <integer>, --di <integer>)
    returns <integer>
{
    result: if(
        style(--r: 0): var(--ax);
        style(--r: 1): var(--cx);
        style(--r: 2): var(--dx);
        style(--r: 3): var(--bx);
        style(--r: 4): var(--sp);
        style(--r: 5): var(--bp);
        style(--r: 6): var(--si);
        style(--r: 7): var(--di);
        else: 0
    );
}

:root {
    --frame: mod(calc(var(--frame) + 1), 8);
    --result: --getReg16(var(--frame),
                          var(--ax), var(--cx), var(--dx), var(--bx),
                          var(--sp), var(--bp), var(--si), var(--di));
}
"#;

#[inline(always)]
fn get_reg16(r: i32, ax: i32, cx: i32, dx: i32, bx: i32,
             sp: i32, bp: i32, si: i32, di: i32) -> i32 {
    match r {
        0 => ax, 1 => cx, 2 => dx, 3 => bx,
        4 => sp, 5 => bp, 6 => si, 7 => di,
        _ => 0,
    }
}

#[test]
fn handcoded_matches_interpreter() {
    let parsed = parse_css(PROGRAM).expect("parse synthetic program");
    let mut state = State::default();
    state.load_properties(&parsed.properties);
    let mut evaluator = Evaluator::from_parsed(&parsed);

    // Read the @property initial-values out of state. These don't change
    // across ticks (no other assignment writes them); they're the inputs
    // both sides see.
    let ax = state.get_var("ax").expect("--ax should be present");
    let cx = state.get_var("cx").expect("--cx should be present");
    let dx = state.get_var("dx").expect("--dx should be present");
    let bx = state.get_var("bx").expect("--bx should be present");
    let sp = state.get_var("sp").expect("--sp should be present");
    let bp = state.get_var("bp").expect("--bp should be present");
    let si = state.get_var("si").expect("--si should be present");
    let di = state.get_var("di").expect("--di should be present");

    // Sanity: state matches what we declared.
    assert_eq!(ax, 1000, "--ax initial-value should be 1000");
    assert_eq!(cx, 2000, "--cx initial-value should be 2000");
    assert_eq!(dx, 3000, "--dx initial-value should be 3000");

    // The :root assignment increments --frame mod 8 each tick, so after
    // tick 1 we have frame=1, after tick 2 frame=2, ..., tick 8 frame=0,
    // tick 9 frame=1, ... Each tick's --result should equal
    // get_reg16(frame_after_tick, ax, cx, ..., di).
    for expected_r in 1..=8 {
        evaluator.tick(&mut state);
        let interp_result = state.get_var("result")
            .expect("--result should be present after tick");
        let frame_after = expected_r % 8;
        let hand_result = get_reg16(frame_after, ax, cx, dx, bx, sp, bp, si, di);
        assert_eq!(
            interp_result, hand_result,
            "tick {} (expected frame={}): interpreter={}, handcoded={}",
            expected_r, frame_after, interp_result, hand_result,
        );
    }
}
