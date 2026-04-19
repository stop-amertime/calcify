//! What slots does my detector pick as 'cyclic' at tick 200K?

use std::path::PathBuf;

fn main() {
    let css = std::fs::read_to_string(PathBuf::from(
        "C:/Users/AdmT9N0CX01V65438A/Documents/src/calcite/output/bootle-ctest.css",
    )).unwrap();
    let parsed = calcite_core::parser::parse_css(&css).unwrap();
    let mut state = calcite_core::State::default();
    state.load_properties(&parsed.properties);
    let mut evaluator = calcite_core::Evaluator::from_parsed(&parsed);
    for _ in 0..200_000 { evaluator.tick(&mut state); }
    const N: usize = 4096;
    let mut snaps: Vec<Vec<i32>> = Vec::with_capacity(N);
    for _ in 0..N {
        snaps.push(state.state_vars.clone());
        evaluator.tick(&mut state);
    }

    let names = &state.state_var_names;
    eprintln!("slot counts (threshold candidates):");
    for s in 0..names.len() {
        let uniq: std::collections::HashSet<i32> = snaps.iter().map(|v| v[s]).collect();
        eprintln!("  {:>14}  uniq={:>5}", names[s], uniq.len());
    }
}
