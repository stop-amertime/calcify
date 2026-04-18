//! Quick probe: compare autocorrelation match rates using (a) absolute-value
//! hash of ALL state vars, (b) absolute-value hash of probe-spec stable
//! subset, (c) delta hash of ALL state vars, over a 4096-sample window
//! taken during the splash phase (starting at tick 200,000).

use std::path::PathBuf;

fn hash_u64_slice_u32(vals: &[i32]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &v in vals {
        h ^= v as u32 as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn main() {
    let css = std::fs::read_to_string(PathBuf::from(
        "C:/Users/AdmT9N0CX01V65438A/Documents/src/calcite/output/bootle-ctest.css",
    ))
    .unwrap();
    let parsed = calcite_core::parser::parse_css(&css).unwrap();
    let mut state = calcite_core::State::default();
    state.load_properties(&parsed.properties);
    let mut evaluator = calcite_core::Evaluator::from_parsed(&parsed);

    // Skip to tick 200_000.
    for _ in 0..200_000 {
        evaluator.tick(&mut state);
    }

    // Collect 4096 vars snapshots.
    const N: usize = 4096;
    let mut snaps: Vec<Vec<i32>> = Vec::with_capacity(N);
    for _ in 0..N {
        snaps.push(state.state_vars.clone());
        evaluator.tick(&mut state);
    }

    let names = &state.state_var_names;
    let iterating = [
        "cycleCount", "memAddr", "SI", "CX", "DI", "AX", "BX", "IP",
        "biosAL", "memVal", "biosAH", "flags", "SP",
    ];
    let iter_idx: std::collections::HashSet<usize> = iterating
        .iter()
        .filter_map(|n| names.iter().position(|x| x == n))
        .collect();
    let uop_ip: Vec<usize> = ["uOp", "IP"]
        .iter()
        .filter_map(|n| names.iter().position(|x| x == n))
        .collect();
    let stable_idx: Vec<usize> = (0..names.len()).filter(|i| !iter_idx.contains(i)).collect();
    let stable_plus_uopip: Vec<usize> = {
        let mut v = stable_idx.clone();
        for i in &uop_ip {
            if !v.contains(i) { v.push(*i); }
        }
        v
    };

    eprintln!("n_vars: {}, stable+uOp+IP count: {}", names.len(), stable_plus_uopip.len());

    // Build three fingerprint streams.
    let fp_all_abs: Vec<u64> = snaps.iter().map(|v| hash_u64_slice_u32(v)).collect();
    let fp_sub_abs: Vec<u64> = snaps
        .iter()
        .map(|v| {
            let sub: Vec<i32> = stable_plus_uopip.iter().map(|&i| v[i]).collect();
            hash_u64_slice_u32(&sub)
        })
        .collect();
    let fp_all_delta: Vec<u64> = (0..N)
        .map(|t| {
            if t == 0 {
                0
            } else {
                let d: Vec<i32> = (0..names.len())
                    .map(|i| snaps[t][i].wrapping_sub(snaps[t - 1][i]))
                    .collect();
                hash_u64_slice_u32(&d)
            }
        })
        .collect();

    fn best_period(fps: &[u64], max_p: usize) -> (usize, usize, usize) {
        let mut best = (0, 0);
        let len = fps.len();
        for p in 2..=max_p.min(len / 3) {
            let overlap = len - p;
            let mut matches = 0;
            for t in 0..overlap {
                if fps[t] == fps[t + p] { matches += 1; }
            }
            if matches > best.1 { best = (p, matches); }
        }
        (best.0, best.1, len - best.0)
    }

    let (p_all_abs, m_all_abs, o_all_abs) = best_period(&fp_all_abs, 512);
    let (p_sub_abs, m_sub_abs, o_sub_abs) = best_period(&fp_sub_abs, 512);
    let (p_all_delta, m_all_delta, o_all_delta) = best_period(&fp_all_delta, 512);

    eprintln!("fingerprint            best P    matches/overlap   rate");
    eprintln!(
        "all-vars abs            {:>3}    {:>6}/{:>6}    {:.2}%",
        p_all_abs, m_all_abs, o_all_abs, m_all_abs as f64 * 100.0 / o_all_abs as f64
    );
    eprintln!(
        "stable+uOp+IP abs       {:>3}    {:>6}/{:>6}    {:.2}%",
        p_sub_abs, m_sub_abs, o_sub_abs, m_sub_abs as f64 * 100.0 / o_sub_abs as f64
    );
    eprintln!(
        "all-vars delta          {:>3}    {:>6}/{:>6}    {:.2}%",
        p_all_delta, m_all_delta, o_all_delta, m_all_delta as f64 * 100.0 / o_all_delta as f64
    );
}
