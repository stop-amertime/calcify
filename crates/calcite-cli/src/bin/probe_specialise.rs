//! Per-opcode (per-dispatch-key) specialisation probe.
//!
//! Tests the architectural hypothesis from CSS-DOS LOGBOOK 2026-05-12:
//! if we pick the dispatch key that the most StyleConditions branch on
//! and specialise the entire cabinet's Expr trees by holding that key
//! constant, do the per-property Expr trees collapse dramatically?
//!
//! Two phases:
//!   1. Pick the hot dispatch key generically (purely by counting
//!      StyleConditions whose Single test names a given property).
//!   2. For ONE value V of that key (CLI arg or default), walk every
//!      assignment's Expr tree and prune branches that can't fire when
//!      key=V; recurse into the remaining branch's `then`. Count nodes
//!      (total Exprs, StyleConditions, branches, Vars) before vs after.
//!
//! No calcite knowledge above the CSS layer: the key name is whatever
//! property the most StyleConditions test; the value is whatever the
//! caller passes. A brainfuck cabinet would specialise identically on
//! its own hot key. The probe writes nothing back into calcite-core —
//! the specialisation runs on cloned Expr trees.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::collections::HashMap;
use std::time::Instant;

use calcite_core::types::{Assignment, CalcOp, Expr, StyleBranch, StyleTest};

#[derive(Debug, Default, Clone, Copy)]
struct NodeStats {
    total_nodes: usize,
    style_conditions: usize,
    branches: usize,
    vars: usize,
    literals: usize,
    calcs: usize,
    function_calls: usize,
    concats: usize,
}

impl NodeStats {
    fn add(&mut self, other: &NodeStats) {
        self.total_nodes += other.total_nodes;
        self.style_conditions += other.style_conditions;
        self.branches += other.branches;
        self.vars += other.vars;
        self.literals += other.literals;
        self.calcs += other.calcs;
        self.function_calls += other.function_calls;
        self.concats += other.concats;
    }
}

fn count_expr(e: &Expr, s: &mut NodeStats) {
    s.total_nodes += 1;
    match e {
        Expr::Literal(_) | Expr::StringLiteral(_) => s.literals += 1,
        Expr::Var { fallback, .. } => {
            s.vars += 1;
            if let Some(fb) = fallback {
                count_expr(fb, s);
            }
        }
        Expr::Calc(op) => {
            s.calcs += 1;
            count_calc(op, s);
        }
        Expr::StyleCondition { branches, fallback } => {
            s.style_conditions += 1;
            s.branches += branches.len();
            for b in branches {
                count_styletest(&b.condition, s);
                count_expr(&b.then, s);
            }
            count_expr(fallback, s);
        }
        Expr::FunctionCall { args, .. } => {
            s.function_calls += 1;
            for a in args {
                count_expr(a, s);
            }
        }
        Expr::Concat(parts) => {
            s.concats += 1;
            for p in parts {
                count_expr(p, s);
            }
        }
    }
}

fn count_calc(op: &CalcOp, s: &mut NodeStats) {
    match op {
        CalcOp::Add(a, b)
        | CalcOp::Sub(a, b)
        | CalcOp::Mul(a, b)
        | CalcOp::Div(a, b)
        | CalcOp::Mod(a, b)
        | CalcOp::Pow(a, b) => {
            count_expr(a, s);
            count_expr(b, s);
        }
        CalcOp::Min(args) | CalcOp::Max(args) => {
            for x in args {
                count_expr(x, s);
            }
        }
        CalcOp::Clamp(a, b, c) => {
            count_expr(a, s);
            count_expr(b, s);
            count_expr(c, s);
        }
        CalcOp::Round(_, a, b) => {
            count_expr(a, s);
            count_expr(b, s);
        }
        CalcOp::Sign(a) | CalcOp::Abs(a) | CalcOp::Negate(a) => {
            count_expr(a, s);
        }
    }
}

fn count_styletest(t: &StyleTest, s: &mut NodeStats) {
    match t {
        StyleTest::Single { value, .. } => count_expr(value, s),
        StyleTest::And(parts) | StyleTest::Or(parts) => {
            for p in parts {
                count_styletest(p, s);
            }
        }
    }
}

/// Count, per `property` name appearing in `StyleTest::Single`, how many
/// StyleConditions across all assignments branch on it. The key with the
/// highest count is the candidate hot dispatch key.
fn key_dispatch_counts(assignments: &[Assignment]) -> HashMap<String, usize> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for a in assignments {
        walk_collect_keys(&a.value, &mut counts);
    }
    counts
}

fn walk_collect_keys(e: &Expr, counts: &mut HashMap<String, usize>) {
    match e {
        Expr::StyleCondition { branches, fallback } => {
            for b in branches {
                collect_keys_in_test(&b.condition, counts);
                walk_collect_keys(&b.then, counts);
            }
            walk_collect_keys(fallback, counts);
        }
        Expr::Calc(op) => walk_calc_keys(op, counts),
        Expr::Concat(parts) => {
            for p in parts {
                walk_collect_keys(p, counts);
            }
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                walk_collect_keys(a, counts);
            }
        }
        Expr::Var { fallback, .. } => {
            if let Some(fb) = fallback {
                walk_collect_keys(fb, counts);
            }
        }
        Expr::Literal(_) | Expr::StringLiteral(_) => {}
    }
}

fn walk_calc_keys(op: &CalcOp, counts: &mut HashMap<String, usize>) {
    match op {
        CalcOp::Add(a, b)
        | CalcOp::Sub(a, b)
        | CalcOp::Mul(a, b)
        | CalcOp::Div(a, b)
        | CalcOp::Mod(a, b)
        | CalcOp::Pow(a, b) => {
            walk_collect_keys(a, counts);
            walk_collect_keys(b, counts);
        }
        CalcOp::Min(args) | CalcOp::Max(args) => {
            for x in args {
                walk_collect_keys(x, counts);
            }
        }
        CalcOp::Clamp(a, b, c) => {
            walk_collect_keys(a, counts);
            walk_collect_keys(b, counts);
            walk_collect_keys(c, counts);
        }
        CalcOp::Round(_, a, b) => {
            walk_collect_keys(a, counts);
            walk_collect_keys(b, counts);
        }
        CalcOp::Sign(a) | CalcOp::Abs(a) | CalcOp::Negate(a) => {
            walk_collect_keys(a, counts);
        }
    }
}

fn collect_keys_in_test(t: &StyleTest, counts: &mut HashMap<String, usize>) {
    match t {
        StyleTest::Single { property, .. } => {
            *counts.entry(property.clone()).or_insert(0) += 1;
        }
        StyleTest::And(parts) | StyleTest::Or(parts) => {
            for p in parts {
                collect_keys_in_test(p, counts);
            }
        }
    }
}

/// Result of testing one StyleBranch against a known (key, value) binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BranchOutcome {
    /// Branch is guaranteed to fire (condition simplifies to true under binding).
    AlwaysTakes,
    /// Branch can never fire under binding (condition simplifies to false).
    NeverTakes,
    /// Condition depends on something other than the known binding; keep branch.
    Unknown,
}

/// Evaluate a StyleTest under the binding `(key = value)`. Returns:
/// - `AlwaysTakes` if every reachable Single test on `key` matches `value`
///   and the compound shape resolves to true.
/// - `NeverTakes` if any required Single test on `key` rejects `value`.
/// - `Unknown` if the test reads anything else (different property, or
///   a value Expr that isn't a literal).
///
/// We only resolve `Single { property == key, value == Literal(L) }`. We
/// don't try to evaluate the test's `value` symbolically against the
/// current cabinet's other slots — that would require a real evaluator.
fn test_outcome(t: &StyleTest, key: &str, value: i64) -> BranchOutcome {
    match t {
        StyleTest::Single { property, value: v } => {
            if property != key {
                return BranchOutcome::Unknown;
            }
            match v {
                Expr::Literal(lit) => {
                    if (*lit as i64) == value {
                        BranchOutcome::AlwaysTakes
                    } else {
                        BranchOutcome::NeverTakes
                    }
                }
                _ => BranchOutcome::Unknown,
            }
        }
        StyleTest::And(parts) => {
            let mut all_take = true;
            for p in parts {
                match test_outcome(p, key, value) {
                    BranchOutcome::NeverTakes => return BranchOutcome::NeverTakes,
                    BranchOutcome::Unknown => all_take = false,
                    BranchOutcome::AlwaysTakes => {}
                }
            }
            if all_take { BranchOutcome::AlwaysTakes } else { BranchOutcome::Unknown }
        }
        StyleTest::Or(parts) => {
            let mut any_take = false;
            let mut all_reject = true;
            for p in parts {
                match test_outcome(p, key, value) {
                    BranchOutcome::AlwaysTakes => any_take = true,
                    BranchOutcome::NeverTakes => {}
                    BranchOutcome::Unknown => all_reject = false,
                }
            }
            if any_take {
                BranchOutcome::AlwaysTakes
            } else if all_reject {
                BranchOutcome::NeverTakes
            } else {
                BranchOutcome::Unknown
            }
        }
    }
}

/// Specialise an Expr tree under the binding `(key = value)`. In-place.
///
/// Rules (generic, structural):
/// - StyleCondition { branches, fallback }:
///   * Walk branches in order. Drop any branch whose test `NeverTakes`.
///   * If a branch's test `AlwaysTakes`, the chain is decided at compile
///     time: replace the entire condition with the specialised `then`,
///     dropping subsequent branches and the fallback.
///   * Otherwise keep the branch; specialise its `then`.
///   * After processing, if no branches remain, collapse to specialised
///     fallback.
/// - Calc / Concat / FunctionCall / Var.fallback: recurse into children.
/// - Var / Literal / StringLiteral: leaf, no change.
fn specialise(expr: &mut Expr, key: &str, value: i64) {
    match expr {
        Expr::StyleCondition { branches, fallback } => {
            let mut new_branches: Vec<StyleBranch> = Vec::with_capacity(branches.len());
            let mut decided: Option<Expr> = None;
            for b in branches.drain(..) {
                match test_outcome(&b.condition, key, value) {
                    BranchOutcome::NeverTakes => {} // drop
                    BranchOutcome::AlwaysTakes => {
                        // Chain is decided here. Specialise this branch's
                        // `then` and use it as the whole result.
                        let mut t = b.then;
                        specialise(&mut t, key, value);
                        decided = Some(t);
                        break;
                    }
                    BranchOutcome::Unknown => {
                        let StyleBranch { condition, mut then } = b;
                        specialise(&mut then, key, value);
                        new_branches.push(StyleBranch { condition, then });
                    }
                }
            }

            if let Some(t) = decided {
                *expr = t;
                return;
            }

            // No branch was guaranteed; specialise fallback too.
            specialise(fallback, key, value);

            if new_branches.is_empty() {
                let fb = std::mem::replace(fallback.as_mut(), Expr::Literal(0.0));
                *expr = fb;
            } else {
                *branches = new_branches;
            }
        }
        Expr::Calc(op) => specialise_calc(op, key, value),
        Expr::Concat(parts) => {
            for p in parts {
                specialise(p, key, value);
            }
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                specialise(a, key, value);
            }
        }
        Expr::Var { fallback, .. } => {
            if let Some(fb) = fallback {
                specialise(fb, key, value);
            }
        }
        Expr::Literal(_) | Expr::StringLiteral(_) => {}
    }
}

fn specialise_calc(op: &mut CalcOp, key: &str, value: i64) {
    match op {
        CalcOp::Add(a, b)
        | CalcOp::Sub(a, b)
        | CalcOp::Mul(a, b)
        | CalcOp::Div(a, b)
        | CalcOp::Mod(a, b)
        | CalcOp::Pow(a, b) => {
            specialise(a, key, value);
            specialise(b, key, value);
        }
        CalcOp::Min(args) | CalcOp::Max(args) => {
            for x in args {
                specialise(x, key, value);
            }
        }
        CalcOp::Clamp(a, b, c) => {
            specialise(a, key, value);
            specialise(b, key, value);
            specialise(c, key, value);
        }
        CalcOp::Round(_, a, b) => {
            specialise(a, key, value);
            specialise(b, key, value);
        }
        CalcOp::Sign(a) | CalcOp::Abs(a) | CalcOp::Negate(a) => {
            specialise(a, key, value);
        }
    }
}

fn pct(num: usize, den: usize) -> f64 {
    if den == 0 { 0.0 } else { 100.0 * (num as f64) / (den as f64) }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: probe-specialise <cabinet.css> [opcode_value] [override_key]");
        eprintln!();
        eprintln!("  opcode_value   integer value to specialise on (default: 64 = INC AX)");
        eprintln!("  override_key   force a specific dispatch key instead of auto-pick");
        std::process::exit(2);
    }
    let path = &args[1];
    let value: i64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(64);
    let key_override = args.get(3).cloned();

    let t = Instant::now();
    let css = std::fs::read_to_string(path).expect("read cabinet");
    println!("[probe-specialise] read: {:.2}s  ({} bytes)",
        t.elapsed().as_secs_f64(), css.len());

    let t = Instant::now();
    let program = calcite_core::parser::parse_stylesheet(&css).expect("parse");
    println!("[probe-specialise] parse: {:.2}s  ({} assignments, {} fast-path absorbed)",
        t.elapsed().as_secs_f64(),
        program.assignments.len(),
        program.fast_path_absorbed.len());

    // ---- Phase 1: pick the hot dispatch key ----
    let t = Instant::now();
    let counts = key_dispatch_counts(&program.assignments);
    let mut ranked: Vec<(&String, &usize)> = counts.iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(a.1));
    println!("[probe-specialise] key-count scan: {:.2}s", t.elapsed().as_secs_f64());
    println!("[probe-specialise] top 10 dispatch keys (by StyleCondition Single-test count):");
    for (i, (k, c)) in ranked.iter().take(10).enumerate() {
        println!("  {:>2}. {:<28} {:>8}", i + 1, k, c);
    }

    let key: String = if let Some(k) = key_override {
        println!("[probe-specialise] using override key: {}", k);
        k
    } else {
        let chosen = ranked.first().map(|(k, _)| (*k).clone()).expect("no keys found");
        println!("[probe-specialise] auto-picked hot key: {}", chosen);
        chosen
    };

    // ---- Phase 2: specialise every assignment for (key = value) ----
    // Count Expr nodes before, clone & specialise, count nodes after.
    println!();
    println!("[probe-specialise] specialising every assignment for `{}` = {}", key, value);

    let mut before = NodeStats::default();
    for a in &program.assignments {
        let mut s = NodeStats::default();
        count_expr(&a.value, &mut s);
        before.add(&s);
    }

    let t = Instant::now();
    let mut after = NodeStats::default();
    let mut per_assignment_after: Vec<(String, NodeStats, NodeStats)> =
        Vec::with_capacity(program.assignments.len());
    for a in &program.assignments {
        let mut e = a.value.clone();
        specialise(&mut e, &key, value);
        let mut s_after = NodeStats::default();
        count_expr(&e, &mut s_after);
        let mut s_before = NodeStats::default();
        count_expr(&a.value, &mut s_before);
        after.add(&s_after);
        per_assignment_after.push((a.property.clone(), s_before, s_after));
    }
    println!("[probe-specialise] specialise pass: {:.2}s",
        t.elapsed().as_secs_f64());

    println!();
    println!("[probe-specialise] ===== AGGREGATE: ALL ASSIGNMENTS =====");
    print_compare("total Expr nodes  ", before.total_nodes, after.total_nodes);
    print_compare("StyleConditions   ", before.style_conditions, after.style_conditions);
    print_compare("StyleBranches     ", before.branches, after.branches);
    print_compare("Var nodes         ", before.vars, after.vars);
    print_compare("Literals          ", before.literals, after.literals);
    print_compare("Calc nodes        ", before.calcs, after.calcs);
    print_compare("FunctionCalls     ", before.function_calls, after.function_calls);
    print_compare("Concat nodes      ", before.concats, after.concats);

    // ---- Phase 3: distribution. Which assignments collapsed the most? ----
    // Sort by absolute node reduction.
    println!();
    println!("[probe-specialise] ===== TOP 20 ASSIGNMENTS BY REDUCTION (absolute) =====");
    let mut sorted: Vec<&(String, NodeStats, NodeStats)> = per_assignment_after.iter().collect();
    sorted.sort_by(|a, b| {
        let da = a.1.total_nodes.saturating_sub(a.2.total_nodes);
        let db = b.1.total_nodes.saturating_sub(b.2.total_nodes);
        db.cmp(&da)
    });
    println!("  {:<32} {:>10} {:>10} {:>8}", "property", "before", "after", "ratio");
    for (prop, b, a) in sorted.iter().take(20) {
        println!("  {:<32} {:>10} {:>10} {:>7.1}%",
            prop, b.total_nodes, a.total_nodes, pct(a.total_nodes, b.total_nodes));
    }

    // ---- Phase 4: how many assignments collapse to a leaf (i.e. become trivial)? ----
    let trivial = per_assignment_after.iter().filter(|(_, _, a)| {
        a.style_conditions == 0 && a.total_nodes <= 3
    }).count();
    let unchanged = per_assignment_after.iter().filter(|(_, b, a)| {
        b.total_nodes == a.total_nodes
    }).count();
    let total = per_assignment_after.len();
    println!();
    println!("[probe-specialise] ===== ASSIGNMENT OUTCOMES =====");
    println!("  total assignments       : {}", total);
    println!("  collapsed to ≤3 nodes   : {} ({:.1}%)", trivial, pct(trivial, total));
    println!("  unchanged (no key hit)  : {} ({:.1}%)", unchanged, pct(unchanged, total));

    // ---- Phase 5: dispatch-elimination ratio ----
    let sc_eliminated = before.style_conditions.saturating_sub(after.style_conditions);
    println!();
    println!("[probe-specialise] ===== HEADLINE =====");
    println!("  Expr-node reduction       : {} → {}  ({:+.1}%)",
        before.total_nodes, after.total_nodes,
        -100.0 * (1.0 - (after.total_nodes as f64) / (before.total_nodes as f64)));
    println!("  StyleConditions removed   : {} ({:.1}% of original)",
        sc_eliminated, pct(sc_eliminated, before.style_conditions));
    println!("  Hypothesis check          : if specialised body ≈ small fraction of original,");
    println!("                              the architectural move is structurally validated.");
}

fn print_compare(label: &str, before: usize, after: usize) {
    let ratio = if before == 0 { 0.0 } else { 100.0 * (after as f64) / (before as f64) };
    let delta = if before == 0 {
        0.0
    } else {
        -100.0 * (1.0 - (after as f64) / (before as f64))
    };
    println!("  {:<20} {:>10} → {:>10}  ({:>5.1}% kept, {:+.1}%)",
        label, before, after, ratio, delta);
}
