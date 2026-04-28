//! Phase 0.5.5 — calcite-side primitive conformance against the suite at
//! `tests/conformance/primitives/`.
//!
//! Each fixture has a `<name>.css` and a `<name>.expect.json`. The .json
//! lists the custom-property names to read after one tick and the
//! `expected_str` value Chrome produced via `getComputedStyle()` (captured
//! by `tests/conformance/run-primitives.mjs --capture`). This test parses
//! each fixture, ticks once, formats the slot value the same way Chrome
//! does, and asserts equality.
//!
//! Cardinal rule: if calcite disagrees with Chrome, calcite is wrong.
//! Failures here are bugs in calcite (or in the fixture; review the
//! `.css` against Chrome to decide which).
//!
//! Skipped: `type: "string"` fixtures. Calcite's `State::get_var` returns
//! i32 only; string-property reads need a different code path that
//! Phase 0.5 doesn't yet exercise. Those fixtures still ship in the
//! Chrome runner; this test prints them as SKIP.
//!
//! Walks up from `CARGO_MANIFEST_DIR` (= crates/calcite-core/) to the
//! worktree root and reads `tests/conformance/primitives/` from there —
//! so the test works in both the main checkout and worktrees.
//!
//! Throwaway-friendly: this is a Phase 0.5 deliverable. It pins calcite
//! v1 against the Chrome ground truth before Phase 1 begins. After
//! Phase 1 ships, the same fixtures should pass against the v2
//! implementation; the test stays.

use std::fs;
use std::path::{Path, PathBuf};

use calcite_core::eval::{Backend, Evaluator};
use calcite_core::parser::parse_css;
use calcite_core::State;

fn worktree_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // CARGO_MANIFEST_DIR = .../crates/calcite-core
    p.pop();
    p.pop();
    p
}

fn fixtures_dir() -> PathBuf {
    worktree_root().join("tests").join("conformance").join("primitives")
}

#[derive(Debug)]
struct ReadEntry {
    property: String,
    type_: String,
    expected_str: String,
}

#[derive(Debug)]
struct Expect {
    description: String,
    ticks: u32,
    /// If present, the v1 calcite implementation is known to disagree
    /// with Chrome on this fixture. The test reports XFAIL and does not
    /// fail the run. The string is the reason — typically "Phase 1+
    /// will fix this via …".
    xfail_v1: Option<String>,
    read: Vec<ReadEntry>,
}

/// Tiny hand-rolled JSON reader for the expect.json shape — avoids pulling
/// serde_json into the dev-deps just for these flat objects. The format is
/// intentionally simple (see tests/conformance/README.md):
///   { "description": str, "ticks": int, "read": [ { "property": str,
///                       "type": str, "expected_str": str }, ... ] }
fn parse_expect(json: &str) -> Result<Expect, String> {
    let v: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| format!("expect.json parse error: {e}"))?;
    let description = v.get("description").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let ticks = v.get("ticks").and_then(|x| x.as_u64()).unwrap_or(1) as u32;
    let xfail_v1 = v.get("xfail_v1").and_then(|x| x.as_str()).map(|s| s.to_string());
    let mut read = Vec::new();
    for r in v.get("read").and_then(|x| x.as_array()).ok_or("missing read array")? {
        read.push(ReadEntry {
            property: r.get("property").and_then(|x| x.as_str()).ok_or("read[].property")?.to_string(),
            type_: r.get("type").and_then(|x| x.as_str()).unwrap_or("integer").to_string(),
            expected_str: r.get("expected_str").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        });
    }
    Ok(Expect { description, ticks, xfail_v1, read })
}

/// Format a calcite slot value the same way Chrome's
/// `getComputedStyle().getPropertyValue('--x').trim()` does for an
/// `<integer>` registered property: just the decimal integer, no
/// trailing whitespace, no units.
fn format_int(v: i32) -> String {
    v.to_string()
}

#[derive(Debug)]
enum CaseResult {
    Pass,
    /// Calcite happens to agree with Chrome on a fixture marked xfail_v1
    /// — the implementation gap closed unexpectedly. Treated as a
    /// non-fatal warning so the suite keeps passing, but the operator
    /// should remove the xfail marker.
    XPass(String),
    Skip(String),
    /// Calcite disagrees with Chrome on a fixture marked xfail_v1 — the
    /// known gap. Reported but doesn't fail the test.
    XFail(String, Vec<String>),
    Fail(Vec<String>),
}

fn run_fixture(css_path: &Path, expect_path: &Path, backend: Backend) -> CaseResult {
    let css = match fs::read_to_string(css_path) {
        Ok(s) => s,
        Err(e) => return CaseResult::Fail(vec![format!("read css: {e}")]),
    };
    let expect_raw = match fs::read_to_string(expect_path) {
        Ok(s) => s,
        Err(e) => return CaseResult::Fail(vec![format!("read expect.json: {e}")]),
    };
    let expect = match parse_expect(&expect_raw) {
        Ok(e) => e,
        Err(e) => return CaseResult::Fail(vec![format!("parse expect.json: {e}")]),
    };

    // Skip string-typed cases until calcite exposes a string-read path.
    if expect.read.iter().any(|r| r.type_ == "string") {
        return CaseResult::Skip(format!(
            "string-typed read (calcite get_var is i32-only): {}", expect.description
        ));
    }

    let parsed = match parse_css(&css) {
        Ok(p) => p,
        Err(e) => return CaseResult::Fail(vec![format!("parse css: {e}")]),
    };
    let mut state = State::default();
    state.load_properties(&parsed.properties);
    let mut evaluator = Evaluator::from_parsed(&parsed);
    evaluator.set_backend(backend);

    for _ in 0..expect.ticks {
        evaluator.tick(&mut state);
    }

    let mut fails = Vec::new();
    for r in &expect.read {
        // Property names in expect.json have the leading "--"; State strips it.
        let bare = r.property.strip_prefix("--").unwrap_or(&r.property);
        let value = state.get_var(bare);
        let actual = match value {
            Some(v) => format_int(v),
            None => "".to_string(),
        };
        if actual != r.expected_str {
            fails.push(format!(
                "  {}: expected {:?}, got {:?}",
                r.property, r.expected_str, actual
            ));
        }
    }

    match (&expect.xfail_v1, fails.is_empty()) {
        (Some(_), true) => CaseResult::XPass(
            "fixture is marked xfail_v1 but calcite now agrees with Chrome — \
             remove xfail_v1 from the .expect.json".into(),
        ),
        (Some(reason), false) => CaseResult::XFail(reason.clone(), fails),
        (None, true) => CaseResult::Pass,
        (None, false) => CaseResult::Fail(fails),
    }
}

fn run_suite(backend: Backend) {
    let dir = fixtures_dir();
    if !dir.is_dir() {
        eprintln!(
            "primitive_conformance ({backend:?}): skipping — fixtures dir not found at {}",
            dir.display()
        );
        return;
    }

    let mut entries: Vec<_> = fs::read_dir(&dir)
        .expect("read fixtures dir")
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            (p.extension().is_some_and(|x| x == "css")).then_some(p)
        })
        .collect();
    entries.sort();

    let mut pass = 0;
    let mut fail = 0;
    let mut skip = 0;
    let mut xpass = 0;
    let mut xfail = 0;
    let mut failures: Vec<(String, Vec<String>)> = Vec::new();
    let mut xpasses: Vec<(String, String)> = Vec::new();

    eprintln!("--- primitive_conformance backend = {backend:?} ---");
    for css in &entries {
        let stem = css.file_stem().unwrap().to_string_lossy().to_string();
        let expect = dir.join(format!("{stem}.expect.json"));
        if !expect.exists() {
            eprintln!("MISS  {stem}: missing .expect.json");
            fail += 1;
            failures.push((stem.clone(), vec!["missing .expect.json".into()]));
            continue;
        }
        match run_fixture(css, &expect, backend) {
            CaseResult::Pass => {
                eprintln!("PASS  {stem}");
                pass += 1;
            }
            CaseResult::Skip(reason) => {
                eprintln!("SKIP  {stem}: {reason}");
                skip += 1;
            }
            CaseResult::XFail(reason, fails) => {
                eprintln!("XFAIL {stem}: {reason}");
                for f in &fails { eprintln!("{f}"); }
                xfail += 1;
            }
            CaseResult::XPass(reason) => {
                eprintln!("XPASS {stem}: {reason}");
                xpass += 1;
                xpasses.push((stem.clone(), reason));
            }
            CaseResult::Fail(fails) => {
                eprintln!("FAIL  {stem}");
                for f in &fails { eprintln!("{f}"); }
                fail += 1;
                failures.push((stem.clone(), fails));
            }
        }
    }

    eprintln!(
        "\n[{backend:?}] {pass} passed, {fail} failed, {skip} skipped, {xfail} xfail, {xpass} xpass (of {} fixtures)",
        entries.len(),
    );

    if fail > 0 {
        let mut msg = format!("primitive conformance ({backend:?}) failures:\n");
        for (name, fails) in &failures {
            msg.push_str(&format!("  {name}:\n"));
            for f in fails {
                msg.push_str(&format!("    {f}\n"));
            }
        }
        panic!("{msg}");
    }
    if xpass > 0 {
        let mut msg = format!(
            "primitive conformance ({backend:?}): xfail_v1 markers should be \
             removed (calcite now agrees with Chrome on these fixtures):\n",
        );
        for (name, reason) in &xpasses {
            msg.push_str(&format!("  {name}: {reason}\n"));
        }
        panic!("{msg}");
    }
}

#[test]
fn primitive_conformance_bytecode() {
    run_suite(Backend::Bytecode);
}

/// Phase 1 acceptance gate: the DAG walker produces identical results to
/// the bytecode interpreter on every primitive fixture.
#[test]
fn primitive_conformance_dag() {
    run_suite(Backend::Dag);
}

/// Phase 3 acceptance gate: the Closure backend produces identical
/// results to the bytecode interpreter on every primitive fixture.
#[test]
fn primitive_conformance_closure() {
    run_suite(Backend::Closure);
}
