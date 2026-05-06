//! Unit tests for the self-loop recogniser.
//!
//! Two synthetic cabinets, A and B, produce equivalent descriptors
//! despite having completely different slot/property names. This is the
//! cardinal-rule genericity probe: A is x86-shaped (slot names match the
//! current kiln-emitted cabinets); B is brainfuck-shaped (arbitrary
//! opaque names, no x86 ABI, no shared naming convention with A). The
//! recogniser must not see the difference.

use super::*;
use crate::types::*;

// ---------------------------------------------------------------------------
// Helpers for building Expr trees (lots of `Box::new` otherwise).
// ---------------------------------------------------------------------------

fn lit(v: f64) -> Expr {
    Expr::Literal(v)
}

fn var(name: &str) -> Expr {
    Expr::Var {
        name: name.to_string(),
        fallback: None,
    }
}

fn add(a: Expr, b: Expr) -> Expr {
    Expr::Calc(CalcOp::Add(Box::new(a), Box::new(b)))
}

fn sub(a: Expr, b: Expr) -> Expr {
    Expr::Calc(CalcOp::Sub(Box::new(a), Box::new(b)))
}

fn mul(a: Expr, b: Expr) -> Expr {
    Expr::Calc(CalcOp::Mul(Box::new(a), Box::new(b)))
}

fn maxof(a: Expr, b: Expr) -> Expr {
    Expr::Calc(CalcOp::Max(vec![a, b]))
}

fn call(name: &str, args: Vec<Expr>) -> Expr {
    Expr::FunctionCall {
        name: name.to_string(),
        args,
    }
}

fn style_eq(prop: &str, value: f64) -> StyleTest {
    StyleTest::Single {
        property: prop.to_string(),
        value: Expr::Literal(value),
    }
}

/// Build `if(<test>: then; else: fallback)`.
fn iff(test: StyleTest, then: Expr, fallback: Expr) -> Expr {
    Expr::StyleCondition {
        branches: vec![StyleBranch {
            condition: test,
            then,
        }],
        fallback: Box::new(fallback),
    }
}

/// Build a multi-branch StyleCondition, all branches keyed on `prop`.
fn dispatch(prop: &str, branches: Vec<(f64, Expr)>, fallback: Expr) -> Expr {
    Expr::StyleCondition {
        branches: branches
            .into_iter()
            .map(|(v, then)| StyleBranch {
                condition: style_eq(prop, v),
                then,
            })
            .collect(),
        fallback: Box::new(fallback),
    }
}

fn assign(prop: &str, value: Expr) -> Assignment {
    Assignment {
        property: prop.to_string(),
        value,
    }
}

// ---------------------------------------------------------------------------
// Builders for the per-V kiln shapes.
// ---------------------------------------------------------------------------

/// Build the IP-stay-or-advance body for one opcode.
///
/// Shape: `if(<predicate>: calc(self - prefix_sub); else: calc(self + advance_lit))`.
///
/// `predicate` is the loop-continue gate; `self_var` is the prior IP
/// mirror; `prefix_sub` is what gets subtracted on the stay branch.
fn ip_body(predicate: StyleTest, self_var: &str, prefix_sub: Expr, advance_lit: i32) -> Expr {
    iff(
        predicate,
        sub(var(self_var), prefix_sub),
        add(var(self_var), lit(advance_lit as f64)),
    )
}

/// Build the counter-decrement body for one opcode.
///
/// Shape: `if(<no-rep-guard>: self; else: max(0, calc(self - 1)))`.
fn counter_body(no_rep_guard: StyleTest, self_var: &str) -> Expr {
    iff(
        no_rep_guard,
        var(self_var),
        maxof(lit(0.0), sub(var(self_var), lit(1.0))),
    )
}

/// Build the pointer-step body for one opcode in kiln's actual shape:
///
///   `if(<rep-guard>: var(self); else: <update-expr>)`
///
/// where update-expr =
///   `OUTER_CALL(calc(calc(var(self) + k) - INNER_CALL(var(flag), bit) * (2k)), 16)`
/// — the kiln `--lowerBytes` / direction-flag idiom for a 16-bit
/// modular pointer step.
fn pointer_body(
    rep_guard: StyleTest,
    self_var: &str,
    base_step: i32,
    flag_var: &str,
    flag_bit: u32,
    outer_call: &str,
    inner_call: &str,
) -> Expr {
    let inner = sub(
        add(var(self_var), lit(base_step as f64)),
        mul(
            call(inner_call, vec![var(flag_var), lit(flag_bit as f64)]),
            lit(2.0 * base_step as f64),
        ),
    );
    let update = call(outer_call, vec![inner, lit(16.0)]);
    iff(rep_guard, var(self_var), update)
}

/// Build a "no entry" fallback expression (slot keeps its prior value).
fn keep_self(self_var: &str) -> Expr {
    var(self_var)
}

// ---------------------------------------------------------------------------
// Cabinet A — x86-shaped names.
//
// Two opcodes:
//   0xAA: STOSB-shape — counter, one pointer (DI).
//   0xA4: MOVSB-shape — counter, two pointers (DI, SI).
// ---------------------------------------------------------------------------

fn cabinet_a() -> Vec<Assignment> {
    let pred_continue = style_eq("--_repContinue", 1.0);
    let no_rep = style_eq("--hasREP", 0.0);

    let cx_dispatch = dispatch(
        "--opcode",
        vec![
            (0xAA as f64, counter_body(no_rep.clone(), "--__1CX")),
            (0xA4 as f64, counter_body(no_rep.clone(), "--__1CX")),
        ],
        keep_self("--__1CX"),
    );

    let ip_dispatch = dispatch(
        "--opcode",
        vec![
            (
                0xAA as f64,
                ip_body(
                    pred_continue.clone(),
                    "--__1IP",
                    var("--prefixLen"),
                    1,
                ),
            ),
            (
                0xA4 as f64,
                ip_body(
                    pred_continue.clone(),
                    "--__1IP",
                    var("--prefixLen"),
                    1,
                ),
            ),
        ],
        keep_self("--__1IP"),
    );

    // Outer wrapper kiln adds: calc(<dispatch> + var(--prefixLen)).
    let ip_wrapped = add(ip_dispatch, var("--prefixLen"));

    let active_guard = StyleTest::And(vec![
        style_eq("--hasREP", 1.0),
        style_eq("--_repActive", 0.0),
    ]);
    let di_dispatch = dispatch(
        "--opcode",
        vec![
            (
                0xAA as f64,
                pointer_body(
                    active_guard.clone(),
                    "--__1DI",
                    1,
                    "--__1flags",
                    10,
                    "--lowerBytes",
                    "--bit",
                ),
            ),
            (
                0xA4 as f64,
                pointer_body(
                    active_guard.clone(),
                    "--__1DI",
                    1,
                    "--__1flags",
                    10,
                    "--lowerBytes",
                    "--bit",
                ),
            ),
        ],
        keep_self("--__1DI"),
    );
    let si_dispatch = dispatch(
        "--opcode",
        vec![(
            0xA4 as f64,
            pointer_body(
                active_guard.clone(),
                "--__1SI",
                1,
                "--__1flags",
                10,
                "--lowerBytes",
                "--bit",
            ),
        )],
        keep_self("--__1SI"),
    );

    // Memwrite address slot: -1 when inactive, real address when active.
    let memaddr0_dispatch = dispatch(
        "--opcode",
        vec![
            (
                0xAA as f64,
                iff(
                    active_guard.clone(),
                    lit(-1.0),
                    add(mul(var("--__1ES"), lit(16.0)), var("--__1DI")),
                ),
            ),
            (
                0xA4 as f64,
                iff(
                    active_guard.clone(),
                    lit(-1.0),
                    add(mul(var("--__1ES"), lit(16.0)), var("--__1DI")),
                ),
            ),
        ],
        lit(-1.0),
    );

    let memval0_dispatch = dispatch(
        "--opcode",
        vec![
            (0xAA as f64, var("--AL")),
            (0xA4 as f64, var("--_strSrcByte")),
        ],
        lit(0.0),
    );

    vec![
        assign("--CX", cx_dispatch),
        assign("--IP", ip_wrapped),
        assign("--DI", di_dispatch),
        assign("--SI", si_dispatch),
        assign("--memAddr0", memaddr0_dispatch),
        assign("--memVal0", memval0_dispatch),
    ]
}

// ---------------------------------------------------------------------------
// Cabinet B — brainfuck-shaped names.
//
// Same structural shape as cabinet A, but the slot names share NOTHING
// with x86 land. The recogniser must produce the same descriptor count
// and the same structural fields (counter, pointer count, advance lit).
// Names will obviously differ — we compare structure, not strings.
// ---------------------------------------------------------------------------

fn cabinet_b() -> Vec<Assignment> {
    let pred_continue = style_eq("--moodMeter", 1.0);
    let no_rep = style_eq("--cookbookOpen", 0.0);

    // Two opcodes: 70 (a "fill" shape, like A's 0xAA) and 80 (a "copy"
    // shape, like A's 0xA4).
    let counter_dispatch = dispatch(
        "--recipeStep",
        vec![
            (70.0, counter_body(no_rep.clone(), "--priorTapeUses")),
            (80.0, counter_body(no_rep.clone(), "--priorTapeUses")),
        ],
        keep_self("--priorTapeUses"),
    );

    let cursor_advance_dispatch = dispatch(
        "--recipeStep",
        vec![
            (
                70.0,
                ip_body(
                    pred_continue.clone(),
                    "--priorCursor",
                    var("--introBytes"),
                    1,
                ),
            ),
            (
                80.0,
                ip_body(
                    pred_continue.clone(),
                    "--priorCursor",
                    var("--introBytes"),
                    1,
                ),
            ),
        ],
        keep_self("--priorCursor"),
    );
    let cursor_wrapped = add(cursor_advance_dispatch, var("--introBytes"));

    let active_guard = StyleTest::And(vec![
        style_eq("--cookbookOpen", 1.0),
        style_eq("--ladlePoised", 0.0),
    ]);

    // Pointer 1 — analog of DI, but called tapeWriteHead.
    let twh_dispatch = dispatch(
        "--recipeStep",
        vec![
            (
                70.0,
                pointer_body(
                    active_guard.clone(),
                    "--priorTapeWriteHead",
                    1,
                    "--priorMoodFlags",
                    7, // any small bit, doesn't have to be 10
                    "--clampLowBits",
                    "--readBitN",
                ),
            ),
            (
                80.0,
                pointer_body(
                    active_guard.clone(),
                    "--priorTapeWriteHead",
                    1,
                    "--priorMoodFlags",
                    7,
                    "--clampLowBits",
                    "--readBitN",
                ),
            ),
        ],
        keep_self("--priorTapeWriteHead"),
    );
    // Pointer 2 — analog of SI but only for opcode 80.
    let trh_dispatch = dispatch(
        "--recipeStep",
        vec![(
            80.0,
            pointer_body(
                active_guard.clone(),
                "--priorTapeReadHead",
                1,
                "--priorMoodFlags",
                7,
                "--clampLowBits",
                "--readBitN",
            ),
        )],
        keep_self("--priorTapeReadHead"),
    );

    let bag_addr = dispatch(
        "--recipeStep",
        vec![
            (
                70.0,
                iff(
                    active_guard.clone(),
                    lit(-1.0),
                    add(
                        mul(var("--priorBagPage"), lit(16.0)),
                        var("--priorTapeWriteHead"),
                    ),
                ),
            ),
            (
                80.0,
                iff(
                    active_guard.clone(),
                    lit(-1.0),
                    add(
                        mul(var("--priorBagPage"), lit(16.0)),
                        var("--priorTapeWriteHead"),
                    ),
                ),
            ),
        ],
        lit(-1.0),
    );
    let bag_val = dispatch(
        "--recipeStep",
        vec![
            (70.0, var("--ladleByte")),
            (80.0, var("--mirrorSourceByte")),
        ],
        lit(0.0),
    );

    vec![
        assign("--tapeUses", counter_dispatch),
        assign("--cursor", cursor_wrapped),
        assign("--tapeWriteHead", twh_dispatch),
        assign("--tapeReadHead", trh_dispatch),
        assign("--bagAddr0", bag_addr),
        assign("--bagVal0", bag_val),
    ]
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[test]
fn cabinet_a_recognises_two_loops() {
    let descs = recognise_loops(&cabinet_a());
    assert_eq!(descs.len(), 2, "expected 2 loop descriptors, got {}: {:?}", descs.len(), descs);

    let stosb = descs.iter().find(|d| d.key_value == 0xAA).expect("stosb desc");
    assert!(stosb.counter.is_some(), "stosb should have a counter");
    assert_eq!(stosb.pointers.len(), 1, "stosb should have 1 pointer (DI)");
    assert_eq!(stosb.ip_advance_literal, 1);
    assert_eq!(stosb.writes.len(), 1, "stosb should have 1 write descriptor");

    let movsb = descs.iter().find(|d| d.key_value == 0xA4).expect("movsb desc");
    assert!(movsb.counter.is_some(), "movsb should have a counter");
    assert_eq!(movsb.pointers.len(), 2, "movsb should have 2 pointers (DI, SI)");
    assert_eq!(movsb.ip_advance_literal, 1);
}

#[test]
fn cabinet_b_recognises_two_loops_equivalently() {
    let descs = recognise_loops(&cabinet_b());
    assert_eq!(
        descs.len(),
        2,
        "brainfuck-shaped cabinet should produce same descriptor count as x86-shaped: {:?}",
        descs
    );

    // Sort by key_value for determinism.
    let mut by_k = descs.clone();
    by_k.sort_by_key(|d| d.key_value);
    let fill = &by_k[0]; // key 70 — analog of stosb
    let copy = &by_k[1]; // key 80 — analog of movsb

    assert_eq!(fill.key_value, 70);
    assert!(fill.counter.is_some());
    assert_eq!(fill.pointers.len(), 1);
    assert_eq!(fill.writes.len(), 1);
    assert_eq!(fill.ip_advance_literal, 1);

    assert_eq!(copy.key_value, 80);
    assert!(copy.counter.is_some());
    assert_eq!(copy.pointers.len(), 2);
    assert_eq!(copy.ip_advance_literal, 1);
}

/// The genericity probe in concentrated form: structural fields (counts
/// and step magnitudes) must match exactly between A and B, even though
/// no slot or property name overlaps.
#[test]
fn a_and_b_descriptors_are_structurally_equivalent() {
    let a = recognise_loops(&cabinet_a());
    let b = recognise_loops(&cabinet_b());
    assert_eq!(a.len(), b.len());

    // Cabinets A and B use unrelated key-value sets (e.g. 0xAA/0xA4 vs
    // 70/80). Comparing by key-value would be meaningless. Instead we
    // compare the *multiset* of structural signatures across all
    // descriptors — both cabinets must yield the same multiset.
    let signatures = |x: &[LoopDescriptor]| {
        let mut sigs: Vec<_> = x
            .iter()
            .map(|d| {
                let mut psteps: Vec<i32> = d.pointers.iter().map(|p| p.base_step).collect();
                psteps.sort();
                (
                    d.counter.as_ref().map(|c| c.step),
                    d.pointers.len(),
                    psteps,
                    d.ip_advance_literal,
                    d.writes.len(),
                    d.flag_conditioned,
                )
            })
            .collect();
        sigs.sort();
        sigs
    };

    let a_sig = signatures(&a);
    let b_sig = signatures(&b);
    assert_eq!(
        a_sig, b_sig,
        "cabinet A and cabinet B must produce structurally identical descriptors"
    );
}

#[test]
fn no_loop_when_no_ip_stay_shape() {
    // A dispatch family with only a counter and a pointer, but no
    // IP-stay shape. Should produce zero descriptors — there's no
    // termination signal we can fast-forward against.
    let no_rep = style_eq("--hasREP", 0.0);
    let active_guard = StyleTest::And(vec![
        style_eq("--hasREP", 1.0),
        style_eq("--_repActive", 0.0),
    ]);
    let cx = dispatch(
        "--opcode",
        vec![(0xAA as f64, counter_body(no_rep, "--__1CX"))],
        keep_self("--__1CX"),
    );
    let di = dispatch(
        "--opcode",
        vec![(
            0xAA as f64,
            pointer_body(
                active_guard,
                "--__1DI",
                1,
                "--__1flags",
                10,
                "--lowerBytes",
                "--bit",
            ),
        )],
        keep_self("--__1DI"),
    );
    let asns = vec![assign("--CX", cx), assign("--DI", di)];
    let descs = recognise_loops(&asns);
    assert!(
        descs.is_empty(),
        "no IP-stay-shape means no descriptors, got {:?}",
        descs
    );
}

#[test]
fn no_loop_when_only_ip_stay_no_counter_or_pointer_or_write() {
    // IP-stay alone, nothing else. We refuse: an unbounded loop is not
    // safe to fast-forward.
    let pred = style_eq("--cont", 1.0);
    let ip = dispatch(
        "--opcode",
        vec![(7.0, ip_body(pred, "--prevPC", var("--prefixLen"), 1))],
        keep_self("--prevPC"),
    );
    // Add a second member that's not counter/pointer/write — just a
    // passthrough. This avoids the "single-member family" filter.
    let other = dispatch("--opcode", vec![(7.0, lit(0.0))], lit(0.0));
    let asns = vec![assign("--PC", ip), assign("--noise", other)];
    let descs = recognise_loops(&asns);
    assert!(
        descs.is_empty(),
        "IP-stay without counter/pointer/write must be refused: {:?}",
        descs
    );
}

#[test]
fn dispatch_family_picks_largest_member_set() {
    // Two different dispatch keys present. Recogniser picks the one
    // with more members.
    let pred = style_eq("--cont", 1.0);
    let no_rep = style_eq("--hasREP", 0.0);
    let active_guard = StyleTest::And(vec![
        style_eq("--hasREP", 1.0),
        style_eq("--_repActive", 0.0),
    ]);

    // Family on --opcode (3 members).
    let cx = dispatch(
        "--opcode",
        vec![(0xAA as f64, counter_body(no_rep, "--__1CX"))],
        keep_self("--__1CX"),
    );
    let ip = dispatch(
        "--opcode",
        vec![(
            0xAA as f64,
            ip_body(pred.clone(), "--__1IP", var("--prefixLen"), 1),
        )],
        keep_self("--__1IP"),
    );
    let di = dispatch(
        "--opcode",
        vec![(
            0xAA as f64,
            pointer_body(
                active_guard.clone(),
                "--__1DI",
                1,
                "--__1flags",
                10,
                "--lowerBytes",
                "--bit",
            ),
        )],
        keep_self("--__1DI"),
    );

    // Decoy family on --otherKey (2 members, no shape).
    let dec1 = dispatch("--otherKey", vec![(1.0, lit(1.0))], lit(0.0));
    let dec2 = dispatch("--otherKey", vec![(2.0, lit(2.0))], lit(0.0));

    let asns = vec![
        assign("--CX", cx),
        assign("--IP", ip),
        assign("--DI", di),
        assign("--noise1", dec1),
        assign("--noise2", dec2),
    ];
    let descs = recognise_loops(&asns);
    assert_eq!(descs.len(), 1);
    assert_eq!(descs[0].key_property, "--opcode");
    assert_eq!(descs[0].key_value, 0xAA);
}

/// Renaming a slot must not change the descriptor structure (only the
/// stored names). Concretely: change every `--__1CX` to `--zzzCX`, every
/// `--__1IP` to `--prevIP`, etc., and the recogniser must still produce
/// equivalent structural facts.
#[test]
fn renaming_slots_preserves_structure() {
    fn rename_one(asns: Vec<Assignment>, table: &[(&str, &str)]) -> Vec<Assignment> {
        fn ren(s: &str, t: &[(&str, &str)]) -> String {
            for (from, to) in t {
                if s == *from {
                    return (*to).to_string();
                }
            }
            s.to_string()
        }
        fn ren_expr(e: &Expr, t: &[(&str, &str)]) -> Expr {
            match e {
                Expr::Literal(v) => Expr::Literal(*v),
                Expr::StringLiteral(s) => Expr::StringLiteral(s.clone()),
                Expr::Var { name, fallback } => Expr::Var {
                    name: ren(name, t),
                    fallback: fallback.as_ref().map(|f| Box::new(ren_expr(f, t))),
                },
                Expr::Calc(op) => Expr::Calc(ren_calc(op, t)),
                Expr::StyleCondition { branches, fallback } => Expr::StyleCondition {
                    branches: branches
                        .iter()
                        .map(|b| StyleBranch {
                            condition: ren_test(&b.condition, t),
                            then: ren_expr(&b.then, t),
                        })
                        .collect(),
                    fallback: Box::new(ren_expr(fallback, t)),
                },
                Expr::FunctionCall { name, args } => Expr::FunctionCall {
                    name: ren(name, t),
                    args: args.iter().map(|a| ren_expr(a, t)).collect(),
                },
                Expr::Concat(parts) => Expr::Concat(parts.iter().map(|p| ren_expr(p, t)).collect()),
            }
        }
        fn ren_calc(op: &CalcOp, t: &[(&str, &str)]) -> CalcOp {
            match op {
                CalcOp::Add(a, b) => CalcOp::Add(Box::new(ren_expr(a, t)), Box::new(ren_expr(b, t))),
                CalcOp::Sub(a, b) => CalcOp::Sub(Box::new(ren_expr(a, t)), Box::new(ren_expr(b, t))),
                CalcOp::Mul(a, b) => CalcOp::Mul(Box::new(ren_expr(a, t)), Box::new(ren_expr(b, t))),
                CalcOp::Div(a, b) => CalcOp::Div(Box::new(ren_expr(a, t)), Box::new(ren_expr(b, t))),
                CalcOp::Mod(a, b) => CalcOp::Mod(Box::new(ren_expr(a, t)), Box::new(ren_expr(b, t))),
                CalcOp::Pow(a, b) => CalcOp::Pow(Box::new(ren_expr(a, t)), Box::new(ren_expr(b, t))),
                CalcOp::Min(args) => CalcOp::Min(args.iter().map(|a| ren_expr(a, t)).collect()),
                CalcOp::Max(args) => CalcOp::Max(args.iter().map(|a| ren_expr(a, t)).collect()),
                CalcOp::Clamp(a, b, c) => CalcOp::Clamp(
                    Box::new(ren_expr(a, t)),
                    Box::new(ren_expr(b, t)),
                    Box::new(ren_expr(c, t)),
                ),
                CalcOp::Round(s, a, b) => CalcOp::Round(
                    *s,
                    Box::new(ren_expr(a, t)),
                    Box::new(ren_expr(b, t)),
                ),
                CalcOp::Sign(a) => CalcOp::Sign(Box::new(ren_expr(a, t))),
                CalcOp::Abs(a) => CalcOp::Abs(Box::new(ren_expr(a, t))),
                CalcOp::Negate(a) => CalcOp::Negate(Box::new(ren_expr(a, t))),
            }
        }
        fn ren_test(test: &StyleTest, t: &[(&str, &str)]) -> StyleTest {
            match test {
                StyleTest::Single { property, value } => StyleTest::Single {
                    property: ren(property, t),
                    value: ren_expr(value, t),
                },
                StyleTest::And(parts) => {
                    StyleTest::And(parts.iter().map(|p| ren_test(p, t)).collect())
                }
                StyleTest::Or(parts) => {
                    StyleTest::Or(parts.iter().map(|p| ren_test(p, t)).collect())
                }
            }
        }
        asns.into_iter()
            .map(|a| Assignment {
                property: ren(&a.property, table),
                value: ren_expr(&a.value, table),
            })
            .collect()
    }

    let table: &[(&str, &str)] = &[
        ("--opcode", "--decided-step"),
        ("--__1CX", "--rememberedRunLength"),
        ("--__1IP", "--rememberedSong"),
        ("--__1DI", "--rememberedHand"),
        ("--__1SI", "--rememberedFoot"),
        ("--__1ES", "--rememberedDimension"),
        ("--__1flags", "--rememberedMood"),
        ("--CX", "--runLength"),
        ("--IP", "--song"),
        ("--DI", "--hand"),
        ("--SI", "--foot"),
        ("--memAddr0", "--paint0"),
        ("--memVal0", "--colour0"),
        ("--prefixLen", "--introBytes"),
        ("--hasREP", "--cookbookOpen"),
        ("--_repActive", "--ladlePoised"),
        ("--_repContinue", "--moodMeter"),
        ("--_strSrcByte", "--mirrorSourceByte"),
        ("--AL", "--ladleByte"),
        ("--lowerBytes", "--clampLowBits"),
        ("--bit", "--readBitN"),
    ];

    let original = recognise_loops(&cabinet_a());
    let renamed = recognise_loops(&rename_one(cabinet_a(), table));
    assert_eq!(original.len(), renamed.len());

    let summary = |descs: &[LoopDescriptor]| {
        let mut keys: Vec<i64> = descs.iter().map(|d| d.key_value).collect();
        keys.sort_unstable();
        keys.into_iter()
            .map(|k| {
                let d = descs.iter().find(|x| x.key_value == k).unwrap();
                (
                    d.counter.as_ref().map(|c| c.step),
                    d.pointers.len(),
                    {
                        let mut s: Vec<i32> = d.pointers.iter().map(|p| p.base_step).collect();
                        s.sort();
                        s
                    },
                    d.ip_advance_literal,
                    d.writes.len(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(summary(&original), summary(&renamed));
}

/// Real cabinets wrap each register dispatch in an outer
/// `if(<gate-A>: var(self); <gate-B>: var(self); ...; else: <real-dispatch>)`
/// — the TF / IRQ override layer that takes precedence over normal
/// instruction execution. The recogniser must peel this wrapper and
/// still find the loop. The cabinet may also wrap IP in
/// `calc(<dispatch> + var(--prefixLen))`. Both wrappers must be
/// stripped before recognition.
#[test]
fn outer_wrappers_are_stripped() {
    let pred = style_eq("--cont", 1.0);
    let no_rep = style_eq("--hasREP", 0.0);
    let active_guard = StyleTest::And(vec![
        style_eq("--hasREP", 1.0),
        style_eq("--inhibit", 0.0),
    ]);

    // Inner per-V bodies — same shape as cabinet_a.
    let cx_inner = dispatch(
        "--op",
        vec![(0xAA as f64, counter_body(no_rep, "--prevCount"))],
        keep_self("--prevCount"),
    );
    let ip_inner = dispatch(
        "--op",
        vec![(
            0xAA as f64,
            ip_body(pred.clone(), "--prevPC", var("--introBytes"), 1),
        )],
        keep_self("--prevPC"),
    );
    let di_inner = dispatch(
        "--op",
        vec![(
            0xAA as f64,
            pointer_body(
                active_guard.clone(),
                "--prevHand",
                1,
                "--prevMood",
                10,
                "--clampLow",
                "--bitN",
            ),
        )],
        keep_self("--prevHand"),
    );

    // Wrap CX in TF/IRQ-style passthrough wrapper.
    let cx_wrapped = Expr::StyleCondition {
        branches: vec![
            StyleBranch {
                condition: style_eq("--trapMode", 1.0),
                then: var("--prevCount"),
            },
            StyleBranch {
                condition: style_eq("--alarmActive", 1.0),
                then: var("--prevCount"),
            },
        ],
        fallback: Box::new(cx_inner),
    };

    // Wrap IP in TF/IRQ wrapper, then in calc(... + var(introBytes)).
    let ip_passthrough = Expr::StyleCondition {
        branches: vec![
            StyleBranch {
                condition: style_eq("--trapMode", 1.0),
                then: var("--prevPC"),
            },
            StyleBranch {
                condition: style_eq("--alarmActive", 1.0),
                then: var("--prevPC"),
            },
        ],
        fallback: Box::new(ip_inner),
    };
    let ip_wrapped = add(ip_passthrough, var("--introBytes"));

    // DI gets the same wrapper.
    let di_wrapped = Expr::StyleCondition {
        branches: vec![
            StyleBranch {
                condition: style_eq("--trapMode", 1.0),
                then: var("--prevHand"),
            },
            StyleBranch {
                condition: style_eq("--alarmActive", 1.0),
                then: var("--prevHand"),
            },
        ],
        fallback: Box::new(di_inner),
    };

    let asns = vec![
        assign("--count", cx_wrapped),
        assign("--pc", ip_wrapped),
        assign("--hand", di_wrapped),
    ];

    let descs = recognise_loops(&asns);
    assert_eq!(
        descs.len(),
        1,
        "outer-wrapped dispatches should still recognise the loop, got {:?}",
        descs
    );
    let d = &descs[0];
    assert!(d.counter.is_some());
    assert_eq!(d.pointers.len(), 1);
    assert_eq!(d.ip_advance_literal, 1);
}

/// Outer wrappers around the IP register can return non-self values in
/// their override branches (kiln's TF/IRQ wrapper emits things like
/// `--_tfIP` or `picVector*4`, not `var(self)`). We must still find the
/// real dispatch in the fallback regardless. The recogniser does NOT
/// require that override branches read self — only that the fallback
/// contains a single-key dispatch.
#[test]
fn ip_wrapper_with_non_self_overrides_still_recognises() {
    let pred = style_eq("--cont", 1.0);
    let no_rep = style_eq("--hasREP", 0.0);
    let active_guard = StyleTest::And(vec![
        style_eq("--hasREP", 1.0),
        style_eq("--inhibit", 0.0),
    ]);

    let cx_inner = dispatch(
        "--op",
        vec![(0xAA as f64, counter_body(no_rep, "--prevCount"))],
        keep_self("--prevCount"),
    );
    let ip_inner = dispatch(
        "--op",
        vec![(
            0xAA as f64,
            ip_body(pred.clone(), "--prevPC", var("--introBytes"), 1),
        )],
        keep_self("--prevPC"),
    );
    let di_inner = dispatch(
        "--op",
        vec![(
            0xAA as f64,
            pointer_body(
                active_guard.clone(),
                "--prevHand",
                1,
                "--prevMood",
                10,
                "--clampLow",
                "--bitN",
            ),
        )],
        keep_self("--prevHand"),
    );

    // IP wrapper: TF returns trap-IP target, IRQ returns IVT slot
    // address. Neither is `var(self)`. Recogniser must still descend.
    let ip_wrapped = Expr::StyleCondition {
        branches: vec![
            StyleBranch {
                condition: style_eq("--trapMode", 1.0),
                then: var("--trapTargetIP"),
            },
            StyleBranch {
                condition: style_eq("--alarmActive", 1.0),
                then: mul(var("--ivtSlot"), lit(4.0)),
            },
        ],
        fallback: Box::new(ip_inner),
    };
    let ip_outer = add(ip_wrapped, var("--introBytes"));

    let asns = vec![
        assign("--count", cx_inner),
        assign("--pc", ip_outer),
        assign("--hand", di_inner),
    ];
    let descs = recognise_loops(&asns);
    assert_eq!(
        descs.len(),
        1,
        "must descend into IP wrapper fallback even when branches don't read self: {:?}",
        descs
    );
    let d = &descs[0];
    assert!(d.counter.is_some());
    assert_eq!(d.pointers.len(), 1);
}
